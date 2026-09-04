"""The evaluation harness against a stub kernel that plays scripted runs."""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any

import pytest

from conftest import CapturingProvider, Stub
from kernos.eval import (
    DEFAULT_REQUESTED_BY,
    DEFAULT_SCOPES,
    GateThresholds,
    build_eval_context,
    default_remit,
    gate,
    load_golden,
    main,
    run_golden,
)
from kernos.providers.base import ModelResponse, Usage
from kernos.providers.mock import MockProvider

SET = {
    "name": "invoice_intake",
    "bundle": "halcyon.finance.invoice_intake@1.0.0",
    "workflow": "intake",
    "version": 3,
}
CASE_INPUT = {"invoice_id": "inv-1001", "text": "Invoice from Northwind Dairy", "total": 7250.0}
CASE_1 = {
    "id": "c001",
    "input": CASE_INPUT,
    "expect": {
        "steps.extract.output.vendor": "Northwind Dairy",
        "steps.code.output.account": "5100",
    },
    "assert": [
        "steps.code.output.confidence >= 0.7",
        "run.state == 'completed'",
        'steps.extract.output.vendor == "Northwind Dairy"',
    ],
    "rubric": {"step": "extract", "criteria": ["vendor is the legal name on the invoice"]},
}
CASE_2 = {**CASE_1, "id": "c002", "input": {**CASE_INPUT, "invoice_id": "inv-1002"}}
EXTRACT_OUTPUT = {
    "vendor": "Northwind Dairy",
    "invoice_id": "inv-1001",
    "total": 7250.0,
    "currency": "USD",
}


def write_golden(root: Path, cases: list[dict[str, Any]], golden_set: dict[str, Any] = SET) -> Path:
    golden = root / "golden"
    (golden / "cases").mkdir(parents=True)
    (golden / "set.json").write_text(json.dumps(golden_set), encoding="utf-8")
    for case in cases:
        (golden / "cases" / f"{case['id']}.json").write_text(json.dumps(case), encoding="utf-8")
    return golden


def completed_script(confidence: float, cost: float, state: str = "completed") -> tuple[list, dict]:
    events = [
        {"kind": "run.created"},
        {"kind": "model.called", "payload": {"step": "extract"}},
        {"kind": "model.responded", "payload": {"step": "extract", "cost_usd": cost / 2}},
        {"kind": "model.called", "payload": {"step": "code"}},
        {"kind": "model.responded", "payload": {"step": "code", "cost_usd": cost / 2}},
        {"kind": "run.completed" if state == "completed" else "run.failed", "payload": {}},
    ]
    run_state = {
        "state": state,
        "workflow": "intake",
        "steps": [
            {
                "id": "extract",
                "index": 0,
                "kind": "model",
                "state": "completed",
                "output": EXTRACT_OUTPUT,
            },
            {
                "id": "code",
                "index": 1,
                "kind": "model",
                "state": "completed",
                "output": {"account": "5100", "confidence": confidence},
            },
        ],
        "budget": {"used_usd": cost, "used_tokens": 40},
        "output": {"account": "5100", "confidence": confidence},
    }
    return events, run_state


def prepare(stub: Stub) -> None:
    stub.state.bundles = [
        {
            "bundle_id": "bnd_1",
            "name": "halcyon.finance.invoice_intake",
            "version": "1.0.0",
            "department": "finance",
            "workflows": ["intake"],
        }
    ]
    stub.state.bundle_docs["bnd_1"] = {
        "bundle_id": "bnd_1",
        "bundle": {
            "name": "halcyon.finance.invoice_intake",
            "tools": [
                {"id": "ledger.post_entry", "writes": True},
                {"id": "ledger.lookup_vendor", "writes": False},
                {"id": "http.get", "writes": False},
                {"id": "test.slow", "writes": False},
            ],
            "policies": ["finance-default"],
            "workflows": {"intake": {"input_schema": {"type": "object"}, "steps": []}},
        },
    }


def test_run_golden_scores_and_writes_the_report(stub: Stub, tmp_path: Path) -> None:
    prepare(stub)
    stub.state.script_run(*completed_script(0.93, 0.0003))
    stub.state.script_run(*completed_script(0.2, 0.0002))
    golden = write_golden(tmp_path, [CASE_1, CASE_2])
    out = tmp_path / "report.json"
    report = run_golden(golden, stub.kernel(), stub.gateway(), MockProvider(), out, poll_s=0.01)
    assert report["set"] == "invoice_intake" and report["version"] == 3
    assert (report["cases"], report["passed"], report["pass_rate"]) == (2, 1, 0.5)
    assert report["cost_usd"] == pytest.approx(0.0005)
    assert isinstance(report["p50_latency_ms"], int) and report["p50_latency_ms"] >= 0
    assert report["error_rate"] == 0.0
    assert report["failures"] == [
        {"id": "c002", "reason": "assert failed: steps.code.output.confidence >= 0.7"}
    ]
    assert report["results"][0]["graded"] is None
    assert report["results"][0]["run_id"].startswith("run_")
    assert json.loads(out.read_text(encoding="utf-8")) == report
    assert len(stub.state.remits) == 2
    remit = stub.state.remits[0]
    assert remit["tools"] == ["http.*", "ledger.*", "test.*"]
    assert remit["scopes"] == list(DEFAULT_SCOPES)
    assert remit["autonomy"] == "autonomous"
    assert remit["policy_set"] == ["finance-default"]
    start = stub.state.run_starts[0]
    assert start["bundle_id"] == "bnd_1" and start["workflow"] == "intake"
    assert start["input"] == CASE_1["input"]
    assert start["remit_id"] == remit["remit_id"]
    assert start["requested_by"] == DEFAULT_REQUESTED_BY


def test_run_golden_all_pass(stub: Stub, tmp_path: Path) -> None:
    prepare(stub)
    stub.state.script_run(*completed_script(0.93, 0.0003))
    stub.state.script_run(*completed_script(0.95, 0.0003))
    report = run_golden(write_golden(tmp_path, [CASE_1, CASE_2]), stub.kernel(), poll_s=0.01)
    assert report["pass_rate"] == 1.0 and report["failures"] == []


def test_run_golden_timeouts_and_failed_runs(stub: Stub, tmp_path: Path) -> None:
    prepare(stub)
    stub.state.script_run([{"kind": "run.created"}], {"state": "running", "steps": []})
    stub.state.script_run(*completed_script(0.9, 0.0001, state="failed"))
    report = run_golden(
        write_golden(tmp_path, [CASE_1, CASE_2]),
        stub.kernel(),
        timeout_s=0.2,
        poll_s=0.01,
    )
    first, second = report["results"]
    assert first["state"] == "timeout" and not first["passed"]
    assert second["state"] == "failed" and not second["passed"]
    assert "assert failed: run.state == 'completed'" in second["reasons"]
    assert report["error_rate"] == 1.0 and report["pass_rate"] == 0.0


def test_run_golden_derives_from_a_parent_remit(stub: Stub, tmp_path: Path) -> None:
    prepare(stub)
    stub.state.script_run(*completed_script(0.93, 0.0))
    run_golden(write_golden(tmp_path, [CASE_1]), stub.kernel(), remit_id="rem_parent", poll_s=0.01)
    assert stub.state.remits == []
    assert stub.state.derives[0]["parent_id"] == "rem_parent"
    assert stub.state.run_starts[0]["remit_id"] == stub.state.derives[0]["remit_id"]


def test_run_golden_uses_set_remit_and_requested_by(stub: Stub, tmp_path: Path) -> None:
    prepare(stub)
    stub.state.script_run(*completed_script(0.93, 0.0))
    golden_set = {
        **SET,
        "remit": {"tools": ["ledger.lookup_vendor"], "autonomy": "observe"},
        "requested_by": {"id": "u-ana", "role": "ap_clerk", "manager": "u-tom"},
    }
    run_golden(write_golden(tmp_path, [CASE_1], golden_set), stub.kernel(), poll_s=0.01)
    assert stub.state.remits[0]["tools"] == ["ledger.lookup_vendor"]
    assert stub.state.run_starts[0]["requested_by"]["id"] == "u-ana"


def test_rubric_is_graded_on_the_cheap_tier_with_a_real_provider(
    stub: Stub, tmp_path: Path
) -> None:
    prepare(stub)
    stub.state.script_run(*completed_script(0.93, 0.0))
    provider = CapturingProvider(
        [
            ModelResponse(
                output={"pass": False, "reasons": ["vendor is a trading name"]}, usage=Usage(1, 1)
            )
        ]
    )
    report = run_golden(
        write_golden(tmp_path, [CASE_1]), stub.kernel(), provider=provider, poll_s=0.01
    )
    assert report["passed"] == 0
    assert report["results"][0]["graded"] == {
        "pass": False,
        "reasons": ["vendor is a trading name"],
    }
    assert report["failures"][0]["reason"] == "rubric: vendor is a trading name"
    request = provider.requests[0]
    assert request.model == "claude-haiku-4-5-20251001" and request.effort == "low"
    assert request.output_schema is not None and "vendor is the legal name" in request.user


def test_bundle_must_be_loaded(stub: Stub, tmp_path: Path) -> None:
    with pytest.raises(ValueError):
        run_golden(write_golden(tmp_path, [CASE_1]), stub.kernel())


def test_load_golden_errors(tmp_path: Path) -> None:
    with pytest.raises(FileNotFoundError):
        load_golden(tmp_path)
    golden = tmp_path / "golden"
    (golden / "cases").mkdir(parents=True)
    (golden / "set.json").write_text(json.dumps(SET), encoding="utf-8")
    with pytest.raises(ValueError):
        load_golden(golden)
    (golden / "cases" / "bad.json").write_text(json.dumps({"id": "x"}), encoding="utf-8")
    with pytest.raises(ValueError):
        load_golden(golden)


def test_load_golden_reads_assert_alias(tmp_path: Path) -> None:
    golden_set, cases = load_golden(write_golden(tmp_path, [CASE_1]))
    assert golden_set.name == "invoice_intake"
    assert cases[0].assertions == CASE_1["assert"]
    assert cases[0].rubric is not None and cases[0].rubric.step == "extract"


def test_build_eval_context() -> None:
    _events, state = completed_script(0.5, 0.0)
    context = build_eval_context(state, {"a": 1})
    assert context["steps"]["code"]["output"]["confidence"] == 0.5
    assert context["run"]["state"] == "completed" and "steps" not in context["run"]
    assert context["input"] == {"a": 1}


def test_gate_semantics() -> None:
    base = {"set": "s", "version": 1, "pass_rate": 1.0, "cost_usd": 0.30, "error_rate": 0.0}
    assert gate(base, dict(base)).promote is True
    lower = gate(base, {**base, "pass_rate": 0.9})
    assert lower.promote is False and lower.exit_code == 1
    assert "pass rate dropped" in lower.reasons[0]
    assert gate(base, {**base, "cost_usd": 0.34}).promote is True
    costly = gate(base, {**base, "cost_usd": 0.36})
    assert costly.promote is False and "cost increased" in costly.reasons[0]
    assert gate(base, {**base, "error_rate": 0.1}).promote is False
    assert gate(base, {**base, "pass_rate": 0.9}, {"max_pass_drop": 0.2}).promote is True
    assert gate(base, {**base, "cost_usd": 0.36}, GateThresholds(max_cost_increase=0.5)).promote
    zero = {**base, "cost_usd": 0.0}
    assert gate(zero, {**zero, "cost_usd": 0.01}).promote is False
    assert gate(zero, dict(zero)).promote is True
    assert gate(base, dict(base)).comparison["thresholds"]["max_cost_increase"] == 0.15


def test_gate_cli(tmp_path: Path, capsys: pytest.CaptureFixture[str]) -> None:
    base = {"set": "s", "version": 1, "pass_rate": 1.0, "cost_usd": 0.3, "error_rate": 0.0}
    baseline = tmp_path / "base.json"
    candidate = tmp_path / "cand.json"
    baseline.write_text(json.dumps(base), encoding="utf-8")
    candidate.write_text(json.dumps({**base, "pass_rate": 0.5}), encoding="utf-8")
    assert main(["gate", "--baseline", str(baseline), "--candidate", str(baseline)]) == 0
    assert main(["gate", "--baseline", str(baseline), "--candidate", str(candidate)]) == 1
    assert "pass rate dropped" in capsys.readouterr().out
    assert (
        main(
            [
                "gate",
                "--baseline",
                str(baseline),
                "--candidate",
                str(candidate),
                "--max-pass-drop",
                "0.6",
            ]
        )
        == 0
    )
    assert (
        main(["gate", "--baseline", str(tmp_path / "missing.json"), "--candidate", str(candidate)])
        == 2
    )


def test_run_cli(stub: Stub, tmp_path: Path, capsys: pytest.CaptureFixture[str]) -> None:
    prepare(stub)
    stub.state.script_run(*completed_script(0.93, 0.0003))
    golden = write_golden(tmp_path, [CASE_1])
    out = tmp_path / "report.json"
    code = main(
        [
            "run",
            "--golden",
            str(golden),
            "--kernel",
            stub.kernel_url,
            "--gateway",
            stub.gateway_url,
            "--provider",
            "mock",
            "--out",
            str(out),
            "--timeout",
            "5",
        ]
    )
    assert code == 0
    assert json.loads(out.read_text(encoding="utf-8"))["pass_rate"] == 1.0
    assert '"pass_rate": 1.0' in capsys.readouterr().out
    assert main(["run", "--golden", str(tmp_path / "nowhere")]) == 2


ACCEPTANCE_TOOLS = ["ledger.*", "http.get", "test.*"]
ACCEPTANCE_SCOPES = [
    "sql:table:ledger_entries",
    "sql:table:vendors",
    "http:host:127.0.0.1",
    "test:*",
]


def test_default_remit_shape() -> None:
    bundle = {
        "tools": [
            {"id": "ledger.post_entry"},
            {"id": "ledger.void_entry"},
            {"id": "http.get"},
            {"id": "test.slow"},
        ],
        "policies": ["finance-default"],
    }
    remit = default_remit(bundle, DEFAULT_REQUESTED_BY)
    assert remit["tools"] == ["http.*", "ledger.*", "test.*"]
    assert remit["scopes"] == list(DEFAULT_SCOPES)
    assert {"sql:table:*", "http:host:*", "test:*"} <= set(remit["scopes"])
    assert remit["autonomy"] == "autonomous"
    assert remit["grants"] == ["pii"]
    assert remit["spend"] == {"tokens": 5_000_000, "usd": 50.0}
    assert remit["ttl_seconds"] == 3600
    assert remit["policy_set"] == ["finance-default"]
    assert remit["requested_by"] == DEFAULT_REQUESTED_BY
    assert default_remit({}, DEFAULT_REQUESTED_BY)["tools"] == []


def test_remit_overrides_reach_the_issued_remit(stub: Stub, tmp_path: Path) -> None:
    prepare(stub)
    stub.state.script_run(*completed_script(0.93, 0.0))
    run_golden(
        write_golden(tmp_path, [CASE_1]),
        stub.kernel(),
        poll_s=0.01,
        remit_overrides={
            "tools": ACCEPTANCE_TOOLS,
            "scopes": ACCEPTANCE_SCOPES,
            "autonomy": "supervised",
            "grants": None,
        },
    )
    remit = stub.state.remits[0]
    assert remit["tools"] == ACCEPTANCE_TOOLS
    assert remit["scopes"] == ACCEPTANCE_SCOPES
    assert remit["autonomy"] == "supervised"
    assert remit["grants"] == ["pii"]


def test_run_cli_remit_flags(stub: Stub, tmp_path: Path) -> None:
    prepare(stub)
    stub.state.script_run(*completed_script(0.93, 0.0))
    code = main(
        [
            "run",
            "--golden",
            str(write_golden(tmp_path, [CASE_1])),
            "--kernel",
            stub.kernel_url,
            "--gateway",
            stub.gateway_url,
            "--provider",
            "mock",
            "--tools",
            ",".join(ACCEPTANCE_TOOLS),
            "--scopes",
            ",".join(ACCEPTANCE_SCOPES),
            "--autonomy",
            "autonomous",
            "--grants",
            "pii,phi",
            "--spend-usd",
            "5",
            "--policy-set",
            "finance-default,ops-default",
        ]
    )
    assert code == 0
    remit = stub.state.remits[0]
    assert remit["tools"] == ACCEPTANCE_TOOLS
    assert remit["scopes"] == ACCEPTANCE_SCOPES
    assert remit["autonomy"] == "autonomous"
    assert remit["grants"] == ["pii", "phi"]
    assert remit["spend"] == {"tokens": 5_000_000, "usd": 5.0}
    assert remit["policy_set"] == ["finance-default", "ops-default"]
    assert stub.state.run_starts[0]["remit_id"] == remit["remit_id"]
