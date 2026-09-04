package testtools

import (
	"context"
	"testing"
	"time"

	"github.com/rhs2/kernos/gateway/connect"
)

func TestTools(t *testing.T) {
	conn, err := New(map[string]any{"name": "test"})
	if err != nil {
		t.Fatal(err)
	}
	c := conn.(*Connector)
	ids := []string{}
	for _, tl := range c.Tools() {
		ids = append(ids, tl.ID)
		if tl.Writes || tl.ScopeDerivation != connect.ScopeNone {
			t.Fatalf("test tools are reads with no scope derivation: %+v", tl)
		}
	}
	if len(ids) != 3 || ids[0] != "test.fail" || ids[1] != "test.flaky" || ids[2] != "test.slow" {
		t.Fatalf("ids = %v", ids)
	}
	ctx := context.Background()

	start := time.Now()
	result, _, err := c.Call(ctx, "slow", map[string]any{"ms": float64(30)})
	if err != nil || result["slept_ms"] != float64(30) || time.Since(start) < 30*time.Millisecond {
		t.Fatalf("slow = %v %v after %v", result, err, time.Since(start))
	}
	cctx, cancel := context.WithTimeout(ctx, 20*time.Millisecond)
	defer cancel()
	if _, _, err := c.Call(cctx, "test.slow", map[string]any{"ms": float64(5000)}); err == nil || connect.IsDeterministic(err) {
		t.Fatalf("interrupted sleep is an upstream error: %v", err)
	}
	c.sleep = func(_ context.Context, d time.Duration) error {
		if d != MaxSleep {
			t.Fatalf("sleep must be capped, got %v", d)
		}
		return nil
	}
	c.Call(ctx, "slow", map[string]any{"ms": float64(1e12)})

	keyed := connect.WithCallInfo(ctx, connect.CallInfo{IdempotencyKey: "inv-1"})
	for i := 1; i <= 2; i++ {
		if _, _, err := c.Call(keyed, "flaky", map[string]any{"fail_times": float64(2)}); err == nil || connect.IsDeterministic(err) {
			t.Fatalf("call %d must fail non-deterministically: %v", i, err)
		}
	}
	result, _, err = c.Call(keyed, "flaky", map[string]any{"fail_times": float64(2)})
	if err != nil || result["ok"] != true || result["attempts"] != 3 {
		t.Fatalf("third call succeeds: %v %v", result, err)
	}
	other := connect.WithCallInfo(ctx, connect.CallInfo{IdempotencyKey: "inv-2"})
	if _, _, err := c.Call(other, "flaky", map[string]any{"fail_times": float64(1)}); err == nil {
		t.Fatal("counts are per key")
	}
	if result, _, err := c.Call(ctx, "flaky", map[string]any{"fail_times": float64(0)}); err != nil || result["attempts"] != 1 {
		t.Fatalf("zero fail_times succeeds at once: %v %v", result, err)
	}
	if _, _, err := c.Call(ctx, "fail", map[string]any{}); !connect.IsDeterministic(err) {
		t.Fatalf("fail must be deterministic: %v", err)
	}
	if _, _, err := c.Call(ctx, "other", nil); !connect.IsDeterministic(err) {
		t.Fatal("unknown tool")
	}
	probe, err := c.Probe(ctx)
	spec, ok := c.ProbeSpec()
	if err != nil || probe["ok"] != true || !ok || !connect.CheckContract(spec.Contract, probe).OK() {
		t.Fatalf("probe = %v %v %+v", probe, err, spec)
	}
	if c2, err := New(nil); err != nil || c2.Name() != "test" {
		t.Fatal("nil config gives the default name")
	}
	if _, err := New(map[string]any{"name": "Bad Name"}); err == nil {
		t.Fatal("invalid name")
	}
}
