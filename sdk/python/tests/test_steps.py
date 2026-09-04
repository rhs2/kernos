"""Every step executor against the stub kernel and gateway."""

from __future__ import annotations

import threading
import time

import pytest

from conftest import (
    ACTION_STEP,
    CODE_STEP,
    COMPENSATION_STEP,
    EXTRACT_STEP,
    INPUT_SCHEMA,
    POST_STEP,
    CapturingProvider,
    Stub,
)
from kernos.client import GatewayClient
from kernos.providers.base import ModelResponse, ProviderError, Usage
from kernos.steps import (
    Runtime,
    execute_action,
    execute_compensation,
    execute_model,
    execute_step,
    execute_tool,
)

VALID_EXTRACT = {
    "vendor": "Northwind Dairy",
    "invoice_id": "inv-1001",
    "total": 7250.0,
    "currency": "USD",
}


def kinds(stub: Stub, run_id: str = "run_1") -> list[str]:
    return [event["kind"] for event in stub.state.events_of(run_id)]


def payloads(stub: Stub, kind: str, run_id: str = "run_1") -> list[dict]:
    return [event["payload"] for event in stub.state.events_of(run_id, kind)]


# Model steps


def test_model_step_completes_with_events_and_usage(stub: Stub) -> None:
    lease = stub.lease(EXTRACT_STEP)
    outcome = execute_model(stub.runtime(), lease)
    assert outcome.status == "completed"
    assert kinds(stub) == ["model.called", "model.responded"]
    called = payloads(stub, "model.called")[0]
    assert called["step"] == "extract"
    assert called["model"] == "claude-sonnet-5"
    assert called["tier"] == "standard" and called["effort"] == "low"
    assert called["provider"] == "mock" and called["max_tokens"] == 1024
    assert len(called["prefix_hash"]) == 64 and len(called["input_hash"]) == 64
    responded = payloads(stub, "model.responded")[0]
    assert responded["output"]["total"] == 7250.0
    assert set(responded["usage"]) == {
        "input_tokens",
        "output_tokens",
        "cache_read_tokens",
        "cache_write_tokens",
    }
    assert responded["cost_usd"] > 0
    assert responded["stop_reason"] == "end_turn" and responded["refusal"] is False
    assert responded["latency_ms"] == 1
    complete = stub.state.completes[0]
    assert complete["output"] == responded["output"]
    assert complete["usage"] == {
        "tokens": sum(responded["usage"].values()),
        "usd": responded["cost_usd"],
    }
    assert stub.state.events_of("run_1")[0]["actor"] == {"type": "worker", "id": "wrk-test"}
    event_requests = [r for r in stub.state.requests if r["path"] == "/v1/runs/run_1/events"]
    assert all(r["headers"]["x-kernos-lease"] == "lse_1" for r in event_requests)


def test_confidence_escalation_sums_usage(stub: Stub, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("KERNOS_MOCK_CONFIDENCE", "code=0.4")
    outcome = execute_model(stub.runtime(), stub.lease(CODE_STEP))
    assert outcome.status == "completed" and outcome.escalations == 1
    assert kinds(stub) == [
        "model.called",
        "model.responded",
        "step.escalated",
        "model.called",
        "model.responded",
    ]
    escalated = payloads(stub, "step.escalated")[0]
    assert escalated["step"] == "code"
    assert escalated["from_tier"] == "cheap" and escalated["to_tier"] == "standard"
    assert "0.4" in escalated["reason"]
    called = payloads(stub, "model.called")
    assert [c["tier"] for c in called] == ["cheap", "standard"]
    assert [c["model"] for c in called] == ["claude-haiku-4-5-20251001", "claude-sonnet-5"]
    responded = payloads(stub, "model.responded")
    expected_tokens = sum(sum(r["usage"].values()) for r in responded)
    expected_usd = round(sum(r["cost_usd"] for r in responded), 10)
    assert stub.state.completes[0]["usage"] == {"tokens": expected_tokens, "usd": expected_usd}
    assert stub.state.completes[0]["output"]["confidence"] == 0.4


def test_no_escalation_when_confident(stub: Stub) -> None:
    outcome = execute_model(stub.runtime(), stub.lease(CODE_STEP))
    assert outcome.status == "completed" and outcome.escalations == 0
    assert kinds(stub) == ["model.called", "model.responded"]


def test_no_escalation_at_or_beyond_target_tier(
    stub: Stub, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("KERNOS_MOCK_CONFIDENCE", "code=0.4")
    step = {**CODE_STEP, "tier": "standard"}
    outcome = execute_model(stub.runtime(), stub.lease(step))
    assert outcome.status == "completed"
    assert "step.escalated" not in kinds(stub)


def test_refusal_parks_by_default(stub: Stub, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("KERNOS_MOCK_REFUSE", "extract")
    outcome = execute_model(stub.runtime(), stub.lease(EXTRACT_STEP))
    assert outcome.status == "failed"
    assert stub.state.fails[0] == {
        "lease_id": "lse_1",
        "error": {"code": "model_refused", "message": "model refused prompt 'extract'"},
        "deterministic": True,
    }
    responded = payloads(stub, "model.responded")[0]
    assert responded["refusal"] is True and responded["stop_reason"] == "refusal"
    assert stub.state.completes == []


def test_refusal_escalates_once_then_fails(stub: Stub, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("KERNOS_MOCK_REFUSE", "extract")
    step = {**EXTRACT_STEP, "on_refusal": "escalate"}
    outcome = execute_model(stub.runtime(), stub.lease(step))
    assert outcome.status == "failed"
    assert kinds(stub) == [
        "model.called",
        "model.responded",
        "step.escalated",
        "model.called",
        "model.responded",
    ]
    escalated = payloads(stub, "step.escalated")[0]
    assert (escalated["from_tier"], escalated["to_tier"]) == ("standard", "deep")
    assert stub.state.fails[0]["error"]["code"] == "model_refused"


def test_refusal_fail_mode(stub: Stub, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("KERNOS_MOCK_REFUSE", "extract")
    step = {**EXTRACT_STEP, "on_refusal": "fail"}
    execute_model(stub.runtime(), stub.lease(step))
    assert kinds(stub) == ["model.called", "model.responded"]
    assert stub.state.fails[0]["error"]["code"] == "model_refused"
    assert stub.state.fails[0]["deterministic"] is True


def test_invalid_output_retries_once_then_fails(stub: Stub) -> None:
    step = {**EXTRACT_STEP, "id": "bad", "prompt": "bad"}
    outcome = execute_model(stub.runtime(), stub.lease(step))
    assert outcome.status == "failed"
    failure = stub.state.fails[0]
    assert failure["error"]["code"] == "output_invalid"
    assert failure["deterministic"] is True
    assert "$.vendor" in failure["error"]["message"]
    called = payloads(stub, "model.called")
    assert len(called) == 2
    assert called[0]["prefix_hash"] == called[1]["prefix_hash"]
    assert called[0]["input_hash"] != called[1]["input_hash"]
    assert stub.state.completes == []


def test_corrective_retry_recovers_and_sums_usage(stub: Stub) -> None:
    provider = CapturingProvider(
        [
            ModelResponse(output={"vendor": 1}, usage=Usage(10, 5)),
            ModelResponse(output=VALID_EXTRACT, usage=Usage(20, 7)),
        ]
    )
    outcome = execute_model(stub.runtime(provider), stub.lease(EXTRACT_STEP))
    assert outcome.status == "completed"
    assert stub.state.completes[0]["usage"]["tokens"] == 42
    first, second = provider.requests
    assert second.system == first.system
    assert second.user.startswith(first.user)
    assert "did not satisfy the required output schema" in second.user
    assert first.model == "claude-sonnet-5" and first.effort == "low"
    assert first.output_schema == EXTRACT_STEP["output_schema"]


def test_template_error_is_deterministic(stub: Stub) -> None:
    step = {**EXTRACT_STEP, "id": "broken", "prompt": "broken"}
    outcome = execute_model(stub.runtime(), stub.lease(step))
    assert outcome.status == "failed"
    assert stub.state.fails[0]["error"]["code"] == "template_missing_path"
    assert stub.state.fails[0]["deterministic"] is True
    assert kinds(stub) == []


def test_boundary_redacts_and_notes_when_not_granted(stub: Stub) -> None:
    provider = CapturingProvider([ModelResponse(output=VALID_EXTRACT, usage=Usage(1, 1))])
    step = {**EXTRACT_STEP, "data_classes": ["pii"]}
    outcome = execute_model(stub.runtime(provider), stub.lease(step), input_schema=INPUT_SCHEMA)
    assert outcome.status == "completed"
    assert kinds(stub) == ["note", "model.called", "model.responded"]
    note = payloads(stub, "note")[0]
    assert note["redacted"] == ["pii"]
    assert note["fields"] >= 3
    assert note["data"]["field_names"] == ["contact_name"]
    user = provider.requests[0].user
    assert "ana@halcyon.example" not in user and "0142" not in user
    assert "[REDACTED:contact_name]" in user and "Ana Reyes" not in user


def test_boundary_skipped_when_granted(stub: Stub) -> None:
    provider = CapturingProvider([ModelResponse(output=VALID_EXTRACT, usage=Usage(1, 1))])
    step = {**EXTRACT_STEP, "data_classes": ["pii"]}
    lease = stub.lease(
        step, remit={"autonomy": "supervised", "grants": ["pii"], "tools": [], "scopes": []}
    )
    execute_model(stub.runtime(provider), lease, input_schema=INPUT_SCHEMA)
    assert kinds(stub) == ["model.called", "model.responded"]
    assert "ana@halcyon.example" in provider.requests[0].user


def test_provider_error_keeps_its_flag(stub: Stub) -> None:
    provider = CapturingProvider([ProviderError("upstream 529", deterministic=False)])
    outcome = execute_model(stub.runtime(provider), stub.lease(EXTRACT_STEP))
    assert outcome.status == "failed"
    assert stub.state.fails[0]["error"]["code"] == "model_error"
    assert stub.state.fails[0]["deterministic"] is False
    assert kinds(stub) == ["model.called"]


def test_missing_provider_fails_non_deterministically(stub: Stub) -> None:
    runtime = Runtime(kernel=stub.kernel(), gateway=stub.gateway(), provider=None)
    outcome = execute_model(runtime, stub.lease(EXTRACT_STEP))
    assert outcome.status == "failed"
    assert stub.state.fails[0]["error"]["code"] == "provider_missing"
    assert stub.state.fails[0]["deterministic"] is False


# Tool steps


def test_tool_step_appends_called_before_the_gateway_call(stub: Stub) -> None:
    outcome = execute_tool(stub.runtime(), stub.lease(POST_STEP))
    assert outcome.status == "completed"
    assert outcome.output == {"entry_id": 1, "posted_at": "2026-09-04T12:00:00.000Z"}
    assert kinds(stub) == ["tool.called", "tool.result"]
    assert stub.state.order("event:tool", "gateway:") == [
        "event:tool.called",
        "gateway:ledger.post_entry",
        "event:tool.result",
    ]
    call = stub.state.gateway_calls[0]
    assert call["remit_token"] == "krt1.payload.signature.key_1"
    assert (call["run_id"], call["step"], call["lease_id"]) == ("run_1", "post", "lse_1")
    assert call["tool"] == "ledger.post_entry"
    assert call["args"] == {
        "invoice_id": "inv-1001",
        "vendor": "Northwind Dairy",
        "account": "5100",
        "amount": 7250.0,
    }
    assert call["idempotency_key"] == "inv-1001" and call["scope"] is None
    called = payloads(stub, "tool.called")[0]
    assert called == {
        "step": "post",
        "tool": "ledger.post_entry",
        "args": call["args"],
        "scope": None,
        "idempotency_key": "inv-1001",
    }
    result = payloads(stub, "tool.result")[0]
    assert result == {
        "step": "post",
        "tool": "ledger.post_entry",
        "ok": True,
        "result": outcome.output,
        "replayed": False,
        "latency_ms": 3,
    }
    assert stub.state.completes[0]["output"] == outcome.output


def test_tool_reuses_prior_result_by_idempotency_key(stub: Stub) -> None:
    prior = [
        {
            "seq": 5,
            "kind": "tool.called",
            "payload": {
                "step": "post",
                "tool": "ledger.post_entry",
                "args": {},
                "scope": None,
                "idempotency_key": "inv-1001",
            },
        },
        {
            "seq": 6,
            "kind": "tool.result",
            "payload": {
                "step": "post",
                "tool": "ledger.post_entry",
                "ok": True,
                "result": {"entry_id": 41},
                "replayed": False,
                "latency_ms": 2,
            },
        },
    ]
    lease = stub.lease(POST_STEP, attempt=2, prior_events=prior)
    outcome = execute_tool(stub.runtime(), lease)
    assert outcome.status == "completed" and outcome.reused is True
    assert outcome.output == {"entry_id": 41}
    assert stub.state.gateway_calls == []
    assert kinds(stub) == ["note"]
    assert stub.state.completes[0]["output"] == {"entry_id": 41}


def test_tool_ignores_prior_results_for_other_keys_or_failures(stub: Stub) -> None:
    prior = [
        {
            "seq": 5,
            "kind": "tool.called",
            "payload": {
                "step": "post",
                "tool": "ledger.post_entry",
                "idempotency_key": "inv-0999",
            },
        },
        {
            "seq": 6,
            "kind": "tool.result",
            "payload": {
                "step": "post",
                "tool": "ledger.post_entry",
                "ok": True,
                "result": {"entry_id": 9},
            },
        },
        {
            "seq": 7,
            "kind": "tool.called",
            "payload": {
                "step": "post",
                "tool": "ledger.post_entry",
                "idempotency_key": "inv-1001",
            },
        },
        {
            "seq": 8,
            "kind": "tool.result",
            "payload": {
                "step": "post",
                "tool": "ledger.post_entry",
                "ok": False,
                "result": {"error": {}},
            },
        },
    ]
    lease = stub.lease(POST_STEP, attempt=3, prior_events=prior)
    outcome = execute_tool(stub.runtime(), lease)
    assert outcome.status == "completed" and outcome.reused is False
    assert len(stub.state.gateway_calls) == 1


def test_tool_prior_call_without_result_resends_the_same_key(stub: Stub) -> None:
    prior = [
        {
            "seq": 5,
            "kind": "tool.called",
            "payload": {
                "step": "post",
                "tool": "ledger.post_entry",
                "idempotency_key": "inv-1001",
            },
        }
    ]
    stub.state.gateway_reply(
        "ledger.post_entry",
        200,
        {
            "ok": True,
            "result": {"entry_id": 1},
            "replayed": True,
            "latency_ms": 1,
        },
    )
    outcome = execute_tool(stub.runtime(), stub.lease(POST_STEP, attempt=2, prior_events=prior))
    assert outcome.status == "completed"
    assert stub.state.gateway_calls[0]["idempotency_key"] == "inv-1001"
    assert payloads(stub, "tool.result")[0]["replayed"] is True


def test_tool_refusal_is_deterministic(stub: Stub) -> None:
    stub.state.gateway_reply(
        "ledger.post_entry",
        403,
        {
            "ok": False,
            "refusal": {
                "reason": "tool_not_in_remit",
                "detail": "not matched by [ledger.lookup_vendor]",
            },
        },
    )
    outcome = execute_tool(stub.runtime(), stub.lease(POST_STEP))
    assert outcome.status == "failed"
    failure = stub.state.fails[0]
    assert failure["error"]["code"] == "tool_refused"
    assert failure["deterministic"] is True
    assert "tool_not_in_remit" in failure["error"]["message"]
    result = payloads(stub, "tool.result")[0]
    assert result["ok"] is False
    assert result["result"]["refusal"]["reason"] == "tool_not_in_remit"
    assert stub.state.completes == []


@pytest.mark.parametrize(
    ("status", "body", "code", "deterministic"),
    [
        (
            503,
            {"ok": False, "error": {"code": "connector_quarantined", "connector": "ledger"}},
            "connector_quarantined",
            False,
        ),
        (
            502,
            {"ok": False, "error": {"code": "upstream_error", "circuit": "open"}},
            "upstream_error",
            False,
        ),
        (500, {"unexpected": True}, "upstream_error", False),
        (
            422,
            {"ok": False, "error": {"code": "args_invalid", "details": {"path": "$.amount"}}},
            "args_invalid",
            True,
        ),
        (
            422,
            {"ok": False, "error": {"code": "connector_error", "deterministic": True}},
            "connector_error",
            True,
        ),
        (
            409,
            {"ok": False, "error": {"code": "idempotency_conflict"}},
            "idempotency_conflict",
            True,
        ),
    ],
)
def test_gateway_status_mapping(
    stub: Stub, status: int, body: dict, code: str, deterministic: bool
) -> None:
    stub.state.gateway_reply("ledger.post_entry", status, body)
    outcome = execute_tool(stub.runtime(), stub.lease(POST_STEP))
    assert outcome.status == "failed"
    assert stub.state.fails[0]["error"]["code"] == code
    assert stub.state.fails[0]["deterministic"] is deterministic
    assert kinds(stub) == ["tool.called", "tool.result"]
    assert payloads(stub, "tool.result")[0]["ok"] is False


def test_gateway_unreachable_is_non_deterministic(stub: Stub) -> None:
    runtime = stub.runtime(gateway_url="http://127.0.0.1:9")
    outcome = execute_tool(runtime, stub.lease(POST_STEP))
    assert outcome.status == "failed"
    assert stub.state.fails[0]["error"]["code"] == "gateway_unreachable"
    assert stub.state.fails[0]["deterministic"] is False
    assert kinds(stub) == ["tool.called", "tool.result"]
    assert payloads(stub, "tool.result")[0]["ok"] is False


def test_compensation_is_a_tool_step_with_a_derived_key(stub: Stub) -> None:
    outcome = execute_compensation(stub.runtime(), stub.lease(COMPENSATION_STEP))
    assert outcome.status == "completed"
    call = stub.state.gateway_calls[0]
    assert call["tool"] == "ledger.void_entry"
    assert call["args"] == {"entry_id": 1, "reason": "run abandoned"}
    assert call["idempotency_key"] == "compensation:run_1:comp_post"


def test_read_tool_without_key_sends_null(stub: Stub) -> None:
    step = {
        "id": "lookup",
        "kind": "tool",
        "tool": "ledger.lookup_vendor",
        "args": {"name": "{{steps.extract.output.vendor}}"},
    }
    execute_tool(stub.runtime(), stub.lease(step))
    call = stub.state.gateway_calls[0]
    assert call["idempotency_key"] is None
    assert call["args"] == {"name": "Northwind Dairy"}


# Action steps


def test_action_allow_completes_with_the_decision(stub: Stub) -> None:
    outcome = execute_action(stub.runtime(), stub.lease(ACTION_STEP))
    assert outcome.status == "completed"
    proposed = stub.state.action_calls[0]["action"]
    assert proposed == {
        "kind": "payment.issue",
        "amount": 7250.0,
        "currency": "USD",
        "writes_to_system_of_record": True,
        "target": "ledger",
        "data_classes": [],
        "paths": [],
        "idempotency_key": "inv-1001",
        "summary": "Pay invoice inv-1001 to Northwind Dairy",
    }
    assert outcome.output["decision"] == "allow" and outcome.output["rule"] == "default"
    assert outcome.output["action_id"].startswith("act_")
    assert outcome.output["approval_id"] is None
    assert stub.state.completes[0]["output"] == outcome.output


def test_action_approval_required_stops_without_complete_or_fail(stub: Stub) -> None:
    stub.state.action_decisions["payment.issue"] = (
        200,
        {"decision": "approval_required", "rule": "finance-default@1#0"},
    )
    outcome = execute_action(stub.runtime(), stub.lease(ACTION_STEP))
    assert outcome.status == "waiting_approval"
    assert outcome.output["approval_id"].startswith("apr_")
    assert stub.state.completes == [] and stub.state.fails == []


def test_action_deny_in_body_fails_deterministically(stub: Stub) -> None:
    stub.state.action_decisions["payment.issue"] = (
        200,
        {"decision": "deny", "rule": "finance-default@1#3"},
    )
    outcome = execute_action(stub.runtime(), stub.lease(ACTION_STEP))
    assert outcome.status == "failed"
    assert stub.state.fails[0]["error"]["code"] == "action_denied"
    assert stub.state.fails[0]["deterministic"] is True
    assert "finance-default@1#3" in stub.state.fails[0]["error"]["message"]


def test_action_deny_as_403_fails_deterministically(stub: Stub) -> None:
    stub.state.action_decisions["payment.issue"] = (
        403,
        {"error": {"code": "action_denied", "message": "denied by finance-default@1#3"}},
    )
    outcome = execute_action(stub.runtime(), stub.lease(ACTION_STEP))
    assert outcome.status == "failed"
    assert stub.state.fails[0]["error"] == {
        "code": "action_denied",
        "message": "denied by finance-default@1#3",
    }
    assert stub.state.fails[0]["deterministic"] is True


def test_action_approved_rule_after_resume(stub: Stub) -> None:
    stub.state.action_decisions["payment.issue"] = (
        200,
        {"decision": "allow", "rule": "approved:apr_1", "action_id": "act_1"},
    )
    lease = stub.lease(ACTION_STEP, attempt=2, approved_actions=["act_1"])
    outcome = execute_action(stub.runtime(), lease)
    assert outcome.status == "completed"
    assert outcome.output["rule"] == "approved:apr_1"


# Lease loss, timeouts, dispatch


def test_abort_flag_stops_before_any_call(stub: Stub) -> None:
    abort = threading.Event()
    abort.set()
    outcome = execute_tool(stub.runtime(), stub.lease(POST_STEP), abort=abort)
    assert outcome.status == "lease_lost"
    assert kinds(stub) == [] and stub.state.gateway_calls == []
    assert stub.state.completes == [] and stub.state.fails == []


def test_kernel_refusing_the_lease_maps_to_lease_lost(stub: Stub) -> None:
    lease = stub.lease(POST_STEP)
    stub.state.release("lse_1")
    outcome = execute_tool(stub.runtime(), lease)
    assert outcome.status == "lease_lost"
    assert stub.state.gateway_calls == []


def test_expired_deadline_fails_non_deterministically(stub: Stub) -> None:
    outcome = execute_tool(stub.runtime(), stub.lease(POST_STEP), deadline=time.monotonic() - 1)
    assert outcome.status == "failed"
    assert stub.state.fails[0]["error"]["code"] == "step_timeout"
    assert stub.state.fails[0]["deterministic"] is False


def test_execute_step_dispatches_on_kind(stub: Stub) -> None:
    assert execute_step(stub.runtime(), stub.lease(EXTRACT_STEP)).status == "completed"
    unknown = {"id": "odd", "kind": "mystery"}
    outcome = execute_step(stub.runtime(), stub.lease(unknown, lease_id="lse_2"))
    assert outcome.status == "failed"
    assert stub.state.fails[-1]["error"]["code"] == "unknown_step_kind"
    assert stub.state.fails[-1]["deterministic"] is True


def test_runtime_without_gateway_fails_tool_steps(stub: Stub) -> None:
    runtime = Runtime(kernel=stub.kernel(), gateway=None)
    outcome = execute_tool(runtime, stub.lease(POST_STEP))
    assert outcome.status == "failed"
    assert stub.state.fails[0]["error"]["code"] == "gateway_missing"
    assert isinstance(stub.gateway(), GatewayClient)
