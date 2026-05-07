"""Tests for all path modifiers: order, selection, sort, dedup, aggregation."""
from pathlib import Path

import pygxml

DATA = Path(__file__).parent / "data"
BOOKS = (DATA / "books.xml").read_bytes()


# ---- order / selection modifiers -------------------------------------------

def test_modifier_reverse():
    titles = [str(it) for it in pygxml.get_bytes(BOOKS, "store.book.#.title|@reverse")]
    assert titles == [
        "Programming Rust",
        "The Cathedral and the Bazaar",
        "XML in a Nutshell",
    ]


def test_modifier_first():
    r = pygxml.get_bytes(BOOKS, "store.book.#.title|@first")
    assert str(r) == "XML in a Nutshell"


def test_modifier_last():
    r = pygxml.get_bytes(BOOKS, "store.book.#.title|@last")
    assert str(r) == "Programming Rust"


def test_modifier_count():
    n = str(pygxml.get_bytes(BOOKS, "store.book.#.title|@count"))
    assert n == "3"


def test_modifier_chain():
    r = pygxml.get_bytes(BOOKS, "store.book.#.title|@reverse|@first")
    assert str(r) == "Programming Rust"


# ---- sort modifiers ---------------------------------------------------------

def test_modifier_sort_lexicographic():
    titles = [str(it) for it in pygxml.get_bytes(BOOKS, "store.book.#.title|@sort")]
    assert titles == [
        "Programming Rust",
        "The Cathedral and the Bazaar",
        "XML in a Nutshell",
    ]


def test_modifier_sort_numeric():
    prices = [str(it) for it in pygxml.get_bytes(BOOKS, "store.book.#.price|@sort_n")]
    assert prices == ["20", "30", "45"]


def test_modifier_sort_then_first():
    smallest = str(pygxml.get_bytes(BOOKS, "store.book.#.price|@sort_n|@first"))
    assert smallest == "20"


# ---- deduplication ----------------------------------------------------------

def test_modifier_unique():
    xml = b"<root><a>x</a><a>y</a><a>x</a><a>z</a><a>y</a></root>"
    assert [str(it) for it in pygxml.get_bytes(xml, "root.a.#.#text|@unique")] == ["x", "y", "z"]


def test_modifier_unique_alias_uniq():
    xml = b"<root><a>x</a><a>x</a></root>"
    assert [str(it) for it in pygxml.get_bytes(xml, "root.a.#.#text|@uniq")] == ["x"]


# ---- no-op modifiers (gjson compatibility) ----------------------------------

def test_modifier_flatten_is_noop():
    titles = [str(it) for it in pygxml.get_bytes(BOOKS, "store.book.#.title|@flatten")]
    assert titles == [
        "XML in a Nutshell",
        "The Cathedral and the Bazaar",
        "Programming Rust",
    ]


def test_modifier_tostr_is_noop():
    titles = [str(it) for it in pygxml.get_bytes(BOOKS, "store.book.#.title|@tostr")]
    assert titles[0] == "XML in a Nutshell"


# ---- aggregation modifiers --------------------------------------------------

def test_modifier_sum():
    # books prices: 30, 20, 45 → sum 95
    assert str(pygxml.get_bytes(BOOKS, "store.book.#.price|@sum")) == "95"


def test_modifier_avg():
    out = pygxml.get_bytes(BOOKS, "store.book.#.price|@avg")
    assert out.exists()
    assert abs(float(out) - (95 / 3)) < 1e-6


def test_modifier_avg_alias_mean():
    out = pygxml.get_bytes(BOOKS, "store.book.#.price|@mean")
    assert out.exists() and abs(float(out) - (95 / 3)) < 1e-6


def test_modifier_min_max():
    assert str(pygxml.get_bytes(BOOKS, "store.book.#.price|@min")) == "20"
    assert str(pygxml.get_bytes(BOOKS, "store.book.#.price|@max")) == "45"


def test_modifier_sum_with_filter():
    # Sum of prices for books >= 30: 30 + 45 = 75
    s = str(pygxml.get_bytes(BOOKS, "store.book.#(price>=30)#.price|@sum"))
    assert s == "75"


def test_modifier_sum_empty_is_zero():
    s = str(pygxml.get_bytes(BOOKS, "store.book.#(price>9999)#.price|@sum"))
    assert s == "0"


def test_modifier_min_ignores_non_numeric():
    xml = b"<root><x>5</x><x>not-a-number</x><x>2</x></root>"
    assert str(pygxml.get_bytes(xml, "root.x.#.#text|@min")) == "2"
    assert str(pygxml.get_bytes(xml, "root.x.#.#text|@max")) == "5"


def test_modifier_chain_sort_then_first():
    smallest = str(pygxml.get_bytes(BOOKS, "store.book.#.price|@sort_n|@first"))
    assert smallest == "20"
