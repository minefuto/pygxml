"""pygxml — gjson-style path queries over XML.

Path syntax:
    a.b.c                descend into named child elements
    a.0, a.1             pick the n-th same-named child
    a.#                  count of same-named children (terminal)
    a.#.b                project remaining over each occurrence
    *, ?                 wildcards in element names
    @name                attribute (terminal)
    #text                explicit text terminal
    \\., \\@             escape

    a.#(expr)            filter: first <a> for which expr is true
    a.#(expr)#           filter: all <a> for which expr is true
                         expr ::= path op value
                         op  ∈ ==, =, !=, <, <=, >, >=, %, !%
                         value: "string", number, true, false

    path|@modifier       transform the result list
                         modifiers: @reverse, @first, @last, @count,
                                    @sort, @sort_n, @unique, @flatten,
                                    @tostr, @sum, @avg, @min, @max

    a.**.b               descendant: match every `b` at any depth under `a`

Implicit-selector rule:
    A bare child name (no `.N`/`.#`/filter) returns every match at the
    terminal position, but raises ``ValueError`` mid-path when more than
    one sibling matches. Chain past a multi-match step with `.N` (single)
    or `.#` (each). Single-match siblings descend transparently. The same
    rule applies to ``Result.get(path)``: the receiver must hold at most
    one element. Iterate (``for item in result: item.get(...)``) to walk
    a multi-match Result.

API:
    pygxml.get(data, path)        -> Result  str input
    pygxml.get_bytes(data, path)  -> Result  bytes input (zero-copy)
    pygxml.get_buffer(data, path) -> Result  mmap/bytearray/memoryview input
    pygxml.parse(data)            -> Result  Result over the full document
    pygxml.compile(path)          -> Path    pre-compile a path for reuse

Input-type variants:
    get / get_many               accept str
    get_bytes / get_many_bytes   accept bytes (get_many_bytes uses zero-copy)
    get_buffer / get_many_buffer accept mmap.mmap, bytearray, memoryview
    parse / validate / Path.get* accept any of the above (XmlInput)

Note: ``parse(data)`` keeps a reference to the input object instead of
copying. For mmap inputs, do not close the mmap while a Result derived
from it is still in use. For bytearray/buffer inputs, mutating the buffer
after parse() will be observed by subsequent Result.get()/str() calls.
"""
from __future__ import annotations

from typing import Any, Union

from . import _pygxml as _native

XmlInput = Union[bytes, str, bytearray, memoryview, Any]
BufferInput = Union[bytearray, memoryview, Any]

# Re-export the native types so callers can `isinstance(x, pygxml.Result)`.
Result = _native.Result
Path = _native.Path


def get(data: str, path: str) -> Result:
    """Run `path` against XML string `data` and return a typed `Result`."""
    return _native.get(data, path)


def get_bytes(data: bytes, path: str) -> Result:
    """Run `path` against XML bytes `data` and return a typed `Result`."""
    return _native.get_bytes(data, path)


def get_buffer(data: "BufferInput", path: str) -> Result:
    """Run `path` against a buffer-protocol `data` (mmap, bytearray, memoryview)."""
    return _native.get_buffer(data, path)


def parse(data: XmlInput) -> Result:
    """Wrap `data` in a top-level `Result`. Use `.get(path)` to descend.

    The input object is held by reference; do not close/mutate it while the
    returned Result is in use.
    """
    return _native.parse(data)


def compile(path: str) -> Path:
    """Pre-compile a path for repeated use against many documents."""
    return _native.compile(path)


def validate(data: XmlInput) -> bool:
    """Return True iff `data` is well-formed XML.

    Equivalent to "would a parser accept this document". Does not raise on
    invalid content; non-XML inputs return False. TypeError still applies
    to wholly wrong input types (e.g., int).
    """
    return _native.validate(data)


def get_many(data: str, paths: "list[str | Path]") -> "list[Result]":
    """Run multiple paths against XML string `data` in a single document scan.

    Returns one Result per input path, in the same order as `paths`.
    Accepts pre-compiled ``Path`` objects to skip compilation.
    """
    return _native.get_many(data, paths)


def get_many_bytes(data: bytes, paths: "list[str | Path]") -> "list[Result]":
    """Run multiple paths against XML bytes `data` in a single document scan.

    Uses zero-copy ElementRef for bytes input. Returns one Result per path.
    """
    return _native.get_many_bytes(data, paths)


def get_many_buffer(data: "BufferInput", paths: "list[str | Path]") -> "list[Result]":
    """Run multiple paths against a buffer-protocol `data` in a single scan.

    Accepts mmap.mmap, bytearray, memoryview. Returns one Result per path.
    """
    return _native.get_many_buffer(data, paths)


__all__ = [
    "get",
    "get_bytes",
    "get_buffer",
    "get_many",
    "get_many_bytes",
    "get_many_buffer",
    "parse",
    "compile",
    "validate",
    "Path",
    "Result",
    "XmlInput",
    "BufferInput",
]
