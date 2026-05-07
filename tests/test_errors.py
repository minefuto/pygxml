"""Tests for path compilation errors: messages, caret pointers, validation."""
from pathlib import Path

import pytest

import pygxml

DATA = Path(__file__).parent / "data"
BOOKS = (DATA / "books.xml").read_bytes()


# ---- basic compile errors ---------------------------------------------------

def test_unmatched_paren_raises():
    with pytest.raises(ValueError):
        pygxml.compile("store.book.#(price>10")


def test_unknown_modifier_raises():
    with pytest.raises(ValueError):
        pygxml.compile("store.book.#.title|@bogus")


def test_attr_not_terminal_raises():
    with pytest.raises(ValueError):
        pygxml.compile("a.@id.b")


# ---- error includes path string ---------------------------------------------

def test_error_includes_path_string():
    with pytest.raises(ValueError) as exc:
        pygxml.compile("a.@id.b")
    assert "a.@id.b" in str(exc.value)


def test_error_includes_path_string_for_paren():
    with pytest.raises(ValueError) as exc:
        pygxml.compile("store.book.#(price>10")
    assert "store.book.#(price>10" in str(exc.value)


def test_error_unknown_modifier_message_helpful():
    with pytest.raises(ValueError) as exc:
        pygxml.compile("a|@bogus")
    msg = str(exc.value)
    assert "bogus" in msg
    assert "a|@bogus" in msg


# ---- caret pointer in error messages ----------------------------------------

def test_error_caret_for_unmatched_paren():
    with pytest.raises(ValueError) as exc:
        pygxml.compile("store.book.#(price>10")
    msg = str(exc.value)
    assert "in path:" in msg
    lines = msg.split("\n")
    assert any(line.lstrip().startswith("^") for line in lines)


def test_error_caret_for_unknown_modifier():
    with pytest.raises(ValueError) as exc:
        pygxml.compile("a|@nonsense")
    msg = str(exc.value)
    assert "@nonsense" in msg
    assert "^" in msg


def test_error_caret_for_attr_not_terminal():
    with pytest.raises(ValueError) as exc:
        pygxml.compile("a.@id.b")
    msg = str(exc.value)
    assert "in path:" in msg
    assert "^" in msg


# ---- ambiguity (Auto-with-rest + multi-match) -------------------------------

def test_ambiguous_chain_with_child_step():
    with pytest.raises(ValueError) as exc:
        pygxml.get_bytes(BOOKS, "store.book.title")
    assert "book" in str(exc.value)


def test_ambiguous_chain_with_attribute_terminal():
    with pytest.raises(ValueError) as exc:
        pygxml.get_bytes(BOOKS, "store.book.@id")
    assert "book" in str(exc.value)


def test_ambiguous_chain_with_text_terminal():
    with pytest.raises(ValueError):
        pygxml.get_bytes(BOOKS, "store.book.#text")


def test_compiled_path_propagates_ambiguity():
    p = pygxml.compile("store.book.title")
    with pytest.raises(ValueError):
        p.get_bytes(BOOKS)


def test_ambiguity_inside_filter_predicate():
    xml = b"""<root>
        <a><b><c>1</c></b><b><c>2</c></b></a>
        <a><b><c>9</c></b></a>
    </root>"""
    with pytest.raises(ValueError):
        pygxml.get_bytes(xml, "root.a.#(b.c>0).x")


# ---- Result.get on multi-element receiver -----------------------------------

def test_result_get_on_multi_raises():
    multi = pygxml.get_bytes(BOOKS, "store.book")
    assert len(multi) == 3
    with pytest.raises(ValueError):
        multi.get("title")


def test_result_get_on_multi_via_parse_raises():
    books = pygxml.parse(BOOKS).get("store.book")
    with pytest.raises(ValueError):
        books.get("@id")


def test_result_get_after_indexing_works():
    book = pygxml.get_bytes(BOOKS, "store.book")[0]
    assert str(book.get("title")) == "XML in a Nutshell"


def test_result_get_via_iteration_works():
    titles = [str(b.get("title")) for b in pygxml.get_bytes(BOOKS, "store.book")]
    assert titles == [
        "XML in a Nutshell",
        "The Cathedral and the Bazaar",
        "Programming Rust",
    ]
