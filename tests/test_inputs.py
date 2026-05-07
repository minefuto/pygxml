"""Tests for all supported input types: bytes, str, mmap, bytearray, memoryview."""
import mmap
from pathlib import Path

import pytest

import pygxml

DATA = Path(__file__).parent / "data"
BOOKS_PATH = DATA / "books.xml"
BOOKS = BOOKS_PATH.read_bytes()


# ---- str input (get / get_many) --------------------------------------------

def test_str_input_works():
    xml_text = BOOKS.decode("utf-8")
    assert str(pygxml.get(xml_text, "store.book.0.title")) == "XML in a Nutshell"


# ---- bytes input (get_bytes / get_many_bytes) --------------------------------

def test_bytes_input_works():
    assert str(pygxml.get_bytes(BOOKS, "store.book.0.title")) == "XML in a Nutshell"


# ---- mmap input (get_buffer / get_many_buffer) -----------------------------

def test_mmap_get_simple():
    with open(BOOKS_PATH, "rb") as f:
        with mmap.mmap(f.fileno(), 0, access=mmap.ACCESS_READ) as mm:
            assert str(pygxml.get_buffer(mm, "store.book.0.title")) == "XML in a Nutshell"


def test_mmap_iter_each():
    with open(BOOKS_PATH, "rb") as f:
        with mmap.mmap(f.fileno(), 0, access=mmap.ACCESS_READ) as mm:
            ids = [str(it) for it in pygxml.get_buffer(mm, "store.book.#.@id")]
            assert ids == ["b1", "b2", "b3"]


def test_mmap_filter():
    with open(BOOKS_PATH, "rb") as f:
        with mmap.mmap(f.fileno(), 0, access=mmap.ACCESS_READ) as mm:
            t = str(pygxml.get_buffer(mm, "store.book.#(price>=30).title"))
            assert t == "XML in a Nutshell"


def test_mmap_compiled_path():
    p = pygxml.compile("store.book.#.title")
    with open(BOOKS_PATH, "rb") as f:
        with mmap.mmap(f.fileno(), 0, access=mmap.ACCESS_READ) as mm:
            titles = [str(it) for it in p.get_buffer(mm)]
            assert titles == [
                "XML in a Nutshell",
                "The Cathedral and the Bazaar",
                "Programming Rust",
            ]


def test_mmap_parse_zero_copy_chain():
    with open(BOOKS_PATH, "rb") as f:
        with mmap.mmap(f.fileno(), 0, access=mmap.ACCESS_READ) as mm:
            r = pygxml.parse(mm)
            assert str(r.get("store.book.0.title")) == "XML in a Nutshell"
            assert [str(it) for it in r.get("store.book.#.@id")] == ["b1", "b2", "b3"]


# ---- bytearray (buffer protocol) -------------------------------------------

def test_bytearray_input():
    ba = bytearray(BOOKS)
    assert str(pygxml.get_buffer(ba, "store.book.0.title")) == "XML in a Nutshell"


# ---- memoryview (buffer protocol) ------------------------------------------

def test_memoryview_input():
    mv = memoryview(BOOKS)
    assert str(pygxml.get_buffer(mv, "store.book.1.@id")) == "b2"


# ---- wrong type raises TypeError -------------------------------------------

def test_invalid_input_str_raises():
    with pytest.raises(TypeError):
        pygxml.get(b"<a/>", "a")  # bytes passed to str variant


def test_invalid_input_bytes_raises():
    with pytest.raises(TypeError):
        pygxml.get_bytes("<a/>", "a")  # str passed to bytes variant


def test_invalid_input_buffer_raises():
    with pytest.raises((TypeError, ValueError)):
        pygxml.get_buffer(12345, "a")  # int is not a buffer
