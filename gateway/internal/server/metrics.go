package server

import (
	"fmt"
	"sort"
	"strings"
	"sync"
)

// Call outcomes used as the outcome label of kernos_gateway_calls_total.
const (
	outcomeOK            = "ok"
	outcomeReplayed      = "replayed"
	outcomeRefused       = "refused"
	outcomeArgsInvalid   = "args_invalid"
	outcomeDeterministic = "deterministic_failure"
	outcomeUpstream      = "upstream_error"
	outcomeQuarantined   = "connector_quarantined"
	outcomeCircuitOpen   = "circuit_open"
	outcomeConflict      = "idempotency_conflict"
	outcomeNotFound      = "tool_not_found"
)

var latencyBuckets = []float64{0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1, 2.5, 5, 10, 30, 60}

// metrics is a hand-written Prometheus registry: two counters with labels
// and one histogram. A client library would be the only non-sqlite
// dependency of the module, and the text format is small.
type metrics struct {
	mu       sync.Mutex
	calls    map[string]int64
	refusals map[string]int64
	counts   []int64
	sum      float64
	count    int64
}

func newMetrics() *metrics {
	return &metrics{calls: map[string]int64{}, refusals: map[string]int64{}, counts: make([]int64, len(latencyBuckets))}
}

func (m *metrics) call(tool, outcome string) {
	m.mu.Lock()
	defer m.mu.Unlock()
	m.calls[tool+"\x00"+outcome]++
}

func (m *metrics) refusal(reason string) {
	m.mu.Lock()
	defer m.mu.Unlock()
	m.refusals[reason]++
}

func (m *metrics) observe(seconds float64) {
	m.mu.Lock()
	defer m.mu.Unlock()
	for i, b := range latencyBuckets {
		if seconds <= b {
			m.counts[i]++
		}
	}
	m.sum += seconds
	m.count++
}

func escapeLabel(v string) string {
	v = strings.ReplaceAll(v, `\`, `\\`)
	v = strings.ReplaceAll(v, `"`, `\"`)
	v = strings.ReplaceAll(v, "\n", `\n`)
	return v
}

func (m *metrics) render(b *strings.Builder) {
	m.mu.Lock()
	defer m.mu.Unlock()
	b.WriteString("# HELP kernos_gateway_calls_total Tool calls by tool and outcome.\n# TYPE kernos_gateway_calls_total counter\n")
	keys := make([]string, 0, len(m.calls))
	for k := range m.calls {
		keys = append(keys, k)
	}
	sort.Strings(keys)
	for _, k := range keys {
		parts := strings.SplitN(k, "\x00", 2)
		fmt.Fprintf(b, "kernos_gateway_calls_total{tool=\"%s\",outcome=\"%s\"} %d\n", escapeLabel(parts[0]), escapeLabel(parts[1]), m.calls[k])
	}
	b.WriteString("# HELP kernos_gateway_refusals_total Remit refusals by reason.\n# TYPE kernos_gateway_refusals_total counter\n")
	reasons := make([]string, 0, len(m.refusals))
	for r := range m.refusals {
		reasons = append(reasons, r)
	}
	sort.Strings(reasons)
	for _, r := range reasons {
		fmt.Fprintf(b, "kernos_gateway_refusals_total{reason=\"%s\"} %d\n", escapeLabel(r), m.refusals[r])
	}
	b.WriteString("# HELP kernos_gateway_call_latency_seconds Connector call latency.\n# TYPE kernos_gateway_call_latency_seconds histogram\n")
	for i, bucket := range latencyBuckets {
		fmt.Fprintf(b, "kernos_gateway_call_latency_seconds_bucket{le=\"%g\"} %d\n", bucket, m.counts[i])
	}
	fmt.Fprintf(b, "kernos_gateway_call_latency_seconds_bucket{le=\"+Inf\"} %d\n", m.count)
	fmt.Fprintf(b, "kernos_gateway_call_latency_seconds_sum %g\n", m.sum)
	fmt.Fprintf(b, "kernos_gateway_call_latency_seconds_count %d\n", m.count)
	b.WriteString("# HELP kernos_gateway_canary_status 1 healthy, 0 failed, -1 quarantined.\n# TYPE kernos_gateway_canary_status gauge\n")
}
