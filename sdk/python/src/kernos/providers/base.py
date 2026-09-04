"""Provider protocol and the request and response shapes shared by every provider."""

from __future__ import annotations

from collections.abc import Mapping
from dataclasses import dataclass, field
from typing import Any, Protocol, runtime_checkable

__all__ = [
    "ModelProvider",
    "ModelRequest",
    "ModelResponse",
    "ProviderError",
    "ProviderUnavailable",
    "Usage",
]


class ProviderError(RuntimeError):
    """A provider call failed.

    ``deterministic`` tells the worker whether a retry could help: a rejected
    request (4xx) is deterministic, a timeout or a 5xx is not.
    """

    def __init__(self, message: str, *, deterministic: bool, code: str = "model_error") -> None:
        self.deterministic = deterministic
        self.code = code
        super().__init__(message)


class ProviderUnavailable(ProviderError):
    """The provider cannot be constructed (missing package or credentials).

    This is a configuration error and the worker exits with code 2.
    """

    def __init__(self, message: str) -> None:
        super().__init__(message, deterministic=True, code="provider_unavailable")


@dataclass
class Usage:
    """Token usage of one model call, as recorded in ``model.responded``."""

    input_tokens: int = 0
    output_tokens: int = 0
    cache_read_tokens: int = 0
    cache_write_tokens: int = 0

    def total(self) -> int:
        """Every token billed for the call."""
        return (
            self.input_tokens
            + self.output_tokens
            + self.cache_read_tokens
            + self.cache_write_tokens
        )

    def add(self, other: Usage) -> Usage:
        """Return the element-wise sum of two usages."""
        return Usage(
            input_tokens=self.input_tokens + other.input_tokens,
            output_tokens=self.output_tokens + other.output_tokens,
            cache_read_tokens=self.cache_read_tokens + other.cache_read_tokens,
            cache_write_tokens=self.cache_write_tokens + other.cache_write_tokens,
        )

    def to_dict(self) -> dict[str, int]:
        """The ``usage`` object of the ``model.responded`` payload."""
        return {
            "input_tokens": self.input_tokens,
            "output_tokens": self.output_tokens,
            "cache_read_tokens": self.cache_read_tokens,
            "cache_write_tokens": self.cache_write_tokens,
        }


@dataclass
class ModelRequest:
    """One generation request.

    ``system`` is the stable prefix (07-REASONING-SDK step 1), ``user`` the
    volatile turn. ``prompt`` names the bundle prompt and ``context`` is the lease
    context; both exist so the mock provider can find and render the bundle's
    mock output. ``timeout_s`` bounds the call for providers that support it.
    """

    system: str
    user: str
    model: str
    output_schema: Mapping[str, Any] | None = None
    max_tokens: int = 2048
    effort: str = "medium"
    prompt: str | None = None
    context: Mapping[str, Any] | None = None
    timeout_s: float | None = None


@dataclass
class ModelResponse:
    """One generation result.

    ``output`` is the parsed structured output when the request carried a schema
    and the text parsed as JSON, otherwise ``{"text": "<raw text>"}``.
    """

    output: Any
    usage: Usage = field(default_factory=Usage)
    stop_reason: str = "end_turn"
    refusal: bool = False
    latency_ms: int = 0
    model: str = ""
    text: str = ""


@runtime_checkable
class ModelProvider(Protocol):
    """What the step executor needs from a provider."""

    name: str

    def generate(self, request: ModelRequest) -> ModelResponse:
        """Run one generation; raise :class:`ProviderError` on failure."""
        ...
