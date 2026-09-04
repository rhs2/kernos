"""Templating over the run context (05-BUNDLE, section Templating).

Two forms exist and both are evaluated against the context
``{"input": ..., "steps": {"<id>": {"output": ...}}, "run": {...}}``:

* In strings, ``{{path}}`` is replaced by the value at ``path`` rendered as
  text: strings verbatim, everything else as compact JSON.
* As a whole value, ``{"$ref": "path"}`` is replaced by the value at ``path``
  with its type preserved. ``$ref`` may appear at any depth.

Paths are dotted; list indices are numeric segments
(``steps.extract.output.lines.0.amount``). A missing path is an error, never an
empty string, so a broken template fails a step deterministically.
"""

from __future__ import annotations

import copy
import json
import re
from collections.abc import Mapping
from typing import Any

__all__ = [
    "ROOTS",
    "TemplateError",
    "has_path",
    "lookup",
    "parse_path",
    "render",
    "render_value",
    "resolve_refs",
    "template_context",
    "to_text",
]

ROOTS: tuple[str, ...] = ("input", "steps", "run")
"""The only roots a template path may start from."""

_TEMPLATE_RE = re.compile(r"\{\{\s*([^{}]*?)\s*\}\}")
_WHOLE_TEMPLATE_RE = re.compile(r"^\{\{\s*([^{}]*?)\s*\}\}$")
_SEGMENT_RE = re.compile(r"^(?:[A-Za-z_][A-Za-z0-9_]*|[0-9]+)$")


class TemplateError(ValueError):
    """A template could not be evaluated.

    ``code`` is ``template_missing_path`` when the path does not resolve and
    ``template_invalid_path`` when the path is malformed. Both are deterministic
    failures: re-running the step with the same context gives the same error.
    """

    def __init__(self, code: str, path: str, message: str | None = None) -> None:
        self.code = code
        self.path = path
        super().__init__(message or f"{code}: {path}")


def parse_path(path: str) -> list[str]:
    """Split a dotted path into segments, validating each one.

    Raises ``TemplateError(template_invalid_path)`` for an empty path, an empty
    segment, or a segment that is neither an identifier nor a number.
    """
    if not isinstance(path, str) or not path:
        raise TemplateError("template_invalid_path", str(path), "empty template path")
    segments = path.split(".")
    for segment in segments:
        if not _SEGMENT_RE.match(segment):
            raise TemplateError(
                "template_invalid_path", path, f"invalid path segment {segment!r} in {path!r}"
            )
    return segments


def lookup(context: Mapping[str, Any], path: str) -> Any:
    """Return the value at ``path`` in ``context``.

    Mappings are indexed by key and lists by numeric segment. Any segment that
    does not resolve raises ``TemplateError(template_missing_path)``.
    """
    current: Any = context
    for segment in parse_path(path):
        if isinstance(current, Mapping) and segment in current:
            current = current[segment]
        elif isinstance(current, list) and segment.isdigit() and int(segment) < len(current):
            current = current[int(segment)]
        else:
            raise TemplateError("template_missing_path", path, f"missing template path {path!r}")
    return current


def has_path(context: Mapping[str, Any], path: str) -> bool:
    """Return whether ``path`` resolves in ``context`` without raising."""
    try:
        lookup(context, path)
    except TemplateError:
        return False
    return True


def to_text(value: Any) -> str:
    """Render a value as template text: strings verbatim, the rest as compact JSON."""
    if isinstance(value, str):
        return value
    return json.dumps(value, separators=(",", ":"), ensure_ascii=False)


def render(template: str, context: Mapping[str, Any]) -> str:
    """Replace every ``{{path}}`` in ``template`` with the text of the value at ``path``.

    Text without any ``{{`` is returned unchanged. A missing path raises
    ``TemplateError``.
    """

    def substitute(match: re.Match[str]) -> str:
        return to_text(lookup(context, match.group(1)))

    return _TEMPLATE_RE.sub(substitute, template)


def is_ref(value: Any) -> bool:
    """Return whether ``value`` is exactly ``{"$ref": "<path>"}``."""
    return (
        isinstance(value, Mapping)
        and len(value) == 1
        and "$ref" in value
        and isinstance(value["$ref"], str)
    )


def resolve_refs(value: Any, context: Mapping[str, Any]) -> Any:
    """Replace every ``{"$ref": path}`` inside ``value`` by a copy of the referenced value.

    Strings are left untouched; use :func:`render_value` to apply both forms.
    """
    if is_ref(value):
        return copy.deepcopy(lookup(context, value["$ref"]))
    if isinstance(value, Mapping):
        return {key: resolve_refs(item, context) for key, item in value.items()}
    if isinstance(value, list):
        return [resolve_refs(item, context) for item in value]
    return value


def render_value(value: Any, context: Mapping[str, Any], *, typed_strings: bool = False) -> Any:
    """Apply both template forms to ``value`` at any depth.

    ``$ref`` objects are replaced with their typed value and strings are rendered
    with :func:`render`. With ``typed_strings`` a string that consists of a single
    ``{{path}}`` expression resolves to the referenced value with its type kept;
    the mock provider uses this so bundle mock outputs can satisfy typed schemas.
    """
    if is_ref(value):
        return copy.deepcopy(lookup(context, value["$ref"]))
    if isinstance(value, str):
        whole = _WHOLE_TEMPLATE_RE.match(value) if typed_strings else None
        if whole:
            return copy.deepcopy(lookup(context, whole.group(1)))
        return render(value, context)
    if isinstance(value, Mapping):
        return {
            key: render_value(item, context, typed_strings=typed_strings)
            for key, item in value.items()
        }
    if isinstance(value, list):
        return [render_value(item, context, typed_strings=typed_strings) for item in value]
    return value


def template_context(lease_context: Mapping[str, Any]) -> dict[str, Any]:
    """Project a lease ``context`` onto the three template roots."""
    return {root: lease_context.get(root, {}) for root in ROOTS}
