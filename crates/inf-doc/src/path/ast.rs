//! JSONPath subset AST + canonical printer (M3-S08).
//!
//! The AST is a compile intermediate and a test surface: programs are
//! cached and logged as bytecode (ADR-0040 D1), never as re-printed
//! text. The printer exists for the `parse(print(ast)) == ast` property,
//! diagnostics, and the S15 matrix. Grammar authority:
//! `infinitydb/docs/jsonpath-subset.md`.

/// One parsed path: mode + segments (root is implicit — the encoder
/// emits `Root` as the first op).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathAst {
    pub legacy: bool,
    pub segments: Vec<Segment>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Segment {
    /// `.name` / `['name']` — decoded (unescaped) key bytes, valid UTF-8.
    Child(Vec<u8>),
    /// `.*` / `[*]`.
    ChildAny,
    /// `[i]`, negatives from the end.
    Index(i64),
    /// `[a:b:s]` — `None` preserves "field omitted in the source"
    /// (part of the canonical encoding, ADR-0040 D2).
    Slice(SliceSpec),
    /// `[m, m, …]` — 2..=16 members (grammar §2).
    Union(Vec<Member>),
    /// `..sel` — inner is never `Descend` (grammar: one selector per
    /// descend) and never enforced recursively deeper.
    Descend(Box<Segment>),
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct SliceSpec {
    pub start: Option<i64>,
    pub end: Option<i64>,
    pub step: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Member {
    /// Always bracket-quoted in source and print.
    Name(Vec<u8>),
    Index(i64),
    Slice(SliceSpec),
}

/// `true` iff `name` prints as a dot-shorthand segment (grammar §2:
/// ALPHA / `_` / non-ASCII first, plus digits after).
pub(crate) fn is_shorthand(name: &[u8]) -> bool {
    let Some(&first) = name.first() else { return false };
    if !(first.is_ascii_alphabetic() || first == b'_' || first >= 0x80) {
        return false;
    }
    name[1..].iter().all(|&b| b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80)
}

/// Canonical text form (grammar §5). `parse(print(ast)) == ast` is the
/// S08 property test.
pub fn print(ast: &PathAst) -> String {
    let mut out = String::new();
    if !ast.legacy {
        out.push('$');
    } else if ast.segments.is_empty() {
        return ".".to_string();
    }
    for segment in &ast.segments {
        print_segment(&mut out, segment);
    }
    out
}

fn print_segment(out: &mut String, segment: &Segment) {
    match segment {
        Segment::Child(name) if is_shorthand(name) => {
            out.push('.');
            out.push_str(str::from_utf8(name).expect("AST keys are validated UTF-8"));
        }
        Segment::Child(name) => {
            out.push('[');
            print_quoted(out, name);
            out.push(']');
        }
        Segment::ChildAny => out.push_str(".*"),
        Segment::Index(i) => {
            out.push('[');
            out.push_str(&i.to_string());
            out.push(']');
        }
        Segment::Slice(s) => {
            out.push('[');
            print_slice(out, s);
            out.push(']');
        }
        Segment::Union(members) => {
            out.push('[');
            for (i, member) in members.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                match member {
                    Member::Name(name) => print_quoted(out, name),
                    Member::Index(v) => out.push_str(&v.to_string()),
                    Member::Slice(s) => print_slice(out, s),
                }
            }
            out.push(']');
        }
        Segment::Descend(inner) => {
            // `..name` / `..*` fuse; bracketed selectors keep their `[`.
            out.push('.');
            match inner.as_ref() {
                Segment::Child(name) if is_shorthand(name) => {
                    out.push('.');
                    out.push_str(str::from_utf8(name).expect("AST keys are validated UTF-8"));
                }
                Segment::ChildAny => out.push_str(".*"),
                other => {
                    out.push('.');
                    // Bracket forms print without the leading dot they
                    // never had; `print_segment` emits `[...]` directly.
                    debug_assert!(!matches!(other, Segment::Descend(_)), "descend never nests");
                    let mut inner_text = String::new();
                    print_segment(&mut inner_text, other);
                    debug_assert!(inner_text.starts_with('['), "descend inner is a bracket form");
                    out.push_str(&inner_text);
                }
            }
        }
    }
}

fn print_slice(out: &mut String, s: &SliceSpec) {
    if let Some(a) = s.start {
        out.push_str(&a.to_string());
    }
    out.push(':');
    if let Some(b) = s.end {
        out.push_str(&b.to_string());
    }
    if let Some(step) = s.step {
        out.push(':');
        out.push_str(&step.to_string());
    }
}

/// Single-quoted form with minimal escapes: `\\`, `\'`, controls as
/// lowercase `\u00xx` (the S06 discipline); everything else raw.
fn print_quoted(out: &mut String, name: &[u8]) {
    let name = str::from_utf8(name).expect("AST keys are validated UTF-8");
    out.push('\'');
    for ch in name.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            c if (c as u32) < 0x20 => {
                let b = c as u32 as u8;
                let hex = b"0123456789abcdef";
                out.push_str("\\u00");
                out.push(hex[(b >> 4) as usize] as char);
                out.push(hex[(b & 0xF) as usize] as char);
            }
            c => out.push(c),
        }
    }
    out.push('\'');
}
