//! In-process counters rendered as Prometheus text by the HTTP layer.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

const LATENCY_BUCKETS: &[f64] = &[
    0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0,
];

/// Counters and histograms the kernel keeps in memory since the last start.
/// Gauges (runs by state, pending approvals) are read from the store at render
/// time so they are exact.
#[derive(Debug, Default)]
pub struct Metrics {
    steps_leased_total: AtomicU64,
    leases_expired_total: AtomicU64,
    events_total: Mutex<BTreeMap<String, u64>>,
    usage_usd_total: Mutex<BTreeMap<String, f64>>,
    latency_counts: Mutex<Vec<u64>>,
    latency_sum: Mutex<f64>,
    latency_count: AtomicU64,
}

impl Metrics {
    /// Fresh, zeroed metrics.
    pub fn new() -> Self {
        Metrics {
            latency_counts: Mutex::new(vec![0; LATENCY_BUCKETS.len() + 1]),
            ..Default::default()
        }
    }

    /// Counts a lease issued.
    pub fn step_leased(&self) {
        self.steps_leased_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Counts a lease swept for expiry.
    pub fn lease_expired(&self) {
        self.leases_expired_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Counts an event appended, by kind.
    pub fn event(&self, kind: &str) {
        if let Ok(mut map) = self.events_total.lock() {
            *map.entry(kind.to_string()).or_insert(0) += 1;
        }
    }

    /// Adds spend for a department.
    pub fn usage_usd(&self, department: &str, usd: f64) {
        if let Ok(mut map) = self.usage_usd_total.lock() {
            *map.entry(department.to_string()).or_insert(0.0) += usd;
        }
    }

    /// Records the wall time of one step attempt from lease to outcome.
    pub fn step_latency(&self, seconds: f64) {
        if let Ok(mut counts) = self.latency_counts.lock() {
            let index = LATENCY_BUCKETS
                .iter()
                .position(|b| seconds <= *b)
                .unwrap_or(LATENCY_BUCKETS.len());
            counts[index] += 1;
        }
        if let Ok(mut sum) = self.latency_sum.lock() {
            *sum += seconds;
        }
        self.latency_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Renders everything in Prometheus text exposition format.
    pub fn render(&self, runs_by_state: &BTreeMap<String, u64>, approvals_pending: u64) -> String {
        let mut out = String::new();
        out.push_str("# HELP kernos_runs Runs by state.\n# TYPE kernos_runs gauge\n");
        for state in [
            "created",
            "running",
            "parked",
            "completed",
            "failed",
            "abandoned",
        ] {
            let count = runs_by_state.get(state).copied().unwrap_or(0);
            out.push_str(&format!("kernos_runs{{state=\"{state}\"}} {count}\n"));
        }
        out.push_str("# HELP kernos_steps_leased_total Leases issued.\n# TYPE kernos_steps_leased_total counter\n");
        out.push_str(&format!(
            "kernos_steps_leased_total {}\n",
            self.steps_leased_total.load(Ordering::Relaxed)
        ));
        out.push_str("# HELP kernos_leases_expired_total Leases swept for expiry.\n# TYPE kernos_leases_expired_total counter\n");
        out.push_str(&format!(
            "kernos_leases_expired_total {}\n",
            self.leases_expired_total.load(Ordering::Relaxed)
        ));
        out.push_str("# HELP kernos_approvals_pending Approvals awaiting a decision.\n# TYPE kernos_approvals_pending gauge\n");
        out.push_str(&format!("kernos_approvals_pending {approvals_pending}\n"));
        out.push_str("# HELP kernos_events_total Events appended by kind.\n# TYPE kernos_events_total counter\n");
        if let Ok(map) = self.events_total.lock() {
            for (kind, count) in map.iter() {
                out.push_str(&format!("kernos_events_total{{kind=\"{kind}\"}} {count}\n"));
            }
        }
        out.push_str("# HELP kernos_usage_usd_total Spend recorded by department.\n# TYPE kernos_usage_usd_total counter\n");
        if let Ok(map) = self.usage_usd_total.lock() {
            for (department, usd) in map.iter() {
                out.push_str(&format!(
                    "kernos_usage_usd_total{{department=\"{department}\"}} {usd}\n"
                ));
            }
        }
        out.push_str("# HELP kernos_step_latency_seconds Step attempt duration from lease to outcome.\n# TYPE kernos_step_latency_seconds histogram\n");
        if let Ok(counts) = self.latency_counts.lock() {
            let mut cumulative = 0u64;
            for (i, bucket) in LATENCY_BUCKETS.iter().enumerate() {
                cumulative += counts[i];
                out.push_str(&format!(
                    "kernos_step_latency_seconds_bucket{{le=\"{bucket}\"}} {cumulative}\n"
                ));
            }
            cumulative += counts[LATENCY_BUCKETS.len()];
            out.push_str(&format!(
                "kernos_step_latency_seconds_bucket{{le=\"+Inf\"}} {cumulative}\n"
            ));
        }
        let sum = self.latency_sum.lock().map(|s| *s).unwrap_or(0.0);
        out.push_str(&format!("kernos_step_latency_seconds_sum {sum}\n"));
        out.push_str(&format!(
            "kernos_step_latency_seconds_count {}\n",
            self.latency_count.load(Ordering::Relaxed)
        ));
        out
    }
}
