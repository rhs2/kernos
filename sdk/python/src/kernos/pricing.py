"""Price table per model id and cost computation.

Prices are USD per million tokens for ``input``, ``output``, ``cache_read`` and
``cache_write``. The table ships with the SDK and can be extended or overridden
with ``KERNOS_PRICING_JSON``: either an inline JSON object
``{"<model>": {"input": .., "output": .., "cache_read": .., "cache_write": ..}}``
or the path of a file holding one. A ``"default"`` entry, when present, prices
any model the table does not name.
"""

from __future__ import annotations

import json
import os
from collections.abc import Mapping
from dataclasses import dataclass
from pathlib import Path
from typing import Any

__all__ = ["DEFAULT_PRICES", "Price", "PricingError", "cost", "load_prices", "price_for"]

ENV_PRICING = "KERNOS_PRICING_JSON"
_MILLION = 1_000_000.0


@dataclass(frozen=True)
class Price:
    """USD per million tokens for one model."""

    input: float
    output: float
    cache_read: float
    cache_write: float

    @classmethod
    def from_mapping(cls, data: Mapping[str, Any]) -> Price:
        """Build a price from a mapping; missing cache prices derive from ``input``."""
        base_input = float(data["input"])
        return cls(
            input=base_input,
            output=float(data["output"]),
            cache_read=float(data.get("cache_read", base_input * 0.1)),
            cache_write=float(data.get("cache_write", base_input * 1.25)),
        )


DEFAULT_PRICES: dict[str, Price] = {
    "claude-opus-5": Price(input=5.0, output=25.0, cache_read=0.5, cache_write=6.25),
    "claude-sonnet-5": Price(input=2.0, output=10.0, cache_read=0.2, cache_write=2.5),
    "claude-haiku-4-5-20251001": Price(input=1.0, output=5.0, cache_read=0.1, cache_write=1.25),
    "claude-haiku-4-5": Price(input=1.0, output=5.0, cache_read=0.1, cache_write=1.25),
}
"""Prices that ship with the SDK, keyed by model id."""


class PricingError(KeyError):
    """No price is known for a model id."""


def _parse_override(raw: str) -> dict[str, Any]:
    text = raw.strip()
    if not text.startswith(("{", "[")):
        text = Path(text).read_text(encoding="utf-8")
    parsed = json.loads(text)
    if not isinstance(parsed, dict):
        raise ValueError(f"{ENV_PRICING} must hold a JSON object")
    return parsed


def load_prices(env: Mapping[str, str] | None = None) -> dict[str, Price]:
    """Return the shipped table merged with the ``KERNOS_PRICING_JSON`` override."""
    environment = os.environ if env is None else env
    prices = dict(DEFAULT_PRICES)
    raw = environment.get(ENV_PRICING)
    if raw:
        for model, data in _parse_override(raw).items():
            prices[str(model)] = Price.from_mapping(data)
    return prices


def price_for(model: str, prices: Mapping[str, Price] | None = None) -> Price:
    """Resolve the price for ``model``: exact id, then the longest id prefix, then ``default``."""
    table = load_prices() if prices is None else prices
    if model in table:
        return table[model]
    candidates = [key for key in table if key != "default" and model.startswith(key)]
    if candidates:
        return table[max(candidates, key=len)]
    if "default" in table:
        return table["default"]
    raise PricingError(f"no price known for model {model!r}")


def _usage_field(usage: Any, name: str) -> int:
    if isinstance(usage, Mapping):
        return int(usage.get(name, 0) or 0)
    return int(getattr(usage, name, 0) or 0)


def cost(model: str, usage: Any, prices: Mapping[str, Price] | None = None) -> float:
    """Return the USD cost of ``usage`` (a mapping or object with the four token fields)."""
    price = price_for(model, prices)
    total = (
        _usage_field(usage, "input_tokens") * price.input
        + _usage_field(usage, "output_tokens") * price.output
        + _usage_field(usage, "cache_read_tokens") * price.cache_read
        + _usage_field(usage, "cache_write_tokens") * price.cache_write
    )
    return round(total / _MILLION, 10)
