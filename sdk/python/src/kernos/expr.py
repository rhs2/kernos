"""A small evaluator for the policy expression grammar of 04-POLICY.

The evaluation harness uses it for ``assert`` lines over ``{steps, run, input}``.
Supported: ``or``, ``and``, ``not``, the comparisons ``== != < <= > >= in``,
``+`` and ``-``, unary minus, numbers, strings (double quotes per the grammar,
single quotes accepted as well), ``true``, ``false``, ``null``, lists, dotted
paths (numeric segments accepted for list indices) and calls resolved against a
mapping of functions.

Semantics follow 04-POLICY: ordered comparisons between different types are
false, ``in`` is list membership, arithmetic on non-numbers is ``null``, a
missing path is ``null``, and ``and``/``or`` short-circuit treating ``null``
as false.
"""

from __future__ import annotations

import re
from collections.abc import Callable, Mapping
from dataclasses import dataclass
from typing import Any

__all__ = ["ExprError", "evaluate", "is_truthy", "parse"]

_TOKEN_RE = re.compile(
    r"""
    (?P<ws>\s+|\#[^\n]*)
    |(?P<num>\d+(?:\.\d+)?)
    |(?P<str>"(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*')
    |(?P<ident>[A-Za-z_][A-Za-z0-9_]*)
    |(?P<op>==|!=|<=|>=|<|>|\+|-|\(|\)|\[|\]|,|\.)
    """,
    re.VERBOSE,
)
_KEYWORDS = {"and", "or", "not", "in", "true", "false", "null"}
_COMPARISONS = {"==", "!=", "<", "<=", ">", ">=", "in"}

Functions = Mapping[str, Callable[..., Any]]


class ExprError(ValueError):
    """The expression could not be parsed or names an unknown function."""


@dataclass(frozen=True)
class _Token:
    kind: str
    value: Any
    pos: int


def _unescape(text: str) -> str:
    return text[1:-1].replace('\\"', '"').replace("\\'", "'").replace("\\\\", "\\")


def _tokenize(source: str) -> list[_Token]:
    tokens: list[_Token] = []
    pos = 0
    while pos < len(source):
        match = _TOKEN_RE.match(source, pos)
        if match is None:
            raise ExprError(f"unexpected character {source[pos]!r} at {pos}")
        pos = match.end()
        kind = match.lastgroup or ""
        if kind == "ws":
            continue
        text = match.group(0)
        if kind == "num":
            tokens.append(_Token("num", float(text) if "." in text else int(text), match.start()))
        elif kind == "str":
            tokens.append(_Token("str", _unescape(text), match.start()))
        elif kind == "ident":
            tokens.append(_Token("kw" if text in _KEYWORDS else "ident", text, match.start()))
        else:
            tokens.append(_Token("op", text, match.start()))
    tokens.append(_Token("eof", None, len(source)))
    return tokens


class _Parser:
    """Recursive descent over the grammar; nodes are plain tuples."""

    def __init__(self, source: str) -> None:
        self.tokens = _tokenize(source)
        self.index = 0

    def peek(self) -> _Token:
        return self.tokens[self.index]

    def advance(self) -> _Token:
        token = self.tokens[self.index]
        self.index += 1
        return token

    def accept(self, kind: str, value: Any = None) -> _Token | None:
        token = self.peek()
        if token.kind == kind and (value is None or token.value == value):
            return self.advance()
        return None

    def expect(self, kind: str, value: Any = None) -> _Token:
        token = self.accept(kind, value)
        if token is None:
            found = self.peek()
            wanted = value if value is not None else kind
            raise ExprError(f"expected {wanted!r} at {found.pos}, found {found.value!r}")
        return token

    def parse(self) -> Any:
        node = self.parse_or()
        if self.peek().kind != "eof":
            token = self.peek()
            raise ExprError(f"unexpected {token.value!r} at {token.pos}")
        return node

    def parse_or(self) -> Any:
        node = self.parse_and()
        while self.accept("kw", "or"):
            node = ("or", node, self.parse_and())
        return node

    def parse_and(self) -> Any:
        node = self.parse_not()
        while self.accept("kw", "and"):
            node = ("and", node, self.parse_not())
        return node

    def parse_not(self) -> Any:
        if self.accept("kw", "not"):
            return ("not", self.parse_not())
        return self.parse_cmp()

    def parse_cmp(self) -> Any:
        left = self.parse_sum()
        token = self.peek()
        if (token.kind == "op" or token.kind == "kw") and token.value in _COMPARISONS:
            self.advance()
            return ("cmp", token.value, left, self.parse_sum())
        return left

    def parse_sum(self) -> Any:
        node = self.parse_unary()
        while True:
            if self.accept("op", "+"):
                node = ("add", node, self.parse_unary())
            elif self.accept("op", "-"):
                node = ("sub", node, self.parse_unary())
            else:
                return node

    def parse_unary(self) -> Any:
        if self.accept("op", "-"):
            return ("neg", self.parse_unary())
        return self.parse_primary()

    def parse_primary(self) -> Any:
        token = self.peek()
        if token.kind == "num":
            return ("lit", self.advance().value)
        if token.kind == "str":
            return ("lit", self.advance().value)
        if token.kind == "kw" and token.value in ("true", "false", "null"):
            self.advance()
            return ("lit", {"true": True, "false": False, "null": None}[token.value])
        if self.accept("op", "("):
            node = self.parse_or()
            self.expect("op", ")")
            return node
        if self.accept("op", "["):
            items: list[Any] = []
            if not self.accept("op", "]"):
                items.append(self.parse_or())
                while self.accept("op", ","):
                    items.append(self.parse_or())
                self.expect("op", "]")
            return ("list", items)
        if token.kind == "ident":
            return self.parse_path_or_call()
        raise ExprError(f"unexpected {token.value!r} at {token.pos}")

    def parse_path_or_call(self) -> Any:
        segments = [str(self.expect("ident").value)]
        while self.accept("op", "."):
            segment = self.peek()
            is_index = segment.kind == "num" and isinstance(segment.value, int)
            if segment.kind in ("ident", "kw") or is_index:
                segments.append(str(self.advance().value))
            else:
                raise ExprError(f"bad path segment at {segment.pos}")
        if self.accept("op", "("):
            args: list[Any] = []
            if not self.accept("op", ")"):
                args.append(self.parse_or())
                while self.accept("op", ","):
                    args.append(self.parse_or())
                self.expect("op", ")")
            return ("call", segments, args)
        return ("path", segments)


def parse(source: str) -> Any:
    """Parse ``source`` into a node tree; raises :class:`ExprError` on bad syntax."""
    return _Parser(source).parse()


def _is_number(value: Any) -> bool:
    return isinstance(value, (int, float)) and not isinstance(value, bool)


def _equal(left: Any, right: Any) -> bool:
    if _is_number(left) and _is_number(right):
        return bool(left == right)
    if isinstance(left, bool) or isinstance(right, bool):
        return isinstance(left, bool) and isinstance(right, bool) and left == right
    return bool(left == right)


def is_truthy(value: Any) -> bool:
    """Truth per 04-POLICY: ``null`` is false, otherwise the natural truth of the value."""
    if value is None:
        return False
    if isinstance(value, bool):
        return value
    return bool(value)


def _resolve_path(context: Mapping[str, Any], segments: list[str]) -> Any:
    current: Any = context
    for segment in segments:
        if isinstance(current, Mapping) and segment in current:
            current = current[segment]
        elif isinstance(current, list) and segment.isdigit() and int(segment) < len(current):
            current = current[int(segment)]
        else:
            return None
    return current


def _compare(op: str, left: Any, right: Any) -> bool:
    if op == "==":
        return _equal(left, right)
    if op == "!=":
        return not _equal(left, right)
    if op == "in":
        return isinstance(right, list) and any(_equal(left, item) for item in right)
    both_numbers = _is_number(left) and _is_number(right)
    both_strings = isinstance(left, str) and isinstance(right, str)
    if not (both_numbers or both_strings):
        return False
    if op == "<":
        return bool(left < right)
    if op == "<=":
        return bool(left <= right)
    if op == ">":
        return bool(left > right)
    return bool(left >= right)


def _evaluate(node: Any, context: Mapping[str, Any], functions: Functions) -> Any:
    kind = node[0]
    if kind == "lit":
        return node[1]
    if kind == "list":
        return [_evaluate(item, context, functions) for item in node[1]]
    if kind == "path":
        return _resolve_path(context, node[1])
    if kind == "call":
        name = ".".join(node[1])
        function = functions.get(name)
        if function is None:
            raise ExprError(f"unknown function {name!r}")
        return function(*[_evaluate(arg, context, functions) for arg in node[2]])
    if kind == "not":
        return not is_truthy(_evaluate(node[1], context, functions))
    if kind == "and":
        left = _evaluate(node[1], context, functions)
        return is_truthy(left) and is_truthy(_evaluate(node[2], context, functions))
    if kind == "or":
        left = _evaluate(node[1], context, functions)
        return is_truthy(left) or is_truthy(_evaluate(node[2], context, functions))
    if kind == "cmp":
        left = _evaluate(node[2], context, functions)
        right = _evaluate(node[3], context, functions)
        return _compare(node[1], left, right)
    if kind in ("add", "sub"):
        left = _evaluate(node[1], context, functions)
        right = _evaluate(node[2], context, functions)
        if not (_is_number(left) and _is_number(right)):
            return None
        return left + right if kind == "add" else left - right
    if kind == "neg":
        value = _evaluate(node[1], context, functions)
        return -value if _is_number(value) else None
    raise ExprError(f"unknown node {kind!r}")


def _default_functions() -> dict[str, Callable[..., Any]]:
    return {
        "len": lambda value: len(value) if isinstance(value, (list, str, Mapping)) else None,
    }


def evaluate(source: str, context: Mapping[str, Any], functions: Functions | None = None) -> Any:
    """Evaluate ``source`` against ``context``.

    ``functions`` maps dotted call names to callables; ``len`` is always
    available. Unknown functions raise :class:`ExprError` so a typo in an
    assertion is loud rather than silently false.
    """
    available = _default_functions()
    available.update(functions or {})
    return _evaluate(parse(source), context, available)
