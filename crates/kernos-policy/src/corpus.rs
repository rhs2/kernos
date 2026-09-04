//! Policy testing against a corpus of historical actions.

use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

use crate::eval::{evaluate, Decision, DecisionKind, LoadedPolicy};

/// One corpus row whose decision differs between two policy versions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Flip {
    /// Position in the corpus.
    pub index: usize,
    /// Decision under policy A.
    pub a: DecisionKind,
    /// Decision under policy B.
    pub b: DecisionKind,
    /// Rule id under A.
    pub rule_a: String,
    /// Rule id under B.
    pub rule_b: String,
}

/// Evaluates every `{action, run}` context in the corpus under both policy sets
/// and returns the rows whose OUTCOME differs. This is the promotion evidence for
/// a policy change.
///
/// The outcome is the decision and, when approval is required, who must approve,
/// within what time, and where it escalates. The matched rule id is reported for
/// context but is deliberately not part of the comparison: the two sides of a test
/// are often differently named policies, and comparing names would report every
/// matched row as a change while saying nothing about what actually changed.
pub fn test_corpus(
    policy_a: &[LoadedPolicy],
    policy_b: &[LoadedPolicy],
    corpus: &[Json],
) -> Vec<Flip> {
    corpus
        .iter()
        .enumerate()
        .filter_map(|(index, context)| {
            let a = evaluate(policy_a, context);
            let b = evaluate(policy_b, context);
            let same = outcome(&a) == outcome(&b);
            if same {
                None
            } else {
                Some(Flip {
                    index,
                    a: a.decision,
                    b: b.decision,
                    rule_a: a.rule,
                    rule_b: b.rule,
                })
            }
        })
        .collect()
}

/// The part of a decision a reviewer cares about: what was decided and, for an
/// approval, the gate it creates.
fn outcome(d: &Decision) -> (DecisionKind, String) {
    let gate = if d.decision == DecisionKind::ApprovalRequired {
        format!("{:?}|{:?}|{:?}", d.approver, d.sla_seconds, d.escalate_to)
    } else {
        String::new()
    };
    (d.decision, gate)
}
