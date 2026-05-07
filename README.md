# pygxml

streaming parser using gjson-style path queries over XML. Rust core (quick-xml) + PyO3.

The original GJSON: [tidwall/gjson](https://github.com/tidwall/gjson)

## Installation

```bash
pip install pygxml
```

## Usage examples

```python
import pygxml

xml = b"""<store>
  <book id="b1"><title>XML in a Nutshell</title><price>30</price></book>
  <book id="b2"><title>The Cathedral and the Bazaar</title><price>20</price></book>
  <book id="b3"><title>Programming Rust</title><price>45</price></book>
</store>"""

# Single-shot path query — returns a typed Result.
pygxml.get(xml, "store.book").type_                         # list (3 books)
pygxml.get(xml, "store.book.0.title").to_str()             # 'XML in a Nutshell'
pygxml.get(xml, "store.book.1.@id").to_str()               # 'b2'
pygxml.get(xml, "store.book.#").to_int()                   # 3
[str(r) for r in pygxml.get(xml, "store.book.#.title")]    # ['XML in a Nutshell', ...]

# A bare child name with multiple matches AND a follow-on step is rejected:
# the user must pick `.N` (single) or `.#` (each).
pygxml.get(xml, "store.book.title")                         # ValueError

# Filters
pygxml.get(xml, "store.book.#(price>=30).title").to_str()           # 'XML in a Nutshell'
[str(r) for r in pygxml.get(xml, "store.book.#(price>=30)#.title")] # all matches
pygxml.get(xml, 'store.book.#(@id=="b2").title').to_str()           # 'The Cathedral...'

# Modifiers
pygxml.get(xml, "store.book.#.title|@count").to_int()               # 3

# Result.get(...) — descend into a captured element fragment.
book = pygxml.get(xml, "store.book.0")
book.get("title").to_str()                                # 'XML in a Nutshell'
book.get("@id").to_str()                                  # 'b1'
book.get("price").to_int()                                # 30

# parse(data) — wrap the input as a top-level Result for chained navigation.
r = pygxml.parse(xml)
r.get("store.book.0.title").to_str()                      # 'XML in a Nutshell'
r.get("store.book.#(price>=30)#.title").value             # [Result('XML in a Nutshell'), Result('Programming Rust')]

# get_many — scan the document once and return multiple Results.
title, price = pygxml.get_many(xml, ["store.book.0.title", "store.book.0.price"])

# mmap input — true zero-copy on huge files. parse(mm) keeps the mmap by
# reference, so subsequent .get() calls re-borrow it without copying.
import mmap
with open('huge.xml', 'rb') as f, mmap.mmap(f.fileno(), 0, access=mmap.ACCESS_READ) as mm:
    title = pygxml.parse(mm).get("store.book.0.title").to_str()

# Namespace prefix-aware match
pygxml.get(atom_xml, "atom:feed.atom:entry.atom:title").to_str()

# Validate well-formedness without raising.
pygxml.validate(xml)                                        # True
```

## API

### Module-level functions


| Function                        | Description                                              |
|---------------------------------|----------------------------------------------------------|
| `get(xml, path)`               | Query `xml` (str) at `path`; returns `Result`           |
| `get_bytes(xml, path)`         | Query `xml` (bytes) at `path`; returns `Result`         |
| `get_buffer(xml, path)`         | Query `xml` (buffer protocol) at `path`; returns `Result`         |
| `get_many(xml, paths)`         | Query `xml` (str) at each path; returns `list[Result]`  |
| `get_many_bytes(xml, paths)`   | Query `xml` (bytes) at each path; returns `list[Result]`|
| `get_many_buffer(xml, paths)`   | Query `xml` (buffer protocol) at each path; returns `list[Result]`|
| `parse(xml)`                   | Parse the entire JSON document into a `Result`           |
| `validate(xml)`                | `True` if `xml` is syntactically valid                  |


### Result

`get` and `parse` return a `Result`. Result accessors

**Properties**

| Property    | Description                                                                      |
|-------------|----------------------------------------------------------------------------------|
| `r.type_`   | Python type for this value: `None`, `bool`, `int`, `float`, `str`, `list`, `dict` |
| `r.value`   | Value converted to the corresponding Python type: `None` / `int` / `float` / `str` / `list[Result]` / `dict[str, Result]`                                  |

**gjson-style methods**

| Method                   | Description                                               |
|--------------------------|-----------------------------------------------------------|
| `r.exists()`             | `True` if the value was found in the XML                 |
| `r.to_str()`             | String representation (text content for elements, or full XML for dict/list elements)             |
| `r.to_int()` / `r.to_float()` | Typed coercions; return `0` / `0.0` when empty |
| `r.to_bool()` | gjson-style boolean coercion (see below); returns `False` when empty |
| `r.get(path)`            | Sub-query relative to this value                          |
| `r.get_many(paths)`      | Sub-query at multiple paths; returns `list[Result]`       |

`Result.to_bool()` follows gjson semantics:
- `"1"` / `"true"` → `True`; `"0"` / `"false"` → `False`
- `"\"t\""` / `"\"T\""` / `"\"1\""` → `True`; `"\"f\""` / `"\"F\""` / `"\"0\""` → `False`
- `"\"true\""` / `"\"TRUE\""` / `"\"True\""` → `True`; `"\"false\""` / `"\"FALSE\""` / `"\"False\""` → `False`
- Any other value: `to_int() != 0` (non-numeric strings → `False`)
- Non-empty dict or list Result → `True`; empty Result → `False`

`Result.get(path)` only descends into element items — scalar items
(attributes, `#text`, counts, modifier aggregates like `@sum`) have no
children, so `.get(...)` against them yields an empty Result.

**Pythonic methods**

| Syntax              | Description                                                                   |
|---------------------|-------------------------------------------------------------------------------|
| `str(v)`,`repr(v)`  | dict: `<Result type=dict, keys=[...]>`; list: `<Result type=list, value=[...]>`; others: `str(v.value)` |
| `int(v)`            | 64-bit Integer                                                                |
| `float(v)`          | 64-bit float                                                                  |
| `bool(v)`           | Equivalent to `bool(v.value)` — `False` for null/false/0/""/[]/{}            |
| `len(v)`            | Chars for String; element count for list/dict elements                              |
| `v[key]`            | Subscript access                                                              |
| `key in v`          | Key membership for dict; string match for list                    |
| `iter(v)`           | Lazy iterator: chars for str; `Result`s for list; keys for dict         |
| `v.keys()`          | Lazy `KeysView` of dict keys (raises `TypeError` for non-dict)            |
| `v.values()`        | Lazy `ValuesView` of dict values (raises `TypeError` for non-dict)        |
| `v.items()`         | Lazy `ItemsView` of `(key, Result)` pairs (raises `TypeError` for non-dict) |
| `r == "x"`, `r == ["a", "b"]`, `r == other_result` | Equality with str/list/Result |


## Path syntax

| syntax | meaning |
|---|---|
| `a.b.c` | Descend into child elements (local-name match, ignores namespaces) |
| `a.0`, `a.1` | N-th same-named sibling |
| `a.#` | Count of same-named siblings |
| `a.#.b` | Project `b` over all same-named siblings |
| `*`, `?` | Wildcards in element name |
| `@name` | Attribute reference |
| `#text` | Explicit text content |
| `\.`, `\@` | Escape |
| `a.#(expr)` | Filter, first match. `expr ::= path op value` |
| `a.#(expr)#` | Filter, all matches |
| `path \| @modifier` | Apply modifier (`@reverse`, `@first`, `@last`, `@count`, `@sort`, `@sort_n`, `@unique`/`@uniq`, `@flatten`, `@tostr`, `@sum`, `@avg`/`@mean`, `@min`, `@max`) |
| `prefix:local` | Prefix-aware match — qualified-name literal compare (matches `<atom:title>`, not `<rss:title>`) |
| `a.**.b` | Descendant: match every `b` at any depth under `a` (XPath `//` equivalent) |

A bare child name (no `.N`/`.#`/filter) is **implicit**: at the terminal
position it returns every match (a list-shaped Result), but at a
non-terminal position the engine raises `ValueError` if more than one
element matches. To chain past a multi-match step, pick one (`.0`, `.1`,
…) or project explicitly with `.#`. Single-match elements (e.g., a unique
root element) chain transparently.

`Result.get(path)` follows the same rule: it requires the receiver to
hold at most one element. To process every element of a multi-match
Result, iterate (`for item in result: item.get(...)`).

Filter operators: `==` `=` `!=` `<` `<=` `>` `>=` `%` (glob) `!%` (negative glob).
Filter values: `"string"`, number, `true`/`false`, or bare unquoted text.

## Inputs

`bytes`, `str`, `mmap.mmap`, `bytearray`, `memoryview` — anything
implementing the buffer protocol. `bytes` and `mmap` are zero-copy on the
way in; `str` is copied once to UTF-8.

`pygxml.parse(data)` keeps a *reference* to the input object instead of
copying it. Two consequences worth knowing:

- For `mmap` input, do not close the mmap while a Result derived from it is
  still in use — re-borrows during `.get()` / `.str()` will fault.
- For `bytearray` and other mutable buffers, mutations after `parse(...)`
  are observed by subsequent Result accesses.

`pygxml.get(data, path)` does not retain the input; captured element
fragments inside the returned Result are owned copies.

## License
MIT
