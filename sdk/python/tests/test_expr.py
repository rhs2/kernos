"""The policy expression evaluator used by eval assertions."""

from __future__ import annotations

import pytest

from kernos.expr import ExprError, evaluate, is_truthy, parse

CONTEXT = {
    "steps": {"code": {"output": {"account": "5100", "confidence": 0.93, "tags": ["a", "b"]}}},
    "run": {"state": "completed", "remit": {"autonomy": "supervised"}},
    "input": {"total": 7250, "lines": [{"amount": 1}, {"amount": 2}]},
}


@pytest.mark.parametrize(
    ("expression", "expected"),
    [
        ("steps.code.output.confidence >= 0.7", True),
        ("run.state == 'completed'", True),
        ('run.state == "completed" and steps.code.output.account == "5100"', True),
        ("run.state != 'completed' or input.total > 7000", True),
        ("not run.state == 'failed'", True),
        ("input.total + 250 == 7500", True),
        ("-input.total < 0", True),
        ("(input.total - 7250) == 0", True),
        ("'a' in steps.code.output.tags", True),
        ("'z' in steps.code.output.tags", False),
        ("1 in [1, 2, 3]", True),
        ("input.lines.1.amount == 2", True),
        ("input.missing == null", True),
        ("input.missing", None),
        ("input.total > 'text'", False),
        ("input.total == '7250'", False),
        ("true == 1", False),
        ("true == true", True),
        ("input.total + 'x' == null", True),
        ("null and true", False),
        ("null or true", True),
        ("len(steps.code.output.tags) == 2", True),
        ("len(input.missing) == null", True),
        ("input.total >= 7250 and input.total <= 7250", True),
    ],
)
def test_expressions(expression: str, expected: object) -> None:
    assert evaluate(expression, CONTEXT) == expected


def test_comments_and_escapes() -> None:
    assert evaluate('"a\\"b" == "a\\"b"  # trailing comment', {}) is True


def test_custom_functions() -> None:
    functions = {"run.remit.grants": lambda name: name == "pii"}
    assert evaluate('run.remit.grants("pii")', CONTEXT, functions) is True
    assert evaluate('run.remit.grants("phi")', CONTEXT, functions) is False


def test_unknown_function_is_loud() -> None:
    with pytest.raises(ExprError):
        evaluate("nope(1)", CONTEXT)


@pytest.mark.parametrize("source", ["1 +", "(1", "[1, 2", "a b", "1 == == 2", "$x", "'open"])
def test_parse_errors(source: str) -> None:
    with pytest.raises(ExprError):
        parse(source)


def test_truthiness() -> None:
    assert is_truthy(None) is False
    assert is_truthy(0) is False
    assert is_truthy("x") is True
    assert is_truthy([]) is False


def test_double_and_single_quoted_strings_are_equivalent() -> None:
    context = {"run": {"state": "completed"}}
    assert evaluate('run.state == "completed"', context) is True
    assert evaluate("run.state == 'completed'", context) is True
    assert evaluate('run.state == "failed"', context) is False
    assert evaluate('run.state in ["failed", "completed"]', context) is True
