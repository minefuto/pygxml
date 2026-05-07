// Compile a gjson-style XML path string into a flat Step program.
//
// Path syntax:
//   a.b.c            descend into named child elements
//   a.0  a.1         pick the n-th same-named child (0-based)
//   a.#              count of same-named children (terminal scalar)
//   a.#.b            project remaining over each occurrence (terminal list)
//   *  ?             wildcards in element names
//   @name            attribute (terminal)
//   #text            explicit text terminal
//   \.  \@  \\       escapes
//
//   a.#(expr)        filter: first <a> for which `expr` is true
//   a.#(expr)#       filter: all <a> for which `expr` is true
//                    expr ::= path op value
//                       op ∈ ==, =, !=, <, <=, >, >=, %, !%
//                       value ::= "string" | number | true | false
//                       path is a relative path inside <a>; may contain @attr
//
//   a.**.b           descendant: match `b` at any depth under `a` (every match)
//
//   path|@modifier   apply modifier to the result list
//                    modifiers: @reverse, @first, @last, @count, ...
//
// Implicit-selector rule:
//   A bare child name (no `.N`/`.#`/filter) compiles to `ChildIndex::Auto`.
//   At terminal position it returns every match (a list-shaped Result). At a
//   non-terminal position the engine errors out when more than one element
//   matches: the user must choose `.N` (single) or `.#` (each) explicitly.

use crate::wildcard::NameMatcher;

#[derive(Debug, Clone, PartialEq)]
pub enum FilterOp {
    Eq,           // == or =
    Ne,           // !=
    Lt,           // <
    Le,           // <=
    Gt,           // >
    Ge,           // >=
    GlobMatch,    // %
    GlobNotMatch, // !%
}

#[derive(Debug, Clone, PartialEq)]
pub enum FilterValue {
    Number(f64),
    String(String),
    Bool(bool),
}

#[derive(Debug, Clone)]
pub struct Filter {
    pub path: Vec<Step>, // relative path inside the candidate element
    pub op: FilterOp,
    pub value: FilterValue,
    /// Precomputed: `Some(attr_name)` when `path` is a single `@attr` step.
    /// Avoids a runtime pattern-match on every candidate element.
    pub attr_only: Option<String>,
}

#[derive(Debug, Clone)]
pub enum ChildIndex {
    /// Implicit selector. Bare `name` with no `.0`/`.#`/filter. At terminal
    /// position, returns every match (Multi). At non-terminal position, the
    /// engine descends only when exactly one match exists; multiple matches
    /// produce an `EngineError::AmbiguousMatch` so the caller is forced to
    /// choose `.N` (single) or `.#` (each).
    Auto,
    /// `.N` — pick the N-th same-named sibling (0-based).
    Nth(usize),
    /// `.#` terminal — count of same-named children.
    Count,
    /// `.#.x` projection or descendant target — descend into every match.
    /// Never raises ambiguity.
    Each,
    /// `.#(expr)` — first child for which the predicate holds.
    FilterFirst(Filter),
    /// `.#(expr)#` — every child for which the predicate holds.
    FilterAll(Filter),
}

impl PartialEq for ChildIndex {
    fn eq(&self, other: &Self) -> bool {
        matches!(
            (self, other),
            (ChildIndex::Auto, ChildIndex::Auto)
                | (ChildIndex::Count, ChildIndex::Count)
                | (ChildIndex::Each, ChildIndex::Each)
        ) || match (self, other) {
            (ChildIndex::Nth(a), ChildIndex::Nth(b)) => a == b,
            _ => false,
        }
    }
}
impl Eq for ChildIndex {}

#[derive(Debug, Clone)]
pub enum Step {
    /// Match a child element. `descendant: true` (set by the `**` marker)
    /// allows the match to occur at any depth — equivalent to XPath `//name`.
    Child {
        name: NameMatcher,
        index: ChildIndex,
        descendant: bool,
    },
    Attribute(String),
    Text,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Modifier {
    Reverse,
    First,
    Last,
    Count,
    Sort,         // lexicographic sort, ascending
    SortNumeric,  // numeric sort (parse as f64; non-numbers sort last)
    Unique,       // deduplicate while preserving order
    Flatten,      // no-op for our flat list[str] results (gjson parity)
    Tostr,        // no-op (results are already strings)
    Sum,          // numeric sum of values; emits one item
    Avg,          // arithmetic mean
    Min,          // numeric min
    Max,          // numeric max
}

/// How the engine should capture terminal elements. Computed at compile time
/// so that the scanner avoids per-element allocations for modifier patterns
/// that don't need full element bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalShape {
    /// Capture the full element bytes (default, needed for re-querying).
    Element,
    /// Capture only text content — safe when all modifiers produce scalars
    /// (Sum, Avg, Min, Max) or when comparing by text is sufficient (Sort).
    Text,
    /// Don't capture items at all; just count matches.
    Count,
}

#[derive(Debug, Clone)]
pub struct Program {
    pub steps: Vec<Step>,
    pub modifiers: Vec<Modifier>,
    #[allow(dead_code)]
    pub multi: bool,
    pub terminal_shape: TerminalShape,
}

#[derive(Debug)]
pub enum ParseError {
    EmptyPath,
    BadIndex(String),
    OrphanIndex,
    OrphanHash,
    AttributeNotTerminal,
    TextNotTerminal,
    EmptyAttribute,
    UnmatchedParen,
    FilterMissingOp(String),
    #[allow(dead_code)] // Reserved for stricter filter-value validation
    FilterBadValue(String),
    UnknownModifier(String),
    EmptyModifier,
    DescendantWithoutName,
    DescendantWithIndex,
}

impl core::fmt::Display for ParseError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ParseError::EmptyPath => write!(f, "empty path"),
            ParseError::BadIndex(s) => write!(f, "bad index: {}", s),
            ParseError::OrphanIndex => write!(f, "index without preceding name"),
            ParseError::OrphanHash => write!(f, "# without preceding name"),
            ParseError::AttributeNotTerminal => write!(f, "@attr must be the last component"),
            ParseError::TextNotTerminal => write!(f, "#text must be the last component"),
            ParseError::EmptyAttribute => write!(f, "@ must be followed by an attribute name"),
            ParseError::UnmatchedParen => write!(f, "unmatched ( in filter"),
            ParseError::FilterMissingOp(s) => write!(f, "filter expression missing operator: {}", s),
            ParseError::FilterBadValue(s) => write!(f, "filter value parse error: {}", s),
            ParseError::UnknownModifier(s) => write!(f, "unknown modifier: @{}", s),
            ParseError::EmptyModifier => write!(f, "modifier name is empty"),
            ParseError::DescendantWithoutName => write!(f, "** must be followed by an element name"),
            ParseError::DescendantWithIndex => {
                write!(f, "** does not accept an explicit selector (no index/#/filter)")
            }
        }
    }
}

#[derive(Debug, Clone)]
enum Component {
    Name(NameMatcher),
    Index(usize),
    Hash, // bare `#`
    Attribute(String),
    TextLit, // `#text`
    Filter { filter: Filter, all: bool },
    Descendant, // `**` marker — applies to the next Name component
}

// Split the user-provided path into its main path part and a tail of modifiers
// introduced by `|`. `|` is honored only at top level (not inside `#(...)`,
// not inside quoted strings, not preceded by `\`).
fn split_pipe(path: &str) -> Result<(String, Vec<&str>), ParseError> {
    let mut sections: Vec<&str> = Vec::new();
    let bytes = path.as_bytes();
    let mut depth: i32 = 0;
    let mut in_quote: Option<u8> = None;
    let mut last = 0;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'\\' && i + 1 < bytes.len() {
            i += 2;
            continue;
        }
        if let Some(q) = in_quote {
            if c == q {
                in_quote = None;
            }
            i += 1;
            continue;
        }
        if c == b'"' || c == b'\'' {
            in_quote = Some(c);
            i += 1;
            continue;
        }
        if c == b'(' {
            depth += 1;
        } else if c == b')' {
            depth -= 1;
            if depth < 0 {
                return Err(ParseError::UnmatchedParen);
            }
        } else if c == b'|' && depth == 0 {
            sections.push(&path[last..i]);
            last = i + 1;
        }
        i += 1;
    }
    if depth != 0 {
        return Err(ParseError::UnmatchedParen);
    }
    sections.push(&path[last..]);
    let main = sections.remove(0).to_string();
    Ok((main, sections))
}

fn parse_modifiers(parts: &[&str]) -> Result<Vec<Modifier>, ParseError> {
    let mut out = Vec::with_capacity(parts.len());
    for raw in parts {
        let trimmed = raw.trim();
        let name = trimmed
            .strip_prefix('@')
            .ok_or(ParseError::EmptyModifier)?;
        out.push(match name {
            "reverse" => Modifier::Reverse,
            "first" => Modifier::First,
            "last" => Modifier::Last,
            "count" => Modifier::Count,
            "sort" => Modifier::Sort,
            "sort_n" | "sortn" => Modifier::SortNumeric,
            "unique" | "uniq" => Modifier::Unique,
            "flatten" => Modifier::Flatten,
            "tostr" => Modifier::Tostr,
            "sum" => Modifier::Sum,
            "avg" | "mean" => Modifier::Avg,
            "min" => Modifier::Min,
            "max" => Modifier::Max,
            "" => return Err(ParseError::EmptyModifier),
            other => return Err(ParseError::UnknownModifier(other.to_string())),
        });
    }
    Ok(out)
}

// Tokenize the main path. `#(...)` is taken as one component (with optional `#`
// suffix making it FilterAll). `.` is the separator at top level only. Inside
// parentheses, `.` is part of the predicate path.
fn tokenize_path(path: &str) -> Result<Vec<Component>, ParseError> {
    let chars: Vec<char> = path.chars().collect();
    let mut comps = Vec::new();
    let mut current = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\\' {
            current.push('\\');
            if i + 1 < chars.len() {
                current.push(chars[i + 1]);
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }
        // `**` between dots is the descendant marker. We only recognize it
        // when `current` is empty (i.e. it's at a component boundary). Inside
        // a name (`b**k`), the asterisks are part of the glob pattern.
        if c == '*'
            && current.is_empty()
            && i + 1 < chars.len()
            && chars[i + 1] == '*'
        {
            comps.push(Component::Descendant);
            i += 2;
            continue;
        }
        if c == '#' && i + 1 < chars.len() && chars[i + 1] == '(' {
            if !current.is_empty() {
                comps.push(parse_component(&current)?);
                current.clear();
            }
            // Find matching `)` accounting for quotes and nested parens.
            let mut depth = 1;
            i += 2; // skip "#("
            let filter_start = i;
            let mut in_quote: Option<char> = None;
            while i < chars.len() && depth > 0 {
                let ch = chars[i];
                if ch == '\\' && i + 1 < chars.len() {
                    i += 2;
                    continue;
                }
                if let Some(q) = in_quote {
                    if ch == q {
                        in_quote = None;
                    }
                    i += 1;
                    continue;
                }
                if ch == '"' || ch == '\'' {
                    in_quote = Some(ch);
                    i += 1;
                    continue;
                }
                if ch == '(' {
                    depth += 1;
                } else if ch == ')' {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                i += 1;
            }
            if depth != 0 {
                return Err(ParseError::UnmatchedParen);
            }
            let filter_str: String = chars[filter_start..i].iter().collect();
            i += 1; // skip ')'
            let mut all = false;
            if i < chars.len() && chars[i] == '#' {
                all = true;
                i += 1;
            }
            let filter = parse_filter_expr(&filter_str)?;
            comps.push(Component::Filter { filter, all });
            continue;
        }
        if c == '.' {
            if !current.is_empty() {
                comps.push(parse_component(&current)?);
                current.clear();
            }
            i += 1;
            continue;
        }
        current.push(c);
        i += 1;
    }
    if !current.is_empty() {
        comps.push(parse_component(&current)?);
    }
    Ok(comps)
}

fn parse_component(raw: &str) -> Result<Component, ParseError> {
    if raw == "#" {
        return Ok(Component::Hash);
    }
    if raw == "#text" {
        return Ok(Component::TextLit);
    }
    if let Some(rest) = raw.strip_prefix('@') {
        if rest.is_empty() {
            return Err(ParseError::EmptyAttribute);
        }
        return Ok(Component::Attribute(unescape_simple(rest)));
    }
    if !raw.is_empty() && raw.chars().all(|c| c.is_ascii_digit()) {
        return raw
            .parse::<usize>()
            .map(Component::Index)
            .map_err(|_| ParseError::BadIndex(raw.to_string()));
    }
    Ok(Component::Name(NameMatcher::parse(raw)))
}

fn unescape_simple(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(n) = chars.next() {
                out.push(n);
            }
        } else {
            out.push(c);
        }
    }
    out
}

// Parse a filter expression like `price>10`, `name=="Foo"`, `@id=="b1"`.
fn parse_filter_expr(expr: &str) -> Result<Filter, ParseError> {
    // Find the operator by scanning until we hit one of =, !, <, >, %.
    // Skip quoted strings and escaped chars.
    let bytes = expr.as_bytes();
    let mut i = 0;
    let mut in_quote: Option<u8> = None;
    let op_idx = loop {
        if i >= bytes.len() {
            return Err(ParseError::FilterMissingOp(expr.to_string()));
        }
        let c = bytes[i];
        if c == b'\\' && i + 1 < bytes.len() {
            i += 2;
            continue;
        }
        if let Some(q) = in_quote {
            if c == q {
                in_quote = None;
            }
            i += 1;
            continue;
        }
        if c == b'"' || c == b'\'' {
            in_quote = Some(c);
            i += 1;
            continue;
        }
        if matches!(c, b'=' | b'!' | b'<' | b'>' | b'%') {
            break i;
        }
        i += 1;
    };

    let path_str = expr[..op_idx].trim();
    let (op, op_len) = parse_op(&bytes[op_idx..])?;
    let value_str = expr[op_idx + op_len..].trim();

    let path = compile_inner_path(path_str)?;
    let value = parse_filter_value(value_str)?;
    let attr_only = if path.len() == 1 {
        if let Step::Attribute(name) = &path[0] { Some(name.clone()) } else { None }
    } else {
        None
    };

    Ok(Filter { path, op, value, attr_only })
}

fn parse_op(bytes: &[u8]) -> Result<(FilterOp, usize), ParseError> {
    if bytes.starts_with(b"==") {
        return Ok((FilterOp::Eq, 2));
    }
    if bytes.starts_with(b"!=") {
        return Ok((FilterOp::Ne, 2));
    }
    if bytes.starts_with(b"<=") {
        return Ok((FilterOp::Le, 2));
    }
    if bytes.starts_with(b">=") {
        return Ok((FilterOp::Ge, 2));
    }
    if bytes.starts_with(b"!%") {
        return Ok((FilterOp::GlobNotMatch, 2));
    }
    if bytes.starts_with(b"=") {
        return Ok((FilterOp::Eq, 1));
    }
    if bytes.starts_with(b"<") {
        return Ok((FilterOp::Lt, 1));
    }
    if bytes.starts_with(b">") {
        return Ok((FilterOp::Gt, 1));
    }
    if bytes.starts_with(b"%") {
        return Ok((FilterOp::GlobMatch, 1));
    }
    Err(ParseError::FilterMissingOp(
        String::from_utf8_lossy(bytes).into_owned(),
    ))
}

fn parse_filter_value(s: &str) -> Result<FilterValue, ParseError> {
    if let Some(stripped) = s
        .strip_prefix('"')
        .and_then(|t| t.strip_suffix('"'))
        .or_else(|| s.strip_prefix('\'').and_then(|t| t.strip_suffix('\'')))
    {
        return Ok(FilterValue::String(unescape_simple(stripped)));
    }
    if s == "true" {
        return Ok(FilterValue::Bool(true));
    }
    if s == "false" {
        return Ok(FilterValue::Bool(false));
    }
    if let Ok(n) = s.parse::<f64>() {
        return Ok(FilterValue::Number(n));
    }
    // Bare unquoted strings are also accepted (e.g. `name=Foo`).
    Ok(FilterValue::String(s.to_string()))
}

// Compile a relative path used inside a filter. No modifiers permitted here.
fn compile_inner_path(path: &str) -> Result<Vec<Step>, ParseError> {
    if path.is_empty() {
        return Err(ParseError::EmptyPath);
    }
    let comps = tokenize_path(path)?;
    let prog = build_steps(comps)?;
    Ok(prog.steps)
}

fn build_steps(comps: Vec<Component>) -> Result<Program, ParseError> {
    let mut steps: Vec<Step> = Vec::with_capacity(comps.len());
    let mut multi = false;
    let mut i = 0;
    while i < comps.len() {
        match &comps[i] {
            Component::Name(name) => {
                let (index, advance) = match comps.get(i + 1) {
                    Some(Component::Index(n)) => (ChildIndex::Nth(*n), 2),
                    Some(Component::Hash) => {
                        if i + 2 < comps.len() {
                            multi = true;
                            (ChildIndex::Each, 2)
                        } else {
                            (ChildIndex::Count, 2)
                        }
                    }
                    Some(Component::Filter { filter, all }) => {
                        if *all {
                            multi = true;
                            (ChildIndex::FilterAll(filter.clone()), 2)
                        } else {
                            (ChildIndex::FilterFirst(filter.clone()), 2)
                        }
                    }
                    _ => (ChildIndex::Auto, 1),
                };
                steps.push(Step::Child {
                    name: name.clone(),
                    index,
                    descendant: false,
                });
                i += advance;
            }
            Component::Descendant => {
                // Must be followed by a Name component, and the only supported
                // index for the descendant target is implicit First.
                match comps.get(i + 1) {
                    Some(Component::Name(name)) => {
                        match comps.get(i + 2) {
                            Some(Component::Index(_))
                            | Some(Component::Hash)
                            | Some(Component::Filter { .. }) => {
                                return Err(ParseError::DescendantWithIndex);
                            }
                            _ => {}
                        }
                        // Descendant always emits every match at any depth
                        // ("全件一致"); the runtime ambiguity check that applies
                        // to plain `Auto` does not kick in here.
                        steps.push(Step::Child {
                            name: name.clone(),
                            index: ChildIndex::Each,
                            descendant: true,
                        });
                        multi = true;
                        i += 2;
                    }
                    _ => return Err(ParseError::DescendantWithoutName),
                }
            }
            Component::Attribute(a) => {
                if i != comps.len() - 1 {
                    return Err(ParseError::AttributeNotTerminal);
                }
                steps.push(Step::Attribute(a.clone()));
                i += 1;
            }
            Component::TextLit => {
                if i != comps.len() - 1 {
                    return Err(ParseError::TextNotTerminal);
                }
                steps.push(Step::Text);
                i += 1;
            }
            Component::Index(_) => return Err(ParseError::OrphanIndex),
            Component::Hash => return Err(ParseError::OrphanHash),
            Component::Filter { .. } => return Err(ParseError::OrphanHash),
        }
    }
    Ok(Program { steps, modifiers: Vec::new(), multi, terminal_shape: TerminalShape::Element })
}

/// A path-compilation error annotated with the offending input string.
#[derive(Debug)]
pub struct PathError {
    pub kind: ParseError,
    pub path: String,
}

impl PathError {
    /// Best-effort byte offset where the error occurred. Inferred from the
    /// error variant by re-scanning the path; sufficient for caret-style
    /// pointer output when the cause is locatable.
    fn estimated_pos(&self) -> Option<usize> {
        match &self.kind {
            ParseError::UnmatchedParen => find_unbalanced_paren(&self.path),
            ParseError::UnknownModifier(name) => find_modifier_pos(&self.path, name),
            ParseError::FilterMissingOp(s)
            | ParseError::FilterBadValue(s)
            | ParseError::BadIndex(s) => self.path.find(s.as_str()),
            ParseError::AttributeNotTerminal => self.path.find('@'),
            ParseError::TextNotTerminal => self.path.find("#text"),
            ParseError::EmptyAttribute => self.path.find('@'),
            ParseError::EmptyModifier => self.path.find('|'),
            ParseError::OrphanIndex | ParseError::OrphanHash | ParseError::EmptyPath => None,
            ParseError::DescendantWithoutName | ParseError::DescendantWithIndex => {
                self.path.find("**")
            }
        }
    }
}

fn find_unbalanced_paren(path: &str) -> Option<usize> {
    let bytes = path.as_bytes();
    let mut depth: i32 = 0;
    let mut last_open: Option<usize> = None;
    let mut i = 0;
    let mut in_quote: Option<u8> = None;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'\\' && i + 1 < bytes.len() {
            i += 2;
            continue;
        }
        if let Some(q) = in_quote {
            if c == q {
                in_quote = None;
            }
            i += 1;
            continue;
        }
        if c == b'"' || c == b'\'' {
            in_quote = Some(c);
        } else if c == b'(' {
            if depth == 0 {
                last_open = Some(i);
            }
            depth += 1;
        } else if c == b')' {
            depth -= 1;
            if depth < 0 {
                return Some(i);
            }
        }
        i += 1;
    }
    last_open
}

fn find_modifier_pos(path: &str, name: &str) -> Option<usize> {
    // Look for `@<name>` (typically right after `|`).
    let needle = format!("@{}", name);
    path.find(&needle)
}

impl core::fmt::Display for PathError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}\n  in path: {:?}", self.kind, self.path)?;
        if let Some(pos) = self.estimated_pos() {
            // The repr of the string adds a leading `"`, plus our prefix
            // "  in path: ". Render the caret aligned to the offending byte.
            // Note: alignment is byte-based and may look off for non-ASCII
            // input — paths are almost always ASCII so this is fine in practice.
            let prefix_width = "  in path: \"".len();
            let pad = " ".repeat(prefix_width + pos);
            write!(f, "\n{}^", pad)?;
        }
        Ok(())
    }
}

pub fn compile(path: &str) -> Result<Program, PathError> {
    compile_inner(path).map_err(|kind| PathError {
        kind,
        path: path.to_string(),
    })
}

fn compile_inner(path: &str) -> Result<Program, ParseError> {
    let (main, mod_strs) = split_pipe(path)?;
    let comps = tokenize_path(&main)?;
    if comps.is_empty() {
        return Err(ParseError::EmptyPath);
    }
    let mut prog = build_steps(comps)?;
    prog.modifiers = parse_modifiers(&mod_strs)?;
    prog.terminal_shape = infer_terminal_shape(&prog.steps, &prog.modifiers);
    Ok(prog)
}

/// Determine how the engine should capture terminal elements. Returns Count or
/// Text when all modifiers operate on text/scalars so full element bytes are
/// not needed, avoiding re-parsing overhead in apply_modifiers.
fn infer_terminal_shape(steps: &[Step], modifiers: &[Modifier]) -> TerminalShape {
    // Only element-terminal steps benefit from shape inference.
    match steps.last() {
        Some(Step::Child { .. }) => {}
        _ => return TerminalShape::Element,  // Attribute/Text are already scalars
    }
    // Count alone: track a counter; emit zero items during scan.
    if modifiers == [Modifier::Count] {
        return TerminalShape::Count;
    }
    // Text shape: all modifiers can work on text content without needing element
    // bytes. Reorder/select modifiers (Reverse/First/Last) work on text items
    // too; they only access content via item_text, which is a no-op for Text.
    let text_safe = modifiers.iter().all(|m| {
        matches!(
            m,
            Modifier::Sum
                | Modifier::Avg
                | Modifier::Min
                | Modifier::Max
                | Modifier::Sort
                | Modifier::SortNumeric
                | Modifier::Unique
                | Modifier::Flatten
                | Modifier::Tostr
                | Modifier::Reverse
                | Modifier::First
                | Modifier::Last
        )
    });
    // Only apply Text shape for multi-match terminals (Each/FilterAll/Auto on a
    // multi program) where savings are meaningful. Single-match paths have at
    // most one item, so the optimization is not worth the added complexity.
    let multi_terminal = matches!(
        steps.last(),
        Some(Step::Child { index: ChildIndex::Each | ChildIndex::Auto | ChildIndex::FilterAll(_) | ChildIndex::FilterFirst(_), .. })
    );
    if text_safe && multi_terminal && !modifiers.is_empty() {
        TerminalShape::Text
    } else {
        TerminalShape::Element
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn first_child(p: &Program) -> &Step {
        &p.steps[0]
    }

    #[test]
    fn simple_descent() {
        let p = compile("a.b.c").unwrap();
        assert_eq!(p.steps.len(), 3);
        assert!(matches!(first_child(&p), Step::Child { index: ChildIndex::Auto, .. }));
        assert!(p.modifiers.is_empty());
    }

    #[test]
    fn indexed() {
        let p = compile("a.0.b").unwrap();
        if let Step::Child { index, .. } = &p.steps[0] {
            assert_eq!(*index, ChildIndex::Nth(0));
        } else {
            panic!()
        }
    }

    #[test]
    fn count_terminal() {
        let p = compile("a.#").unwrap();
        if let Step::Child { index, .. } = &p.steps[0] {
            assert_eq!(*index, ChildIndex::Count);
        } else {
            panic!()
        }
    }

    #[test]
    fn each_projection() {
        let p = compile("a.#.b").unwrap();
        if let Step::Child { index, .. } = &p.steps[0] {
            assert_eq!(*index, ChildIndex::Each);
        } else {
            panic!()
        }
    }

    #[test]
    fn attribute_terminal() {
        let p = compile("a.@id").unwrap();
        assert!(matches!(&p.steps[1], Step::Attribute(s) if s == "id"));
    }

    #[test]
    fn filter_first() {
        let p = compile("a.#(price>10).title").unwrap();
        match &p.steps[0] {
            Step::Child { index: ChildIndex::FilterFirst(f), .. } => {
                assert_eq!(f.op, FilterOp::Gt);
                assert_eq!(f.value, FilterValue::Number(10.0));
            }
            _ => panic!("expected FilterFirst"),
        }
        // Followed by `.title.<text>` step
        assert!(matches!(&p.steps[1], Step::Child { index: ChildIndex::Auto, .. }));
    }

    #[test]
    fn filter_all() {
        let p = compile("a.#(price>10)#.title").unwrap();
        match &p.steps[0] {
            Step::Child { index: ChildIndex::FilterAll(f), .. } => {
                assert_eq!(f.op, FilterOp::Gt);
            }
            _ => panic!(),
        }
        assert!(p.multi);
    }

    #[test]
    fn filter_string_value() {
        let p = compile("a.#(@id==\"b1\").title").unwrap();
        match &p.steps[0] {
            Step::Child { index: ChildIndex::FilterFirst(f), .. } => {
                assert_eq!(f.op, FilterOp::Eq);
                assert_eq!(f.value, FilterValue::String("b1".into()));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn filter_glob() {
        let p = compile("a.#(name%\"J*\").title").unwrap();
        match &p.steps[0] {
            Step::Child { index: ChildIndex::FilterFirst(f), .. } => {
                assert_eq!(f.op, FilterOp::GlobMatch);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn modifier_reverse() {
        let p = compile("a.#.title|@reverse").unwrap();
        assert_eq!(p.modifiers, vec![Modifier::Reverse]);
    }

    #[test]
    fn multiple_modifiers() {
        let p = compile("a.#.title|@reverse|@first").unwrap();
        assert_eq!(p.modifiers, vec![Modifier::Reverse, Modifier::First]);
    }

    #[test]
    fn attribute_not_terminal_errors() {
        assert!(compile("a.@id.b").is_err());
    }

    #[test]
    fn unknown_modifier_errors() {
        assert!(compile("a|@bogus").is_err());
    }
}
