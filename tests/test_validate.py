"""Tests for pygxml.validate() — XML well-formedness check."""
import mmap
import tempfile
from pathlib import Path

import pytest

import pygxml

DATA = Path(__file__).parent / "data"
BOOKS = (DATA / "books.xml").read_bytes()


# ---- positive cases --------------------------------------------------------

def test_valid_books_xml():
    assert pygxml.validate(BOOKS) is True


def test_valid_self_closing_root():
    assert pygxml.validate(b"<r/>") is True


def test_valid_simple_document():
    assert pygxml.validate(b"<root><a>1</a><b>2</b></root>") is True


def test_valid_bytes_input():
    assert pygxml.validate(b"<r/>") is True


def test_valid_str_input():
    assert pygxml.validate("<r/>") is True


def test_valid_bytearray_input():
    assert pygxml.validate(bytearray(b"<r/>")) is True


def test_valid_memoryview_input():
    assert pygxml.validate(memoryview(b"<r/>")) is True


def test_valid_mmap_input():
    with tempfile.NamedTemporaryFile() as f:
        f.write(BOOKS)
        f.flush()
        with mmap.mmap(f.fileno(), 0, access=mmap.ACCESS_READ) as mm:
            assert pygxml.validate(mm) is True


def test_valid_with_xml_declaration():
    assert pygxml.validate(b"<?xml version='1.0'?><r/>") is True


def test_valid_with_comment():
    assert pygxml.validate(b"<!-- comment --><r/>") is True


def test_valid_with_pi():
    assert pygxml.validate(b"<?pi target?><r/>") is True


def test_valid_with_decl_comment_pi():
    assert pygxml.validate(b"<?xml version='1.0'?><!-- c --><?pi?><r/>") is True


def test_valid_with_cdata():
    assert pygxml.validate(b"<r><![CDATA[<raw>content</raw>]]></r>") is True


def test_valid_with_entities():
    assert pygxml.validate(b"<r>foo &amp; bar &lt;tag&gt;</r>") is True


def test_valid_with_namespaces():
    assert pygxml.validate(b'<a:r xmlns:a="http://x"><a:c/></a:r>') is True


def test_valid_nested():
    xml = b"<a><b><c><d>text</d></c></b></a>"
    assert pygxml.validate(xml) is True


def test_valid_whitespace_in_prolog():
    assert pygxml.validate(b"  <r/>") is True


# ---- negative cases --------------------------------------------------------

def test_invalid_mismatched_tags():
    assert pygxml.validate(b"<a></b>") is False


def test_invalid_unclosed_tag():
    assert pygxml.validate(b"<a><b></a>") is False


def test_invalid_eof_in_element():
    assert pygxml.validate(b"<a><b>") is False


def test_invalid_only_open_root():
    assert pygxml.validate(b"<r>text") is False


def test_invalid_two_roots_empty():
    assert pygxml.validate(b"<a/><b/>") is False


def test_invalid_two_roots_full():
    assert pygxml.validate(b"<a></a><b></b>") is False


def test_invalid_bare_text_only():
    assert pygxml.validate(b"hello world") is False


def test_invalid_empty_input():
    assert pygxml.validate(b"") is False


def test_invalid_only_declaration():
    assert pygxml.validate(b"<?xml version='1.0'?>") is False


def test_invalid_unterminated_comment():
    assert pygxml.validate(b"<r><!-- never closed</r>") is False


def test_invalid_unterminated_cdata():
    assert pygxml.validate(b"<r><![CDATA[ unterminated</r>") is False


def test_invalid_lone_lt():
    assert pygxml.validate(b"<") is False


def test_invalid_garbage():
    assert pygxml.validate(b"\x00\xff\xfe garbage") is False


# ---- does not raise --------------------------------------------------------

def test_validate_does_not_raise_on_pathological_input():
    bad_inputs = [
        b"",
        b"<",
        b">",
        b"<a>",
        b"</a>",
        b"<a><b>",
        b"<a></b>",
        b"\x00\x01\x02",
        b"<<<",
        b"<a/><b/>",
        b"just text",
    ]
    for data in bad_inputs:
        result = pygxml.validate(data)
        assert isinstance(result, bool), f"validate({data!r}) did not return bool"
        assert result is False, f"validate({data!r}) should be False"


# ---- type error on unsupported input ---------------------------------------

def test_type_error_on_int():
    with pytest.raises((ValueError, TypeError)):
        pygxml.validate(123)
