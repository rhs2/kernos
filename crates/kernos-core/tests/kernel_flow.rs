//! End-to-end flows through the kernel library on a manual clock, and the
//! randomised property test that the materialised tables equal `fold` at every
//! sequence number.

use std::sync::Arc;

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use serde_json::{json, Value};

use kernos_core::bundle::sign_bundle;
use kernos_core::clock::ManualClock;
use kernos_core::events::{DecisionActor, ErrorInfo, EventKind};
use kernos_core::fold::{fold, RunStatus, StepStatus};
use kernos_core::kernel::{ExternalAuth, LeaseGrant, LeaseRequest, StartRunRequest, Usage};
use kernos_core::keys::KeyPair;
use kernos_core::remit::{Autonomy, DeriveRequest, IssueRequest, Spend};
use kernos_core::store;
use kernos_core::{Kernel, KernelConfig};
use kernos_policy::Directory;

const START_MS: i64 = 1_788_523_200_000;

const FINANCE_DEFAULT: &str = r#"
policy "finance-default"

require approval when
  action.kind == "payment.issue" and action.amount >= 5000
  -> approver: role("finance_admin"), sla: 4h, escalate_to: reporting_line

require approval when
  action.writes_to_system_of_record and run.remit.autonomy == "supervised"
  -> approver: run.requested_by.manager, sla: 2s

deny when
  action.touches_data_class("personal") and not run.remit.grants("pii")

allow when
  action.kind == "invoice.read"
"#;

fn bundle() -> Value {
    json!({
        "format": "kernos.bundle/1",
        "name": "halcyon.finance.invoice_intake",
        "version": "1.0.0",
        "department": "finance",
        "description": "Invoice intake, coding and posting for Halcyon Provisions.",
        "policies": ["finance-default"],
        "tools": [
            {"id": "ledger.post_entry", "description": "Post a journal entry", "writes": true},
            {"id": "ledger.void_entry", "description": "Void a posted entry", "writes": true},
            {"id": "ledger.archive", "description": "Archive an invoice", "writes": true},
            {"id": "ledger.unarchive", "description": "Restore an archived invoice", "writes": true},
            {"id": "ledger.lookup_vendor", "description": "Find a vendor by name", "writes": false}
        ],
        "prompts": {
            "extract": {"system": "You extract fields from supplier invoices for Halcyon Provisions.", "user": "Invoice text:\n{{input.text}}"},
            "code": {"system": "You assign a general-ledger account to an invoice line.", "user": "Vendor: {{steps.extract.output.vendor}}"}
        },
        "mock": {
            "extract": {"vendor": "Northwind Dairy", "invoice_id": "{{input.invoice_id}}", "total": "{{input.total}}", "currency": "USD"},
            "code": {"account": "5100", "confidence": 0.93}
        },
        "workflows": {
            "intake": {
                "input_schema": {"type": "object", "required": ["invoice_id", "text", "total"],
                                 "properties": {"invoice_id": {"type": "string"}, "text": {"type": "string"}, "total": {"type": "number"}}},
                "steps": [
                    {"id": "extract", "kind": "model", "tier": "standard", "effort": "low", "prompt": "extract",
                     "output_schema": {"type": "object", "required": ["vendor", "total"], "properties": {"vendor": {"type": "string"}, "total": {"type": "number"}}}},
                    {"id": "code", "kind": "model", "tier": "cheap", "prompt": "code",
                     "output_schema": {"type": "object", "properties": {"account": {"type": "string"}, "confidence": {"type": "number"}}}},
                    {"id": "propose_payment", "kind": "action",
                     "action": {"kind": "payment.issue", "amount": {"$ref": "steps.extract.output.total"}, "currency": "USD",
                                "writes_to_system_of_record": true, "target": "ledger", "data_classes": [], "paths": [],
                                "idempotency_key": {"$ref": "input.invoice_id"}, "summary": "Pay invoice {{input.invoice_id}}"}},
                    {"id": "post", "kind": "tool", "tool": "ledger.post_entry",
                     "args": {"invoice_id": {"$ref": "input.invoice_id"}, "amount": {"$ref": "steps.extract.output.total"}},
                     "idempotency_key": {"$ref": "input.invoice_id"},
                     "compensation": {"tool": "ledger.void_entry", "args": {"entry_id": {"$ref": "steps.post.output.entry_id"}, "reason": "run abandoned"}}},
                    {"id": "archive", "kind": "tool", "tool": "ledger.archive",
                     "args": {"invoice_id": {"$ref": "input.invoice_id"}}, "idempotency_key": "arch-{{input.invoice_id}}",
                     "compensation": {"tool": "ledger.unarchive", "args": {"invoice_id": {"$ref": "input.invoice_id"}, "archive_id": {"$ref": "steps.archive.output.archive_id"}}}},
                    {"id": "finish", "kind": "tool", "tool": "ledger.lookup_vendor", "args": {"name": {"$ref": "steps.extract.output.vendor"}}}
                ]
            }
        }
    })
}

struct Harness {
    kernel: Kernel,
    clock: Arc<ManualClock>,
    bundle_id: String,
}

fn harness() -> Harness {
    harness_with(KernelConfig::default())
}

fn harness_with(config: KernelConfig) -> Harness {
    let clock = Arc::new(ManualClock::new(START_MS));
    let mut kernel = Kernel::in_memory(config, clock.clone()).expect("kernel");
    let publisher = KeyPair::generate(START_MS);
    kernel.trust_key(publisher.public());
    kernel.set_directory(
        serde_json::from_value(json!({"users": {"u-tom": {"role": "finance_admin", "manager": "u-cfo"}, "u-ana": {"role": "ap_clerk", "manager": "u-tom"}}}))
            .expect("directory"),
    );
    let b = bundle();
    let sig = sign_bundle(&b, &publisher);
    let applied = kernel.apply_bundle(b, sig).expect("apply bundle");
    kernel
        .apply_policy("finance-default", 1, FINANCE_DEFAULT)
        .expect("apply policy");
    Harness {
        kernel,
        clock,
        bundle_id: applied.bundle_id,
    }
}

impl Harness {
    fn remit(&self, autonomy: Autonomy, usd: Option<f64>) -> String {
        self.kernel
            .issue_remit(&IssueRequest {
                tools: vec!["ledger.*".into()],
                scopes: vec!["sql:table:*".into()],
                grants: vec![],
                spend: Spend { tokens: None, usd },
                autonomy: Some(autonomy),
                ttl_seconds: Some(3600),
                policy_set: vec!["finance-default".into()],
                requested_by: Some(json!({"id": "u-ana", "role": "ap_clerk", "manager": "u-tom"})),
            })
            .expect("remit")
            .remit_id
    }

    fn start(&self, remit_id: &str, total: f64) -> String {
        self.kernel
            .start_run(&StartRunRequest {
                bundle_id: self.bundle_id.clone(),
                workflow: "intake".into(),
                input: json!({"invoice_id": "inv-1001", "text": "Milk delivery", "total": total}),
                remit_id: remit_id.into(),
                requested_by: Some(json!({"id": "u-ana", "role": "ap_clerk", "manager": "u-tom"})),
            })
            .expect("start")
            .run_id
    }

    fn lease(&self, worker: &str) -> Option<LeaseGrant> {
        self.kernel
            .lease(&LeaseRequest {
                worker_id: worker.into(),
                kinds: vec![],
                ttl_seconds: Some(5),
            })
            .expect("lease")
    }

    fn must_lease(&self, worker: &str) -> LeaseGrant {
        self.lease(worker).expect("a runnable step")
    }

    fn check_materialised(&self, run_id: &str) {
        let events = self.kernel.all_events(run_id).expect("events");
        let folded = fold(&events).expect("fold");
        let materialised = self.kernel.get_run(run_id).expect("run");
        assert_eq!(
            folded, materialised,
            "materialised state differs from fold at seq {}",
            folded.last_seq
        );
        let rows = self
            .kernel
            .with_store(|c| store::steps_of_run(c, run_id))
            .expect("steps");
        assert_eq!(rows.len(), folded.steps.len());
        for (row, step) in rows.iter().zip(folded.steps.iter()) {
            assert_eq!(row.step_id, step.id);
            assert_eq!(row.idx, step.index);
            assert_eq!(row.state, step.state, "step {} state", step.id);
            assert_eq!(row.attempts, step.attempts, "step {} attempts", step.id);
            assert_eq!(
                row.lease_id,
                step.lease.as_ref().map(|l| l.lease_id.clone())
            );
        }
    }

    fn kinds(&self, run_id: &str) -> Vec<String> {
        self.kernel
            .all_events(run_id)
            .expect("events")
            .iter()
            .map(|e| e.kind.as_str().to_string())
            .collect()
    }

    fn run_through(&self, run_id: &str, worker: &str) {
        while let Some(lease) = self.lease(worker) {
            assert_eq!(lease.run_id, run_id);
            match lease.step_def["kind"].as_str() {
                Some("model") => {
                    let output = if lease.step == "extract" {
                        json!({"vendor": "Northwind Dairy", "total": lease.context["input"]["total"]})
                    } else {
                        json!({"account": "5100", "confidence": 0.93})
                    };
                    self.kernel
                        .complete(
                            &lease.lease_id,
                            output,
                            Some(Usage {
                                tokens: 100,
                                usd: 0.01,
                            }),
                        )
                        .expect("complete");
                }
                Some("action") => {
                    let action = json!({"kind": "payment.issue", "amount": lease.context["steps"]["extract"]["output"]["total"],
                                        "currency": "USD", "writes_to_system_of_record": true, "target": "ledger",
                                        "idempotency_key": "inv-1001", "summary": "Pay invoice inv-1001"});
                    let outcome = self
                        .kernel
                        .propose_action(&lease.lease_id, action)
                        .expect("propose");
                    match outcome.decision.as_str() {
                        "allow" => {
                            self.kernel
                                .complete(&lease.lease_id, json!({"action_id": outcome.action_id, "decision": "allow", "rule": outcome.rule}), None)
                                .expect("complete");
                        }
                        _ => break,
                    }
                }
                _ => {
                    let output = match lease.step.as_str() {
                        "post" => json!({"entry_id": 7, "posted_at": "2026-09-04T12:00:00.000Z"}),
                        "archive" => json!({"archive_id": 99}),
                        _ => json!({"rows": []}),
                    };
                    self.kernel
                        .complete(&lease.lease_id, output, None)
                        .expect("complete");
                }
            }
            self.check_materialised(run_id);
        }
    }
}

#[test]
fn happy_path_completes_and_replays() {
    let h = harness();
    let remit = h.remit(Autonomy::Autonomous, Some(2.0));
    let run_id = h.start(&remit, 1200.0);
    h.check_materialised(&run_id);
    let state = h.kernel.get_run(&run_id).expect("run");
    assert_eq!(state.state, RunStatus::Running);
    assert_eq!(state.steps.len(), 6);
    assert_eq!(state.department.as_deref(), Some("finance"));

    let first = h.must_lease("wrk-a1");
    assert_eq!(first.step, "extract");
    assert_eq!(first.attempt, 1);
    assert_eq!(first.context["run"]["department"], "finance");
    assert_eq!(first.context["remit"]["autonomy"], "autonomous");
    assert!(first.context["remit_token"]
        .as_str()
        .unwrap()
        .starts_with("krt1."));
    assert_eq!(first.context["pacing"], false);
    assert_eq!(
        first.context["prompts"]["extract"]["system"]
            .as_str()
            .unwrap(),
        "You extract fields from supplier invoices for Halcyon Provisions."
    );
    assert!(
        h.lease("wrk-a2").is_none(),
        "strict order: nothing else runnable"
    );
    h.kernel
        .complete(
            &first.lease_id,
            json!({"vendor": "Northwind Dairy", "total": 1200.0}),
            Some(Usage {
                tokens: 100,
                usd: 0.01,
            }),
        )
        .expect("complete");
    h.run_through(&run_id, "wrk-a1");

    let state = h.kernel.get_run(&run_id).expect("run");
    assert_eq!(state.state, RunStatus::Completed);
    assert_eq!(state.output, Some(json!({"rows": []})));
    assert_eq!(state.budget.used_usd, 0.02);
    assert_eq!(state.decisions.len(), 1);
    assert_eq!(state.decisions[0].decision, "allow");
    assert_eq!(state.decisions[0].rule, "default");
    let kinds = h.kinds(&run_id);
    assert_eq!(kinds.iter().filter(|k| *k == "usage.recorded").count(), 2);
    assert_eq!(kinds.last().map(String::as_str), Some("run.completed"));

    let report = h.kernel.replay(&run_id).expect("replay");
    assert!(report.chain_valid);
    assert!(report.state_matches);
    assert_eq!(report.decisions, 1);
    assert!(report.decision_mismatches.is_empty());
    assert_eq!(report.events, kinds.len() as u64);

    // Tamper with one payload directly in the store: the completion that carries the vendor.
    let events = h.kernel.all_events(&run_id).expect("events");
    let target = events
        .iter()
        .find(|e| {
            e.kind == EventKind::StepCompleted && e.payload.to_string().contains("Northwind Dairy")
        })
        .expect("a payload with the vendor")
        .seq;
    h.kernel
        .with_store(|c| {
            c.execute(
                "UPDATE events SET payload = replace(payload, 'Northwind Dairy', 'Northwind Diary') WHERE run_id = ?1 AND seq = ?2",
                rusqlite_params(&run_id, target),
            )?;
            Ok(())
        })
        .expect("tamper");
    let report = h.kernel.replay(&run_id).expect("replay");
    assert!(!report.chain_valid);
    assert_eq!(report.chain_errors[0]["seq"], target);
    assert_eq!(report.chain_errors[0]["error"], "hash_mismatch");
    assert!(
        !report.state_matches,
        "the tampered output changes the fold too"
    );
}

#[test]
fn approval_parks_resumes_and_reproposal_is_allowed() {
    let h = harness();
    let remit = h.remit(Autonomy::Supervised, None);
    let run_id = h.start(&remit, 7250.0);
    h.run_through(&run_id, "wrk-a1");
    let state = h.kernel.get_run(&run_id).expect("run");
    assert_eq!(state.state, RunStatus::Parked);
    assert_eq!(state.park_reason.as_deref(), Some("approval"));
    let pending = state.pending_approval.clone().expect("pending");
    assert_eq!(
        pending.approver,
        json!({"type": "role", "value": "finance_admin"})
    );
    let step = state.step("propose_payment").expect("step");
    assert_eq!(step.state, StepStatus::WaitingApproval);
    assert!(step.lease.is_none());
    assert_eq!(state.decisions[0].rule, "finance-default@1#0");
    assert!(h.lease("wrk-a1").is_none(), "parked runs lease nothing");
    let kinds = h.kinds(&run_id);
    let tail: Vec<&str> = kinds.iter().rev().take(5).map(String::as_str).collect();
    assert_eq!(
        tail,
        vec![
            "run.parked",
            "step.waiting_approval",
            "approval.requested",
            "policy.decided",
            "action.proposed"
        ]
    );

    let listed = h
        .kernel
        .list_approvals(Some("pending"), Some("role:finance_admin"))
        .expect("list");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["action"]["amount"], 7250.0);
    assert!(h
        .kernel
        .list_approvals(Some("pending"), Some("role:ap_clerk"))
        .expect("list")
        .is_empty());

    let wrong = h.kernel.decide_approval(
        &pending.approval_id,
        "approved",
        &DecisionActor {
            id: "u-ana".into(),
            role: "ap_clerk".into(),
        },
        "please",
    );
    assert_eq!(wrong.expect_err("wrong role").code(), "not_the_approver");
    let short = h.kernel.decide_approval(
        &pending.approval_id,
        "approved",
        &DecisionActor {
            id: "u-tom".into(),
            role: "finance_admin".into(),
        },
        "ok",
    );
    assert_eq!(short.expect_err("reason").code(), "reason_required");
    let outcome = h
        .kernel
        .decide_approval(
            &pending.approval_id,
            "approved",
            &DecisionActor {
                id: "u-tom".into(),
                role: "finance_admin".into(),
            },
            "Vendor verified",
        )
        .expect("approve");
    assert_eq!(outcome.run_state, RunStatus::Running);
    h.check_materialised(&run_id);
    let twice = h.kernel.decide_approval(
        &pending.approval_id,
        "approved",
        &DecisionActor {
            id: "u-tom".into(),
            role: "finance_admin".into(),
        },
        "again",
    );
    assert_eq!(twice.expect_err("twice").code(), "already_decided");

    let lease = h.must_lease("wrk-a2");
    assert_eq!(lease.step, "propose_payment");
    assert_eq!(lease.attempt, 1);
    assert_eq!(
        lease.context["approved_actions"],
        json!([pending.action_id])
    );
    let action = json!({"kind": "payment.issue", "amount": 7250.0, "currency": "USD", "writes_to_system_of_record": true,
                        "target": "ledger", "idempotency_key": "inv-1001", "summary": "Pay invoice inv-1001"});
    let again = h
        .kernel
        .propose_action(&lease.lease_id, action)
        .expect("propose");
    assert_eq!(again.decision.as_str(), "allow");
    assert_eq!(again.rule, format!("approved:{}", pending.approval_id));
    assert_eq!(again.action_id, pending.action_id);
    h.kernel
        .complete(&lease.lease_id, json!({"decision": "allow"}), None)
        .expect("complete");
    h.run_through(&run_id, "wrk-a2");
    let state = h.kernel.get_run(&run_id).expect("run");
    assert_eq!(state.state, RunStatus::Completed);

    let report = h.kernel.replay(&run_id).expect("replay");
    assert!(report.chain_valid && report.state_matches);
    assert_eq!(report.decisions, 2);
    assert!(report.decision_mismatches.is_empty());

    // Reconstruct who approved what from the log alone.
    let events = h.kernel.all_events(&run_id).expect("events");
    let decided = events
        .iter()
        .find(|e| e.kind == EventKind::ApprovalDecided)
        .expect("decided");
    assert_eq!(
        decided.payload["actor"],
        json!({"id": "u-tom", "role": "finance_admin"})
    );
    assert_eq!(decided.payload["reason"], "Vendor verified");
    let proposed = events
        .iter()
        .find(|e| e.kind == EventKind::ActionProposed)
        .expect("proposed");
    assert_eq!(proposed.payload["action"]["amount"], 7250.0);
}

#[test]
fn rejection_fails_the_run() {
    let h = harness();
    let remit = h.remit(Autonomy::Supervised, None);
    let run_id = h.start(&remit, 9000.0);
    h.run_through(&run_id, "wrk-a1");
    let approval_id = h
        .kernel
        .get_run(&run_id)
        .expect("run")
        .pending_approval
        .expect("pending")
        .approval_id;
    let outcome = h
        .kernel
        .decide_approval(
            &approval_id,
            "rejected",
            &DecisionActor {
                id: "u-tom".into(),
                role: "finance_admin".into(),
            },
            "Duplicate invoice",
        )
        .expect("reject");
    assert_eq!(outcome.run_state, RunStatus::Failed);
    let state = h.kernel.get_run(&run_id).expect("run");
    assert_eq!(
        state.error.as_ref().and_then(|e| e["code"].as_str()),
        Some("action_rejected")
    );
    assert_eq!(
        state.step("propose_payment").unwrap().state,
        StepStatus::Failed
    );
    assert!(!state.needs_human);
    h.check_materialised(&run_id);
    assert!(h.lease("w").is_none());
}

#[test]
fn sla_escalates_once_then_parks_for_a_human() {
    let h = harness();
    let remit = h.remit(Autonomy::Supervised, None);
    let run_id = h.start(&remit, 100.0);
    h.run_through(&run_id, "wrk-a1");
    let state = h.kernel.get_run(&run_id).expect("run");
    let pending = state.pending_approval.clone().expect("pending");
    assert_eq!(pending.approver, json!({"type": "user", "value": "u-tom"}));
    assert_eq!(state.decisions[0].rule, "finance-default@1#1");
    assert_eq!(h.kernel.sweep_approvals().expect("sweep"), 0);
    h.clock.advance(2_500);
    assert_eq!(h.kernel.sweep_approvals().expect("sweep"), 1);
    h.check_materialised(&run_id);
    let state = h.kernel.get_run(&run_id).expect("run");
    let escalated = state.pending_approval.clone().expect("pending");
    assert_eq!(
        escalated.approver,
        json!({"type": "user", "value": "u-cfo"})
    );
    assert_eq!(state.state, RunStatus::Parked);
    assert!(!state.needs_human);
    let events = h.kernel.all_events(&run_id).expect("events");
    let esc = events
        .iter()
        .find(|e| e.kind == EventKind::ApprovalEscalated)
        .expect("escalated");
    assert_eq!(
        esc.payload["from"],
        json!({"type": "user", "value": "u-tom"})
    );
    assert_eq!(esc.payload["to"], json!({"type": "user", "value": "u-cfo"}));
    assert_eq!(h.kernel.sweep_approvals().expect("sweep"), 0);
    h.clock.advance(2_500);
    assert_eq!(h.kernel.sweep_approvals().expect("sweep"), 1);
    let state = h.kernel.get_run(&run_id).expect("run");
    assert!(state.needs_human);
    assert_eq!(state.park_reason.as_deref(), Some("human"));
    h.clock.advance(10_000);
    assert_eq!(
        h.kernel.sweep_approvals().expect("sweep"),
        0,
        "no further escalation"
    );
    let resume = h.kernel.resume(&run_id, json!({"id": "u-ops"}));
    assert_eq!(resume.expect_err("pending").code(), "approval_pending");
    // The escalated-to user may decide, and so may the original approver.
    let outcome = h
        .kernel
        .decide_approval(
            &pending.approval_id,
            "approved",
            &DecisionActor {
                id: "u-cfo".into(),
                role: "cfo".into(),
            },
            "Approved late",
        )
        .expect("approve");
    assert_eq!(outcome.run_state, RunStatus::Running);
    assert!(!h.kernel.get_run(&run_id).expect("run").needs_human);
    h.run_through(&run_id, "wrk-a1");
    assert_eq!(
        h.kernel.get_run(&run_id).expect("run").state,
        RunStatus::Completed
    );

    // A role approver escalates to role admin per 04-POLICY.
    let remit = h.remit(Autonomy::Supervised, None);
    let run2 = h.start(&remit, 8000.0);
    h.run_through(&run2, "wrk-a1");
    h.clock.advance(4 * 3600 * 1000 + 1000);
    h.kernel.sweep_approvals().expect("sweep");
    let state = h.kernel.get_run(&run2).expect("run");
    assert_eq!(
        state.pending_approval.unwrap().approver,
        json!({"type": "role", "value": "admin"})
    );
}

#[test]
fn retries_back_off_and_quarantine() {
    let h = harness();
    let remit = h.remit(Autonomy::Autonomous, None);
    let run_id = h.start(&remit, 10.0);
    let lease = h.must_lease("w");
    let result = h
        .kernel
        .fail(
            &lease.lease_id,
            ErrorInfo {
                code: "timeout".into(),
                message: "upstream slow".into(),
            },
            false,
        )
        .expect("fail");
    assert_eq!(result.outcome, "retry_scheduled");
    let delay = result.delay_ms.expect("delay");
    assert!(
        (250..=500).contains(&delay),
        "first backoff is 500 ms with equal jitter, got {delay}"
    );
    h.check_materialised(&run_id);
    assert!(h.lease("w").is_none(), "not runnable before the delay");
    h.clock.advance(delay as i64);
    let lease = h.must_lease("w");
    assert_eq!(lease.attempt, 2);
    let result = h
        .kernel
        .fail(
            &lease.lease_id,
            ErrorInfo {
                code: "timeout".into(),
                message: "again".into(),
            },
            false,
        )
        .expect("fail");
    let delay2 = result.delay_ms.expect("delay");
    assert!(
        (500..=1000).contains(&delay2),
        "second backoff doubles, got {delay2}"
    );
    h.clock.advance(delay2 as i64);

    // Deterministic failures retry immediately and quarantine at three attempts.
    let lease = h.must_lease("w");
    assert_eq!(lease.attempt, 3);
    let result = h
        .kernel
        .fail(
            &lease.lease_id,
            ErrorInfo {
                code: "output_invalid".into(),
                message: "bad".into(),
            },
            true,
        )
        .expect("fail");
    assert_eq!(
        result.outcome, "quarantined",
        "attempt 3 deterministic quarantines"
    );
    let state = h.kernel.get_run(&run_id).expect("run");
    assert_eq!(state.state, RunStatus::Parked);
    assert_eq!(state.park_reason.as_deref(), Some("quarantine"));
    assert_eq!(
        state.step("extract").unwrap().state,
        StepStatus::Quarantined
    );
    assert!(h.lease("w").is_none(), "no fourth lease");
    let events = h.kernel.all_events(&run_id).expect("events");
    let q = events
        .iter()
        .find(|e| e.kind == EventKind::StepQuarantined)
        .expect("quarantined");
    assert_eq!(q.payload["attempts"], 3);
    h.check_materialised(&run_id);

    // Resume re-schedules the step with a fresh attempt budget.
    h.kernel
        .resume(&run_id, json!({"id": "u-ops"}))
        .expect("resume");
    let lease = h.must_lease("w");
    assert_eq!(lease.attempt, 1);
    h.check_materialised(&run_id);

    // Three deterministic failures in a row from fresh.
    for expected in [1u32, 2] {
        assert_eq!(lease_attempt(&h, &run_id), expected);
        let l = h
            .kernel
            .get_run(&run_id)
            .unwrap()
            .step("extract")
            .unwrap()
            .lease
            .clone()
            .unwrap();
        let r = h
            .kernel
            .fail(
                &l.lease_id,
                ErrorInfo {
                    code: "x".into(),
                    message: "y".into(),
                },
                true,
            )
            .expect("fail");
        assert_eq!(r.outcome, "retry_scheduled");
        assert_eq!(r.delay_ms, Some(0));
        h.must_lease("w");
    }
    let l = h
        .kernel
        .get_run(&run_id)
        .unwrap()
        .step("extract")
        .unwrap()
        .lease
        .clone()
        .unwrap();
    let r = h
        .kernel
        .fail(
            &l.lease_id,
            ErrorInfo {
                code: "x".into(),
                message: "y".into(),
            },
            true,
        )
        .expect("fail");
    assert_eq!(r.outcome, "quarantined");

    // A refusal parks at once with reason refusal.
    let remit = h.remit(Autonomy::Autonomous, None);
    let run2 = h.start(&remit, 10.0);
    let lease = h.must_lease("w");
    assert_eq!(lease.run_id, run2);
    let r = h
        .kernel
        .fail(
            &lease.lease_id,
            ErrorInfo {
                code: "model_refused".into(),
                message: "no".into(),
            },
            true,
        )
        .expect("fail");
    assert_eq!(r.outcome, "parked");
    assert_eq!(
        h.kernel.get_run(&run2).unwrap().park_reason.as_deref(),
        Some("refusal")
    );

    // connector_quarantined names its park reason after five non-deterministic failures.
    let remit = h.remit(Autonomy::Autonomous, None);
    let run3 = h.start(&remit, 10.0);
    for _ in 0..5 {
        h.clock.advance(60_000);
        let lease = h.must_lease("w");
        assert_eq!(lease.run_id, run3);
        h.kernel
            .fail(
                &lease.lease_id,
                ErrorInfo {
                    code: "connector_quarantined".into(),
                    message: "ledger".into(),
                },
                false,
            )
            .expect("fail");
    }
    let state = h.kernel.get_run(&run3).expect("run");
    assert_eq!(state.park_reason.as_deref(), Some("connector_quarantined"));
    assert_eq!(state.step("extract").unwrap().attempts, 5);
}

fn rusqlite_params(run_id: &str, seq: u64) -> [Box<dyn rusqlite::ToSql>; 2] {
    [Box::new(run_id.to_string()), Box::new(seq as i64)]
}

fn lease_attempt(h: &Harness, run_id: &str) -> u32 {
    h.kernel
        .get_run(run_id)
        .unwrap()
        .step("extract")
        .unwrap()
        .attempts
}

#[test]
fn leases_expire_and_are_retaken() {
    let h = harness();
    let remit = h.remit(Autonomy::Autonomous, None);
    let run_id = h.start(&remit, 10.0);
    let lease = h.must_lease("wrk-a1");
    let expires = h.kernel.heartbeat(&lease.lease_id).expect("heartbeat");
    assert!(expires >= lease.expires_at);
    h.clock.advance(4_000);
    h.kernel.heartbeat(&lease.lease_id).expect("still alive");
    h.clock.advance(5_001);
    assert_eq!(h.kernel.sweep_leases().expect("sweep"), 1);
    assert_eq!(h.kernel.sweep_leases().expect("sweep"), 0);
    h.check_materialised(&run_id);
    let state = h.kernel.get_run(&run_id).expect("run");
    assert_eq!(state.step("extract").unwrap().state, StepStatus::Scheduled);
    assert!(h.kinds(&run_id).contains(&"step.lease_expired".to_string()));
    assert_eq!(
        h.kernel
            .heartbeat(&lease.lease_id)
            .expect_err("gone")
            .code(),
        "lease_expired"
    );
    assert_eq!(
        h.kernel
            .complete(&lease.lease_id, json!({}), None)
            .expect_err("gone")
            .code(),
        "lease_expired"
    );

    let second = h.must_lease("wrk-a2");
    assert_eq!(second.step, "extract");
    assert_eq!(second.attempt, 2);
    assert_ne!(second.lease_id, lease.lease_id);
    // Lazy expiry: an expired lease is swept on use even without the sweeper.
    h.clock.advance(6_000);
    assert_eq!(
        h.kernel
            .complete(&second.lease_id, json!({}), None)
            .expect_err("expired")
            .code(),
        "lease_expired"
    );
    let third = h.must_lease("wrk-a3");
    assert_eq!(third.attempt, 3);
    assert_eq!(
        h.kernel.heartbeat("lse_missing").expect_err("404").code(),
        "lease_not_found"
    );
}

#[test]
fn budget_paces_then_parks() {
    let h = harness();
    let remit = h.remit(Autonomy::Autonomous, Some(1.0));
    let run_id = h.start(&remit, 10.0);
    let lease = h.must_lease("w");
    h.kernel
        .complete(
            &lease.lease_id,
            json!({"vendor": "Northwind Dairy", "total": 10.0}),
            Some(Usage {
                tokens: 10,
                usd: 0.85,
            }),
        )
        .expect("complete");
    let state = h.kernel.get_run(&run_id).expect("run");
    assert!(state.budget.soft_hit);
    assert!(!state.budget.exceeded);
    assert_eq!(state.state, RunStatus::Running);
    let lease = h.must_lease("w");
    assert_eq!(lease.context["pacing"], true);
    h.kernel
        .complete(
            &lease.lease_id,
            json!({"account": "5100"}),
            Some(Usage {
                tokens: 10,
                usd: 0.3,
            }),
        )
        .expect("complete");
    let state = h.kernel.get_run(&run_id).expect("run");
    assert!(state.budget.exceeded);
    assert_eq!(state.state, RunStatus::Parked);
    assert_eq!(state.park_reason.as_deref(), Some("budget"));
    assert!(h.lease("w").is_none());
    let kinds = h.kinds(&run_id);
    assert_eq!(
        kinds
            .iter()
            .filter(|k| *k == "budget.soft_threshold")
            .count(),
        1
    );
    assert!(kinds.contains(&"budget.exceeded".to_string()));
    assert_eq!(
        h.kernel
            .resume(&run_id, json!({}))
            .expect_err("budget")
            .code(),
        "run_not_resumable"
    );
    h.check_materialised(&run_id);
}

#[test]
fn abandon_schedules_compensations_in_reverse_order() {
    let h = harness();
    let remit = h.remit(Autonomy::Autonomous, None);
    let run_id = h.start(&remit, 10.0);
    // Complete everything up to and including archive, then stop.
    for _ in 0..5 {
        let lease = h.must_lease("w");
        match lease.step.as_str() {
            "extract" => h
                .kernel
                .complete(
                    &lease.lease_id,
                    json!({"vendor": "Northwind Dairy", "total": 10.0}),
                    None,
                )
                .unwrap(),
            "code" => h
                .kernel
                .complete(&lease.lease_id, json!({"account": "5100"}), None)
                .unwrap(),
            "propose_payment" => {
                let o = h.kernel.propose_action(&lease.lease_id, json!({"kind": "payment.issue", "amount": 10.0, "writes_to_system_of_record": true})).unwrap();
                assert_eq!(o.decision.as_str(), "allow");
                h.kernel
                    .complete(&lease.lease_id, json!({"action_id": o.action_id}), None)
                    .unwrap()
            }
            "post" => h
                .kernel
                .complete(&lease.lease_id, json!({"entry_id": 7}), None)
                .unwrap(),
            "archive" => h
                .kernel
                .complete(&lease.lease_id, json!({"archive_id": 99}), None)
                .unwrap(),
            other => panic!("unexpected step {other}"),
        };
    }
    let finish = h.must_lease("w");
    assert_eq!(finish.step, "finish");
    let scheduled = h
        .kernel
        .abandon(&run_id, "operator request", json!({"id": "u-ops"}))
        .expect("abandon");
    assert_eq!(scheduled, 2);
    h.check_materialised(&run_id);
    assert_eq!(
        h.kernel
            .complete(&finish.lease_id, json!({}), None)
            .expect_err("released")
            .code(),
        "lease_expired"
    );
    let state = h.kernel.get_run(&run_id).expect("run");
    assert_eq!(
        state.state,
        RunStatus::Running,
        "the run keeps running until its writes are unwound"
    );
    assert!(state.abandoning);
    assert!(!state.is_terminal());
    assert_eq!(
        state
            .compensations
            .iter()
            .map(|c| c.for_step.as_str())
            .collect::<Vec<_>>(),
        vec!["archive", "post"]
    );
    let events = h.kernel.all_events(&run_id).expect("events");
    let comps: Vec<&Value> = events
        .iter()
        .filter(|e| e.kind == EventKind::CompensationScheduled)
        .map(|e| &e.payload)
        .collect();
    assert_eq!(comps[0]["for_step"], "archive");
    assert_eq!(
        comps[0]["args"],
        json!({"invoice_id": "inv-1001", "archive_id": 99})
    );
    assert_eq!(comps[1]["for_step"], "post");
    assert_eq!(comps[1]["tool"], "ledger.void_entry");
    assert_eq!(
        comps[1]["args"],
        json!({"entry_id": 7, "reason": "run abandoned"})
    );

    let c1 = h.must_lease("w");
    assert_eq!(c1.step_def["kind"], "compensation");
    assert_eq!(c1.step_def["tool"], "ledger.unarchive");
    assert_eq!(c1.step_def["for_step"], "archive");
    assert_eq!(
        c1.step_def["idempotency_key"],
        format!("comp:{run_id}:archive")
    );
    assert!(h.lease("w2").is_none(), "compensations run one at a time");
    let result = h
        .kernel
        .complete(&c1.lease_id, json!({"ok": true}), None)
        .expect("complete");
    assert_eq!(
        result.run_state,
        RunStatus::Running,
        "one compensation left, so the run is not abandoned yet"
    );
    assert_eq!(result.next_step.as_deref(), Some("comp_post"));
    let c2 = h.must_lease("w");
    assert_eq!(c2.step_def["tool"], "ledger.void_entry");
    assert_eq!(c2.step_def["args"]["entry_id"], 7);
    assert_eq!(
        h.kernel
            .abandon(&run_id, "again", json!({}))
            .expect_err("already unwinding")
            .code(),
        "run_not_abandonable"
    );
    let result = h
        .kernel
        .complete(&c2.lease_id, json!({"voided": true}), None)
        .expect("complete");
    assert_eq!(result.run_state, RunStatus::Abandoned);
    assert_eq!(result.next_step, None);
    let state = h.kernel.get_run(&run_id).expect("run");
    assert_eq!(state.state, RunStatus::Abandoned);
    assert!(state.compensations.iter().all(|c| c.state == "completed"));
    assert!(state.is_terminal());
    assert!(h.lease("w").is_none());
    h.check_materialised(&run_id);
    let kinds = h.kinds(&run_id);
    assert_eq!(
        kinds
            .iter()
            .filter(|k| *k == "compensation.completed")
            .count(),
        2
    );
    assert!(h.kernel.replay(&run_id).expect("replay").state_matches);

    // A failing compensation ends the run failed with needs_human.
    let remit = h.remit(Autonomy::Autonomous, None);
    let run2 = h.start(&remit, 10.0);
    let lease = h.must_lease("w");
    h.kernel
        .complete(
            &lease.lease_id,
            json!({"vendor": "Northwind Dairy", "total": 10.0}),
            None,
        )
        .unwrap();
    let lease = h.must_lease("w");
    h.kernel
        .complete(&lease.lease_id, json!({"account": "5100"}), None)
        .unwrap();
    let lease = h.must_lease("w");
    let o = h
        .kernel
        .propose_action(
            &lease.lease_id,
            json!({"kind": "payment.issue", "amount": 10.0, "writes_to_system_of_record": true}),
        )
        .unwrap();
    h.kernel
        .complete(&lease.lease_id, json!({"action_id": o.action_id}), None)
        .unwrap();
    let lease = h.must_lease("w");
    h.kernel
        .complete(&lease.lease_id, json!({"entry_id": 8}), None)
        .unwrap();
    assert_eq!(
        h.kernel
            .abandon(&run2, "test", json!({"id": "u-ops"}))
            .unwrap(),
        1
    );
    for _ in 0..3 {
        let c = h.must_lease("w");
        assert_eq!(c.run_id, run2);
        h.kernel
            .fail(
                &c.lease_id,
                ErrorInfo {
                    code: "void_failed".into(),
                    message: "period closed".into(),
                },
                true,
            )
            .unwrap();
    }
    let state = h.kernel.get_run(&run2).expect("run");
    assert_eq!(state.state, RunStatus::Failed);
    assert!(state.needs_human);
    assert_eq!(state.compensations[0].state, "failed");
    assert!(h.kinds(&run2).contains(&"compensation.failed".to_string()));
    assert_eq!(
        h.kernel
            .abandon(&run_id, "again", json!({}))
            .expect_err("terminal")
            .code(),
        "run_not_abandonable"
    );
}

#[test]
fn trusting_a_key_takes_effect_without_a_restart() {
    // The published quickstart starts the kernel first and trusts a publisher
    // key afterwards, so a key installed while the kernel is open must work.
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = Arc::new(ManualClock::new(START_MS));
    let kernel = Kernel::open(dir.path(), KernelConfig::default(), clock).expect("open");
    let trusted_dir = dir.path().join("keys").join("trusted");
    assert!(trusted_dir.is_dir());
    assert!(kernel.trusted_keys().is_empty());

    let publisher = KeyPair::generate(START_MS);
    let b = bundle();
    let signature = sign_bundle(&b, &publisher);
    let refused = kernel
        .apply_bundle(b.clone(), signature.clone())
        .expect_err("not trusted yet");
    assert_eq!(refused.code(), "bundle_signature_invalid");

    // `kernos keys trust` is exactly this: write the key into the directory.
    let key_file = trusted_dir.join(format!("{}.pub", publisher.key_id));
    publisher.write_public(&key_file).expect("trust");
    assert_eq!(kernel.trusted_keys().len(), 1);
    let applied = kernel
        .apply_bundle(b.clone(), signature.clone())
        .expect("trusted now");
    assert!(applied.created);
    assert_eq!(applied.name, "halcyon.finance.invoice_intake");

    // Untrusting is equally immediate: the next signature from that key fails.
    std::fs::remove_file(&key_file).expect("untrust");
    assert!(kernel.trusted_keys().is_empty());
    let mut next = b.clone();
    next["version"] = json!("1.1.0");
    let refused = kernel
        .apply_bundle(next.clone(), sign_bundle(&next, &publisher))
        .expect_err("untrusted");
    assert_eq!(refused.code(), "bundle_signature_invalid");

    // A second publisher trusted later works too, and the first stays out.
    let second = KeyPair::generate(START_MS + 1);
    second
        .write_public(&trusted_dir.join(format!("{}.pub", second.key_id)))
        .expect("trust");
    let applied = kernel
        .apply_bundle(next.clone(), sign_bundle(&next, &second))
        .expect("second publisher");
    assert!(applied.created);
    assert_eq!(applied.version, "1.1.0");
    assert_eq!(kernel.trusted_keys().len(), 1);
    let mut third = b.clone();
    third["version"] = json!("1.2.0");
    assert_eq!(
        kernel
            .apply_bundle(third.clone(), sign_bundle(&third, &publisher))
            .expect_err("still untrusted")
            .code(),
        "bundle_signature_invalid"
    );

    // A key trusted in process needs no file and survives directory changes.
    kernel.trust_key(publisher.public());
    assert_eq!(
        kernel
            .apply_bundle(third.clone(), sign_bundle(&third, &publisher))
            .expect("pinned key")
            .version,
        "1.2.0"
    );
}

#[test]
fn abandoned_means_unwound() {
    let h = harness();
    let remit = h.remit(Autonomy::Autonomous, None);
    let run_id = h.start(&remit, 10.0);
    // Two writes, both with a compensation.
    let lease = h.must_lease("w");
    h.kernel
        .complete(
            &lease.lease_id,
            json!({"vendor": "Northwind Dairy", "total": 10.0}),
            None,
        )
        .expect("extract");
    let lease = h.must_lease("w");
    h.kernel
        .complete(&lease.lease_id, json!({"account": "5100"}), None)
        .expect("code");
    let lease = h.must_lease("w");
    let outcome = h
        .kernel
        .propose_action(
            &lease.lease_id,
            json!({"kind": "payment.issue", "amount": 10.0, "writes_to_system_of_record": true}),
        )
        .expect("propose");
    h.kernel
        .complete(
            &lease.lease_id,
            json!({"action_id": outcome.action_id}),
            None,
        )
        .expect("action");
    let lease = h.must_lease("w");
    h.kernel
        .complete(&lease.lease_id, json!({"entry_id": 7}), None)
        .expect("post");
    let lease = h.must_lease("w");
    h.kernel
        .complete(&lease.lease_id, json!({"archive_id": 99}), None)
        .expect("archive");

    assert_eq!(
        h.kernel
            .abandon(&run_id, "test", json!({"id": "u-ops"}))
            .expect("abandon"),
        2
    );
    for expected in ["comp_archive", "comp_post"] {
        let state = h.kernel.get_run(&run_id).expect("run");
        assert_eq!(
            state.state,
            RunStatus::Running,
            "still unwinding before {expected} completed"
        );
        assert!(!state.is_terminal());
        assert!(state.unwind_pending());
        assert_eq!(state.next_step().map(|s| s.id.as_str()), Some(expected));
        let lease = h.must_lease("w");
        assert_eq!(lease.step, expected);
        h.kernel
            .complete(&lease.lease_id, json!({"ok": true}), None)
            .expect("compensation");
        h.check_materialised(&run_id);
    }
    let state = h.kernel.get_run(&run_id).expect("run");
    assert_eq!(
        state.state,
        RunStatus::Abandoned,
        "abandoned only once the last compensation completed"
    );
    assert!(state.is_terminal());
    assert!(!state.unwind_pending());
    assert!(state.compensations.iter().all(|c| c.state == "completed"));
    assert!(h.lease("w").is_none());
    assert!(h.kernel.replay(&run_id).expect("replay").state_matches);

    // A run with nothing to unwind is abandoned at once.
    let remit = h.remit(Autonomy::Autonomous, None);
    let clean = h.start(&remit, 10.0);
    let lease = h.must_lease("w");
    assert_eq!(lease.run_id, clean);
    h.kernel
        .complete(
            &lease.lease_id,
            json!({"vendor": "Northwind Dairy", "total": 10.0}),
            None,
        )
        .expect("extract");
    assert_eq!(
        h.kernel
            .abandon(&clean, "no writes yet", json!({"id": "u-ops"}))
            .expect("abandon"),
        0
    );
    let state = h.kernel.get_run(&clean).expect("run");
    assert_eq!(state.state, RunStatus::Abandoned);
    assert!(state.is_terminal());
    assert!(state.compensations.is_empty());
    assert!(state.next_step().is_none());
    assert!(h.lease("w").is_none(), "an abandoned run leases nothing");
    h.check_materialised(&clean);
    let report = h.kernel.replay(&clean).expect("replay");
    assert!(report.chain_valid && report.state_matches);
}

#[test]
fn external_events_follow_the_permission_rules() {
    let h = harness();
    let remit = h.remit(Autonomy::Autonomous, None);
    let run_id = h.start(&remit, 10.0);
    let lease = h.must_lease("wrk-a1");
    let actor = json!({"type": "worker", "id": "wrk-a1"});
    let payload = json!({"step": "extract", "model": "mock", "tier": "standard", "effort": "low", "provider": "mock", "prefix_hash": "a", "input_hash": "b", "max_tokens": 10});
    let appended = h
        .kernel
        .post_external_event(
            &run_id,
            "model.called",
            payload.clone(),
            actor.clone(),
            &ExternalAuth::Lease(lease.lease_id.clone()),
        )
        .expect("append");
    assert_eq!(appended.seq, 9);
    assert_eq!(appended.hash.len(), 64);
    let wrong_step = h.kernel.post_external_event(
        &run_id,
        "model.called",
        json!({"step": "code"}),
        actor.clone(),
        &ExternalAuth::Lease(lease.lease_id.clone()),
    );
    assert_eq!(wrong_step.expect_err("step").code(), "event_not_permitted");
    let internal = h.kernel.post_external_event(
        &run_id,
        "step.completed",
        payload.clone(),
        actor.clone(),
        &ExternalAuth::Lease(lease.lease_id.clone()),
    );
    assert_eq!(internal.expect_err("kind").code(), "event_not_permitted");
    let none = h.kernel.post_external_event(
        &run_id,
        "note",
        json!({"text": "hi"}),
        actor.clone(),
        &ExternalAuth::None,
    );
    assert_eq!(none.expect_err("none").code(), "event_not_permitted");
    let bogus = h.kernel.post_external_event(
        &run_id,
        "note",
        json!({"text": "hi"}),
        actor.clone(),
        &ExternalAuth::Lease("lse_nope".into()),
    );
    assert_eq!(bogus.expect_err("lease").code(), "event_not_permitted");
    let missing_run = h.kernel.post_external_event(
        "run_nope",
        "note",
        json!({"text": "hi"}),
        actor.clone(),
        &ExternalAuth::Lease(lease.lease_id.clone()),
    );
    assert_eq!(missing_run.expect_err("run").code(), "run_not_found");

    let token = lease.context["remit_token"].as_str().unwrap().to_string();
    let gateway = json!({"type": "gateway", "id": "gw-1"});
    h.kernel
        .post_external_event(&run_id, "tool.refused", json!({"step": "extract", "tool": "ledger.post_entry", "reason": "tool_not_in_remit", "remit_id": "rem_x", "detail": {}}), gateway.clone(), &ExternalAuth::Remit(token))
        .expect("gateway append");
    let other_remit = h.remit(Autonomy::Autonomous, None);
    let other_run = h.start(&other_remit, 10.0);
    let other_lease = h.must_lease("wrk-b");
    assert_eq!(other_lease.run_id, other_run);
    let other_token = other_lease.context["remit_token"]
        .as_str()
        .unwrap()
        .to_string();
    let cross = h.kernel.post_external_event(
        &run_id,
        "note",
        json!({"text": "x"}),
        gateway.clone(),
        &ExternalAuth::Remit(other_token),
    );
    assert_eq!(cross.expect_err("cross").code(), "event_not_permitted");
    let malformed = h.kernel.post_external_event(
        &run_id,
        "note",
        json!({"text": "x"}),
        gateway,
        &ExternalAuth::Remit("krt1.bad".into()),
    );
    assert_eq!(
        malformed.expect_err("malformed").code(),
        "event_not_permitted"
    );

    // model.responded never records usage; the prior_events context carries tool events only.
    h.kernel
        .post_external_event(&run_id, "model.responded", json!({"step": "extract", "output": {"text": "x"}, "usage": {"input_tokens": 5, "output_tokens": 5}, "cost_usd": 9.0}), actor.clone(), &ExternalAuth::Lease(lease.lease_id.clone()))
        .expect("append");
    h.kernel
        .post_external_event(&run_id, "tool.called", json!({"step": "extract", "tool": "ledger.lookup_vendor", "args": {}, "idempotency_key": null}), actor, &ExternalAuth::Lease(lease.lease_id.clone()))
        .expect("append");
    let state = h.kernel.get_run(&run_id).expect("run");
    assert_eq!(state.budget.used_usd, 0.0);
    h.check_materialised(&run_id);
    h.clock.advance(6_000);
    h.kernel.sweep_leases().expect("sweep");
    let again = h.must_lease("wrk-a2");
    assert_eq!(again.run_id, run_id);
    assert_eq!(again.context["prior_events"].as_array().unwrap().len(), 1);
    assert_eq!(again.context["prior_events"][0]["kind"], "tool.called");
}

#[test]
fn remits_and_run_start_validation() {
    let h = harness();
    let parent = h
        .kernel
        .issue_remit(&IssueRequest {
            tools: vec!["ledger.post_entry".into()],
            scopes: vec!["sql:table:ledger_entries".into()],
            grants: vec![],
            spend: Spend {
                tokens: None,
                usd: Some(2.0),
            },
            autonomy: Some(Autonomy::Supervised),
            ttl_seconds: Some(3600),
            policy_set: vec!["finance-default".into()],
            requested_by: Some(json!({"id": "u-ana", "role": "ap_clerk", "manager": "u-tom"})),
        })
        .expect("issue");
    assert!(parent.token.starts_with("krt1."));
    let verified = h.kernel.verify_remit_token(&parent.token).expect("verify");
    assert_eq!(verified.rid, parent.remit_id);
    let field = |req: DeriveRequest| {
        h.kernel
            .derive_remit(&parent.remit_id, &req)
            .expect_err("widens")
            .details()["field"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert_eq!(
        field(DeriveRequest {
            tools: Some(vec!["ledger.*".into()]),
            ..Default::default()
        }),
        "tools"
    );
    assert_eq!(
        field(DeriveRequest {
            spend: Some(Spend {
                tokens: None,
                usd: Some(3.0)
            }),
            ..Default::default()
        }),
        "spend.usd"
    );
    assert_eq!(
        field(DeriveRequest {
            autonomy: Some(Autonomy::Autonomous),
            ..Default::default()
        }),
        "autonomy"
    );
    let child = h
        .kernel
        .derive_remit(
            &parent.remit_id,
            &DeriveRequest {
                tools: Some(vec!["ledger.post_entry".into()]),
                spend: Some(Spend {
                    tokens: None,
                    usd: Some(1.0),
                }),
                autonomy: Some(Autonomy::Propose),
                ..Default::default()
            },
        )
        .expect("derive");
    assert_eq!(child.parent_id.as_deref(), Some(parent.remit_id.as_str()));
    let shown = h.kernel.get_remit(&child.remit_id).expect("get");
    assert_eq!(shown["autonomy"], "propose");
    assert_eq!(shown["spend"]["usd"], 1.0);
    assert_eq!(shown["parent_id"], parent.remit_id);

    let bad_input = h.kernel.start_run(&StartRunRequest {
        bundle_id: h.bundle_id.clone(),
        workflow: "intake".into(),
        input: json!({"invoice_id": "x"}),
        remit_id: parent.remit_id.clone(),
        requested_by: None,
    });
    let err = bad_input.expect_err("input");
    assert_eq!(err.code(), "input_invalid");
    assert_eq!(err.details()["path"], "text");
    let wrong_type = h.kernel.start_run(&StartRunRequest {
        bundle_id: h.bundle_id.clone(),
        workflow: "intake".into(),
        input: json!({"invoice_id": "x", "text": "t", "total": "7"}),
        remit_id: parent.remit_id.clone(),
        requested_by: None,
    });
    assert_eq!(wrong_type.expect_err("type").details()["path"], "total");
    let no_workflow = h.kernel.start_run(&StartRunRequest {
        bundle_id: h.bundle_id.clone(),
        workflow: "nope".into(),
        input: json!({}),
        remit_id: parent.remit_id.clone(),
        requested_by: None,
    });
    assert_eq!(no_workflow.expect_err("wf").code(), "workflow_not_found");
    // A remit without the bundle's tools still starts: refusal is the gateway's job.
    let run_id = h.start(&parent.remit_id, 10.0);
    let bound = h.kernel.start_run(&StartRunRequest {
        bundle_id: h.bundle_id.clone(),
        workflow: "intake".into(),
        input: json!({"invoice_id": "x", "text": "t", "total": 1.0}),
        remit_id: parent.remit_id.clone(),
        requested_by: None,
    });
    assert_eq!(bound.expect_err("bound").code(), "remit_bound");
    let shown = h.kernel.get_remit(&parent.remit_id).expect("get");
    assert_eq!(shown["run_id"], run_id);
    let no_policy = h
        .kernel
        .issue_remit(&IssueRequest {
            tools: vec!["ledger.*".into()],
            policy_set: vec![],
            ..Default::default()
        })
        .expect("issue");
    let missing = h.kernel.start_run(&StartRunRequest {
        bundle_id: h.bundle_id.clone(),
        workflow: "intake".into(),
        input: json!({"invoice_id": "x", "text": "t", "total": 1.0}),
        remit_id: no_policy.remit_id,
        requested_by: Some(json!({"id": "u-ana"})),
    });
    assert_eq!(
        missing.expect_err("policy").code(),
        "remit_policy_set_missing"
    );
    h.clock.advance(3_600_001);
    let expired = h.kernel.start_run(&StartRunRequest {
        bundle_id: h.bundle_id.clone(),
        workflow: "intake".into(),
        input: json!({"invoice_id": "x", "text": "t", "total": 1.0}),
        remit_id: child.remit_id,
        requested_by: None,
    });
    assert_eq!(expired.expect_err("expired").code(), "remit_expired");
}

/// Drives the kernel with random operations under a seeded RNG and checks after
/// every one that every run's materialised state equals the fold of its log.
#[test]
fn randomised_operations_keep_tables_equal_to_fold() {
    for seed in 1..=5u64 {
        let h = harness();
        let mut rng = StdRng::seed_from_u64(seed);
        let mut runs: Vec<String> = Vec::new();
        let mut leases: Vec<String> = Vec::new();
        let mut appended = 0usize;
        for _ in 0..220 {
            let op = rng.gen_range(0..100);
            match op {
                0..=7 => {
                    let autonomy = if rng.gen_bool(0.5) {
                        Autonomy::Autonomous
                    } else {
                        Autonomy::Supervised
                    };
                    let usd = if rng.gen_bool(0.3) {
                        Some(0.05)
                    } else {
                        Some(5.0)
                    };
                    let remit = h.remit(autonomy, usd);
                    let total = if rng.gen_bool(0.5) { 100.0 } else { 9000.0 };
                    runs.push(h.start(&remit, total));
                }
                8..=35 => {
                    if let Some(lease) = h.lease(&format!("wrk-{}", rng.gen_range(0..3))) {
                        leases.push(lease.lease_id);
                    }
                }
                36..=60 => {
                    if leases.is_empty() {
                        continue;
                    }
                    let lease_id = leases[rng.gen_range(0..leases.len())].clone();
                    let choice = rng.gen_range(0..10);
                    let result = if choice < 5 {
                        let usage = rng.gen_bool(0.5).then_some(Usage {
                            tokens: 10,
                            usd: 0.02,
                        });
                        h.kernel
                            .complete(&lease_id, json!({"vendor": "Northwind Dairy", "total": 100.0, "account": "5100", "entry_id": 1, "archive_id": 2}), usage)
                            .map(|_| ())
                    } else if choice < 8 {
                        let deterministic = rng.gen_bool(0.5);
                        let code = if rng.gen_bool(0.1) {
                            "model_refused"
                        } else {
                            "boom"
                        };
                        h.kernel
                            .fail(
                                &lease_id,
                                ErrorInfo {
                                    code: code.into(),
                                    message: "m".into(),
                                },
                                deterministic,
                            )
                            .map(|_| ())
                    } else if choice < 9 {
                        let amount = if rng.gen_bool(0.5) { 100.0 } else { 9000.0 };
                        let action = json!({"kind": "payment.issue", "amount": amount, "writes_to_system_of_record": true, "idempotency_key": "inv-1001"});
                        h.kernel.propose_action(&lease_id, action).map(|_| ())
                    } else {
                        h.kernel.heartbeat(&lease_id).map(|_| ())
                    };
                    if let Err(e) = result {
                        assert!(
                            matches!(e.code(), "lease_expired" | "lease_not_found"),
                            "unexpected {e}"
                        );
                    }
                }
                61..=70 => {
                    let pending = h
                        .kernel
                        .list_approvals(Some("pending"), None)
                        .expect("list");
                    if pending.is_empty() {
                        continue;
                    }
                    let a = &pending[rng.gen_range(0..pending.len())];
                    let approver = a["approver"].clone();
                    let (id, role) = if approver["type"] == "role" {
                        (
                            "u-x".to_string(),
                            approver["value"].as_str().unwrap().to_string(),
                        )
                    } else {
                        (
                            approver["value"].as_str().unwrap().to_string(),
                            "any".to_string(),
                        )
                    };
                    let decision = if rng.gen_bool(0.7) {
                        "approved"
                    } else {
                        "rejected"
                    };
                    let r = h.kernel.decide_approval(
                        a["approval_id"].as_str().unwrap(),
                        decision,
                        &DecisionActor { id, role },
                        "random decision",
                    );
                    if let Err(e) = r {
                        assert_eq!(e.code(), "already_decided", "unexpected {e}");
                    }
                }
                71..=80 => {
                    h.clock.advance(rng.gen_range(100..8_000));
                    h.kernel.sweep_leases().expect("sweep leases");
                    h.kernel.sweep_approvals().expect("sweep approvals");
                }
                81..=86 => {
                    if runs.is_empty() {
                        continue;
                    }
                    let run_id = runs[rng.gen_range(0..runs.len())].clone();
                    if let Err(e) = h.kernel.abandon(&run_id, "random", json!({"id": "u-ops"})) {
                        assert_eq!(e.code(), "run_not_abandonable");
                    }
                }
                87..=92 => {
                    if runs.is_empty() {
                        continue;
                    }
                    let run_id = runs[rng.gen_range(0..runs.len())].clone();
                    if let Err(e) = h.kernel.resume(&run_id, json!({"id": "u-ops"})) {
                        assert!(
                            matches!(
                                e.code(),
                                "run_not_parked" | "run_not_resumable" | "approval_pending"
                            ),
                            "unexpected {e}"
                        );
                    }
                }
                _ => {
                    if leases.is_empty() {
                        continue;
                    }
                    let lease_id = leases[rng.gen_range(0..leases.len())].clone();
                    if let Some(l) = h
                        .kernel
                        .with_store(|c| store::get_lease(c, &lease_id))
                        .expect("lease")
                    {
                        let r = h.kernel.post_external_event(
                            &l.run_id,
                            "note",
                            json!({"text": "hello"}),
                            json!({"type": "worker", "id": l.worker_id}),
                            &ExternalAuth::Lease(lease_id),
                        );
                        if r.is_ok() {
                            appended += 1;
                        }
                    }
                }
            }
            for run_id in &runs {
                h.check_materialised(run_id);
                let report = h.kernel.replay(run_id).expect("replay");
                assert!(report.chain_valid, "seed {seed}: chain broken");
                assert!(report.state_matches, "seed {seed}: state mismatch");
                assert!(
                    report.decision_mismatches.is_empty(),
                    "seed {seed}: {:?}",
                    report.decision_mismatches
                );
            }
        }
        assert!(!runs.is_empty());
        let _ = appended;
    }
}

#[test]
fn bundle_and_policy_rejections() {
    let h = harness();
    let stranger = KeyPair::generate(1);
    let b = bundle();
    let foreign = sign_bundle(&b, &stranger);
    assert_eq!(
        h.kernel
            .apply_bundle(b.clone(), foreign)
            .expect_err("untrusted")
            .code(),
        "bundle_signature_invalid"
    );
    let publisher_sig = h.kernel.sign_bundle(&b);
    let mut tampered = b.clone();
    tampered["description"] = json!("changed");
    assert_eq!(
        h.kernel
            .apply_bundle(tampered, publisher_sig.clone())
            .expect_err("tampered")
            .code(),
        "bundle_signature_invalid"
    );
    let again = h
        .kernel
        .apply_bundle(b.clone(), publisher_sig)
        .expect("same content");
    assert!(!again.created);
    assert_eq!(again.bundle_id, h.bundle_id);
    let mut other = b.clone();
    other["description"] = json!("a different bundle with the same version");
    let sig = h.kernel.sign_bundle(&other);
    assert_eq!(
        h.kernel
            .apply_bundle(other, sig)
            .expect_err("conflict")
            .code(),
        "bundle_version_exists"
    );
    let mut invalid = b.clone();
    invalid["version"] = json!("2.0.0");
    invalid["workflows"]["intake"]["steps"][3]["tool"] = json!("crm.write");
    let sig = h.kernel.sign_bundle(&invalid);
    let err = h.kernel.apply_bundle(invalid, sig).expect_err("invalid");
    assert_eq!(err.code(), "bundle_invalid");
    assert_eq!(err.details()["path"], "workflows.intake.steps.3.tool");
    assert_eq!(h.kernel.list_bundles().expect("list").len(), 1);
    assert_eq!(
        h.kernel.list_bundles().unwrap()[0]["workflows"],
        json!(["intake"])
    );

    let err = h
        .kernel
        .apply_policy("broken", 1, "allow when action.kind ==\n  ")
        .expect_err("syntax");
    assert_eq!(err.code(), "policy_invalid");
    assert_eq!(err.details()["line"], 2);
    assert_eq!(err.details()["column"], 3);
    let same = h
        .kernel
        .apply_policy("finance-default", 1, FINANCE_DEFAULT)
        .expect("same");
    assert!(!same.created);
    assert_eq!(
        h.kernel
            .apply_policy("finance-default", 1, "allow when true")
            .expect_err("conflict")
            .code(),
        "policy_version_exists"
    );
    h.kernel
        .apply_policy(
            "finance-default",
            2,
            &FINANCE_DEFAULT.replace("5000", "10000"),
        )
        .expect("v2");
    assert_eq!(
        h.kernel.policy_versions("finance-default").unwrap().len(),
        2
    );
    assert!(
        h.kernel.policy_source("finance-default", 2).unwrap()["source"]
            .as_str()
            .unwrap()
            .contains("10000")
    );
    assert_eq!(
        h.kernel.policy_versions("nope").expect_err("404").code(),
        "policy_not_found"
    );

    let corpus: Vec<Value> = [1000.0, 6000.0, 12000.0]
        .iter()
        .map(|amount| json!({"action": {"kind": "payment.issue", "amount": amount, "writes_to_system_of_record": true},
                             "run": {"remit": {"autonomy": "autonomous"}, "requested_by": {"manager": "u-tom"}}}))
        .collect();
    let report = h
        .kernel
        .test_policies(
            &kernos_core::kernel::PolicySelector::Stored {
                name: "finance-default".into(),
                version: 1,
            },
            &kernos_core::kernel::PolicySelector::Stored {
                name: "finance-default".into(),
                version: 2,
            },
            &corpus,
        )
        .expect("test");
    assert_eq!(report["cases"], 3);
    assert_eq!(report["flips"].as_array().unwrap().len(), 1);
    assert_eq!(report["flips"][0]["index"], 1);
    let inline = h
        .kernel
        .test_policies(
            &kernos_core::kernel::PolicySelector::Stored {
                name: "finance-default".into(),
                version: 1,
            },
            &kernos_core::kernel::PolicySelector::Inline {
                source: FINANCE_DEFAULT.replace("5000", "10000"),
            },
            &corpus,
        )
        .expect("test");
    assert_eq!(inline["flips"][0]["rule_a"], "finance-default@1#0");
    assert_eq!(inline["flips"][0]["rule_b"], "default");
    assert_eq!(inline["flips"][0]["b"], "allow");

    let actions = h.kernel.export_actions(0).expect("export");
    assert!(actions.is_empty());
    let health = h.kernel.health().expect("health");
    assert!(health.ok);
    assert_eq!(health.version, "0.1.0");
    let text = h.kernel.metrics_text().expect("metrics");
    assert!(text.contains("kernos_runs{state=\"running\"} 0"));
    let _ = Directory::empty();
}
