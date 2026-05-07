"""Tests for basic element navigation: descend, index, count, wildcards, compiled path."""
from pathlib import Path

import pytest
from lxml import etree

import pygxml

DATA = Path(__file__).parent / "data"
BOOKS = (DATA / "books.xml").read_bytes()


# ---- single value (get) -----------------------------------------------------

def test_descend_terminal_returns_all_matches():
    # Implicit selector at terminal returns every match (multi).
    r = pygxml.get_bytes(BOOKS, "store.book")
    assert len(r) == 3
    assert r.type_ is list


def test_descend_with_chain_requires_explicit_selector():
    # Multiple <book> siblings + chained step is ambiguous → ValueError.
    with pytest.raises(ValueError):
        pygxml.get_bytes(BOOKS, "store.book.title")
    with pytest.raises(ValueError):
        pygxml.get_bytes(BOOKS, "store.book.@id")


def test_descend_single_match_chains_implicitly():
    # `magazine` has exactly one occurrence; the engine descends silently.
    assert str(pygxml.get_bytes(BOOKS, "store.magazine.title")) == "Linux Journal"
    assert str(pygxml.get_bytes(BOOKS, "store.magazine.@id")) == "m1"


def test_descend_indexed():
    assert str(pygxml.get_bytes(BOOKS, "store.book.0.title")) == "XML in a Nutshell"
    assert str(pygxml.get_bytes(BOOKS, "store.book.1.title")) == "The Cathedral and the Bazaar"
    assert str(pygxml.get_bytes(BOOKS, "store.book.2.author")) == "Jim Blandy"


def test_attribute_terminal():
    assert str(pygxml.get_bytes(BOOKS, "store.book.0.@id")) == "b1"
    assert str(pygxml.get_bytes(BOOKS, "store.book.1.@id")) == "b2"
    assert str(pygxml.get_bytes(BOOKS, "store.magazine.@id")) == "m1"


def test_count_terminal():
    assert str(pygxml.get_bytes(BOOKS, "store.book.#")) == "3"
    assert str(pygxml.get_bytes(BOOKS, "store.magazine.#")) == "1"


def test_missing_returns_empty_result():
    assert not pygxml.get_bytes(BOOKS, "store.cd.title").exists()
    assert not pygxml.get_bytes(BOOKS, "store.book.99.title").exists()
    assert not pygxml.get_bytes(BOOKS, "missing").exists()


# ---- multi value (iteration) ------------------------------------------------

def test_iter_each_text():
    titles = [str(it) for it in pygxml.get_bytes(BOOKS, "store.book.#.title")]
    assert titles == [
        "XML in a Nutshell",
        "The Cathedral and the Bazaar",
        "Programming Rust",
    ]


def test_iter_each_attr():
    ids = [str(it) for it in pygxml.get_bytes(BOOKS, "store.book.#.@id")]
    assert ids == ["b1", "b2", "b3"]


def test_iter_missing_returns_empty():
    assert [str(it) for it in pygxml.get_bytes(BOOKS, "store.cd.#.title")] == []


# ---- explicit text terminal -------------------------------------------------

def test_explicit_text_terminal():
    assert str(pygxml.get_bytes(BOOKS, "store.book.0.title.#text")) == "XML in a Nutshell"


# ---- wildcards --------------------------------------------------------------

def test_wildcard_star():
    # `b*k` matches `book` (3 siblings); `.0` resolves the chain.
    assert str(pygxml.get_bytes(BOOKS, "store.b*k.0.title")) == "XML in a Nutshell"


def test_wildcard_question():
    assert str(pygxml.get_bytes(BOOKS, "store.b??k.0.title")) == "XML in a Nutshell"


# ---- compiled path ----------------------------------------------------------

def test_compiled_path_reuse():
    p = pygxml.compile("store.book.#.title")
    assert [str(it) for it in p.get_bytes(BOOKS)] == [
        "XML in a Nutshell",
        "The Cathedral and the Bazaar",
        "Programming Rust",
    ]
    other = b"<store><book><title>Other</title></book></store>"
    assert str(p.get_bytes(other)) == "Other"


def test_compile_invalid_path_raises():
    with pytest.raises(ValueError):
        pygxml.compile("a.@id.b")  # @attr must be terminal


# ---- entity / CDATA --------------------------------------------------------

def test_entity_decoding():
    xml = b"<root><a>foo &amp; bar &lt;1&gt;</a></root>"
    assert str(pygxml.get_bytes(xml, "root.a")) == "foo & bar <1>"


def test_cdata_passthrough():
    xml = b"<root><a><![CDATA[<inner attr=\"x\"/>]]></a></root>"
    assert str(pygxml.get_bytes(xml, "root.a")) == '<inner attr="x"/>'


def test_attribute_entity():
    xml = b'<root><a id="x &amp; y"/></root>'
    assert str(pygxml.get_bytes(xml, "root.a.@id")) == "x & y"


# ---- empty element ---------------------------------------------------------

def test_empty_element_text_is_empty():
    xml = b'<root><a id="x"/></root>'
    assert str(pygxml.get_bytes(xml, "root.a")) == ""
    assert str(pygxml.get_bytes(xml, "root.a.@id")) == "x"


# ---- escapes ---------------------------------------------------------------

def test_escape_dot_in_name():
    xml = b'<root><my.tag>value</my.tag></root>'
    assert str(pygxml.get_bytes(xml, "root.my\\.tag")) == "value"



# ---- equivalence with lxml.etree (XPath analogs) ----------------------------

def test_matches_lxml_xpath():
    tree = etree.fromstring(BOOKS)
    expected = tree.xpath("//book[1]/title/text()")[0]
    assert str(pygxml.get_bytes(BOOKS, "store.book.0.title")) == expected

    expected_ids = tree.xpath("//book/@id")
    assert [str(it) for it in pygxml.get_bytes(BOOKS, "store.book.#.@id")] == list(expected_ids)
