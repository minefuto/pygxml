"""Tests for the descendant `**` path syntax."""
from pathlib import Path

import pytest

import pygxml

DATA = Path(__file__).parent / "data"
BOOKS = (DATA / "books.xml").read_bytes()

NESTED = b"""<root>
    <a><b><c><x>shallow</x></c></b></a>
    <m><n><o><p><x>deep</x></p></o></n></m>
    <q><x>direct</x></q>
</root>"""


def test_descendant_collects_all_matches():
    # `**` always returns every match at any depth (DFS order).
    assert [str(it) for it in pygxml.get_bytes(NESTED, "root.**.x")] == ["shallow", "deep", "direct"]


def test_descendant_collects_when_nested():
    xml = b"<root><wrap><wrap><target>X</target></wrap></wrap></root>"
    assert str(pygxml.get_bytes(xml, "root.**.target")) == "X"


def test_descendant_with_attr_terminal_single():
    xml = b'<root><a><b><c id="42"/></b></a></root>'
    assert str(pygxml.get_bytes(xml, "root.**.c.@id")) == "42"


def test_descendant_with_attr_terminal_multi():
    # Multiple matches across depths — descendant projects @id over each.
    xml = b'<root><a><c id="X"/></a><b><c id="Y"/></b></root>'
    assert [str(it) for it in pygxml.get_bytes(xml, "root.**.c.@id")] == ["X", "Y"]


def test_descendant_with_text_terminal_explicit():
    xml = b"<root><a><b>val</b></a></root>"
    assert str(pygxml.get_bytes(xml, "root.**.b.#text")) == "val"


def test_descendant_no_match_returns_empty():
    assert not pygxml.get_bytes(NESTED, "root.**.nope").exists()
    assert [str(it) for it in pygxml.get_bytes(NESTED, "root.**.nope")] == []


def test_descendant_at_top_level_in_books():
    # 3 <book> + 1 <magazine>, each with one <title> → 4 titles.
    assert [str(it) for it in pygxml.get_bytes(BOOKS, "store.**.title")] == [
        "XML in a Nutshell",
        "The Cathedral and the Bazaar",
        "Programming Rust",
        "Linux Journal",
    ]


def test_descendant_collects_unrelated_subtrees():
    xml = b"""<root>
        <wrap1><deep><target>A</target></deep></wrap1>
        <wrap2><target>B</target></wrap2>
    </root>"""
    assert [str(it) for it in pygxml.get_bytes(xml, "root.**.target")] == ["A", "B"]


def test_descendant_with_wildcard_glob():
    xml = b"<root><a><b><foobar>F</foobar></b></a></root>"
    assert str(pygxml.get_bytes(xml, "root.**.foo*")) == "F"


def test_descendant_error_without_following_name():
    with pytest.raises(ValueError) as exc:
        pygxml.compile("a.**")
    assert "**" in str(exc.value)


def test_descendant_error_with_index():
    with pytest.raises(ValueError) as exc:
        pygxml.compile("a.**.b.0")
    assert "**" in str(exc.value)
