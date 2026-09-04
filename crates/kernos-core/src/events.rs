//! The event record of 01-EVENTS: one append-only, hash-chained stream per run.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::canonical::{canonical_bytes, sha256_hex};

/// The event schema version carried by every record.
pub const SCHEMA: &str = "kernos.events/1";

/// `prev_hash` of the first event of a run.
pub const ZERO_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// The payload size limit; larger tool results are stored truncated.
pub const MAX_PAYLOAD_BYTES: usize = 256 * 1024;

/// Who appended an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActorType {
    /// The kernel itself.
    Kernel,
    /// A reasoning worker.
    Worker,
    /// The gateway.
    Gateway,
    /// The policy engine.
    Policy,
    /// A human.
    User,
    /// A background task such as a sweeper.
    System,
}

/// The `actor` field of an event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Actor {
    /// The actor category.
    #[serde(rename = "type")]
    pub kind: ActorType,
    /// The actor id (worker id, user id, policy name, ...).
    pub id: String,
}

impl Actor {
    /// The kernel actor.
    pub fn kernel() -> Self {
        Actor {
            kind: ActorType::Kernel,
            id: "kernos".into(),
        }
    }

    /// A worker actor.
    pub fn worker(id: impl Into<String>) -> Self {
        Actor {
            kind: ActorType::Worker,
            id: id.into(),
        }
    }

    /// A policy actor, named after the policy set that decided.
    pub fn policy(id: impl Into<String>) -> Self {
        Actor {
            kind: ActorType::Policy,
            id: id.into(),
        }
    }

    /// A human actor.
    pub fn user(id: impl Into<String>) -> Self {
        Actor {
            kind: ActorType::User,
            id: id.into(),
        }
    }

    /// A system actor such as a sweeper.
    pub fn system(id: impl Into<String>) -> Self {
        Actor {
            kind: ActorType::System,
            id: id.into(),
        }
    }
}

macro_rules! event_kinds {
    ($( $variant:ident => $name:literal, $external:expr; )*) => {
        /// Every event kind of 01-EVENTS. External actors may append only the
        /// kinds for which [`EventKind::is_external`] is true.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub enum EventKind {
            $(
                #[doc = $name]
                #[serde(rename = $name)]
                $variant,
            )*
        }

        impl EventKind {
            /// All kinds, in the order of the specification table.
            pub const ALL: &'static [EventKind] = &[$(EventKind::$variant),*];

            /// The wire name, such as `tool.called`.
            pub fn as_str(self) -> &'static str {
                match self { $(EventKind::$variant => $name,)* }
            }

            /// Parses a wire name.
            pub fn parse(name: &str) -> Option<EventKind> {
                match name { $($name => Some(EventKind::$variant),)* _ => None }
            }

            /// True for the kinds workers and the gateway may append.
            pub fn is_external(self) -> bool {
                match self { $(EventKind::$variant => $external,)* }
            }
        }
    };
}

event_kinds! {
    RunCreated => "run.created", false;
    StepScheduled => "step.scheduled", false;
    StepLeased => "step.leased", false;
    StepLeaseExpired => "step.lease_expired", false;
    StepCompleted => "step.completed", false;
    StepFailed => "step.failed", false;
    StepRetryScheduled => "step.retry_scheduled", false;
    StepQuarantined => "step.quarantined", false;
    StepEscalated => "step.escalated", true;
    StepWaitingApproval => "step.waiting_approval", false;
    ModelCalled => "model.called", true;
    ModelResponded => "model.responded", true;
    ToolCalled => "tool.called", true;
    ToolResult => "tool.result", true;
    ToolRefused => "tool.refused", true;
    ActionProposed => "action.proposed", false;
    PolicyDecided => "policy.decided", false;
    ApprovalRequested => "approval.requested", false;
    ApprovalDecided => "approval.decided", false;
    ApprovalEscalated => "approval.escalated", false;
    UsageRecorded => "usage.recorded", false;
    BudgetSoftThreshold => "budget.soft_threshold", false;
    BudgetExceeded => "budget.exceeded", false;
    RunParked => "run.parked", false;
    RunResumed => "run.resumed", false;
    RunAbandoned => "run.abandoned", false;
    CompensationScheduled => "compensation.scheduled", false;
    CompensationCompleted => "compensation.completed", false;
    CompensationFailed => "compensation.failed", false;
    RunCompleted => "run.completed", false;
    RunFailed => "run.failed", false;
    Note => "note", true;
}

impl std::fmt::Display for EventKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One record of a run's stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// Always `kernos.events/1`.
    pub schema: String,
    /// The run.
    pub run_id: String,
    /// 1-based, gap-free per run.
    pub seq: u64,
    /// Kernel clock at append, RFC 3339 with milliseconds.
    pub ts: String,
    /// The kind.
    pub kind: EventKind,
    /// Who appended it.
    pub actor: Actor,
    /// The kind-specific payload.
    pub payload: Value,
    /// The previous event's hash, or 64 zeros for `seq` 1.
    pub prev_hash: String,
    /// SHA-256 over the canonical JSON of the record without `schema` and `hash`.
    pub hash: String,
}

impl Event {
    /// Computes the chain hash for a record.
    pub fn compute_hash(
        run_id: &str,
        seq: u64,
        ts: &str,
        kind: EventKind,
        actor: &Actor,
        payload: &Value,
        prev_hash: &str,
    ) -> String {
        let object = json!({
            "run_id": run_id,
            "seq": seq,
            "ts": ts,
            "kind": kind.as_str(),
            "actor": actor,
            "payload": payload,
            "prev_hash": prev_hash,
        });
        sha256_hex(&canonical_bytes(&object))
    }

    /// Builds a record with its hash.
    pub fn build(
        run_id: &str,
        seq: u64,
        ts: &str,
        kind: EventKind,
        actor: Actor,
        payload: Value,
        prev_hash: &str,
    ) -> Event {
        let hash = Event::compute_hash(run_id, seq, ts, kind, &actor, &payload, prev_hash);
        Event {
            schema: SCHEMA.to_string(),
            run_id: run_id.to_string(),
            seq,
            ts: ts.to_string(),
            kind,
            actor,
            payload,
            prev_hash: prev_hash.to_string(),
            hash,
        }
    }

    /// The hash this record should carry given its contents.
    pub fn expected_hash(&self) -> String {
        Event::compute_hash(
            &self.run_id,
            self.seq,
            &self.ts,
            self.kind,
            &self.actor,
            &self.payload,
            &self.prev_hash,
        )
    }

    /// True when the stored hash matches the contents.
    pub fn verify_hash(&self) -> bool {
        self.hash == self.expected_hash()
    }

    /// The timestamp as epoch milliseconds.
    pub fn ts_ms(&self) -> Option<i64> {
        crate::time::parse_rfc3339(&self.ts)
    }

    /// The `step` field of the payload, when present.
    pub fn step(&self) -> Option<&str> {
        self.payload.get("step").and_then(Value::as_str)
    }

    /// Decodes the payload into its typed struct.
    pub fn typed<P: DeserializeOwned>(&self) -> serde_json::Result<P> {
        serde_json::from_value(self.payload.clone())
    }
}

/// A typed payload that knows its kind, so kernel code cannot pair a payload
/// with the wrong kind string.
pub trait EventPayload: Serialize + DeserializeOwned {
    /// The kind this payload belongs to.
    const KIND: EventKind;

    /// The payload as JSON.
    fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

macro_rules! payload {
    ($(#[$meta:meta])* $name:ident = $kind:ident { $( $(#[$fmeta:meta])* pub $field:ident : $ty:ty ),* $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
        pub struct $name {
            $( $(#[$fmeta])* #[allow(missing_docs)] pub $field: $ty, )*
        }
        impl EventPayload for $name { const KIND: EventKind = EventKind::$kind; }
    };
}

/// An error as carried by `step.failed`, `run.failed` and `compensation.failed`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ErrorInfo {
    /// Stable code such as `output_invalid`.
    pub code: String,
    /// Human sentence.
    pub message: String,
}

/// The budget block of `run.created`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct BudgetSpec {
    /// Token ceiling, `None` for unlimited.
    pub tokens: Option<u64>,
    /// Currency ceiling, `None` for unlimited.
    pub usd: Option<f64>,
    /// The soft threshold as a ratio of the ceiling.
    pub soft_ratio: f64,
}

/// Token usage as reported by `model.responded`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct TokenUsage {
    /// Prompt tokens.
    #[serde(default)]
    pub input_tokens: u64,
    /// Completion tokens.
    #[serde(default)]
    pub output_tokens: u64,
    /// Tokens served from the prompt cache.
    #[serde(default)]
    pub cache_read_tokens: u64,
    /// Tokens written to the prompt cache.
    #[serde(default)]
    pub cache_write_tokens: u64,
}

payload!(
    /// `run.created`: always `seq` 1. `department` is carried so the fold and
    /// replay never need the bundle.
    RunCreated = RunCreated {
        pub bundle_id: String,
        pub bundle_name: String,
        pub bundle_version: String,
        pub workflow: String,
        pub input: Value,
        pub remit_id: String,
        pub requested_by: Value,
        pub budget: BudgetSpec,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub department: Option<String>,
    }
);

payload!(
    /// `step.scheduled`: a fresh schedule that resets the attempt counter.
    StepScheduled = StepScheduled {
        pub step: String,
        pub index: u32,
        pub kind: String,
    }
);

payload!(
    /// `step.leased`.
    StepLeased = StepLeased {
        pub step: String,
        pub lease_id: String,
        pub worker_id: String,
        pub attempt: u32,
        pub expires_at: String,
    }
);

payload!(
    /// `step.lease_expired`.
    StepLeaseExpired = StepLeaseExpired {
        pub step: String,
        pub lease_id: String,
        pub worker_id: String,
    }
);

payload!(
    /// `step.completed`.
    StepCompleted = StepCompleted {
        pub step: String,
        pub lease_id: String,
        pub attempt: u32,
        pub output: Value,
    }
);

payload!(
    /// `step.failed`. `lease_id` is null when the kernel fails a step that holds
    /// no lease (a rejected approval).
    StepFailed = StepFailed {
        pub step: String,
        pub lease_id: Option<String>,
        pub attempt: u32,
        pub error: ErrorInfo,
        pub deterministic: bool,
    }
);

payload!(
    /// `step.retry_scheduled`: the step returns to `scheduled` after `delay_ms`.
    StepRetryScheduled = StepRetryScheduled {
        pub step: String,
        pub attempt: u32,
        pub delay_ms: u64,
    }
);

payload!(
    /// `step.quarantined`.
    StepQuarantined = StepQuarantined {
        pub step: String,
        pub reason: String,
        pub attempts: u32,
    }
);

payload!(
    /// `step.escalated` (external).
    StepEscalated = StepEscalated {
        pub step: String,
        pub from_tier: String,
        pub to_tier: String,
        pub reason: String,
    }
);

payload!(
    /// `step.waiting_approval`.
    StepWaitingApproval = StepWaitingApproval {
        pub step: String,
        pub action_id: String,
        pub approval_id: String,
    }
);

payload!(
    /// `model.called` (external).
    ModelCalled = ModelCalled {
        pub step: String,
        #[serde(default)]
        pub model: String,
        #[serde(default)]
        pub tier: String,
        #[serde(default)]
        pub effort: String,
        #[serde(default)]
        pub provider: String,
        #[serde(default)]
        pub prefix_hash: String,
        #[serde(default)]
        pub input_hash: String,
        #[serde(default)]
        pub max_tokens: u64,
    }
);

payload!(
    /// `model.responded` (external). Never feeds budgets; only the `usage`
    /// field of lease completion does.
    ModelResponded = ModelResponded {
        pub step: String,
        #[serde(default)]
        pub output: Value,
        #[serde(default)]
        pub usage: TokenUsage,
        #[serde(default)]
        pub cost_usd: f64,
        #[serde(default)]
        pub stop_reason: String,
        #[serde(default)]
        pub refusal: bool,
        #[serde(default)]
        pub latency_ms: u64,
    }
);

payload!(
    /// `tool.called` (external), appended before the gateway call.
    ToolCalled = ToolCalled {
        pub step: String,
        pub tool: String,
        #[serde(default)]
        pub args: Value,
        #[serde(default)]
        pub scope: Option<String>,
        #[serde(default)]
        pub idempotency_key: Option<String>,
    }
);

payload!(
    /// `tool.result` (external).
    ToolResult = ToolResult {
        pub step: String,
        pub tool: String,
        #[serde(default)]
        pub ok: bool,
        #[serde(default)]
        pub result: Value,
        #[serde(default)]
        pub replayed: bool,
        #[serde(default)]
        pub latency_ms: u64,
    }
);

payload!(
    /// `tool.refused` (external, gateway).
    ToolRefused = ToolRefused {
        pub step: String,
        pub tool: String,
        pub reason: String,
        #[serde(default)]
        pub remit_id: Option<String>,
        #[serde(default)]
        pub detail: Value,
    }
);

payload!(
    /// `action.proposed`.
    ActionProposed = ActionProposed {
        pub action_id: String,
        pub step: String,
        pub action: Value,
    }
);

payload!(
    /// `policy.decided`. `policy_set` lists every policy version evaluated so
    /// replay can rebuild the exact set; `policy` and `policy_version` name the
    /// one whose rule matched.
    PolicyDecided = PolicyDecided {
        pub action_id: String,
        pub decision: String,
        pub rule: String,
        pub policy: Option<String>,
        pub policy_version: Value,
        pub approver: Value,
        pub sla_seconds: Option<u64>,
        pub escalate_to: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub policy_set: Option<Vec<PolicyRef>>,
    }
);

/// A policy name and version pair.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PolicyRef {
    /// Policy name.
    pub name: String,
    /// Policy version.
    pub version: u64,
}

payload!(
    /// `approval.requested`.
    ApprovalRequested = ApprovalRequested {
        pub approval_id: String,
        pub action_id: String,
        pub approver: Value,
        pub sla_seconds: u64,
        pub escalate_to: Value,
        pub due_at: String,
    }
);

/// The `actor` of `approval.decided`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DecisionActor {
    /// User id.
    pub id: String,
    /// The role the actor decided under.
    pub role: String,
}

payload!(
    /// `approval.decided`.
    ApprovalDecided = ApprovalDecided {
        pub approval_id: String,
        pub action_id: String,
        pub decision: String,
        pub actor: DecisionActor,
        pub reason: String,
    }
);

payload!(
    /// `approval.escalated`. `due_at` is the extended deadline.
    ApprovalEscalated = ApprovalEscalated {
        pub approval_id: String,
        pub from: Value,
        pub to: Value,
        pub reason: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub due_at: Option<String>,
    }
);

payload!(
    /// `usage.recorded`.
    UsageRecorded = UsageRecorded {
        pub step: String,
        pub tokens: u64,
        pub usd: f64,
        pub cumulative_tokens: u64,
        pub cumulative_usd: f64,
    }
);

payload!(
    /// `budget.soft_threshold`, once per run.
    BudgetSoftThreshold = BudgetSoftThreshold {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub cumulative_usd: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub ceiling_usd: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub cumulative_tokens: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub ceiling_tokens: Option<u64>,
        pub ratio: f64,
    }
);

payload!(
    /// `budget.exceeded`.
    BudgetExceeded = BudgetExceeded {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub cumulative_usd: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub ceiling_usd: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub cumulative_tokens: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub ceiling_tokens: Option<u64>,
    }
);

payload!(
    /// `run.parked`.
    RunParked = RunParked {
        pub reason: String,
        pub detail: String,
    }
);

payload!(
    /// `run.resumed`.
    RunResumed = RunResumed {
        pub reason: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub actor: Option<Value>,
    }
);

payload!(
    /// `run.abandoned`.
    RunAbandoned = RunAbandoned {
        pub reason: String,
        pub actor: Value,
    }
);

payload!(
    /// `compensation.scheduled`.
    CompensationScheduled = CompensationScheduled {
        pub step: String,
        pub for_step: String,
        pub tool: String,
        pub args: Value,
    }
);

payload!(
    /// `compensation.completed`.
    CompensationCompleted = CompensationCompleted {
        pub step: String,
        pub for_step: String,
        pub result: Value,
    }
);

payload!(
    /// `compensation.failed`.
    CompensationFailed = CompensationFailed {
        pub step: String,
        pub for_step: String,
        pub error: ErrorInfo,
    }
);

payload!(
    /// `run.completed`.
    RunCompleted = RunCompleted {
        pub output: Value,
    }
);

payload!(
    /// `run.failed`.
    RunFailed = RunFailed {
        pub error: ErrorInfo,
        pub needs_human: bool,
    }
);

payload!(
    /// `note` (external): free-form diagnostics that never affect state.
    Note = Note {
        #[serde(default)]
        pub text: String,
        #[serde(default)]
        pub data: Value,
    }
);

/// Truncates an oversized payload: the `result` or `output` field is replaced by
/// a prefix, `truncated: true` and the SHA-256 of the full canonical content are
/// added. Returns `None` when the payload cannot be brought under the limit.
pub fn truncate_payload(payload: Value) -> Option<Value> {
    let size = serde_json::to_vec(&payload).ok()?.len();
    if size <= MAX_PAYLOAD_BYTES {
        return Some(payload);
    }
    let Value::Object(mut map) = payload else {
        return None;
    };
    let field = ["result", "output", "args", "data"]
        .into_iter()
        .find(|f| map.contains_key(*f))?;
    let full = map.remove(field)?;
    let digest = crate::canonical::hash_value(&full);
    let text = full.to_string();
    let keep: String = text.chars().take(MAX_PAYLOAD_BYTES / 2).collect();
    map.insert(field.to_string(), Value::String(keep));
    map.insert("truncated".into(), Value::Bool(true));
    map.insert("sha256".into(), Value::String(digest));
    let out = Value::Object(map);
    if serde_json::to_vec(&out).ok()?.len() <= MAX_PAYLOAD_BYTES {
        Some(out)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_chain_matches_the_spec_recipe() {
        let actor = Actor::worker("wrk-a1");
        let payload = json!({"step": "post", "tool": "ledger.post_entry"});
        let event = Event::build(
            "run_x",
            1,
            "2026-09-04T12:00:00.000Z",
            EventKind::ToolCalled,
            actor.clone(),
            payload.clone(),
            ZERO_HASH,
        );
        let object = json!({"run_id": "run_x", "seq": 1, "ts": "2026-09-04T12:00:00.000Z", "kind": "tool.called",
                            "actor": {"type": "worker", "id": "wrk-a1"}, "payload": payload, "prev_hash": ZERO_HASH});
        assert_eq!(event.hash, sha256_hex(&canonical_bytes(&object)));
        assert!(event.verify_hash());
        let mut tampered = event.clone();
        tampered.payload["tool"] = json!("ledger.void_entry");
        assert!(!tampered.verify_hash());
        let json = serde_json::to_value(&event).expect("json");
        assert_eq!(json["schema"], SCHEMA);
        assert_eq!(json["kind"], "tool.called");
        assert_eq!(json["actor"]["type"], "worker");
    }

    #[test]
    fn kinds_round_trip_and_flag_external() {
        for kind in EventKind::ALL {
            assert_eq!(EventKind::parse(kind.as_str()), Some(*kind));
            let json = serde_json::to_value(kind).expect("json");
            assert_eq!(json, json!(kind.as_str()));
        }
        assert!(EventKind::ToolCalled.is_external());
        assert!(EventKind::Note.is_external());
        assert!(EventKind::StepEscalated.is_external());
        assert!(!EventKind::UsageRecorded.is_external());
        assert!(!EventKind::PolicyDecided.is_external());
        assert_eq!(EventKind::parse("run.deleted"), None);
        assert_eq!(EventKind::ALL.len(), 32);
    }

    #[test]
    fn typed_payloads_serialise_with_spec_field_names() {
        let p = StepFailed {
            step: "post".into(),
            lease_id: Some("lse_1".into()),
            attempt: 2,
            error: ErrorInfo {
                code: "boom".into(),
                message: "it broke".into(),
            },
            deterministic: true,
        };
        assert_eq!(
            p.to_value(),
            json!({"step": "post", "lease_id": "lse_1", "attempt": 2, "error": {"code": "boom", "message": "it broke"}, "deterministic": true})
        );
        assert_eq!(StepFailed::KIND, EventKind::StepFailed);
        let back: StepFailed = serde_json::from_value(p.to_value()).expect("parse");
        assert_eq!(back, p);
    }

    #[test]
    fn oversized_payloads_are_truncated_with_a_digest() {
        let big = "x".repeat(MAX_PAYLOAD_BYTES + 10);
        let payload = json!({"step": "s", "tool": "t", "ok": true, "result": big});
        let out = truncate_payload(payload).expect("truncated");
        assert_eq!(out["truncated"], json!(true));
        assert_eq!(out["sha256"].as_str().map(str::len), Some(64));
        assert!(serde_json::to_vec(&out).expect("json").len() <= MAX_PAYLOAD_BYTES);
        assert!(truncate_payload(json!({"only": "x".repeat(MAX_PAYLOAD_BYTES + 10)})).is_none());
    }
}
