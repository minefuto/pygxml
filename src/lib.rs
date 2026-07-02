// PyO3 entry: exposes `get`, `get_bytes`, `get_buffer`, `get_many`,
// `get_many_bytes`, `get_many_buffer`, `parse`, `compile`, `validate`,
// and the `Path` and `Result` classes to Python.
//
// Result holds matched items by reference (`Source::Borrowed`) when produced
// by `parse(data)`, so the entire input is not copied — the underlying object
// is re-borrowed on demand. Items captured by descending a path
// (`get(data, path)` or `Result.get(path)`) are stored as `Source::Owned`
// since they are small fragments. `get_many_bytes` uses `Source::BorrowedSlice`
// (zero-copy byte range) backed by the immutable `PyBytes` input.
//
// `py_get` and `py_get_many` use `PyString::to_cow()` so canonical (already
// UTF-8) Python strings are passed to the engine without copying.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use pyo3::buffer::PyBuffer;
use pyo3::class::basic::CompareOp;
use pyo3::exceptions::{PyIndexError, PyKeyError, PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyFloat, PyInt, PyList, PySlice, PyString, PyTuple};
use pyo3::PyTypeInfo;

mod engine;
mod path;
mod wildcard;

use engine::{extract_element_text, run_many, run_many_within, run_with, validate, EngineError, Item};
use path::Program;

fn engine_err_to_py(err: EngineError) -> pyo3::PyErr {
    match err {
        EngineError::Done => unreachable!("Done is converted to Ok before reaching here"),
        _ => PyValueError::new_err(err.to_string()),
    }
}

// Run `f` against the input's bytes without copying when possible. The buffer
// (if any) stays alive for the duration of `f` — `f` returns an owned value, so
// nothing borrows from the buffer after `with_xml_input` returns.
fn with_xml_input<R>(
    data: &Bound<'_, PyAny>,
    f: impl FnOnce(&[u8]) -> R,
) -> PyResult<R> {
    if let Ok(b) = data.cast::<PyBytes>() {
        return Ok(f(b.as_bytes()));
    }
    if let Ok(s) = data.cast::<PyString>() {
        // to_cow() is zero-copy for canonical (UTF-8-internal) Python strings;
        // for non-canonical representations it encodes to UTF-8 and caches
        // the result on the Python object.
        let cow = s.to_cow()?;
        return Ok(f(cow.as_bytes()));
    }
    // Fall back to the buffer protocol (mmap.mmap, bytearray, numpy bytes, …).
    if let Ok(buf) = PyBuffer::<u8>::get(data) {
        if !buf.is_c_contiguous() {
            return Err(PyValueError::new_err("buffer must be C-contiguous"));
        }
        if buf.item_size() != 1 {
            return Err(PyValueError::new_err(format!(
                "buffer item size must be 1, got {}",
                buf.item_size()
            )));
        }
        // SAFETY: PyBuffer keeps the underlying buffer locked for its lifetime.
        // The slice cannot outlive `buf`, and `f` returns an owned value before
        // we drop `buf` below.
        let slice = unsafe {
            std::slice::from_raw_parts(buf.buf_ptr() as *const u8, buf.item_count())
        };
        let result = f(slice);
        drop(buf);
        return Ok(result);
    }
    Err(PyValueError::new_err(
        "data must be bytes, str, or a buffer-protocol object (mmap, bytearray)",
    ))
}

fn with_buffer_input<R>(data: &Bound<'_, PyAny>, f: impl FnOnce(&[u8]) -> R) -> PyResult<R> {
    let buf = PyBuffer::<u8>::get(data).map_err(|_| {
        PyValueError::new_err(
            "data must be a buffer-protocol object (mmap, bytearray, memoryview)",
        )
    })?;
    if !buf.is_c_contiguous() {
        return Err(PyValueError::new_err("buffer must be C-contiguous"));
    }
    if buf.item_size() != 1 {
        return Err(PyValueError::new_err(format!(
            "buffer item size must be 1, got {}",
            buf.item_size()
        )));
    }
    // SAFETY: PyBuffer keeps the underlying buffer locked for its lifetime.
    // The slice cannot outlive `buf`, and `f` returns an owned value before
    // we drop `buf` below.
    let slice = unsafe {
        std::slice::from_raw_parts(buf.buf_ptr() as *const u8, buf.item_count())
    };
    let result = f(slice);
    drop(buf);
    Ok(result)
}

// Compiling a path is a pure function of the path string, so the result is
// memoized. Every get*/get_many*/Result.get call funnels through here, so the
// cache removes repeated tokenize+build work when the same path is queried many
// times (the common case). Thread-local, so no locking is needed: PyO3 calls
// run under the GIL, and separate threads each keep their own small cache.
const PATH_CACHE_CAP: usize = 512;

thread_local! {
    static PATH_CACHE: RefCell<HashMap<String, Arc<Program>>> =
        RefCell::new(HashMap::new());
}

fn compile_path(path: &str) -> PyResult<Arc<Program>> {
    if let Some(prog) = PATH_CACHE.with(|c| c.borrow().get(path).cloned()) {
        return Ok(prog);
    }
    let prog = path::compile(path)
        .map(Arc::new)
        .map_err(|e| PyValueError::new_err(e.to_string()))?;
    PATH_CACHE.with(|c| {
        let mut map = c.borrow_mut();
        // Bound the cache; on overflow drop everything (rare for realistic path
        // sets) to avoid unbounded growth on pathological input.
        if map.len() >= PATH_CACHE_CAP {
            map.clear();
        }
        map.insert(path.to_string(), Arc::clone(&prog));
    });
    Ok(prog)
}

fn compile_path_opt(path: &str) -> PyResult<Option<Arc<Program>>> {
    if path.is_empty() {
        return Ok(None);
    }
    compile_path(path).map(Some)
}

/// The bytes backing an `Item::Element`.
enum Source {
    /// Bytes captured from a previous engine run — small and self-contained.
    /// Arc-shared so `clone_ref` (Multi-mode iteration / `.value` / indexing)
    /// is a refcount bump instead of a byte copy.
    Owned(Arc<Vec<u8>>),
    /// A reference back to the original Python input. Re-borrowed on each
    /// access via `with_xml_input` (zero-copy for bytes/mmap/buffer).
    Borrowed(Py<PyAny>),
    /// Zero-copy sub-slice of a PyBytes input. The object is kept alive for
    /// the Result's lifetime; the byte range is resolved on each access.
    BorrowedSlice { obj: Py<PyAny>, start: u32, end: u32 },
}

enum PyItem {
    Element(Source),
    Scalar(Vec<u8>),
}

impl PyItem {
    /// Run `f` against the item's underlying bytes. For Element/Borrowed this
    /// re-acquires the Python buffer; for Element/Owned and Scalar it borrows
    /// the inline Vec.
    fn with_bytes<R>(
        &self,
        py: Python<'_>,
        f: impl FnOnce(&[u8]) -> R,
    ) -> PyResult<R> {
        match self {
            PyItem::Scalar(v) => Ok(f(v.as_slice())),
            PyItem::Element(Source::Owned(v)) => Ok(f(v.as_slice())),
            PyItem::Element(Source::Borrowed(obj)) => with_xml_input(obj.bind(py), f),
            PyItem::Element(Source::BorrowedSlice { obj, start, end }) => {
                with_xml_input(obj.bind(py), |bytes| {
                    f(&bytes[*start as usize..*end as usize])
                })
            }
        }
    }

    /// Item text (text content for elements, raw value for scalars).
    fn text(&self, py: Python<'_>) -> PyResult<Vec<u8>> {
        match self {
            PyItem::Scalar(v) => Ok(v.clone()),
            PyItem::Element(_) => self.with_bytes(py, extract_element_text),
        }
    }

    /// Cheap clone for use when materializing a single-item Result from one
    /// entry of a multi-item parent. Borrowed elements bump the underlying
    /// PyObject refcount; owned bytes are copied.
    fn clone_ref(&self, py: Python<'_>) -> Self {
        match self {
            PyItem::Scalar(v) => PyItem::Scalar(v.clone()),
            PyItem::Element(Source::Owned(v)) => PyItem::Element(Source::Owned(Arc::clone(v))),
            PyItem::Element(Source::Borrowed(obj)) => {
                PyItem::Element(Source::Borrowed(obj.clone_ref(py)))
            }
            PyItem::Element(Source::BorrowedSlice { obj, start, end }) => {
                PyItem::Element(Source::BorrowedSlice {
                    obj: obj.clone_ref(py),
                    start: *start,
                    end: *end,
                })
            }
        }
    }
}

/// Convert `engine::Item` results to `PyItem`. Engine output is always Owned;
/// Borrowed sources are only produced by `py_parse`.
fn lift(items: Vec<Item>) -> Vec<PyItem> {
    items
        .into_iter()
        .map(|it| match it {
            Item::Element(b) => PyItem::Element(Source::Owned(Arc::new(b))),
            Item::Scalar(b) | Item::Text(b) => PyItem::Scalar(b),
            // ElementRef without a source — shouldn't happen for borrowable=false runs.
            Item::ElementRef { .. } => PyItem::Element(Source::Owned(Arc::new(Vec::new()))),
        })
        .collect()
}

/// Like `lift`, but resolves `Item::ElementRef` items as `Source::BorrowedSlice`
/// backed by the given Python object. Called when the input is an immutable
/// `PyBytes`/`PyString` that outlives the Result.
fn lift_with_source(items: Vec<Item>, source: &Py<PyAny>, py: Python<'_>) -> Vec<PyItem> {
    items
        .into_iter()
        .map(|it| match it {
            Item::Element(b) => PyItem::Element(Source::Owned(Arc::new(b))),
            Item::ElementRef { start, end } => PyItem::Element(Source::BorrowedSlice {
                obj: source.clone_ref(py),
                start,
                end,
            }),
            Item::Scalar(b) | Item::Text(b) => PyItem::Scalar(b),
        })
        .collect()
}

/// Returns true if the XML bytes contain at least one child element inside the
/// root element. Used by `Result.type_()` to distinguish dict-like elements
/// from string-like (text-only) elements.
fn element_has_child_elements(bytes: &[u8]) -> bool {
    use quick_xml::events::Event;
    use quick_xml::reader::Reader;
    let mut reader = Reader::from_reader(bytes);
    reader.config_mut().trim_text(false);
    reader.config_mut().expand_empty_elements = false;
    reader.config_mut().check_end_names = false;
    let mut entered_root = false;
    loop {
        match reader.read_event() {
            Ok(Event::Start(_)) => {
                if !entered_root {
                    entered_root = true;
                    continue;
                }
                return true;
            }
            Ok(Event::Empty(_)) => {
                if !entered_root {
                    return false;
                }
                return true;
            }
            Ok(Event::End(_)) | Ok(Event::Eof) => return false,
            _ => continue,
        }
    }
}

/// Byte range of a direct child element inside an element's full bytes,
/// together with that child's tag name. Used by the dict-mode iterators and
/// `__getitem__[str]` to slice fragments lazily.
struct ChildSpan {
    name: Vec<u8>,
    start: usize,
    end: usize,
}

/// Walk the full bytes of an element (`<root ...>...</root>` or `<root .../>`)
/// and return the direct children in document order. Each entry records the
/// child's tag name and the byte range covering its full `<tag …>…</tag>` (or
/// `<tag …/>`) form within the parent bytes.
fn collect_direct_children(bytes: &[u8]) -> Vec<ChildSpan> {
    use quick_xml::events::Event;
    use quick_xml::reader::Reader;
    let mut out: Vec<ChildSpan> = Vec::new();
    let mut reader = Reader::from_reader(bytes);
    reader.config_mut().trim_text(false);
    reader.config_mut().expand_empty_elements = false;
    reader.config_mut().check_end_names = false;
    let mut entered_root = false;
    loop {
        let pos_before = reader.buffer_position() as usize;
        match reader.read_event() {
            Ok(Event::Start(s)) => {
                if !entered_root {
                    entered_root = true;
                    continue;
                }
                // qn borrows from `bytes` (slice-backed reader: no copy needed).
                let qn_raw = s.name().into_inner();
                let qn: Vec<u8> = qn_raw.to_vec();
                let _ = reader.read_to_end(quick_xml::name::QName(qn_raw));
                let pos_after = reader.buffer_position() as usize;
                out.push(ChildSpan { name: qn, start: pos_before, end: pos_after });
            }
            Ok(Event::Empty(s)) => {
                if !entered_root {
                    return out;
                }
                let qn: Vec<u8> = s.name().into_inner().to_vec();
                let pos_after = reader.buffer_position() as usize;
                out.push(ChildSpan { name: qn, start: pos_before, end: pos_after });
            }
            Ok(Event::End(_)) | Ok(Event::Eof) => return out,
            _ => continue,
        }
    }
}

/// Project a child-span list to the unique tag names in first-occurrence order.
fn unique_child_names(children: &[ChildSpan]) -> Vec<Vec<u8>> {
    let mut seen: HashSet<&[u8]> = HashSet::new();
    let mut out: Vec<Vec<u8>> = Vec::new();
    for c in children {
        if seen.insert(c.name.as_slice()) {
            out.push(c.name.clone());
        }
    }
    out
}

/// What kind of value the Result holds. Drives `__getitem__`/`__iter__` and
/// determines whether `keys/values/items` are valid.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Empty,
    /// Single scalar OR single text-only element — both behave like a string.
    ScalarLike,
    /// Single element that has child elements — behaves like a dict keyed by
    /// child tag name.
    DictElement,
    /// Multiple items — behaves like a list of single-item Results.
    Multi,
}

fn mode_label(m: Mode) -> &'static str {
    match m {
        Mode::Empty => "empty",
        Mode::ScalarLike => "scalar",
        Mode::DictElement => "dict",
        Mode::Multi => "multi",
    }
}

enum ScalarKind {
    Int(i64),
    Float(f64),
    Str,
}

fn classify_scalar(s: &str) -> ScalarKind {
    if let Ok(n) = s.parse::<i64>() {
        return ScalarKind::Int(n);
    }
    if let Ok(f) = s.parse::<f64>() {
        return ScalarKind::Float(f);
    }
    ScalarKind::Str
}

/// Python `Result` class — wraps a list of matched items. Constructed by
/// `py_get` / `py_parse` / `py_get_many` etc. and dispatches `__getitem__`,
/// `__iter__`, `to_str`, `to_int`, and so on based on the number and type of
/// items held (Empty / ScalarLike / DictElement / Multi mode).
#[pyclass(name = "Result", module = "pygxml._pygxml", frozen)]
struct PyResult_ {
    items: Vec<PyItem>,
}

impl PyResult_ {
    fn new(items: Vec<PyItem>) -> Self {
        PyResult_ { items }
    }

    fn first_text(&self, py: Python<'_>) -> PyResult<Option<Vec<u8>>> {
        match self.items.first() {
            Some(it) => it.text(py).map(Some),
            None => Ok(None),
        }
    }

    /// Run `f` against the first item's text bytes, borrowing Scalar bytes in
    /// place (no clone). Returns `None` only when the Result holds no items.
    /// Used by the numeric/bool accessors, which tolerate (lossy) UTF-8.
    fn first_text_with<R>(
        &self,
        py: Python<'_>,
        f: impl FnOnce(&[u8]) -> R,
    ) -> PyResult<Option<R>> {
        match self.items.first() {
            Some(PyItem::Scalar(v)) => Ok(Some(f(v.as_slice()))),
            Some(it) => {
                let bytes = it.text(py)?;
                Ok(Some(f(&bytes)))
            }
            None => Ok(None),
        }
    }

    /// First item's text as a trimmed `String`, surfacing UTF-8 errors. Avoids
    /// the Scalar byte-buffer clone that `first_text` performs. Used by `type_`
    /// and `value`, which need the textual form.
    fn first_text_string_trimmed(&self, py: Python<'_>) -> PyResult<String> {
        match self.items.first() {
            Some(PyItem::Scalar(v)) => std::str::from_utf8(v.as_slice())
                .map(|s| s.trim().to_owned())
                .map_err(|e| PyValueError::new_err(e.to_string())),
            Some(it) => {
                let bytes = it.text(py)?;
                String::from_utf8(bytes)
                    .map(|s| s.trim().to_owned())
                    .map_err(|e| PyValueError::new_err(e.to_string()))
            }
            None => Ok(String::new()),
        }
    }

    fn all_strings(&self, py: Python<'_>) -> PyResult<Vec<String>> {
        let mut out = Vec::with_capacity(self.items.len());
        for it in &self.items {
            let bytes = it.text(py)?;
            out.push(String::from_utf8(bytes).map_err(|e| PyValueError::new_err(e.to_string()))?);
        }
        Ok(out)
    }

    /// Classify the Result for dispatching `__getitem__`/`__iter__`.
    fn mode(&self, py: Python<'_>) -> PyResult<Mode> {
        if self.items.is_empty() {
            return Ok(Mode::Empty);
        }
        if self.items.len() > 1 {
            return Ok(Mode::Multi);
        }
        match &self.items[0] {
            PyItem::Scalar(_) => Ok(Mode::ScalarLike),
            it @ PyItem::Element(_) => {
                let has_children = it.with_bytes(py, element_has_child_elements)?;
                Ok(if has_children { Mode::DictElement } else { Mode::ScalarLike })
            }
        }
    }

    /// Materialize the bytes of a DictElement-mode Result. Returns `None` if
    /// the Result is not in DictElement mode (caller should classify first).
    fn dict_element_bytes(&self, py: Python<'_>) -> PyResult<Option<Vec<u8>>> {
        if self.items.len() != 1 {
            return Ok(None);
        }
        match &self.items[0] {
            PyItem::Scalar(_) => Ok(None),
            it @ PyItem::Element(_) => {
                let bytes = it.with_bytes(py, |b| b.to_vec())?;
                if !element_has_child_elements(&bytes) {
                    return Ok(None);
                }
                Ok(Some(bytes))
            }
        }
    }
}

#[pymethods]
impl PyResult_ {
    fn exists(&self) -> bool {
        !self.items.is_empty()
    }

    fn to_int(&self, py: Python<'_>) -> PyResult<i64> {
        let n = self.first_text_with(py, |bytes| {
            let s = std::str::from_utf8(bytes).unwrap_or("").trim();
            if s.is_empty() {
                return 0;
            }
            if let Ok(n) = s.parse::<i64>() {
                return n;
            }
            // Fall back to float-then-truncate for "30.0" → 30.
            if let Ok(f) = s.parse::<f64>() {
                return f as i64;
            }
            0
        })?;
        Ok(n.unwrap_or(0))
    }

    fn to_float(&self, py: Python<'_>) -> PyResult<f64> {
        let f = self.first_text_with(py, |bytes| {
            std::str::from_utf8(bytes)
                .unwrap_or("")
                .trim()
                .parse::<f64>()
                .unwrap_or(0.0)
        })?;
        Ok(f.unwrap_or(0.0))
    }

    fn to_bool(&self, py: Python<'_>) -> PyResult<bool> {
        match self.mode(py)? {
            Mode::Empty => return Ok(false),
            Mode::Multi | Mode::DictElement => return Ok(true),
            Mode::ScalarLike => {}
        }
        let decided = self.first_text_with(py, |bytes| {
            let raw = std::str::from_utf8(bytes).unwrap_or("");
            match raw {
                "1" | "true" => Some(true),
                "0" | "false" => Some(false),
                r#""t""# | r#""1""# | r#""T""# => Some(true),
                r#""f""# | r#""0""# | r#""F""# => Some(false),
                r#""true""# | r#""TRUE""# | r#""True""# => Some(true),
                r#""false""# | r#""FALSE""# | r#""False""# => Some(false),
                _ => None,
            }
        })?;
        match decided {
            Some(Some(b)) => Ok(b),
            // Scalar present but not a recognized literal: numeric truthiness.
            Some(None) => Ok(self.to_int(py)? != 0),
            // No first item at all.
            None => Ok(false),
        }
    }

    fn to_str(&self, py: Python<'_>) -> PyResult<String> {
        match self.mode(py)? {
            Mode::Empty => Ok(String::new()),
            Mode::ScalarLike => {
                match self.items.first() {
                    Some(PyItem::Scalar(v)) => {
                        // Skip classify_scalar and Python object roundtrip.
                        std::str::from_utf8(v.as_slice())
                            .map(|s| s.trim().to_owned())
                            .map_err(|e| PyValueError::new_err(e.to_string()))
                    }
                    Some(it @ PyItem::Element(_)) => {
                        let bytes = it.text(py)?;
                        let s = String::from_utf8(bytes)
                            .map_err(|e| PyValueError::new_err(e.to_string()))?;
                        Ok(s.trim().to_owned())
                    }
                    None => Ok(String::new()),
                }
            }
            Mode::Multi | Mode::DictElement => self.xml_string(py),
        }
    }

    /// Return the Python type that best describes the value held by this Result:
    ///   - `None`  when the Result is empty (no match)
    ///   - `int`   for a single numeric value parseable as integer
    ///   - `float` for a single numeric value parseable as float
    ///   - `str`   for a single scalar or a text-only element
    ///   - `list`  when multiple items are present
    ///   - `dict`  for a single element that has child elements
    #[getter]
    fn type_(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        match self.mode(py)? {
            Mode::Empty => Ok(py.None()),
            Mode::ScalarLike => {
                let text = self.first_text_string_trimmed(py)?;
                match classify_scalar(&text) {
                    ScalarKind::Int(_) => Ok(PyInt::type_object(py).into_any().unbind()),
                    ScalarKind::Float(_) => Ok(PyFloat::type_object(py).into_any().unbind()),
                    ScalarKind::Str => Ok(PyString::type_object(py).into_any().unbind()),
                }
            }
            Mode::Multi => Ok(PyList::type_object(py).into_any().unbind()),
            Mode::DictElement => Ok(PyDict::type_object(py).into_any().unbind()),
        }
    }

    /// Return the value held by this Result in its natural Python type:
    ///   - `None`          when the Result is empty
    ///   - `int` / `float` for numeric scalars
    ///   - `str`           for text scalars
    ///   - `list[Result]`  for multiple items
    ///   - `dict[str, Result]` for an element with child elements
    #[getter]
    fn value(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        match self.mode(py)? {
            Mode::Empty => Ok(py.None()),
            Mode::ScalarLike => {
                let text = self.first_text_string_trimmed(py)?;
                match classify_scalar(&text) {
                    ScalarKind::Int(n) => Ok(n.into_pyobject(py)?.into_any().unbind()),
                    ScalarKind::Float(f) => Ok(f.into_pyobject(py)?.into_any().unbind()),
                    ScalarKind::Str => Ok(PyString::new(py, &text).into_any().unbind()),
                }
            }
            Mode::Multi => {
                let list = PyList::empty(py);
                for it in &self.items {
                    let item = it.clone_ref(py);
                    let result = Py::new(py, PyResult_::new(vec![item]))?;
                    list.append(result.into_bound(py))?;
                }
                Ok(list.into_any().unbind())
            }
            Mode::DictElement => {
                let bytes = self
                    .dict_element_bytes(py)?
                    .expect("DictElement mode implies element bytes are available");
                let children = collect_direct_children(&bytes);
                let unique_names = unique_child_names(&children);
                let dict = PyDict::new(py);
                for name in &unique_names {
                    let items: Vec<PyItem> = children
                        .iter()
                        .filter(|c| &c.name == name)
                        .map(|c| PyItem::Element(Source::Owned(Arc::new(bytes[c.start..c.end].to_vec()))))
                        .collect();
                    let result = Py::new(py, PyResult_::new(items))?;
                    let key = String::from_utf8(name.clone())
                        .map_err(|e| PyValueError::new_err(e.to_string()))?;
                    dict.set_item(key, result.into_bound(py))?;
                }
                Ok(dict.into_any().unbind())
            }
        }
    }

    fn get(&self, py: Python<'_>, path: &str) -> PyResult<Py<Self>> {
        // A Multi-mode receiver is rejected: descending a path into "many
        // elements at once" is the same ambiguity the path-level Auto rule
        // forbids. Callers must iterate (`for x in result: x.get(...)`) or
        // index (`result[i].get(...)`) explicitly.
        if self.items.len() > 1 {
            return Err(PyValueError::new_err(
                "Result.get(path) is not supported on a multi-element Result; \
                 iterate the Result or pick an index first",
            ));
        }
        if path.is_empty() {
            return Py::new(py, PyResult_::new(Vec::new()));
        }
        let prog = compile_path(path)?;
        let mut out: Vec<PyItem> = Vec::new();
        for it in &self.items {
            if let PyItem::Element(src) = it {
                // Borrowed = the original raw input (from `parse(data)`); the
                // path matches elements at the document root just like
                // `pygxml.get(data, path)`.
                // Owned / BorrowedSlice = a captured fragment; descend *into*
                // the fragment's outer element so `book.get("@id")` reads
                // book's id and `book.get("title")` finds the inner <title>.
                let chunk = match src {
                    Source::Borrowed(_) => it
                        .with_bytes(py, |bytes| engine::run(&prog, bytes))?
                        .map_err(engine_err_to_py)?,
                    Source::Owned(_) | Source::BorrowedSlice { .. } => it
                        .with_bytes(py, |bytes| engine::run_within(&prog, bytes))?
                        .map_err(engine_err_to_py)?,
                };
                out.extend(lift(chunk));
            }
            // Scalar items have no children — skip.
        }
        Py::new(py, PyResult_::new(out))
    }

    fn get_many(
        &self,
        py: Python<'_>,
        paths: &Bound<'_, PyList>,
    ) -> PyResult<Py<PyList>> {
        if self.items.len() > 1 {
            return Err(PyValueError::new_err(
                "Result.get_many(paths) is not supported on a multi-element Result; \
                 iterate the Result or pick an index first",
            ));
        }

        let mut programs: Vec<Option<Arc<Program>>> = Vec::with_capacity(paths.len());
        for item in paths.iter() {
            if let Ok(s) = item.extract::<&str>() {
                programs.push(compile_path_opt(s)?);
            } else if let Ok(p) = item.extract::<PyRef<PyPath>>() {
                programs.push(Some(Arc::clone(&p.program)));
            } else {
                return Err(PyTypeError::new_err(
                    "paths must be a list of str or pygxml.Path",
                ));
            }
        }

        let list = PyList::empty(py);
        if programs.is_empty() {
            return Ok(list.into());
        }

        let active: Vec<&Arc<Program>> = programs.iter().filter_map(|o| o.as_ref()).collect();

        for it in &self.items {
            if let PyItem::Element(src) = it {
                let mut engine_results = if active.is_empty() {
                    vec![]
                } else {
                    let prog_refs: Vec<&path::Program> =
                        active.iter().map(|p| p.as_ref()).collect();
                    match src {
                        Source::Borrowed(_) => it
                            .with_bytes(py, |bytes| run_many(&prog_refs, bytes, false))?
                            .map_err(engine_err_to_py)?,
                        Source::Owned(_) | Source::BorrowedSlice { .. } => it
                            .with_bytes(py, |bytes| run_many_within(&prog_refs, bytes))?
                            .map_err(engine_err_to_py)?,
                    }
                };
                let mut engine_iter = engine_results.drain(..);
                for opt in &programs {
                    let r = if opt.is_some() {
                        Py::new(py, PyResult_::new(lift(engine_iter.next().unwrap())))?
                    } else {
                        Py::new(py, PyResult_::new(Vec::new()))?
                    };
                    list.append(r)?;
                }
                return Ok(list.into());
            }
        }

        // Empty Result or scalar-only items: return N empty Results.
        for _ in &programs {
            let r = Py::new(py, PyResult_::new(Vec::new()))?;
            list.append(r)?;
        }
        Ok(list.into())
    }

    /// Return a view of the dict-mode Result's child element keys (unique,
    /// first-occurrence order). Errors with `TypeError` for non-dict modes.
    fn keys(slf: PyRef<'_, Self>, py: Python<'_>) -> PyResult<Py<ResultKeysView>> {
        let mode = slf.mode(py)?;
        if mode != Mode::DictElement {
            return Err(PyTypeError::new_err(format!(
                "Result.keys() requires a dict-like Result (single element with children); got {}",
                mode_label(mode)
            )));
        }
        let parent: Py<Self> = slf.into();
        Py::new(py, ResultKeysView { parent })
    }

    /// Return a view of the dict-mode Result's child values. For each unique
    /// key, the value is a Result over all direct children with that tag.
    fn values(slf: PyRef<'_, Self>, py: Python<'_>) -> PyResult<Py<ResultValuesView>> {
        let mode = slf.mode(py)?;
        if mode != Mode::DictElement {
            return Err(PyTypeError::new_err(format!(
                "Result.values() requires a dict-like Result (single element with children); got {}",
                mode_label(mode)
            )));
        }
        let parent: Py<Self> = slf.into();
        Py::new(py, ResultValuesView { parent })
    }

    /// Return a view of the dict-mode Result's `(key, value_Result)` pairs.
    fn items(slf: PyRef<'_, Self>, py: Python<'_>) -> PyResult<Py<ResultItemsView>> {
        let mode = slf.mode(py)?;
        if mode != Mode::DictElement {
            return Err(PyTypeError::new_err(format!(
                "Result.items() requires a dict-like Result (single element with children); got {}",
                mode_label(mode)
            )));
        }
        let parent: Py<Self> = slf.into();
        Py::new(py, ResultItemsView { parent })
    }

    fn __bool__(&self, py: Python<'_>) -> PyResult<bool> {
        let val = self.value(py)?;
        val.bind(py).is_truthy()
    }

    fn __len__(&self) -> usize {
        self.items.len()
    }

    fn __int__(&self, py: Python<'_>) -> PyResult<i64> {
        let val = self.value(py)?;
        let builtins = PyModule::import(py, "builtins")?;
        let result = builtins.getattr("int")?.call1((val.bind(py),))?;
        result.extract::<i64>()
    }

    fn __float__(&self, py: Python<'_>) -> PyResult<f64> {
        let val = self.value(py)?;
        let builtins = PyModule::import(py, "builtins")?;
        let result = builtins.getattr("float")?.call1((val.bind(py),))?;
        result.extract::<f64>()
    }

    /// Lazy iteration whose element type depends on the Result's mode:
    ///   - Empty       → yields nothing
    ///   - ScalarLike  → yields each Unicode code point of the text as `str`
    ///   - Multi       → yields a single-item `Result` per parent item
    ///   - DictElement → yields each unique child tag name (as `str`) in
    ///                   first-occurrence order
    fn __iter__(slf: PyRef<'_, Self>, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let mode = slf.mode(py)?;
        match mode {
            Mode::Empty => {
                let list = PyList::empty(py);
                Ok(list.try_iter()?.into_any().unbind())
            }
            Mode::ScalarLike => {
                let text = slf.first_text(py)?.unwrap_or_default();
                let iter = ResultCharIter { text, offset: 0 };
                Ok(Py::new(py, iter)?.into_any())
            }
            Mode::Multi => {
                let parent: Py<Self> = slf.into();
                let iter = ResultItemIter { parent, cursor: 0 };
                Ok(Py::new(py, iter)?.into_any())
            }
            Mode::DictElement => {
                let bytes = slf
                    .dict_element_bytes(py)?
                    .expect("DictElement mode implies element bytes are available");
                let children = collect_direct_children(&bytes);
                let names: Vec<String> = unique_child_names(&children)
                    .into_iter()
                    .map(|b| String::from_utf8(b).expect("XML tag names are valid UTF-8"))
                    .collect();
                let iter = ResultChildKeyIter { names, cursor: 0 };
                Ok(Py::new(py, iter)?.into_any())
            }
        }
    }

    /// Mode-aware indexing:
    ///   - ScalarLike: int → Nth code point as `str`; slice → substring;
    ///                 str → TypeError
    ///   - Multi:      int → single-item Result; slice → range Result;
    ///                 str → TypeError
    ///   - DictElement: str → Result over all matching children; int/slice →
    ///                  KeyError
    ///   - Empty:      int → IndexError; slice → empty Result; str → TypeError
    fn __getitem__(&self, py: Python<'_>, key: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
        let mode = self.mode(py)?;

        if let Ok(s) = key.cast::<PyString>() {
            let key_str: String = s.extract()?;
            return self.getitem_str(py, mode, &key_str);
        }
        if let Ok(slice) = key.cast::<PySlice>() {
            return self.getitem_slice(py, mode, slice);
        }
        // Anything else: try to coerce to int. PyTypeError on non-integer.
        let idx: isize = key.extract().map_err(|_| {
            PyTypeError::new_err("Result indices must be integers, slices, or strings")
        })?;
        self.getitem_int(py, mode, idx)
    }

    fn __richcmp__(
        &self,
        py: Python<'_>,
        other: &Bound<'_, PyAny>,
        op: CompareOp,
    ) -> PyResult<Py<PyAny>> {
        let result = match op {
            CompareOp::Eq | CompareOp::Ne => {
                let eq = self.eq_helper(py, other)?;
                let value = if matches!(op, CompareOp::Eq) { eq } else { !eq };
                pyo3::types::PyBool::new(py, value)
                    .to_owned()
                    .into_any()
                    .unbind()
            }
            _ => py.NotImplemented(),
        };
        Ok(result)
    }

    fn __repr__(&self, py: Python<'_>) -> PyResult<String> {
        match self.mode(py)? {
            Mode::DictElement => {
                let bytes = self
                    .dict_element_bytes(py)?
                    .expect("DictElement mode implies element bytes are available");
                let children = collect_direct_children(&bytes);
                let names = unique_child_names(&children);
                let key_strs: Vec<String> = names
                    .iter()
                    .map(|b| String::from_utf8_lossy(b).into_owned())
                    .collect();
                let keys_repr = if key_strs.len() >= 3 {
                    format!("[{}, ...]", key_strs[..2].join(", "))
                } else {
                    format!("[{}]", key_strs.join(", "))
                };
                Ok(format!("<Result type=dict, keys={}>", keys_repr))
            }
            Mode::Multi => {
                let total = self.items.len();
                let show = if total >= 3 { 2 } else { total };
                let mut reprs: Vec<String> = Vec::with_capacity(show);
                for it in self.items.iter().take(show) {
                    let single = PyResult_::new(vec![it.clone_ref(py)]);
                    reprs.push(single.__repr__(py)?);
                }
                let list_repr = if total >= 3 {
                    format!("[{}, ...]", reprs[..2].join(", "))
                } else {
                    format!("[{}]", reprs.join(", "))
                };
                Ok(format!("<Result type=list, value={}>", list_repr))
            }
            _ => {
                let val = self.value(py)?;
                Ok(val.bind(py).str()?.to_string_lossy().into_owned())
            }
        }
    }
}

impl PyResult_ {
    fn xml_string(&self, py: Python<'_>) -> PyResult<String> {
        if self.items.is_empty() {
            return Ok(String::new());
        }
        let mut parts: Vec<String> = Vec::with_capacity(self.items.len());
        for it in &self.items {
            let s = it.with_bytes(py, |b| String::from_utf8_lossy(b).into_owned())?;
            parts.push(s);
        }
        Ok(parts.join("\n"))
    }

    fn eq_helper(&self, py: Python<'_>, other: &Bound<'_, PyAny>) -> PyResult<bool> {
        // Result == Result → compare items as strings
        if let Ok(other_res) = other.extract::<PyRef<'_, PyResult_>>() {
            let a = self.all_strings(py)?;
            let b = other_res.all_strings(py)?;
            return Ok(a == b);
        }
        // Result == list[str] → element-wise string compare
        if let Ok(list) = other.cast::<PyList>() {
            let mut other_strs: Vec<String> = Vec::with_capacity(list.len());
            for v in list.iter() {
                match v.extract::<String>() {
                    Ok(s) => other_strs.push(s),
                    Err(_) => return Ok(false),
                }
            }
            let a = self.all_strings(py)?;
            return Ok(a == other_strs);
        }
        // Result == str → first item's text == str
        if let Ok(s) = other.extract::<String>() {
            let first = self.first_text(py)?;
            return Ok(match first {
                Some(b) => match std::str::from_utf8(&b) {
                    Ok(t) => t == s,
                    Err(_) => false,
                },
                None => false,
            });
        }
        Ok(false)
    }

    fn getitem_str(&self, py: Python<'_>, mode: Mode, key: &str) -> PyResult<Py<PyAny>> {
        match mode {
            Mode::DictElement => {
                let bytes = self
                    .dict_element_bytes(py)?
                    .expect("DictElement mode implies element bytes are available");
                let children = collect_direct_children(&bytes);
                let mut out: Vec<PyItem> = Vec::new();
                for c in &children {
                    if c.name.as_slice() == key.as_bytes() {
                        out.push(PyItem::Element(Source::Owned(Arc::new(bytes[c.start..c.end].to_vec()))));
                    }
                }
                if out.is_empty() {
                    return Err(PyKeyError::new_err(key.to_string()));
                }
                let r = Py::new(py, PyResult_::new(out))?;
                Ok(r.into_any())
            }
            other => Err(PyTypeError::new_err(format!(
                "string keys are only valid on dict-like Result; this Result is {}",
                mode_label(other)
            ))),
        }
    }

    fn getitem_int(&self, py: Python<'_>, mode: Mode, idx: isize) -> PyResult<Py<PyAny>> {
        match mode {
            Mode::Empty => Err(PyIndexError::new_err("Result index out of range")),
            Mode::ScalarLike => {
                let bytes = self.first_text(py)?.unwrap_or_default();
                let text = String::from_utf8(bytes)
                    .map_err(|e| PyValueError::new_err(e.to_string()))?;
                let n = text.chars().count() as isize;
                let mut i = idx;
                if i < 0 {
                    i += n;
                }
                if i < 0 || i >= n {
                    return Err(PyIndexError::new_err("Result index out of range"));
                }
                let c = text.chars().nth(i as usize).unwrap();
                Ok(PyString::new(py, &c.to_string()).into_any().unbind())
            }
            Mode::Multi => {
                let n = self.items.len() as isize;
                let mut i = idx;
                if i < 0 {
                    i += n;
                }
                if i < 0 || i >= n {
                    return Err(PyIndexError::new_err("Result index out of range"));
                }
                let item = self.items[i as usize].clone_ref(py);
                let r = Py::new(py, PyResult_::new(vec![item]))?;
                Ok(r.into_any())
            }
            Mode::DictElement => Err(PyKeyError::new_err(idx)),
        }
    }

    fn getitem_slice(
        &self,
        py: Python<'_>,
        mode: Mode,
        slice: &Bound<'_, PySlice>,
    ) -> PyResult<Py<PyAny>> {
        match mode {
            Mode::Empty => {
                let r = Py::new(py, PyResult_::new(Vec::new()))?;
                Ok(r.into_any())
            }
            Mode::ScalarLike => {
                let bytes = self.first_text(py)?.unwrap_or_default();
                let text = String::from_utf8(bytes)
                    .map_err(|e| PyValueError::new_err(e.to_string()))?;
                let chars: Vec<char> = text.chars().collect();
                let indices = slice.indices(chars.len() as isize)?;
                let mut out = String::new();
                let mut i = indices.start;
                while (indices.step > 0 && i < indices.stop)
                    || (indices.step < 0 && i > indices.stop)
                {
                    if i >= 0 && (i as usize) < chars.len() {
                        out.push(chars[i as usize]);
                    }
                    i += indices.step;
                }
                Ok(PyString::new(py, &out).into_any().unbind())
            }
            Mode::Multi => {
                let indices = slice.indices(self.items.len() as isize)?;
                let mut out: Vec<PyItem> = Vec::new();
                let mut i = indices.start;
                while (indices.step > 0 && i < indices.stop)
                    || (indices.step < 0 && i > indices.stop)
                {
                    if i >= 0 && (i as usize) < self.items.len() {
                        out.push(self.items[i as usize].clone_ref(py));
                    }
                    i += indices.step;
                }
                let r = Py::new(py, PyResult_::new(out))?;
                Ok(r.into_any())
            }
            Mode::DictElement => Err(PyKeyError::new_err(
                "slice keys are not supported on dict-like Result",
            )),
        }
    }
}

// ---- Iterators -------------------------------------------------------------

/// Yields each Unicode code point of a Scalar/TextElement Result one at a
/// time. The text bytes are materialized once at iter creation; iteration
/// itself walks UTF-8 code points without further allocation per step.
#[pyclass(name = "ResultCharIter", module = "pygxml._pygxml")]
struct ResultCharIter {
    text: Vec<u8>,
    offset: usize,
}

#[pymethods]
impl ResultCharIter {
    fn __iter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    fn __next__(&mut self) -> PyResult<Option<String>> {
        if self.offset >= self.text.len() {
            return Ok(None);
        }
        let s = std::str::from_utf8(&self.text[self.offset..])
            .map_err(|e| PyValueError::new_err(e.to_string()))?;
        match s.chars().next() {
            Some(c) => {
                self.offset += c.len_utf8();
                Ok(Some(c.to_string()))
            }
            None => Ok(None),
        }
    }
}

/// Yields a single-item `Result` for each entry of a Multi-mode parent.
/// Holds only a cursor and a borrowed reference to the parent Result; each
/// step clones one `PyItem` (cheap) and wraps it in a fresh `Result`.
#[pyclass(name = "ResultItemIter", module = "pygxml._pygxml")]
struct ResultItemIter {
    parent: Py<PyResult_>,
    cursor: usize,
}

#[pymethods]
impl ResultItemIter {
    fn __iter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    fn __next__(&mut self, py: Python<'_>) -> PyResult<Option<Py<PyResult_>>> {
        let parent = self.parent.get();
        if self.cursor >= parent.items.len() {
            return Ok(None);
        }
        let item = parent.items[self.cursor].clone_ref(py);
        self.cursor += 1;
        Py::new(py, PyResult_::new(vec![item])).map(Some)
    }
}

/// Yields each unique child tag name of a DictElement-mode Result.
#[pyclass(name = "ResultChildKeyIter", module = "pygxml._pygxml")]
struct ResultChildKeyIter {
    names: Vec<String>,
    cursor: usize,
}

#[pymethods]
impl ResultChildKeyIter {
    fn __iter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    fn __next__(&mut self) -> PyResult<Option<String>> {
        if self.cursor >= self.names.len() {
            return Ok(None);
        }
        let s = self.names[self.cursor].clone();
        self.cursor += 1;
        Ok(Some(s))
    }
}

/// Yields a `Result` per unique child tag, containing every direct child with
/// that tag. Bytes for matching children are sliced out of the parent bytes
/// only when each value is produced.
#[pyclass(name = "ResultChildValueIter", module = "pygxml._pygxml")]
struct ResultChildValueIter {
    bytes: Vec<u8>,
    children: Vec<ChildSpan>,
    unique_names: Vec<Vec<u8>>,
    cursor: usize,
}

#[pymethods]
impl ResultChildValueIter {
    fn __iter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    fn __next__(&mut self, py: Python<'_>) -> PyResult<Option<Py<PyResult_>>> {
        if self.cursor >= self.unique_names.len() {
            return Ok(None);
        }
        let name = &self.unique_names[self.cursor];
        self.cursor += 1;
        let items: Vec<PyItem> = self
            .children
            .iter()
            .filter(|c| &c.name == name)
            .map(|c| PyItem::Element(Source::Owned(Arc::new(self.bytes[c.start..c.end].to_vec()))))
            .collect();
        Py::new(py, PyResult_::new(items)).map(Some)
    }
}

/// Yields `(key_str, value_Result)` tuples, one per unique child tag.
#[pyclass(name = "ResultChildItemIter", module = "pygxml._pygxml")]
struct ResultChildItemIter {
    bytes: Vec<u8>,
    children: Vec<ChildSpan>,
    unique_names: Vec<String>,
    cursor: usize,
}

#[pymethods]
impl ResultChildItemIter {
    fn __iter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    fn __next__(&mut self, py: Python<'_>) -> PyResult<Option<Py<PyAny>>> {
        if self.cursor >= self.unique_names.len() {
            return Ok(None);
        }
        let key_str = self.unique_names[self.cursor].clone();
        self.cursor += 1;
        let name_bytes = key_str.as_bytes();
        let items: Vec<PyItem> = self
            .children
            .iter()
            .filter(|c| c.name.as_slice() == name_bytes)
            .map(|c| PyItem::Element(Source::Owned(Arc::new(self.bytes[c.start..c.end].to_vec()))))
            .collect();
        let value_result = Py::new(py, PyResult_::new(items))?;
        let key_obj = PyString::new(py, &key_str).into_any();
        let value_obj = value_result.into_bound(py).into_any();
        let tuple = PyTuple::new(py, &[key_obj, value_obj])?;
        Ok(Some(tuple.into_any().unbind()))
    }
}

// ---- View objects ----------------------------------------------------------

/// dict.keys()-like view over a DictElement Result. Re-iterable; each call to
/// __iter__ produces a fresh, lazy iterator.
#[pyclass(name = "ResultKeysView", module = "pygxml._pygxml", frozen)]
struct ResultKeysView {
    parent: Py<PyResult_>,
}

#[pymethods]
impl ResultKeysView {
    fn __iter__(&self, py: Python<'_>) -> PyResult<Py<ResultChildKeyIter>> {
        let parent = self.parent.get();
        let bytes = parent
            .dict_element_bytes(py)?
            .expect("DictElement mode implies element bytes are available");
        let children = collect_direct_children(&bytes);
        let names: Vec<String> = unique_child_names(&children)
            .into_iter()
            .map(|b| String::from_utf8(b).expect("XML tag names are valid UTF-8"))
            .collect();
        Py::new(py, ResultChildKeyIter { names, cursor: 0 })
    }

    fn __len__(&self, py: Python<'_>) -> PyResult<usize> {
        let parent = self.parent.get();
        let bytes = parent
            .dict_element_bytes(py)?
            .expect("DictElement mode implies element bytes are available");
        let children = collect_direct_children(&bytes);
        Ok(unique_child_names(&children).len())
    }

    fn __contains__(&self, py: Python<'_>, key: &Bound<'_, PyAny>) -> PyResult<bool> {
        let key_str: String = match key.extract() {
            Ok(s) => s,
            Err(_) => return Ok(false),
        };
        let parent = self.parent.get();
        let bytes = parent
            .dict_element_bytes(py)?
            .expect("DictElement mode implies element bytes are available");
        let children = collect_direct_children(&bytes);
        Ok(children.iter().any(|c| c.name.as_slice() == key_str.as_bytes()))
    }

    fn __repr__(&self, py: Python<'_>) -> PyResult<String> {
        let parent = self.parent.get();
        let bytes = parent
            .dict_element_bytes(py)?
            .expect("DictElement mode implies element bytes are available");
        let children = collect_direct_children(&bytes);
        let names = unique_child_names(&children);
        let strs: Vec<String> = names
            .into_iter()
            .map(|b| String::from_utf8(b).unwrap_or_default())
            .collect();
        Ok(format!("result_keys({:?})", strs))
    }
}

/// dict.values()-like view over a DictElement Result.
#[pyclass(name = "ResultValuesView", module = "pygxml._pygxml", frozen)]
struct ResultValuesView {
    parent: Py<PyResult_>,
}

#[pymethods]
impl ResultValuesView {
    fn __iter__(&self, py: Python<'_>) -> PyResult<Py<ResultChildValueIter>> {
        let parent = self.parent.get();
        let bytes = parent
            .dict_element_bytes(py)?
            .expect("DictElement mode implies element bytes are available");
        let children = collect_direct_children(&bytes);
        let unique_names = unique_child_names(&children);
        Py::new(
            py,
            ResultChildValueIter { bytes, children, unique_names, cursor: 0 },
        )
    }

    fn __len__(&self, py: Python<'_>) -> PyResult<usize> {
        let parent = self.parent.get();
        let bytes = parent
            .dict_element_bytes(py)?
            .expect("DictElement mode implies element bytes are available");
        let children = collect_direct_children(&bytes);
        Ok(unique_child_names(&children).len())
    }

    fn __repr__(&self) -> String {
        "result_values([...])".to_string()
    }
}

/// dict.items()-like view over a DictElement Result.
#[pyclass(name = "ResultItemsView", module = "pygxml._pygxml", frozen)]
struct ResultItemsView {
    parent: Py<PyResult_>,
}

#[pymethods]
impl ResultItemsView {
    fn __iter__(&self, py: Python<'_>) -> PyResult<Py<ResultChildItemIter>> {
        let parent = self.parent.get();
        let bytes = parent
            .dict_element_bytes(py)?
            .expect("DictElement mode implies element bytes are available");
        let children = collect_direct_children(&bytes);
        let unique_names: Vec<String> = unique_child_names(&children)
            .into_iter()
            .map(|b| String::from_utf8(b).expect("XML tag names are valid UTF-8"))
            .collect();
        Py::new(
            py,
            ResultChildItemIter { bytes, children, unique_names, cursor: 0 },
        )
    }

    fn __len__(&self, py: Python<'_>) -> PyResult<usize> {
        let parent = self.parent.get();
        let bytes = parent
            .dict_element_bytes(py)?
            .expect("DictElement mode implies element bytes are available");
        let children = collect_direct_children(&bytes);
        Ok(unique_child_names(&children).len())
    }

    fn __repr__(&self) -> String {
        "result_items([...])".to_string()
    }
}

// ---- Path / module exports -------------------------------------------------

#[pyclass(name = "Path", module = "pygxml._pygxml", frozen)]
struct PyPath {
    program: Arc<Program>,
    path_str: String,
}

#[pymethods]
impl PyPath {
    fn get(&self, py: Python<'_>, data: &Bound<'_, PyString>) -> PyResult<Py<PyResult_>> {
        // to_cow() is zero-copy for canonical Python strings; element terminals
        // come back as zero-copy ElementRef ranges resolved against the string.
        let cow = data.to_cow()?;
        let items = run_with(&self.program, cow.as_bytes(), true).map_err(engine_err_to_py)?;
        let source: Py<PyAny> = data.clone().into_any().unbind();
        Py::new(py, PyResult_::new(lift_with_source(items, &source, py)))
    }

    fn get_bytes(&self, py: Python<'_>, data: &Bound<'_, PyBytes>) -> PyResult<Py<PyResult_>> {
        // PyBytes is immutable — element terminals are zero-copy ElementRefs.
        let items = run_with(&self.program, data.as_bytes(), true).map_err(engine_err_to_py)?;
        let source: Py<PyAny> = data.clone().into_any().unbind();
        Py::new(py, PyResult_::new(lift_with_source(items, &source, py)))
    }

    fn get_buffer(&self, py: Python<'_>, data: &Bound<'_, PyAny>) -> PyResult<Py<PyResult_>> {
        let items = with_buffer_input(data, |bytes| engine::run(&self.program, bytes))?
            .map_err(engine_err_to_py)?;
        Py::new(py, PyResult_::new(lift(items)))
    }

    fn __repr__(&self) -> String {
        format!("Path({:?})", self.path_str)
    }
}

#[pyfunction]
#[pyo3(name = "get")]
fn py_get(py: Python<'_>, data: &Bound<'_, PyString>, path: &str) -> PyResult<Py<PyResult_>> {
    if path.is_empty() {
        return Py::new(py, PyResult_::new(Vec::new()));
    }
    let prog = compile_path(path)?;
    // to_cow() is zero-copy for canonical Python strings (UTF-8 internal repr).
    // Element terminals are zero-copy ElementRef ranges: the string object is
    // kept alive by the Result and re-borrowed (same cached UTF-8 buffer) on
    // each access, so the offsets stay valid.
    let cow = data.to_cow()?;
    let items = run_with(&prog, cow.as_bytes(), true).map_err(engine_err_to_py)?;
    let source: Py<PyAny> = data.clone().into_any().unbind();
    Py::new(py, PyResult_::new(lift_with_source(items, &source, py)))
}

#[pyfunction]
#[pyo3(name = "get_bytes")]
fn py_get_bytes(py: Python<'_>, data: &Bound<'_, PyBytes>, path: &str) -> PyResult<Py<PyResult_>> {
    if path.is_empty() {
        return Py::new(py, PyResult_::new(Vec::new()));
    }
    let prog = compile_path(path)?;
    // PyBytes is immutable — element terminals are zero-copy ElementRefs.
    let items = run_with(&prog, data.as_bytes(), true).map_err(engine_err_to_py)?;
    let source: Py<PyAny> = data.clone().into_any().unbind();
    Py::new(py, PyResult_::new(lift_with_source(items, &source, py)))
}

#[pyfunction]
#[pyo3(name = "get_buffer")]
fn py_get_buffer(py: Python<'_>, data: &Bound<'_, PyAny>, path: &str) -> PyResult<Py<PyResult_>> {
    if path.is_empty() {
        return Py::new(py, PyResult_::new(Vec::new()));
    }
    let prog = compile_path(path)?;
    let items = with_buffer_input(data, |bytes| engine::run(&prog, bytes))?
        .map_err(engine_err_to_py)?;
    Py::new(py, PyResult_::new(lift(items)))
}

#[pyfunction]
#[pyo3(name = "parse")]
fn py_parse(py: Python<'_>, data: &Bound<'_, PyAny>) -> PyResult<Py<PyResult_>> {
    // Validate the input now so callers get an immediate error rather than at
    // first .get()/.str() call. The closure does no work; we just exercise
    // the buffer/string path once.
    with_xml_input(data, |_| ())?;
    let item = PyItem::Element(Source::Borrowed(data.clone().unbind()));
    Py::new(py, PyResult_::new(vec![item]))
}

#[pyfunction]
#[pyo3(name = "validate")]
fn py_validate(data: &Bound<'_, PyAny>) -> PyResult<bool> {
    with_xml_input(data, validate)
}

fn collect_programs(paths: &Bound<'_, PyList>) -> PyResult<Vec<Option<Arc<Program>>>> {
    let mut programs: Vec<Option<Arc<Program>>> = Vec::with_capacity(paths.len());
    for item in paths.iter() {
        if let Ok(s) = item.extract::<&str>() {
            programs.push(compile_path_opt(s)?);
        } else if let Ok(p) = item.extract::<PyRef<PyPath>>() {
            programs.push(Some(Arc::clone(&p.program)));
        } else {
            return Err(PyTypeError::new_err(
                "paths must be a list of str or pygxml.Path",
            ));
        }
    }
    Ok(programs)
}

#[pyfunction]
#[pyo3(name = "get_many")]
fn py_get_many(
    py: Python<'_>,
    data: &Bound<'_, PyString>,
    paths: &Bound<'_, PyList>,
) -> PyResult<Py<PyList>> {
    let programs = collect_programs(paths)?;
    let list = PyList::empty(py);
    if programs.is_empty() {
        return Ok(list.into());
    }
    let active: Vec<&Arc<Program>> = programs.iter().filter_map(|o| o.as_ref()).collect();
    let mut engine_results = if active.is_empty() {
        vec![]
    } else {
        let prog_refs: Vec<&path::Program> = active.iter().map(|p| p.as_ref()).collect();
        // to_cow() is zero-copy for canonical Python strings (UTF-8 internal repr).
        let cow = data.to_cow()?;
        run_many(&prog_refs, cow.as_bytes(), false).map_err(engine_err_to_py)?
    };
    let mut engine_iter = engine_results.drain(..);
    let mut results: Vec<Py<PyResult_>> = Vec::with_capacity(programs.len());
    for opt in &programs {
        let r = if opt.is_some() {
            Py::new(py, PyResult_::new(lift(engine_iter.next().unwrap())))?
        } else {
            Py::new(py, PyResult_::new(Vec::new()))?
        };
        results.push(r);
    }
    Ok(PyList::new(py, results)?.into())
}

#[pyfunction]
#[pyo3(name = "get_many_bytes")]
fn py_get_many_bytes(
    py: Python<'_>,
    data: &Bound<'_, PyBytes>,
    paths: &Bound<'_, PyList>,
) -> PyResult<Py<PyList>> {
    let programs = collect_programs(paths)?;
    let list = PyList::empty(py);
    if programs.is_empty() {
        return Ok(list.into());
    }
    let active: Vec<&Arc<Program>> = programs.iter().filter_map(|o| o.as_ref()).collect();
    // PyBytes is immutable — always use zero-copy ElementRef.
    let source: Py<PyAny> = data.clone().into_any().unbind();
    let mut engine_results = if active.is_empty() {
        vec![]
    } else {
        let prog_refs: Vec<&path::Program> = active.iter().map(|p| p.as_ref()).collect();
        run_many(&prog_refs, data.as_bytes(), true).map_err(engine_err_to_py)?
    };
    let mut engine_iter = engine_results.drain(..);
    let mut results: Vec<Py<PyResult_>> = Vec::with_capacity(programs.len());
    for opt in &programs {
        let r = if opt.is_some() {
            Py::new(py, PyResult_::new(lift_with_source(engine_iter.next().unwrap(), &source, py)))?
        } else {
            Py::new(py, PyResult_::new(Vec::new()))?
        };
        results.push(r);
    }
    Ok(PyList::new(py, results)?.into())
}

#[pyfunction]
#[pyo3(name = "get_many_buffer")]
fn py_get_many_buffer(
    py: Python<'_>,
    data: &Bound<'_, PyAny>,
    paths: &Bound<'_, PyList>,
) -> PyResult<Py<PyList>> {
    let programs = collect_programs(paths)?;
    let list = PyList::empty(py);
    if programs.is_empty() {
        return Ok(list.into());
    }
    let active: Vec<&Arc<Program>> = programs.iter().filter_map(|o| o.as_ref()).collect();
    // Buffer inputs (mmap, bytearray) may be mutable — cannot use zero-copy ElementRef.
    let mut engine_results = if active.is_empty() {
        vec![]
    } else {
        let prog_refs: Vec<&path::Program> = active.iter().map(|p| p.as_ref()).collect();
        with_buffer_input(data, |bytes| run_many(&prog_refs, bytes, false))?
            .map_err(engine_err_to_py)?
    };
    let mut engine_iter = engine_results.drain(..);
    let mut results: Vec<Py<PyResult_>> = Vec::with_capacity(programs.len());
    for opt in &programs {
        let r = if opt.is_some() {
            Py::new(py, PyResult_::new(lift(engine_iter.next().unwrap())))?
        } else {
            Py::new(py, PyResult_::new(Vec::new()))?
        };
        results.push(r);
    }
    Ok(PyList::new(py, results)?.into())
}

#[pyfunction]
#[pyo3(name = "compile")]
fn py_compile(path: &str) -> PyResult<PyPath> {
    let program = compile_path(path)?;
    Ok(PyPath {
        program,
        path_str: path.to_string(),
    })
}

#[pymodule]
fn _pygxml(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(py_get, m)?)?;
    m.add_function(wrap_pyfunction!(py_get_bytes, m)?)?;
    m.add_function(wrap_pyfunction!(py_get_buffer, m)?)?;
    m.add_function(wrap_pyfunction!(py_get_many, m)?)?;
    m.add_function(wrap_pyfunction!(py_get_many_bytes, m)?)?;
    m.add_function(wrap_pyfunction!(py_get_many_buffer, m)?)?;
    m.add_function(wrap_pyfunction!(py_parse, m)?)?;
    m.add_function(wrap_pyfunction!(py_compile, m)?)?;
    m.add_function(wrap_pyfunction!(py_validate, m)?)?;
    m.add_class::<PyPath>()?;
    m.add_class::<PyResult_>()?;
    m.add_class::<ResultCharIter>()?;
    m.add_class::<ResultItemIter>()?;
    m.add_class::<ResultChildKeyIter>()?;
    m.add_class::<ResultChildValueIter>()?;
    m.add_class::<ResultChildItemIter>()?;
    m.add_class::<ResultKeysView>()?;
    m.add_class::<ResultValuesView>()?;
    m.add_class::<ResultItemsView>()?;
    Ok(())
}
