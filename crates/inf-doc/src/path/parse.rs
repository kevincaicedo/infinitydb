//! JSONPath subset text parser (M3-S08): hand-rolled, single left-to-
//! right pass, iterative — no recursion anywhere (the L9 decoder rule;
//! `Descend` binds exactly one following selector, so nesting never
//! exceeds one level and is handled by a flag, not a stack). Grammar
//! authority: `infinitydb/docs/jsonpath-subset.md`; typed errors carry
//! the byte offset like `JsonParseError`.

use super::ast::{Member, PathAst, Segment, SliceSpec};
use super::{PathError, PathErrorKind, SEGMENTS_MAX, UNION_MEMBERS_MAX};

fn err<T>(offset: usize, kind: PathErrorKind) -> Result<T, PathError> {
    Err(PathError { offset, kind })
}

/// Parse `text` (already length-capped and UTF-8-validated by
/// `compile`) into an AST. Mode detection per grammar §1.
pub(crate) fn parse(text: &[u8]) -> Result<PathAst, PathError> {
    let legacy = text.first() != Some(&b'$');
    let mut at = usize::from(!legacy);
    let mut segments = Vec::new();
    // Legacy roots: "" and "." are the root itself.
    if legacy && (text.is_empty() || text == b".") {
        return Ok(PathAst { legacy, segments });
    }
    // A legacy path may open with a bare shorthand name ("foo.bar").
    if legacy && at == 0 && starts_shorthand(text[0]) {
        let (name, next) = take_shorthand(text, 0);
        segments.push(Segment::Child(name));
        at = next;
    }
    while at < text.len() {
        if segments.len() == SEGMENTS_MAX {
            return err(at, PathErrorKind::PathTooDeep);
        }
        let (segment, next) = parse_segment(text, at)?;
        segments.push(segment);
        at = next;
    }
    Ok(PathAst { legacy, segments })
}

/// One segment starting at `at`: `.name`, `.*`, `..sel`, or `[...]`.
fn parse_segment(text: &[u8], at: usize) -> Result<(Segment, usize), PathError> {
    match text[at] {
        b'[' => parse_bracket(text, at),
        b'.' if text.get(at + 1) == Some(&b'.') => {
            // Descend: exactly one selector follows.
            let sel_at = at + 2;
            let (inner, next) = match text.get(sel_at) {
                None => return err(at, PathErrorKind::TrailingDescend),
                Some(b'[') => parse_bracket(text, sel_at)?,
                Some(b'*') => (Segment::ChildAny, sel_at + 1),
                Some(&b) if starts_shorthand(b) => {
                    let (name, next) = take_shorthand(text, sel_at);
                    (Segment::Child(name), next)
                }
                Some(b'?') if text.get(sel_at + 1) == Some(&b'(') => {
                    return err(sel_at, PathErrorKind::FilterUnsupported);
                }
                Some(_) => return err(sel_at, PathErrorKind::UnexpectedChar),
            };
            debug_assert!(!matches!(inner, Segment::Descend(_)));
            Ok((Segment::Descend(Box::new(inner)), next))
        }
        b'.' => {
            let sel_at = at + 1;
            match text.get(sel_at) {
                Some(b'*') => Ok((Segment::ChildAny, sel_at + 1)),
                Some(&b) if starts_shorthand(b) => {
                    let (name, next) = take_shorthand(text, sel_at);
                    Ok((Segment::Child(name), next))
                }
                _ => err(sel_at.min(text.len()), PathErrorKind::UnexpectedChar),
            }
        }
        b'?' if text.get(at + 1) == Some(&b'(') => err(at, PathErrorKind::FilterUnsupported),
        _ => err(at, PathErrorKind::UnexpectedChar),
    }
}

/// `[` ws ( `*` / selector *( ws `,` ws selector ) ) ws `]`.
fn parse_bracket(text: &[u8], open: usize) -> Result<(Segment, usize), PathError> {
    debug_assert_eq!(text[open], b'[');
    let mut at = skip_ws(text, open + 1);
    if text.get(at) == Some(&b'*') {
        let close = skip_ws(text, at + 1);
        return match text.get(close) {
            Some(b']') => Ok((Segment::ChildAny, close + 1)),
            // `[*ateral]`, `[*,…]` — the wildcard is only valid alone.
            Some(b',') => err(close, PathErrorKind::BadUnionMember),
            _ => err(close.min(text.len()), PathErrorKind::UnexpectedChar),
        };
    }
    let mut members: Vec<Member> = Vec::new();
    loop {
        let (member, next) = parse_selector(text, at)?;
        if members.len() == UNION_MEMBERS_MAX {
            return err(at, PathErrorKind::BadUnionMember);
        }
        members.push(member);
        at = skip_ws(text, next);
        match text.get(at) {
            Some(b']') => break,
            Some(b',') => at = skip_ws(text, at + 1),
            None => return err(open, PathErrorKind::Unterminated),
            Some(_) => return err(at, PathErrorKind::UnexpectedChar),
        }
    }
    let next = at + 1;
    if members.len() == 1 {
        let segment = match members.pop().expect("one member") {
            Member::Name(name) => Segment::Child(name),
            Member::Index(i) => Segment::Index(i),
            Member::Slice(s) => Segment::Slice(s),
        };
        return Ok((segment, next));
    }
    Ok((Segment::Union(members), next))
}

/// One union-capable selector: quoted name, index, or slice.
fn parse_selector(text: &[u8], at: usize) -> Result<(Member, usize), PathError> {
    match text.get(at) {
        Some(b'\'') | Some(b'"') => {
            let (name, next) = parse_quoted(text, at)?;
            Ok((Member::Name(name), next))
        }
        Some(b'-') | Some(b'0'..=b'9') | Some(b':') => parse_index_or_slice(text, at),
        Some(b'?') if text.get(at + 1) == Some(&b'(') => err(at, PathErrorKind::FilterUnsupported),
        // `[0,*]` — the wildcard cannot be a union member (grammar §2).
        Some(b'*') => err(at, PathErrorKind::BadUnionMember),
        None => err(at, PathErrorKind::Unterminated),
        Some(_) => err(at, PathErrorKind::UnexpectedChar),
    }
}

/// `int`, `int? ":" int? (":" int?)?` — slice fields keep their
/// omitted-ness (canonical encoding, ADR-0040 D2); `step == 0` rejects.
fn parse_index_or_slice(text: &[u8], at: usize) -> Result<(Member, usize), PathError> {
    let (start, mut i) = parse_opt_int(text, at)?;
    if text.get(i) != Some(&b':') {
        let Some(v) = start else { return err(at, PathErrorKind::UnexpectedChar) };
        return Ok((Member::Index(v), i));
    }
    i = skip_ws(text, i + 1);
    let (end, mut i) = parse_opt_int(text, i)?;
    let mut step = None;
    if text.get(i) == Some(&b':') {
        i = skip_ws(text, i + 1);
        let (parsed, next) = parse_opt_int(text, i)?;
        if parsed == Some(0) {
            return err(i, PathErrorKind::BadSlice);
        }
        step = parsed;
        i = next;
    }
    Ok((Member::Slice(SliceSpec { start, end, step }), i))
}

/// Optional canonical int: no leading zeros, no `-0`, i64 range.
/// Returns `(None, at)` when `text[at]` does not start a number.
fn parse_opt_int(text: &[u8], at: usize) -> Result<(Option<i64>, usize), PathError> {
    let negative = text.get(at) == Some(&b'-');
    let digits_at = at + usize::from(negative);
    let mut i = digits_at;
    while matches!(text.get(i), Some(b'0'..=b'9')) {
        i += 1;
    }
    if i == digits_at {
        if negative {
            return err(at, PathErrorKind::BadNumber);
        }
        return Ok((None, skip_ws(text, at)));
    }
    if text[digits_at] == b'0' && i > digits_at + 1 {
        return err(at, PathErrorKind::BadNumber); // leading zero
    }
    // Accumulate negative so `i64::MIN` is representable before the
    // sign is applied (positive accumulation overflows on its magnitude).
    let mut value: i64 = 0;
    for &b in &text[digits_at..i] {
        value = value
            .checked_mul(10)
            .and_then(|v| v.checked_sub((b - b'0') as i64))
            .ok_or(PathError { offset: at, kind: PathErrorKind::BadNumber })?;
    }
    if negative {
        if value == 0 {
            return err(at, PathErrorKind::BadNumber); // "-0"
        }
    } else {
        value =
            value.checked_neg().ok_or(PathError { offset: at, kind: PathErrorKind::BadNumber })?;
    }
    Ok((Some(value), skip_ws(text, i)))
}

/// Quoted name with JSON escape semantics plus `\'`/`\"` (grammar §2).
fn parse_quoted(text: &[u8], open: usize) -> Result<(Vec<u8>, usize), PathError> {
    let quote = text[open];
    let mut name = Vec::new();
    let mut i = open + 1;
    loop {
        match text.get(i) {
            None => return err(open, PathErrorKind::Unterminated),
            Some(&b) if b == quote => return Ok((name, i + 1)),
            Some(b'\\') => {
                let Some(&esc) = text.get(i + 1) else {
                    return err(i, PathErrorKind::BadEscape);
                };
                i += 2;
                match esc {
                    b'\'' | b'"' | b'\\' | b'/' => name.push(esc),
                    b'b' => name.push(0x08),
                    b'f' => name.push(0x0C),
                    b'n' => name.push(b'\n'),
                    b'r' => name.push(b'\r'),
                    b't' => name.push(b'\t'),
                    b'u' => i = push_unicode_escape(text, i, &mut name)?,
                    _ => return err(i - 2, PathErrorKind::BadEscape),
                }
            }
            Some(&b) if b < 0x20 => return err(i, PathErrorKind::UnexpectedChar),
            Some(&b) => {
                name.push(b);
                i += 1;
            }
        }
    }
}

/// `\uXXXX` starting with its hex digits at `i` (the `\u` is consumed);
/// surrogate pairs follow the JSON string rules (the S05 discipline).
fn push_unicode_escape(text: &[u8], i: usize, name: &mut Vec<u8>) -> Result<usize, PathError> {
    let escape_at = i - 2;
    let hi = parse_hex4(text, i)
        .ok_or(PathError { offset: escape_at, kind: PathErrorKind::BadEscape })?;
    let mut next = i + 4;
    let code = if (0xD800..=0xDBFF).contains(&hi) {
        if text.get(next) != Some(&b'\\') || text.get(next + 1) != Some(&b'u') {
            return err(escape_at, PathErrorKind::BadEscape); // lone high surrogate
        }
        let lo = parse_hex4(text, next + 2)
            .ok_or(PathError { offset: escape_at, kind: PathErrorKind::BadEscape })?;
        if !(0xDC00..=0xDFFF).contains(&lo) {
            return err(escape_at, PathErrorKind::BadEscape);
        }
        next += 6;
        0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)
    } else if (0xDC00..=0xDFFF).contains(&hi) {
        return err(escape_at, PathErrorKind::BadEscape); // lone low surrogate
    } else {
        hi
    };
    let ch = char::from_u32(code).expect("surrogates handled; scalar remains");
    let mut utf8 = [0u8; 4];
    name.extend_from_slice(ch.encode_utf8(&mut utf8).as_bytes());
    Ok(next)
}

fn parse_hex4(text: &[u8], i: usize) -> Option<u32> {
    if text.len() < i + 4 {
        return None;
    }
    let mut v = 0u32;
    for &b in &text[i..i + 4] {
        v = v << 4 | (b as char).to_digit(16)?;
    }
    Some(v)
}

/// Space/tab, inside brackets only — every caller sits between `[`
/// and `]` (grammar §2).
fn skip_ws(text: &[u8], mut at: usize) -> usize {
    while matches!(text.get(at), Some(b' ') | Some(b'\t')) {
        at += 1;
    }
    at
}

fn starts_shorthand(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b >= 0x80
}

/// Longest shorthand name from `at` (caller checked the first byte).
fn take_shorthand(text: &[u8], at: usize) -> (Vec<u8>, usize) {
    debug_assert!(starts_shorthand(text[at]));
    let mut i = at + 1;
    while matches!(text.get(i), Some(&b) if b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80) {
        i += 1;
    }
    (text[at..i].to_vec(), i)
}
