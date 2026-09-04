"""The data boundary: redaction before content reaches a model provider.

A step declares the data classes its prompt content carries. When the run's
remit does not grant one of them, the rendered user content is redacted before
the provider sees it and the worker records the redaction in a ``note`` event
``{"redacted": [<classes>], "fields": <count>}`` (03-REMIT, Grants).

Rules applied, in order:

1. Every input field whose ``input_schema`` property carries ``"x-data-class"``
   with a class that is not granted: its rendered value is replaced wherever it
   appears in the text.
2. Built-in patterns: email addresses, national identifiers and phone numbers.

The pattern rules are heuristics and are deliberately conservative about
numbers: a candidate phone number needs 10 to 15 digits and must not look like a
date, so amounts and invoice numbers survive.
"""

from __future__ import annotations

import re
from collections.abc import Callable, Iterable, Mapping
from typing import Any

from kernos.templating import TemplateError, lookup, to_text

__all__ = ["RedactionReport", "data_class_fields", "redact"]

RedactionReport = dict[str, Any]
"""``{"redacted": [classes], "fields": n, "matches": {rule: n}, "field_names": [...]}``."""

_EMAIL_RE = re.compile(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)*\.[A-Za-z]{2,}")
_SSN_RE = re.compile(r"(?<![\w-])\d{3}-\d{2}-\d{4}(?![\w-])")
_NINO_RE = re.compile(r"(?<![\w-])[A-CEGHJ-PR-TW-Z]{2}\d{6}[A-D](?![\w-])")
_PHONE_RE = re.compile(r"(?<![\w-])\+?\(?\d[\d\s().-]{7,18}\d(?![\w-])")
_DATE_LIKE_RE = re.compile(r"^\d{4}[-.]\d{2}[-.]\d{2}")


def _phone_accept(candidate: str) -> bool:
    digits = sum(1 for char in candidate if char.isdigit())
    return 10 <= digits <= 15 and not _DATE_LIKE_RE.match(candidate)


_PATTERNS: tuple[tuple[str, re.Pattern[str], Callable[[str], bool] | None], ...] = (
    ("email", _EMAIL_RE, None),
    ("national_id", _SSN_RE, None),
    ("national_id", _NINO_RE, None),
    ("phone", _PHONE_RE, _phone_accept),
)


def _classes_of(prop: Mapping[str, Any]) -> list[str]:
    declared = prop.get("x-data-class")
    if isinstance(declared, str):
        return [declared]
    if isinstance(declared, list):
        return [str(item) for item in declared]
    return []


def data_class_fields(
    input_schema: Mapping[str, Any] | None, classes: Iterable[str], prefix: str = ""
) -> list[tuple[str, str]]:
    """Return ``(dotted field path, data class)`` pairs for schema properties marked
    ``x-data-class`` with a class in ``classes``. Nested objects are walked."""
    wanted = set(classes)
    found: list[tuple[str, str]] = []
    if not input_schema:
        return found
    for name, prop in (input_schema.get("properties") or {}).items():
        if not isinstance(prop, Mapping):
            continue
        path = f"{prefix}{name}"
        for cls in _classes_of(prop):
            if cls in wanted:
                found.append((path, cls))
                break
        if prop.get("properties"):
            found.extend(data_class_fields(prop, wanted, prefix=f"{path}."))
    return found


def _redact_fields(
    text: str,
    fields: list[tuple[str, str]],
    input_values: Mapping[str, Any] | None,
) -> tuple[str, list[str]]:
    replaced: list[str] = []
    if not input_values:
        return text, replaced
    values: list[tuple[str, str]] = []
    for path, _cls in fields:
        try:
            rendered = to_text(lookup({"input": input_values}, f"input.{path}"))
        except TemplateError:
            continue
        if rendered:
            values.append((path, rendered))
    for path, rendered in sorted(values, key=lambda item: len(item[1]), reverse=True):
        if rendered in text:
            text = text.replace(rendered, f"[REDACTED:{path}]")
            replaced.append(path)
    return text, replaced


def _redact_patterns(text: str) -> tuple[str, dict[str, int]]:
    counts: dict[str, int] = {}
    for label, pattern, accept in _PATTERNS:
        hits = 0

        def substitute(match: re.Match[str], label: str = label, accept: Any = accept) -> str:
            nonlocal hits
            candidate = match.group(0)
            if accept is not None and not accept(candidate):
                return candidate
            hits += 1
            return f"[REDACTED:{label}]"

        text = pattern.sub(substitute, text)
        if hits:
            counts[label] = counts.get(label, 0) + hits
    return text, counts


def redact(
    text: str,
    classes: Iterable[str],
    input_schema: Mapping[str, Any] | None = None,
    input_values: Mapping[str, Any] | None = None,
) -> tuple[str, RedactionReport]:
    """Redact ``text`` for the data classes in ``classes`` that are not granted.

    Returns the redacted text and a report. When ``classes`` is empty nothing is
    touched and the report has ``fields`` 0. ``fields`` counts every replacement
    made: one per schema field found in the text plus one per pattern match.
    """
    ordered = list(dict.fromkeys(classes))
    if not ordered:
        return text, {"redacted": [], "fields": 0, "matches": {}, "field_names": []}
    fields = data_class_fields(input_schema, ordered)
    text, field_names = _redact_fields(text, fields, input_values)
    text, matches = _redact_patterns(text)
    report: RedactionReport = {
        "redacted": ordered,
        "fields": len(field_names) + sum(matches.values()),
        "matches": matches,
        "field_names": field_names,
    }
    return text, report
