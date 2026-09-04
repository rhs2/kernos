"""KernelClient and GatewayClient: paths, headers, error mapping, follow."""

from __future__ import annotations

import httpx
import pytest

from conftest import POST_STEP, Stub, lease_dict
from kernos.client import GatewayClient, KernelClient, KernosError, KernosNetworkError

ACTOR = {"type": "worker", "id": "wrk-a1"}


def test_kernel_error_shape(stub: Stub) -> None:
    with pytest.raises(KernosError) as info:
        stub.kernel().get_bundle("bnd_missing")
    error = info.value
    assert (error.status, error.code, error.message) == (404, "not_found", "no such bundle")
    assert error.details == {}
    assert str(error) == "404 not_found: no such bundle"


def test_gateway_refusal_maps_reason_to_code(stub: Stub) -> None:
    stub.state.gateway_reply(
        "ledger.post_entry",
        403,
        {"ok": False, "refusal": {"reason": "tool_not_in_remit", "detail": "not matched"}},
    )
    with pytest.raises(KernosError) as info:
        stub.gateway().call_tool(
            remit_token="krt1.x.y.z",
            run_id="run_1",
            step="post",
            lease_id="lse_1",
            tool="ledger.post_entry",
            args={},
            idempotency_key="inv-1001",
        )
    assert info.value.status == 403
    assert info.value.code == "tool_not_in_remit"
    assert info.value.message == "not matched"
    assert info.value.details["reason"] == "tool_not_in_remit"


def test_gateway_error_keeps_extra_fields(stub: Stub) -> None:
    stub.state.gateway_reply(
        "ledger.post_entry",
        503,
        {"ok": False, "error": {"code": "connector_quarantined", "connector": "ledger"}},
    )
    with pytest.raises(KernosError) as info:
        stub.gateway().call_tool(
            remit_token="t",
            run_id="r",
            step="s",
            lease_id="l",
            tool="ledger.post_entry",
            args={},
            idempotency_key=None,
        )
    assert info.value.code == "connector_quarantined"
    assert info.value.details["connector"] == "ledger"


def test_non_json_error_body() -> None:
    response = httpx.Response(500, text="boom")
    error = KernosError.from_response(response)
    assert (error.status, error.code, error.message) == (500, "http_500", "boom")


def test_connection_failure_is_a_network_error() -> None:
    client = KernelClient("http://127.0.0.1:9", timeout=1.0)
    with pytest.raises(KernosNetworkError) as info:
        client.health()
    assert info.value.status == 0
    assert info.value.code == "network_error"
    assert isinstance(info.value, KernosError)


def test_timeout_is_a_network_error_with_its_own_code(stub: Stub) -> None:
    stub.state.gateway_delay_s = 0.5
    client = GatewayClient(stub.gateway_url, timeout=0.1)
    with pytest.raises(KernosNetworkError) as info:
        client.call_tool(
            remit_token="t",
            run_id="r",
            step="s",
            lease_id="l",
            tool="ledger.lookup_vendor",
            args={},
            idempotency_key=None,
        )
    assert info.value.code == "timeout"


def test_bearer_token_and_lease_header(stub: Stub) -> None:
    stub.state.add_lease(lease_dict(POST_STEP))
    kernel = stub.kernel(token="secret")
    result = kernel.post_event("run_1", "note", {"text": "hi"}, ACTOR, lease_id="lse_1")
    assert result["seq"] == 1 and len(result["hash"]) == 64
    request = stub.state.requests[-1]
    assert request["headers"]["authorization"] == "Bearer secret"
    assert request["headers"]["x-kernos-lease"] == "lse_1"
    assert request["body"] == {"kind": "note", "payload": {"text": "hi"}, "actor": ACTOR}


def test_remit_header_for_gateway_appends(stub: Stub) -> None:
    stub.kernel().post_event(
        "run_1",
        "tool.refused",
        {"step": "post"},
        {"type": "gateway", "id": "gw"},
        remit_token="krt1.a.b.c",
    )
    assert stub.state.requests[-1]["headers"]["x-kernos-remit"] == "krt1.a.b.c"


def test_event_without_lease_is_refused(stub: Stub) -> None:
    with pytest.raises(KernosError) as info:
        stub.kernel().post_event("run_1", "note", {}, ACTOR)
    assert info.value.code == "event_not_permitted"


def test_lease_returns_none_on_204(stub: Stub) -> None:
    kernel = stub.kernel()
    assert kernel.lease("wrk-a1", ["tool", "model"], 30) is None
    assert stub.state.lease_requests[-1] == {
        "worker_id": "wrk-a1",
        "kinds": ["tool", "model"],
        "ttl_seconds": 30,
    }
    stub.state.add_lease(lease_dict(POST_STEP))
    leased = kernel.lease("wrk-a1", ["tool"], 30)
    assert leased is not None and leased["lease_id"] == "lse_1"


def test_follow_stops_at_the_terminal_event(stub: Stub) -> None:
    stub.state.script_run(
        [
            {"kind": "run.created"},
            {"kind": "note", "payload": {"text": "a"}},
            {"kind": "run.completed", "payload": {"output": {}}},
            {"kind": "note", "payload": {"text": "after"}},
        ],
        {"state": "completed"},
    )
    kernel = stub.kernel()
    run = kernel.start_run(
        "bnd_1", "intake", {"a": 1}, "rem_1", {"id": "u", "role": "r", "manager": "m"}
    )
    kinds = [event["kind"] for event in kernel.follow(run["run_id"], poll_s=0.01)]
    assert kinds == ["run.created", "note", "run.completed"]


def test_follow_times_out(stub: Stub) -> None:
    stub.state.script_run([{"kind": "run.created"}], {"state": "running"})
    kernel = stub.kernel()
    run = kernel.start_run("bnd_1", "intake", {}, "rem_1", {"id": "u", "role": "r", "manager": "m"})
    with pytest.raises(TimeoutError):
        list(kernel.follow(run["run_id"], poll_s=0.01, timeout_s=0.1))


def test_invalid_base_url() -> None:
    with pytest.raises(ValueError):
        KernelClient("localhost:7401")


def test_kernel_endpoints_round_trip(stub: Stub) -> None:
    kernel = stub.kernel()
    assert kernel.health()["ok"] is True
    remit = kernel.issue_remit({"tools": ["ledger.*"], "autonomy": "supervised"})
    assert remit["token"].startswith("krt1.")
    child = kernel.derive_remit(remit["remit_id"], {"autonomy": "propose"})
    assert child["parent_id"] == remit["remit_id"]
    run = kernel.start_run(
        "bnd_1", "intake", {"x": 1}, remit["remit_id"], {"id": "u", "role": "r", "manager": "m"}
    )
    run_id = run["run_id"]
    assert kernel.get_run(run_id)["run_id"] == run_id
    assert kernel.get_events(run_id, from_seq=2)["events"][0]["seq"] == 2
    assert kernel.replay(run_id)["chain_valid"] is True
    assert kernel.abandon_run(run_id, "test", ACTOR)["compensations_scheduled"] == 0
    assert kernel.resume_run(run_id, ACTOR)["state"] == "running"
    stub.state.bundles = [{"bundle_id": "bnd_1", "name": "n", "version": "1.0.0"}]
    stub.state.bundle_docs["bnd_1"] = {"bundle_id": "bnd_1", "bundle": {"name": "n"}}
    assert kernel.list_bundles()[0]["bundle_id"] == "bnd_1"
    assert kernel.get_bundle("bnd_1")["bundle"]["name"] == "n"
    stub.state.approvals = [{"approval_id": "apr_1"}]
    assert kernel.list_approvals(state="pending")[0]["approval_id"] == "apr_1"
    assert stub.state.requests[-1]["path"] == "/v1/approvals"
    assert kernel.decide_approval(
        "apr_1", "approved", {"id": "u-tom", "role": "finance_admin"}, "ok"
    )["run_state"]
    assert stub.state.decisions[-1]["reason"] == "ok"
    stub.state.add_lease(lease_dict(POST_STEP))
    assert kernel.heartbeat("lse_1")["expires_at"]
    assert kernel.propose_action("lse_1", {"kind": "payment.issue"})["decision"] == "allow"
    assert kernel.complete("lse_1", {"a": 1}, {"tokens": 3, "usd": 0.1})["run_state"] == "running"
    assert kernel.fail("lse_1", "x", "y", True)["outcome"] == "quarantined"
    assert stub.state.fails[-1] == {
        "lease_id": "lse_1",
        "error": {"code": "x", "message": "y"},
        "deterministic": True,
    }
    stub.state.release("lse_1")
    with pytest.raises(KernosError) as info:
        kernel.heartbeat("lse_1")
    assert info.value.status == 410 and info.value.code == "lease_expired"


def test_gateway_endpoints_round_trip(stub: Stub) -> None:
    with stub.gateway() as gateway:
        assert gateway.health()["ok"] is True
        assert gateway.tools()[0]["id"] == "ledger.post_entry"
        assert gateway.canaries()[0]["connector"] == "ledger"
        assert gateway.probe("ledger")["connector"] == "ledger"
        assert gateway.release("ledger")["status"] == "healthy"
        result = gateway.call_tool(
            remit_token="t",
            run_id="run_1",
            step="post",
            lease_id="lse_1",
            tool="ledger.post_entry",
            args={"a": 1},
            idempotency_key="k",
            scope="sql:table:x",
        )
    assert result["ok"] is True
    assert stub.state.gateway_calls[-1]["scope"] == "sql:table:x"
    assert stub.state.gateway_calls[-1]["idempotency_key"] == "k"
