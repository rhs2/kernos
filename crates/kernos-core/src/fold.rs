//! `fold(events) -> RunState`: the pure derivation of run and step state.
//!
//! The materialised `runs` and `steps` tables are written from the same
//! function in the same transaction as each event, and a test asserts that they
//! equal `fold` at every `seq`.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::events::{
    ActionProposed, ApprovalDecided, ApprovalEscalated, ApprovalRequested, BudgetSpec,
    CompensationCompleted, CompensationFailed, CompensationScheduled, Event, EventKind,
    PolicyDecided, RunAbandoned, RunCompleted, RunCreated, RunFailed, RunParked, RunResumed,
    StepCompleted, StepFailed, StepLeaseExpired, StepLeased, StepQuarantined, StepRetryScheduled,
    StepScheduled, StepWaitingApproval, UsageRecorded,
};
use crate::time::format_ms;

/// Why a stream could not be folded. A well-formed kernel never produces these;
/// they guard against corrupted storage.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum FoldError {
    /// No events at all.
    #[error("the event stream is empty")]
    Empty,
    /// The first event is not `run.created`.
    #[error("event {seq} is {kind} but the first event must be run.created")]
    FirstNotRunCreated {
        /// Its sequence number.
        seq: u64,
        /// Its kind.
        kind: EventKind,
    },
    /// A payload could not be decoded into its typed form.
    #[error("event {seq} ({kind}) has an unreadable payload: {message}")]
    BadPayload {
        /// Its sequence number.
        seq: u64,
        /// Its kind.
        kind: EventKind,
        /// The decoding error.
        message: String,
    },
}

/// Run states of the 01-EVENTS state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// `run.created` seen, no step scheduled yet.
    Created,
    /// Work is available or in progress.
    Running,
    /// Waiting on an approval, a budget, an operator or a repair.
    Parked,
    /// The last step completed.
    Completed,
    /// The run failed.
    Failed,
    /// The run was abandoned; compensations may still be running.
    Abandoned,
}

impl RunStatus {
    /// The wire spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            RunStatus::Created => "created",
            RunStatus::Running => "running",
            RunStatus::Parked => "parked",
            RunStatus::Completed => "completed",
            RunStatus::Failed => "failed",
            RunStatus::Abandoned => "abandoned",
        }
    }

    /// Parses the wire spelling.
    pub fn parse(text: &str) -> Option<RunStatus> {
        match text {
            "created" => Some(RunStatus::Created),
            "running" => Some(RunStatus::Running),
            "parked" => Some(RunStatus::Parked),
            "completed" => Some(RunStatus::Completed),
            "failed" => Some(RunStatus::Failed),
            "abandoned" => Some(RunStatus::Abandoned),
            _ => None,
        }
    }
}

/// Step states of the 01-EVENTS state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    /// Runnable once every earlier step is completed and `not_before` has passed.
    Scheduled,
    /// Held by a worker.
    Leased,
    /// Done.
    Completed,
    /// The last attempt failed; a retry may follow.
    Failed,
    /// Retries exhausted.
    Quarantined,
    /// Parked on an approval.
    WaitingApproval,
}

impl StepStatus {
    /// The wire spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            StepStatus::Scheduled => "scheduled",
            StepStatus::Leased => "leased",
            StepStatus::Completed => "completed",
            StepStatus::Failed => "failed",
            StepStatus::Quarantined => "quarantined",
            StepStatus::WaitingApproval => "waiting_approval",
        }
    }
}

/// The bundle a run was started from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BundleRef {
    /// Bundle id.
    pub id: String,
    /// Bundle name.
    pub name: String,
    /// Bundle version.
    pub version: String,
}

/// The live lease on a step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseInfo {
    /// Lease id.
    pub lease_id: String,
    /// Holder.
    pub worker_id: String,
    /// Attempt number.
    pub attempt: u32,
    /// Expiry as recorded at lease time (heartbeats extend the store, not the log).
    pub expires_at: String,
}

/// One step of the run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StepState {
    /// Step id.
    pub id: String,
    /// Position in bundle order; compensation steps follow the workflow's steps.
    pub index: u32,
    /// `model`, `tool`, `action` or `compensation`.
    pub kind: String,
    /// Current state.
    pub state: StepStatus,
    /// Leases taken since the last fresh schedule.
    pub attempts: u32,
    /// The live lease, if any.
    pub lease: Option<LeaseInfo>,
    /// Output of the last completion.
    pub output: Option<Value>,
    /// Error of the last failure.
    pub error: Option<Value>,
    /// The most recent action proposed by this step.
    pub action_id: Option<String>,
    /// The approval this step waits or waited on.
    pub approval_id: Option<String>,
    /// Earliest time the step may be leased again after a retry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_before: Option<String>,
}

/// Budget accounting for the run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct BudgetState {
    /// Token ceiling, `None` for unlimited.
    pub ceiling_tokens: Option<u64>,
    /// Currency ceiling, `None` for unlimited.
    pub ceiling_usd: Option<f64>,
    /// The soft threshold ratio.
    pub soft_ratio: f64,
    /// Tokens used.
    pub used_tokens: u64,
    /// Currency used.
    pub used_usd: f64,
    /// The soft threshold has been crossed; leases carry `pacing: true`.
    pub soft_hit: bool,
    /// The hard ceiling has been crossed.
    pub exceeded: bool,
}

/// The approval the run is parked on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingApproval {
    /// Approval id.
    pub approval_id: String,
    /// The action awaiting approval.
    pub action_id: String,
    /// The current approver.
    pub approver: Value,
    /// The current deadline.
    pub due_at: String,
}

/// One `policy.decided` record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionRecord {
    /// The action.
    pub action_id: String,
    /// `allow`, `approval_required` or `deny`.
    pub decision: String,
    /// The rule id.
    pub rule: String,
    /// The policy whose rule matched.
    pub policy: Option<String>,
    /// Its version.
    pub policy_version: Value,
}

/// The state of one compensation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompensationState {
    /// The completed step being unwound.
    pub for_step: String,
    /// The compensation step id.
    pub step: String,
    /// `scheduled`, `completed` or `failed`.
    pub state: String,
}

/// The derived state of a run, the shape `GET /v1/runs/{id}` returns.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunState {
    /// Run id.
    pub run_id: String,
    /// Current state.
    pub state: RunStatus,
    /// The bundle.
    pub bundle: BundleRef,
    /// The bundle's department.
    pub department: Option<String>,
    /// Workflow name.
    pub workflow: String,
    /// The validated input.
    pub input: Value,
    /// The remit the run was started with.
    pub remit_id: String,
    /// Who asked for the run.
    pub requested_by: Value,
    /// Steps in order.
    pub steps: Vec<StepState>,
    /// Budget accounting.
    pub budget: BudgetState,
    /// The approval the run is parked on, if any.
    pub pending_approval: Option<PendingApproval>,
    /// Every policy decision so far.
    pub decisions: Vec<DecisionRecord>,
    /// Compensations scheduled after abandonment.
    pub compensations: Vec<CompensationState>,
    /// Output of the last step once completed.
    pub output: Option<Value>,
    /// The terminal error, if failed.
    pub error: Option<Value>,
    /// A human must act before anything else happens.
    pub needs_human: bool,
    /// An unwind is under way: `run.abandoned` was recorded and at least one
    /// compensation is still pending. The run stays `running` while that is
    /// true, so workers keep leasing compensation steps; it becomes `abandoned`
    /// only when the last one completes. The flag stays set afterwards to say
    /// how the run ended up in its terminal state.
    pub abandoning: bool,
    /// Why the run is parked, when it is.
    pub park_reason: Option<String>,
    /// The last folded sequence number.
    pub last_seq: u64,
}

/// Folds a whole stream. The first event must be `run.created`.
pub fn fold(events: &[Event]) -> Result<RunState, FoldError> {
    let first = events.first().ok_or(FoldError::Empty)?;
    let mut state = RunState::from_created(first)?;
    for event in &events[1..] {
        state.apply(event)?;
    }
    Ok(state)
}

fn bad<E: std::fmt::Display>(event: &Event) -> impl FnOnce(E) -> FoldError + '_ {
    move |e| FoldError::BadPayload {
        seq: event.seq,
        kind: event.kind,
        message: e.to_string(),
    }
}

impl RunState {
    /// The initial state from a `run.created` event.
    pub fn from_created(event: &Event) -> Result<RunState, FoldError> {
        if event.kind != EventKind::RunCreated {
            return Err(FoldError::FirstNotRunCreated {
                seq: event.seq,
                kind: event.kind,
            });
        }
        let created: RunCreated = event.typed().map_err(bad(event))?;
        let BudgetSpec {
            tokens,
            usd,
            soft_ratio,
        } = created.budget;
        Ok(RunState {
            run_id: event.run_id.clone(),
            state: RunStatus::Created,
            bundle: BundleRef {
                id: created.bundle_id,
                name: created.bundle_name,
                version: created.bundle_version,
            },
            department: created.department,
            workflow: created.workflow,
            input: created.input,
            remit_id: created.remit_id,
            requested_by: created.requested_by,
            steps: Vec::new(),
            budget: BudgetState {
                ceiling_tokens: tokens,
                ceiling_usd: usd,
                soft_ratio,
                ..Default::default()
            },
            pending_approval: None,
            decisions: Vec::new(),
            compensations: Vec::new(),
            output: None,
            error: None,
            needs_human: false,
            abandoning: false,
            park_reason: None,
            last_seq: event.seq,
        })
    }

    /// Applies one later event.
    pub fn apply(&mut self, event: &Event) -> Result<(), FoldError> {
        self.last_seq = event.seq;
        match event.kind {
            EventKind::RunCreated => {
                return Err(FoldError::BadPayload {
                    seq: event.seq,
                    kind: event.kind,
                    message: "run.created may only be the first event".into(),
                })
            }
            EventKind::StepScheduled => {
                let p: StepScheduled = event.typed().map_err(bad(event))?;
                match self.step_mut(&p.step) {
                    Some(step) => {
                        step.state = StepStatus::Scheduled;
                        step.attempts = 0;
                        step.lease = None;
                        step.not_before = None;
                        step.error = None;
                    }
                    None => self.steps.push(StepState {
                        id: p.step,
                        index: p.index,
                        kind: p.kind,
                        state: StepStatus::Scheduled,
                        attempts: 0,
                        lease: None,
                        output: None,
                        error: None,
                        action_id: None,
                        approval_id: None,
                        not_before: None,
                    }),
                }
                if self.state == RunStatus::Created {
                    self.state = RunStatus::Running;
                }
            }
            EventKind::StepLeased => {
                let p: StepLeased = event.typed().map_err(bad(event))?;
                if let Some(step) = self.step_mut(&p.step) {
                    step.state = StepStatus::Leased;
                    step.attempts = p.attempt;
                    step.not_before = None;
                    step.lease = Some(LeaseInfo {
                        lease_id: p.lease_id,
                        worker_id: p.worker_id,
                        attempt: p.attempt,
                        expires_at: p.expires_at,
                    });
                }
            }
            EventKind::StepLeaseExpired => {
                let p: StepLeaseExpired = event.typed().map_err(bad(event))?;
                if let Some(step) = self.step_mut(&p.step) {
                    step.state = StepStatus::Scheduled;
                    step.lease = None;
                }
            }
            EventKind::StepCompleted => {
                let p: StepCompleted = event.typed().map_err(bad(event))?;
                if let Some(step) = self.step_mut(&p.step) {
                    step.state = StepStatus::Completed;
                    step.output = Some(p.output);
                    step.error = None;
                    step.lease = None;
                    step.not_before = None;
                }
            }
            EventKind::StepFailed => {
                let p: StepFailed = event.typed().map_err(bad(event))?;
                if let Some(step) = self.step_mut(&p.step) {
                    step.state = StepStatus::Failed;
                    step.error = Some(serde_json::to_value(&p.error).unwrap_or(Value::Null));
                    step.lease = None;
                }
            }
            EventKind::StepRetryScheduled => {
                let p: StepRetryScheduled = event.typed().map_err(bad(event))?;
                let not_before = event.ts_ms().map(|ts| format_ms(ts + p.delay_ms as i64));
                if let Some(step) = self.step_mut(&p.step) {
                    step.state = StepStatus::Scheduled;
                    step.lease = None;
                    step.not_before = not_before;
                }
            }
            EventKind::StepQuarantined => {
                let p: StepQuarantined = event.typed().map_err(bad(event))?;
                if let Some(step) = self.step_mut(&p.step) {
                    step.state = StepStatus::Quarantined;
                    step.lease = None;
                    step.not_before = None;
                }
            }
            EventKind::StepWaitingApproval => {
                let p: StepWaitingApproval = event.typed().map_err(bad(event))?;
                if let Some(step) = self.step_mut(&p.step) {
                    step.state = StepStatus::WaitingApproval;
                    step.lease = None;
                    step.action_id = Some(p.action_id);
                    step.approval_id = Some(p.approval_id);
                }
            }
            EventKind::ActionProposed => {
                let p: ActionProposed = event.typed().map_err(bad(event))?;
                if let Some(step) = self.step_mut(&p.step) {
                    step.action_id = Some(p.action_id);
                }
            }
            EventKind::PolicyDecided => {
                let p: PolicyDecided = event.typed().map_err(bad(event))?;
                self.decisions.push(DecisionRecord {
                    action_id: p.action_id,
                    decision: p.decision,
                    rule: p.rule,
                    policy: p.policy,
                    policy_version: p.policy_version,
                });
            }
            EventKind::ApprovalRequested => {
                let p: ApprovalRequested = event.typed().map_err(bad(event))?;
                self.pending_approval = Some(PendingApproval {
                    approval_id: p.approval_id,
                    action_id: p.action_id,
                    approver: p.approver,
                    due_at: p.due_at,
                });
            }
            EventKind::ApprovalEscalated => {
                let p: ApprovalEscalated = event.typed().map_err(bad(event))?;
                if let Some(pending) = self.pending_approval.as_mut() {
                    if pending.approval_id == p.approval_id {
                        pending.approver = p.to;
                        if let Some(due) = p.due_at {
                            pending.due_at = due;
                        }
                    }
                }
            }
            EventKind::ApprovalDecided => {
                let p: ApprovalDecided = event.typed().map_err(bad(event))?;
                if self
                    .pending_approval
                    .as_ref()
                    .is_some_and(|a| a.approval_id == p.approval_id)
                {
                    self.pending_approval = None;
                }
            }
            EventKind::UsageRecorded => {
                let p: UsageRecorded = event.typed().map_err(bad(event))?;
                self.budget.used_tokens = p.cumulative_tokens;
                self.budget.used_usd = p.cumulative_usd;
            }
            EventKind::BudgetSoftThreshold => self.budget.soft_hit = true,
            EventKind::BudgetExceeded => self.budget.exceeded = true,
            EventKind::RunParked => {
                let p: RunParked = event.typed().map_err(bad(event))?;
                self.state = RunStatus::Parked;
                if p.reason == "human" {
                    self.needs_human = true;
                }
                self.park_reason = Some(p.reason);
            }
            EventKind::RunResumed => {
                let _p: RunResumed = event.typed().map_err(bad(event))?;
                self.state = RunStatus::Running;
                self.park_reason = None;
                self.needs_human = false;
            }
            EventKind::RunAbandoned => {
                // Abandonment is an intent, not an outcome: the run keeps
                // running until its compensations have unwound the writes. With
                // nothing to unwind it is abandoned at once, and each
                // compensation scheduled after this event puts it back to work.
                let _p: RunAbandoned = event.typed().map_err(bad(event))?;
                self.abandoning = true;
                self.park_reason = None;
                self.pending_approval = None;
                self.state = if self.unwind_pending() {
                    RunStatus::Running
                } else {
                    RunStatus::Abandoned
                };
            }
            EventKind::CompensationScheduled => {
                let p: CompensationScheduled = event.typed().map_err(bad(event))?;
                self.compensations.push(CompensationState {
                    for_step: p.for_step,
                    step: p.step,
                    state: "scheduled".into(),
                });
                if self.abandoning && self.state == RunStatus::Abandoned {
                    self.state = RunStatus::Running;
                }
            }
            EventKind::CompensationCompleted => {
                let p: CompensationCompleted = event.typed().map_err(bad(event))?;
                if let Some(c) = self
                    .compensations
                    .iter_mut()
                    .find(|c| c.for_step == p.for_step)
                {
                    c.state = "completed".into();
                }
                // The last compensation is what makes a run abandoned. A run
                // already failed by a compensation that could not be unwound
                // stays failed and needing a human.
                if self.abandoning && self.state == RunStatus::Running && !self.unwind_pending() {
                    self.state = RunStatus::Abandoned;
                }
            }
            EventKind::CompensationFailed => {
                let p: CompensationFailed = event.typed().map_err(bad(event))?;
                if let Some(c) = self
                    .compensations
                    .iter_mut()
                    .find(|c| c.for_step == p.for_step)
                {
                    c.state = "failed".into();
                }
            }
            EventKind::RunCompleted => {
                let p: RunCompleted = event.typed().map_err(bad(event))?;
                self.state = RunStatus::Completed;
                self.output = Some(p.output);
                self.park_reason = None;
            }
            EventKind::RunFailed => {
                let p: RunFailed = event.typed().map_err(bad(event))?;
                self.state = RunStatus::Failed;
                self.error = Some(serde_json::to_value(&p.error).unwrap_or(Value::Null));
                self.needs_human = p.needs_human;
                self.park_reason = None;
            }
            EventKind::StepEscalated
            | EventKind::ModelCalled
            | EventKind::ModelResponded
            | EventKind::ToolCalled
            | EventKind::ToolResult
            | EventKind::ToolRefused
            | EventKind::Note => {}
        }
        Ok(())
    }

    /// A step by id.
    pub fn step(&self, id: &str) -> Option<&StepState> {
        self.steps.iter().find(|s| s.id == id)
    }

    /// A step by id, mutably.
    pub fn step_mut(&mut self, id: &str) -> Option<&mut StepState> {
        self.steps.iter_mut().find(|s| s.id == id)
    }

    /// The workflow steps (everything that is not a compensation), in order.
    pub fn workflow_steps(&self) -> impl Iterator<Item = &StepState> {
        self.steps.iter().filter(|s| s.kind != "compensation")
    }

    /// The compensation steps, in order.
    pub fn compensation_steps(&self) -> impl Iterator<Item = &StepState> {
        self.steps.iter().filter(|s| s.kind == "compensation")
    }

    /// True while a scheduled compensation has still to run.
    pub fn unwind_pending(&self) -> bool {
        self.compensations.iter().any(|c| c.state == "scheduled")
    }

    /// The next step that would run: the first pending compensation once an
    /// unwind is under way, otherwise the first workflow step that is not
    /// completed. `None` when nothing is left.
    pub fn next_step(&self) -> Option<&StepState> {
        if self.is_terminal() {
            return None;
        }
        if self.abandoning {
            self.compensation_steps()
                .find(|s| s.state != StepStatus::Completed)
        } else {
            self.workflow_steps()
                .find(|s| s.state != StepStatus::Completed)
        }
    }

    /// True once the run can never change again on its own. `abandoned` is
    /// terminal because it now means the unwind finished.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.state,
            RunStatus::Completed | RunStatus::Failed | RunStatus::Abandoned
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{Actor, ErrorInfo, EventPayload, ZERO_HASH};
    use serde_json::json;

    fn stream(items: Vec<(EventKind, Value)>) -> Vec<Event> {
        let mut out = Vec::new();
        let mut prev = ZERO_HASH.to_string();
        for (i, (kind, payload)) in items.into_iter().enumerate() {
            let e = Event::build(
                "run_1",
                i as u64 + 1,
                "2026-09-04T12:00:00.000Z",
                kind,
                Actor::kernel(),
                payload,
                &prev,
            );
            prev = e.hash.clone();
            out.push(e);
        }
        out
    }

    fn created() -> (EventKind, Value) {
        (
            EventKind::RunCreated,
            RunCreated {
                bundle_id: "bnd_1".into(),
                bundle_name: "halcyon.finance.invoice_intake".into(),
                bundle_version: "1.0.0".into(),
                workflow: "intake".into(),
                input: json!({"invoice_id": "inv-1"}),
                remit_id: "rem_1".into(),
                requested_by: json!({"id": "u-ana", "role": "ap_clerk", "manager": "u-tom"}),
                budget: BudgetSpec {
                    tokens: None,
                    usd: Some(2.0),
                    soft_ratio: 0.8,
                },
                department: Some("finance".into()),
            }
            .to_value(),
        )
    }

    #[test]
    fn folds_a_happy_path() {
        let events = stream(vec![
            created(),
            (
                EventKind::StepScheduled,
                json!({"step": "extract", "index": 0, "kind": "model"}),
            ),
            (
                EventKind::StepScheduled,
                json!({"step": "post", "index": 1, "kind": "tool"}),
            ),
            (
                EventKind::StepLeased,
                json!({"step": "extract", "lease_id": "lse_1", "worker_id": "wrk-a1", "attempt": 1, "expires_at": "2026-09-04T12:00:30.000Z"}),
            ),
            (
                EventKind::StepCompleted,
                json!({"step": "extract", "lease_id": "lse_1", "attempt": 1, "output": {"vendor": "Northwind Dairy"}}),
            ),
            (
                EventKind::UsageRecorded,
                json!({"step": "extract", "tokens": 100, "usd": 0.5, "cumulative_tokens": 100, "cumulative_usd": 0.5}),
            ),
            (
                EventKind::StepLeased,
                json!({"step": "post", "lease_id": "lse_2", "worker_id": "wrk-a1", "attempt": 1, "expires_at": "2026-09-04T12:01:00.000Z"}),
            ),
            (
                EventKind::StepCompleted,
                json!({"step": "post", "lease_id": "lse_2", "attempt": 1, "output": {"entry_id": 7}}),
            ),
            (EventKind::RunCompleted, json!({"output": {"entry_id": 7}})),
        ]);
        let state = fold(&events).expect("fold");
        assert_eq!(state.state, RunStatus::Completed);
        assert_eq!(state.department.as_deref(), Some("finance"));
        assert_eq!(state.steps.len(), 2);
        assert_eq!(state.steps[0].state, StepStatus::Completed);
        assert_eq!(state.steps[0].attempts, 1);
        assert!(state.steps[0].lease.is_none());
        assert_eq!(state.budget.used_usd, 0.5);
        assert_eq!(state.output, Some(json!({"entry_id": 7})));
        assert_eq!(state.last_seq, 9);
        assert!(state.is_terminal());
        assert!(state.next_step().is_none());
        let json = serde_json::to_value(&state).expect("json");
        assert_eq!(json["state"], "completed");
        assert_eq!(json["steps"][0]["state"], "completed");
        assert_eq!(json["bundle"]["name"], "halcyon.finance.invoice_intake");
        assert!(json["steps"][0].get("not_before").is_none());
    }

    #[test]
    fn retries_approvals_and_parking() {
        let events = stream(vec![
            created(),
            (
                EventKind::StepScheduled,
                json!({"step": "pay", "index": 0, "kind": "action"}),
            ),
            (
                EventKind::StepLeased,
                json!({"step": "pay", "lease_id": "lse_1", "worker_id": "w", "attempt": 1, "expires_at": "x"}),
            ),
            (
                EventKind::StepFailed,
                StepFailed {
                    step: "pay".into(),
                    lease_id: Some("lse_1".into()),
                    attempt: 1,
                    error: ErrorInfo {
                        code: "timeout".into(),
                        message: "m".into(),
                    },
                    deterministic: false,
                }
                .to_value(),
            ),
            (
                EventKind::StepRetryScheduled,
                json!({"step": "pay", "attempt": 2, "delay_ms": 500}),
            ),
            (
                EventKind::StepLeased,
                json!({"step": "pay", "lease_id": "lse_2", "worker_id": "w", "attempt": 2, "expires_at": "x"}),
            ),
            (
                EventKind::ActionProposed,
                json!({"action_id": "act_1", "step": "pay", "action": {"kind": "payment.issue"}}),
            ),
            (
                EventKind::PolicyDecided,
                json!({"action_id": "act_1", "decision": "approval_required", "rule": "finance-default@1#0", "policy": "finance-default", "policy_version": 1, "approver": {"type": "role", "value": "finance_admin"}, "sla_seconds": 14400, "escalate_to": "reporting_line"}),
            ),
            (
                EventKind::ApprovalRequested,
                json!({"approval_id": "apr_1", "action_id": "act_1", "approver": {"type": "role", "value": "finance_admin"}, "sla_seconds": 14400, "escalate_to": "reporting_line", "due_at": "2026-09-04T16:00:00.000Z"}),
            ),
            (
                EventKind::StepWaitingApproval,
                json!({"step": "pay", "action_id": "act_1", "approval_id": "apr_1"}),
            ),
            (
                EventKind::RunParked,
                json!({"reason": "approval", "detail": "apr_1"}),
            ),
        ]);
        let mut state = fold(&events).expect("fold");
        assert_eq!(state.state, RunStatus::Parked);
        assert_eq!(state.park_reason.as_deref(), Some("approval"));
        assert_eq!(state.steps[0].state, StepStatus::WaitingApproval);
        assert_eq!(state.steps[0].attempts, 2);
        assert_eq!(state.steps[0].approval_id.as_deref(), Some("apr_1"));
        assert_eq!(state.decisions.len(), 1);
        assert_eq!(
            state
                .pending_approval
                .as_ref()
                .map(|a| a.approval_id.as_str()),
            Some("apr_1")
        );
        assert!(!state.needs_human);

        let after_retry = fold(&events[..5]).expect("fold");
        assert_eq!(after_retry.steps[0].state, StepStatus::Scheduled);
        assert_eq!(
            after_retry.steps[0].not_before.as_deref(),
            Some("2026-09-04T12:00:00.500Z")
        );

        let more = stream(vec![created()]);
        let mut next = more[0].clone();
        next.seq = 12;
        next.kind = EventKind::ApprovalEscalated;
        next.payload = json!({"approval_id": "apr_1", "from": {"type": "role", "value": "finance_admin"}, "to": {"type": "user", "value": "u-cfo"}, "reason": "sla_expired", "due_at": "2026-09-04T20:00:00.000Z"});
        state.apply(&next).expect("apply");
        assert_eq!(
            state.pending_approval.as_ref().map(|a| a.approver.clone()),
            Some(json!({"type": "user", "value": "u-cfo"}))
        );
        next.seq = 13;
        next.kind = EventKind::RunParked;
        next.payload = json!({"reason": "human", "detail": "sla"});
        state.apply(&next).expect("apply");
        assert!(state.needs_human);
        next.seq = 14;
        next.kind = EventKind::ApprovalDecided;
        next.payload = json!({"approval_id": "apr_1", "action_id": "act_1", "decision": "approved", "actor": {"id": "u-tom", "role": "finance_admin"}, "reason": "looks right"});
        state.apply(&next).expect("apply");
        assert!(state.pending_approval.is_none());
        next.seq = 15;
        next.kind = EventKind::RunResumed;
        next.payload = json!({"reason": "approval"});
        state.apply(&next).expect("apply");
        assert_eq!(state.state, RunStatus::Running);
        assert!(!state.needs_human);
        next.seq = 16;
        next.kind = EventKind::StepScheduled;
        next.payload = json!({"step": "pay", "index": 0, "kind": "action"});
        state.apply(&next).expect("apply");
        assert_eq!(state.steps[0].attempts, 0);
        assert_eq!(state.steps[0].state, StepStatus::Scheduled);
        assert_eq!(state.steps.len(), 1);
    }

    #[test]
    fn abandonment_and_compensation() {
        let events = stream(vec![
            created(),
            (
                EventKind::StepScheduled,
                json!({"step": "post", "index": 0, "kind": "tool"}),
            ),
            (
                EventKind::StepLeased,
                json!({"step": "post", "lease_id": "l", "worker_id": "w", "attempt": 1, "expires_at": "x"}),
            ),
            (
                EventKind::StepCompleted,
                json!({"step": "post", "lease_id": "l", "attempt": 1, "output": {"entry_id": 1}}),
            ),
            (
                EventKind::RunAbandoned,
                json!({"reason": "operator", "actor": {"id": "u-tom"}}),
            ),
            (
                EventKind::CompensationScheduled,
                json!({"step": "comp_post", "for_step": "post", "tool": "ledger.void_entry", "args": {"entry_id": 1}}),
            ),
            (
                EventKind::StepScheduled,
                json!({"step": "comp_post", "index": 1, "kind": "compensation"}),
            ),
        ]);
        let state = fold(&events).expect("fold");
        assert_eq!(
            state.state,
            RunStatus::Running,
            "the unwind has to finish before the run is abandoned"
        );
        assert!(state.abandoning);
        assert!(state.unwind_pending());
        assert!(!state.is_terminal());
        assert_eq!(state.next_step().map(|s| s.id.as_str()), Some("comp_post"));
        assert_eq!(state.compensations[0].state, "scheduled");

        // Completing the last compensation is what abandons the run.
        let mut done = events[events.len() - 1].clone();
        done.seq += 1;
        done.kind = EventKind::StepCompleted;
        done.payload =
            json!({"step": "comp_post", "lease_id": "l", "attempt": 1, "output": {"voided": true}});
        let mut state = state;
        state.apply(&done).expect("apply");
        assert_eq!(state.state, RunStatus::Running);
        done.seq += 1;
        done.kind = EventKind::CompensationCompleted;
        done.payload = json!({"step": "comp_post", "for_step": "post", "result": {"voided": true}});
        state.apply(&done).expect("apply");
        assert_eq!(state.state, RunStatus::Abandoned);
        assert!(state.is_terminal());
        assert!(!state.unwind_pending());
        assert!(state.next_step().is_none());

        // A run abandoned with nothing to unwind is abandoned at once.
        let bare = stream(vec![
            created(),
            (
                EventKind::StepScheduled,
                json!({"step": "post", "index": 0, "kind": "tool"}),
            ),
            (
                EventKind::RunAbandoned,
                json!({"reason": "operator", "actor": {"id": "u-tom"}}),
            ),
        ]);
        let state = fold(&bare).expect("fold");
        assert_eq!(state.state, RunStatus::Abandoned);
        assert!(state.abandoning);
        assert!(state.is_terminal());
        assert!(state.compensations.is_empty());
    }

    #[test]
    fn rejects_bad_streams() {
        assert_eq!(fold(&[]), Err(FoldError::Empty));
        let events = stream(vec![(EventKind::Note, json!({"text": "hi"}))]);
        assert!(matches!(
            fold(&events),
            Err(FoldError::FirstNotRunCreated { seq: 1, .. })
        ));
        let events = stream(vec![
            created(),
            (EventKind::StepLeased, json!({"nonsense": true})),
        ]);
        assert!(matches!(
            fold(&events),
            Err(FoldError::BadPayload { seq: 2, .. })
        ));
    }
}
