"""The data boundary: pattern and schema-field redaction."""

from __future__ import annotations

from kernos.boundary import data_class_fields, redact

INPUT_SCHEMA = {
    "type": "object",
    "properties": {
        "invoice_id": {"type": "string"},
        "contact_name": {"type": "string", "x-data-class": "pii"},
        "notes": {"type": "string", "x-data-class": ["internal", "pii"]},
        "billing": {
            "type": "object",
            "properties": {"iban": {"type": "string", "x-data-class": "financial"}},
        },
    },
}


def test_email_is_redacted() -> None:
    text, report = redact("Write to ana@halcyon.example today.", ["pii"])
    assert text == "Write to [REDACTED:email] today."
    assert report["redacted"] == ["pii"]
    assert report["fields"] == 1
    assert report["matches"] == {"email": 1}


def test_phone_numbers_are_redacted() -> None:
    text, report = redact("Call +1 415 555 0142 or (020) 7946 0958.", ["pii"])
    assert "[REDACTED:phone]" in text
    assert "0142" not in text and "0958" not in text
    assert report["matches"]["phone"] == 2


def test_national_ids_are_redacted() -> None:
    text, report = redact("SSN 123-45-6789 and NI AB123456C on file.", ["pii"])
    assert text == "SSN [REDACTED:national_id] and NI [REDACTED:national_id] on file."
    assert report["matches"]["national_id"] == 2


def test_amounts_dates_and_invoice_numbers_survive() -> None:
    text = "Invoice INV-2026-0001 dated 2026-09-04 12:00 for 7250.00 USD, entry 1234567."
    redacted, report = redact(text, ["pii"])
    assert redacted == text
    assert report["fields"] == 0


def test_schema_field_values_are_redacted_by_name() -> None:
    values = {"invoice_id": "inv-1001", "contact_name": "Ana Reyes", "notes": "top secret"}
    text, report = redact(
        "Contact: Ana Reyes about inv-1001 (top secret)", ["pii"], INPUT_SCHEMA, values
    )
    assert text == "Contact: [REDACTED:contact_name] about inv-1001 ([REDACTED:notes])"
    assert report["fields"] == 2
    assert sorted(report["field_names"]) == ["contact_name", "notes"]


def test_only_ungranted_classes_select_fields() -> None:
    fields = data_class_fields(INPUT_SCHEMA, ["financial"])
    assert fields == [("billing.iban", "financial")]
    values = {"billing": {"iban": "HAL1234567890"}}
    text, report = redact("Pay to HAL1234567890", ["financial"], INPUT_SCHEMA, values)
    assert text == "Pay to [REDACTED:billing.iban]"
    assert report["field_names"] == ["billing.iban"]


def test_empty_classes_touch_nothing() -> None:
    text, report = redact("ana@halcyon.example", [])
    assert text == "ana@halcyon.example"
    assert report == {"redacted": [], "fields": 0, "matches": {}, "field_names": []}


def test_classes_are_deduplicated_in_the_report() -> None:
    _text, report = redact("nothing here", ["pii", "pii", "internal"])
    assert report["redacted"] == ["pii", "internal"]
