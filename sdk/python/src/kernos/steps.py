"""Step execution (07-REASONING-SDK, Step execution).

Every executor takes the shared :class:`Runtime`, one :class:`Lease`, and calls
``complete`` or ``fail`` on the kernel itself, exactly as the specification
sequences it. Each returns a :class:`StepOutcome` describing what happened.

Every executor is safe to re-run from the start after a lost lease: the tool
executor reads ``context.prior_events`` first and reuses a result recorded under
the same idempotency key, and the kernel guarantees that a stale lease can no
longer append events, complete or fail.
"""

from __future__ import annotations

import logging
import threading
import time
from collections.abc import Callable, Mapping
from dataclasses import dataclass, field
from typing import Any

from pydantic import BaseModel, ConfigDict, Field

from kernos import boundary, pricing
from kernos._logging import StepLogger, step_logger
from kernos.client import GatewayClient, KernelClient, KernosError, KernosNetworkError
from kernos.prompting import build_prefix, input_hash, prefix_hash
from kernos.providers.base import ModelProvider, ModelRequest, ProviderError, Usage
from kernos.router import ModelRouter
from kernos.schema import SchemaError, check
from kernos.templating import TemplateError, render, render_value, template_context

__all__ = [
    "STEP_KINDS",
    "Lease",
    "LeaseLost",
    "Runtime",
    "StepFailure",
    "StepOutcome",
    "StepTimeout",
    "execute_action",
    "execute_compensation",
    "execute_model",
    "execute_step",
    "execute_tool",
]

logger = logging.getLogger("kernos.steps")

STEP_KINDS: tuple[str, ...] = ("model", "tool", "action", "compensation")
_LEASE_GONE_CODES = frozenset({"lease_expired", "event_not_permitted", "lease_not_found"})
_CORRECTIVE_MESSAGE = (
    "Your previous answer did not satisfy the required output schema ({problem}). "
    "Answer again with only the JSON the schema asks for."
)


class Lease(BaseModel):
    """The response of ``POST /v1/leases`` (02-KERNEL-API, Leases)."""

    model_config = ConfigDict(extra="allow")

    lease_id: str
    run_id: str
    step: str
    attempt: int = 1
    expires_at: str | None = None
    heartbeat_seconds: float | None = None
    step_def: dict[str, Any]
    context: dict[str, Any] = Field(default_factory=dict)

    @property
    def kind(self) -> str:
        """The step kind from ``step_def``."""
        return str(self.step_def.get("kind", ""))

    @property
    def timeout_seconds(self) -> float:
        """The step's ``timeout_seconds`` (default 120)."""
        return float(self.step_def.get("timeout_seconds", 120))

    def log_context(self, worker_id: str) -> dict[str, str]:
        """Fields stamped on every log record about this lease."""
        return {
            "run_id": self.run_id,
            "step": self.step,
            "lease_id": self.lease_id,
            "worker_id": worker_id,
        }


class LeaseLost(Exception):
    """The lease is no longer ours: stop at once without ``complete`` or ``fail``."""


class StepTimeout(Exception):
    """The step ran past its ``timeout_seconds``."""


class StepFailure(Exception):
    """A step failure with an honest ``deterministic`` flag."""

    def __init__(self, code: str, message: str, deterministic: bool) -> None:
        self.code = code
        self.message = message
        self.deterministic = deterministic
        super().__init__(f"{code}: {message}")


@dataclass
class StepOutcome:
    """What an executor did.

    ``status`` is ``completed``, ``failed``, ``waiting_approval`` (the kernel
    released the lease; nothing else was called) or ``lease_lost``.
    """

    status: str
    output: Any = None
    error: dict[str, Any] | None = None
    deterministic: bool | None = None
    usage: dict[str, Any] | None = None
    escalations: int = 0
    reused: bool = False


@dataclass
class Runtime:
    """The clients and provider every executor uses."""

    kernel: KernelClient
    gateway: GatewayClient | None = None
    provider: ModelProvider | None = None
    router: ModelRouter = field(default_factory=ModelRouter)
    worker_id: str = "wrk-local"

    @property
    def actor(self) -> dict[str, str]:
        """The ``actor`` object on every event this worker appends."""
        return {"type": "worker", "id": self.worker_id}


def _lease_gone(exc: KernosError) -> bool:
    return exc.status == 410 or exc.code in _LEASE_GONE_CODES


class _Execution:
    """Per-step plumbing: event posting, completion, abort and deadline checks."""

    def __init__(
        self,
        runtime: Runtime,
        lease: Lease,
        abort: threading.Event | None,
        deadline: float | None,
    ) -> None:
        self.runtime = runtime
        self.lease = lease
        self.abort = abort
        self.deadline = deadline
        self.log: StepLogger = step_logger(logger, lease.log_context(runtime.worker_id))

    def check(self) -> None:
        """Raise when the lease is gone or the step is out of time."""
        if self.abort is not None and self.abort.is_set():
            raise LeaseLost(f"lease {self.lease.lease_id} lost (heartbeat returned 410)")
        if self.deadline is not None and time.monotonic() > self.deadline:
            raise StepTimeout(f"step exceeded {self.lease.timeout_seconds:g} s")

    def remaining(self) -> float | None:
        """Seconds left before the deadline, floored at one second."""
        if self.deadline is None:
            return None
        return max(1.0, self.deadline - time.monotonic())

    def _kernel(self, call: Callable[[], Any]) -> Any:
        try:
            return call()
        except KernosNetworkError:
            raise
        except KernosError as exc:
            if _lease_gone(exc):
                raise LeaseLost(f"kernel refused lease {self.lease.lease_id}: {exc}") from exc
            raise

    def post(self, kind: str, payload: Mapping[str, Any]) -> dict[str, Any]:
        """Append an external event under this lease."""
        self.check()
        result = self._kernel(
            lambda: self.runtime.kernel.post_event(
                self.lease.run_id,
                kind,
                payload,
                self.runtime.actor,
                lease_id=self.lease.lease_id,
                timeout=self.remaining(),
            )
        )
        self.log.debug("appended %s seq=%s", kind, result.get("seq"))
        return dict(result)

    def complete(
        self, output: Any, usage: Mapping[str, Any] | None = None, **extra: Any
    ) -> StepOutcome:
        """``POST complete`` and describe the result."""
        if self.abort is not None and self.abort.is_set():
            raise LeaseLost(f"lease {self.lease.lease_id} lost before completion")
        self._kernel(
            lambda: self.runtime.kernel.complete(
                self.lease.lease_id, output, usage, timeout=self.remaining()
            )
        )
        self.log.info("step completed")
        return StepOutcome(
            status="completed", output=output, usage=dict(usage) if usage else None, **extra
        )

    def fail(self, code: str, message: str, deterministic: bool) -> StepOutcome:
        """``POST fail`` with an honest ``deterministic`` flag and describe the result."""
        if self.abort is not None and self.abort.is_set():
            raise LeaseLost(f"lease {self.lease.lease_id} lost before failure report")
        self._kernel(
            lambda: self.runtime.kernel.fail(
                self.lease.lease_id, code, message, deterministic, timeout=self.remaining()
            )
        )
        self.log.warning("step failed code=%s deterministic=%s: %s", code, deterministic, message)
        return StepOutcome(
            status="failed",
            error={"code": code, "message": message},
            deterministic=deterministic,
        )


def _guarded(execution: _Execution, body: Callable[[], StepOutcome]) -> StepOutcome:
    """Run ``body`` and map every failure class onto ``fail`` with the right flag."""
    try:
        try:
            return body()
        except (LeaseLost, KernosNetworkError):
            raise
        except StepTimeout as exc:
            return execution.fail("step_timeout", str(exc), deterministic=False)
        except TemplateError as exc:
            return execution.fail(exc.code, str(exc), deterministic=True)
        except SchemaError as exc:
            return execution.fail(exc.code, str(exc), deterministic=True)
        except StepFailure as exc:
            return execution.fail(exc.code, exc.message, exc.deterministic)
        except ProviderError as exc:
            return execution.fail(exc.code, str(exc), deterministic=exc.deterministic)
        except ValueError as exc:
            return execution.fail("step_invalid", str(exc), deterministic=True)
        except KernosError as exc:
            return execution.fail(exc.code, exc.message, deterministic=exc.status < 500)
    except LeaseLost as exc:
        execution.log.warning("stopping step: %s", exc)
        return StepOutcome(status="lease_lost", error={"code": "lease_lost", "message": str(exc)})


# Model steps


def _apply_boundary(
    execution: _Execution, user: str, input_schema: Mapping[str, Any] | None
) -> str:
    step = execution.lease.step_def
    context = execution.lease.context
    declared = [str(item) for item in step.get("data_classes") or []]
    grants = {str(item) for item in (context.get("remit") or {}).get("grants") or []}
    ungranted = [item for item in declared if item not in grants]
    if not ungranted:
        return user
    redacted, report = boundary.redact(user, ungranted, input_schema, context.get("input"))
    execution.post(
        "note",
        {
            "text": "data boundary applied before the model call",
            "redacted": report["redacted"],
            "fields": report["fields"],
            "data": report,
        },
    )
    execution.log.info("redacted %s field(s) for %s", report["fields"], report["redacted"])
    return redacted


def _confidence_below(output: Any, threshold: Any) -> float | None:
    if not isinstance(output, Mapping):
        return None
    value = output.get("confidence")
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    return float(value) if float(value) < float(threshold) else None


@dataclass
class _ModelAttempt:
    tier: str
    user: str
    corrected: bool = False
    escalated_for_refusal: bool = False
    escalations: int = 0
    usage: Usage = field(default_factory=Usage)
    cost_usd: float = 0.0


def _escalate(execution: _Execution, attempt: _ModelAttempt, to_tier: str, reason: str) -> None:
    execution.post(
        "step.escalated",
        {
            "step": execution.lease.step,
            "from_tier": attempt.tier,
            "to_tier": to_tier,
            "reason": reason,
        },
    )
    execution.log.info("escalating %s to %s: %s", attempt.tier, to_tier, reason)
    attempt.tier = to_tier
    attempt.escalations += 1


def _handle_refusal(execution: _Execution, attempt: _ModelAttempt, prompt_name: str) -> bool:
    """Return True to retry at a deeper tier; otherwise the caller fails the step."""
    step = execution.lease.step_def
    on_refusal = str(step.get("on_refusal", "park"))
    next_tier = execution.runtime.router.next_tier(attempt.tier)
    if on_refusal == "escalate" and not attempt.escalated_for_refusal and next_tier:
        attempt.escalated_for_refusal = True
        _escalate(execution, attempt, next_tier, f"model refused prompt {prompt_name!r}")
        return True
    return False


def _run_model(execution: _Execution, input_schema: Mapping[str, Any] | None) -> StepOutcome:
    runtime, lease = execution.runtime, execution.lease
    if runtime.provider is None:
        raise StepFailure("provider_missing", "worker has no model provider", deterministic=False)
    step, context = lease.step_def, lease.context
    prompt_name = str(step.get("prompt", ""))
    prompt = (context.get("prompts") or {}).get(prompt_name)
    if not isinstance(prompt, Mapping):
        raise StepFailure("prompt_missing", f"prompt {prompt_name!r} not in lease", True)
    output_schema = step.get("output_schema")
    system_block = build_prefix(
        str(prompt.get("system", "")), context.get("tools") or [], output_schema
    )
    original_user = render(str(prompt.get("user", "")), template_context(context))
    original_user = _apply_boundary(execution, original_user, input_schema)
    tier, _model, effort = runtime.router.resolve(step)
    attempt = _ModelAttempt(tier=tier, user=original_user)
    max_tokens = int(step.get("max_output_tokens", 2048))
    escalate = step.get("escalate") or {}

    while True:
        tier, model, effort = runtime.router.resolve(step, attempt.tier)
        request = ModelRequest(
            system=system_block,
            user=attempt.user,
            model=model,
            output_schema=output_schema,
            max_tokens=max_tokens,
            effort=effort,
            prompt=prompt_name,
            context=context,
            timeout_s=execution.remaining(),
        )
        execution.post(
            "model.called",
            {
                "step": lease.step,
                "model": model,
                "tier": tier,
                "effort": effort,
                "provider": runtime.provider.name,
                "prefix_hash": prefix_hash(system_block),
                "input_hash": input_hash(attempt.user),
                "max_tokens": max_tokens,
            },
        )
        response = runtime.provider.generate(request)
        cost_usd = pricing.cost(model, response.usage)
        attempt.usage = attempt.usage.add(response.usage)
        attempt.cost_usd += cost_usd
        execution.post(
            "model.responded",
            {
                "step": lease.step,
                "output": response.output,
                "usage": response.usage.to_dict(),
                "cost_usd": cost_usd,
                "stop_reason": response.stop_reason,
                "refusal": response.refusal,
                "latency_ms": response.latency_ms,
            },
        )
        if response.refusal:
            if _handle_refusal(execution, attempt, prompt_name):
                continue
            return execution.fail(
                "model_refused", f"model refused prompt {prompt_name!r}", deterministic=True
            )
        problem = check(response.output, output_schema) if output_schema else None
        if problem is not None:
            if not attempt.corrected:
                attempt.corrected = True
                attempt.user = f"{original_user}\n\n{_CORRECTIVE_MESSAGE.format(problem=problem)}"
                execution.log.info("output invalid, retrying once with a corrective message")
                continue
            return execution.fail("output_invalid", problem, deterministic=True)
        if escalate:
            to_tier = str(escalate.get("to_tier", ""))
            threshold = escalate.get("when_confidence_below")
            low = _confidence_below(response.output, threshold) if threshold is not None else None
            if low is not None and runtime.router.rank(attempt.tier) < runtime.router.rank(to_tier):
                _escalate(execution, attempt, to_tier, f"confidence {low:g} below {threshold}")
                continue
        usage = {"tokens": attempt.usage.total(), "usd": round(attempt.cost_usd, 10)}
        return execution.complete(response.output, usage, escalations=attempt.escalations)


def execute_model(
    runtime: Runtime,
    lease: Lease,
    *,
    abort: threading.Event | None = None,
    deadline: float | None = None,
    input_schema: Mapping[str, Any] | None = None,
) -> StepOutcome:
    """Execute a ``model`` step: stable prefix, data boundary, provider call,
    refusal handling, output validation with one corrective retry, confidence
    escalation, then ``complete`` with the summed usage of every attempt.

    ``input_schema`` is the workflow's input schema, used by the data boundary
    for ``x-data-class`` fields; ``abort`` is set by the heartbeat on 410;
    ``deadline`` is the monotonic time the step must finish by.
    """
    execution = _Execution(runtime, lease, abort, deadline)
    return _guarded(execution, lambda: _run_model(execution, input_schema))


# Tool and compensation steps


def _idempotency_key(execution: _Execution, template_ctx: Mapping[str, Any]) -> str | None:
    lease = execution.lease
    declared = lease.step_def.get("idempotency_key")
    if declared is None:
        if lease.kind == "compensation":
            return f"compensation:{lease.run_id}:{lease.step}"
        return None
    key = render_value(declared, template_ctx)
    return str(key)


def _prior_result(
    events: list[Mapping[str, Any]], step: str, tool: str, key: str | None
) -> tuple[Any, int] | None:
    """Find a successful ``tool.result`` recorded for ``key`` in earlier attempts."""
    if key is None:
        return None
    called_seq: int | None = None
    for event in events:
        payload = event.get("payload") or {}
        seq = int(event.get("seq", 0) or 0)
        kind = event.get("kind")
        if payload.get("step") != step or payload.get("tool") != tool:
            continue
        if kind == "tool.called" and payload.get("idempotency_key") == key:
            called_seq = seq
        elif kind == "tool.result" and payload.get("ok"):
            direct = payload.get("idempotency_key") == key
            follows = called_seq is not None and seq > called_seq
            if direct or follows:
                return payload.get("result"), seq
    return None


def _gateway_failure(exc: KernosError) -> tuple[str, bool, dict[str, Any]]:
    """Map a gateway error onto ``(code, deterministic, tool.result payload)``."""
    if exc.status == 403:
        detail = {"reason": exc.code, "detail": exc.message, **exc.details}
        return "tool_refused", True, {"refusal": detail}
    error = {"error": {"code": exc.code, "message": exc.message, **exc.details}}
    if exc.status == 409:
        return "idempotency_conflict", True, error
    if 400 <= exc.status < 500:
        return exc.code if exc.code != f"http_{exc.status}" else "args_invalid", True, error
    code = exc.code if exc.code != f"http_{exc.status}" else "upstream_error"
    return code, False, error


def _run_tool(execution: _Execution) -> StepOutcome:
    runtime, lease = execution.runtime, execution.lease
    if runtime.gateway is None:
        raise StepFailure("gateway_missing", "worker has no gateway client", deterministic=False)
    step, context = lease.step_def, lease.context
    tool = str(step.get("tool", ""))
    template_ctx = template_context(context)
    args = render_value(step.get("args") or {}, template_ctx)
    key = _idempotency_key(execution, template_ctx)
    scope = step.get("scope")
    prior = _prior_result(context.get("prior_events") or [], lease.step, tool, key)
    if prior is not None:
        result, seq = prior
        execution.post(
            "note",
            {
                "text": f"reused tool result from seq {seq} for idempotency key {key}",
                "data": {"tool": tool, "idempotency_key": key, "seq": seq},
            },
        )
        return execution.complete(result, reused=True)
    execution.post(
        "tool.called",
        {"step": lease.step, "tool": tool, "args": args, "scope": scope, "idempotency_key": key},
    )
    started = time.monotonic()
    try:
        response = runtime.gateway.call_tool(
            remit_token=str(context.get("remit_token", "")),
            run_id=lease.run_id,
            step=lease.step,
            lease_id=lease.lease_id,
            tool=tool,
            args=args,
            idempotency_key=key,
            scope=scope,
            timeout=execution.remaining(),
        )
    except KernosNetworkError as exc:
        latency_ms = int((time.monotonic() - started) * 1000)
        execution.post(
            "tool.result",
            {
                "step": lease.step,
                "tool": tool,
                "ok": False,
                "result": {"error": {"code": exc.code, "message": str(exc)}},
                "replayed": False,
                "latency_ms": latency_ms,
            },
        )
        return execution.fail("gateway_unreachable", str(exc), deterministic=False)
    except KernosError as exc:
        latency_ms = int((time.monotonic() - started) * 1000)
        code, deterministic, result = _gateway_failure(exc)
        execution.post(
            "tool.result",
            {
                "step": lease.step,
                "tool": tool,
                "ok": False,
                "result": result,
                "replayed": False,
                "latency_ms": latency_ms,
            },
        )
        return execution.fail(code, f"{tool}: {exc.code}: {exc.message}", deterministic)
    latency_ms = int(response.get("latency_ms", (time.monotonic() - started) * 1000))
    execution.post(
        "tool.result",
        {
            "step": lease.step,
            "tool": tool,
            "ok": True,
            "result": response.get("result"),
            "replayed": bool(response.get("replayed", False)),
            "latency_ms": latency_ms,
        },
    )
    return execution.complete(response.get("result"))


def execute_tool(
    runtime: Runtime,
    lease: Lease,
    *,
    abort: threading.Event | None = None,
    deadline: float | None = None,
) -> StepOutcome:
    """Execute a ``tool`` step: render args and key, reuse a prior result for the
    same key, else append ``tool.called``, call the gateway, append
    ``tool.result``, then ``complete`` with the result or ``fail`` with the
    right ``deterministic`` flag (403, 409 and 422 deterministic; 5xx, 503 and
    network errors not)."""
    execution = _Execution(runtime, lease, abort, deadline)
    return _guarded(execution, lambda: _run_tool(execution))


def execute_compensation(
    runtime: Runtime,
    lease: Lease,
    *,
    abort: threading.Event | None = None,
    deadline: float | None = None,
) -> StepOutcome:
    """Execute a ``compensation`` step; identical to a tool step with the tool and
    args the kernel resolved. A compensation without an idempotency key gets
    ``compensation:<run_id>:<step>`` so it also happens at most once."""
    return execute_tool(runtime, lease, abort=abort, deadline=deadline)


# Action steps


def _run_action(execution: _Execution) -> StepOutcome:
    runtime, lease = execution.runtime, execution.lease
    action = render_value(lease.step_def.get("action") or {}, template_context(lease.context))
    execution.check()
    try:
        decision = execution._kernel(
            lambda: runtime.kernel.propose_action(
                lease.lease_id, action, timeout=execution.remaining()
            )
        )
    except KernosError as exc:
        if exc.status == 403 and exc.code == "action_denied":
            return execution.fail("action_denied", exc.message, deterministic=True)
        raise
    verdict = str(decision.get("decision", ""))
    output = {
        "action_id": decision.get("action_id"),
        "decision": verdict,
        "rule": decision.get("rule"),
        "approval_id": decision.get("approval_id"),
    }
    if verdict == "allow":
        return execution.complete(output)
    if verdict == "approval_required":
        execution.log.info(
            "action %s needs approval %s; lease released",
            output["action_id"],
            output["approval_id"],
        )
        return StepOutcome(status="waiting_approval", output=output)
    if verdict == "deny":
        return execution.fail(
            "action_denied", f"denied by policy rule {decision.get('rule')}", deterministic=True
        )
    raise StepFailure("action_decision_invalid", f"unknown decision {verdict!r}", True)


def execute_action(
    runtime: Runtime,
    lease: Lease,
    *,
    abort: threading.Event | None = None,
    deadline: float | None = None,
) -> StepOutcome:
    """Execute an ``action`` step: render the action, propose it, then
    ``complete`` on ``allow``, stop without any call on ``approval_required``
    (the kernel released the lease) and ``fail`` deterministically on ``deny``."""
    execution = _Execution(runtime, lease, abort, deadline)
    return _guarded(execution, lambda: _run_action(execution))


def execute_step(
    runtime: Runtime,
    lease: Lease,
    *,
    abort: threading.Event | None = None,
    deadline: float | None = None,
    input_schema: Mapping[str, Any] | None = None,
) -> StepOutcome:
    """Dispatch on the lease's step kind."""
    kind = lease.kind
    if kind == "model":
        return execute_model(
            runtime, lease, abort=abort, deadline=deadline, input_schema=input_schema
        )
    if kind == "tool":
        return execute_tool(runtime, lease, abort=abort, deadline=deadline)
    if kind == "compensation":
        return execute_compensation(runtime, lease, abort=abort, deadline=deadline)
    if kind == "action":
        return execute_action(runtime, lease, abort=abort, deadline=deadline)
    execution = _Execution(runtime, lease, abort, deadline)
    return _guarded(
        execution,
        lambda: execution.fail("unknown_step_kind", f"unknown step kind {kind!r}", True),
    )
