// Package testtools is the "test" connector the acceptance suite uses:
// test.slow sleeps, test.flaky fails the first N calls per idempotency key
// with a non-deterministic error, and test.fail always fails
// deterministically. The gateway only builds it when
// KERNOS_GATEWAY_TEST_TOOLS=1.
package testtools

import (
	"context"
	"fmt"
	"sync"
	"time"

	"github.com/rhs2/kernos/gateway/connect"
)

// TypeName is the value of "type" in gateway.json for this connector.
const TypeName = "test"

// DefaultName is the connector name the gateway adds automatically.
const DefaultName = "test"

// MaxSleep caps test.slow so a mistaken argument cannot hold a worker for
// an hour.
const MaxSleep = 10 * time.Minute

func init() {
	connect.Register(New, TypeName)
}

// Connector is the test tool set.
type Connector struct {
	name  string
	mu    sync.Mutex
	flaky map[string]int
	sleep func(context.Context, time.Duration) error
}

// New is the Factory for the test type; it takes only "name".
func New(cfg map[string]any) (connect.Connector, error) {
	name := DefaultName
	if cfg != nil {
		if n, err := connect.ConnectorName(cfg); err == nil && n != "" {
			name = n
		} else if err != nil {
			return nil, err
		}
	}
	return &Connector{name: name, flaky: map[string]int{}, sleep: sleepCtx}, nil
}

func sleepCtx(ctx context.Context, d time.Duration) error {
	if d <= 0 {
		return nil
	}
	timer := time.NewTimer(d)
	defer timer.Stop()
	select {
	case <-ctx.Done():
		return ctx.Err()
	case <-timer.C:
		return nil
	}
}

// Name implements connect.Connector.
func (c *Connector) Name() string { return c.name }

// Tools implements connect.Connector.
func (c *Connector) Tools() []connect.ToolSpec {
	return []connect.ToolSpec{
		{ID: connect.ToolID(c.name, "fail"), Description: "Always fails deterministically", Writes: false,
			InputSchema:     map[string]any{"type": "object"},
			Contract:        connect.Contract{Required: map[string]string{}},
			ScopeDerivation: connect.ScopeNone},
		{ID: connect.ToolID(c.name, "flaky"), Description: "Fails the first fail_times calls per idempotency key with an upstream error", Writes: false,
			InputSchema:     map[string]any{"type": "object", "required": []any{"fail_times"}, "properties": map[string]any{"fail_times": map[string]any{"type": "integer", "minimum": 0}}},
			Contract:        connect.Contract{Required: map[string]string{"ok": connect.TypeBool, "attempts": connect.TypeNumber}},
			ScopeDerivation: connect.ScopeNone},
		{ID: connect.ToolID(c.name, "slow"), Description: "Sleeps ms milliseconds", Writes: false,
			InputSchema:     map[string]any{"type": "object", "required": []any{"ms"}, "properties": map[string]any{"ms": map[string]any{"type": "number", "minimum": 0}}},
			Contract:        connect.Contract{Required: map[string]string{"slept_ms": connect.TypeNumber}},
			ScopeDerivation: connect.ScopeNone},
	}
}

// ProbeSpec implements connect.ProbeDescriber.
func (c *Connector) ProbeSpec() (connect.ProbeSpec, bool) {
	return connect.ProbeSpec{Tool: "ping", Args: map[string]any{}, Contract: connect.Contract{Required: map[string]string{"ok": connect.TypeBool}}}, true
}

// Probe implements connect.Connector; it never fails.
func (c *Connector) Probe(context.Context) (map[string]any, error) {
	return map[string]any{"ok": true}, nil
}

// Call implements connect.Connector.
func (c *Connector) Call(ctx context.Context, toolName string, args map[string]any) (map[string]any, []string, error) {
	switch connect.Operation(c.name, toolName) {
	case "slow":
		ms, _ := number(args["ms"])
		d := time.Duration(ms * float64(time.Millisecond))
		if d > MaxSleep {
			d = MaxSleep
		}
		if err := c.sleep(ctx, d); err != nil {
			return nil, nil, fmt.Errorf("test.slow interrupted: %w", err)
		}
		return map[string]any{"slept_ms": ms}, nil, nil
	case "flaky":
		times, _ := number(args["fail_times"])
		key := connect.CallInfoFrom(ctx).IdempotencyKey
		c.mu.Lock()
		count := c.flaky[key]
		if float64(count) < times {
			c.flaky[key]++
			c.mu.Unlock()
			return nil, nil, fmt.Errorf("test.flaky: simulated upstream failure %d of %d for key %q", count+1, int(times), key)
		}
		c.flaky[key] = count + 1
		c.mu.Unlock()
		return map[string]any{"ok": true, "attempts": count + 1}, nil, nil
	case "fail":
		return nil, nil, connect.Deterministic("test.fail always fails")
	}
	return nil, nil, connect.Deterministic("unknown tool %s", toolName)
}

func number(v any) (float64, bool) {
	switch n := v.(type) {
	case float64:
		return n, true
	case int:
		return float64(n), true
	case int64:
		return float64(n), true
	}
	return 0, false
}
