package canary

import (
	"context"
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/rhs2/kernos/gateway/connect"
)

type fake struct {
	mu       sync.Mutex
	name     string
	result   map[string]any
	err      error
	required []any
	probes   int
}

func (f *fake) Name() string { return f.name }
func (f *fake) Tools() []connect.ToolSpec {
	f.mu.Lock()
	defer f.mu.Unlock()
	return []connect.ToolSpec{{ID: f.name + ".get", InputSchema: map[string]any{"required": f.required}}}
}
func (f *fake) Call(context.Context, string, map[string]any) (map[string]any, []string, error) {
	return nil, nil, nil
}
func (f *fake) Probe(context.Context) (map[string]any, error) {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.probes++
	return f.result, f.err
}
func (f *fake) RefreshTools(context.Context) ([]connect.ToolSpec, error) { return f.Tools(), nil }
func (f *fake) set(result map[string]any, err error) {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.result, f.err = result, err
}

var contract = connect.Contract{Required: map[string]string{"status": "number", "body": "string", "json": "object"}}

func good() map[string]any {
	return map[string]any{"status": float64(200), "body": "{}", "json": map[string]any{"ok": true}}
}

func newManager(t *testing.T, auto bool) (*Manager, *fake, string) {
	t.Helper()
	dir := filepath.Join(t.TempDir(), "repairs")
	now := time.Date(2026, 9, 4, 12, 0, 0, 0, time.UTC)
	m := New(Options{Interval: time.Hour, QuarantineAfter: 2, AutoRelease: auto, RepairDir: dir, Now: func() time.Time {
		now = now.Add(time.Second)
		return now
	}})
	f := &fake{name: "http", result: good(), required: []any{"url"}}
	m.Add(Target{Connector: f, Probe: connect.ProbeSpec{Tool: "get", Args: map[string]any{"url": "http://127.0.0.1:7499/probe"}, Contract: contract}, HasProbe: true})
	return m, f, dir
}

func TestQuarantineRepairAndRelease(t *testing.T) {
	ctx := context.Background()
	m, f, dir := newManager(t, false)
	st, err := m.Probe(ctx, "http")
	if err != nil || st.Status != Healthy || st.LastOkAt == nil || st.ConsecutiveFailures != 0 {
		t.Fatalf("healthy probe: %+v %v", st, err)
	}
	f.set(map[string]any{"status": float64(200), "body": "{}"}, nil)
	st, _ = m.Probe(ctx, "http")
	if st.Status != Failed || st.ConsecutiveFailures != 1 || len(st.ContractDiff.Missing) != 1 || st.ContractDiff.Missing[0] != "json" {
		t.Fatalf("first failure: %+v", st)
	}
	if _, q := m.Quarantined("http"); q {
		t.Fatal("not yet quarantined")
	}
	st, _ = m.Probe(ctx, "http")
	if st.Status != Quarantined || st.ConsecutiveFailures != 2 || st.Since == nil || st.RepairFile == nil {
		t.Fatalf("second failure must quarantine: %+v", st)
	}
	if since, q := m.Quarantined("http"); !q || since != *st.Since {
		t.Fatal("Quarantined must report the since timestamp")
	}
	entries, err := os.ReadDir(dir)
	if err != nil || len(entries) != 1 || !strings.HasPrefix(entries[0].Name(), "http-") || !strings.HasSuffix(entries[0].Name(), ".json") {
		t.Fatalf("repair file: %v %v", entries, err)
	}
	data, _ := os.ReadFile(filepath.Join(dir, entries[0].Name()))
	var rep Repair
	if err := json.Unmarshal(data, &rep); err != nil {
		t.Fatal(err)
	}
	if rep.Connector != "http" || rep.Contract.Required["json"] != "object" || rep.Probe["tool"] != "get" || rep.Diff.Missing[0] != "json" || rep.Observed["body"] != "{}" {
		t.Fatalf("repair content: %s", data)
	}
	if shape := rep.ObservedShape.(map[string]any); shape["status"] != "number" || shape["body"] != "string" {
		t.Fatalf("observed shape: %v", shape)
	}
	var raw map[string]any
	json.Unmarshal(data, &raw)
	if raw["contract"].(map[string]any)["required"] == nil {
		t.Fatalf("contract must marshal with a required key: %s", data)
	}
	// Further failures while quarantined do not write more repair files.
	m.Probe(ctx, "http")
	if entries, _ := os.ReadDir(dir); len(entries) != 1 {
		t.Fatal("one repair file per quarantine")
	}
	// Passing probes without auto release keep the quarantine.
	f.set(good(), nil)
	m.Probe(ctx, "http")
	st, _ = m.Probe(ctx, "http")
	if st.Status != Quarantined || st.PassesSinceQuarantine != 2 {
		t.Fatalf("without auto_release the quarantine must hold: %+v", st)
	}
	st, err = m.Release("http")
	if err != nil || st.Status != Healthy || st.ConsecutiveFailures != 0 || st.Since != nil {
		t.Fatalf("release: %+v %v", st, err)
	}
	if _, err := m.Release("nope"); err == nil {
		t.Fatal("unknown connector must error")
	}
	all := m.All()
	if len(all) != 1 || all[0].Connector != "http" || all[0].Value() != 1 {
		t.Fatalf("All = %+v", all)
	}
}

func TestAutoRelease(t *testing.T) {
	ctx := context.Background()
	m, f, _ := newManager(t, true)
	f.set(nil, errors.New("connection refused"))
	m.Probe(ctx, "http")
	st, _ := m.Probe(ctx, "http")
	if st.Status != Quarantined || st.LastError == nil || !strings.Contains(*st.LastError, "connection refused") {
		t.Fatalf("errors count as failures: %+v", st)
	}
	f.set(good(), nil)
	st, _ = m.Probe(ctx, "http")
	if st.Status != Quarantined || st.PassesSinceQuarantine != 1 {
		t.Fatalf("one pass is not enough: %+v", st)
	}
	f.set(nil, errors.New("flap"))
	st, _ = m.Probe(ctx, "http")
	if st.Status != Quarantined || st.PassesSinceQuarantine != 0 {
		t.Fatalf("a failure resets the pass count: %+v", st)
	}
	f.set(good(), nil)
	m.Probe(ctx, "http")
	st, _ = m.Probe(ctx, "http")
	if st.Status != Healthy || st.Since != nil {
		t.Fatalf("two consecutive passes must auto release: %+v", st)
	}
}

func TestUnexpectedRequired(t *testing.T) {
	ctx := context.Background()
	m, f, _ := newManager(t, false)
	f.mu.Lock()
	f.required = []any{"url", "mode"}
	f.mu.Unlock()
	st, _ := m.Probe(ctx, "http")
	if st.Status != Failed || len(st.ContractDiff.UnexpectedRequired) != 1 || st.ContractDiff.UnexpectedRequired[0] != "get.mode" {
		t.Fatalf("new required input must fail the contract: %+v", st)
	}
	if len(st.ContractDiff.Missing) != 0 {
		t.Fatalf("diff must otherwise be clean: %+v", st.ContractDiff)
	}
}

func TestTypeMismatchAndNoProbe(t *testing.T) {
	ctx := context.Background()
	m, f, _ := newManager(t, false)
	f.set(map[string]any{"status": "200", "body": "{}", "json": map[string]any{}}, nil)
	st, _ := m.Probe(ctx, "http")
	if mm, ok := st.ContractDiff.TypeMismatch["status"]; !ok || mm.Expected != "number" || mm.Observed != "string" {
		t.Fatalf("type mismatch: %+v", st.ContractDiff)
	}
	silent := &fake{name: "ledger", result: nil, err: errors.New("never called")}
	m.Add(Target{Connector: silent, HasProbe: false})
	st, err := m.Probe(ctx, "ledger")
	if err != nil || st.Status != Healthy || st.LastProbeAt != nil {
		t.Fatalf("a connector without a probe stays healthy: %+v %v", st, err)
	}
	if _, err := m.Probe(ctx, "missing"); err == nil {
		t.Fatal("unknown connector")
	}
	if _, ok := m.Get("ledger"); !ok {
		t.Fatal("Get")
	}
}

func TestLoop(t *testing.T) {
	dir := filepath.Join(t.TempDir(), "repairs")
	m := New(Options{Interval: 20 * time.Millisecond, QuarantineAfter: 2, RepairDir: dir})
	f := &fake{name: "http", err: errors.New("down")}
	m.Add(Target{Connector: f, Probe: connect.ProbeSpec{Tool: "get", Contract: contract}, HasProbe: true})
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	m.Start(ctx)
	deadline := time.Now().Add(3 * time.Second)
	for time.Now().Before(deadline) {
		if _, q := m.Quarantined("http"); q {
			break
		}
		time.Sleep(10 * time.Millisecond)
	}
	m.Stop()
	if _, q := m.Quarantined("http"); !q {
		t.Fatal("the loop must quarantine a failing connector")
	}
	f.mu.Lock()
	n := f.probes
	f.mu.Unlock()
	if n < 2 {
		t.Fatalf("probes = %d", n)
	}
}
