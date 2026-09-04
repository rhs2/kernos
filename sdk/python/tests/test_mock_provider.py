"""The mock provider and the provider registry."""

from __future__ import annotations

import json

import pytest

from conftest import CODE_SCHEMA, EXTRACT_SCHEMA, base_context
from kernos.providers import ProviderUnavailable, get_provider
from kernos.providers.base import ModelRequest
from kernos.providers.mock import MockProvider, mock_confidence_overrides, mock_refusals


def _request(prompt: str, schema: dict | None = None, **kwargs: object) -> ModelRequest:
    return ModelRequest(
        system="system block",
        user="user turn",
        model="claude-sonnet-5",
        output_schema=schema,
        prompt=prompt,
        context=base_context(),
        **kwargs,  # type: ignore[arg-type]
    )


def test_bundle_mock_output_is_rendered_with_types() -> None:
    response = MockProvider().generate(_request("extract", EXTRACT_SCHEMA))
    assert response.output == {
        "vendor": "Northwind Dairy",
        "invoice_id": "inv-1001",
        "total": 7250.0,
        "currency": "USD",
        "description": "Milk delivery",
    }
    assert isinstance(response.output["total"], float)
    assert response.refusal is False
    assert response.stop_reason == "end_turn"


def test_schema_default_when_the_bundle_has_no_mock() -> None:
    response = MockProvider().generate(_request("unknown", CODE_SCHEMA))
    assert response.output == {"account": "", "confidence": 1.0}


def test_text_when_no_mock_and_no_schema() -> None:
    assert MockProvider().generate(_request("unknown")).output == {"text": "mock response"}


def test_usage_is_deterministic() -> None:
    request = _request("code", CODE_SCHEMA)
    first = MockProvider().generate(request)
    second = MockProvider().generate(request)
    assert first.usage == second.usage
    assert first.usage.input_tokens == len("system block\nuser turn") // 4
    assert first.usage.output_tokens == len(json.dumps(first.output, separators=(",", ":"))) // 4
    assert first.usage.cache_read_tokens == 0 and first.usage.cache_write_tokens == 0
    assert first.latency_ms == 1
    assert first.model == "claude-sonnet-5"


def test_refuse_environment_variable(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("KERNOS_MOCK_REFUSE", "extract, other")
    assert mock_refusals() == {"extract", "other"}
    refused = MockProvider().generate(_request("extract", EXTRACT_SCHEMA))
    assert refused.refusal is True and refused.stop_reason == "refusal"
    assert refused.output == {"text": "I cannot help with this request."}
    assert MockProvider().generate(_request("code", CODE_SCHEMA)).refusal is False


def test_confidence_override(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("KERNOS_MOCK_CONFIDENCE", "code=0.4,extract=0.9")
    assert mock_confidence_overrides() == {"code": 0.4, "extract": 0.9}
    assert MockProvider().generate(_request("code", CODE_SCHEMA)).output["confidence"] == 0.4


def test_confidence_override_must_be_numeric(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("KERNOS_MOCK_CONFIDENCE", "code=high")
    with pytest.raises(ValueError):
        MockProvider().generate(_request("code", CODE_SCHEMA))


def test_explicit_environment_mapping() -> None:
    provider = MockProvider(env={"KERNOS_MOCK_REFUSE": "code"})
    assert provider.generate(_request("code", CODE_SCHEMA)).refusal is True


def test_registry() -> None:
    assert get_provider("mock").name == "mock"
    with pytest.raises(ProviderUnavailable):
        get_provider("other")
    with pytest.raises(ProviderUnavailable):
        get_provider("anthropic")


def test_ref_values_inside_mock_outputs_keep_their_type() -> None:
    context = base_context()
    context["mock"]["extract"] = {
        "vendor": "Northwind Dairy",
        "invoice_id": {"$ref": "input.invoice_id"},
        "total": {"$ref": "input.total"},
        "currency": "USD",
        "lines": [{"amount": {"$ref": "input.total"}, "label": "{{input.invoice_id}}"}],
    }
    request = ModelRequest(
        system="s",
        user="u",
        model="claude-sonnet-5",
        output_schema=EXTRACT_SCHEMA,
        prompt="extract",
        context=context,
    )
    output = MockProvider().generate(request).output
    assert output["total"] == 7250.0 and isinstance(output["total"], float)
    assert output["invoice_id"] == "inv-1001"
    assert output["lines"] == [{"amount": 7250.0, "label": "inv-1001"}]
