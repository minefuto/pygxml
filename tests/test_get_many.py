"""Tests for get_many, get_many_bytes, get_many_buffer, and Result.get_many."""
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


# ---- edge cases ------------------------------------------------------------

def test_empty_paths_returns_empty_list():
    assert pygxml.get_many_bytes(BOOKS, []) == []


def test_single_path_parity_with_get():
    paths = [
        "store.book.0.title",
        "store.book.1.title",
        "store.book.2.title",
        "store.book.0.@id",
        "store.book.1.@id",
        "store.book.2.@id",
        "store.book.#",
        "store.book.0.price",
        "store.book.2.price",
        "store.magazine.title",
        "store.magazine.@id",
    ]
    for p in paths:
        single = pygxml.get_bytes(BOOKS, p)
        many = pygxml.get_many_bytes(BOOKS, [p])
        assert len(many) == 1
        assert str(single) == str(many[0]), f"Mismatch for path {p!r}"


def test_order_preserved():
    paths = ["store.book.0.title", "store.book.#", "store.book.1.@id"]
    rs = pygxml.get_many_bytes(BOOKS, paths)
    assert str(rs[0]) == "XML in a Nutshell"
    assert str(rs[1]) == "3"
    assert str(rs[2]) == "b2"


# ---- correctness -----------------------------------------------------------

def test_overlapping_prefixes():
    rs = pygxml.get_many_bytes(BOOKS, ["store.book.0.title", "store.book.0.price"])
    assert str(rs[0]) == "XML in a Nutshell"
    assert str(rs[1]) == "30"


def test_disjoint_subtrees():
    xml = b"<root><a><x>1</x></a><b><y>2</y></b></root>"
    rs = pygxml.get_many_bytes(xml, ["root.a.x", "root.b.y"])
    assert str(rs[0]) == "1"
    assert str(rs[1]) == "2"


def test_overlapping_element_and_attr():
    rs = pygxml.get_many_bytes(BOOKS, ["store.book.0.title", "store.book.0.@id"])
    assert str(rs[0]) == "XML in a Nutshell"
    assert str(rs[1]) == "b1"


def test_multi_match_path():
    rs = pygxml.get_many_bytes(BOOKS, ["store.book.#.title"])
    # Multi-mode: iterate yields single-item Results
    titles = [str(x) for x in rs[0]]
    assert titles == ["XML in a Nutshell", "The Cathedral and the Bazaar", "Programming Rust"]


def test_count_path():
    rs = pygxml.get_many_bytes(BOOKS, ["store.book.#", "store.magazine.#"])
    assert str(rs[0]) == "3"
    assert str(rs[1]) == "1"


def test_missing_path_returns_empty_result():
    rs = pygxml.get_many_bytes(BOOKS, ["store.cd.title", "store.book.99.title"])
    assert not rs[0].exists()
    assert not rs[1].exists()


def test_filter_paths():
    rs = pygxml.get_many_bytes(BOOKS, [
        "store.book.#(price>=30).title",
        'store.book.#(@id=="b2").title',
    ])
    assert str(rs[0]) == "XML in a Nutshell"
    assert str(rs[1]) == "The Cathedral and the Bazaar"


def test_filter_all_paths():
    rs = pygxml.get_many_bytes(BOOKS, [
        "store.book.#(price>=30)#.title",
        "store.book.#(price<30)#.title",
    ])
    # price>=30: 2 matches → Multi mode
    assert [str(x) for x in rs[0]] == ["XML in a Nutshell", "Programming Rust"]
    # price<30: 1 match → ScalarLike mode
    assert str(rs[1]) == "The Cathedral and the Bazaar"


def test_descendant_paths():
    rs = pygxml.get_many_bytes(NESTED, ["root.**.x"])
    assert [str(x) for x in rs[0]] == ["shallow", "deep", "direct"]


def test_two_descendant_paths():
    xml = b"<r><a><x>1</x></a><b><y>2</y></b><c><x>3</x></c></r>"
    rs = pygxml.get_many_bytes(xml, ["r.**.x", "r.**.y"])
    assert [str(x) for x in rs[0]] == ["1", "3"]
    assert str(rs[1]) == "2"


def test_modifier_paths_applied_per_program():
    rs = pygxml.get_many_bytes(BOOKS, [
        "store.book.#.title|@reverse",
        "store.book.#.@id|@count",
    ])
    titles = [str(x) for x in rs[0]]
    assert titles[0] == "Programming Rust"
    assert str(rs[1]) == "3"


def test_modifier_independence():
    rs = pygxml.get_many_bytes(BOOKS, [
        "store.book.#.title|@first",
        "store.book.#.title|@last",
    ])
    assert str(rs[0]) == "XML in a Nutshell"
    assert str(rs[1]) == "Programming Rust"


# ---- compiled Path objects ------------------------------------------------

def test_compiled_paths_accepted():
    c0 = pygxml.compile("store.book.0.title")
    c1 = pygxml.compile("store.book.#")
    rs = pygxml.get_many_bytes(BOOKS, [c0, "store.book.1.title", c1])
    assert str(rs[0]) == "XML in a Nutshell"
    assert str(rs[1]) == "The Cathedral and the Bazaar"
    assert str(rs[2]) == "3"


def test_mixed_str_and_path_objects():
    c = pygxml.compile("store.book.0.@id")
    rs = pygxml.get_many_bytes(BOOKS, ["store.book.0.title", c])
    assert str(rs[0]) == "XML in a Nutshell"
    assert str(rs[1]) == "b1"


# ---- error cases ----------------------------------------------------------

def test_invalid_path_raises_before_scan():
    with pytest.raises(ValueError):
        pygxml.get_many_bytes(BOOKS, ["store.book.0.title", "@"])


def test_ambiguity_propagates():
    with pytest.raises(ValueError, match="ambiguous"):
        pygxml.get_many_bytes(BOOKS, ["store.book.title"])


def test_unsupported_path_type_raises_typeerror():
    with pytest.raises(TypeError):
        pygxml.get_many_bytes(BOOKS, [123])


# ---- equivalence against individual get -----------------------------------

def test_equivalence_with_get():
    paths = [
        "store.book.0.title",
        "store.book.1.title",
        "store.book.2.title",
        "store.book.#.title",
        "store.book.0.@id",
        "store.book.1.@id",
        "store.book.2.@id",
        "store.book.#",
        "store.magazine.title",
        "store.magazine.@id",
        "store.book.#(price>25).title",
        "store.book.#(price>=30)#.title",
        "store.book.#.title|@reverse",
        "store.book.#.title|@count",
        "store.book.#.@id|@sort",
    ]
    individual = [pygxml.get_bytes(BOOKS, p) for p in paths]
    many = pygxml.get_many_bytes(BOOKS, paths)
    assert len(many) == len(individual)
    for p, exp, got in zip(paths, individual, many):
        assert exp == got, f"Mismatch for {p!r}: expected {exp!r}, got {got!r}"


# ---- get_many (str input) -------------------------------------------------

def test_get_many_str_basic():
    xml = BOOKS.decode("utf-8")
    rs = pygxml.get_many(xml, ["store.book.0.title", "store.book.#"])
    assert str(rs[0]) == "XML in a Nutshell"
    assert str(rs[1]) == "3"


def test_get_many_str_empty():
    assert pygxml.get_many(BOOKS.decode("utf-8"), []) == []


# ---- get_many_buffer (buffer input) ---------------------------------------

def test_get_many_buffer_bytearray():
    ba = bytearray(BOOKS)
    rs = pygxml.get_many_buffer(ba, ["store.book.0.title", "store.book.#"])
    assert str(rs[0]) == "XML in a Nutshell"
    assert str(rs[1]) == "3"


# ---- Result.get_many -------------------------------------------------------

def test_result_get_many_basic():
    book = pygxml.get_bytes(BOOKS, "store.book.0")
    rs = book.get_many(["title", "@id", "price"])
    assert str(rs[0]) == "XML in a Nutshell"
    assert str(rs[1]) == "b1"
    assert str(rs[2]) == "30"


def test_result_get_many_parse():
    r = pygxml.parse(BOOKS)
    rs = r.get_many(["store.book.0.title", "store.book.#"])
    assert str(rs[0]) == "XML in a Nutshell"
    assert str(rs[1]) == "3"


def test_result_get_many_empty_paths():
    book = pygxml.get_bytes(BOOKS, "store.book.0")
    assert book.get_many([]) == []


def test_result_get_many_rejects_multi():
    multi = pygxml.get_bytes(BOOKS, "store.book")
    with pytest.raises(ValueError, match="multi-element"):
        multi.get_many(["title"])


def test_result_get_many_parity_with_get():
    book = pygxml.get_bytes(BOOKS, "store.book.1")
    paths = ["title", "@id", "price"]
    individual = [book.get(p) for p in paths]
    many = book.get_many(paths)
    for p, exp, got in zip(paths, individual, many):
        assert str(exp) == str(got), f"Mismatch for {p!r}"
