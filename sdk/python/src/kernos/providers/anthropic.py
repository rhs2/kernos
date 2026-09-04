"""The Anthropic provider (07-REASONING-SDK, Model router).

Uses the ``anthropic`` package, imported lazily so the SDK works without it.
The request is a Messages API call with adaptive thinking, ``output_config``
carrying the step's effort and the output schema as a ``json_schema`` format, and
``cache_control`` on the system block so the stable prefix is cached.
``stop_reason == "refusal"`` sets the refusal flag. Usage maps onto the
``model.responded`` fields and cost comes from :mod:`kernos.pricing`.
"""

from __future__ import annotations

import json
import os
import time
from typing import Any

from kernos.providers.base import (
    ModelRequest,
    ModelResponse,
    ProviderError,
    ProviderUnavailable,
    Usage,
)

__all__ = ["AnthropicProvider"]

ENV_API_KEY = "ANTHROPIC_API_KEY"
_NO_EFFORT_PREFIXES = ("claude-haiku-4-5",)


def _import_anthropic() -> Any:
    try:
        import anthropic
    except ImportError as exc:  # pragma: no cover - depends on the environment
        raise ProviderUnavailable(
            "the anthropic package is not installed; install kernos-sdk[anthropic]"
        ) from exc
    return anthropic


def _supports_effort(model: str) -> bool:
    return not model.startswith(_NO_EFFORT_PREFIXES)


class AnthropicProvider:
    """Messages API provider. Needs ``ANTHROPIC_API_KEY`` unless a client is injected."""

    name = "anthropic"

    def __init__(
        self,
        api_key: str | None = None,
        *,
        client: Any | None = None,
        timeout_s: float = 120.0,
        max_retries: int = 2,
    ) -> None:
        self._sdk = _import_anthropic()
        if client is not None:
            self._client = client
            return
        key = api_key or os.environ.get(ENV_API_KEY)
        if not key:
            raise ProviderUnavailable(f"{ENV_API_KEY} is not set")
        self._client = self._sdk.Anthropic(api_key=key, timeout=timeout_s, max_retries=max_retries)

    def _build_kwargs(self, request: ModelRequest) -> dict[str, Any]:
        kwargs: dict[str, Any] = {
            "model": request.model,
            "max_tokens": request.max_tokens,
            "system": [
                {"type": "text", "text": request.system, "cache_control": {"type": "ephemeral"}}
            ],
            "messages": [{"role": "user", "content": request.user}],
        }
        output_config: dict[str, Any] = {}
        if _supports_effort(request.model):
            kwargs["thinking"] = {"type": "adaptive"}
            output_config["effort"] = request.effort
        if request.output_schema:
            output_config["format"] = {"type": "json_schema", "schema": dict(request.output_schema)}
        if output_config:
            kwargs["output_config"] = output_config
        return kwargs

    def _call(self, kwargs: dict[str, Any], timeout_s: float | None) -> Any:
        sdk = self._sdk
        client = self._client if timeout_s is None else self._client.with_options(timeout=timeout_s)
        try:
            return client.messages.create(**kwargs)
        except sdk.RateLimitError as exc:
            raise ProviderError(
                f"rate limited: {exc}", deterministic=False, code="rate_limited"
            ) from exc
        except sdk.APIStatusError as exc:
            status = int(getattr(exc, "status_code", 0) or 0)
            raise ProviderError(
                f"provider returned {status}: {exc}",
                deterministic=400 <= status < 500 and status not in (408, 429),
                code="model_error",
            ) from exc
        except sdk.APIConnectionError as exc:
            raise ProviderError(f"provider unreachable: {exc}", deterministic=False) from exc

    @staticmethod
    def _parse_output(text: str, structured: bool) -> Any:
        if structured:
            try:
                return json.loads(text)
            except ValueError:
                return {"text": text}
        return {"text": text}

    def generate(self, request: ModelRequest) -> ModelResponse:
        """Call the Messages API once and map the response onto :class:`ModelResponse`."""
        kwargs = self._build_kwargs(request)
        started = time.monotonic()
        message = self._call(kwargs, request.timeout_s)
        latency_ms = int((time.monotonic() - started) * 1000)
        text = "".join(
            getattr(block, "text", "") for block in message.content if block.type == "text"
        )
        refusal = message.stop_reason == "refusal"
        usage = Usage(
            input_tokens=int(message.usage.input_tokens or 0),
            output_tokens=int(message.usage.output_tokens or 0),
            cache_read_tokens=int(getattr(message.usage, "cache_read_input_tokens", 0) or 0),
            cache_write_tokens=int(getattr(message.usage, "cache_creation_input_tokens", 0) or 0),
        )
        output = (
            {"text": text}
            if refusal
            else self._parse_output(text, structured=bool(request.output_schema))
        )
        return ModelResponse(
            output=output,
            usage=usage,
            stop_reason=str(message.stop_reason or "end_turn"),
            refusal=refusal,
            latency_ms=latency_ms,
            model=str(getattr(message, "model", request.model)),
            text=text,
        )
