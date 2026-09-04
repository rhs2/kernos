// Package canary runs the contract canaries of the gateway specification:
// every connector's probe on a timer, a contract check of the response,
// quarantine after N consecutive failures with a repair request written to
// disk, and release by an operator or automatically after two clean probes.
// It exists so a renamed upstream field stops runs before they produce wrong
// output instead of after.
package canary

import (
	"context"
	"encoding/json"
	"fmt"
	"log/slog"
	"os"
	"path/filepath"
	"sort"
	"sync"
	"time"

	"github.com/rhs2/kernos/gateway/connect"
)

// Connector states as exposed on /v1/canaries and /v1/health.
const (
	Healthy     = "healthy"
	Failed      = "failed"
	Quarantined = "quarantined"
)

// Target is one connector under watch. HasProbe is false when the connector
// has nothing harmless to run; its status then stays healthy.
type Target struct {
	Connector connect.Connector
	Probe     connect.ProbeSpec
	HasProbe  bool
}

// Status is the canary state of one connector, in the JSON shape of
// GET /v1/canaries.
type Status struct {
	Connector             string       `json:"connector"`
	Status                string       `json:"status"`
	LastProbeAt           *string      `json:"last_probe_at"`
	LastOkAt              *string      `json:"last_ok_at"`
	ConsecutiveFailures   int          `json:"consecutive_failures"`
	LastError             *string      `json:"last_error"`
	ContractDiff          connect.Diff `json:"contract_diff"`
	Since                 *string      `json:"since"`
	PassesSinceQuarantine int          `json:"passes_since_quarantine"`
	RepairFile            *string      `json:"repair_file"`
}

// Value is the metric value of a status: 1 healthy, 0 failed, -1 quarantined.
func (s Status) Value() int {
	switch s.Status {
	case Healthy:
		return 1
	case Quarantined:
		return -1
	}
	return 0
}

// Options configure the loop. Zero values take the specification defaults.
type Options struct {
	Interval        time.Duration
	QuarantineAfter int
	AutoRelease     bool
	RepairDir       string
	ProbeTimeout    time.Duration
	Log             *slog.Logger
	Now             func() time.Time
}

type entry struct {
	target   Target
	status   Status
	baseline []string
	probing  sync.Mutex
}

// Manager owns the canary state of every connector.
type Manager struct {
	opts    Options
	mu      sync.Mutex
	entries map[string]*entry
	order   []string
	cancel  context.CancelFunc
	wg      sync.WaitGroup
}

// New builds a manager.
func New(opts Options) *Manager {
	if opts.Interval <= 0 {
		opts.Interval = 60 * time.Second
	}
	if opts.QuarantineAfter <= 0 {
		opts.QuarantineAfter = 2
	}
	if opts.ProbeTimeout <= 0 {
		opts.ProbeTimeout = 10 * time.Second
	}
	if opts.Log == nil {
		opts.Log = slog.Default()
	}
	if opts.Now == nil {
		opts.Now = time.Now
	}
	return &Manager{opts: opts, entries: map[string]*entry{}}
}

// Add registers a connector. When the connector refreshes its tools from
// upstream, the required input parameters seen now become the baseline for
// the unexpected_required check.
func (m *Manager) Add(t Target) {
	name := t.Connector.Name()
	e := &entry{target: t, status: Status{Connector: name, Status: Healthy, ContractDiff: connect.Diff{}.Normalized()}}
	if _, ok := t.Connector.(connect.ToolRefresher); ok {
		e.baseline = connect.RequiredInputs(name, t.Connector.Tools())
	}
	m.mu.Lock()
	defer m.mu.Unlock()
	if _, exists := m.entries[name]; !exists {
		m.order = append(m.order, name)
	}
	m.entries[name] = e
}

// Start probes every connector once and then on every interval, each in its
// own goroutine so a slow upstream does not delay the others. Stop or the
// context ends the loops.
func (m *Manager) Start(ctx context.Context) {
	ctx, m.cancel = context.WithCancel(ctx)
	m.mu.Lock()
	names := append([]string{}, m.order...)
	m.mu.Unlock()
	for _, name := range names {
		e := m.entries[name]
		if !e.target.HasProbe {
			continue
		}
		m.wg.Add(1)
		go func(e *entry) {
			defer m.wg.Done()
			m.probe(ctx, e)
			ticker := time.NewTicker(m.opts.Interval)
			defer ticker.Stop()
			for {
				select {
				case <-ctx.Done():
					return
				case <-ticker.C:
					m.probe(ctx, e)
				}
			}
		}(e)
	}
}

// Stop ends the loops and waits for in-flight probes.
func (m *Manager) Stop() {
	if m.cancel != nil {
		m.cancel()
	}
	m.wg.Wait()
}

// Probe runs a connector's probe now and returns the resulting status.
func (m *Manager) Probe(ctx context.Context, name string) (Status, error) {
	m.mu.Lock()
	e, ok := m.entries[name]
	m.mu.Unlock()
	if !ok {
		return Status{}, fmt.Errorf("unknown connector %q", name)
	}
	return m.probe(ctx, e), nil
}

// Release clears a quarantine (operator action after a repair). The
// connector returns to healthy with its failure count reset; the next probe
// decides afresh.
func (m *Manager) Release(name string) (Status, error) {
	m.mu.Lock()
	defer m.mu.Unlock()
	e, ok := m.entries[name]
	if !ok {
		return Status{}, fmt.Errorf("unknown connector %q", name)
	}
	m.release(e, "operator")
	return e.status, nil
}

func (m *Manager) release(e *entry, by string) {
	e.status.Status = Healthy
	e.status.ConsecutiveFailures = 0
	e.status.PassesSinceQuarantine = 0
	e.status.Since = nil
	e.status.LastError = nil
	e.status.ContractDiff = connect.Diff{}.Normalized()
	m.opts.Log.Info("connector released from quarantine", "connector", e.status.Connector, "by", by)
}

// Get returns one connector's status.
func (m *Manager) Get(name string) (Status, bool) {
	m.mu.Lock()
	defer m.mu.Unlock()
	e, ok := m.entries[name]
	if !ok {
		return Status{}, false
	}
	return e.status, true
}

// All returns every status in registration order.
func (m *Manager) All() []Status {
	m.mu.Lock()
	defer m.mu.Unlock()
	out := make([]Status, 0, len(m.order))
	for _, name := range m.order {
		out = append(out, m.entries[name].status)
	}
	return out
}

// Quarantined reports whether a connector is quarantined and since when.
func (m *Manager) Quarantined(name string) (string, bool) {
	m.mu.Lock()
	defer m.mu.Unlock()
	e, ok := m.entries[name]
	if !ok || e.status.Status != Quarantined {
		return "", false
	}
	since := ""
	if e.status.Since != nil {
		since = *e.status.Since
	}
	return since, true
}

func (m *Manager) probe(ctx context.Context, e *entry) Status {
	e.probing.Lock()
	defer e.probing.Unlock()
	if !e.target.HasProbe {
		m.mu.Lock()
		defer m.mu.Unlock()
		return e.status
	}
	name := e.status.Connector
	pctx, cancel := context.WithTimeout(ctx, m.opts.ProbeTimeout)
	defer cancel()
	result, err := e.target.Connector.Probe(pctx)
	var diff connect.Diff
	failure := err
	if failure == nil {
		diff = connect.CheckContract(e.target.Probe.Contract, result)
		if refresher, ok := e.target.Connector.(connect.ToolRefresher); ok {
			tools, rerr := refresher.RefreshTools(pctx)
			if rerr != nil {
				failure = fmt.Errorf("tool list: %w", rerr)
			} else {
				diff.UnexpectedRequired = newRequired(e.baseline, connect.RequiredInputs(name, tools))
			}
		}
		if failure == nil && !diff.OK() {
			failure = fmt.Errorf("contract violated: %s", diff)
		}
	}
	now := m.opts.Now()
	stamp := connect.Timestamp(now)
	m.mu.Lock()
	st := &e.status
	st.LastProbeAt = &stamp
	st.ContractDiff = diff.Normalized()
	if failure == nil {
		st.ConsecutiveFailures = 0
		st.LastOkAt = &stamp
		st.LastError = nil
		if st.Status == Quarantined {
			st.PassesSinceQuarantine++
			if m.opts.AutoRelease && st.PassesSinceQuarantine >= 2 {
				m.release(e, "auto_release")
			}
		} else {
			st.Status = Healthy
		}
	} else {
		msg := failure.Error()
		st.LastError = &msg
		st.ConsecutiveFailures++
		if st.Status == Quarantined {
			st.PassesSinceQuarantine = 0
		} else {
			st.Status = Failed
			if st.ConsecutiveFailures >= m.opts.QuarantineAfter {
				st.Status = Quarantined
				st.Since = &stamp
				st.PassesSinceQuarantine = 0
				if path, werr := m.writeRepair(e, now, result, diff, failure); werr != nil {
					m.opts.Log.Error("could not write repair request", "connector", name, "error", werr.Error())
				} else {
					st.RepairFile = &path
				}
				m.opts.Log.Error("connector quarantined", "connector", name, "consecutive_failures", st.ConsecutiveFailures, "error", msg, "contract_diff", diff.String())
			}
		}
	}
	snapshot := *st
	m.mu.Unlock()
	if failure != nil {
		m.opts.Log.Warn("canary probe failed", "connector", name, "status", snapshot.Status, "consecutive_failures", snapshot.ConsecutiveFailures, "error", failure.Error())
	} else {
		m.opts.Log.Debug("canary probe ok", "connector", name, "status", snapshot.Status)
	}
	return snapshot
}

func newRequired(baseline, current []string) []string {
	known := map[string]bool{}
	for _, b := range baseline {
		known[b] = true
	}
	var out []string
	for _, c := range current {
		if !known[c] {
			out = append(out, c)
		}
	}
	sort.Strings(out)
	return out
}

// Repair is the repair request written when a connector is quarantined: the
// ticket the system files against itself.
type Repair struct {
	Connector           string           `json:"connector"`
	QuarantinedAt       string           `json:"quarantined_at"`
	ConsecutiveFailures int              `json:"consecutive_failures"`
	Probe               map[string]any   `json:"probe"`
	Contract            connect.Contract `json:"contract"`
	Observed            map[string]any   `json:"observed"`
	ObservedShape       any              `json:"observed_shape"`
	Diff                connect.Diff     `json:"diff"`
	Error               string           `json:"error"`
	BaselineRequired    []string         `json:"baseline_required_inputs,omitempty"`
}

func (m *Manager) writeRepair(e *entry, now time.Time, observed map[string]any, diff connect.Diff, failure error) (string, error) {
	if m.opts.RepairDir == "" {
		return "", fmt.Errorf("no repair directory configured")
	}
	if err := os.MkdirAll(m.opts.RepairDir, 0o755); err != nil {
		return "", err
	}
	args := e.target.Probe.Args
	if args == nil {
		args = map[string]any{}
	}
	contract := e.target.Probe.Contract
	if contract.Required == nil {
		contract.Required = map[string]string{}
	}
	if observed == nil {
		observed = map[string]any{}
	}
	rep := Repair{
		Connector:           e.status.Connector,
		QuarantinedAt:       connect.Timestamp(now),
		ConsecutiveFailures: e.status.ConsecutiveFailures,
		Probe:               map[string]any{"tool": e.target.Probe.Tool, "args": args},
		Contract:            contract,
		Observed:            observed,
		ObservedShape:       connect.Shape(observed),
		Diff:                diff.Normalized(),
		Error:               failure.Error(),
		BaselineRequired:    e.baseline,
	}
	data, err := json.MarshalIndent(rep, "", "  ")
	if err != nil {
		return "", err
	}
	name := fmt.Sprintf("%s-%s.json", e.status.Connector, now.UTC().Format("20060102T150405.000Z"))
	path := filepath.Join(m.opts.RepairDir, name)
	if err := os.WriteFile(path, data, 0o644); err != nil {
		return "", err
	}
	return path, nil
}
