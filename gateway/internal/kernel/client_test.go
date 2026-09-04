package kernel

import (
	"context"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"testing"
	"time"
)

func TestClient(t *testing.T) {
	var got struct {
		path, auth, remit string
		body              map[string]any
	}
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		got.path = r.URL.Path
		got.auth = r.Header.Get("Authorization")
		got.remit = r.Header.Get("X-Kernos-Remit")
		switch r.URL.Path {
		case "/v1/keys":
			json.NewEncoder(w).Encode(Keys{KeyID: "key_1", Algorithm: "ed25519", PublicKey: "AAAA"})
		case "/v1/runs/run_1/events":
			json.NewDecoder(r.Body).Decode(&got.body)
			w.WriteHeader(201)
			w.Write([]byte(`{"seq": 3, "hash": "x"}`))
		case "/v1/runs/run_missing/events":
			w.WriteHeader(404)
			w.Write([]byte(`{"error": {"code": "run_not_found", "message": "no"}}`))
		}
	}))
	defer srv.Close()
	c := New(srv.URL+"/", "tok", time.Second)
	keys, err := c.FetchKeys(context.Background())
	if err != nil || keys.KeyID != "key_1" || got.auth != "Bearer tok" {
		t.Fatalf("FetchKeys = %+v %v auth=%q", keys, err, got.auth)
	}
	err = c.PostRefused(context.Background(), "run_1", "krt1.x.y.z", map[string]any{"reason": "tool_not_in_remit"})
	if err != nil || got.remit != "krt1.x.y.z" || got.body["kind"] != "tool.refused" {
		t.Fatalf("PostRefused = %v remit=%q body=%v", err, got.remit, got.body)
	}
	actor := got.body["actor"].(map[string]any)
	if actor["type"] != "gateway" {
		t.Fatalf("actor = %v", actor)
	}
	err = c.PostRefused(context.Background(), "run_missing", "", nil)
	kerr, ok := err.(*Error)
	if !ok || kerr.Status != 404 || kerr.Code != "run_not_found" {
		t.Fatalf("expected a typed error, got %v", err)
	}
	if kerr.Error() != "kernel answered 404 run_not_found" {
		t.Fatalf("Error() = %q", kerr.Error())
	}
}
