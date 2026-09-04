"""The model router."""

from __future__ import annotations

import pytest

from kernos.router import TIERS, ModelRouter


def test_defaults_per_tier() -> None:
    router = ModelRouter(env={})
    assert router.model_for("deep") == "claude-opus-5"
    assert router.model_for("standard") == "claude-sonnet-5"
    assert router.model_for("cheap") == "claude-haiku-4-5-20251001"
    assert router.default_effort("deep") == "high"
    assert router.default_effort("standard") == "medium"
    assert router.default_effort("cheap") == "low"


def test_environment_overrides() -> None:
    router = ModelRouter(env={"KERNOS_MODEL_CHEAP": "local-small", "KERNOS_MODEL_DEEP": ""})
    assert router.model_for("cheap") == "local-small"
    assert router.model_for("deep") == "claude-opus-5"


def test_explicit_overrides_win() -> None:
    router = ModelRouter({"standard": "custom"}, env={"KERNOS_MODEL_STANDARD": "from-env"})
    assert router.model_for("standard") == "custom"
    assert router.as_dict()["standard"] == "custom"


def test_escalation_order() -> None:
    assert TIERS == ("cheap", "standard", "deep")
    assert ModelRouter.next_tier("cheap") == "standard"
    assert ModelRouter.next_tier("standard") == "deep"
    assert ModelRouter.next_tier("deep") is None
    assert ModelRouter.rank("deep") > ModelRouter.rank("standard") > ModelRouter.rank("cheap")


def test_resolve_from_the_step_declaration() -> None:
    router = ModelRouter(env={})
    assert router.resolve({"tier": "cheap", "effort": "high"}) == (
        "cheap",
        "claude-haiku-4-5-20251001",
        "high",
    )
    assert router.resolve({"tier": "deep"}) == ("deep", "claude-opus-5", "high")
    assert router.resolve({}) == ("standard", "claude-sonnet-5", "medium")
    assert router.resolve({"tier": "cheap"}, "standard")[0] == "standard"


def test_unknown_tier_or_effort_rejected() -> None:
    router = ModelRouter(env={})
    with pytest.raises(ValueError):
        router.model_for("ultra")
    with pytest.raises(ValueError):
        router.resolve({"tier": "cheap", "effort": "extreme"})
