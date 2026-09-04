package httpconn

import (
	"context"
	"encoding/json"
	"io"
	"net/http"
	"net/http/httptest"
	"net/url"
	"strings"
	"testing"
	"time"

	"github.com/rhs2/kernos/gateway/connect"
)

func newConn(t *testing.T, cfg map[string]any) *Connector {
	t.Helper()
	if cfg["name"] == nil {
		cfg["name"] = "http"
	}
	c, err := New(cfg)
	if err != nil {
		t.Fatal(err)
	}
	return c.(*Connector)
}

func TestGetPostAndAllowList(t *testing.T) {
	var seenAuth, seenBody, seenMethod, seenCT string
	upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		seenAuth = r.Header.Get("Authorization")
		seenMethod = r.Method
		seenCT = r.Header.Get("Content-Type")
		b, _ := io.ReadAll(r.Body)
		seenBody = string(b)
		switch r.URL.Path {
		case "/probe":
			w.Header().Set("Content-Type", "application/json")
			w.Header().Add("X-Multi", "a")
			w.Header().Add("X-Multi", "b")
			w.Write([]byte(`{"ok": true}`))
		case "/text":
			w.WriteHeader(503)
			w.Write([]byte("not json"))
		case "/redirect":
			http.Redirect(w, r, "http://evil.example/", http.StatusFound)
		default:
			w.WriteHeader(404)
		}
	}))
	defer upstream.Close()
	u, _ := url.Parse(upstream.URL)
	c := newConn(t, map[string]any{
		"allowed_hosts": []any{"127.0.0.1", "*.halcyon.example"},
		"headers":       map[string]any{"Authorization": "Bearer halcyon-api-secret"},
		"probe":         map[string]any{"tool": "get", "args": map[string]any{"url": upstream.URL + "/probe"}, "contract": map[string]any{"required": map[string]any{"status": "number", "body": "string", "json": "object"}}},
	})
	tools := c.Tools()
	if len(tools) != 2 || tools[0].ID != "http.get" || tools[0].Writes || tools[1].ID != "http.post" || !tools[1].Writes {
		t.Fatalf("tools = %+v", tools)
	}
	if tools[0].ScopeDerivation != connect.ScopeByHost || tools[0].Contract.Required["status"] != "number" {
		t.Fatalf("get spec = %+v", tools[0])
	}
	scopes, err := c.Scopes("get", map[string]any{"url": upstream.URL + "/probe"})
	if err != nil || len(scopes) != 1 || scopes[0] != "http:host:127.0.0.1" {
		t.Fatalf("Scopes = %v %v", scopes, err)
	}
	if _, err := c.Scopes("get", map[string]any{}); !connect.IsDeterministic(err) {
		t.Fatal("missing url must be deterministic")
	}
	if _, err := c.Scopes("get", map[string]any{"url": "ftp://x/"}); !connect.IsDeterministic(err) {
		t.Fatal("bad scheme must be deterministic")
	}
	ctx := context.Background()
	result, scopes, err := c.Call(ctx, "get", map[string]any{"url": upstream.URL + "/probe", "headers": map[string]any{"X-Run": "run_1"}})
	if err != nil {
		t.Fatal(err)
	}
	if result["status"] != 200 || result["body"] != `{"ok": true}` || scopes[0] != "http:host:127.0.0.1" || seenAuth != "Bearer halcyon-api-secret" || seenMethod != "GET" {
		t.Fatalf("get = %v %v auth=%q", result, scopes, seenAuth)
	}
	if result["json"].(map[string]any)["ok"] != true {
		t.Fatalf("json must be parsed: %v", result["json"])
	}
	if result["headers"].(map[string]any)["X-Multi"] != "a, b" {
		t.Fatalf("headers = %v", result["headers"])
	}
	if d := connect.CheckContract(c.probe.Contract, result); !d.OK() {
		t.Fatalf("probe contract: %s", d)
	}
	probe, err := c.Probe(ctx)
	if err != nil || probe["status"] != 200 {
		t.Fatalf("Probe = %v %v", probe, err)
	}
	result, _, err = c.Call(ctx, "http.post", map[string]any{"url": upstream.URL + "/probe", "body": map[string]any{"a": 1}})
	if err != nil || seenMethod != "POST" || seenBody != `{"a":1}` || seenCT != "application/json" {
		t.Fatalf("post = %v %v method=%s body=%q ct=%q", result, err, seenMethod, seenBody, seenCT)
	}
	_, _, err = c.Call(ctx, "post", map[string]any{"url": upstream.URL + "/probe", "body": "raw text"})
	if err != nil || seenBody != "raw text" || !strings.HasPrefix(seenCT, "text/plain") {
		t.Fatalf("post text body: %v %q %q", err, seenBody, seenCT)
	}
	result, _, err = c.Call(ctx, "get", map[string]any{"url": upstream.URL + "/text"})
	if err != nil || result["status"] != 503 || result["json"] != nil {
		t.Fatalf("non-json 503 is a result, not an error: %v %v", result, err)
	}
	result, _, err = c.Call(ctx, "get", map[string]any{"url": upstream.URL + "/redirect"})
	if err != nil || result["status"] != 302 {
		t.Fatalf("redirects are not followed: %v %v", result, err)
	}
	scopes, err = c.Scopes("get", map[string]any{"url": "http://evil.example/x"})
	if err != nil || scopes[0] != "http:host:evil.example" {
		t.Fatalf("scope for a disallowed host still derives: %v %v", scopes, err)
	}
	_, _, err = c.Call(ctx, "get", map[string]any{"url": "http://evil.example/x"})
	if !connect.IsDeterministic(err) || !strings.Contains(err.Error(), "allowed_hosts") {
		t.Fatalf("disallowed host must fail deterministically: %v", err)
	}
	if !c.HostAllowed("api.halcyon.example") || c.HostAllowed("halcyon.example") || c.HostAllowed("api.halcyon.example.evil") {
		t.Fatal("wildcard host matching")
	}
	if _, _, err := c.Call(ctx, "get", map[string]any{"url": upstream.URL, "body": "x"}); !connect.IsDeterministic(err) {
		t.Fatal("get with a body is deterministic")
	}
	if _, _, err := c.Call(ctx, "delete", map[string]any{"url": upstream.URL}); !connect.IsDeterministic(err) {
		t.Fatal("unknown tool")
	}
	_ = u
}

func TestUpstreamErrorsAreNotDeterministic(t *testing.T) {
	dead := httptest.NewServer(http.HandlerFunc(func(http.ResponseWriter, *http.Request) {}))
	addr := dead.URL
	dead.Close()
	c := newConn(t, map[string]any{"allowed_hosts": []any{"127.0.0.1"}, "timeout_seconds": 0.5})
	_, _, err := c.Call(context.Background(), "get", map[string]any{"url": addr + "/"})
	if err == nil || connect.IsDeterministic(err) {
		t.Fatalf("connection refused must be an upstream error: %v", err)
	}
	slow := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		select {
		case <-time.After(2 * time.Second):
		case <-r.Context().Done():
		}
	}))
	defer slow.Close()
	_, _, err = c.Call(context.Background(), "get", map[string]any{"url": slow.URL + "/"})
	if err == nil || connect.IsDeterministic(err) {
		t.Fatalf("timeout must be an upstream error: %v", err)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 50*time.Millisecond)
	defer cancel()
	_, _, err = c.Call(ctx, "get", map[string]any{"url": slow.URL + "/"})
	if err == nil || connect.IsDeterministic(err) {
		t.Fatalf("context deadline must be an upstream error: %v", err)
	}
	big := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Write(make([]byte, 100))
	}))
	defer big.Close()
	small := newConn(t, map[string]any{"allowed_hosts": []any{"127.0.0.1"}, "max_body_bytes": 10})
	if _, _, err := small.Call(context.Background(), "get", map[string]any{"url": big.URL}); !connect.IsDeterministic(err) {
		t.Fatalf("oversize body: %v", err)
	}
}

func TestConfig(t *testing.T) {
	if _, err := New(map[string]any{"name": "http"}); err == nil {
		t.Fatal("allowed_hosts required")
	}
	if _, err := New(map[string]any{"name": "http", "allowed_hosts": []any{"a"}, "tools": map[string]any{"put": map[string]any{}}}); err == nil {
		t.Fatal("unknown tool must be rejected")
	}
	if _, err := New(map[string]any{"name": "http", "allowed_hosts": []any{"a"}, "probe": map[string]any{"tool": "post"}, "tools": map[string]any{"get": map[string]any{}}}); err == nil {
		t.Fatal("probe must name an exposed tool")
	}
	var cfg map[string]any
	json.Unmarshal([]byte(`{"name":"http","allowed_hosts":["api.halcyon.example","127.0.0.1"],
	  "tools":{"get":{"writes":false,"contract":{"required":{"status":"number","body":"string"}}}}}`), &cfg)
	c := newConn(t, cfg)
	if len(c.Tools()) != 1 || c.Tools()[0].ID != "http.get" {
		t.Fatalf("only listed tools are exposed: %+v", c.Tools())
	}
	if _, ok := c.ProbeSpec(); ok {
		t.Fatal("no probe configured")
	}
	if _, err := c.Probe(context.Background()); !connect.IsDeterministic(err) {
		t.Fatal("probe without configuration is a deterministic error")
	}
}
