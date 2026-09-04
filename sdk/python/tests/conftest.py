"""Shared fixtures: the stub servers and the Halcyon Provisions test bundle data."""

from __future__ import annotations

import copy
from collections.abc import Iterator
from dataclasses import dataclass
from typing import Any

import pytest

from kernos.client import GatewayClient, KernelClient
from kernos.providers.base import ModelProvider
from kernos.providers.mock import MockProvider
from kernos.router import ModelRouter
from kernos.steps import Lease, Runtime
from stubs import GatewayApi, KernelApi, StubState, start_server

INPUT: dict[str, Any] = {
    "invoice_id": "inv-1001",
    "text": (
        "Invoice from Northwind Dairy for Halcyon Provisions, total 7250.00 USD. "
        "Contact ana@halcyon.example or +1 415 555 0142."
    ),
    "total": 7250.0,
    "accounts": ["5100", "5200"],
    "contact_name": "Ana Reyes",
}
PROMPTS: dict[str, dict[str, str]] = {
    "extract": {
        "system": "You extract fields from supplier invoices for Halcyon Provisions.",
        "user": "Invoice text:\n{{input.text}}\nContact: {{input.contact_name}}",
    },
    "code": {
        "system": "You assign a general-ledger account to an invoice line.",
        "user": "Vendor: {{steps.extract.output.vendor}}\nAccounts: {{input.accounts}}",
    },
    "bad": {"system": "You answer badly.", "user": "Invoice {{input.invoice_id}}"},
    "broken": {"system": "You never run.", "user": "{{input.does_not_exist}}"},
}
MOCK: dict[str, Any] = {
    "extract": {
        "vendor": "Northwind Dairy",
        "invoice_id": "{{input.invoice_id}}",
        "total": "{{input.total}}",
        "currency": "USD",
        "description": "Milk delivery",
    },
    "code": {"account": "5100", "confidence": 0.93},
    "bad": {"vendor": 1, "invoice_id": "inv-1001", "total": 7250.0, "currency": "USD"},
}
TOOLS: list[dict[str, Any]] = [
    {"id": "ledger.void_entry", "description": "Void a posted entry", "writes": True},
    {"id": "ledger.post_entry", "description": "Post a journal entry", "writes": True},
    {"id": "ledger.lookup_vendor", "description": "Find a vendor by name", "writes": False},
]
EXTRACT_SCHEMA: dict[str, Any] = {
    "type": "object",
    "required": ["vendor", "invoice_id", "total", "currency"],
    "properties": {
        "vendor": {"type": "string"},
        "invoice_id": {"type": "string"},
        "total": {"type": "number"},
        "currency": {"type": "string"},
        "description": {"type": "string"},
    },
}
CODE_SCHEMA: dict[str, Any] = {
    "type": "object",
    "required": ["account", "confidence"],
    "properties": {"account": {"type": "string"}, "confidence": {"type": "number"}},
}
INPUT_SCHEMA: dict[str, Any] = {
    "type": "object",
    "required": ["invoice_id", "text", "total"],
    "properties": {
        "invoice_id": {"type": "string"},
        "text": {"type": "string"},
        "total": {"type": "number"},
        "accounts": {"type": "array"},
        "contact_name": {"type": "string", "x-data-class": "pii"},
    },
}
EXTRACT_STEP: dict[str, Any] = {
    "id": "extract",
    "kind": "model",
    "tier": "standard",
    "effort": "low",
    "prompt": "extract",
    "output_schema": EXTRACT_SCHEMA,
    "on_refusal": "park",
    "max_output_tokens": 1024,
}
CODE_STEP: dict[str, Any] = {
    "id": "code",
    "kind": "model",
    "tier": "cheap",
    "effort": "low",
    "prompt": "code",
    "output_schema": CODE_SCHEMA,
    "escalate": {"when_confidence_below": 0.7, "to_tier": "standard"},
}
ACTION_STEP: dict[str, Any] = {
    "id": "propose_payment",
    "kind": "action",
    "action": {
        "kind": "payment.issue",
        "amount": {"$ref": "steps.extract.output.total"},
        "currency": {"$ref": "steps.extract.output.currency"},
        "writes_to_system_of_record": True,
        "target": "ledger",
        "data_classes": [],
        "paths": [],
        "idempotency_key": {"$ref": "input.invoice_id"},
        "summary": "Pay invoice {{input.invoice_id}} to {{steps.extract.output.vendor}}",
    },
}
POST_STEP: dict[str, Any] = {
    "id": "post",
    "kind": "tool",
    "tool": "ledger.post_entry",
    "args": {
        "invoice_id": {"$ref": "input.invoice_id"},
        "vendor": {"$ref": "steps.extract.output.vendor"},
        "account": {"$ref": "steps.code.output.account"},
        "amount": {"$ref": "steps.extract.output.total"},
    },
    "idempotency_key": "{{input.invoice_id}}",
    "compensation": {
        "tool": "ledger.void_entry",
        "args": {"entry_id": {"$ref": "steps.post.output.entry_id"}, "reason": "run abandoned"},
    },
}
COMPENSATION_STEP: dict[str, Any] = {
    "id": "comp_post",
    "kind": "compensation",
    "for_step": "post",
    "tool": "ledger.void_entry",
    "args": {"entry_id": 1, "reason": "run abandoned"},
}
STEP_OUTPUTS: dict[str, Any] = {
    "extract": {
        "output": {
            "vendor": "Northwind Dairy",
            "invoice_id": "inv-1001",
            "total": 7250.0,
            "currency": "USD",
            "description": "Milk delivery",
        }
    },
    "code": {"output": {"account": "5100", "confidence": 0.93}},
}


def base_context(**overrides: Any) -> dict[str, Any]:
    """A lease ``context`` for the Halcyon invoice intake workflow."""
    context: dict[str, Any] = {
        "input": copy.deepcopy(INPUT),
        "steps": copy.deepcopy(STEP_OUTPUTS),
        "run": {
            "id": "run_1",
            "bundle": {"name": "halcyon.finance.invoice_intake", "version": "1.0.0"},
            "workflow": "intake",
            "requested_by": {"id": "u-ana", "role": "ap_clerk", "manager": "u-tom"},
            "department": "finance",
        },
        "remit_token": "krt1.payload.signature.key_1",
        "remit": {
            "autonomy": "supervised",
            "grants": [],
            "tools": ["ledger.*"],
            "scopes": ["sql:table:ledger_entries"],
        },
        "prompts": copy.deepcopy(PROMPTS),
        "mock": copy.deepcopy(MOCK),
        "tools": copy.deepcopy(TOOLS),
        "pacing": False,
        "approved_actions": [],
        "prior_events": [],
    }
    context.update(overrides)
    return context


def lease_dict(
    step_def: dict[str, Any],
    *,
    lease_id: str = "lse_1",
    run_id: str = "run_1",
    attempt: int = 1,
    heartbeat_seconds: float = 0.2,
    context: dict[str, Any] | None = None,
    **context_overrides: Any,
) -> dict[str, Any]:
    """A ``POST /v1/leases`` response for ``step_def``."""
    return {
        "lease_id": lease_id,
        "run_id": run_id,
        "step": step_def["id"],
        "attempt": attempt,
        "expires_at": "2026-09-04T12:00:30.000Z",
        "heartbeat_seconds": heartbeat_seconds,
        "step_def": copy.deepcopy(step_def),
        "context": context if context is not None else base_context(**context_overrides),
    }


class CapturingProvider:
    """A provider that records requests and replays scripted responses."""

    name = "capture"

    def __init__(self, responses: list[Any] | None = None) -> None:
        from kernos.providers.base import ModelResponse, Usage

        self.requests: list[Any] = []
        self._responses = list(responses or [])
        self._default = ModelResponse(output={"text": "ok"}, usage=Usage(10, 5))

    def generate(self, request: Any) -> Any:
        self.requests.append(request)
        if self._responses:
            item = self._responses.pop(0)
            if isinstance(item, Exception):
                raise item
            return item
        return self._default


@dataclass
class Stub:
    """Handles on the running stubs for one test."""

    state: StubState
    kernel_url: str
    gateway_url: str

    def kernel(self, **kwargs: Any) -> KernelClient:
        return KernelClient(self.kernel_url, **kwargs)

    def gateway(self, **kwargs: Any) -> GatewayClient:
        return GatewayClient(self.gateway_url, **kwargs)

    def runtime(
        self,
        provider: ModelProvider | None = None,
        *,
        worker_id: str = "wrk-test",
        router: ModelRouter | None = None,
        gateway_url: str | None = None,
    ) -> Runtime:
        return Runtime(
            kernel=self.kernel(timeout=5.0),
            gateway=GatewayClient(gateway_url or self.gateway_url, timeout=5.0),
            provider=provider or MockProvider(),
            router=router or ModelRouter(env={}),
            worker_id=worker_id,
        )

    def lease(self, step_def: dict[str, Any], **kwargs: Any) -> Lease:
        """Register a lease with the stub kernel and return it as a model."""
        raw = lease_dict(step_def, **kwargs)
        self.state.add_lease(raw)
        self.state.lease_queue.clear()
        return Lease.model_validate(raw)

    def queue(self, step_def: dict[str, Any], **kwargs: Any) -> dict[str, Any]:
        """Register a lease and leave it queued for ``POST /v1/leases``."""
        return self.state.add_lease(lease_dict(step_def, **kwargs))


@pytest.fixture
def stub() -> Iterator[Stub]:
    state = StubState()
    kernel_server, kernel_url = start_server(KernelApi(state))
    gateway_server, gateway_url = start_server(GatewayApi(state))
    try:
        yield Stub(state, kernel_url, gateway_url)
    finally:
        kernel_server.shutdown()
        kernel_server.server_close()
        gateway_server.shutdown()
        gateway_server.server_close()


@pytest.fixture(autouse=True)
def clean_environment(monkeypatch: pytest.MonkeyPatch) -> None:
    for name in (
        "KERNOS_MOCK_REFUSE",
        "KERNOS_MOCK_CONFIDENCE",
        "KERNOS_PRICING_JSON",
        "KERNOS_MODEL_DEEP",
        "KERNOS_MODEL_STANDARD",
        "KERNOS_MODEL_CHEAP",
        "KERNOS_KERNEL_URL",
        "KERNOS_GATEWAY_URL",
        "KERNOS_TOKEN",
        "KERNOS_PROVIDER",
        "KERNOS_LOG",
        "ANTHROPIC_API_KEY",
    ):
        monkeypatch.delenv(name, raising=False)
