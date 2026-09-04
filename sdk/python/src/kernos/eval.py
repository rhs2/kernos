"""The evaluation harness (07-REASONING-SDK, Evaluation harness).

``run_golden`` executes every case of a golden set as a real run through the
kernel, so the event log is the evidence, then scores ``expect`` paths,
``assert`` expressions (the policy expression grammar over ``steps``, ``run``
and ``input``) and optional ``rubric`` criteria graded on the cheap tier.
``gate`` compares a baseline report with a candidate and decides promotion.
"""

from __future__ import annotations

import argparse
import json
import logging
import os
import statistics
import sys
import time
from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from pydantic import BaseModel, ConfigDict, Field, ValidationError

from kernos.client import GatewayClient, KernelClient, KernosError
from kernos.expr import ExprError, evaluate, is_truthy
from kernos.prompting import build_prefix
from kernos.providers import ProviderUnavailable, get_provider
from kernos.providers.base import ModelProvider, ModelRequest, ProviderError
from kernos.router import ModelRouter
from kernos.templating import TemplateError, lookup

__all__ = [
    "AUTONOMY_LEVELS",
    "DEFAULT_SCOPES",
    "EvalReport",
    "GateResult",
    "GateThresholds",
    "GoldenCase",
    "GoldenSet",
    "default_remit",
    "gate",
    "load_golden",
    "main",
    "run_golden",
]

logger = logging.getLogger("kernos.eval")

EXIT_PROMOTE = 0
EXIT_ROLLBACK = 1
EXIT_CONFIG = 2
DEFAULT_REQUESTED_BY: dict[str, str] = {
    "id": "u-eval",
    "role": "evaluator",
    "manager": "u-eval-manager",
}
_RUBRIC_SCHEMA: dict[str, Any] = {
    "type": "object",
    "required": ["pass", "reasons"],
    "properties": {
        "pass": {"type": "boolean"},
        "reasons": {"type": "array", "items": {"type": "string"}},
    },
}
_RUBRIC_SYSTEM = (
    "You grade the output of one automated workflow step against written criteria. "
    "Answer only with the JSON the schema asks for: pass is true only when every "
    "criterion holds, and reasons lists one sentence per criterion."
)


class Rubric(BaseModel):
    """Model-graded criteria for one step's output."""

    step: str
    criteria: list[str]


class GoldenCase(BaseModel):
    """One ``cases/*.json`` file."""

    model_config = ConfigDict(populate_by_name=True)

    id: str
    input: dict[str, Any]
    expect: dict[str, Any] = Field(default_factory=dict)
    assertions: list[str] = Field(default_factory=list, alias="assert")
    rubric: Rubric | None = None


class GoldenSet(BaseModel):
    """``set.json``: which bundle and workflow the cases run against.

    ``remit`` (a ``POST /v1/remits`` body) and ``requested_by`` are optional
    extensions; without them the harness derives a remit from the bundle.
    """

    model_config = ConfigDict(extra="allow")

    name: str
    bundle: str
    workflow: str
    version: int = 1
    remit: dict[str, Any] | None = None
    requested_by: dict[str, Any] | None = None
    timeout_s: float = 120.0


class CaseResult(BaseModel):
    """Per-case detail kept in the report."""

    id: str
    run_id: str | None = None
    state: str = "unknown"
    passed: bool = False
    reasons: list[str] = Field(default_factory=list)
    graded: dict[str, Any] | None = None
    cost_usd: float = 0.0
    latency_ms: int = 0


class EvalReport(BaseModel):
    """The report of 07-REASONING-SDK plus ``error_rate`` and per-case results."""

    set: str
    version: int
    cases: int
    passed: int
    pass_rate: float
    cost_usd: float
    p50_latency_ms: int
    error_rate: float = 0.0
    failures: list[dict[str, str]] = Field(default_factory=list)
    results: list[CaseResult] = Field(default_factory=list)


@dataclass(frozen=True)
class GateThresholds:
    """Limits the candidate may not exceed relative to the baseline."""

    max_pass_drop: float = 0.0
    max_cost_increase: float = 0.15
    max_error_increase: float = 0.0


@dataclass
class GateResult:
    """The gate decision with the comparison that produced it."""

    promote: bool
    reasons: list[str]
    comparison: dict[str, Any]

    @property
    def exit_code(self) -> int:
        """0 to promote, 1 to roll back."""
        return EXIT_PROMOTE if self.promote else EXIT_ROLLBACK


# Loading


def load_golden(golden_dir: str | Path) -> tuple[GoldenSet, list[GoldenCase]]:
    """Read ``set.json`` and every ``cases/*.json`` (sorted by file name)."""
    root = Path(golden_dir)
    set_path = root / "set.json"
    if not set_path.is_file():
        raise FileNotFoundError(f"{set_path} not found")
    golden = GoldenSet.model_validate_json(set_path.read_text(encoding="utf-8"))
    cases: list[GoldenCase] = []
    for path in sorted((root / "cases").glob("*.json")):
        try:
            cases.append(GoldenCase.model_validate_json(path.read_text(encoding="utf-8")))
        except ValidationError as exc:
            raise ValueError(f"{path}: {exc}") from exc
    if not cases:
        raise ValueError(f"no cases under {root / 'cases'}")
    return golden, cases


def resolve_bundle(kernel: KernelClient, reference: str) -> dict[str, Any]:
    """Find a bundle by ``name@version`` or id; returns the listing entry."""
    if reference.startswith("bnd_"):
        document = kernel.get_bundle(reference)
        bundle = document.get("bundle", document)
        return {"bundle_id": reference, **bundle}
    name, _, version = reference.partition("@")
    for entry in kernel.list_bundles():
        if entry.get("name") == name and (not version or entry.get("version") == version):
            return entry
    raise ValueError(f"bundle {reference!r} is not loaded in the kernel")


def _bundle_document(kernel: KernelClient, bundle_id: str) -> dict[str, Any]:
    document = kernel.get_bundle(bundle_id)
    return dict(document.get("bundle", document))


DEFAULT_SCOPES: tuple[str, ...] = ("sql:table:*", "http:host:*", "fs:path:*", "test:*", "mcp:*")
"""Scope patterns of the default evaluation remit: every scope of every built-in connector."""
DEFAULT_SPEND: dict[str, float] = {"tokens": 5_000_000, "usd": 50.0}
AUTONOMY_LEVELS: tuple[str, ...] = ("observe", "propose", "supervised", "autonomous")


def default_remit(bundle: Mapping[str, Any], requested_by: Mapping[str, Any]) -> dict[str, Any]:
    """A permissive remit for evaluation runs.

    Tools are ``<connector>.*`` for every connector the bundle declares a tool
    on (``ledger.*``, ``http.*``, ``test.*`` for the reference bundle), the
    scopes are :data:`DEFAULT_SCOPES`, autonomy is ``autonomous`` so no approval
    stalls a case, the grant is ``pii``, the spend is generous and the policy
    set is the bundle's. Any field can be replaced through ``remit_overrides``
    or the matching ``kernos-eval run`` flags.
    """
    connectors = sorted(
        {str(tool["id"]).split(".", 1)[0] for tool in bundle.get("tools") or [] if tool.get("id")}
    )
    return {
        "tools": [f"{connector}.*" for connector in connectors],
        "scopes": list(DEFAULT_SCOPES),
        "grants": ["pii"],
        "spend": dict(DEFAULT_SPEND),
        "autonomy": "autonomous",
        "ttl_seconds": 3600,
        "policy_set": list(bundle.get("policies") or []),
        "requested_by": dict(requested_by),
    }


# Scoring


def build_eval_context(
    run_state: Mapping[str, Any], case_input: Mapping[str, Any]
) -> dict[str, Any]:
    """``{steps, run, input}`` for ``expect`` paths and ``assert`` expressions."""
    steps = {str(step.get("id")): dict(step) for step in run_state.get("steps") or []}
    run = {key: value for key, value in run_state.items() if key != "steps"}
    return {"steps": steps, "run": run, "input": dict(case_input)}


def _score_expectations(case: GoldenCase, context: Mapping[str, Any]) -> list[str]:
    reasons: list[str] = []
    for path, expected in case.expect.items():
        try:
            actual = lookup(context, path)
        except TemplateError:
            reasons.append(f"expect {path}: path missing")
            continue
        if actual != expected:
            reasons.append(f"expect {path}: got {actual!r}, wanted {expected!r}")
    return reasons


def _score_assertions(case: GoldenCase, context: Mapping[str, Any]) -> list[str]:
    reasons: list[str] = []
    for expression in case.assertions:
        try:
            if not is_truthy(evaluate(expression, context)):
                reasons.append(f"assert failed: {expression}")
        except ExprError as exc:
            reasons.append(f"assert invalid: {expression} ({exc})")
    return reasons


def _grade_rubric(
    provider: ModelProvider | None,
    router: ModelRouter,
    case: GoldenCase,
    context: Mapping[str, Any],
) -> tuple[dict[str, Any] | None, list[str]]:
    if case.rubric is None or provider is None or provider.name == "mock":
        return None, []
    output = (context["steps"].get(case.rubric.step) or {}).get("output")
    criteria = "\n".join(f"- {item}" for item in case.rubric.criteria)
    user = f"Criteria:\n{criteria}\n\nStep output:\n{json.dumps(output, ensure_ascii=False)}"
    request = ModelRequest(
        system=build_prefix(_RUBRIC_SYSTEM, [], _RUBRIC_SCHEMA),
        user=user,
        model=router.model_for("cheap"),
        output_schema=_RUBRIC_SCHEMA,
        max_tokens=512,
        effort="low",
    )
    try:
        response = provider.generate(request)
    except ProviderError as exc:
        return {"pass": False, "reasons": [str(exc)]}, [f"rubric grading failed: {exc}"]
    graded: dict[str, Any] = (
        dict(response.output) if isinstance(response.output, dict) else {"pass": False}
    )
    listed = graded.get("reasons") or []
    notes = "; ".join(str(item) for item in listed) if isinstance(listed, list) else str(listed)
    reasons = [] if graded.get("pass") is True else [f"rubric: {notes}"]
    return graded, reasons


def _run_cost(run_state: Mapping[str, Any], events: Sequence[Mapping[str, Any]]) -> float:
    used = (run_state.get("budget") or {}).get("used_usd")
    if isinstance(used, (int, float)) and not isinstance(used, bool):
        return float(used)
    total = 0.0
    for event in events:
        if event.get("kind") == "model.responded":
            total += float((event.get("payload") or {}).get("cost_usd", 0) or 0)
    return total


@dataclass
class _Session:
    kernel: KernelClient
    provider: ModelProvider | None
    router: ModelRouter
    golden: GoldenSet
    bundle_id: str
    bundle: dict[str, Any]
    requested_by: dict[str, Any]
    remit_id: str | None
    remit_spec: dict[str, Any] | None
    remit_overrides: dict[str, Any]
    timeout_s: float
    poll_s: float

    def remit_for_case(self) -> str:
        """One fresh remit per run: a remit binds to a single run."""
        if self.remit_id:
            return str(self.kernel.derive_remit(self.remit_id, {})["remit_id"])
        spec = dict(self.remit_spec or default_remit(self.bundle, self.requested_by))
        spec.update(self.remit_overrides)
        return str(self.kernel.issue_remit(spec)["remit_id"])


def _run_case(session: _Session, case: GoldenCase) -> CaseResult:
    result = CaseResult(id=case.id)
    started = time.monotonic()
    try:
        remit_id = session.remit_for_case()
        run = session.kernel.start_run(
            session.bundle_id, session.golden.workflow, case.input, remit_id, session.requested_by
        )
        result.run_id = str(run["run_id"])
        events = list(
            session.kernel.follow(result.run_id, poll_s=session.poll_s, timeout_s=session.timeout_s)
        )
        run_state = session.kernel.get_run(result.run_id)
    except KernosError as exc:
        result.state = "error"
        result.reasons.append(f"kernel error: {exc}")
        return result
    except TimeoutError as exc:
        result.state = "timeout"
        result.reasons.append(str(exc))
        return result
    finally:
        result.latency_ms = int((time.monotonic() - started) * 1000)
    result.state = str(run_state.get("state", "unknown"))
    result.cost_usd = _run_cost(run_state, events)
    context = build_eval_context(run_state, case.input)
    result.reasons.extend(_score_expectations(case, context))
    result.reasons.extend(_score_assertions(case, context))
    graded, rubric_reasons = _grade_rubric(session.provider, session.router, case, context)
    result.graded = graded
    result.reasons.extend(rubric_reasons)
    result.passed = not result.reasons
    return result


def _summarise(golden: GoldenSet, results: list[CaseResult]) -> EvalReport:
    passed = sum(1 for item in results if item.passed)
    errors = sum(1 for item in results if item.state in ("failed", "abandoned", "timeout", "error"))
    latencies = [item.latency_ms for item in results] or [0]
    return EvalReport(
        set=golden.name,
        version=golden.version,
        cases=len(results),
        passed=passed,
        pass_rate=round(passed / len(results), 4) if results else 0.0,
        cost_usd=round(sum(item.cost_usd for item in results), 8),
        p50_latency_ms=int(statistics.median(latencies)),
        error_rate=round(errors / len(results), 4) if results else 0.0,
        failures=[
            {"id": item.id, "reason": "; ".join(item.reasons)}
            for item in results
            if not item.passed
        ],
        results=results,
    )


def run_golden(
    golden_dir: str | Path,
    kernel: KernelClient,
    gateway: GatewayClient | None = None,
    provider: ModelProvider | None = None,
    out: str | Path | None = None,
    *,
    remit_id: str | None = None,
    remit: Mapping[str, Any] | None = None,
    remit_overrides: Mapping[str, Any] | None = None,
    requested_by: Mapping[str, Any] | None = None,
    timeout_s: float | None = None,
    poll_s: float = 0.5,
    router: ModelRouter | None = None,
) -> dict[str, Any]:
    """Run every case of the golden set through the kernel and score it.

    ``gateway`` is only checked for health (the kernel and workers do the tool
    calls); ``provider`` grades rubrics on the cheap tier and is skipped for the
    mock (``graded: null``). ``remit_id`` names a parent remit to derive one
    child per case from; ``remit`` is a full remit body to issue per case; with
    neither, :func:`default_remit` builds one from the bundle. ``remit_overrides``
    replaces individual fields (``tools``, ``scopes``, ``grants``, ``autonomy``,
    ``spend``, ``policy_set``) of whichever remit body is issued. The report
    dictionary is returned and, when ``out`` is given, written there as JSON.
    """
    golden, cases = load_golden(golden_dir)
    if gateway is not None:
        gateway.health()
    listing = resolve_bundle(kernel, golden.bundle)
    bundle_id = str(listing["bundle_id"])
    bundle = _bundle_document(kernel, bundle_id)
    session = _Session(
        kernel=kernel,
        provider=provider,
        router=router or ModelRouter(),
        golden=golden,
        bundle_id=bundle_id,
        bundle=bundle,
        requested_by=dict(requested_by or golden.requested_by or DEFAULT_REQUESTED_BY),
        remit_id=remit_id,
        remit_spec=dict(remit) if remit else golden.remit,
        remit_overrides={k: v for k, v in (remit_overrides or {}).items() if v is not None},
        timeout_s=timeout_s if timeout_s is not None else golden.timeout_s,
        poll_s=poll_s,
    )
    results = []
    for case in cases:
        result = _run_case(session, case)
        logger.info("case %s %s (%s)", case.id, "pass" if result.passed else "fail", result.state)
        results.append(result)
    report = _summarise(golden, results).model_dump()
    if out is not None:
        Path(out).write_text(json.dumps(report, indent=2, ensure_ascii=False), encoding="utf-8")
    return report


# The gate


def _load_report(report: Mapping[str, Any] | str | Path) -> dict[str, Any]:
    if isinstance(report, Mapping):
        return dict(report)
    return dict(json.loads(Path(report).read_text(encoding="utf-8")))


def _thresholds(thresholds: GateThresholds | Mapping[str, Any] | None) -> GateThresholds:
    if thresholds is None:
        return GateThresholds()
    if isinstance(thresholds, GateThresholds):
        return thresholds
    return GateThresholds(**{k: float(v) for k, v in thresholds.items()})


def gate(
    baseline: Mapping[str, Any] | str | Path,
    candidate: Mapping[str, Any] | str | Path,
    thresholds: GateThresholds | Mapping[str, Any] | None = None,
) -> GateResult:
    """Decide whether ``candidate`` may replace ``baseline``.

    The candidate is rolled back when its pass rate drops by more than
    ``max_pass_drop``, its cost rises by more than ``max_cost_increase``
    (relative to the baseline cost) or its error rate rises by more than
    ``max_error_increase``. Identical reports promote.
    """
    base, cand = _load_report(baseline), _load_report(candidate)
    limits = _thresholds(thresholds)
    epsilon = 1e-9
    base_cost = float(base.get("cost_usd", 0.0))
    cand_cost = float(cand.get("cost_usd", 0.0))
    if base_cost > 0:
        cost_increase = (cand_cost - base_cost) / base_cost
    else:
        cost_increase = 0.0 if cand_cost <= base_cost + epsilon else float("inf")
    pass_drop = float(base.get("pass_rate", 0.0)) - float(cand.get("pass_rate", 0.0))
    error_increase = float(cand.get("error_rate", 0.0)) - float(base.get("error_rate", 0.0))
    reasons: list[str] = []
    if pass_drop > limits.max_pass_drop + epsilon:
        reasons.append(f"pass rate dropped by {pass_drop:.4f} (limit {limits.max_pass_drop})")
    if cost_increase > limits.max_cost_increase + epsilon:
        reasons.append(
            f"cost increased by {cost_increase:.2%} (limit {limits.max_cost_increase:.0%})"
        )
    if error_increase > limits.max_error_increase + epsilon:
        reasons.append(
            f"error rate increased by {error_increase:.4f} (limit {limits.max_error_increase})"
        )
    comparison = {
        "baseline": {
            k: base.get(k) for k in ("set", "version", "pass_rate", "cost_usd", "error_rate")
        },
        "candidate": {
            k: cand.get(k) for k in ("set", "version", "pass_rate", "cost_usd", "error_rate")
        },
        "pass_drop": round(pass_drop, 6),
        "cost_increase": cost_increase,
        "error_increase": round(error_increase, 6),
        "thresholds": limits.__dict__,
    }
    return GateResult(promote=not reasons, reasons=reasons, comparison=comparison)


# Command line


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="kernos-eval", description="Kernos evaluation harness.")
    commands = parser.add_subparsers(dest="command", required=True)
    run = commands.add_parser("run", help="run a golden set through the kernel")
    run.add_argument("--golden", required=True)
    run.add_argument("--kernel", default=None, help="kernel url (env KERNOS_KERNEL_URL)")
    run.add_argument("--gateway", default=None, help="gateway url (env KERNOS_GATEWAY_URL)")
    run.add_argument("--provider", choices=["mock", "anthropic"], default=None)
    run.add_argument("--out", default=None)
    run.add_argument("--token", default=None, help="bearer token (env KERNOS_TOKEN)")
    run.add_argument("--remit", default=None, help="parent remit id to derive one child per case")
    run.add_argument("--remit-json", default=None, help="file with a remit body to issue per case")
    run.add_argument("--tools", default=None, help="comma separated tool patterns for the remit")
    run.add_argument("--scopes", default=None, help="comma separated scope patterns for the remit")
    run.add_argument("--grants", default=None, help="comma separated data-class grants")
    run.add_argument("--autonomy", choices=list(AUTONOMY_LEVELS), default=None)
    run.add_argument("--spend-usd", type=float, default=None, help="usd ceiling of the remit")
    run.add_argument("--spend-tokens", type=int, default=None, help="token ceiling of the remit")
    run.add_argument("--policy-set", default=None, help="comma separated policy names")
    run.add_argument("--timeout", type=float, default=None, help="seconds to wait per run")
    run.add_argument("--log-format", choices=["text", "json"], default=None)
    gate_cmd = commands.add_parser("gate", help="compare two reports")
    gate_cmd.add_argument("--baseline", required=True)
    gate_cmd.add_argument("--candidate", required=True)
    gate_cmd.add_argument("--max-pass-drop", type=float, default=0.0)
    gate_cmd.add_argument("--max-cost-increase", type=float, default=0.15)
    gate_cmd.add_argument("--max-error-increase", type=float, default=0.0)
    return parser


def _csv(value: str | None) -> list[str] | None:
    if value is None:
        return None
    return [item.strip() for item in value.split(",") if item.strip()]


def _remit_overrides(args: argparse.Namespace) -> dict[str, Any]:
    """Remit fields the ``run`` flags replace on the issued remit."""
    overrides: dict[str, Any] = {
        "tools": _csv(args.tools),
        "scopes": _csv(args.scopes),
        "grants": _csv(args.grants),
        "autonomy": args.autonomy,
        "policy_set": _csv(args.policy_set),
    }
    if args.spend_usd is not None or args.spend_tokens is not None:
        spend = dict(DEFAULT_SPEND)
        if args.spend_usd is not None:
            spend["usd"] = args.spend_usd
        if args.spend_tokens is not None:
            spend["tokens"] = args.spend_tokens
        overrides["spend"] = spend
    return {key: value for key, value in overrides.items() if value is not None}


def _command_run(args: argparse.Namespace) -> int:
    from kernos._logging import configure_logging

    env = os.environ
    configure_logging(args.log_format or env.get("KERNOS_LOG", "text"))
    kernel_url = args.kernel or env.get("KERNOS_KERNEL_URL", "http://127.0.0.1:7401")
    gateway_url = args.gateway or env.get("KERNOS_GATEWAY_URL", "http://127.0.0.1:7402")
    token = args.token or env.get("KERNOS_TOKEN")
    provider_name = args.provider or env.get("KERNOS_PROVIDER", "mock")
    try:
        remit = (
            json.loads(Path(args.remit_json).read_text(encoding="utf-8"))
            if args.remit_json
            else None
        )
        provider = get_provider(provider_name)
        kernel = KernelClient(kernel_url, token=token)
        gateway = GatewayClient(gateway_url, token=token)
        report = run_golden(
            args.golden,
            kernel,
            gateway,
            provider,
            args.out,
            remit_id=args.remit,
            remit=remit,
            remit_overrides=_remit_overrides(args),
            timeout_s=args.timeout,
        )
    except (OSError, ValueError, ValidationError, ProviderUnavailable, KernosError) as exc:
        print(f"kernos-eval: {exc}", file=sys.stderr)
        return EXIT_CONFIG
    summary = {k: v for k, v in report.items() if k != "results"}
    print(json.dumps(summary, indent=2, ensure_ascii=False))
    return EXIT_PROMOTE


def _command_gate(args: argparse.Namespace) -> int:
    try:
        result = gate(
            args.baseline,
            args.candidate,
            GateThresholds(args.max_pass_drop, args.max_cost_increase, args.max_error_increase),
        )
    except (OSError, ValueError) as exc:
        print(f"kernos-eval: {exc}", file=sys.stderr)
        return EXIT_CONFIG
    print(
        json.dumps(
            {"promote": result.promote, "reasons": result.reasons, **result.comparison},
            indent=2,
            ensure_ascii=False,
        )
    )
    return result.exit_code


def main(argv: Sequence[str] | None = None) -> int:
    """Entry point of ``kernos-eval``: ``run`` and ``gate`` subcommands."""
    args = _parser().parse_args(argv)
    if args.command == "run":
        return _command_run(args)
    return _command_gate(args)


if __name__ == "__main__":  # pragma: no cover
    sys.exit(main())
