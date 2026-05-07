"""Tests for XML namespace handling: local-name match and prefix-aware match."""
import pygxml

ATOM = b"""<?xml version="1.0"?>
<atom:feed xmlns:atom="http://www.w3.org/2005/Atom">
  <atom:title>Example Feed</atom:title>
  <atom:entry>
    <atom:title>First Post</atom:title>
    <atom:summary>Hello</atom:summary>
  </atom:entry>
  <atom:entry>
    <atom:title>Second Post</atom:title>
    <atom:summary>World</atom:summary>
  </atom:entry>
</atom:feed>"""


def test_namespace_default_local_name_match():
    assert str(pygxml.get_bytes(ATOM, "feed.title")) == "Example Feed"
    titles = [str(it) for it in pygxml.get_bytes(ATOM, "feed.entry.#.title")]
    assert titles == ["First Post", "Second Post"]


def test_namespace_prefix_aware_match():
    assert str(pygxml.get_bytes(ATOM, "atom:feed.atom:title")) == "Example Feed"
    titles = [str(it) for it in pygxml.get_bytes(ATOM, "atom:feed.atom:entry.#.atom:title")]
    assert titles == ["First Post", "Second Post"]


def test_namespace_wrong_prefix_misses():
    assert not pygxml.get_bytes(ATOM, "rss:feed.rss:title").exists()


def test_namespace_mixed_path():
    assert str(pygxml.get_bytes(ATOM, "atom:feed.title")) == "Example Feed"
    assert str(pygxml.get_bytes(ATOM, "feed.atom:title")) == "Example Feed"


def test_namespace_default_xmlns_uses_local_name():
    xml = b"""<feed xmlns="http://www.w3.org/2005/Atom">
        <entry><title>Hi</title></entry>
    </feed>"""
    assert str(pygxml.get_bytes(xml, "feed.entry.title")) == "Hi"


def test_namespace_filter():
    expected = "Hello"
    assert str(pygxml.get_bytes(ATOM, "atom:feed.atom:entry.#(atom:title==\"First Post\").atom:summary")) == expected
