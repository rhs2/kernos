package server

import (
	"bytes"
	"context"
	"crypto/ed25519"
	"crypto/rand"
	"database/sql"
	"encoding/base64"
	"encoding/json"
	"fmt"
	"io"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/rhs2/kernos/gateway/internal/canary"
	"github.com/rhs2/kernos/gateway/internal/config"
	_ "github.com/rhs2/kernos/gateway/internal/connectors/http"
	_ "github.com/rhs2/kernos/gateway/internal/connectors/sqlite"
	_ "github.com/rhs2/kernos/gateway/internal/connectors/testtools"
	"github.com/rhs2/kernos/gateway/internal/idem"
	"github.com/rhs2/kernos/gateway/internal/kernel"
	"github.com/rhs2/kernos/gateway/internal/remit"
)

const secretToken = "halcyon-api-token-9f2c"

const ledgerSchema = `
create table if not exists vendors(id integer primary key, name text not null, terms text);
create table if not exists ledger_entries(id integer primary key, invoice_id text not null, vendor text not null, account text not null, amount real not null, posted_at text not null, voided_at text, void_reason text);
insert into vendors(name, terms) values ('Northwind Dairy', 'net30');
`

type recordedEvent struct {
	RunID string
	Remit string
	Auth  string
	Body  map[string]any
}

type kernelStub struct {
	mu     sync.Mutex
	events []recordedEvent
	srv    *httptest.Server
	pub    ed25519.PublicKey
}

func newKernelStub(t *testing.T, pub ed25519.PublicKey) *kernelStub {
	t.Helper()
	k := &kernelStub{pub: pub}
	k.srv = httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		switch {
		case r.URL.Path == "/v1/keys":
			json.NewEncoder(w).Encode(map[string]any{"key_id": "key_test", "algorithm": "ed25519", "public_key": base64.RawURLEncoding.EncodeToString(k.pub)})
		case strings.HasPrefix(r.URL.Path, "/v1/runs/") && strings.HasSuffix(r.URL.Path, "/events"):
			var body map[string]any
			json.NewDecoder(r.Body).Decode(&body)
			runID := strings.TrimSuffix(strings.TrimPrefix(r.URL.Path, "/v1/runs/"), "/events")
			k.mu.Lock()
			k.events = append(k.events, recordedEvent{RunID: runID, Remit: r.Header.Get("X-Kernos-Remit"), Auth: r.Header.Get("Authorization"), Body: body})
			k.mu.Unlock()
			w.WriteHeader(201)
			w.Write([]byte(`{"seq": 1, "hash": "x"}`))
		default:
			w.WriteHeader(404)
		}
	}))
	t.Cleanup(k.srv.Close)
	return k
}

func (k *kernelStub) last() (recordedEvent, bool) {
	k.mu.Lock()
	defer k.mu.Unlock()
	if len(k.events) == 0 {
		return recordedEvent{}, false
	}
	return k.events[len(k.events)-1], true
}

func (k *kernelStub) count() int {
	k.mu.Lock()
	defer k.mu.Unlock()
	return len(k.events)
}

type upstream struct {
	mu       sync.Mutex
	mode     string
	seenAuth string
	srv      *httptest.Server
}

func newUpstream(t *testing.T) *upstream {
	u := &upstream{mode: "ok"}
	u.srv = httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		u.mu.Lock()
		mode := u.mode
		u.seenAuth = r.Header.Get("Authorization")
		u.mu.Unlock()
		switch r.URL.Path {
		case "/probe":
			if mode == "ok" {
				w.Header().Set("Content-Type", "application/json")
				w.Write([]byte(`{"ok": true}`))
			} else {
				w.Header().Set("Content-Type", "text/plain")
				w.Write([]byte("okay=true"))
			}
		case "/echo":
			w.Write([]byte("authorization was " + r.Header.Get("Authorization")))
		default:
			w.WriteHeader(404)
		}
	}))
	t.Cleanup(u.srv.Close)
	return u
}

func (u *upstream) set(mode string) {
	u.mu.Lock()
	defer u.mu.Unlock()
	u.mode = mode
}

type harness struct {
	t        *testing.T
	priv     ed25519.PrivateKey
	kernel   *kernelStub
	up       *upstream
	server   *Server
	http     *httptest.Server
	logs     *bytes.Buffer
	secrets  *config.Secrets
	dataDir  string
	ledger   string
	now      time.Time
	nowMu    sync.Mutex
	canaryMg *canary.Manager
}

func (h *harness) clock() time.Time {
	h.nowMu.Lock()
	defer h.nowMu.Unlock()
	return h.now
}

func (h *harness) advance(d time.Duration) {
	h.nowMu.Lock()
	defer h.nowMu.Unlock()
	h.now = h.now.Add(d)
}

func newHarness(t *testing.T) *harness {
	t.Helper()
	pub, priv, err := ed25519.GenerateKey(rand.Reader)
	if err != nil {
		t.Fatal(err)
	}
	h := &harness{t: t, priv: priv, now: time.Now(), logs: &bytes.Buffer{}}
	h.kernel = newKernelStub(t, pub)
	h.up = newUpstream(t)
	h.dataDir = t.TempDir()
	h.ledger = filepath.Join(h.dataDir, "halcyon-ledger.db")
	schemaJSON, _ := json.Marshal(ledgerSchema)
	raw := fmt.Sprintf(`{
	  "listen": "127.0.0.1:0",
	  "kernel_url": %q,
	  "token": "${KERNOS_TOKEN}",
	  "data_dir": %q,
	  "canary": {"interval_seconds": 3600, "quarantine_after": 2, "auto_release": false},
	  "connectors": [
	    {"name": "ledger", "type": "sqlite", "path": %q, "init_sql": %s,
	     "tools": {
	       "post_entry": {"description": "Post a journal entry", "writes": true,
	         "statement": "insert into ledger_entries(invoice_id, vendor, account, amount, posted_at) values (:invoice_id, :vendor, :account, :amount, :now) returning id as entry_id, posted_at",
	         "input_schema": {"type": "object", "required": ["invoice_id", "vendor", "account", "amount"]},
	         "contract": {"required": {"entry_id": "number", "posted_at": "string"}}},
	       "void_entry": {"description": "Void a posted entry", "writes": true,
	         "statement": "update ledger_entries set voided_at=:now, void_reason=:reason where id=:entry_id returning id as entry_id, voided_at",
	         "contract": {"required": {"entry_id": "number", "voided_at": "string"}}},
	       "lookup_vendor": {"description": "Find a vendor", "writes": false,
	         "statement": "select id, name, terms from vendors where name = :name",
	         "contract": {"required": {"rows": "list"}}}
	     },
	     "probe": {"tool": "lookup_vendor", "args": {"name": "__probe__"}}},
	    {"name": "http", "type": "http", "allowed_hosts": ["api.halcyon.example", "127.0.0.1"],
	     "headers": {"Authorization": "Bearer ${HALCYON_API_TOKEN}"},
	     "tools": {"get": {"writes": false, "contract": {"required": {"status": "number", "body": "string"}}}},
	     "probe": {"tool": "get", "args": {"url": %q}, "contract": {"required": {"status": "number", "body": "string", "json": "object"}}}}
	  ]
	}`, h.kernel.srv.URL, h.dataDir, h.ledger, schemaJSON, h.up.srv.URL+"/probe")
	env := map[string]string{"HALCYON_API_TOKEN": secretToken, "KERNOS_TOKEN": "kernel-shared-secret", "KERNOS_GATEWAY_TEST_TOOLS": "1"}
	lookup := func(name string) (string, bool) { v, ok := env[name]; return v, ok }
	cfg := config.Default()
	h.secrets = config.NewSecrets()
	if err := config.Parse([]byte(raw), cfg, lookup, h.secrets); err != nil {
		t.Fatal(err)
	}
	if err := config.ApplyEnv(cfg, lookup, h.secrets); err != nil {
		t.Fatal(err)
	}
	if err := config.Validate(cfg); err != nil {
		t.Fatal(err)
	}
	h.secrets.Add(cfg.Token)
	logger := slog.New(slog.NewJSONHandler(h.secrets.Writer(h.logs), &slog.HandlerOptions{Level: slog.LevelDebug}))
	kc := kernel.New(cfg.KernelURL, cfg.Token, 2*time.Second)
	ks := remit.NewKeyStore(func(ctx context.Context) (string, ed25519.PublicKey, error) {
		keys, err := kc.FetchKeys(ctx)
		if err != nil {
			return "", nil, err
		}
		pk, err := remit.ParsePublicKey(keys.PublicKey)
		return keys.KeyID, pk, err
	}, time.Hour, logger)
	ctx, cancel := context.WithCancel(context.Background())
	t.Cleanup(cancel)
	ks.Start(ctx)
	built, err := BuildConnectors(cfg, logger)
	if err != nil {
		t.Fatal(err)
	}
	store, err := idem.Open(filepath.Join(h.dataDir, "idempotency.db"))
	if err != nil {
		t.Fatal(err)
	}
	h.canaryMg = canary.New(canary.Options{Interval: time.Hour, QuarantineAfter: 2, RepairDir: filepath.Join(h.dataDir, "repairs"), Log: logger, Now: h.clock})
	srv, err := New(Options{Config: cfg, Logger: logger, Secrets: h.secrets, Verifier: &remit.Verifier{Keys: ks}, Kernel: kc, Idem: store, Canary: h.canaryMg, Connectors: built, Now: h.clock})
	if err != nil {
		t.Fatal(err)
	}
	h.server = srv
	h.http = httptest.NewServer(srv.Handler())
	t.Cleanup(func() {
		h.http.Close()
		srv.Close()
	})
	return h
}

func (h *harness) token(overrides map[string]any) string {
	now := h.clock().Unix()
	payload := map[string]any{
		"rid": "rem_01j6zq0000000000000000000a", "run": "run_01j6zr0000000000000000000a", "iss": "key_test",
		"iat": now - 10, "nbf": now - 10, "exp": now + 3600,
		"tools":  []string{"ledger.*", "http.*", "test.*"},
		"scopes": []string{"sql:table:*", "http:host:127.0.0.1", "test:*"},
		"grants": []string{}, "spend": map[string]any{"tokens": 200000, "usd": 2.0},
		"autonomy": "supervised", "policy_set": []string{"finance-default"},
		"requested_by": map[string]any{"id": "u-ana", "role": "ap_clerk", "manager": "u-tom"},
	}
	for k, v := range overrides {
		if v == nil {
			delete(payload, k)
		} else {
			payload[k] = v
		}
	}
	b, err := json.Marshal(payload)
	if err != nil {
		h.t.Fatal(err)
	}
	return remit.Sign(b, h.priv, "key_test")
}

func (h *harness) call(body map[string]any) (int, map[string]any) {
	h.t.Helper()
	if _, ok := body["run_id"]; !ok {
		body["run_id"] = "run_01j6zr0000000000000000000a"
	}
	if _, ok := body["step"]; !ok {
		body["step"] = "post"
	}
	b, _ := json.Marshal(body)
	return h.do(http.MethodPost, "/v1/tools/call", b)
}

func (h *harness) do(method, path string, body []byte) (int, map[string]any) {
	h.t.Helper()
	var reader io.Reader
	if body != nil {
		reader = bytes.NewReader(body)
	}
	req, _ := http.NewRequest(method, h.http.URL+path, reader)
	res, err := http.DefaultClient.Do(req)
	if err != nil {
		h.t.Fatal(err)
	}
	defer res.Body.Close()
	data, _ := io.ReadAll(res.Body)
	if h.secrets.Contains(string(data)) {
		h.t.Fatalf("secret leaked in %s %s response: %s", method, path, data)
	}
	var out map[string]any
	if len(data) > 0 && data[0] == '{' {
		json.Unmarshal(data, &out)
	} else if len(data) > 0 && data[0] == '[' {
		var list []any
		json.Unmarshal(data, &list)
		out = map[string]any{"list": list}
	} else {
		out = map[string]any{"text": string(data)}
	}
	return res.StatusCode, out
}

func (h *harness) ledgerRows() int {
	h.t.Helper()
	db, err := sql.Open("sqlite", h.ledger)
	if err != nil {
		h.t.Fatal(err)
	}
	defer db.Close()
	var n int
	if err := db.QueryRow("select count(*) from ledger_entries").Scan(&n); err != nil {
		h.t.Fatal(err)
	}
	return n
}

func nested(m map[string]any, keys ...string) any {
	var cur any = m
	for _, k := range keys {
		mm, ok := cur.(map[string]any)
		if !ok {
			return nil
		}
		cur = mm[k]
	}
	return cur
}

func TestHealthToolsAndMetrics(t *testing.T) {
	h := newHarness(t)
	status, body := h.do(http.MethodGet, "/v1/health", nil)
	if status != 200 || body["ok"] != true || body["version"] != Version {
		t.Fatalf("health = %d %v", status, body)
	}
	conns := body["connectors"].(map[string]any)
	if conns["ledger"] != "healthy" || conns["http"] != "healthy" || conns["test"] != "healthy" {
		t.Fatalf("connectors = %v", conns)
	}
	status, body = h.do(http.MethodGet, "/v1/tools", nil)
	list := body["list"].([]any)
	ids := []string{}
	byID := map[string]map[string]any{}
	for _, item := range list {
		m := item.(map[string]any)
		ids = append(ids, m["id"].(string))
		byID[m["id"].(string)] = m
	}
	if status != 200 || len(ids) != 7 || ids[0] != "http.get" || ids[1] != "ledger.lookup_vendor" || ids[6] != "test.slow" {
		t.Fatalf("tools = %v", ids)
	}
	pe := byID["ledger.post_entry"]
	if pe["writes"] != true || pe["connector"] != "ledger" || pe["scope_derivation"] != "table" || nested(pe, "contract", "required", "entry_id") != "number" || pe["description"] != "Post a journal entry" {
		t.Fatalf("post_entry = %v", pe)
	}
	if byID["http.get"]["writes"] != false || byID["http.get"]["scope_derivation"] != "host" || byID["test.slow"]["scope_derivation"] != "none" {
		t.Fatal("tool flags")
	}
	status, body = h.do(http.MethodGet, "/v1/metrics", nil)
	text := body["text"].(string)
	if status != 200 || !strings.Contains(text, `kernos_gateway_canary_status{connector="ledger"} 1`) || !strings.Contains(text, `kernos_gateway_circuit_open{connector="http"} 0`) || !strings.Contains(text, "kernos_gateway_call_latency_seconds_bucket") {
		t.Fatalf("metrics = %d %s", status, text)
	}
	status, body = h.do(http.MethodGet, "/v1/nothing", nil)
	if status != 404 || nested(body, "error", "code") != "not_found" {
		t.Fatalf("unknown route = %d %v", status, body)
	}
	status, body = h.do(http.MethodPost, "/v1/tools/call", []byte("{not json"))
	if status != 400 || nested(body, "error", "code") != "bad_request" {
		t.Fatalf("bad json = %d %v", status, body)
	}
	status, body = h.do(http.MethodPost, "/v1/tools/call", []byte(`{"tool": "ledger.post_entry", "args": [1], "remit_token": "x"}`))
	if status != 400 {
		t.Fatalf("args not an object = %d %v", status, body)
	}
	status, body = h.do(http.MethodPost, "/v1/tools/call", []byte(`{"remit_token": "x"}`))
	if status != 400 {
		t.Fatalf("missing tool = %d %v", status, body)
	}
	rec := httptest.NewRecorder()
	h.server.recover(http.HandlerFunc(func(http.ResponseWriter, *http.Request) { panic("boom " + secretToken) })).ServeHTTP(rec, httptest.NewRequest(http.MethodGet, "/x", nil))
	if rec.Code != 500 || !strings.Contains(rec.Body.String(), "internal_error") {
		t.Fatalf("panic recovery = %d %s", rec.Code, rec.Body.String())
	}
	if h.secrets.Contains(h.logs.String()) {
		t.Fatal("secret leaked into the log through a panic")
	}
}

func TestEveryRefusalReachesTheKernel(t *testing.T) {
	h := newHarness(t)
	_, otherPriv, _ := ed25519.GenerateKey(rand.Reader)
	now := h.clock().Unix()
	forged := func() string {
		b, _ := json.Marshal(map[string]any{"rid": "rem_forged", "exp": now + 3600, "nbf": now - 10, "autonomy": "supervised", "tools": []string{"ledger.*"}, "scopes": []string{"sql:table:*"}})
		return remit.Sign(b, otherPriv, "key_test")
	}
	postArgs := map[string]any{"invoice_id": "inv-1001", "vendor": "Northwind Dairy", "account": "5100", "amount": 1234.56}
	cases := []struct {
		name     string
		token    string
		runID    string
		tool     string
		args     map[string]any
		reason   string
		detail   string
		remitNil bool
	}{
		{"token_malformed", "krt2.a.b.c", "", "ledger.post_entry", postArgs, "token_malformed", "prefix", true},
		{"token_malformed empty", "", "", "ledger.post_entry", postArgs, "token_malformed", "empty", true},
		{"signature_invalid", forged(), "", "ledger.post_entry", postArgs, "signature_invalid", "does not verify", true},
		{"signature_invalid unknown key", strings.TrimSuffix(h.token(nil), "key_test") + "key_unknown", "", "ledger.post_entry", postArgs, "signature_invalid", "unknown key id", true},
		{"remit_expired", h.token(map[string]any{"exp": now - 1}), "", "ledger.post_entry", postArgs, "remit_expired", "expired", false},
		{"remit_not_yet_valid", h.token(map[string]any{"nbf": now + 100}), "", "ledger.post_entry", postArgs, "remit_not_yet_valid", "valid from", false},
		{"remit_run_mismatch", h.token(nil), "run_01j6zr0000000000000000000b", "ledger.post_entry", postArgs, "remit_run_mismatch", "bound to run", false},
		{"tool_not_in_remit", h.token(map[string]any{"tools": []string{"ledger.lookup_vendor"}}), "", "ledger.post_entry", postArgs, "tool_not_in_remit", "ledger.post_entry not matched by [ledger.lookup_vendor]", false},
		{"scope_not_granted", h.token(map[string]any{"scopes": []string{"sql:table:vendors"}}), "", "ledger.post_entry", postArgs, "scope_not_granted", "sql:table:ledger_entries not granted by [sql:table:vendors]", false},
		{"scope_not_granted http", h.token(map[string]any{"scopes": []string{"http:host:api.halcyon.example"}}), "", "http.get", map[string]any{"url": h.up.srv.URL + "/probe"}, "scope_not_granted", "http:host:127.0.0.1 not granted", false},
		{"scope_not_granted literal", h.token(map[string]any{"scopes": []string{"sql:table:*"}}), "", "test.slow", map[string]any{"ms": 1}, "scope_not_granted", "test:* not granted", false},
		{"autonomy_too_low", h.token(map[string]any{"autonomy": "observe"}), "", "ledger.post_entry", postArgs, "autonomy_too_low", "observe", false},
	}
	for i, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			before := h.kernel.count()
			body := map[string]any{"remit_token": c.token, "tool": c.tool, "args": c.args, "idempotency_key": fmt.Sprintf("ref-%d", i), "lease_id": "lse_1"}
			if c.runID != "" {
				body["run_id"] = c.runID
			}
			status, res := h.call(body)
			if status != 403 || nested(res, "refusal", "reason") != c.reason {
				t.Fatalf("got %d %v, want 403 %s", status, res, c.reason)
			}
			detail, _ := nested(res, "refusal", "detail").(string)
			if !strings.Contains(detail, c.detail) {
				t.Fatalf("detail %q does not mention %q", detail, c.detail)
			}
			if h.kernel.count() != before+1 {
				t.Fatalf("tool.refused was not appended (events %d -> %d)", before, h.kernel.count())
			}
			ev, _ := h.kernel.last()
			wantRun := "run_01j6zr0000000000000000000a"
			if c.runID != "" {
				wantRun = c.runID
			}
			if ev.RunID != wantRun || ev.Remit != c.token || ev.Auth != "Bearer kernel-shared-secret" || ev.Body["kind"] != "tool.refused" {
				t.Fatalf("event = %+v", ev)
			}
			payload := ev.Body["payload"].(map[string]any)
			if payload["reason"] != c.reason || payload["tool"] != c.tool || payload["step"] != "post" || payload["detail"] != detail {
				t.Fatalf("payload = %v", payload)
			}
			if c.remitNil && payload["remit_id"] != nil {
				t.Fatalf("remit_id must be null for an undecodable token: %v", payload)
			}
			if !c.remitNil && payload["remit_id"] != "rem_01j6zq0000000000000000000a" {
				t.Fatalf("remit_id = %v", payload["remit_id"])
			}
			if actor := ev.Body["actor"].(map[string]any); actor["type"] != "gateway" {
				t.Fatalf("actor = %v", actor)
			}
		})
	}
	if n := h.ledgerRows(); n != 0 {
		t.Fatalf("refused calls must not write: %d rows", n)
	}
	// A refusal without run_id is still a refusal, but nothing can be appended.
	before := h.kernel.count()
	status, res := h.call(map[string]any{"remit_token": "bad", "tool": "ledger.post_entry", "run_id": ""})
	if status != 403 || h.kernel.count() != before {
		t.Fatalf("no run_id: %d %v events=%d", status, res, h.kernel.count()-before)
	}
	// observe may read.
	status, res = h.call(map[string]any{"remit_token": h.token(map[string]any{"autonomy": "observe"}), "tool": "ledger.lookup_vendor", "args": map[string]any{"name": "Northwind Dairy"}})
	if status != 200 || res["ok"] != true {
		t.Fatalf("observe read = %d %v", status, res)
	}
	// propose reads but, like observe, does not write at the gateway (A5).
	status, res = h.call(map[string]any{"remit_token": h.token(map[string]any{"autonomy": "propose"}), "tool": "ledger.lookup_vendor", "args": map[string]any{"name": "Northwind Dairy"}})
	if status != 200 {
		t.Fatalf("propose read = %d %v", status, res)
	}
	status, res = h.call(map[string]any{"remit_token": h.token(map[string]any{"autonomy": "propose"}), "tool": "ledger.post_entry", "args": postArgs, "idempotency_key": "inv-propose"})
	if status != 403 || nested(res, "refusal", "reason") != "autonomy_too_low" {
		t.Fatalf("propose write = %d %v", status, res)
	}
	for _, level := range []string{"supervised", "autonomous"} {
		status, res = h.call(map[string]any{"remit_token": h.token(map[string]any{"autonomy": level}), "tool": "ledger.post_entry", "args": postArgs, "idempotency_key": "inv-" + level})
		if status != 200 {
			t.Fatalf("%s write = %d %v", level, status, res)
		}
	}
	_, body := h.do(http.MethodGet, "/v1/metrics", nil)
	text := body["text"].(string)
	for _, reason := range []string{"token_malformed", "signature_invalid", "remit_expired", "remit_not_yet_valid", "remit_run_mismatch", "tool_not_in_remit", "scope_not_granted", "autonomy_too_low"} {
		if !strings.Contains(text, `kernos_gateway_refusals_total{reason="`+reason+`"}`) {
			t.Fatalf("metrics lack refusal reason %s: %s", reason, text)
		}
	}
	if !strings.Contains(text, `kernos_gateway_calls_total{tool="ledger.post_entry",outcome="refused"}`) {
		t.Fatalf("metrics lack refused outcome: %s", text)
	}
}

func TestCallsIdempotencyAndFailures(t *testing.T) {
	h := newHarness(t)
	tok := h.token(nil)
	post := func(key string, args map[string]any) (int, map[string]any) {
		body := map[string]any{"remit_token": tok, "tool": "ledger.post_entry", "args": args}
		if key != "" {
			body["idempotency_key"] = key
		}
		return h.call(body)
	}
	args := map[string]any{"invoice_id": "inv-1001", "vendor": "Northwind Dairy", "account": "5100", "amount": 1234.56}
	status, res := post("inv-1001", args)
	if status != 200 || res["ok"] != true || res["replayed"] != false || res["scope"] != "sql:table:ledger_entries" {
		t.Fatalf("post = %d %v", status, res)
	}
	if nested(res, "result", "entry_id") != float64(1) || nested(res, "result", "posted_at") == nil {
		t.Fatalf("result = %v", res["result"])
	}
	if _, ok := res["latency_ms"].(float64); !ok {
		t.Fatalf("latency_ms missing: %v", res)
	}
	status, res = post("inv-1001", map[string]any{"amount": 1234.56, "account": "5100", "vendor": "Northwind Dairy", "invoice_id": "inv-1001"})
	if status != 200 || res["replayed"] != true || nested(res, "result", "entry_id") != float64(1) {
		t.Fatalf("replay = %d %v", status, res)
	}
	if n := h.ledgerRows(); n != 1 {
		t.Fatalf("replay must not write: %d rows", n)
	}
	status, res = post("inv-1001", map[string]any{"invoice_id": "inv-1001", "vendor": "Northwind Dairy", "account": "5100", "amount": 99.0})
	if status != 409 || nested(res, "error", "code") != "idempotency_conflict" {
		t.Fatalf("conflict = %d %v", status, res)
	}
	status, res = post("", args)
	if status != 422 || nested(res, "error", "code") != "idempotency_key_required" {
		t.Fatalf("write without key = %d %v", status, res)
	}
	status, res = post("inv-1002", map[string]any{"invoice_id": "inv-1002", "vendor": "x"})
	if status != 422 || nested(res, "error", "code") != "args_invalid" {
		t.Fatalf("args invalid = %d %v", status, res)
	}
	issues := nested(res, "error", "details", "issues").([]any)
	if len(issues) != 2 {
		t.Fatalf("issues = %v", issues)
	}
	if n := h.ledgerRows(); n != 1 {
		t.Fatalf("invalid calls must not write: %d rows", n)
	}
	status, res = h.call(map[string]any{"remit_token": tok, "tool": "ledger.lookup_vendor", "args": map[string]any{"name": "Northwind Dairy"}})
	if status != 200 || res["scope"] != "sql:table:vendors" || len(nested(res, "result", "rows").([]any)) != 1 {
		t.Fatalf("lookup = %d %v", status, res)
	}
	h.call(map[string]any{"remit_token": tok, "tool": "ledger.lookup_vendor", "args": map[string]any{"name": "Northwind Dairy"}, "idempotency_key": "read-1"})
	status, res = h.call(map[string]any{"remit_token": tok, "tool": "ledger.lookup_vendor", "args": map[string]any{"name": "Northwind Dairy"}, "idempotency_key": "read-1"})
	if status != 200 || res["replayed"] != true {
		t.Fatalf("keys are honoured on reads: %d %v", status, res)
	}
	status, res = h.call(map[string]any{"remit_token": tok, "tool": "ledger.void_entry", "args": map[string]any{"entry_id": 1, "reason": "run abandoned"}, "idempotency_key": "void-1"})
	if status != 200 || nested(res, "result", "voided_at") == nil {
		t.Fatalf("void = %d %v", status, res)
	}

	status, res = h.call(map[string]any{"remit_token": tok, "tool": "test.fail", "args": map[string]any{}})
	if status != 422 || nested(res, "error", "code") != "deterministic_failure" || nested(res, "error", "deterministic") != true {
		t.Fatalf("test.fail = %d %v", status, res)
	}
	status, res = h.call(map[string]any{"remit_token": tok, "tool": "test.flaky", "args": map[string]any{"fail_times": 1}, "idempotency_key": "flaky-1"})
	if status != 502 || nested(res, "error", "code") != "upstream_error" || nested(res, "error", "deterministic") != false {
		t.Fatalf("flaky first = %d %v", status, res)
	}
	status, res = h.call(map[string]any{"remit_token": tok, "tool": "test.flaky", "args": map[string]any{"fail_times": 1}, "idempotency_key": "flaky-1"})
	if status != 200 || nested(res, "result", "attempts") != float64(2) || res["scope"] != "test:*" {
		t.Fatalf("flaky second = %d %v", status, res)
	}
	status, res = h.call(map[string]any{"remit_token": tok, "tool": "test.slow", "args": map[string]any{"ms": 5}})
	if status != 200 || nested(res, "result", "slept_ms") != float64(5) {
		t.Fatalf("slow = %d %v", status, res)
	}
	status, res = h.call(map[string]any{"remit_token": tok, "tool": "test.slow", "args": map[string]any{"ms": "soon"}})
	if status != 422 || nested(res, "error", "code") != "args_invalid" {
		t.Fatalf("slow bad args = %d %v", status, res)
	}
	status, res = h.call(map[string]any{"remit_token": tok, "tool": "ledger.nope", "args": map[string]any{}})
	if status != 404 || nested(res, "error", "code") != "tool_not_found" {
		t.Fatalf("unknown tool = %d %v", status, res)
	}
	status, res = h.call(map[string]any{"remit_token": tok, "tool": "http.get", "args": map[string]any{"url": h.up.srv.URL + "/probe"}})
	if status != 200 || res["scope"] != "http:host:127.0.0.1" || nested(res, "result", "json", "ok") != true {
		t.Fatalf("http.get = %d %v", status, res)
	}
	status, res = h.call(map[string]any{"remit_token": tok, "tool": "http.get", "args": map[string]any{}})
	if status != 422 || nested(res, "error", "code") != "args_invalid" {
		t.Fatalf("http.get without url = %d %v", status, res)
	}
	wide := h.token(map[string]any{"scopes": []string{"http:host:*"}})
	status, res = h.call(map[string]any{"remit_token": wide, "tool": "http.get", "args": map[string]any{"url": "http://evil.example/x"}})
	if status != 422 || nested(res, "error", "code") != "deterministic_failure" {
		t.Fatalf("disallowed host = %d %v", status, res)
	}
	_, body := h.do(http.MethodGet, "/v1/metrics", nil)
	text := body["text"].(string)
	for _, want := range []string{
		`kernos_gateway_calls_total{tool="ledger.post_entry",outcome="ok"} 1`,
		`kernos_gateway_calls_total{tool="ledger.post_entry",outcome="replayed"} 1`,
		`kernos_gateway_calls_total{tool="ledger.post_entry",outcome="idempotency_conflict"} 1`,
		`kernos_gateway_calls_total{tool="test.fail",outcome="deterministic_failure"} 1`,
		`kernos_gateway_calls_total{tool="test.flaky",outcome="upstream_error"} 1`,
		`kernos_gateway_calls_total{tool="ledger.nope",outcome="tool_not_found"} 1`,
	} {
		if !strings.Contains(text, want) {
			t.Fatalf("metrics lack %q:\n%s", want, text)
		}
	}
	if !strings.Contains(h.logs.String(), `"run_id":"run_01j6zr0000000000000000000a"`) || !strings.Contains(h.logs.String(), `"tool":"ledger.post_entry"`) {
		t.Fatal("call logs must carry run_id and tool")
	}
}

func TestCircuitBreaker(t *testing.T) {
	h := newHarness(t)
	tok := h.token(nil)
	for i := 0; i < 5; i++ {
		status, res := h.call(map[string]any{"remit_token": tok, "tool": "test.flaky", "args": map[string]any{"fail_times": 100}, "idempotency_key": fmt.Sprintf("b-%d", i)})
		if status != 502 || nested(res, "error", "circuit") != nil {
			t.Fatalf("failure %d = %d %v", i+1, status, res)
		}
	}
	status, res := h.call(map[string]any{"remit_token": tok, "tool": "test.slow", "args": map[string]any{"ms": 0}})
	if status != 502 || nested(res, "error", "circuit") != "open" || nested(res, "error", "code") != "upstream_error" {
		t.Fatalf("open circuit = %d %v", status, res)
	}
	_, body := h.do(http.MethodGet, "/v1/metrics", nil)
	if !strings.Contains(body["text"].(string), `kernos_gateway_circuit_open{connector="test"} 1`) {
		t.Fatal("circuit_open metric")
	}
	h.advance(13 * time.Second)
	tok = h.token(nil)
	status, res = h.call(map[string]any{"remit_token": tok, "tool": "test.slow", "args": map[string]any{"ms": 0}})
	if status != 200 {
		t.Fatalf("half-open trial = %d %v", status, res)
	}
	status, _ = h.call(map[string]any{"remit_token": tok, "tool": "test.slow", "args": map[string]any{"ms": 0}})
	if status != 200 {
		t.Fatal("closed after a successful trial")
	}
	// Deterministic failures never open the circuit.
	for i := 0; i < 6; i++ {
		h.call(map[string]any{"remit_token": tok, "tool": "test.fail", "args": map[string]any{}})
	}
	if status, _ := h.call(map[string]any{"remit_token": tok, "tool": "test.slow", "args": map[string]any{"ms": 0}}); status != 200 {
		t.Fatal("deterministic failures must not open the circuit")
	}
}

func TestCanaryQuarantineAndRelease(t *testing.T) {
	h := newHarness(t)
	tok := h.token(nil)
	status, res := h.do(http.MethodPost, "/v1/canaries/http/probe", nil)
	if status != 200 || res["status"] != "healthy" || res["connector"] != "http" {
		t.Fatalf("probe = %d %v", status, res)
	}
	h.up.set("drift")
	status, res = h.do(http.MethodPost, "/v1/canaries/http/probe", nil)
	if status != 200 || res["status"] != "failed" || res["consecutive_failures"] != float64(1) {
		t.Fatalf("first failing probe = %d %v", status, res)
	}
	status, res = h.do(http.MethodPost, "/v1/canaries/http/probe", nil)
	if status != 200 || res["status"] != "quarantined" || res["since"] == nil {
		t.Fatalf("second failing probe = %d %v", status, res)
	}
	missing := nested(res, "contract_diff", "missing").([]any)
	if len(missing) != 1 || missing[0] != "json" {
		t.Fatalf("contract_diff = %v", res["contract_diff"])
	}
	_, list := h.do(http.MethodGet, "/v1/canaries", nil)
	found := false
	for _, item := range list["list"].([]any) {
		m := item.(map[string]any)
		if m["connector"] == "http" && m["status"] == "quarantined" {
			found = true
		}
	}
	if !found {
		t.Fatalf("canaries = %v", list)
	}
	repairs, _ := os.ReadDir(filepath.Join(h.dataDir, "repairs"))
	if len(repairs) != 1 || !strings.HasPrefix(repairs[0].Name(), "http-") {
		t.Fatalf("repair files = %v", repairs)
	}
	_, health := h.do(http.MethodGet, "/v1/health", nil)
	if nested(health, "connectors", "http") != "quarantined" {
		t.Fatalf("health = %v", health)
	}
	_, metrics := h.do(http.MethodGet, "/v1/metrics", nil)
	if !strings.Contains(metrics["text"].(string), `kernos_gateway_canary_status{connector="http"} -1`) {
		t.Fatal("canary metric")
	}
	status, res = h.call(map[string]any{"remit_token": tok, "tool": "http.get", "args": map[string]any{"url": h.up.srv.URL + "/probe"}})
	if status != 503 || nested(res, "error", "code") != "connector_quarantined" || nested(res, "error", "connector") != "http" || nested(res, "error", "since") == nil {
		t.Fatalf("quarantined call = %d %v", status, res)
	}
	if status, _ := h.call(map[string]any{"remit_token": tok, "tool": "ledger.lookup_vendor", "args": map[string]any{"name": "x"}}); status != 200 {
		t.Fatal("other connectors keep serving")
	}
	h.up.set("ok")
	status, res = h.do(http.MethodPost, "/v1/canaries/http/release", nil)
	if status != 200 || res["status"] != "healthy" {
		t.Fatalf("release = %d %v", status, res)
	}
	status, res = h.call(map[string]any{"remit_token": tok, "tool": "http.get", "args": map[string]any{"url": h.up.srv.URL + "/probe"}})
	if status != 200 {
		t.Fatalf("after release = %d %v", status, res)
	}
	if status, _ := h.do(http.MethodPost, "/v1/canaries/nope/probe", nil); status != 404 {
		t.Fatal("unknown connector probe")
	}
	if status, _ := h.do(http.MethodPost, "/v1/canaries/nope/release", nil); status != 404 {
		t.Fatal("unknown connector release")
	}
}

func TestSecretsNeverLeak(t *testing.T) {
	h := newHarness(t)
	tok := h.token(nil)
	status, res := h.call(map[string]any{"remit_token": tok, "tool": "http.get", "args": map[string]any{"url": h.up.srv.URL + "/echo"}})
	if status != 200 {
		t.Fatalf("echo = %d %v", status, res)
	}
	h.up.mu.Lock()
	seen := h.up.seenAuth
	h.up.mu.Unlock()
	if seen != "Bearer "+secretToken {
		t.Fatalf("upstream must receive the real credential, got %q", seen)
	}
	body, _ := nested(res, "result", "body").(string)
	if !strings.Contains(body, config.Redacted) || strings.Contains(body, secretToken) {
		t.Fatalf("an upstream echoing the credential must be redacted: %q", body)
	}
	h.do(http.MethodGet, "/v1/tools", nil)
	h.do(http.MethodGet, "/v1/health", nil)
	h.do(http.MethodGet, "/v1/canaries", nil)
	h.do(http.MethodGet, "/v1/metrics", nil)
	h.server.log.Info("deliberate log of a credential", "authorization", "Bearer "+secretToken, "token", "kernel-shared-secret")
	logs := h.logs.String()
	if strings.Contains(logs, secretToken) || strings.Contains(logs, "kernel-shared-secret") {
		t.Fatalf("secret leaked into the log:\n%s", logs)
	}
	if !strings.Contains(logs, config.Redacted) {
		t.Fatal("the deliberate log line must have been redacted, not dropped")
	}
	if h.secrets.Count() < 2 {
		t.Fatalf("both the credential and the kernel token must be registered: %d", h.secrets.Count())
	}
}

func TestBuildConnectors(t *testing.T) {
	cfg := config.Default()
	cfg.Connectors = []map[string]any{{"name": "x", "type": "nope"}}
	if _, err := BuildConnectors(cfg, nil); err == nil || !strings.Contains(err.Error(), "unknown connector type") {
		t.Fatalf("unknown type: %v", err)
	}
	cfg.Connectors = []map[string]any{{"name": "test", "type": "test"}}
	built, err := BuildConnectors(cfg, nil)
	if err != nil || len(built) != 0 {
		t.Fatalf("test connector must be skipped without the flag: %v %v", built, err)
	}
	cfg.TestTools = true
	cfg.Connectors = nil
	built, err = BuildConnectors(cfg, nil)
	if err != nil || len(built) != 1 || built[0].Connector.Name() != "test" || !built[0].HasProbe {
		t.Fatalf("test connector must be added with the flag: %v %v", built, err)
	}
	cfg.Connectors = []map[string]any{{"name": "ledger", "type": "sqlite"}}
	if _, err := BuildConnectors(cfg, nil); err == nil || !strings.Contains(err.Error(), "path is required") {
		t.Fatalf("factory errors propagate: %v", err)
	}
}

func TestConcurrentSameKeyWritesOnce(t *testing.T) {
	h := newHarness(t)
	tok := h.token(nil)
	var wg sync.WaitGroup
	results := make([]int, 8)
	for i := range results {
		wg.Add(1)
		go func(i int) {
			defer wg.Done()
			status, _ := h.call(map[string]any{"remit_token": tok, "tool": "ledger.post_entry", "idempotency_key": "race", "args": map[string]any{"invoice_id": "inv-race", "vendor": "Northwind Dairy", "account": "5100", "amount": 10.0}})
			results[i] = status
		}(i)
	}
	wg.Wait()
	for i, s := range results {
		if s != 200 {
			t.Fatalf("call %d = %d", i, s)
		}
	}
	if n := h.ledgerRows(); n != 1 {
		t.Fatalf("concurrent calls with one key must write once, got %d rows", n)
	}
}
