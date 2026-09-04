//! The Kernos policy language: parser, evaluator and decision procedure.
//!
//! A policy is a versioned text artefact evaluated against a proposed action and
//! its run context. The language is declarative and total: no loops, no
//! assignment, no side effects, and every input produces a decision.
//!
//! ```
//! use kernos_policy::{evaluate, parse, DecisionKind, LoadedPolicy};
//!
//! let source = r#"
//! policy "finance-default"
//! require approval when action.kind == "payment.issue" and action.amount >= 5000
//!   -> approver: role("finance_admin"), sla: 4h, escalate_to: reporting_line
//! "#;
//! let policy = LoadedPolicy::new("finance-default", 1, parse(source).unwrap());
//! let context = serde_json::json!({
//!     "action": {"kind": "payment.issue", "amount": 7250.0, "writes_to_system_of_record": true},
//!     "run": {"remit": {"autonomy": "supervised"}, "requested_by": {"manager": "u-tom"}}
//! });
//! let decision = evaluate(&[policy], &context);
//! assert_eq!(decision.decision, DecisionKind::ApprovalRequired);
//! assert_eq!(decision.rule, "finance-default@1#0");
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod ast;
pub mod corpus;
pub mod directory;
pub mod duration;
pub mod error;
pub mod eval;
pub mod glob;
pub mod lexer;
pub mod parser;
pub mod value;

pub use ast::{ApprovalClause, ApproverExpr, BinaryOp, EscalateExpr, Expr, Policy, Rule, RuleKind};
pub use corpus::{test_corpus, Flip};
pub use directory::{Directory, DirectoryUser};
pub use duration::parse_duration;
pub use error::ParseError;
pub use eval::{
    default_decision, eval_expr, evaluate, resolve_approver, rule_id, rule_identity, Approver,
    ApproverType, Decision, DecisionKind, EscalateTo, LoadedPolicy, DEFAULT_SLA_SECONDS,
};
pub use glob::glob_match;
pub use parser::parse;
pub use value::{lookup, Value};

/// Parses policy text and pairs it with a registered name and version in one
/// step, which is how the control plane loads policies from its store.
pub fn load(name: &str, version: impl ToString, source: &str) -> Result<LoadedPolicy, ParseError> {
    let policy = parse(source)?;
    Ok(LoadedPolicy::new(name, version, policy))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value as Json};

    const FINANCE_DEFAULT: &str = r#"
# finance-default v1
policy "finance-default"

require approval when
  action.kind == "payment.issue" and action.amount >= 5000
  -> approver: role("finance_admin"), sla: 4h, escalate_to: reporting_line

require approval when
  action.writes_to_system_of_record and run.remit.autonomy == "supervised"
  -> approver: run.requested_by.manager, sla: 24h

require approval when
  action.kind == "code.merge" and action.touches_path("infra/**")
  -> approver: role("platform_owner"), sla: 8h

deny when
  action.touches_data_class("personal") and not run.remit.grants("pii")

allow when
  action.kind == "invoice.read"
"#;

    fn finance() -> Vec<LoadedPolicy> {
        vec![load("finance-default", 1, FINANCE_DEFAULT).expect("parse")]
    }

    fn context(action: Json, autonomy: &str, grants: Vec<&str>, manager: Option<&str>) -> Json {
        let mut requested_by = json!({"id": "u-ana", "role": "ap_clerk"});
        if let Some(m) = manager {
            requested_by["manager"] = json!(m);
        }
        json!({
            "action": action,
            "run": {
                "id": "run_01j6zq5v9k3m8x2w4y7a0b1c2d",
                "department": "finance",
                "bundle": {"name": "halcyon.finance.invoice_intake", "version": "1.0.0"},
                "workflow": "intake",
                "remit": {"autonomy": autonomy, "grants": grants, "tools": ["ledger.*"], "scopes": ["sql:table:*"]},
                "requested_by": requested_by
            }
        })
    }

    fn payment(amount: f64) -> Json {
        json!({"kind": "payment.issue", "amount": amount, "currency": "USD", "writes_to_system_of_record": true,
               "target": "ledger", "data_classes": [], "paths": [], "idempotency_key": "inv-1001",
               "summary": "Pay invoice 1001 to Northwind Dairy"})
    }

    #[test]
    fn large_payment_needs_finance_admin() {
        let d = evaluate(
            &finance(),
            &context(payment(7250.0), "supervised", vec![], Some("u-tom")),
        );
        assert_eq!(d.decision, DecisionKind::ApprovalRequired);
        assert_eq!(d.rule, "finance-default@1#0");
        assert_eq!(d.approver, Some(Approver::role("finance_admin")));
        assert_eq!(d.sla_seconds, Some(14400));
        assert_eq!(d.escalate_to, Some(EscalateTo::reporting_line()));
        assert_eq!(d.policy.as_deref(), Some("finance-default"));
        assert_eq!(d.policy_version.as_deref(), Some("1"));
    }

    #[test]
    fn supervised_write_needs_manager() {
        let d = evaluate(
            &finance(),
            &context(payment(100.0), "supervised", vec![], Some("u-tom")),
        );
        assert_eq!(d.decision, DecisionKind::ApprovalRequired);
        assert_eq!(d.rule, "finance-default@1#1");
        assert_eq!(d.approver, Some(Approver::user("u-tom")));
        assert_eq!(d.sla_seconds, Some(86400));
        assert_eq!(d.escalate_to, Some(EscalateTo::reporting_line()));
    }

    #[test]
    fn missing_manager_falls_back_to_admin() {
        let d = evaluate(
            &finance(),
            &context(payment(100.0), "supervised", vec![], None),
        );
        let approver = d.approver.expect("approver");
        assert_eq!(approver.kind, ApproverType::Role);
        assert_eq!(approver.value, "admin");
        assert!(approver.fallback);
        let serialised = serde_json::to_value(&approver).expect("json");
        assert_eq!(
            serialised,
            json!({"type": "role", "value": "admin", "fallback": true})
        );
        let plain = serde_json::to_value(Approver::user("u-tom")).expect("json");
        assert_eq!(plain, json!({"type": "user", "value": "u-tom"}));
    }

    #[test]
    fn infra_merge_needs_platform_owner() {
        let action = json!({"kind": "code.merge", "writes_to_system_of_record": false, "paths": ["infra/net/main.tf"], "data_classes": []});
        let d = evaluate(&finance(), &context(action, "autonomous", vec![], None));
        assert_eq!(d.decision, DecisionKind::ApprovalRequired);
        assert_eq!(d.rule, "finance-default@1#2");
        assert_eq!(d.approver, Some(Approver::role("platform_owner")));
        let action = json!({"kind": "code.merge", "writes_to_system_of_record": false, "paths": ["app/main.rs"], "data_classes": []});
        let d = evaluate(&finance(), &context(action, "autonomous", vec![], None));
        assert_eq!(d.decision, DecisionKind::Allow);
        assert_eq!(d.rule, "default");
    }

    #[test]
    fn deny_beats_approval_and_allow() {
        let mut action = payment(9000.0);
        action["data_classes"] = json!(["personal"]);
        let d = evaluate(
            &finance(),
            &context(action.clone(), "supervised", vec![], Some("u-tom")),
        );
        assert_eq!(d.decision, DecisionKind::Deny);
        assert_eq!(d.rule, "finance-default@1#3");
        assert!(d.approver.is_none());
        let d = evaluate(
            &finance(),
            &context(action, "supervised", vec!["pii"], Some("u-tom")),
        );
        assert_eq!(d.decision, DecisionKind::ApprovalRequired);
        assert_eq!(d.rule, "finance-default@1#0");
        let read = json!({"kind": "invoice.read", "writes_to_system_of_record": false, "data_classes": ["personal"], "paths": []});
        let d = evaluate(
            &finance(),
            &context(read, "supervised", vec![], Some("u-tom")),
        );
        assert_eq!(d.decision, DecisionKind::Deny);
    }

    #[test]
    fn approval_beats_allow_and_first_approval_rule_wins() {
        let src = r#"
allow when action.kind == "x"
require approval when action.kind == "x" -> approver: user("u-a"), sla: 1h
require approval when action.kind == "x" -> approver: user("u-b"), sla: 2h
"#;
        let p = vec![load("p", 3, src).expect("parse")];
        let d = evaluate(
            &p,
            &context(json!({"kind": "x"}), "autonomous", vec![], None),
        );
        assert_eq!(d.decision, DecisionKind::ApprovalRequired);
        assert_eq!(d.rule, "p@3#1");
        assert_eq!(d.approver, Some(Approver::user("u-a")));
    }

    #[test]
    fn allow_rule_matches() {
        let read = json!({"kind": "invoice.read", "writes_to_system_of_record": false, "data_classes": [], "paths": []});
        let d = evaluate(&finance(), &context(read, "propose", vec![], None));
        assert_eq!(d.decision, DecisionKind::Allow);
        assert_eq!(d.rule, "finance-default@1#4");
    }

    #[test]
    fn default_rule_for_each_autonomy_level() {
        let write = json!({"kind": "email.send", "writes_to_system_of_record": true, "data_classes": [], "paths": []});
        let read = json!({"kind": "email.read", "writes_to_system_of_record": false, "data_classes": [], "paths": []});
        for level in ["observe", "propose", "supervised"] {
            let d = evaluate(&[], &context(write.clone(), level, vec![], Some("u-tom")));
            assert_eq!(d.decision, DecisionKind::ApprovalRequired, "{level}");
            assert_eq!(d.rule, "default");
            assert_eq!(d.approver, Some(Approver::user("u-tom")));
            assert_eq!(d.sla_seconds, Some(86400));
            assert_eq!(d.escalate_to, Some(EscalateTo::reporting_line()));
            let d = evaluate(&[], &context(read.clone(), level, vec![], Some("u-tom")));
            assert_eq!(d.decision, DecisionKind::Allow, "{level}");
            assert_eq!(d.rule, "default");
        }
        let d = evaluate(
            &[],
            &context(write.clone(), "autonomous", vec![], Some("u-tom")),
        );
        assert_eq!(d.decision, DecisionKind::Allow);
        assert_eq!(d.rule, "default");
        // No approval clause on a require approval rule uses the manager default.
        let p = vec![load("p", 1, "require approval when true").expect("parse")];
        let d = evaluate(&p, &context(read, "autonomous", vec![], None));
        assert_eq!(d.rule, "p@1#0");
        assert!(d.approver.expect("approver").fallback);
    }

    #[test]
    fn several_policies_are_concatenated_in_order() {
        let a = load("a", 1, "allow when action.kind == \"x\"").expect("a");
        let b = load("b", 2, "deny when action.kind == \"x\"").expect("b");
        let d = evaluate(
            &[a.clone(), b.clone()],
            &context(json!({"kind": "x"}), "observe", vec![], None),
        );
        assert_eq!(d.decision, DecisionKind::Deny);
        assert_eq!(d.rule, "b@2#0");
        let c = load(
            "c",
            1,
            "require approval when action.kind == \"x\" -> approver: role(\"r\")",
        )
        .expect("c");
        let d = evaluate(
            &[a, c],
            &context(json!({"kind": "x"}), "observe", vec![], None),
        );
        assert_eq!(d.rule, "c@1#0");
    }

    fn value_of(src: &str, ctx: &Json) -> Value {
        let p = parse(&format!("allow when {src}")).expect("parse");
        eval_expr(&p.rules[0].condition, ctx)
    }

    #[test]
    fn null_semantics() {
        let ctx = json!({"action": {"amount": null, "paths": ["a"]}});
        assert_eq!(value_of("action.missing", &ctx), Value::Null);
        assert_eq!(value_of("action.missing.deeper", &ctx), Value::Null);
        assert_eq!(value_of("action.amount == null", &ctx), Value::Bool(true));
        assert_eq!(value_of("action.missing == null", &ctx), Value::Bool(true));
        assert_eq!(value_of("action.missing != null", &ctx), Value::Bool(false));
        assert_eq!(value_of("action.missing < 5", &ctx), Value::Bool(false));
        assert_eq!(value_of("action.missing >= 5", &ctx), Value::Bool(false));
        assert_eq!(value_of("null and true", &ctx), Value::Bool(false));
        assert_eq!(value_of("null or true", &ctx), Value::Bool(true));
        assert_eq!(value_of("not null", &ctx), Value::Bool(true));
        assert_eq!(value_of("action.missing + 1", &ctx), Value::Null);
        assert_eq!(value_of("-action.missing", &ctx), Value::Null);
        assert_eq!(value_of("\"a\" + 1", &ctx), Value::Null);
    }

    #[test]
    fn comparisons_across_types_are_false_except_equality() {
        let ctx = json!({});
        assert_eq!(value_of("1 == \"1\"", &ctx), Value::Bool(false));
        assert_eq!(value_of("1 != \"1\"", &ctx), Value::Bool(true));
        assert_eq!(value_of("1 < \"2\"", &ctx), Value::Bool(false));
        assert_eq!(value_of("\"a\" < \"b\"", &ctx), Value::Bool(true));
        assert_eq!(value_of("true == true", &ctx), Value::Bool(true));
        assert_eq!(value_of("[1, 2] == [1, 2]", &ctx), Value::Bool(true));
        assert_eq!(value_of("2 >= 2", &ctx), Value::Bool(true));
        assert_eq!(value_of("2 > 2", &ctx), Value::Bool(false));
        assert_eq!(value_of("1.5 <= 2", &ctx), Value::Bool(true));
    }

    #[test]
    fn membership_and_arithmetic() {
        let ctx = json!({"action": {"kind": "b", "amount": 10, "tags": ["x", "y"]}});
        assert_eq!(
            value_of("action.kind in [\"a\", \"b\"]", &ctx),
            Value::Bool(true)
        );
        assert_eq!(value_of("\"z\" in action.tags", &ctx), Value::Bool(false));
        assert_eq!(value_of("\"y\" in action.tags", &ctx), Value::Bool(true));
        assert_eq!(value_of("1 in 1", &ctx), Value::Bool(false));
        assert_eq!(value_of("null in [null]", &ctx), Value::Bool(true));
        assert_eq!(value_of("action.amount + 5 - 3", &ctx), Value::Number(12.0));
        assert_eq!(value_of("-action.amount + 5", &ctx), Value::Number(-5.0));
        assert_eq!(value_of("action.amount + 5 >= 15", &ctx), Value::Bool(true));
        assert_eq!(value_of("(1 + 2) - (3 - 4)", &ctx), Value::Number(4.0));
        assert_eq!(
            value_of("1 + 2 == 3 and 2 - 1 == 1", &ctx),
            Value::Bool(true)
        );
    }

    #[test]
    fn short_circuit_and_precedence_of_boolean_operators() {
        let ctx = json!({"a": true, "b": false});
        assert_eq!(value_of("a or b and b", &ctx), Value::Bool(true));
        assert_eq!(value_of("(a or b) and b", &ctx), Value::Bool(false));
        assert_eq!(value_of("not b and a", &ctx), Value::Bool(true));
        assert_eq!(value_of("not (b and a)", &ctx), Value::Bool(true));
        assert_eq!(value_of("not a == b", &ctx), Value::Bool(true));
    }

    #[test]
    fn calls_over_lists() {
        let ctx = json!({"action": {"paths": ["infra/x.tf", "app/y"], "data_classes": ["personal"]}, "run": {"remit": {"grants": ["pii"]}}});
        assert_eq!(
            value_of("action.touches_path(\"infra/**\")", &ctx),
            Value::Bool(true)
        );
        assert_eq!(
            value_of("action.touches_path(\"db/**\")", &ctx),
            Value::Bool(false)
        );
        assert_eq!(
            value_of("action.touches_data_class(\"personal\")", &ctx),
            Value::Bool(true)
        );
        assert_eq!(
            value_of("action.touches_data_class(\"health\")", &ctx),
            Value::Bool(false)
        );
        assert_eq!(
            value_of("run.remit.grants(\"pii\")", &ctx),
            Value::Bool(true)
        );
        assert_eq!(
            value_of("run.remit.grants(\"phi\")", &ctx),
            Value::Bool(false)
        );
        assert_eq!(value_of("run.remit.grants(1)", &ctx), Value::Bool(false));
        let empty = json!({});
        assert_eq!(
            value_of("action.touches_path(\"infra/**\")", &empty),
            Value::Bool(false)
        );
    }

    #[test]
    fn rule_id_format_and_identity() {
        let p = load("finance-default", 7, "allow when true\nallow when true").expect("parse");
        assert_eq!(rule_id(&p, 1), "finance-default@7#1");
        assert_eq!(rule_identity("finance-default@7#1"), "finance-default#1");
        assert_eq!(rule_identity("default"), "default");
        assert_eq!(rule_identity("approved:apr_x"), "approved:apr_x");
    }

    #[test]
    fn corpus_flips_between_thresholds() {
        let a = finance();
        let b = vec![load(
            "finance-default",
            2,
            &FINANCE_DEFAULT.replace("5000", "10000"),
        )
        .expect("parse")];
        let corpus: Vec<Json> = [1000.0, 5000.0, 7250.0, 9999.0, 10000.0, 20000.0]
            .iter()
            .map(|amount| context(payment(*amount), "supervised", vec![], Some("u-tom")))
            .collect();
        let flips = test_corpus(&a, &b, &corpus);
        let indices: Vec<usize> = flips.iter().map(|f| f.index).collect();
        assert_eq!(indices, vec![1, 2, 3]);
        assert_eq!(flips[0].rule_a, "finance-default@1#0");
        assert_eq!(flips[0].rule_b, "finance-default@2#1");
        assert_eq!(flips[0].a, DecisionKind::ApprovalRequired);
        // Autonomous runs flip on decision as well.
        let corpus: Vec<Json> = [7250.0, 20000.0]
            .iter()
            .map(|amount| context(payment(*amount), "autonomous", vec![], Some("u-tom")))
            .collect();
        let flips = test_corpus(&a, &b, &corpus);
        assert_eq!(flips.len(), 1);
        assert_eq!(flips[0].a, DecisionKind::ApprovalRequired);
        assert_eq!(flips[0].b, DecisionKind::Allow);
    }

    #[test]
    fn a_differently_named_policy_with_the_same_gate_is_not_a_flip() {
        // The two sides of a policy test are usually differently named files
        // (finance-default against finance-default-10k). Rows whose decision and
        // approval gate are identical must not be reported just because the rule
        // that matched carries the other file's name.
        let a = finance();
        let b = vec![load(
            "finance-default-10k",
            1,
            &FINANCE_DEFAULT
                .replace("5000", "10000")
                .replace("\"finance-default\"", "\"finance-default-10k\""),
        )
        .expect("parse")];
        let corpus: Vec<Json> = [12000.0, 20000.0]
            .iter()
            .map(|amount| context(payment(*amount), "autonomous", vec![], Some("u-tom")))
            .collect();
        assert!(test_corpus(&a, &b, &corpus).is_empty());
        // Below the raised threshold the decision itself changes, so it flips.
        let corpus: Vec<Json> = [7250.0]
            .iter()
            .map(|amount| context(payment(*amount), "autonomous", vec![], Some("u-tom")))
            .collect();
        let flips = test_corpus(&a, &b, &corpus);
        assert_eq!(flips.len(), 1);
        assert_eq!(flips[0].a, DecisionKind::ApprovalRequired);
        assert_eq!(flips[0].b, DecisionKind::Allow);
        assert_eq!(flips[0].rule_a, "finance-default@1#0");
    }

    #[test]
    fn decisions_serialise_in_wire_form() {
        let d = evaluate(
            &finance(),
            &context(payment(7250.0), "supervised", vec![], Some("u-tom")),
        );
        let json = serde_json::to_value(&d).expect("json");
        assert_eq!(json["decision"], "approval_required");
        assert_eq!(json["escalate_to"], "reporting_line");
        assert_eq!(
            json["approver"],
            json!({"type": "role", "value": "finance_admin"})
        );
        let fixed = EscalateTo::Approver(Approver::role("cfo"));
        assert_eq!(
            serde_json::to_value(&fixed).expect("json"),
            json!({"type": "role", "value": "cfo"})
        );
        let back: EscalateTo = serde_json::from_value(json!("reporting_line")).expect("parse");
        assert_eq!(back, EscalateTo::reporting_line());
    }
}
