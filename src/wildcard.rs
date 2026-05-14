// Tiny glob matcher: `*` zero-or-more, `?` exactly one, `\` escape.
// Operates on UTF-8 strings — matches by Unicode scalars, not bytes.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NameMatcher {
    Exact(String),
    Glob(String),
}

impl NameMatcher {
    pub fn parse(raw: &str) -> Self {
        let mut has_meta = false;
        let mut chars = raw.chars();
        while let Some(c) = chars.next() {
            match c {
                '\\' => {
                    chars.next();
                }
                '*' | '?' => {
                    has_meta = true;
                    break;
                }
                _ => {}
            }
        }
        if has_meta {
            NameMatcher::Glob(raw.to_string())
        } else {
            NameMatcher::Exact(unescape(raw))
        }
    }

    /// Pattern source as written by the user, suitable for error messages.
    pub fn as_source(&self) -> &str {
        match self {
            NameMatcher::Exact(s) | NameMatcher::Glob(s) => s,
        }
    }

    /// Match an element name. The caller passes the full qualified name (e.g.
    /// `atom:entry`) and the bare local name (`entry`). Patterns with no `:`
    /// match by local name (namespace-agnostic, the gjson default). Patterns
    /// containing `:` are treated as a prefix-aware literal — they match the
    /// full qualified name. This catches the common case of stable XML
    /// prefixes (`xmlns:atom="..."` declared at document root) without the
    /// complexity of full URI resolution.
    pub fn matches(&self, full: &str, local: &str) -> bool {
        match self {
            NameMatcher::Exact(s) => {
                if s.contains(':') {
                    s == full
                } else {
                    s == local
                }
            }
            NameMatcher::Glob(pat) => {
                if pat.contains(':') {
                    glob_match(pat, full)
                } else {
                    glob_match(pat, local)
                }
            }
        }
    }

    /// Match using raw UTF-8 bytes, avoiding UTF-8 decode for the common case
    /// of ASCII element names. For Exact patterns, byte equality is equivalent
    /// to string equality. For Glob patterns with non-ASCII, falls back to the
    /// string-based path.
    pub fn matches_bytes(&self, full: &[u8], local: &[u8]) -> bool {
        match self {
            NameMatcher::Exact(s) => {
                if s.contains(':') {
                    s.as_bytes() == full
                } else {
                    s.as_bytes() == local
                }
            }
            NameMatcher::Glob(_) => {
                let full_str = std::str::from_utf8(full).unwrap_or("");
                let local_str = std::str::from_utf8(local).unwrap_or("");
                self.matches(full_str, local_str)
            }
        }
    }
}

fn unescape(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(next) = chars.next() {
                out.push(next);
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let mut pi: usize = 0;
    let mut ti: usize = 0;
    let mut star_pi: Option<usize> = None;
    let mut star_ti: usize = 0;

    while ti < t.len() {
        if pi < p.len() {
            let pc = p[pi];
            if pc == '\\' && pi + 1 < p.len() {
                if p[pi + 1] == t[ti] {
                    pi += 2;
                    ti += 1;
                    continue;
                }
            } else if pc == '?' {
                pi += 1;
                ti += 1;
                continue;
            } else if pc == '*' {
                star_pi = Some(pi);
                star_ti = ti;
                pi += 1;
                continue;
            } else if pc == t[ti] {
                pi += 1;
                ti += 1;
                continue;
            }
        }
        if let Some(sp) = star_pi {
            // Backtrack: advance the star's text anchor by one and retry.
            pi = sp + 1;
            star_ti += 1;
            ti = star_ti;
            continue;
        }
        return false;
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(matcher: &NameMatcher, name: &str) -> bool {
        matcher.matches(name, name)
    }

    #[test]
    fn exact() {
        let pat = NameMatcher::parse("book");
        assert!(m(&pat, "book"));
        assert!(!m(&pat, "books"));
    }

    #[test]
    fn glob_star() {
        let pat = NameMatcher::parse("b*k");
        assert!(m(&pat, "book"));
        assert!(m(&pat, "bk"));
        assert!(m(&pat, "bxxxk"));
        assert!(!m(&pat, "blob"));
    }

    #[test]
    fn glob_question() {
        let pat = NameMatcher::parse("b??k");
        assert!(m(&pat, "book"));
        assert!(!m(&pat, "bk"));
        assert!(!m(&pat, "bxxxk"));
    }

    #[test]
    fn escape() {
        let pat = NameMatcher::parse("a\\*b");
        assert!(m(&pat, "a*b"));
        assert!(!m(&pat, "axb"));
    }

    #[test]
    fn prefix_aware_when_pattern_has_colon() {
        let pat = NameMatcher::parse("atom:entry");
        // Matches the full qname only — local-only would also be "entry".
        assert!(pat.matches("atom:entry", "entry"));
        assert!(!pat.matches("entry", "entry"));
        assert!(!pat.matches("rss:entry", "entry"));
    }

    #[test]
    fn local_only_when_pattern_has_no_colon() {
        let pat = NameMatcher::parse("entry");
        // Matches by local name regardless of prefix.
        assert!(pat.matches("atom:entry", "entry"));
        assert!(pat.matches("entry", "entry"));
    }
}
