"""Tests for the Result type API: accessors, chaining, equality, and type_."""
from pathlib import Path

import pytest
import pygxml

DATA = Path(__file__).parent / "data"
BOOKS = (DATA / "books.xml").read_bytes()


# ---- typed accessors --------------------------------------------------------

def test_result_str_and_typed():
    r = pygxml.parse(BOOKS).get("store.book.0.price")
    assert r.exists()
    assert str(r) == "30"
    assert int(r) == 30
    assert float(r) == 30.0
    assert bool(r) is True


def test_result_missing_returns_zero_and_empty():
    r = pygxml.parse(BOOKS).get("store.nope")
    assert not r.exists()
    assert str(r) == "None"   # str(None) - Python standard
    assert r.to_int() == 0
    assert r.to_float() == 0.0
    assert bool(r) is False


def test_result_bool_checks_value():
    xml = b"<root><flag>true</flag><off>false</off><n>1</n></root>"
    # bool(r) is bool(value), non-empty strings are truthy
    assert bool(pygxml.parse(xml).get("root.flag")) is True
    assert bool(pygxml.parse(xml).get("root.off")) is True
    assert bool(pygxml.parse(xml).get("root.missing")) is False


def test_result_bool_zero_value_is_falsy():
    xml = b"<root><n>0</n></root>"
    r = pygxml.get_bytes(xml, "root.n")
    assert r.exists() is True   # item exists
    assert bool(r) is False     # bool(0) is False


# ---- list / iter / index ----------------------------------------------------

def test_result_iter_multi():
    r = pygxml.parse(BOOKS).get("store.book.#.title")
    assert len(r) == 3
    assert [str(it) for it in r] == [
        "XML in a Nutshell",
        "The Cathedral and the Bazaar",
        "Programming Rust",
    ]


def test_result_iter_multi_yields_results():
    r = pygxml.parse(BOOKS).get("store.book.#.title")
    items = list(r)
    assert all(isinstance(it, pygxml.Result) for it in items)


# ---- equality ---------------------------------------------------------------

def test_result_eq_string_and_list():
    assert pygxml.parse(BOOKS).get("store.book.0.@id") == "b1"
    assert pygxml.parse(BOOKS).get("store.book.#.@id") == ["b1", "b2", "b3"]


# ---- compiled path ----------------------------------------------------------

def test_compiled_path_get():
    p = pygxml.compile("store.book.#(price>=30)#.title")
    r = p.get_bytes(BOOKS)
    assert [str(it) for it in r] == ["XML in a Nutshell", "Programming Rust"]


# ---- Result.get chaining ---------------------------------------------------

def test_result_get_chain_into_element():
    book = pygxml.parse(BOOKS).get("store.book.0")
    assert str(book.get("title")) == "XML in a Nutshell"
    assert str(book.get("@id")) == "b1"
    assert int(book.get("price")) == 30


def test_result_get_chain_from_get():
    book = pygxml.get_bytes(BOOKS, "store.book.1")
    assert str(book.get("title")) == "The Cathedral and the Bazaar"
    assert str(book.get("@id")) == "b2"


def test_result_get_on_scalar_is_empty():
    n = pygxml.get_bytes(BOOKS, "store.book.#")
    assert str(n) == "3"
    assert not n.get("anything").exists()


# ---- type_ property ---------------------------------------------------------

def test_type_scalar_attribute():
    r = pygxml.get_bytes(BOOKS, "store.book.0.@id")
    assert r.type_ is str          # "b1" is not numeric


def test_type_scalar_count():
    r = pygxml.get_bytes(BOOKS, "store.book.#")
    assert r.type_ is int          # "3" parses as integer


def test_type_text_only_element():
    r = pygxml.get_bytes(BOOKS, "store.book.0.title")
    assert r.type_ is str          # non-numeric text


def test_type_numeric_element():
    r = pygxml.get_bytes(BOOKS, "store.book.0.price")
    assert r.type_ is int          # "30" parses as integer


def test_type_float_element():
    xml = b"<root><x>3.14</x></root>"
    r = pygxml.get_bytes(xml, "root.x")
    assert r.type_ is float


def test_type_element_with_children():
    r = pygxml.get_bytes(BOOKS, "store.book.0")
    assert r.type_ is dict


def test_type_multiple_items():
    r = pygxml.get_bytes(BOOKS, "store.book.#.title")
    assert r.type_ is list


def test_type_empty_result():
    r = pygxml.get_bytes(BOOKS, "store.zzz")
    assert r.type_ is None


def test_type_empty_element():
    xml = b'<root><a id="x"/></root>'
    r = pygxml.get_bytes(xml, "root.a")
    assert r.type_ is str          # empty text → neither int nor float


def test_type_aggregation_scalar():
    r = pygxml.get_bytes(BOOKS, "store.book.#.price|@sum")
    assert r.type_ is int          # "95" parses as integer


# ---- mode-aware __iter__ ----------------------------------------------------

def test_iter_scalar_text_yields_chars():
    r = pygxml.parse(BOOKS).get("store.book.0.price")
    assert list(r) == ["3", "0"]


def test_iter_scalar_attribute_yields_chars():
    r = pygxml.parse(BOOKS).get("store.book.0.@id")
    assert list(r) == ["b", "1"]


def test_iter_scalar_unicode_codepoints():
    xml = "<r>あい</r>".encode("utf-8")
    r = pygxml.get_bytes(xml, "r")
    assert list(r) == ["あ", "い"]


def test_iter_multi_yields_results():
    r = pygxml.parse(BOOKS).get("store.book.#.title")
    items = list(r)
    assert len(items) == 3
    assert all(isinstance(it, pygxml.Result) for it in items)
    assert [str(it) for it in items] == [
        "XML in a Nutshell",
        "The Cathedral and the Bazaar",
        "Programming Rust",
    ]


def test_iter_dict_yields_unique_child_keys():
    r = pygxml.parse(BOOKS).get("store.book.0")
    assert list(r) == ["title", "author", "price"]


def test_iter_dict_unique_with_duplicates():
    r = pygxml.parse(BOOKS).get("store")
    assert list(r) == ["book", "magazine"]


def test_iter_empty_yields_nothing():
    r = pygxml.parse(BOOKS).get("store.zzz")
    assert list(r) == []


# ---- mode-aware __getitem__ -------------------------------------------------

def test_getitem_int_scalar_returns_char():
    r = pygxml.parse(BOOKS).get("store.book.0.price")
    assert r[0] == "3"
    assert r[1] == "0"
    assert r[-1] == "0"


def test_getitem_slice_scalar_returns_substring():
    r = pygxml.parse(BOOKS).get("store.book.0.title")
    assert r[0:3] == "XML"
    assert r[-4:] == "hell"


def test_getitem_str_on_scalar_raises_typeerror():
    r = pygxml.parse(BOOKS).get("store.book.0.price")
    with pytest.raises(TypeError):
        r["foo"]


def test_getitem_int_multi_returns_result():
    r = pygxml.parse(BOOKS).get("store.book.#.title")
    first = r[0]
    assert isinstance(first, pygxml.Result)
    assert str(first) == "XML in a Nutshell"
    assert str(r[-1]) == "Programming Rust"


def test_getitem_slice_multi_returns_result():
    r = pygxml.parse(BOOKS).get("store.book.#.title")
    sub = r[0:2]
    assert isinstance(sub, pygxml.Result)
    assert len(sub) == 2
    assert [str(x) for x in sub] == ["XML in a Nutshell", "The Cathedral and the Bazaar"]


def test_getitem_str_on_multi_raises_typeerror():
    r = pygxml.parse(BOOKS).get("store.book.#.title")
    with pytest.raises(TypeError):
        r["title"]


def test_getitem_str_on_dict_returns_value_result():
    r = pygxml.parse(BOOKS).get("store.book.0")
    title = r["title"]
    assert isinstance(title, pygxml.Result)
    assert str(title) == "XML in a Nutshell"


def test_getitem_str_on_dict_with_duplicate_keys_collects_all():
    r = pygxml.parse(BOOKS).get("store")
    books = r["book"]
    assert isinstance(books, pygxml.Result)
    assert len(books) == 3
    assert [str(b.get("@id")) for b in books] == ["b1", "b2", "b3"]


def test_getitem_str_on_dict_missing_raises_keyerror():
    r = pygxml.parse(BOOKS).get("store.book.0")
    with pytest.raises(KeyError):
        r["nope"]


def test_getitem_int_on_dict_raises_keyerror():
    r = pygxml.parse(BOOKS).get("store.book.0")
    with pytest.raises(KeyError):
        r[0]


def test_getitem_slice_on_dict_raises_keyerror():
    r = pygxml.parse(BOOKS).get("store.book.0")
    with pytest.raises(KeyError):
        r[0:2]


def test_getitem_int_on_empty_raises_indexerror():
    r = pygxml.parse(BOOKS).get("store.zzz")
    with __import__("pytest").raises(IndexError):
        r[0]


def test_getitem_str_on_empty_raises_typeerror():
    r = pygxml.parse(BOOKS).get("store.zzz")
    with pytest.raises(TypeError):
        r["foo"]


def test_getitem_slice_on_empty_returns_empty_result():
    r = pygxml.parse(BOOKS).get("store.zzz")
    sub = r[0:2]
    assert isinstance(sub, pygxml.Result)
    assert len(sub) == 0


# ---- keys / values / items --------------------------------------------------

def test_keys_view_dict_element():
    r = pygxml.parse(BOOKS).get("store.book.0")
    keys = r.keys()
    assert list(keys) == ["title", "author", "price"]
    assert len(keys) == 3
    assert list(keys) == ["title", "author", "price"]


def test_keys_view_unique_with_duplicates():
    r = pygxml.parse(BOOKS).get("store")
    assert list(r.keys()) == ["book", "magazine"]
    assert len(r.keys()) == 2


def test_keys_view_contains():
    r = pygxml.parse(BOOKS).get("store.book.0")
    keys = r.keys()
    assert "title" in keys
    assert "nope" not in keys


def test_values_view_dict_element():
    r = pygxml.parse(BOOKS).get("store.book.0")
    values = list(r.values())
    assert len(values) == 3
    assert all(isinstance(v, pygxml.Result) for v in values)
    assert str(values[0]) == "XML in a Nutshell"   # title
    assert int(values[2]) == 30                     # price


def test_values_view_unique_collects_duplicates():
    r = pygxml.parse(BOOKS).get("store")
    values = list(r.values())
    assert len(values) == 2  # book, magazine
    book_value = values[0]
    assert isinstance(book_value, pygxml.Result)
    assert len(book_value) == 3
    assert [str(b.get("@id")) for b in book_value] == ["b1", "b2", "b3"]


def test_items_view_dict_element():
    r = pygxml.parse(BOOKS).get("store.book.0")
    pairs = list(r.items())
    assert len(pairs) == 3
    keys = [k for k, _ in pairs]
    assert keys == ["title", "author", "price"]
    assert str(pairs[0][1]) == "XML in a Nutshell"


def test_items_view_unique_with_duplicates():
    r = pygxml.parse(BOOKS).get("store")
    pairs = list(r.items())
    assert [k for k, _ in pairs] == ["book", "magazine"]
    book_value = pairs[0][1]
    assert len(book_value) == 3


def test_keys_values_items_on_non_dict_raise_typeerror():
    text_r = pygxml.parse(BOOKS).get("store.book.0.title")
    with pytest.raises(TypeError):
        text_r.keys()
    with pytest.raises(TypeError):
        text_r.values()
    with pytest.raises(TypeError):
        text_r.items()
    multi_r = pygxml.parse(BOOKS).get("store.book.#.title")
    with pytest.raises(TypeError):
        multi_r.keys()
    empty_r = pygxml.parse(BOOKS).get("store.zzz")
    with pytest.raises(TypeError):
        empty_r.keys()


def test_keys_view_repeated_iteration():
    r = pygxml.parse(BOOKS).get("store.book.0")
    keys = r.keys()
    first = list(keys)
    second = list(keys)
    assert first == second == ["title", "author", "price"]


def test_dict_index_chaining():
    book = pygxml.parse(BOOKS).get("store.book.0")
    assert str(book["title"]) == "XML in a Nutshell"
    assert int(book["price"]) == 30


# ---- __int__, __float__ dunder methods --------------------------------------

def test_dunder_int():
    r = pygxml.parse(BOOKS).get("store.book.0.price")
    assert int(r) == 30


def test_dunder_int_missing():
    r = pygxml.parse(BOOKS).get("store.zzz")
    with pytest.raises(TypeError):
        int(r)


def test_dunder_float():
    r = pygxml.parse(BOOKS).get("store.book.0.price")
    assert float(r) == 30.0


def test_dunder_float_missing():
    r = pygxml.parse(BOOKS).get("store.zzz")
    with pytest.raises(TypeError):
        float(r)


# ---- value property ---------------------------------------------------------

def test_value_empty():
    r = pygxml.get_bytes(BOOKS, "store.zzz")
    assert r.value is None


def test_value_scalar_int():
    r = pygxml.get_bytes(BOOKS, "store.book.0.price")
    assert r.value == 30
    assert type(r.value) is int


def test_value_scalar_float():
    xml = b"<root><x>3.14</x></root>"
    r = pygxml.get_bytes(xml, "root.x")
    assert abs(r.value - 3.14) < 1e-9
    assert type(r.value) is float


def test_value_scalar_str():
    r = pygxml.get_bytes(BOOKS, "store.book.0.title")
    assert r.value == "XML in a Nutshell"
    assert type(r.value) is str


def test_value_multi():
    r = pygxml.get_bytes(BOOKS, "store.book.#.title")
    v = r.value
    assert isinstance(v, list)
    assert len(v) == 3
    assert all(isinstance(it, pygxml.Result) for it in v)
    assert [str(it) for it in v] == [
        "XML in a Nutshell",
        "The Cathedral and the Bazaar",
        "Programming Rust",
    ]


def test_value_dict():
    r = pygxml.get_bytes(BOOKS, "store.book.0")
    v = r.value
    assert isinstance(v, dict)
    assert set(v.keys()) == {"title", "author", "price"}
    assert isinstance(v["title"], pygxml.Result)
    assert str(v["title"]) == "XML in a Nutshell"
    assert int(v["price"]) == 30


# ---- to_int / to_float ------------------------------------------------------

def test_to_int():
    r = pygxml.parse(BOOKS).get("store.book.0.price")
    assert r.to_int() == 30


def test_to_int_missing():
    r = pygxml.parse(BOOKS).get("store.zzz")
    assert r.to_int() == 0


def test_to_float():
    r = pygxml.parse(BOOKS).get("store.book.0.price")
    assert r.to_float() == 30.0


def test_to_float_missing():
    r = pygxml.parse(BOOKS).get("store.zzz")
    assert r.to_float() == 0.0


# ---- to_bool ----------------------------------------------------------------

def test_to_bool_true():
    xml = b"<root><flag>true</flag><on>1</on></root>"
    assert pygxml.get_bytes(xml, "root.flag").to_bool() is True
    assert pygxml.get_bytes(xml, "root.on").to_bool() is True


def test_to_bool_false():
    xml = b"<root><f>false</f><zero>0</zero><F>False</F><FF>FALSE</FF></root>"
    assert pygxml.get_bytes(xml, "root.f").to_bool() is False
    assert pygxml.get_bytes(xml, "root.zero").to_bool() is False
    assert pygxml.get_bytes(xml, "root.F").to_bool() is False
    assert pygxml.get_bytes(xml, "root.FF").to_bool() is False


def test_to_bool_nonzero_int():
    xml = b"<root><n>42</n></root>"
    assert pygxml.get_bytes(xml, "root.n").to_bool() is True


def test_to_bool_unknown_str():
    xml = b"<root><s>hello</s></root>"
    assert pygxml.get_bytes(xml, "root.s").to_bool() is False


def test_to_bool_dict():
    assert pygxml.get_bytes(BOOKS, "store.book.0").to_bool() is True


def test_to_bool_multi():
    assert pygxml.get_bytes(BOOKS, "store.book.#.title").to_bool() is True


def test_to_bool_missing():
    assert pygxml.get_bytes(BOOKS, "store.zzz").to_bool() is False


# ---- to_str -----------------------------------------------------------------

def test_to_str_scalar_int():
    r = pygxml.get_bytes(BOOKS, "store.book.0.price")
    assert r.to_str() == "30"


def test_to_str_scalar_float():
    xml = b"<root><x>3.14</x></root>"
    r = pygxml.get_bytes(xml, "root.x")
    assert r.to_str() == "3.14"


def test_to_str_scalar_str():
    r = pygxml.get_bytes(BOOKS, "store.book.0.title")
    assert r.to_str() == "XML in a Nutshell"


def test_to_str_attribute_scalar():
    r = pygxml.get_bytes(BOOKS, "store.book.0.@id")
    assert r.to_str() == "b1"


def test_to_str_element_with_children():
    r = pygxml.get_bytes(BOOKS, "store.book.0")
    xml_str = r.to_str()
    assert "<title>XML in a Nutshell</title>" in xml_str
    assert xml_str.startswith("<book")


def test_to_str_multi_joined_with_newline():
    r = pygxml.get_bytes(BOOKS, "store.book.#.title")
    parts = r.to_str().split("\n")
    assert len(parts) == 3
    assert parts[0] == "<title>XML in a Nutshell</title>"
    assert parts[1] == "<title>The Cathedral and the Bazaar</title>"
    assert parts[2] == "<title>Programming Rust</title>"


def test_to_str_empty():
    r = pygxml.get_bytes(BOOKS, "store.zzz")
    assert r.to_str() == ""


# ---- __repr__ ---------------------------------------------------------------

def test_repr_empty():
    r = pygxml.get_bytes(BOOKS, "store.zzz")
    assert repr(r) == "None"


def test_repr_scalar():
    r = pygxml.get_bytes(BOOKS, "store.book.0.@id")
    assert repr(r) == "b1"


def test_repr_dict_many_keys():
    r = pygxml.get_bytes(BOOKS, "store.book.0")
    assert repr(r) == "<Result type=dict, keys=[title, author, ...]>"


def test_repr_dict_few_keys():
    r = pygxml.get_bytes(BOOKS, "store.magazine.0")
    assert repr(r) == "<Result type=dict, keys=[title]>"


def test_repr_list_many():
    r = pygxml.get_bytes(BOOKS, "store.book.#.title")
    assert repr(r) == "<Result type=list, value=[XML in a Nutshell, The Cathedral and the Bazaar, ...]>"


def test_repr_list_few():
    r = pygxml.get_bytes(BOOKS, "store.book.#.@id")
    items = list(r)
    r2 = pygxml.get_bytes(BOOKS, "store.book.#.@id")
    assert repr(items[0]) == "b1"
    assert repr(r2).startswith("<Result type=list")
