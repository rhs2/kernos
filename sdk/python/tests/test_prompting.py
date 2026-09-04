"""Stable prefix construction and hashes."""

from __future__ import annotations

from kernos.prompting import build_prefix, input_hash, prefix_hash, tools_block

TOOLS = [
    {"id": "ledger.void_entry", "description": "Void", "writes": True},
    {"id": "ledger.post_entry", "description": "Post", "writes": True, "input_schema": {}},
]
SCHEMA = {"type": "object", "properties": {"vendor": {"type": "string"}}}


def test_prefix_hash_ignores_user_content() -> None:
    prefix = build_prefix("You extract invoices.", TOOLS, SCHEMA)
    assert prefix_hash(prefix) == prefix_hash(build_prefix("You extract invoices.", TOOLS, SCHEMA))
    assert input_hash("invoice one") != input_hash("invoice two")


def test_tools_are_sorted_by_id_and_normalised() -> None:
    reordered = list(reversed(TOOLS))
    assert tools_block(TOOLS) == tools_block(reordered)
    assert tools_block(TOOLS) == (
        'Tools available: [{"description":"Post","id":"ledger.post_entry","writes":true},'
        '{"description":"Void","id":"ledger.void_entry","writes":true}]'
    )


def test_prefix_layout_is_system_then_tools_then_schema() -> None:
    prefix = build_prefix("System text.\n", TOOLS, SCHEMA)
    parts = prefix.split("\n\n")
    assert parts[0] == "System text."
    assert parts[1].startswith("Tools available: ")
    assert parts[2] == 'Output schema: {"properties":{"vendor":{"type":"string"}},"type":"object"}'


def test_different_tools_or_schema_change_the_hash() -> None:
    base = prefix_hash(build_prefix("S", TOOLS, SCHEMA))
    assert prefix_hash(build_prefix("S", TOOLS[:1], SCHEMA)) != base
    assert prefix_hash(build_prefix("S", TOOLS, None)) != base
    assert prefix_hash(build_prefix("S", TOOLS, None)) == prefix_hash(build_prefix("S", TOOLS))


def test_hashes_are_sha256_hex() -> None:
    assert len(prefix_hash("x")) == 64
    assert prefix_hash("x") == "2d711642b726b04401627ca9fbac32f5c8530fb1903cc4db02258717921a4881"
