"""The price table and cost computation."""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from kernos.pricing import DEFAULT_PRICES, Price, PricingError, cost, load_prices, price_for
from kernos.providers.base import Usage


def test_shipped_prices_for_the_three_tiers() -> None:
    assert cost("claude-sonnet-5", {"input_tokens": 1_000_000}) == 2.0
    assert cost("claude-opus-5", Usage(input_tokens=1_000_000, output_tokens=1_000_000)) == 30.0
    assert cost("claude-haiku-4-5-20251001", {"output_tokens": 200_000}) == 1.0


def test_cache_tokens_are_priced() -> None:
    usage = Usage(cache_read_tokens=1_000_000, cache_write_tokens=1_000_000)
    assert cost("claude-sonnet-5", usage) == pytest.approx(0.2 + 2.5)


def test_zero_usage_costs_nothing() -> None:
    assert cost("claude-opus-5", Usage()) == 0.0


def test_unknown_model_raises() -> None:
    with pytest.raises(PricingError):
        price_for("mystery-model", DEFAULT_PRICES)


def test_prefix_match_resolves_dated_ids() -> None:
    assert (
        price_for("claude-sonnet-5-20260101", DEFAULT_PRICES) == DEFAULT_PRICES["claude-sonnet-5"]
    )


def test_inline_override_adds_and_replaces(monkeypatch: pytest.MonkeyPatch) -> None:
    override = {
        "claude-sonnet-5": {"input": 1.0, "output": 2.0, "cache_read": 0.1, "cache_write": 1.25},
        "local-model": {"input": 0.0, "output": 0.0},
    }
    monkeypatch.setenv("KERNOS_PRICING_JSON", json.dumps(override))
    prices = load_prices()
    assert prices["claude-sonnet-5"] == Price(1.0, 2.0, 0.1, 1.25)
    assert prices["local-model"] == Price(0.0, 0.0, 0.0, 0.0)
    assert cost("claude-sonnet-5", {"input_tokens": 1_000_000}) == 1.0


def test_file_override_and_default_entry(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    path = tmp_path / "prices.json"
    path.write_text(json.dumps({"default": {"input": 4.0, "output": 8.0}}), encoding="utf-8")
    monkeypatch.setenv("KERNOS_PRICING_JSON", str(path))
    assert price_for("mystery-model") == Price(4.0, 8.0, 0.4, 5.0)


def test_override_must_be_an_object(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("KERNOS_PRICING_JSON", "[]")
    with pytest.raises(ValueError):
        load_prices()
