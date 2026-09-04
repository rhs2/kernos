"""Templating: ``{{path}}`` rendering and ``$ref`` resolution."""

from __future__ import annotations

import pytest

from kernos.templating import (
    TemplateError,
    has_path,
    lookup,
    parse_path,
    render,
    render_value,
    resolve_refs,
    template_context,
    to_text,
)

CONTEXT = {
    "input": {"invoice_id": "inv-1001", "total": 7250.0, "count": 3, "flag": True, "nothing": None},
    "steps": {
        "extract": {
            "output": {
                "vendor": "Northwind Dairy",
                "lines": [{"amount": 12.5}, {"amount": 7.25}],
                "meta": {"b": 2, "a": 1},
            }
        }
    },
    "run": {"id": "run_1", "workflow": "intake"},
}


def test_render_strings_verbatim_and_numbers_as_json() -> None:
    assert (
        render("Pay {{input.invoice_id}} of {{input.total}}", CONTEXT) == "Pay inv-1001 of 7250.0"
    )
    assert render("{{input.count}} lines", CONTEXT) == "3 lines"


def test_render_objects_and_lists_as_compact_json() -> None:
    assert render("{{steps.extract.output.meta}}", CONTEXT) == '{"b":2,"a":1}'
    assert render("{{steps.extract.output.lines}}", CONTEXT) == '[{"amount":12.5},{"amount":7.25}]'


def test_render_booleans_and_null_as_json() -> None:
    assert render("{{input.flag}} {{input.nothing}}", CONTEXT) == "true null"


def test_render_numeric_list_index() -> None:
    assert render("{{steps.extract.output.lines.1.amount}}", CONTEXT) == "7.25"


def test_render_tolerates_whitespace_inside_braces() -> None:
    assert render("{{ input.invoice_id }}", CONTEXT) == "inv-1001"


def test_render_leaves_text_without_templates_unchanged() -> None:
    text = "no templates { here } or {{ unclosed"
    assert render(text, CONTEXT) == text


def test_render_missing_path_is_an_error_not_empty() -> None:
    with pytest.raises(TemplateError) as info:
        render("{{input.missing}}", CONTEXT)
    assert info.value.code == "template_missing_path"
    assert info.value.path == "input.missing"


def test_render_missing_list_index_is_an_error() -> None:
    with pytest.raises(TemplateError) as info:
        render("{{steps.extract.output.lines.5.amount}}", CONTEXT)
    assert info.value.code == "template_missing_path"


def test_render_unknown_root_is_missing() -> None:
    with pytest.raises(TemplateError):
        render("{{secrets.key}}", CONTEXT)


def test_invalid_path_segments_are_rejected() -> None:
    with pytest.raises(TemplateError) as info:
        parse_path("input..x")
    assert info.value.code == "template_invalid_path"
    with pytest.raises(TemplateError):
        parse_path("input.bad-name")
    with pytest.raises(TemplateError):
        parse_path("")


def test_render_is_deterministic() -> None:
    template = "{{input.invoice_id}}:{{steps.extract.output.meta}}"
    assert render(template, CONTEXT) == render(template, CONTEXT)


def test_lookup_and_has_path() -> None:
    assert lookup(CONTEXT, "run.workflow") == "intake"
    assert has_path(CONTEXT, "input.total")
    assert not has_path(CONTEXT, "input.total.x")


def test_to_text_rules() -> None:
    assert to_text("plain") == "plain"
    assert to_text(1) == "1"
    assert to_text(None) == "null"
    assert to_text({"a": [1, "x"]}) == '{"a":[1,"x"]}'


def test_resolve_refs_preserves_types_at_any_depth() -> None:
    value = {
        "amount": {"$ref": "steps.extract.output.total"},
        "nested": {"ids": [{"$ref": "input.invoice_id"}, {"$ref": "input.count"}]},
        "literal": "{{input.invoice_id}}",
    }
    context = {**CONTEXT, "steps": {"extract": {"output": {"total": 7250.0}}}}
    resolved = resolve_refs(value, context)
    assert resolved["amount"] == 7250.0 and isinstance(resolved["amount"], float)
    assert resolved["nested"]["ids"] == ["inv-1001", 3]
    assert resolved["literal"] == "{{input.invoice_id}}"


def test_resolve_refs_copies_referenced_values() -> None:
    resolved = resolve_refs({"$ref": "steps.extract.output.lines"}, CONTEXT)
    resolved.append("x")
    assert len(CONTEXT["steps"]["extract"]["output"]["lines"]) == 2


def test_resolve_refs_ignores_objects_with_extra_keys() -> None:
    value = {"$ref": "input.total", "note": "not a ref"}
    assert resolve_refs(value, CONTEXT) == value


def test_resolve_refs_missing_path_raises() -> None:
    with pytest.raises(TemplateError) as info:
        resolve_refs({"x": {"$ref": "input.nope"}}, CONTEXT)
    assert info.value.code == "template_missing_path"


def test_render_value_applies_both_forms() -> None:
    value = {"summary": "Pay {{input.invoice_id}}", "amount": {"$ref": "input.total"}, "n": 1}
    assert render_value(value, CONTEXT) == {"summary": "Pay inv-1001", "amount": 7250.0, "n": 1}


def test_render_value_typed_strings_only_for_whole_templates() -> None:
    assert render_value("{{input.total}}", CONTEXT, typed_strings=True) == 7250.0
    assert render_value("Total {{input.total}}", CONTEXT, typed_strings=True) == "Total 7250.0"
    assert render_value("{{input.total}}", CONTEXT) == "7250.0"


def test_template_context_projects_the_three_roots() -> None:
    lease_context = {"input": {"a": 1}, "steps": {}, "run": {"id": "r"}, "remit_token": "x"}
    assert template_context(lease_context) == {"input": {"a": 1}, "steps": {}, "run": {"id": "r"}}
