"""JSON Schema validation (draft 2020-12) with compact error messages."""

from __future__ import annotations

import copy
from collections.abc import Mapping, Sequence
from typing import Any

import jsonschema
from jsonschema import Draft202012Validator
from jsonschema.exceptions import best_match

__all__ = ["SchemaError", "check", "schema_default", "validate"]


class SchemaError(ValueError):
    """An instance does not satisfy a schema, or the schema itself is invalid.

    ``path`` is the JSON path of the failing element (``$`` for the root) and
    ``code`` is ``output_invalid`` for an instance failure or ``schema_invalid``
    for a broken schema.
    """

    def __init__(self, message: str, path: str = "$", code: str = "output_invalid") -> None:
        self.path = path
        self.code = code
        super().__init__(message)


def _format_path(parts: Sequence[Any]) -> str:
    return "$" + "".join(f".{part}" for part in parts)


def check(instance: Any, schema: Mapping[str, Any]) -> str | None:
    """Return a compact ``"<path>: <problem>"`` message, or ``None`` when valid."""
    try:
        Draft202012Validator.check_schema(schema)
    except jsonschema.SchemaError as exc:
        raise SchemaError(f"invalid schema: {exc.message}", code="schema_invalid") from exc
    errors = list(Draft202012Validator(schema).iter_errors(instance))
    if not errors:
        return None
    error = best_match(errors)
    return f"{_format_path(list(error.absolute_path))}: {error.message}"


def validate(instance: Any, schema: Mapping[str, Any]) -> None:
    """Raise :class:`SchemaError` when ``instance`` does not satisfy ``schema``."""
    message = check(instance, schema)
    if message is not None:
        path = message.split(":", 1)[0]
        raise SchemaError(message, path=path)


def _first_type(schema: Mapping[str, Any]) -> str | None:
    declared = schema.get("type")
    if isinstance(declared, list):
        return str(declared[0]) if declared else None
    if isinstance(declared, str):
        return declared
    if "properties" in schema:
        return "object"
    if "items" in schema:
        return "array"
    return None


def _number_default(schema: Mapping[str, Any], integer: bool) -> float | int:
    value: float | int = 0
    if "minimum" in schema:
        value = schema["minimum"]
    elif "exclusiveMinimum" in schema:
        value = schema["exclusiveMinimum"] + 1
    if "maximum" in schema and value > schema["maximum"]:
        value = schema["maximum"]
    if "exclusiveMaximum" in schema and value >= schema["exclusiveMaximum"]:
        value = schema["exclusiveMaximum"] - 1
    return int(value) if integer else value


def _confidence_default(schema: Mapping[str, Any]) -> float:
    value = 1.0
    if "maximum" in schema:
        value = min(value, float(schema["maximum"]))
    if "exclusiveMaximum" in schema and value >= schema["exclusiveMaximum"]:
        value = float(schema["exclusiveMaximum"]) - 0.01
    return value


def schema_default(schema: Mapping[str, Any], *, name: str | None = None) -> Any:
    """Derive an instance that satisfies the common subset of ``schema``.

    ``default``, ``const`` and ``enum`` win when present. Objects get every
    declared property, arrays get ``minItems`` items, strings honour
    ``minLength``, numbers honour their bounds. A number property called
    ``confidence`` defaults to the highest allowed value so a mock run does not
    escalate unless the operator asks it to.
    """
    if "default" in schema:
        return copy.deepcopy(schema["default"])
    if "const" in schema:
        return copy.deepcopy(schema["const"])
    if schema.get("enum"):
        return copy.deepcopy(schema["enum"][0])
    kind = _first_type(schema)
    if kind == "object":
        return {
            key: schema_default(sub, name=key) for key, sub in schema.get("properties", {}).items()
        }
    if kind == "array":
        count = int(schema.get("minItems", 0))
        item_schema = schema.get("items", {})
        return [schema_default(item_schema) for _ in range(count)]
    if kind == "string":
        return "x" * int(schema.get("minLength", 0))
    if kind in ("number", "integer"):
        if name == "confidence" and kind == "number":
            return _confidence_default(schema)
        return _number_default(schema, integer=kind == "integer")
    if kind == "boolean":
        return False
    return None
