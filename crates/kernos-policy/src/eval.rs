//! Expression evaluation and the decision procedure.

use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

use crate::ast::{ApproverExpr, BinaryOp, EscalateExpr, Expr, Policy, RuleKind};
use crate::glob::glob_match;
use crate::value::{lookup, Value};

/// A policy as loaded by the control plane: the parsed rules plus the name and
/// version they were registered under, which is what rule ids are built from.
#[derive(Debug, Clone, PartialEq)]
pub struct LoadedPolicy {
    /// The registered policy name (falls back to the header name when loading
    /// unregistered text).
    pub name: String,
    /// The registered version, as text so that draft policies (`draft`) and
    /// numbered versions share one type.
    pub version: String,
    /// The parsed policy.
    pub policy: Policy,
}

impl LoadedPolicy {
    /// Pairs a parsed policy with its registered identity.
    pub fn new(name: impl Into<String>, version: impl ToString, policy: Policy) -> Self {
        LoadedPolicy {
            name: name.into(),
            version: version.to_string(),
            policy,
        }
    }
}

/// The three outcomes of a policy evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionKind {
    /// The action may proceed.
    Allow,
    /// A human must approve before the action proceeds.
    ApprovalRequired,
    /// The action must not proceed.
    Deny,
}

impl DecisionKind {
    /// The wire spelling (`allow`, `approval_required`, `deny`).
    pub fn as_str(self) -> &'static str {
        match self {
            DecisionKind::Allow => "allow",
            DecisionKind::ApprovalRequired => "approval_required",
            DecisionKind::Deny => "deny",
        }
    }
}

/// Whether an approver is a role or a single user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApproverType {
    /// Any actor holding the role may decide.
    Role,
    /// Only this user id may decide.
    User,
}

/// A resolved approver as recorded in `policy.decided` and `approval.requested`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Approver {
    /// Role or user.
    #[serde(rename = "type")]
    pub kind: ApproverType,
    /// The role name or user id.
    pub value: String,
    /// True when a manager path could not be resolved and `role("admin")` was
    /// substituted, as 04-POLICY requires the event to record.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub fallback: bool,
}

impl Approver {
    /// A role approver.
    pub fn role(value: impl Into<String>) -> Self {
        Approver {
            kind: ApproverType::Role,
            value: value.into(),
            fallback: false,
        }
    }

    /// A user approver.
    pub fn user(value: impl Into<String>) -> Self {
        Approver {
            kind: ApproverType::User,
            value: value.into(),
            fallback: false,
        }
    }

    /// The `role("admin")` fallback with `fallback: true` set.
    pub fn admin_fallback() -> Self {
        Approver {
            kind: ApproverType::Role,
            value: "admin".into(),
            fallback: true,
        }
    }

    /// Whether an actor with this id and role may decide for this approver.
    pub fn accepts(&self, actor_id: &str, actor_role: &str) -> bool {
        match self.kind {
            ApproverType::Role => self.value == actor_role,
            ApproverType::User => self.value == actor_id,
        }
    }

    /// The `type:value` spelling used in query strings (`role:finance_admin`).
    pub fn to_query_string(&self) -> String {
        match self.kind {
            ApproverType::Role => format!("role:{}", self.value),
            ApproverType::User => format!("user:{}", self.value),
        }
    }
}

/// Where an unanswered approval escalates. `reporting_line` is resolved at
/// escalation time against the directory; the other forms are fixed targets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EscalateTo {
    /// The literal `reporting_line`.
    ReportingLine(ReportingLineTag),
    /// A fixed approver.
    Approver(Approver),
}

/// Serialises `reporting_line` as the bare string. Exists because serde's
/// untagged enums need a concrete unit representation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReportingLineTag {
    /// The only value.
    #[serde(rename = "reporting_line")]
    ReportingLine,
}

impl EscalateTo {
    /// The `reporting_line` escalation.
    pub fn reporting_line() -> Self {
        EscalateTo::ReportingLine(ReportingLineTag::ReportingLine)
    }
}

/// The result of evaluating a policy set against one action.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Decision {
    /// Allow, approval required or deny.
    pub decision: DecisionKind,
    /// `"<policy>@<version>#<index>"` or `"default"`.
    pub rule: String,
    /// The policy that produced the rule, `None` for the default rule.
    pub policy: Option<String>,
    /// Its version, `None` for the default rule.
    pub policy_version: Option<String>,
    /// The approver when approval is required.
    pub approver: Option<Approver>,
    /// The SLA in seconds when approval is required.
    pub sla_seconds: Option<u64>,
    /// The escalation target when approval is required.
    pub escalate_to: Option<EscalateTo>,
}

/// Default SLA of 24 hours applied by the default rule and by approval rules
/// that omit `sla`.
pub const DEFAULT_SLA_SECONDS: u64 = 86400;

/// Evaluates a policy set (in the order listed on the remit) against a context
/// of the shape documented in 04-POLICY (`{"action": {...}, "run": {...}}`).
/// Rules are evaluated in order across all policies as if concatenated; deny
/// wins, then the first approval rule, then allow, then the default rule.
pub fn evaluate(policies: &[LoadedPolicy], context: &Json) -> Decision {
    let mut first_approval: Option<(usize, usize)> = None;
    let mut first_allow: Option<(usize, usize)> = None;
    for (policy_index, loaded) in policies.iter().enumerate() {
        for (rule_index, rule) in loaded.policy.rules.iter().enumerate() {
            if !eval_expr(&rule.condition, context).truthy() {
                continue;
            }
            match rule.kind {
                RuleKind::Deny => {
                    return Decision {
                        decision: DecisionKind::Deny,
                        rule: rule_id(loaded, rule_index),
                        policy: Some(loaded.name.clone()),
                        policy_version: Some(loaded.version.clone()),
                        approver: None,
                        sla_seconds: None,
                        escalate_to: None,
                    }
                }
                RuleKind::RequireApproval => {
                    if first_approval.is_none() {
                        first_approval = Some((policy_index, rule_index));
                    }
                }
                RuleKind::Allow => {
                    if first_allow.is_none() {
                        first_allow = Some((policy_index, rule_index));
                    }
                }
            }
        }
    }

    if let Some((policy_index, rule_index)) = first_approval {
        let loaded = &policies[policy_index];
        let rule = &loaded.policy.rules[rule_index];
        let (approver, sla, escalate) = match &rule.approval {
            Some(clause) => (
                resolve_approver(&clause.approver, context),
                clause.sla_seconds.unwrap_or(DEFAULT_SLA_SECONDS),
                clause
                    .escalate_to
                    .as_ref()
                    .map(|e| resolve_escalate(e, context))
                    .unwrap_or_else(EscalateTo::reporting_line),
            ),
            None => (
                manager_approver(context),
                DEFAULT_SLA_SECONDS,
                EscalateTo::reporting_line(),
            ),
        };
        return Decision {
            decision: DecisionKind::ApprovalRequired,
            rule: rule_id(loaded, rule_index),
            policy: Some(loaded.name.clone()),
            policy_version: Some(loaded.version.clone()),
            approver: Some(approver),
            sla_seconds: Some(sla),
            escalate_to: Some(escalate),
        };
    }

    if let Some((policy_index, rule_index)) = first_allow {
        let loaded = &policies[policy_index];
        return Decision {
            decision: DecisionKind::Allow,
            rule: rule_id(loaded, rule_index),
            policy: Some(loaded.name.clone()),
            policy_version: Some(loaded.version.clone()),
            approver: None,
            sla_seconds: None,
            escalate_to: None,
        };
    }

    default_decision(context)
}

/// The default rule of 04-POLICY: `allow` for autonomous remits; otherwise
/// approval by the requester's manager for writes to a system of record, and
/// `allow` for everything else.
pub fn default_decision(context: &Json) -> Decision {
    let autonomy = lookup(context, &path(&["run", "remit", "autonomy"]))
        .and_then(Json::as_str)
        .unwrap_or("");
    let writes = lookup(context, &path(&["action", "writes_to_system_of_record"]))
        .and_then(Json::as_bool)
        .unwrap_or(false);
    let allow = Decision {
        decision: DecisionKind::Allow,
        rule: "default".into(),
        policy: None,
        policy_version: None,
        approver: None,
        sla_seconds: None,
        escalate_to: None,
    };
    if autonomy == "autonomous" || !writes {
        return allow;
    }
    Decision {
        decision: DecisionKind::ApprovalRequired,
        rule: "default".into(),
        policy: None,
        policy_version: None,
        approver: Some(manager_approver(context)),
        sla_seconds: Some(DEFAULT_SLA_SECONDS),
        escalate_to: Some(EscalateTo::reporting_line()),
    }
}

/// Builds the `name@version#index` rule id.
pub fn rule_id(policy: &LoadedPolicy, index: usize) -> String {
    format!("{}@{}#{}", policy.name, policy.version, index)
}

/// Strips the version from a rule id so two versions of the same policy can be
/// compared rule for rule (`finance-default@1#0` and `finance-default@2#0` are
/// the same rule). `default` is returned unchanged.
pub fn rule_identity(rule: &str) -> String {
    match (rule.find('@'), rule.rfind('#')) {
        (Some(at), Some(hash)) if at < hash => format!("{}{}", &rule[..at], &rule[hash..]),
        _ => rule.to_string(),
    }
}

fn manager_approver(context: &Json) -> Approver {
    resolve_approver(
        &ApproverExpr::Path(path(&["run", "requested_by", "manager"])),
        context,
    )
}

/// Resolves an approver expression against the context: role and user forms are
/// literal; a path must evaluate to a non-empty string user id, else the
/// `role("admin")` fallback is recorded.
pub fn resolve_approver(expr: &ApproverExpr, context: &Json) -> Approver {
    match expr {
        ApproverExpr::Role(role) => Approver::role(role),
        ApproverExpr::User(user) => Approver::user(user),
        ApproverExpr::Path(segments) => match lookup(context, segments).and_then(Json::as_str) {
            Some(user) if !user.is_empty() => Approver::user(user),
            _ => Approver::admin_fallback(),
        },
    }
}

fn resolve_escalate(expr: &EscalateExpr, context: &Json) -> EscalateTo {
    match expr {
        EscalateExpr::ReportingLine => EscalateTo::reporting_line(),
        EscalateExpr::Role(role) => EscalateTo::Approver(Approver::role(role)),
        EscalateExpr::User(user) => EscalateTo::Approver(Approver::user(user)),
        EscalateExpr::Path(segments) => EscalateTo::Approver(resolve_approver(
            &ApproverExpr::Path(segments.clone()),
            context,
        )),
    }
}

fn path(segments: &[&str]) -> Vec<String> {
    segments.iter().map(|s| s.to_string()).collect()
}

/// Evaluates one expression against the context. Total: every input yields a
/// value, with `null` standing in for anything undefined.
pub fn eval_expr(expr: &Expr, context: &Json) -> Value {
    match expr {
        Expr::Number(n) => Value::Number(*n),
        Expr::Str(s) => Value::Str(s.clone()),
        Expr::Bool(b) => Value::Bool(*b),
        Expr::Null => Value::Null,
        Expr::List(items) => Value::List(items.iter().map(|e| eval_expr(e, context)).collect()),
        Expr::Path(segments) => lookup(context, segments)
            .map(Value::from_json)
            .unwrap_or(Value::Null),
        Expr::Call { path, args } => eval_call(path, args, context),
        Expr::Neg(inner) => match eval_expr(inner, context) {
            Value::Number(n) => Value::Number(-n),
            _ => Value::Null,
        },
        Expr::Not(inner) => Value::Bool(!eval_expr(inner, context).truthy()),
        Expr::Binary { op, left, right } => match op {
            BinaryOp::And => {
                if !eval_expr(left, context).truthy() {
                    Value::Bool(false)
                } else {
                    Value::Bool(eval_expr(right, context).truthy())
                }
            }
            BinaryOp::Or => {
                if eval_expr(left, context).truthy() {
                    Value::Bool(true)
                } else {
                    Value::Bool(eval_expr(right, context).truthy())
                }
            }
            _ => {
                let l = eval_expr(left, context);
                let r = eval_expr(right, context);
                eval_binary(*op, &l, &r)
            }
        },
    }
}

fn eval_binary(op: BinaryOp, l: &Value, r: &Value) -> Value {
    match op {
        BinaryOp::Eq => Value::Bool(l == r),
        BinaryOp::Ne => Value::Bool(l != r),
        BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge => {
            let ordering = match (l, r) {
                (Value::Number(a), Value::Number(b)) => a.partial_cmp(b),
                (Value::Str(a), Value::Str(b)) => Some(a.cmp(b)),
                _ => None,
            };
            Value::Bool(match ordering {
                None => false,
                Some(o) => match op {
                    BinaryOp::Lt => o.is_lt(),
                    BinaryOp::Le => o.is_le(),
                    BinaryOp::Gt => o.is_gt(),
                    _ => o.is_ge(),
                },
            })
        }
        BinaryOp::In => match r {
            Value::List(items) => Value::Bool(items.iter().any(|item| item == l)),
            _ => Value::Bool(false),
        },
        BinaryOp::Add => match (l, r) {
            (Value::Number(a), Value::Number(b)) => Value::Number(a + b),
            _ => Value::Null,
        },
        BinaryOp::Sub => match (l, r) {
            (Value::Number(a), Value::Number(b)) => Value::Number(a - b),
            _ => Value::Null,
        },
        BinaryOp::And | BinaryOp::Or => Value::Null,
    }
}

fn eval_call(callee: &[String], args: &[Expr], context: &Json) -> Value {
    let arg = args
        .first()
        .map(|a| eval_expr(a, context))
        .unwrap_or(Value::Null);
    let Some(needle) = arg.as_str() else {
        return Value::Bool(false);
    };
    let joined = callee.join(".");
    let list_path: Vec<String> = match joined.as_str() {
        "action.touches_path" => path(&["action", "paths"]),
        "action.touches_data_class" => path(&["action", "data_classes"]),
        "run.remit.grants" => path(&["run", "remit", "grants"]),
        _ => return Value::Null,
    };
    let Some(Json::Array(items)) = lookup(context, &list_path) else {
        return Value::Bool(false);
    };
    let hit = items.iter().filter_map(Json::as_str).any(|item| {
        if joined == "action.touches_path" {
            glob_match(needle, item)
        } else {
            item == needle
        }
    });
    Value::Bool(hit)
}
