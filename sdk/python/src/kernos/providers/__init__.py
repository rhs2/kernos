"""Model providers: the protocol, the mock, and the Anthropic implementation."""

from __future__ import annotations

from typing import Any

from kernos.providers.base import (
    ModelProvider,
    ModelRequest,
    ModelResponse,
    ProviderError,
    ProviderUnavailable,
    Usage,
)
from kernos.providers.mock import MockProvider

__all__ = [
    "PROVIDER_NAMES",
    "MockProvider",
    "ModelProvider",
    "ModelRequest",
    "ModelResponse",
    "ProviderError",
    "ProviderUnavailable",
    "Usage",
    "get_provider",
]

PROVIDER_NAMES: tuple[str, ...] = ("mock", "anthropic")


def get_provider(name: str, **kwargs: Any) -> ModelProvider:
    """Construct a provider by name (``mock`` or ``anthropic``).

    Raises :class:`ProviderUnavailable` for an unknown name or when the
    Anthropic provider cannot be built.
    """
    if name == "mock":
        return MockProvider(**kwargs)
    if name == "anthropic":
        from kernos.providers.anthropic import AnthropicProvider

        return AnthropicProvider(**kwargs)
    raise ProviderUnavailable(f"unknown provider {name!r}; expected one of {PROVIDER_NAMES}")
