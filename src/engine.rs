// Streaming evaluator. Drives a `quick_xml::Reader` over the input bytes,
// matching the program's Step list and emitting results without ever building
// a DOM. Both the single-program path (`run`/`enter`) and the multi-program
// path (`run_many`/`scan_many_children`) use `Reader<&[u8]>` slice mode:
// `reader.read_event()` and `reader.read_to_end()` borrow directly from the
// input slice with no per-event copy into a temporary buffer.
//
// Subtrees that cannot match are skipped via `read_to_end`.
//
// Element-terminal matches (`a.b.0`) capture the matched element's full
// `<e ...>...</e>` byte fragment as `Item::Element`, so the caller can re-run
// path queries against just that subtree (powering `Result.get(path)`).
// Attribute, `#text`, count, and modifier-collapsed scalars are emitted as
// `Item::Scalar` (a plain byte string).
//
// For filter steps `#(expr)` we briefly leave streaming mode for one element:
// the candidate <e>...</e> is sliced out of the original input by byte position
// (zero-copy) and re-parsed twice — once for the predicate, once for the rest
// of the path. The cost is bounded by one record's size, never the whole doc.

use crate::path::{ChildIndex, Filter, FilterOp, FilterValue, Modifier, Program, Step, TerminalShape};
use crate::wildcard::NameMatcher;
use quick_xml::escape::unescape;
use quick_xml::events::{BytesStart, Event};
use quick_xml::reader::Reader;
use smallvec::SmallVec;

/// A single result entry.
#[derive(Debug, Clone)]
pub enum Item {
    /// Matched element: full `<e ...>...</e>` bytes (or `<e/>` for empty).
    /// `Result.get(path)` re-parses this fragment.
    Element(Vec<u8>),
    /// Zero-copy reference into the original input buffer. Emitted when the
    /// caller guarantees the underlying bytes stay alive (e.g. PyBytes input).
    /// `start` and `end` are byte offsets into the same `xml` slice passed to
    /// `run_many`; the caller must resolve them back to `&xml[start..end]`.
    ElementRef { start: u32, end: u32 },
    /// Attribute value, `#text`, count, or modifier-collapsed scalar.
    Scalar(Vec<u8>),
    /// Text content extracted from an element during the scan — produced when
    /// `Program::terminal_shape == Text` to avoid re-parsing at modifier time.
    Text(Vec<u8>),
}

/// Engine evaluation error. Currently only emitted when an implicit-selector
/// `Child` step (`ChildIndex::Auto`) matches multiple elements at a position
/// that has follow-on path steps — the caller must choose `.N` or `.#`.
#[derive(Debug)]
pub enum EngineError {
    AmbiguousMatch { name: String },
    /// Internal-only: single-shot match found; propagates up to `run()`/`run_within()`,
    /// which converts it to `Ok`. Never surfaces to callers.
    Done,
}

impl core::fmt::Display for EngineError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            EngineError::AmbiguousMatch { name } => write!(
                f,
                "path is ambiguous: {} matches multiple elements but the path continues; \
                 specify .N (single) or .# (each)",
                name
            ),
            EngineError::Done => write!(f, "early stop (internal)"),
        }
    }
}

/// Per-run flags threaded through the single-program evaluator.
#[derive(Clone, Copy)]
struct RunCtx {
    /// Emit `Item::ElementRef` (zero-copy byte range into the run's input)
    /// instead of `Item::Element(to_vec())` for element terminals. Only valid
    /// when the offsets refer to the original input (not a local fragment).
    borrowable: bool,
    /// Stop the scan as soon as any item has been emitted. Set when the
    /// program's first modifier is `@first` — later modifiers then operate on
    /// the same single-item list they would have seen after truncation.
    stop_first: bool,
    /// True while every ancestor frame's step can produce no further results
    /// after the current descent: `Nth` / `FilterFirst` emit at most once, and
    /// `Auto`-with-rest errors (rather than emits) on a second match. While
    /// the chain holds, a single-shot emission may unwind the whole scan
    /// (`EngineError::Done`) without changing the result set. Once an `Each` /
    /// `FilterAll` / descendant frame is entered, single-shot completion is
    /// scope-local: skip to the end of the current scope and let the parent
    /// keep scanning — matching `run_many`'s `done_in_scope` semantics.
    chain_single: bool,
}

impl Default for RunCtx {
    fn default() -> Self {
        RunCtx { borrowable: false, stop_first: false, chain_single: true }
    }
}

/// True when the program's result is fully determined by its first item.
fn stops_after_first(program: &Program) -> bool {
    matches!(program.modifiers.first(), Some(Modifier::First))
}

pub fn run(program: &Program, xml: &[u8]) -> Result<Vec<Item>, EngineError> {
    run_with(program, xml, false)
}

/// Like `run`, but with `borrowable = true` element terminals are emitted as
/// `Item::ElementRef` byte ranges into `xml` (the caller must keep `xml` alive
/// and resolve the refs, e.g. via `lift_with_source`).
pub fn run_with(program: &Program, xml: &[u8], borrowable: bool) -> Result<Vec<Item>, EngineError> {
    let mut out: Vec<Item> = Vec::new();
    if !program.steps.is_empty() {
        let ctx = RunCtx {
            borrowable,
            stop_first: stops_after_first(program),
            chain_single: true,
        };
        let mut reader = Reader::from_reader(xml);
        reader.config_mut().trim_text(false);
        reader.config_mut().expand_empty_elements = false;
        reader.config_mut().check_end_names = false;
        match enter(&mut reader, xml, None, &program.steps, &mut out, ctx) {
            Ok(()) | Err(EngineError::Done) => {}
            Err(e) => return Err(e),
        }
    }
    apply_modifiers(&program.modifiers, &mut out, xml);
    Ok(out)
}

/// Returns true iff `xml` is a well-formed XML document with exactly one root
/// element, all start/end tags balanced, and no parser errors. Single forward
/// pass; no allocation per event. Mirrors gjson's Valid().
pub fn validate(xml: &[u8]) -> bool {
    let mut reader = Reader::from_reader(xml);
    let cfg = reader.config_mut();
    cfg.trim_text(false);
    cfg.expand_empty_elements = false;
    cfg.check_end_names = true; // default in quick-xml; set explicitly for safety

    let mut depth: i32 = 0;
    let mut roots: u32 = 0;
    loop {
        match reader.read_event() {
            Ok(Event::Start(_)) => {
                if depth == 0 {
                    roots += 1;
                    if roots > 1 {
                        return false;
                    }
                }
                depth += 1;
            }
            Ok(Event::End(_)) => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            Ok(Event::Empty(_)) => {
                if depth == 0 {
                    roots += 1;
                    if roots > 1 {
                        return false;
                    }
                }
            }
            Ok(Event::Eof) => return depth == 0 && roots == 1,
            Ok(_) => continue, // Decl / Comment / PI / Text / CData / DocType
            Err(_) => return false,
        }
    }
}

/// Run a path against a captured element fragment, treating the fragment's
/// outer element as the implicit current scope. Used by `Result.get(path)`
/// when the Result holds Owned element fragments (the path descends *into*
/// the matched element, rather than re-matching it by name).
pub fn run_within(program: &Program, fragment: &[u8]) -> Result<Vec<Item>, EngineError> {
    let mut out: Vec<Item> = Vec::new();
    if !program.steps.is_empty() {
        match apply_within_fragment(fragment, &program.steps, &mut out) {
            Ok(()) | Err(EngineError::Done) => {}
            Err(e) => return Err(e),
        }
    }
    apply_modifiers(&program.modifiers, &mut out, fragment);
    Ok(out)
}

/// Extract the textual representation of an item — text content for elements,
/// the raw bytes for scalars. `input` is the byte slice the run was executed
/// against; it resolves `Item::ElementRef` ranges. Used by the `Unique`
/// modifier and in tests.
pub fn item_text(item: &Item, input: &[u8]) -> Vec<u8> {
    match item {
        Item::Scalar(v) | Item::Text(v) => v.clone(),
        Item::Element(frag) => extract_element_text(frag),
        Item::ElementRef { start, end } => {
            extract_element_text(&input[*start as usize..*end as usize])
        }
    }
}

/// Borrow the raw bytes of an item without allocating for Text/Scalar variants.
/// Element variants fall back to extract_element_text (allocates). Use in hot
/// comparison or aggregation loops where items are expected to be Text/Scalar.
fn item_bytes_for_cmp<'a>(item: &'a Item, input: &[u8]) -> std::borrow::Cow<'a, [u8]> {
    match item {
        Item::Scalar(v) | Item::Text(v) => std::borrow::Cow::Borrowed(v.as_slice()),
        Item::Element(frag) => std::borrow::Cow::Owned(extract_element_text(frag)),
        Item::ElementRef { start, end } => std::borrow::Cow::Owned(extract_element_text(
            &input[*start as usize..*end as usize],
        )),
    }
}

pub fn extract_element_text(fragment: &[u8]) -> Vec<u8> {
    let mut reader = Reader::from_reader(fragment);
    reader.config_mut().trim_text(false);
    reader.config_mut().expand_empty_elements = false;
    reader.config_mut().check_end_names = false;
    loop {
        match reader.read_event() {
            Ok(Event::Start(_)) => return read_text_in_scope(&mut reader),
            Ok(Event::Empty(_)) => return Vec::new(),
            Ok(Event::Eof) => return Vec::new(),
            _ => continue,
        }
    }
}

fn collapse_numeric(out: &mut Vec<Item>, input: &[u8], agg: impl FnOnce(&[f64]) -> f64) {
    let nums: Vec<f64> = out
        .iter()
        .filter_map(|it| {
            let bytes = item_bytes_for_cmp(it, input);
            std::str::from_utf8(bytes.as_ref()).ok().and_then(|s| s.trim().parse::<f64>().ok())
        })
        .collect();
    let result = agg(&nums);
    out.clear();
    let s = if result.is_nan() {
        "nan".to_string()
    } else if result.is_finite() && result.fract() == 0.0 && result.abs() < 1e15 {
        format!("{}", result as i64)
    } else {
        format!("{}", result)
    };
    out.push(Item::Scalar(s.into_bytes()));
}

fn apply_modifiers(modifiers: &[Modifier], out: &mut Vec<Item>, input: &[u8]) {
    for m in modifiers {
        match m {
            Modifier::Reverse => out.reverse(),
            Modifier::First => out.truncate(1),
            Modifier::Last => {
                if out.len() > 1 {
                    let last = out.pop().unwrap();
                    out.clear();
                    out.push(last);
                }
            }
            Modifier::Count => {
                let n = out.len();
                out.clear();
                out.push(Item::Scalar(n.to_string().into_bytes()));
            }
            Modifier::Sort => {
                out.sort_by(|a, b| item_bytes_for_cmp(a, input).cmp(&item_bytes_for_cmp(b, input)));
            }
            Modifier::SortNumeric => {
                out.sort_by(|a, b| {
                    let pa = parse_item_number(a, input);
                    let pb = parse_item_number(b, input);
                    match (pa, pb) {
                        (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal),
                        (Some(_), None) => std::cmp::Ordering::Less,
                        (None, Some(_)) => std::cmp::Ordering::Greater,
                        (None, None) => item_bytes_for_cmp(a, input).cmp(&item_bytes_for_cmp(b, input)),
                    }
                });
            }
            Modifier::Unique => {
                let mut seen = std::collections::HashSet::new();
                out.retain(|it| seen.insert(item_text(it, input)));
            }
            // No-ops kept for gjson-syntax compatibility.
            Modifier::Flatten | Modifier::Tostr => {}
            Modifier::Sum => collapse_numeric(out, input, |xs| xs.iter().sum()),
            Modifier::Avg => collapse_numeric(out, input, |xs| {
                if xs.is_empty() {
                    f64::NAN
                } else {
                    xs.iter().sum::<f64>() / xs.len() as f64
                }
            }),
            Modifier::Min => collapse_numeric(out, input, |xs| {
                xs.iter().cloned().fold(f64::INFINITY, f64::min)
            }),
            Modifier::Max => collapse_numeric(out, input, |xs| {
                xs.iter().cloned().fold(f64::NEG_INFINITY, f64::max)
            }),
        }
    }
}

fn parse_item_number(it: &Item, input: &[u8]) -> Option<f64> {
    let bytes = item_bytes_for_cmp(it, input);
    std::str::from_utf8(bytes.as_ref()).ok().and_then(|s| s.trim().parse::<f64>().ok())
}

fn enter<'i>(
    reader: &mut Reader<&'i [u8]>,
    input: &'i [u8],
    current: Option<&BytesStart<'_>>,
    steps: &[Step],
    out: &mut Vec<Item>,
    ctx: RunCtx,
) -> Result<(), EngineError> {
    if let Some(Step::Attribute(name)) = steps.first() {
        if let Some(start) = current {
            if let Some(val) = read_attr(start, name) {
                out.push(Item::Scalar(val));
            }
        }
        consume_to_end(reader, current.is_some());
        return Ok(());
    }

    if matches!(steps.first(), Some(Step::Text)) {
        if current.is_some() {
            out.push(Item::Scalar(read_text_in_scope(reader)));
        } else {
            out.push(Item::Scalar(Vec::new()));
        }
        return Ok(());
    }

    // Implicit element terminal at root with no current scope: nothing to do.
    if steps.is_empty() {
        if current.is_none() {
            out.push(Item::Scalar(Vec::new()));
        }
        return Ok(());
    }

    let (name, index, descendant) = match &steps[0] {
        Step::Child { name, index, descendant } => (name.clone(), index.clone(), *descendant),
        _ => return Ok(()),
    };
    let want_count_terminal = matches!(index, ChildIndex::Count);
    let single_shot = matches!(
        index,
        ChildIndex::Nth(_) | ChildIndex::FilterFirst(_)
    );
    let rest = &steps[1..];
    // `Auto` with follow-on steps: descend into the first match immediately
    // (the reader stays in the same forward pass), then error if a second
    // sibling match appears — that is the ambiguity the caller must resolve
    // with `.N` or `.#`.
    let auto_check = matches!(index, ChildIndex::Auto) && !rest.is_empty();
    let mut auto_matched = false;
    let mut count: usize = 0;

    loop {
        // `@first`-style early stop: any emitted item completes the program.
        if ctx.stop_first && !out.is_empty() {
            return Err(EngineError::Done);
        }
        let pos_before = reader.buffer_position() as usize;
        match reader.read_event() {
            Ok(Event::Start(child)) => {
                // qn_raw and `child` itself borrow from `input` (slice-backed
                // reader), so the event can be used directly with no copy.
                let qn_raw = child.name().into_inner();
                let local_raw = local_name_bytes(qn_raw);
                if name.matches_bytes(qn_raw, local_raw) {
                    count += 1;
                    if auto_check {
                        if auto_matched {
                            return Err(EngineError::AmbiguousMatch {
                                name: name.as_source().to_string(),
                            });
                        }
                        auto_matched = true;
                        descend_or_capture(reader, input, &child, rest, out, pos_before, ctx)?;
                    } else {
                        if let Some(emitted) = handle_match_start(
                            reader,
                            input,
                            &child,
                            &index,
                            count,
                            rest,
                            out,
                            pos_before,
                            ctx,
                        )? {
                            if emitted && single_shot {
                                if ctx.chain_single {
                                    return Err(EngineError::Done);
                                }
                                // Scope-local completion (an Each/FilterAll/
                                // descendant ancestor may still emit): skip the
                                // rest of this scope, resume in the parent.
                                consume_to_end(reader, current.is_some());
                                return Ok(());
                            }
                        }
                    }
                } else if descendant {
                    // Descendant marker (`**.name`): keep looking for `name`
                    // inside this child's subtree. Pass the same `steps` so
                    // the search continues at deeper levels. Descendant target
                    // is always `Each` — never short-circuits, never errors,
                    // and may emit again → single-shot chain is broken.
                    let child_ctx = RunCtx { chain_single: false, ..ctx };
                    enter(reader, input, Some(&child), steps, out, child_ctx)?;
                } else {
                    // qn_raw borrows from input (not from a temporary buf),
                    // so it can be passed directly to read_to_end.
                    let _ = reader.read_to_end(quick_xml::name::QName(qn_raw));
                }
            }
            Ok(Event::Empty(child)) => {
                let qn_raw = child.name().into_inner();
                let local_raw = local_name_bytes(qn_raw);
                if name.matches_bytes(qn_raw, local_raw) {
                    count += 1;
                    if auto_check {
                        if auto_matched {
                            return Err(EngineError::AmbiguousMatch {
                                name: name.as_source().to_string(),
                            });
                        }
                        auto_matched = true;
                        handle_match_empty(&child, &index, count, rest, out)?;
                    } else {
                        let emitted =
                            handle_match_empty(&child, &index, count, rest, out)?;
                        if emitted && single_shot {
                            if ctx.chain_single {
                                return Err(EngineError::Done);
                            }
                            consume_to_end(reader, current.is_some());
                            return Ok(());
                        }
                    }
                }
            }
            Ok(Event::End(_)) | Ok(Event::Eof) => {
                if want_count_terminal {
                    out.push(Item::Scalar(count.to_string().into_bytes()));
                }
                return Ok(());
            }
            Ok(_) => continue,
            Err(_) => return Ok(()),
        }
    }
}


/// When `rest` is empty the matched child is itself the terminal: capture its
/// `<e>...</e>` bytes as `Item::Element`. Otherwise descend.
fn descend_or_capture<'i>(
    reader: &mut Reader<&'i [u8]>,
    input: &'i [u8],
    child: &BytesStart<'_>,
    rest: &[Step],
    out: &mut Vec<Item>,
    pos_before: usize,
    ctx: RunCtx,
) -> Result<bool, EngineError> {
    let qn_bytes = child.name().into_inner();
    if rest.is_empty() {
        let _ = reader.read_to_end(quick_xml::name::QName(qn_bytes));
        let pos_after = reader.buffer_position() as usize;
        out.push(capture_element(input, pos_before, pos_after, ctx.borrowable));
        Ok(true)
    } else {
        let prev_len = out.len();
        enter(reader, input, Some(child), rest, out, ctx)?;
        Ok(out.len() > prev_len)
    }
}

/// Element-terminal capture: zero-copy byte range when the caller guarantees
/// the input outlives the items (`borrowable`), owned bytes otherwise.
fn capture_element(input: &[u8], start: usize, end: usize, borrowable: bool) -> Item {
    if borrowable {
        Item::ElementRef { start: start as u32, end: end as u32 }
    } else {
        Item::Element(input[start..end].to_vec())
    }
}

// Returns `Ok(Some(true))` if a value was emitted (descent happened and
// matched), `Ok(Some(false))` if descent happened but no value emerged, or
// `Ok(None)` if no descent took place (e.g., counting an unmatched item).
// `Auto` is handled by `enter` (deferred), not here.
fn handle_match_start<'i>(
    reader: &mut Reader<&'i [u8]>,
    input: &'i [u8],
    child: &BytesStart<'i>,
    index: &ChildIndex,
    count: usize,
    rest: &[Step],
    out: &mut Vec<Item>,
    pos_before: usize,
    ctx: RunCtx,
) -> Result<Option<bool>, EngineError> {
    let qn_bytes = child.name().into_inner();
    match index {
        ChildIndex::Auto => {
            // `Auto` with empty rest emits each match (terminal multi). The
            // ambiguity check only applies when there are follow-on steps,
            // and that path is handled by `enter`'s deferred branch.
            Ok(Some(descend_or_capture(
                reader, input, child, rest, out, pos_before, ctx,
            )?))
        }
        ChildIndex::Nth(n) => {
            if count == n + 1 {
                Ok(Some(descend_or_capture(
                    reader, input, child, rest, out, pos_before, ctx,
                )?))
            } else {
                let _ = reader.read_to_end(quick_xml::name::QName(qn_bytes));
                Ok(None)
            }
        }
        ChildIndex::Each => {
            // Each may emit again for later siblings → deeper single-shot
            // steps must complete scope-locally, not unwind the whole scan.
            let child_ctx = RunCtx { chain_single: false, ..ctx };
            Ok(Some(descend_or_capture(
                reader, input, child, rest, out, pos_before, child_ctx,
            )?))
        }
        ChildIndex::Count => {
            let _ = reader.read_to_end(quick_xml::name::QName(qn_bytes));
            Ok(None)
        }
        ChildIndex::FilterFirst(filter) | ChildIndex::FilterAll(filter) => {
            // FilterAll keeps matching later siblings → break the chain;
            // FilterFirst emits at most once → chain preserved.
            let child_ctx = if matches!(index, ChildIndex::FilterAll(_)) {
                RunCtx { chain_single: false, ..ctx }
            } else {
                ctx
            };
            // Fast path: predicates of the form `@attr op value` can be
            // evaluated directly against the start tag, with no need to
            // capture or re-parse the subtree.
            if let Some(attr_name) = filter.attr_only.as_deref() {
                let pred_value = read_attr_str(child, attr_name);
                let pred_passes = match pred_value {
                    Some(s) => compare(&s, &filter.op, &filter.value),
                    None => false,
                };
                if pred_passes {
                    Ok(Some(descend_or_capture(
                        reader, input, child, rest, out, pos_before, child_ctx,
                    )?))
                } else {
                    let _ = reader.read_to_end(quick_xml::name::QName(qn_bytes));
                    Ok(Some(false))
                }
            } else if simple_child_filter(filter).is_some() && stream_rest_supported(rest) {
                // Single-pass path: evaluate the direct-child predicate and
                // collect the rest terminal in one walk over the live reader —
                // no fragment capture, no re-parse of the subtree.
                let pred_name = simple_child_filter(filter).unwrap();
                let passed = stream_filter_scope(
                    reader,
                    input,
                    child,
                    pred_name,
                    filter,
                    rest,
                    pos_before,
                    TerminalShape::Element,
                    ctx.borrowable,
                    out,
                )?;
                Ok(Some(passed))
            } else {
                // General path: capture the candidate's bytes <e>...</e>
                // and run the predicate + remaining-path on the fragment.
                let _ = reader.read_to_end(quick_xml::name::QName(qn_bytes));
                let pos_after = reader.buffer_position() as usize;
                let fragment = &input[pos_before..pos_after];
                if eval_filter(fragment, filter)? {
                    apply_within_fragment(fragment, rest, out)?;
                    Ok(Some(true))
                } else {
                    Ok(Some(false))
                }
            }
        }
    }
}

fn handle_match_empty(
    start: &BytesStart<'_>,
    index: &ChildIndex,
    count: usize,
    rest: &[Step],
    out: &mut Vec<Item>,
) -> Result<bool, EngineError> {
    match index {
        ChildIndex::Auto => {}
        ChildIndex::Nth(n) => {
            if count != n + 1 {
                return Ok(false);
            }
        }
        ChildIndex::Each => {}
        ChildIndex::Count => return Ok(false),
        ChildIndex::FilterFirst(filter) | ChildIndex::FilterAll(filter) => {
            // Re-emit the empty element as a one-element fragment for filtering.
            let frag = serialize_empty(start);
            if !eval_filter(&frag, filter)? {
                return Ok(false);
            }
            apply_within_fragment(&frag, rest, out)?;
            return Ok(true);
        }
    }
    let prev_len = out.len();
    handle_empty_inner(start, rest, out);
    Ok(out.len() > prev_len)
}

fn handle_empty_inner(start: &BytesStart<'_>, steps: &[Step], out: &mut Vec<Item>) {
    if let Some(Step::Attribute(name)) = steps.first() {
        if let Some(val) = read_attr(start, name) {
            out.push(Item::Scalar(val));
        }
        return;
    }
    if matches!(steps.first(), Some(Step::Text)) {
        out.push(Item::Scalar(Vec::new()));
        return;
    }
    if steps.is_empty() {
        // Empty element terminal: emit the element fragment itself.
        out.push(Item::Element(serialize_empty(start)));
    }
}

// Re-create the empty element as `<tag attrs/>` bytes so it can be re-parsed.
fn serialize_empty(start: &BytesStart<'_>) -> Vec<u8> {
    let mut out = Vec::with_capacity(start.as_ref().len() + 3);
    out.push(b'<');
    out.extend_from_slice(start.as_ref());
    out.push(b'/');
    out.push(b'>');
    out
}

fn apply_within_fragment(
    fragment: &[u8],
    inner_steps: &[Step],
    out: &mut Vec<Item>,
) -> Result<(), EngineError> {
    if inner_steps.is_empty() {
        // The whole captured fragment is the result.
        out.push(Item::Element(fragment.to_vec()));
        return Ok(());
    }
    let mut reader = Reader::from_reader(fragment);
    reader.config_mut().trim_text(false);
    reader.config_mut().expand_empty_elements = false;
    reader.config_mut().check_end_names = false;
    loop {
        match reader.read_event() {
            Ok(Event::Start(start)) => {
                // Fragment-local offsets are not valid ElementRefs → default
                // (non-borrowable) ctx.
                match enter(&mut reader, fragment, Some(&start), inner_steps, out, RunCtx::default()) {
                    Ok(()) | Err(EngineError::Done) => {}
                    Err(e) => return Err(e),
                }
                return Ok(());
            }
            Ok(Event::Empty(start)) => {
                handle_empty_inner(&start, inner_steps, out);
                return Ok(());
            }
            Ok(Event::Eof) => return Ok(()),
            _ => continue,
        }
    }
}


/// Returns the matcher if the filter's predicate path is a single `Child` step
/// (with no further nesting and the implicit text terminal). This is the
/// common case of `book.#(price > 10)` and friends.
fn simple_child_filter(filter: &Filter) -> Option<&NameMatcher> {
    if filter.path.len() == 1 {
        if let Step::Child {
            name,
            index: ChildIndex::Auto,
            descendant: false,
        } = &filter.path[0]
        {
            return Some(name);
        }
    }
    None
}

fn read_attr_str(start: &BytesStart<'_>, name: &str) -> Option<String> {
    read_attr(start, name).and_then(|v| String::from_utf8(v).ok())
}

fn eval_filter(fragment: &[u8], filter: &Filter) -> Result<bool, EngineError> {
    // Specialized walker for the common "direct-child text" predicate. Walks
    // the candidate's subtree in place, returning as soon as the predicate
    // child is found — no need to scan to end of element on a hit.
    if let Some(name) = simple_child_filter(filter) {
        return Ok(eval_child_filter_fast(fragment, name, &filter.op, &filter.value));
    }
    // General path: re-evaluate the predicate path against the fragment.
    // An ambiguous Auto step inside the predicate propagates as an error.
    let mut values: Vec<Item> = Vec::new();
    apply_within_fragment(fragment, &filter.path, &mut values)?;
    let value_bytes = match values.first() {
        Some(item) => item_text(item, fragment),
        None => return Ok(false),
    };
    let value_str = std::str::from_utf8(&value_bytes).unwrap_or("");
    Ok(compare(value_str, &filter.op, &filter.value))
}

/// Walk the candidate fragment, looking for the first direct child whose name
/// matches `pred_name`. Capture its text content and apply the comparison.
/// Returns true on match, false on miss (including missing predicate child).
fn eval_child_filter_fast(
    fragment: &[u8],
    pred_name: &NameMatcher,
    op: &FilterOp,
    value: &FilterValue,
) -> bool {
    let mut reader = Reader::from_reader(fragment);
    reader.config_mut().trim_text(false);
    reader.config_mut().expand_empty_elements = false;
    reader.config_mut().check_end_names = false;
    let mut entered_root = false;

    loop {
        match reader.read_event() {
            Ok(Event::Start(s)) => {
                if !entered_root {
                    entered_root = true;
                    continue;
                }
                let qn_raw = s.name().into_inner();
                let local_raw = local_name_bytes(qn_raw);
                if pred_name.matches_bytes(qn_raw, local_raw) {
                    let text = read_text_in_scope(&mut reader);
                    let raw = std::str::from_utf8(&text).unwrap_or("");
                    return compare(raw, op, value);
                }
                // qn_raw borrows from fragment; no workaround needed.
                let _ = reader.read_to_end(quick_xml::name::QName(qn_raw));
            }
            Ok(Event::Empty(s)) => {
                if !entered_root {
                    // Empty root: nothing inside can match
                    return false;
                }
                let qn_raw = s.name().into_inner();
                let local_raw = local_name_bytes(qn_raw);
                if pred_name.matches_bytes(qn_raw, local_raw) {
                    // Empty element → empty text
                    return compare("", op, value);
                }
            }
            Ok(Event::End(_)) => {
                // End of root without finding the predicate child
                return false;
            }
            Ok(Event::Eof) => return false,
            _ => continue,
        }
    }
}

/// Single-pass combine of simple-child predicate check + single-`Child`-step
/// rest collection. Called only when `simple_child_filter(filter)` is `Some`
/// and `rest` is exactly one non-descendant `Step::Child`.
/// Returns true iff the predicate passed (results appended to `out`).
fn eval_simple_child_filter_and_collect(
    fragment: &[u8],
    pred_name: &NameMatcher,
    filter: &Filter,
    rest_step: &Step,
    terminal_shape: TerminalShape,
    out: &mut Vec<Item>,
) -> bool {
    let (rest_name, rest_idx) = match rest_step {
        Step::Child { name, index, descendant: false } => (name, index),
        _ => return false,
    };

    let mut reader = Reader::from_reader(fragment);
    reader.config_mut().trim_text(false);
    reader.config_mut().expand_empty_elements = false;
    reader.config_mut().check_end_names = false;

    let mut pred_text: Option<Vec<u8>> = None;
    let mut collected: Vec<Item> = Vec::new();
    let mut rest_count: usize = 0;
    let mut entered_root = false;

    loop {
        let pos_before = reader.buffer_position() as usize;
        match reader.read_event() {
            Ok(Event::Start(s)) => {
                if !entered_root {
                    entered_root = true;
                    continue;
                }
                let raw = s.name().into_inner();
                let local = local_name_bytes(raw);

                if pred_name.matches_bytes(raw, local) {
                    if pred_text.is_none() {
                        pred_text = Some(read_text_in_scope(&mut reader));
                    } else {
                        // raw borrows from fragment; no workaround needed.
                        let _ = reader.read_to_end(quick_xml::name::QName(raw));
                    }
                } else if rest_name.matches_bytes(raw, local) {
                    rest_count += 1;
                    let take = match rest_idx {
                        ChildIndex::Auto | ChildIndex::Each => true,
                        ChildIndex::Nth(n) => rest_count == n + 1,
                        _ => false,
                    };
                    if take {
                        match terminal_shape {
                            TerminalShape::Count => {
                                let _ = reader.read_to_end(quick_xml::name::QName(raw));
                                collected.push(Item::Scalar(Vec::new()));
                            }
                            TerminalShape::Text => {
                                let text = read_text_in_scope(&mut reader);
                                collected.push(Item::Text(text));
                            }
                            TerminalShape::Element => {
                                let _ = reader.read_to_end(quick_xml::name::QName(raw));
                                let pos_after = reader.buffer_position() as usize;
                                collected.push(Item::Element(
                                    fragment[pos_before..pos_after].to_vec(),
                                ));
                            }
                        }
                    } else {
                        let _ = reader.read_to_end(quick_xml::name::QName(raw));
                    }
                } else {
                    let _ = reader.read_to_end(quick_xml::name::QName(raw));
                }
            }
            Ok(Event::Empty(s)) => {
                if !entered_root {
                    break; // empty root: pred_text stays None → predicate fails
                }
                let raw = s.name().into_inner();
                let local = local_name_bytes(raw);

                if pred_name.matches_bytes(raw, local) {
                    if pred_text.is_none() {
                        pred_text = Some(Vec::new()); // empty element → empty text
                    }
                } else if rest_name.matches_bytes(raw, local) {
                    rest_count += 1;
                    let take = match rest_idx {
                        ChildIndex::Auto | ChildIndex::Each => true,
                        ChildIndex::Nth(n) => rest_count == n + 1,
                        _ => false,
                    };
                    if take {
                        let frag = serialize_empty(&s);
                        match terminal_shape {
                            TerminalShape::Count => collected.push(Item::Scalar(Vec::new())),
                            TerminalShape::Text => collected.push(Item::Text(Vec::new())),
                            TerminalShape::Element => collected.push(Item::Element(frag)),
                        }
                    }
                }
            }
            Ok(Event::End(_)) | Ok(Event::Eof) => break,
            Ok(_) => continue,
            Err(_) => break,
        }
    }

    let text_bytes = pred_text.as_deref().unwrap_or(&[]);
    let text_str = std::str::from_utf8(text_bytes).unwrap_or("");
    if compare(text_str, &filter.op, &filter.value) {
        out.extend(collected);
        true
    } else {
        false
    }
}

/// Rest shapes `stream_filter_scope` can evaluate without a fragment re-parse:
/// empty (the candidate element itself is the result), a single non-descendant
/// child step with a plain selector, or a single attribute terminal.
fn stream_rest_supported(rest: &[Step]) -> bool {
    match rest {
        [] => true,
        [Step::Attribute(_)] => true,
        [Step::Child { descendant: false, index, .. }] => matches!(
            index,
            ChildIndex::Auto | ChildIndex::Each | ChildIndex::Nth(_)
        ),
        _ => false,
    }
}

/// Single-pass filter evaluation on the live reader. Called just after the
/// candidate's Start tag was consumed; reads events through the candidate's
/// End tag exactly once. Looks for the first direct child matching `pred_name`
/// (its text is the predicate value; a missing predicate child fails the
/// filter) while buffering rest-terminal matches, then emits the buffer iff
/// the predicate passes. Replaces the capture + double re-parse of the
/// fragment path for the common `e.#(child op value)` filters.
#[allow(clippy::too_many_arguments)]
fn stream_filter_scope<'i>(
    reader: &mut Reader<&'i [u8]>,
    input: &'i [u8],
    candidate: &BytesStart<'i>,
    pred_name: &NameMatcher,
    filter: &Filter,
    rest: &[Step],
    pos_before: usize,
    shape: TerminalShape,
    borrowable: bool,
    out: &mut Vec<Item>,
) -> Result<bool, EngineError> {
    let (rest_child, rest_attr) = match rest {
        [Step::Child { name, index, .. }] => (Some((name, index)), None),
        [Step::Attribute(name)] => (None, Some(name.as_str())),
        _ => (None, None),
    };

    let mut pred_text: Option<Vec<u8>> = None;
    let mut buffered: Vec<Item> = Vec::new();
    let mut rest_count: usize = 0;

    loop {
        let pos_c = reader.buffer_position() as usize;
        match reader.read_event() {
            Ok(Event::Start(c)) => {
                let raw = c.name().into_inner();
                let local = local_name_bytes(raw);
                let is_pred = pred_text.is_none() && pred_name.matches_bytes(raw, local);
                let rest_take = match rest_child {
                    Some((rname, ridx)) if rname.matches_bytes(raw, local) => {
                        rest_count += 1;
                        match ridx {
                            ChildIndex::Auto | ChildIndex::Each => true,
                            ChildIndex::Nth(n) => rest_count == n + 1,
                            _ => false,
                        }
                    }
                    _ => false,
                };
                if is_pred {
                    let text = read_text_in_scope(reader);
                    if rest_take {
                        let pos_after = reader.buffer_position() as usize;
                        buffered.push(match shape {
                            TerminalShape::Text => Item::Text(text.clone()),
                            TerminalShape::Count => Item::Scalar(Vec::new()),
                            TerminalShape::Element => {
                                capture_element(input, pos_c, pos_after, borrowable)
                            }
                        });
                    }
                    pred_text = Some(text);
                } else if rest_take {
                    match shape {
                        TerminalShape::Text => {
                            buffered.push(Item::Text(read_text_in_scope(reader)));
                        }
                        TerminalShape::Count => {
                            let _ = reader.read_to_end(quick_xml::name::QName(raw));
                            buffered.push(Item::Scalar(Vec::new()));
                        }
                        TerminalShape::Element => {
                            let _ = reader.read_to_end(quick_xml::name::QName(raw));
                            let pos_after = reader.buffer_position() as usize;
                            buffered.push(capture_element(input, pos_c, pos_after, borrowable));
                        }
                    }
                } else {
                    let _ = reader.read_to_end(quick_xml::name::QName(raw));
                }
            }
            Ok(Event::Empty(c)) => {
                let raw = c.name().into_inner();
                let local = local_name_bytes(raw);
                if pred_text.is_none() && pred_name.matches_bytes(raw, local) {
                    // Empty element → empty predicate text.
                    pred_text = Some(Vec::new());
                }
                if let Some((rname, ridx)) = rest_child {
                    if rname.matches_bytes(raw, local) {
                        rest_count += 1;
                        let take = match ridx {
                            ChildIndex::Auto | ChildIndex::Each => true,
                            ChildIndex::Nth(n) => rest_count == n + 1,
                            _ => false,
                        };
                        if take {
                            let pos_after = reader.buffer_position() as usize;
                            buffered.push(match shape {
                                TerminalShape::Text => Item::Text(Vec::new()),
                                TerminalShape::Count => Item::Scalar(Vec::new()),
                                // `<tag …/>` is literally present in the input,
                                // so the byte range is a valid fragment.
                                TerminalShape::Element => {
                                    capture_element(input, pos_c, pos_after, borrowable)
                                }
                            });
                        }
                    }
                }
            }
            Ok(Event::End(_)) | Ok(Event::Eof) => break,
            Ok(_) => continue,
            Err(_) => break,
        }
    }

    let passed = match &pred_text {
        Some(text) => {
            let raw = std::str::from_utf8(text).unwrap_or("");
            compare(raw, &filter.op, &filter.value)
        }
        None => false,
    };
    if !passed {
        return Ok(false);
    }

    if rest.is_empty() {
        let pos_after = reader.buffer_position() as usize;
        out.push(match shape {
            TerminalShape::Count => Item::Scalar(Vec::new()),
            _ => capture_element(input, pos_before, pos_after, borrowable),
        });
    } else if let Some(attr_name) = rest_attr {
        if let Some(val) = read_attr(candidate, attr_name) {
            out.push(Item::Scalar(val));
        }
    } else {
        out.extend(buffered);
    }
    Ok(true)
}

fn compare(raw: &str, op: &FilterOp, target: &FilterValue) -> bool {
    match op {
        FilterOp::Eq => match target {
            FilterValue::String(s) => raw == s.as_str(),
            FilterValue::Number(n) => parse_number(raw).map(|x| x == *n).unwrap_or(false),
            FilterValue::Bool(b) => raw == if *b { "true" } else { "false" },
        },
        FilterOp::Ne => !compare(raw, &FilterOp::Eq, target),
        FilterOp::Lt => num_cmp(raw, target, |a, b| a < b),
        FilterOp::Le => num_cmp(raw, target, |a, b| a <= b),
        FilterOp::Gt => num_cmp(raw, target, |a, b| a > b),
        FilterOp::Ge => num_cmp(raw, target, |a, b| a >= b),
        FilterOp::GlobMatch => match target {
            // Filter values are scalar strings, no element prefix concept.
            FilterValue::String(pat) => NameMatcher::parse(pat).matches(raw, raw),
            _ => false,
        },
        FilterOp::GlobNotMatch => match target {
            FilterValue::String(pat) => !NameMatcher::parse(pat).matches(raw, raw),
            _ => false,
        },
    }
}

fn num_cmp(raw: &str, target: &FilterValue, cmp: impl Fn(f64, f64) -> bool) -> bool {
    let a = match parse_number(raw) {
        Some(v) => v,
        None => return false,
    };
    let b = match target {
        FilterValue::Number(n) => *n,
        FilterValue::String(s) => match parse_number(s) {
            Some(v) => v,
            None => return false,
        },
        FilterValue::Bool(_) => return false,
    };
    cmp(a, b)
}

fn parse_number(s: &str) -> Option<f64> {
    s.trim().parse::<f64>().ok()
}

fn read_attr(start: &BytesStart<'_>, name: &str) -> Option<Vec<u8>> {
    for attr in start.attributes() {
        if let Ok(a) = attr {
            let key_local = local_name_bytes(a.key.as_ref());
            if key_local == name.as_bytes() {
                if let Ok(s) = std::str::from_utf8(&a.value) {
                    if let Ok(u) = unescape(s) {
                        return Some(u.into_owned().into_bytes());
                    }
                }
                return Some(a.value.into_owned());
            }
        }
    }
    None
}

fn local_name_bytes(qname: &[u8]) -> &[u8] {
    match memchr::memchr(b':', qname) {
        Some(i) => &qname[i + 1..],
        None => qname,
    }
}

fn read_text_in_scope(reader: &mut Reader<&[u8]>) -> Vec<u8> {
    let mut out = Vec::new();
    let mut depth: i32 = 1;
    loop {
        match reader.read_event() {
            Ok(Event::Start(_)) => depth += 1,
            Ok(Event::End(_)) => {
                depth -= 1;
                if depth == 0 {
                    return out;
                }
            }
            Ok(Event::Empty(_)) => {}
            Ok(Event::Text(t)) => {
                if let Ok(decoded) = t.unescape() {
                    out.extend_from_slice(decoded.as_bytes());
                }
            }
            Ok(Event::CData(c)) => {
                out.extend_from_slice(&c);
            }
            Ok(Event::Eof) => return out,
            _ => continue,
        }
    }
}


fn consume_to_end(reader: &mut Reader<&[u8]>, scoped: bool) {
    let mut depth: i32 = 1;
    loop {
        match reader.read_event() {
            Ok(Event::Start(_)) => depth += 1,
            Ok(Event::End(_)) => {
                if scoped {
                    depth -= 1;
                    if depth == 0 {
                        return;
                    }
                }
            }
            Ok(Event::Empty(_)) => {}
            Ok(Event::Eof) => return,
            _ => continue,
        }
    }
}

// ---- Multi-program lock-step engine ----------------------------------------
//
// `run_many` drives a single `quick_xml::Reader` over `xml`, evaluating
// multiple Program objects in lock-step. Each program maintains its own
// ProgState (cursor, counters, flags) inside the current recursion frame.
// A subtree is skipped via `read_to_end` only when no active program
// needs to descend into it, preserving the single-scan speed advantage.

/// Per-program state for one recursion frame of `scan_many_children`.
struct ProgState {
    id: usize,
    cursor: usize,
    auto_check: bool,
    auto_matched: bool,
    count: usize,
    want_count_terminal: bool,
    single_shot: bool,
    done_in_scope: bool,
}

impl ProgState {
    fn for_cursor(id: usize, cursor: usize, programs: &[&Program]) -> Self {
        let steps = &programs[id].steps[cursor..];
        let (want_count, single_shot, auto_check) =
            if let Some(Step::Child { index, .. }) = steps.first() {
                (
                    matches!(index, ChildIndex::Count),
                    matches!(index, ChildIndex::Nth(_) | ChildIndex::FilterFirst(_)),
                    matches!(index, ChildIndex::Auto) && steps.len() > 1,
                )
            } else {
                (false, false, false)
            };
        ProgState {
            id,
            cursor,
            auto_check,
            auto_matched: false,
            count: 0,
            want_count_terminal: want_count,
            single_shot,
            done_in_scope: false,
        }
    }
}

/// Immutable per-run settings for the multi-program engine, shared by every
/// recursion frame of `scan_many_children`.
struct ManyCtx<'p> {
    programs: &'p [&'p Program],
    /// Per-program: result is fully determined by its first item (`|@first`).
    /// Such a program is skipped once its out list is non-empty; when *every*
    /// program is first_only and has emitted, the whole scan stops early.
    first_only: Vec<bool>,
    all_first_only: bool,
    /// Emit `Item::ElementRef` instead of `Item::Element(to_vec())` for
    /// Element-shape terminals. The caller must resolve the refs back to
    /// `&xml[start..end]` slices via `lift_with_source`.
    borrowable: bool,
}

impl<'p> ManyCtx<'p> {
    fn new(programs: &'p [&'p Program], borrowable: bool) -> Self {
        let first_only: Vec<bool> = programs.iter().map(|p| stops_after_first(p)).collect();
        let all_first_only = !programs.is_empty() && first_only.iter().all(|&b| b);
        ManyCtx { programs, first_only, all_first_only, borrowable }
    }

    /// Program `id` can emit nothing new (first_only and already has output).
    fn prog_done(&self, id: usize, outs: &[Vec<Item>]) -> bool {
        self.first_only[id] && !outs[id].is_empty()
    }
}

pub fn run_many(
    programs: &[&Program],
    xml: &[u8],
    borrowable: bool,
) -> Result<Vec<Vec<Item>>, EngineError> {
    let n = programs.len();
    let mut outs: Vec<Vec<Item>> = vec![Vec::new(); n];
    if programs.is_empty() {
        return Ok(outs);
    }

    let non_empty: Vec<usize> = (0..n).filter(|&i| !programs[i].steps.is_empty()).collect();
    if !non_empty.is_empty() {
        let mctx = ManyCtx::new(programs, borrowable);
        let mut reader = Reader::from_reader(xml);
        reader.config_mut().trim_text(false);
        reader.config_mut().expand_empty_elements = false;
        reader.config_mut().check_end_names = false;

        // Initial states: scanning at document-root level (current = None).
        let mut states: Vec<ProgState> = non_empty
            .iter()
            .map(|&i| ProgState::for_cursor(i, 0, programs))
            .collect();

        // Scan at document root level (no enclosing element, scoped=false).
        match scan_many_children(&mut reader, xml, None, &mctx, &mut states, &mut outs) {
            Ok(()) | Err(EngineError::Done) => {}
            Err(e) => return Err(e),
        }
    }

    for i in 0..n {
        apply_modifiers(&programs[i].modifiers, &mut outs[i], xml);
    }
    Ok(outs)
}

pub fn run_many_within(
    programs: &[&Program],
    fragment: &[u8],
) -> Result<Vec<Vec<Item>>, EngineError> {
    let n = programs.len();
    let mut outs: Vec<Vec<Item>> = vec![Vec::new(); n];
    if programs.is_empty() {
        return Ok(outs);
    }

    let non_empty: Vec<usize> = (0..n).filter(|&i| !programs[i].steps.is_empty()).collect();
    if !non_empty.is_empty() {
        // ElementRef not valid — fragment is a local slice.
        let mctx = ManyCtx::new(programs, false);
        let mut reader = Reader::from_reader(fragment);
        reader.config_mut().trim_text(false);
        reader.config_mut().expand_empty_elements = false;
        reader.config_mut().check_end_names = false;

        let mut states: Vec<ProgState> = non_empty
            .iter()
            .map(|&i| ProgState::for_cursor(i, 0, programs))
            .collect();

        // Skip the outer element's Start tag, then scan inside it.
        loop {
            match reader.read_event() {
                Ok(Event::Start(start)) => {
                    // Handle Attr/Text terminals that target the outer element.
                    collect_many_attr_text(Some(&start), programs, &mut states, &mut outs);
                    match scan_many_children(
                        &mut reader,
                        fragment,
                        Some(&start),
                        &mctx,
                        &mut states,
                        &mut outs,
                    ) {
                        Ok(()) | Err(EngineError::Done) => {}
                        Err(e) => return Err(e),
                    }
                    break;
                }
                Ok(Event::Empty(start)) => {
                    collect_many_attr_text(Some(&start), programs, &mut states, &mut outs);
                    break;
                }
                Ok(Event::Eof) => break,
                Ok(_) => continue,
                Err(_) => break,
            }
        }
    }

    for i in 0..n {
        apply_modifiers(&programs[i].modifiers, &mut outs[i], fragment);
    }
    Ok(outs)
}

/// Extract Attribute and Text terminal results from the current scope's start
/// tag, then remove those programs from `states` so they don't interfere with
/// child-element scanning.
fn collect_many_attr_text(
    current: Option<&BytesStart<'_>>,
    programs: &[&Program],
    states: &mut Vec<ProgState>,
    outs: &mut Vec<Vec<Item>>,
) {
    for s in states.iter() {
        match programs[s.id].steps.get(s.cursor) {
            Some(Step::Attribute(name)) => {
                if let Some(start) = current {
                    if let Some(val) = read_attr(start, name) {
                        outs[s.id].push(Item::Scalar(val));
                    }
                }
            }
            Some(Step::Text) => {
                // Text content is in the element body, not the start tag.
                // `read_text_in_scope` would consume the reader, conflicting with
                // child scanning. For programs that reach this point alongside
                // child-step programs, text terminals emit empty (known limitation).
                // When these are the *only* active programs, the caller should
                // have taken a different path. Emit nothing here; the text programs
                // are removed below so the child scan can proceed.
            }
            _ => {}
        }
    }
    states.retain(|s| {
        !matches!(
            programs[s.id].steps.get(s.cursor),
            Some(Step::Attribute(_)) | Some(Step::Text)
        )
    });
}

/// Drive the reader over the children of the current scope.
/// `current` is the BytesStart of the enclosing element (None at document root).
/// `states` contains the active programs for this scope level.
fn scan_many_children<'i>(
    reader: &mut Reader<&'i [u8]>,
    input: &'i [u8],
    current: Option<&BytesStart<'_>>,
    mctx: &ManyCtx<'_>,
    states: &mut Vec<ProgState>,
    outs: &mut Vec<Vec<Item>>,
) -> Result<(), EngineError> {
    let programs = mctx.programs;
    if states.is_empty() {
        consume_to_end(reader, current.is_some());
        return Ok(());
    }

    // If no active program is looking for a child element, nothing to scan.
    let any_child_step = states
        .iter()
        .any(|s| matches!(programs[s.id].steps.get(s.cursor), Some(Step::Child { .. })));
    if !any_child_step {
        consume_to_end(reader, current.is_some());
        return Ok(());
    }

    loop {
        // All programs are `@first`-determined and have emitted: nothing new
        // can be produced anywhere — unwind the whole scan (caught by
        // run_many / run_many_within, like `EngineError::Done` in `run`).
        if mctx.all_first_only && outs.iter().all(|o| !o.is_empty()) {
            return Err(EngineError::Done);
        }
        let pos_before = reader.buffer_position() as usize;
        match reader.read_event() {
            Ok(Event::Start(child)) => {
                process_many_start(
                    reader, input, child,
                    pos_before, mctx, states, outs,
                )?;
            }
            Ok(Event::Empty(child)) => {
                process_many_empty(
                    child,
                    mctx, states, outs,
                )?;
            }
            Ok(Event::End(_)) | Ok(Event::Eof) => {
                for s in states.iter() {
                    if s.want_count_terminal {
                        outs[s.id].push(Item::Scalar(s.count.to_string().into_bytes()));
                    }
                }
                return Ok(());
            }
            Ok(_) => continue,
            Err(_) => return Ok(()),
        }
    }
}

/// Process one non-empty Start event against all active programs.
#[allow(clippy::too_many_arguments)]
fn process_many_start<'i>(
    reader: &mut Reader<&'i [u8]>,
    input: &'i [u8],
    child: BytesStart<'i>,
    pos_before: usize,
    mctx: &ManyCtx<'_>,
    states: &mut Vec<ProgState>,
    outs: &mut Vec<Vec<Item>>,
) -> Result<(), EngineError> {
    let programs = mctx.programs;
    let borrowable = mctx.borrowable;
    let qn_bytes: &[u8] = child.name().into_inner();
    // A3: compute local once from raw bytes; avoid per-program UTF-8 decoding.
    let local = local_name_bytes(qn_bytes);

    // Per-program actions.
    enum Action {
        Skip,
        DescendantFollow,
        ConsumeForCount,
        EmitTerminal,
        Descend { rest_start: usize },
        CaptureFilter { filter_cursor: usize, rest_start: usize },
    }

    // A2: SmallVec avoids heap allocation for the typical case (≤16 programs).
    let mut actions: SmallVec<[Action; 16]> = SmallVec::new();
    for s in states.iter_mut() {
        if s.done_in_scope || mctx.prog_done(s.id, outs) {
            actions.push(Action::Skip);
            continue;
        }
        let steps = &programs[s.id].steps[s.cursor..];
        let (name, index, descendant) = match steps.first() {
            Some(Step::Child { name, index, descendant }) => (name, index, *descendant),
            Some(Step::Attribute(_)) | Some(Step::Text) => {
                actions.push(Action::Skip);
                continue;
            }
            None => {
                actions.push(Action::Skip);
                continue;
            }
        };

        // A3: byte comparison avoids UTF-8 decode for Exact matchers.
        if !name.matches_bytes(qn_bytes, local) {
            if descendant {
                actions.push(Action::DescendantFollow);
            } else {
                actions.push(Action::Skip);
            }
            continue;
        }

        s.count += 1;
        let count = s.count;
        let rest_start = s.cursor + 1;
        let rest = &programs[s.id].steps[rest_start..];

        let action = match index {
            ChildIndex::Count => Action::ConsumeForCount,
            ChildIndex::Nth(n) => {
                if count != n + 1 {
                    Action::Skip
                } else if rest.is_empty() {
                    Action::EmitTerminal
                } else {
                    Action::Descend { rest_start }
                }
            }
            ChildIndex::Auto | ChildIndex::Each => {
                if rest.is_empty() {
                    Action::EmitTerminal
                } else {
                    Action::Descend { rest_start }
                }
            }
            ChildIndex::FilterFirst(filter) | ChildIndex::FilterAll(filter) => {
                let _ = filter;
                Action::CaptureFilter { filter_cursor: s.cursor, rest_start }
            }
        };

        // Auto ambiguity check.
        if s.auto_check {
            if let Action::Descend { .. } = &action {
                if s.auto_matched {
                    return Err(EngineError::AmbiguousMatch {
                        name: match steps.first() {
                            Some(Step::Child { name, .. }) => name.as_source().to_string(),
                            _ => String::new(),
                        },
                    });
                }
                s.auto_matched = true;
            }
        }
        actions.push(action);
    }

    // `CaptureFilter` programs need the child's byte range but must NOT be
    // added to child_states: their rest steps are evaluated by
    // `apply_within_fragment` after the filter check, not during descent.
    let needs_actual_descent = actions
        .iter()
        .any(|a| matches!(a, Action::Descend { .. } | Action::DescendantFollow));

    // Fast path: when all EmitTerminal programs want Text and there is no
    // CaptureFilter or descent, read the element's text content directly from
    // the live reader. Avoids fragment capture + a secondary Reader creation
    // (extract_element_text) that would otherwise be needed per element.
    let has_capture_filter = actions.iter().any(|a| matches!(a, Action::CaptureFilter { .. }));
    let all_emit_text_only = !needs_actual_descent
        && !has_capture_filter
        && actions.iter().zip(states.iter()).all(|(a, s)| match a {
            Action::EmitTerminal => programs[s.id].terminal_shape == TerminalShape::Text,
            Action::CaptureFilter { .. } => false,
            _ => true, // Skip / ConsumeForCount are neutral
        })
        && actions.iter().any(|a| matches!(a, Action::EmitTerminal));

    if all_emit_text_only {
        // Read text in-place; reader advances past the end tag.
        let text = read_text_in_scope(reader);
        let item = Item::Text(text);
        for (s, act) in states.iter_mut().zip(actions.iter()) {
            if matches!(act, Action::EmitTerminal) {
                outs[s.id].push(item.clone());
                if s.single_shot { s.done_in_scope = true; }
            }
            // ConsumeForCount: count already incremented; reader consumed above.
        }
        return Ok(());
    }

    // Streaming filter fast path: exactly one program wants this element and
    // it is a simple-child filter — evaluate predicate + rest terminal in a
    // single walk over the live reader instead of read_to_end + fragment
    // re-parse. Only taken when no other program needs the subtree.
    if !needs_actual_descent {
        let mut cf_iter = actions.iter().zip(states.iter_mut()).filter_map(|(a, s)| match a {
            Action::CaptureFilter { filter_cursor, rest_start } => {
                Some((s, *filter_cursor, *rest_start))
            }
            Action::EmitTerminal => Some((s, usize::MAX, usize::MAX)), // sentinel: disqualifies
            _ => None,
        });
        if let Some((s, filter_cursor, rest_start)) = cf_iter.next() {
            let exclusive = filter_cursor != usize::MAX && cf_iter.next().is_none();
            if exclusive {
                let filter = match &programs[s.id].steps[filter_cursor] {
                    Step::Child { index: ChildIndex::FilterFirst(f), .. }
                    | Step::Child { index: ChildIndex::FilterAll(f), .. } => Some(f),
                    _ => None,
                };
                if let Some(filter) = filter {
                    let rest = &programs[s.id].steps[rest_start..];
                    let shape = programs[s.id].terminal_shape;
                    if filter.attr_only.is_none()
                        && stream_rest_supported(rest)
                        && !(rest.is_empty() && shape == TerminalShape::Text)
                    {
                        if let Some(pred_name) = simple_child_filter(filter) {
                            let passed = stream_filter_scope(
                                reader, input, &child, pred_name, filter, rest,
                                pos_before, shape, borrowable, &mut outs[s.id],
                            )?;
                            if passed && s.single_shot {
                                s.done_in_scope = true;
                            }
                            return Ok(());
                        }
                    }
                }
            }
        }
    }

    // Count-shape EmitTerminals don't need the fragment bytes; only Element/Text
    // shapes and CaptureFilter need pos_after/fragment.
    let needs_bytes = actions.iter().zip(states.iter()).any(|(a, s)| match a {
        Action::EmitTerminal => programs[s.id].terminal_shape != TerminalShape::Count,
        Action::CaptureFilter { .. } => true,
        _ => false,
    });
    let has_count_emit = actions.iter().zip(states.iter()).any(|(a, s)| {
        matches!(a, Action::EmitTerminal)
            && programs[s.id].terminal_shape == TerminalShape::Count
    });

    if needs_actual_descent || needs_bytes || has_count_emit {
        // Get pos_after by either recursing (for Descend programs) or skipping.
        if needs_actual_descent {
            let mut child_states: Vec<ProgState> = Vec::new();
            for (s, act) in states.iter().zip(actions.iter()) {
                match act {
                    Action::Descend { rest_start } => {
                        child_states.push(ProgState::for_cursor(s.id, *rest_start, programs));
                    }
                    Action::DescendantFollow => {
                        child_states.push(ProgState::for_cursor(s.id, s.cursor, programs));
                    }
                    _ => {}
                }
            }
            collect_many_attr_text(Some(&child), programs, &mut child_states, outs);
            scan_many_children(reader, input, Some(&child), mctx, &mut child_states, outs)?;
        } else {
            let _ = reader.read_to_end(quick_xml::name::QName(qn_bytes));
        }

        let pos_after = if needs_bytes { reader.buffer_position() as usize } else { 0 };
        let fragment: &[u8] = if needs_bytes { &input[pos_before..pos_after] } else { &[] };

        // Post-descent: emit terminals and evaluate filters.
        for (s, act) in states.iter_mut().zip(actions.iter()) {
            match act {
                Action::EmitTerminal => {
                    // B: use terminal_shape to avoid re-parsing overhead later.
                    match programs[s.id].terminal_shape {
                        TerminalShape::Element => {
                            if borrowable {
                                // D: zero-copy byte range — caller resolves via
                                // lift_with_source.
                                outs[s.id].push(Item::ElementRef {
                                    start: pos_before as u32,
                                    end: pos_after as u32,
                                });
                            } else {
                                outs[s.id].push(Item::Element(fragment.to_vec()));
                            }
                        }
                        TerminalShape::Text => {
                            outs[s.id].push(Item::Text(extract_element_text(fragment)));
                        }
                        TerminalShape::Count => {
                            // Push a zero-byte placeholder; apply_modifiers(Count)
                            // just counts items, ignoring their content.
                            outs[s.id].push(Item::Scalar(Vec::new()));
                        }
                    }
                    if s.single_shot {
                        s.done_in_scope = true;
                    }
                }
                Action::CaptureFilter { filter_cursor, rest_start } => {
                    let filter = match &programs[s.id].steps[*filter_cursor] {
                        Step::Child { index: ChildIndex::FilterFirst(f), .. }
                        | Step::Child { index: ChildIndex::FilterAll(f), .. } => f,
                        _ => continue,
                    };
                    let rest = &programs[s.id].steps[*rest_start..];
                    let terminal_shape = programs[s.id].terminal_shape;

                    if let Some(attr_name) = filter.attr_only.as_deref() {
                        // Attr predicate checked directly from start tag — no parse.
                        let passes = read_attr_str(&child, attr_name)
                            .map(|v| compare(&v, &filter.op, &filter.value))
                            .unwrap_or(false);
                        if passes {
                            apply_within_fragment(fragment, rest, &mut outs[s.id])?;
                            if s.single_shot { s.done_in_scope = true; }
                        }
                    } else if let Some(pred_name) = simple_child_filter(filter) {
                        if rest.len() == 1
                            && matches!(&rest[0], Step::Child { descendant: false, .. })
                        {
                            // Single-pass: predicate check + rest collection combined.
                            let emitted = eval_simple_child_filter_and_collect(
                                fragment, pred_name, filter, &rest[0], terminal_shape,
                                &mut outs[s.id],
                            );
                            if emitted && s.single_shot { s.done_in_scope = true; }
                        } else {
                            // Fallback for complex rest (multi-step or descendant).
                            if eval_child_filter_fast(fragment, pred_name, &filter.op, &filter.value) {
                                apply_within_fragment(fragment, rest, &mut outs[s.id])?;
                                if s.single_shot { s.done_in_scope = true; }
                            }
                        }
                    } else {
                        // General filter: two passes unavoidable.
                        if eval_filter(fragment, filter)? {
                            apply_within_fragment(fragment, rest, &mut outs[s.id])?;
                            if s.single_shot { s.done_in_scope = true; }
                        }
                    }
                }
                _ => {}
            }
        }
    } else {
        // No program needs this subtree: skip entirely.
        let _ = reader.read_to_end(quick_xml::name::QName(qn_bytes));
    }

    Ok(())
}

/// Process one Empty event against all active programs.
fn process_many_empty<'i>(
    child: BytesStart<'i>,
    mctx: &ManyCtx<'_>,
    states: &mut Vec<ProgState>,
    outs: &mut Vec<Vec<Item>>,
) -> Result<(), EngineError> {
    let programs = mctx.programs;
    let qn_bytes: &[u8] = child.name().into_inner();
    // A3: byte-based local name; A4: serialize_empty only when actually needed.
    let local = local_name_bytes(qn_bytes);
    let mut frag: Option<Vec<u8>> = None;

    for s in states.iter_mut() {
        if s.done_in_scope || mctx.prog_done(s.id, outs) {
            continue;
        }
        let steps = &programs[s.id].steps[s.cursor..];
        let (name, index, descendant) = match steps.first() {
            Some(Step::Child { name, index, descendant }) => (name, index, *descendant),
            _ => continue,
        };

        if !name.matches_bytes(qn_bytes, local) {
            if descendant {
                // Descendant follow into empty element: nothing inside.
            }
            continue;
        }

        s.count += 1;
        let count = s.count;
        let rest_start = s.cursor + 1;
        let rest = &programs[s.id].steps[rest_start..];

        match index {
            ChildIndex::Count => {}
            ChildIndex::Nth(n) => {
                if count != n + 1 {
                    continue;
                }
                if rest.is_empty() {
                    let f = frag.get_or_insert_with(|| serialize_empty(&child));
                    outs[s.id].push(Item::Element(f.clone()));
                } else {
                    handle_empty_inner(&child, rest, &mut outs[s.id]);
                }
                if s.single_shot {
                    s.done_in_scope = true;
                }
            }
            ChildIndex::Auto | ChildIndex::Each => {
                if rest.is_empty() {
                    let f = frag.get_or_insert_with(|| serialize_empty(&child));
                    outs[s.id].push(Item::Element(f.clone()));
                } else {
                    handle_empty_inner(&child, rest, &mut outs[s.id]);
                }
                if s.single_shot {
                    s.done_in_scope = true;
                }
            }
            ChildIndex::FilterFirst(filter) | ChildIndex::FilterAll(filter) => {
                let passes = if let Some(attr_name) = filter.attr_only.as_deref() {
                    read_attr_str(&child, attr_name)
                        .map(|v| compare(&v, &filter.op, &filter.value))
                        .unwrap_or(false)
                } else {
                    let f = frag.get_or_insert_with(|| serialize_empty(&child));
                    eval_filter(f, filter)?
                };
                if passes {
                    let f = frag.get_or_insert_with(|| serialize_empty(&child));
                    apply_within_fragment(f, rest, &mut outs[s.id])?;
                    if s.single_shot {
                        s.done_in_scope = true;
                    }
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::path;

    fn run_path(xml: &[u8], p: &str) -> Vec<String> {
        let prog = path::compile(p).unwrap();
        run(&prog, xml)
            .expect("run should succeed")
            .iter()
            .map(|it| String::from_utf8_lossy(&item_text(it, xml)).into_owned())
            .collect()
    }

    fn run_path_err(xml: &[u8], p: &str) -> EngineError {
        let prog = path::compile(p).unwrap();
        run(&prog, xml).expect_err("run should error")
    }

    const BOOKS: &[u8] = br#"<store>
  <book id="b1"><title>XML in a Nutshell</title><price>30</price></book>
  <book id="b2"><title>The Cathedral and the Bazaar</title><price>20</price></book>
  <book id="b3"><title>Programming Rust</title><price>45</price></book>
</store>"#;

    // ---- Auto (implicit-selector) semantics --------------------------------

    #[test]
    fn auto_terminal_collects_all_matches() {
        // `store.book` is terminal Auto → returns all three books.
        let prog = path::compile("store.book").unwrap();
        let items = run(&prog, BOOKS).unwrap();
        assert_eq!(items.len(), 3);
    }

    #[test]
    fn auto_single_match_descends() {
        // `store` resolves to a single root element, so the engine descends
        // without complaining and `magazine` returns its single match.
        let xml = br#"<store><magazine><title>L</title></magazine></store>"#;
        assert_eq!(run_path(xml, "store.magazine.title"), vec!["L"]);
    }

    #[test]
    fn auto_multi_with_rest_errors() {
        // `book` matches three siblings but rest (`title`) follows → error.
        let err = run_path_err(BOOKS, "store.book.title");
        match err {
            EngineError::AmbiguousMatch { name } => assert_eq!(name, "book"),
            EngineError::Done => unreachable!("Done is internal-only"),
        }
    }

    #[test]
    fn auto_multi_attr_terminal_errors() {
        // Even `@id` after a multi-Auto step is ambiguous.
        let err = run_path_err(BOOKS, "store.book.@id");
        match err {
            EngineError::AmbiguousMatch { name } => assert_eq!(name, "book"),
            EngineError::Done => unreachable!("Done is internal-only"),
        }
    }

    #[test]
    fn auto_zero_matches_with_rest_is_empty() {
        // `cd` does not exist; rest is dropped silently as empty.
        assert!(run_path(BOOKS, "store.cd.title").is_empty());
    }

    // ---- Indexing / counting -----------------------------------------------

    #[test]
    fn indexed() {
        assert_eq!(run_path(BOOKS, "store.book.1.title"), vec!["The Cathedral and the Bazaar"]);
        assert_eq!(run_path(BOOKS, "store.book.2.@id"), vec!["b3"]);
    }

    #[test]
    fn count() {
        assert_eq!(run_path(BOOKS, "store.book.#"), vec!["3"]);
    }

    #[test]
    fn each_projection() {
        assert_eq!(
            run_path(BOOKS, "store.book.#.title"),
            vec![
                "XML in a Nutshell",
                "The Cathedral and the Bazaar",
                "Programming Rust"
            ]
        );
    }

    #[test]
    fn each_attr() {
        assert_eq!(run_path(BOOKS, "store.book.#.@id"), vec!["b1", "b2", "b3"]);
    }

    #[test]
    fn nth_under_each_is_scope_local() {
        // `.0` nested under `.#` completes per book, not globally: each book
        // contributes its own title.0 — parity with run_many.
        let xml = br#"<store>
          <book><title>T1</title><title>T1b</title></book>
          <book><title>T2</title></book>
          <book><title>T3</title></book>
        </store>"#;
        assert_eq!(run_path(xml, "store.book.#.title.0"), vec!["T1", "T2", "T3"]);
        assert_eq!(run_path(xml, "store.book.#.title.1"), vec!["T1b"]);

        // Same results from the multi-program engine.
        let prog = path::compile("store.book.#.title.0").unwrap();
        let single: Vec<Vec<u8>> = run(&prog, xml)
            .unwrap()
            .iter()
            .map(|it| item_text(it, xml))
            .collect();
        let many: Vec<Vec<u8>> = run_many(&[&prog], xml, false)
            .unwrap()
            .remove(0)
            .iter()
            .map(|it| item_text(it, xml))
            .collect();
        assert_eq!(single, many);
    }

    #[test]
    fn nth_chain_from_root_still_single() {
        // A pure Nth/Auto chain keeps the whole-scan early exit and returns
        // exactly one item.
        assert_eq!(run_path(BOOKS, "store.book.0.title"), vec!["XML in a Nutshell"]);
    }

    #[test]
    fn missing_returns_empty() {
        // No `cd` — the Auto step matches zero elements; `title` is silently
        // dropped, no ambiguity error.
        assert!(run_path(BOOKS, "store.cd.title").is_empty());
        assert!(run_path(BOOKS, "store.book.99.title").is_empty());
    }

    #[test]
    fn wildcard_name() {
        // `b*k` matches `book` (3 matches). Use `.0` to disambiguate.
        assert_eq!(run_path(BOOKS, "store.b*k.0.title"), vec!["XML in a Nutshell"]);
    }

    #[test]
    fn empty_element() {
        let xml = br#"<root><a id="x"/></root>"#;
        assert_eq!(run_path(xml, "root.a.@id"), vec!["x"]);
        assert_eq!(run_path(xml, "root.a"), vec![""]);
    }

    #[test]
    fn cdata_and_entities() {
        let xml = br#"<root><a>foo &amp; bar</a><b><![CDATA[<raw>]]></b></root>"#;
        assert_eq!(run_path(xml, "root.a"), vec!["foo & bar"]);
        assert_eq!(run_path(xml, "root.b"), vec!["<raw>"]);
    }

    #[test]
    fn explicit_text_terminal() {
        assert_eq!(run_path(BOOKS, "store.book.0.#text"), vec!["XML in a Nutshell30"]);
    }

    // ---- Phase 2 filter tests -----------------------------------------------

    #[test]
    fn filter_first_numeric_gt() {
        assert_eq!(
            run_path(BOOKS, "store.book.#(price>25).title"),
            vec!["XML in a Nutshell"]
        );
    }

    #[test]
    fn filter_first_numeric_le() {
        assert_eq!(
            run_path(BOOKS, "store.book.#(price<=20).title"),
            vec!["The Cathedral and the Bazaar"]
        );
    }

    #[test]
    fn filter_all_numeric() {
        assert_eq!(
            run_path(BOOKS, "store.book.#(price>=30)#.title"),
            vec!["XML in a Nutshell", "Programming Rust"]
        );
    }

    #[test]
    fn filter_string_eq() {
        assert_eq!(
            run_path(BOOKS, "store.book.#(@id==\"b2\").title"),
            vec!["The Cathedral and the Bazaar"]
        );
    }

    #[test]
    fn filter_string_ne() {
        let r = run_path(BOOKS, "store.book.#(@id!=\"b1\")#.title");
        assert_eq!(r, vec!["The Cathedral and the Bazaar", "Programming Rust"]);
    }

    #[test]
    fn filter_glob_pattern() {
        assert_eq!(
            run_path(BOOKS, "store.book.#(title%\"Programming*\").@id"),
            vec!["b3"]
        );
    }

    #[test]
    fn filter_no_match() {
        assert!(run_path(BOOKS, "store.book.#(price>9999).title").is_empty());
    }

    // ---- Phase 2 modifier tests --------------------------------------------

    #[test]
    fn modifier_reverse() {
        assert_eq!(
            run_path(BOOKS, "store.book.#.title|@reverse"),
            vec![
                "Programming Rust",
                "The Cathedral and the Bazaar",
                "XML in a Nutshell",
            ]
        );
    }

    #[test]
    fn modifier_first() {
        assert_eq!(
            run_path(BOOKS, "store.book.#.title|@first"),
            vec!["XML in a Nutshell"]
        );
    }

    #[test]
    fn modifier_last() {
        assert_eq!(
            run_path(BOOKS, "store.book.#.title|@last"),
            vec!["Programming Rust"]
        );
    }

    #[test]
    fn modifier_count() {
        assert_eq!(run_path(BOOKS, "store.book.#.title|@count"), vec!["3"]);
    }

    #[test]
    fn modifier_chain() {
        assert_eq!(
            run_path(BOOKS, "store.book.#.title|@reverse|@first"),
            vec!["Programming Rust"]
        );
    }

    // ---- Element-fragment capture (new in this revision) -------------------

    #[test]
    fn element_terminal_captures_fragment() {
        // `store.book.0` should capture the full <book>...</book> bytes.
        let prog = path::compile("store.book.0").unwrap();
        let items = run(&prog, BOOKS).unwrap();
        assert_eq!(items.len(), 1);
        match &items[0] {
            Item::Element(frag) => {
                let s = std::str::from_utf8(frag).unwrap();
                assert!(s.starts_with("<book id=\"b1\">"));
                assert!(s.ends_with("</book>"));
                assert!(s.contains("<title>XML in a Nutshell</title>"));
            }
            _ => panic!("expected Element"),
        }
    }

    #[test]
    fn empty_element_terminal_captures_fragment() {
        let xml = br#"<root><a id="x"/></root>"#;
        let prog = path::compile("root.a").unwrap();
        let items = run(&prog, xml).unwrap();
        assert_eq!(items.len(), 1);
        match &items[0] {
            Item::Element(frag) => {
                assert_eq!(std::str::from_utf8(frag).unwrap(), "<a id=\"x\"/>");
            }
            _ => panic!("expected Element"),
        }
    }

    #[test]
    fn attribute_terminal_emits_scalar() {
        let prog = path::compile("store.book.0.@id").unwrap();
        let items = run(&prog, BOOKS).unwrap();
        assert_eq!(items.len(), 1);
        assert!(matches!(items[0], Item::Scalar(_)));
    }

    // ---- Phase 6 descendant tests -----------------------------------------

    const NESTED: &[u8] = br#"<root>
        <a><b><c><x>shallow</x></c></b></a>
        <m><n><o><p><x>deep</x></p></o></n></m>
        <q><x>direct</x></q>
    </root>"#;

    #[test]
    fn descendant_collects_all_matches() {
        // `**` finds every <x> at any depth (DFS order).
        assert_eq!(
            run_path(NESTED, "root.**.x"),
            vec!["shallow", "deep", "direct"]
        );
    }

    #[test]
    fn descendant_with_no_match_is_empty() {
        assert!(run_path(NESTED, "root.**.zzz").is_empty());
    }

    #[test]
    fn descendant_collects_when_nested() {
        let xml = br#"<root><wrap><wrap><target>found</target></wrap></wrap></root>"#;
        assert_eq!(run_path(xml, "root.**.target"), vec!["found"]);
    }

    #[test]
    fn descendant_then_attribute_terminal() {
        let xml = br#"<root><a><b><c id="X"/></b></a></root>"#;
        assert_eq!(run_path(xml, "root.**.c.@id"), vec!["X"]);
    }

    #[test]
    fn descendant_then_attribute_each() {
        // Two <c> at different depths; `**.c.@id` projects @id over each.
        let xml = br#"<root><a><c id="X"/></a><b><c id="Y"/></b></root>"#;
        assert_eq!(run_path(xml, "root.**.c.@id"), vec!["X", "Y"]);
    }
}
