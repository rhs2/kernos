//! `replay(run)`: the three checks of 01-EVENTS over a stored stream.

use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use kernos_policy::evaluate;

use crate::error::KernelResult;
use crate::events::{EventKind, PolicyDecided, PolicyRef, ZERO_HASH};
use crate::fold::RunState;
use crate::kernel::{load_policy_refs, policy_context};
use crate::store::{self, RunRow};

/// The report of `POST /v1/runs/{id}/replay`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplayReport {
    /// Every hash and link recomputed correctly.
    pub chain_valid: bool,
    /// Number of events replayed.
    pub events: u64,
    /// The fold equals the materialised state.
    pub state_matches: bool,
    /// Number of `policy.decided` events checked.
    pub decisions: u64,
    /// Decisions that came out differently.
    pub decision_mismatches: Vec<Value>,
    /// Chain problems, each with its `seq`.
    pub chain_errors: Vec<Value>,
    /// The folded state (the materialised one when the fold failed).
    pub state: RunState,
    /// The difference when the state does not match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_mismatch: Option<Value>,
}

/// Replays one run inside a store connection.
pub fn replay_run(conn: &Connection, row: &RunRow) -> KernelResult<ReplayReport> {
    let run_id = &row.state.run_id;
    let events = store::all_events(conn, run_id)?;

    // 1. Chain.
    let mut chain_errors = Vec::new();
    let mut prev_hash = ZERO_HASH.to_string();
    for (i, event) in events.iter().enumerate() {
        let expected_seq = i as u64 + 1;
        if event.seq != expected_seq {
            chain_errors
                .push(json!({"seq": event.seq, "error": "seq_gap", "expected": expected_seq}));
        }
        if event.prev_hash != prev_hash {
            chain_errors.push(json!({"seq": event.seq, "error": "prev_hash_mismatch", "expected": prev_hash, "actual": event.prev_hash}));
        }
        let recomputed = event.expected_hash();
        if event.hash != recomputed {
            chain_errors.push(json!({"seq": event.seq, "error": "hash_mismatch", "expected": recomputed, "actual": event.hash}));
        }
        prev_hash = event.hash.clone();
    }
    if row.last_hash != prev_hash {
        chain_errors.push(json!({"seq": events.len(), "error": "last_hash_mismatch", "expected": prev_hash, "actual": row.last_hash}));
    }

    // 2. State.
    let (folded, fold_error) = match crate::fold::fold(&events) {
        Ok(state) => (state, None),
        Err(e) => (row.state.clone(), Some(e.to_string())),
    };
    let mut state_mismatch = fold_error.map(|e| json!({"fold_error": e}));
    let state_matches = state_mismatch.is_none() && folded == row.state;
    if !state_matches && state_mismatch.is_none() {
        state_mismatch = Some(json!({
            "folded": serde_json::to_value(&folded)?,
            "materialised": serde_json::to_value(&row.state)?,
        }));
    }

    // 3. Decisions.
    let remit = store::get_remit(conn, &row.child_remit_id)?
        .or(store::get_remit(conn, &row.state.remit_id)?)
        .map(|r| r.payload);
    let mut decisions = 0u64;
    let mut mismatches = Vec::new();
    for (i, event) in events.iter().enumerate() {
        if event.kind != EventKind::PolicyDecided {
            continue;
        }
        decisions += 1;
        let Ok(decided) = event.typed::<PolicyDecided>() else {
            mismatches.push(json!({"seq": event.seq, "error": "unreadable policy.decided"}));
            continue;
        };
        if let Some(approval_id) = decided.rule.strip_prefix("approved:") {
            let approved_before = events[..i].iter().any(|e| {
                e.kind == EventKind::ApprovalDecided
                    && e.payload.get("approval_id").and_then(Value::as_str) == Some(approval_id)
                    && e.payload.get("decision").and_then(Value::as_str) == Some("approved")
            });
            if !approved_before || decided.decision != "allow" {
                mismatches.push(json!({
                    "seq": event.seq, "action_id": decided.action_id,
                    "recorded": {"decision": decided.decision, "rule": decided.rule},
                    "computed": {"decision": "allow", "rule": decided.rule},
                    "error": "no prior approval for the recorded rule",
                }));
            }
            continue;
        }
        let action = events[..i]
            .iter()
            .rev()
            .find(|e| {
                e.kind == EventKind::ActionProposed
                    && e.payload.get("action_id").and_then(Value::as_str)
                        == Some(decided.action_id.as_str())
            })
            .and_then(|e| e.payload.get("action").cloned());
        let (Some(action), Some(remit)) = (action, remit.as_ref()) else {
            mismatches.push(json!({"seq": event.seq, "action_id": decided.action_id, "error": "action or remit not found"}));
            continue;
        };
        let refs: Vec<PolicyRef> = match &decided.policy_set {
            Some(set) => set.clone(),
            None => match (&decided.policy, decided.policy_version.as_u64()) {
                (Some(name), Some(version)) => vec![PolicyRef {
                    name: name.clone(),
                    version,
                }],
                _ => Vec::new(),
            },
        };
        let policies = match load_policy_refs(conn, &refs) {
            Ok(p) => p,
            Err(e) => {
                mismatches.push(json!({"seq": event.seq, "action_id": decided.action_id, "error": e.to_string()}));
                continue;
            }
        };
        let context = policy_context(&folded, remit, &action);
        let computed = evaluate(&policies, &context);
        if computed.decision.as_str() != decided.decision || computed.rule != decided.rule {
            mismatches.push(json!({
                "seq": event.seq, "action_id": decided.action_id,
                "recorded": {"decision": decided.decision, "rule": decided.rule},
                "computed": {"decision": computed.decision.as_str(), "rule": computed.rule},
            }));
        }
    }

    Ok(ReplayReport {
        chain_valid: chain_errors.is_empty(),
        events: events.len() as u64,
        state_matches,
        decisions,
        decision_mismatches: mismatches,
        chain_errors,
        state: folded,
        state_mismatch,
    })
}
