"""The stable prompt prefix and its hashes (07-REASONING-SDK, model step 1).

The system block is built stable-first so the provider's prompt cache hits: the
frozen prompt ``system`` text, then a fixed ``Tools available`` block listing the
run's tools sorted by id, then the output schema. Everything volatile (the
rendered user template, prior outputs) belongs in the user turn.
"""

from __future__ import annotations

import hashlib
import json
from collections.abc import Iterable, Mapping
from typing import Any

__all__ = ["build_prefix", "input_hash", "prefix_hash", "tools_block"]

_TOOL_KEYS = ("id", "description", "writes")


def _compact(value: Any) -> str:
    return json.dumps(value, separators=(",", ":"), sort_keys=True, ensure_ascii=False)


def tools_block(tools: Iterable[Mapping[str, Any]]) -> str:
    """The fixed ``Tools available: [...]`` line, tools sorted by id."""
    normalised = [
        {key: tool[key] for key in _TOOL_KEYS if key in tool}
        for tool in sorted(tools, key=lambda tool: str(tool.get("id", "")))
    ]
    return f"Tools available: {_compact(normalised)}"


def build_prefix(
    system: str,
    tools: Iterable[Mapping[str, Any]] | None = None,
    output_schema: Mapping[str, Any] | None = None,
) -> str:
    """Assemble the system block: system text, tools block, output schema."""
    parts = [system.rstrip(), tools_block(tools or [])]
    if output_schema:
        parts.append(f"Output schema: {_compact(output_schema)}")
    return "\n\n".join(parts)


def _sha256(text: str) -> str:
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def prefix_hash(system_block: str) -> str:
    """``sha256`` of the system block, recorded in ``model.called``."""
    return _sha256(system_block)


def input_hash(user_content: str) -> str:
    """``sha256`` of the user turn, recorded in ``model.called``."""
    return _sha256(user_content)
