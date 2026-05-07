"""Tests for filter expressions: numeric, string, glob, and fast paths."""
from pathlib import Path

import pytest
from lxml import etree

import pygxml

DATA = Path(__file__).parent / "data"
BOOKS = (DATA / "books.xml").read_bytes()


# ---- filter: numeric comparison ---------------------------------------------

def test_filter_first_gt():
    assert str(pygxml.get_bytes(BOOKS, "store.book.#(price>25).title")) == "XML in a Nutshell"


def test_filter_first_le():
    assert str(pygxml.get_bytes(BOOKS, "store.book.#(price<=20).title")) == "The Cathedral and the Bazaar"


def test_filter_all_ge():
    titles = [str(it) for it in pygxml.get_bytes(BOOKS, "store.book.#(price>=30)#.title")]
    assert titles == ["XML in a Nutshell", "Programming Rust"]


def test_filter_no_match_is_empty():
    assert not pygxml.get_bytes(BOOKS, "store.book.#(price>9999).title").exists()
    assert [str(it) for it in pygxml.get_bytes(BOOKS, "store.book.#(price>9999)#.title")] == []


# ---- filter: string comparison ----------------------------------------------

def test_filter_attribute_eq():
    title = str(pygxml.get_bytes(BOOKS, 'store.book.#(@id=="b2").title'))
    assert title == "The Cathedral and the Bazaar"


def test_filter_string_ne():
    titles = [str(it) for it in pygxml.get_bytes(BOOKS, 'store.book.#(@id!="b1")#.title')]
    assert titles == ["The Cathedral and the Bazaar", "Programming Rust"]


def test_filter_child_text_eq():
    author = str(pygxml.get_bytes(BOOKS, 'store.book.#(author=="Eric S. Raymond").title'))
    assert author == "The Cathedral and the Bazaar"


# ---- filter: glob -----------------------------------------------------------

def test_filter_glob_match():
    r = pygxml.get_bytes(BOOKS, 'store.book.#(title%"Programming*")#.@id')
    assert str(r) == "b3"


def test_filter_glob_not_match():
    ids = [str(it) for it in pygxml.get_bytes(BOOKS, 'store.book.#(title!%"Programming*")#.@id')]
    assert ids == ["b1", "b2"]


# ---- attribute-filter fast path ---------------------------------------------

def test_attr_filter_fast_path_eq():
    title = str(pygxml.get_bytes(BOOKS, 'store.book.#(@id=="b3").title'))
    assert title == "Programming Rust"


def test_attr_filter_fast_path_glob():
    titles = [str(it) for it in pygxml.get_bytes(BOOKS, 'store.book.#(@id%"b?")#.title')]
    assert titles == [
        "XML in a Nutshell",
        "The Cathedral and the Bazaar",
        "Programming Rust",
    ]


def test_attr_filter_fast_path_no_match():
    assert not pygxml.get_bytes(BOOKS, 'store.book.#(@id=="zzz").title').exists()


# ---- child-element filter fast path -----------------------------------------

def test_child_filter_fast_path_eq():
    assert str(pygxml.get_bytes(BOOKS, "store.book.#(price==30).title")) == "XML in a Nutshell"


def test_child_filter_fast_path_lt():
    assert str(pygxml.get_bytes(BOOKS, "store.book.#(price<25).title")) == "The Cathedral and the Bazaar"


def test_child_filter_fast_path_glob():
    r = pygxml.get_bytes(BOOKS, 'store.book.#(title%"Programming*")#.title')
    assert str(r) == "Programming Rust"


def test_child_filter_fast_path_no_match():
    assert not pygxml.get_bytes(BOOKS, "store.book.#(price>9999).title").exists()


def test_child_filter_fast_path_missing_predicate_child_misses():
    xml = b"""<store>
        <book><title>Has price</title><price>10</price></book>
        <book><title>Missing price</title></book>
    </store>"""
    r = pygxml.get_bytes(xml, "store.book.#(price>0)#.title")
    assert str(r) == "Has price"


# ---- descendant + filter not supported --------------------------------------

def test_descendant_with_filter_not_supported():
    with pytest.raises(ValueError):
        pygxml.compile("a.**.b.#(x>0)")


# ---- cross-check with lxml --------------------------------------------------

def test_filter_matches_lxml_xpath():
    tree = etree.fromstring(BOOKS)

    expected_gt30 = tree.xpath("//book[number(price/text())>=30]/title/text()")
    assert [str(it) for it in pygxml.get_bytes(BOOKS, "store.book.#(price>=30)#.title")] == list(expected_gt30)

    expected_b2 = tree.xpath('//book[@id="b2"]/title/text()')
    assert [str(pygxml.get_bytes(BOOKS, 'store.book.#(@id=="b2").title'))] == list(expected_b2)
