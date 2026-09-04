"""The model router: three tiers, chosen from the step declaration only."""

from __future__ import annotations

import os
from collections.abc import Mapping
from typing import Any

__all__ = ["DEFAULT_MODELS", "TIERS", "ModelRouter"]

TIERS: tuple[str, ...] = ("cheap", "standard", "deep")
"""Tier names in escalation order: cheap < standard < deep."""

DEFAULT_MODELS: dict[str, str] = {
    "deep": "claude-opus-5",
    "standard": "claude-sonnet-5",
    "cheap": "claude-haiku-4-5-20251001",
}
DEFAULT_EFFORT: dict[str, str] = {"deep": "high", "standard": "medium", "cheap": "low"}
ENV_OVERRIDES: dict[str, str] = {
    "deep": "KERNOS_MODEL_DEEP",
    "standard": "KERNOS_MODEL_STANDARD",
    "cheap": "KERNOS_MODEL_CHEAP",
}
EFFORTS: tuple[str, ...] = ("low", "medium", "high", "xhigh")


class ModelRouter:
    """Map a tier to a model id and a default effort.

    Model ids come from the shipped table unless ``KERNOS_MODEL_DEEP``,
    ``KERNOS_MODEL_STANDARD`` or ``KERNOS_MODEL_CHEAP`` override them; explicit
    ``overrides`` win over both.
    """

    def __init__(
        self,
        overrides: Mapping[str, str] | None = None,
        env: Mapping[str, str] | None = None,
    ) -> None:
        environment = os.environ if env is None else env
        self._models = dict(DEFAULT_MODELS)
        for tier, variable in ENV_OVERRIDES.items():
            value = environment.get(variable)
            if value:
                self._models[tier] = value
        for tier, model in (overrides or {}).items():
            self._models[self.check_tier(tier)] = model

    @staticmethod
    def check_tier(tier: str) -> str:
        """Return ``tier`` or raise ``ValueError`` when it is not a known tier."""
        if tier not in TIERS:
            raise ValueError(f"unknown tier {tier!r}; expected one of {TIERS}")
        return tier

    @staticmethod
    def rank(tier: str) -> int:
        """Position in the escalation order (cheap 0, standard 1, deep 2)."""
        return TIERS.index(ModelRouter.check_tier(tier))

    @staticmethod
    def next_tier(tier: str) -> str | None:
        """The next deeper tier, or ``None`` at ``deep``."""
        index = ModelRouter.rank(tier)
        return TIERS[index + 1] if index + 1 < len(TIERS) else None

    def model_for(self, tier: str) -> str:
        """The model id of ``tier``."""
        return self._models[self.check_tier(tier)]

    def default_effort(self, tier: str) -> str:
        """The effort used when the step does not declare one."""
        return DEFAULT_EFFORT[self.check_tier(tier)]

    def resolve(self, step_def: Mapping[str, Any], tier: str | None = None) -> tuple[str, str, str]:
        """Return ``(tier, model, effort)`` for a model step, optionally at another tier."""
        chosen = self.check_tier(tier or str(step_def.get("tier", "standard")))
        effort = str(step_def.get("effort") or self.default_effort(chosen))
        if effort not in EFFORTS:
            raise ValueError(f"unknown effort {effort!r}; expected one of {EFFORTS}")
        return chosen, self.model_for(chosen), effort

    def as_dict(self) -> dict[str, str]:
        """The tier to model mapping in force."""
        return dict(self._models)
