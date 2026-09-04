package remit

import (
	"context"
	"crypto/ed25519"
	"crypto/rand"
	"encoding/base64"
	"encoding/json"
	"errors"
	"strings"
	"testing"
	"time"
)

type staticKeys map[string]ed25519.PublicKey

func (s staticKeys) PublicKey(_ context.Context, id string) (ed25519.PublicKey, bool) {
	k, ok := s[id]
	return k, ok
}

var testNow = time.Unix(1_757_000_500, 0)

func basePayload() map[string]any {
	return map[string]any{
		"rid": "rem_01j6zq0000000000000000000a", "run": "run_01j6zr0000000000000000000a", "iss": "key_test",
		"iat": 1_757_000_000, "nbf": 1_757_000_000, "exp": 1_757_086_400,
		"tools": []string{"ledger.*", "http.get"}, "scopes": []string{"sql:table:*", "http:host:api.halcyon.example"},
		"grants": []string{"pii"}, "spend": map[string]any{"tokens": 200000, "usd": 2.0},
		"autonomy": "supervised", "policy_set": []string{"finance-default"},
		"requested_by": map[string]any{"id": "u-ana", "role": "ap_clerk", "manager": "u-tom"},
	}
}

func mint(t *testing.T, priv ed25519.PrivateKey, keyID string, payload map[string]any) string {
	t.Helper()
	b, err := json.Marshal(payload)
	if err != nil {
		t.Fatal(err)
	}
	return Sign(b, priv, keyID)
}

func newKeys(t *testing.T) (ed25519.PublicKey, ed25519.PrivateKey) {
	t.Helper()
	pub, priv, err := ed25519.GenerateKey(rand.Reader)
	if err != nil {
		t.Fatal(err)
	}
	return pub, priv
}

func verifyStatic(t *testing.T, v *Verifier, raw, runID string) (*Token, *Refusal) {
	t.Helper()
	tok, ref := Parse(raw)
	if ref != nil {
		return nil, ref
	}
	if ref := v.CheckSignature(context.Background(), tok); ref != nil {
		return tok, ref
	}
	if ref := CheckTime(tok, testNow); ref != nil {
		return tok, ref
	}
	if ref := CheckRun(tok, runID); ref != nil {
		return tok, ref
	}
	return tok, nil
}

func TestTokenFormat(t *testing.T) {
	pub, priv := newKeys(t)
	raw := mint(t, priv, "key_test", basePayload())
	parts := strings.Split(raw, ".")
	if len(parts) != 4 || parts[0] != "krt1" || parts[3] != "key_test" || strings.ContainsAny(parts[1]+parts[2], "=+/") {
		t.Fatalf("token format: %s", raw)
	}
	tok, ref := Parse(raw)
	if ref != nil {
		t.Fatal(ref)
	}
	payload, _ := base64.RawURLEncoding.DecodeString(parts[1])
	if string(tok.Payload) != string(payload) || len(tok.Signature) != 64 {
		t.Fatal("parsed parts differ from the encoded ones")
	}
	v := &Verifier{Keys: staticKeys{"key_test": pub}}
	if ref := v.CheckSignature(context.Background(), tok); ref != nil {
		t.Fatal(ref)
	}
	if !tok.Decoded() || tok.RemitID() != "rem_01j6zq0000000000000000000a" || tok.Claims.Autonomy != "supervised" || len(tok.Claims.Tools) != 2 {
		t.Fatalf("claims = %+v", tok.Claims)
	}
}

func TestMalformed(t *testing.T) {
	cases := map[string]string{
		"empty":          "",
		"three parts":    "krt1.a.b",
		"five parts":     "krt1.a.b.c.d",
		"bad prefix":     "krt2.YQ.YQ.key",
		"padded payload": "krt1.YQ==.YQ.key",
		"bad base64":     "krt1.!!.YQ.key",
		"bad sig base64": "krt1.YQ.!!.key",
		"empty key id":   "krt1.YQ.YQ.",
		"empty payload":  "krt1..YQ.key",
	}
	for name, raw := range cases {
		if _, ref := Parse(raw); ref == nil || ref.Reason != ReasonTokenMalformed {
			t.Errorf("%s: expected token_malformed, got %v", name, ref)
		}
	}
}

func TestEveryRefusalReason(t *testing.T) {
	pub, priv := newKeys(t)
	_, otherPriv := newKeys(t)
	v := &Verifier{Keys: staticKeys{"key_test": pub}}
	run := "run_01j6zr0000000000000000000a"

	t.Run("signature_invalid wrong key", func(t *testing.T) {
		_, ref := verifyStatic(t, v, mint(t, otherPriv, "key_test", basePayload()), run)
		if ref == nil || ref.Reason != ReasonSignatureInvalid {
			t.Fatalf("got %v", ref)
		}
	})
	t.Run("signature_invalid unknown key id", func(t *testing.T) {
		_, ref := verifyStatic(t, v, mint(t, priv, "key_other", basePayload()), run)
		if ref == nil || ref.Reason != ReasonSignatureInvalid {
			t.Fatalf("got %v", ref)
		}
	})
	t.Run("signature_invalid tampered payload", func(t *testing.T) {
		raw := mint(t, priv, "key_test", basePayload())
		parts := strings.Split(raw, ".")
		p := basePayload()
		p["autonomy"] = "autonomous"
		b, _ := json.Marshal(p)
		parts[1] = base64.RawURLEncoding.EncodeToString(b)
		_, ref := verifyStatic(t, v, strings.Join(parts, "."), run)
		if ref == nil || ref.Reason != ReasonSignatureInvalid {
			t.Fatalf("got %v", ref)
		}
	})
	t.Run("signature_invalid short signature", func(t *testing.T) {
		raw := mint(t, priv, "key_test", basePayload())
		parts := strings.Split(raw, ".")
		parts[2] = "YQ"
		_, ref := verifyStatic(t, v, strings.Join(parts, "."), run)
		if ref == nil || ref.Reason != ReasonSignatureInvalid {
			t.Fatalf("got %v", ref)
		}
	})
	t.Run("token_malformed payload not an object", func(t *testing.T) {
		raw := Sign([]byte(`[1,2]`), priv, "key_test")
		_, ref := verifyStatic(t, v, raw, run)
		if ref == nil || ref.Reason != ReasonTokenMalformed {
			t.Fatalf("got %v", ref)
		}
	})
	t.Run("token_malformed unknown autonomy", func(t *testing.T) {
		p := basePayload()
		p["autonomy"] = "godmode"
		_, ref := verifyStatic(t, v, mint(t, priv, "key_test", p), run)
		if ref == nil || ref.Reason != ReasonTokenMalformed {
			t.Fatalf("got %v", ref)
		}
	})
	t.Run("token_malformed missing exp", func(t *testing.T) {
		p := basePayload()
		delete(p, "exp")
		_, ref := verifyStatic(t, v, mint(t, priv, "key_test", p), run)
		if ref == nil || ref.Reason != ReasonTokenMalformed {
			t.Fatalf("got %v", ref)
		}
	})
	t.Run("remit_expired", func(t *testing.T) {
		p := basePayload()
		p["exp"] = testNow.Unix()
		_, ref := verifyStatic(t, v, mint(t, priv, "key_test", p), run)
		if ref == nil || ref.Reason != ReasonExpired {
			t.Fatalf("got %v", ref)
		}
	})
	t.Run("remit_not_yet_valid", func(t *testing.T) {
		p := basePayload()
		p["nbf"] = testNow.Unix() + 1
		_, ref := verifyStatic(t, v, mint(t, priv, "key_test", p), run)
		if ref == nil || ref.Reason != ReasonNotYetValid {
			t.Fatalf("got %v", ref)
		}
	})
	t.Run("remit_run_mismatch", func(t *testing.T) {
		_, ref := verifyStatic(t, v, mint(t, priv, "key_test", basePayload()), "run_01j6zr0000000000000000000b")
		if ref == nil || ref.Reason != ReasonRunMismatch {
			t.Fatalf("got %v", ref)
		}
		_, ref = verifyStatic(t, v, mint(t, priv, "key_test", basePayload()), "")
		if ref == nil || ref.Reason != ReasonRunMismatch {
			t.Fatalf("missing run_id against a bound token: got %v", ref)
		}
	})
	t.Run("unbound token serves any run", func(t *testing.T) {
		p := basePayload()
		delete(p, "run")
		if _, ref := verifyStatic(t, v, mint(t, priv, "key_test", p), "run_anything"); ref != nil {
			t.Fatalf("got %v", ref)
		}
	})
	tok, ref := verifyStatic(t, v, mint(t, priv, "key_test", basePayload()), run)
	if ref != nil {
		t.Fatal(ref)
	}
	t.Run("tool_not_in_remit", func(t *testing.T) {
		if ref := CheckTool(tok, "ledger.post_entry"); ref != nil {
			t.Fatalf("ledger.* must match ledger.post_entry: %v", ref)
		}
		if ref := CheckTool(tok, "http.get"); ref != nil {
			t.Fatalf("exact tool must match: %v", ref)
		}
		ref := CheckTool(tok, "http.post")
		if ref == nil || ref.Reason != ReasonToolNotInRemit || !strings.Contains(ref.Detail, "http.post not matched by [ledger.*, http.get]") {
			t.Fatalf("got %v", ref)
		}
		if ref := CheckTool(tok, "ledgerx.post"); ref == nil {
			t.Fatal("ledger.* must not match ledgerx.post")
		}
	})
	t.Run("scope_not_granted", func(t *testing.T) {
		if ref := CheckScopes(tok, []string{"sql:table:ledger_entries", "sql:table:vendors", "http:host:api.halcyon.example"}); ref != nil {
			t.Fatalf("granted scopes refused: %v", ref)
		}
		if ref := CheckScopes(tok, nil); ref != nil {
			t.Fatalf("no scopes needed: %v", ref)
		}
		ref := CheckScopes(tok, []string{"sql:table:vendors", "http:host:evil.example"})
		if ref == nil || ref.Reason != ReasonScopeNotGranted || !strings.HasPrefix(ref.Detail, "http:host:evil.example not granted by") {
			t.Fatalf("got %v", ref)
		}
	})
	t.Run("autonomy_too_low", func(t *testing.T) {
		if ref := CheckAutonomy(tok, true); ref != nil {
			t.Fatalf("supervised allows writes: %v", ref)
		}
		p := basePayload()
		p["autonomy"] = "observe"
		obs, ref := verifyStatic(t, v, mint(t, priv, "key_test", p), run)
		if ref != nil {
			t.Fatal(ref)
		}
		if ref := CheckAutonomy(obs, false); ref != nil {
			t.Fatalf("observe allows reads: %v", ref)
		}
		if ref := CheckAutonomy(obs, true); ref == nil || ref.Reason != ReasonAutonomyTooLow {
			t.Fatalf("got %v", ref)
		}
		p["autonomy"] = "propose"
		prop, _ := verifyStatic(t, v, mint(t, priv, "key_test", p), run)
		if ref := CheckAutonomy(prop, false); ref != nil {
			t.Fatalf("propose allows reads: %v", ref)
		}
		if ref := CheckAutonomy(prop, true); ref == nil || ref.Reason != ReasonAutonomyTooLow {
			t.Fatalf("propose is observe at the gateway, got %v", ref)
		}
		for _, level := range []string{"supervised", "autonomous"} {
			p["autonomy"] = level
			lt, _ := verifyStatic(t, v, mint(t, priv, "key_test", p), run)
			if ref := CheckAutonomy(lt, true); ref != nil {
				t.Fatalf("%s must allow writes at the gateway: %v", level, ref)
			}
		}
	})
	if AutonomyRank("observe") >= AutonomyRank("propose") || AutonomyRank("supervised") >= AutonomyRank("autonomous") || AutonomyRank("x") != -1 {
		t.Fatal("AutonomyRank order")
	}
}

func TestSignatureCoversExactBytes(t *testing.T) {
	pub, priv := newKeys(t)
	v := &Verifier{Keys: staticKeys{"key_test": pub}}
	// Non-canonical JSON (extra whitespace, unsorted keys) must verify as long
	// as the bytes signed are the bytes encoded.
	payload := []byte(`{"tools": ["ledger.*"],  "rid":"rem_x", "exp": 1757086400, "nbf": 1757000000, "autonomy":"propose", "scopes":[]}`)
	tok, ref := Parse(Sign(payload, priv, "key_test"))
	if ref != nil {
		t.Fatal(ref)
	}
	if ref := v.CheckSignature(context.Background(), tok); ref != nil {
		t.Fatalf("exact bytes must verify: %v", ref)
	}
	if tok.Claims.RID != "rem_x" || tok.Claims.Autonomy != "propose" {
		t.Fatalf("claims = %+v", tok.Claims)
	}
	if ref := (&Verifier{}).CheckSignature(context.Background(), tok); ref == nil || ref.Reason != ReasonSignatureInvalid {
		t.Fatal("no key resolver must refuse with signature_invalid")
	}
}

func TestKeyStore(t *testing.T) {
	pub, _ := newKeys(t)
	pub2, _ := newKeys(t)
	calls := 0
	current := "key_a"
	fetch := func(context.Context) (string, ed25519.PublicKey, error) {
		calls++
		if calls == 1 {
			return "", nil, errors.New("kernel starting")
		}
		if current == "key_a" {
			return "key_a", pub, nil
		}
		return "key_b", pub2, nil
	}
	ks := NewKeyStore(fetch, time.Hour, nil)
	ks.retry = time.Hour // keep the background loop out of this test
	now := testNow
	ks.now = func() time.Time { return now }
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	started := time.Now()
	ks.Start(ctx)
	if time.Since(started) > time.Second || calls != 1 || ks.HasKeys() {
		t.Fatalf("start must try once and return at once: calls = %d, elapsed %v", calls, time.Since(started))
	}
	if _, ok := ks.PublicKey(ctx, "key_a"); ok || calls != 1 {
		t.Fatalf("an on-demand refetch right after a failed attempt is rate limited, calls = %d", calls)
	}
	now = now.Add(2 * time.Second)
	if k, ok := ks.PublicKey(ctx, "key_a"); !ok || string(k) != string(pub) || calls != 2 {
		t.Fatalf("with no key known, an unknown key id refetches after a second: ok=%v calls=%d", ok, calls)
	}
	if !ks.HasKeys() {
		t.Fatal("HasKeys")
	}
	if _, ok := ks.PublicKey(ctx, "key_b"); ok || calls != 2 {
		t.Fatalf("unknown key within the refetch gap must not refetch, calls = %d", calls)
	}
	current = "key_b"
	now = now.Add(2 * time.Minute)
	if k, ok := ks.PublicKey(ctx, "key_b"); !ok || string(k) != string(pub2) || calls != 3 {
		t.Fatalf("unknown key after the gap must trigger a refetch, ok=%v calls=%d", ok, calls)
	}
	if len(ks.Known()) != 2 {
		t.Fatalf("Known = %v", ks.Known())
	}
	pinned := NewPinnedKeyStore(pub, nil)
	pinned.Start(ctx)
	if k, ok := pinned.PublicKey(ctx, "any_key_id"); !ok || string(k) != string(pub) || !pinned.Pinned() {
		t.Fatal("pinned store serves every key id")
	}
	enc := base64.RawURLEncoding.EncodeToString(pub)
	if parsed, err := ParsePublicKey(enc); err != nil || string(parsed) != string(pub) {
		t.Fatalf("ParsePublicKey raw url: %v", err)
	}
	if parsed, err := ParsePublicKey(base64.StdEncoding.EncodeToString(pub)); err != nil || string(parsed) != string(pub) {
		t.Fatalf("ParsePublicKey std: %v", err)
	}
	if _, err := ParsePublicKey("short"); err == nil {
		t.Fatal("short key must fail")
	}
}
