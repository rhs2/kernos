package idem

import (
	"context"
	"encoding/json"
	"path/filepath"
	"testing"
	"time"
)

func TestReplayConflictAndExpiry(t *testing.T) {
	ctx := context.Background()
	s, err := Open(filepath.Join(t.TempDir(), "idem.db"))
	if err != nil {
		t.Fatal(err)
	}
	defer s.Close()
	now := time.Date(2026, 9, 4, 12, 0, 0, 0, time.UTC)
	s.SetClock(func() time.Time { return now })

	args := map[string]any{"invoice_id": "inv-1001", "amount": 12.5}
	same := map[string]any{"amount": 12.5, "invoice_id": "inv-1001"}
	if HashArgs(args) != HashArgs(same) {
		t.Fatal("argument order must not change the hash")
	}
	if HashArgs(args) == HashArgs(map[string]any{"invoice_id": "inv-1002"}) {
		t.Fatal("different args must hash differently")
	}
	if e, err := s.Lookup(ctx, "ledger.post_entry", "inv-1001"); err != nil || e != nil {
		t.Fatalf("empty store returned %v %v", e, err)
	}
	if err := s.Save(ctx, "ledger.post_entry", "inv-1001", HashArgs(args), json.RawMessage(`{"entry_id":1}`)); err != nil {
		t.Fatal(err)
	}
	e, err := s.Lookup(ctx, "ledger.post_entry", "inv-1001")
	if err != nil || e == nil {
		t.Fatalf("lookup after save: %v %v", e, err)
	}
	if e.ArgsHash != HashArgs(args) || string(e.Result) != `{"entry_id":1}` || !e.CreatedAt.Equal(now) {
		t.Fatalf("entry = %+v", e)
	}
	if e.ArgsHash == HashArgs(map[string]any{"invoice_id": "inv-1001", "amount": 13.0}) {
		t.Fatal("a different argument set must be a conflict")
	}
	if e, _ := s.Lookup(ctx, "ledger.void_entry", "inv-1001"); e != nil {
		t.Fatal("keys are scoped by tool")
	}
	now = now.Add(TTL - time.Second)
	if e, _ := s.Lookup(ctx, "ledger.post_entry", "inv-1001"); e == nil {
		t.Fatal("entry must live for thirty days")
	}
	now = now.Add(2 * time.Second)
	if e, _ := s.Lookup(ctx, "ledger.post_entry", "inv-1001"); e != nil {
		t.Fatal("expired entry must not be honoured")
	}
	n, err := s.Purge(ctx)
	if err != nil || n != 1 {
		t.Fatalf("purge = %d %v", n, err)
	}
	if err := s.Save(ctx, "ledger.post_entry", "inv-1001", "h2", json.RawMessage(`{"entry_id":2}`)); err != nil {
		t.Fatal(err)
	}
	if e, _ := s.Lookup(ctx, "ledger.post_entry", "inv-1001"); e == nil || string(e.Result) != `{"entry_id":2}` {
		t.Fatal("save after expiry must replace the entry")
	}
}

func TestMemoryStore(t *testing.T) {
	s, err := Open(":memory:")
	if err != nil {
		t.Fatal(err)
	}
	defer s.Close()
	if err := s.Save(context.Background(), "t.x", "k", "h", json.RawMessage(`{}`)); err != nil {
		t.Fatal(err)
	}
	if e, _ := s.Lookup(context.Background(), "t.x", "k"); e == nil {
		t.Fatal("memory store lost the entry")
	}
}
