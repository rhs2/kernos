"""Schema validation messages and schema-derived defaults."""

from __future__ import annotations

import pytest

from kernos.schema import SchemaError, check, schema_default, validate

SCHEMA = {
    "type": "object",
    "required": ["vendor", "total"],
    "properties": {
        "vendor": {"type": "string"},
        "total": {"type": "number"},
        "lines": {
            "type": "array",
            "items": {"type": "object", "properties": {"amount": {"type": "number"}}},
        },
    },
}


def test_valid_instance_passes() -> None:
    assert check({"vendor": "Northwind Dairy", "total": 7250.0}, SCHEMA) is None
    validate({"vendor": "Northwind Dairy", "total": 7250.0}, SCHEMA)


def test_type_error_names_the_path() -> None:
    message = check({"vendor": "Northwind Dairy", "total": "7250"}, SCHEMA)
    assert message == "$.total: '7250' is not of type 'number'"


def test_required_error_is_at_the_root() -> None:
    message = check({"vendor": "Northwind Dairy"}, SCHEMA)
    assert message is not None and message.startswith("$: 'total' is a required property")


def test_nested_list_path_in_message() -> None:
    instance = {"vendor": "x", "total": 1, "lines": [{"amount": 1}, {"amount": "two"}]}
    message = check(instance, SCHEMA)
    assert message == "$.lines.1.amount: 'two' is not of type 'number'"


def test_validate_raises_schema_error_with_path() -> None:
    with pytest.raises(SchemaError) as info:
        validate({"vendor": 5, "total": 1}, SCHEMA)
    assert info.value.path == "$.vendor"
    assert info.value.code == "output_invalid"


def test_invalid_schema_is_reported() -> None:
    with pytest.raises(SchemaError) as info:
        check({}, {"type": "not-a-type"})
    assert info.value.code == "schema_invalid"


def test_schema_default_object_with_every_property() -> None:
    schema = {
        "type": "object",
        "required": ["vendor"],
        "properties": {
            "vendor": {"type": "string", "minLength": 3},
            "total": {"type": "number", "minimum": 5},
            "count": {"type": "integer", "maximum": -2},
            "currency": {"type": "string", "enum": ["USD", "EUR"]},
            "ok": {"type": "boolean"},
            "lines": {"type": "array", "minItems": 2, "items": {"type": "number"}},
            "confidence": {"type": "number", "maximum": 0.9},
            "fixed": {"const": "yes"},
            "given": {"type": "string", "default": "d"},
            "untyped": {},
        },
    }
    value = schema_default(schema)
    assert value == {
        "vendor": "xxx",
        "total": 5,
        "count": -2,
        "currency": "USD",
        "ok": False,
        "lines": [0, 0],
        "confidence": 0.9,
        "fixed": "yes",
        "given": "d",
        "untyped": None,
    }
    assert check(value, schema) is None


def test_schema_default_confidence_is_high_by_default() -> None:
    schema = {"type": "object", "properties": {"confidence": {"type": "number"}}}
    assert schema_default(schema) == {"confidence": 1.0}


def test_schema_default_type_list_and_exclusive_bounds() -> None:
    assert schema_default({"type": ["integer", "null"], "exclusiveMinimum": 3}) == 4
    assert schema_default({"type": "number", "exclusiveMaximum": 0}) == -1
    assert schema_default({"properties": {"a": {"type": "string"}}}) == {"a": ""}
