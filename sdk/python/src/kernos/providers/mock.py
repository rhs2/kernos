"""The mock provider: runs any bundle end to end without a model.

Behaviour (07-REASONING-SDK, Model router):

* The output is the bundle's ``mock[<prompt name>]`` rendered with the run
  context, or a schema-derived default when the bundle has none.
* Usage is deterministic: ``input_tokens = len(prompt) // 4`` where the prompt is
  the system block plus the user turn, ``output_tokens = len(output) // 4`` over
  the compact JSON of the output, no cache tokens, ``latency_ms`` 1.
* ``KERNOS_MOCK_REFUSE=<prompt>[,<prompt>...]`` makes the mock refuse those
  prompts with ``stop_reason`` ``refusal``.
* ``KERNOS_MOCK_CONFIDENCE=<prompt>=<value>[,<prompt>=<value>...]`` overrides
  the ``confidence`` field of those prompts' outputs.

Both variables are read on every call so tests and operators can flip them
without restarting anything.
"""

from __future__ import annotations

import copy
import json
import os
from collections.abc import Mapping
from typing import Any

from kernos.providers.base import ModelRequest, ModelResponse, Usage
from kernos.schema import schema_default
from kernos.templating import render_value, template_context

__all__ = ["MockProvider", "mock_confidence_overrides", "mock_refusals"]

ENV_REFUSE = "KERNOS_MOCK_REFUSE"
ENV_CONFIDENCE = "KERNOS_MOCK_CONFIDENCE"
REFUSAL_TEXT = "I cannot help with this request."


def mock_refusals(env: Mapping[str, str] | None = None) -> set[str]:
    """Prompt names the mock refuses, from ``KERNOS_MOCK_REFUSE``."""
    raw = (os.environ if env is None else env).get(ENV_REFUSE, "")
    return {item.strip() for item in raw.split(",") if item.strip()}


def mock_confidence_overrides(env: Mapping[str, str] | None = None) -> dict[str, float]:
    """Confidence overrides per prompt name, from ``KERNOS_MOCK_CONFIDENCE``."""
    raw = (os.environ if env is None else env).get(ENV_CONFIDENCE, "")
    overrides: dict[str, float] = {}
    for item in raw.split(","):
        if "=" not in item:
            continue
        prompt, _, value = item.partition("=")
        try:
            overrides[prompt.strip()] = float(value.strip())
        except ValueError as exc:
            raise ValueError(
                f"{ENV_CONFIDENCE}: {item.strip()!r} is not <prompt>=<number>"
            ) from exc
    return overrides


class MockProvider:
    """Deterministic provider for tests, acceptance runs and bundles under development."""

    name = "mock"

    def __init__(self, env: Mapping[str, str] | None = None) -> None:
        self._env = env

    def _output_for(self, request: ModelRequest) -> Any:
        context = request.context or {}
        mock_outputs = context.get("mock") or {}
        if request.prompt is not None and request.prompt in mock_outputs:
            template = copy.deepcopy(mock_outputs[request.prompt])
            return render_value(template, template_context(context), typed_strings=True)
        if request.output_schema:
            return schema_default(request.output_schema)
        return {"text": "mock response"}

    def generate(self, request: ModelRequest) -> ModelResponse:
        """Produce the mock output for ``request`` with deterministic usage."""
        prompt_text = f"{request.system}\n{request.user}"
        if request.prompt is not None and request.prompt in mock_refusals(self._env):
            output: Any = {"text": REFUSAL_TEXT}
            refusal = True
            stop_reason = "refusal"
        else:
            output = self._output_for(request)
            override = mock_confidence_overrides(self._env).get(request.prompt or "")
            if override is not None and isinstance(output, dict):
                output["confidence"] = override
            refusal = False
            stop_reason = "end_turn"
        text = json.dumps(output, separators=(",", ":"), ensure_ascii=False)
        usage = Usage(input_tokens=len(prompt_text) // 4, output_tokens=len(text) // 4)
        return ModelResponse(
            output=output,
            usage=usage,
            stop_reason=stop_reason,
            refusal=refusal,
            latency_ms=1,
            model=request.model,
            text=text,
        )
