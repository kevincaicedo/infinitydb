//! Statement lexer (M4.5-S09): one left-to-right pass, no recursion
//! (L9), typed errors with byte offsets. Statements are ≤ 8 KiB and
//! UTF-8-validated by the entry, so the token vector is a bounded
//! cold-path allocation.
// ADR-0144 D2/D3: a decoder scope; docs/lint-scopes.tsv names its tier per lint family.
#![cfg_attr(
    not(test),
    deny(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_possible_wrap,
        clippy::arithmetic_side_effects
    )
)]

use super::{QlError, QlErrorKind};

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Token<'s> {
    pub at: usize,
    pub kind: Tok<'s>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Tok<'s> {
    /// Bare name: keywords, attribute names, namespace/index names.
    /// Continuation includes `-` (the namespace charset) — attribute
    /// names containing `-` therefore need bracket quoting (spec §3).
    Ident(&'s str),
    /// `"double quoted"` name (`""` doubling) — FROM parts.
    Quoted(String),
    /// `'single quoted'` literal (`''` doubling).
    Str(String),
    Int(i64),
    Float(f64),
    /// `$name` — pseudo-paths (`$key`).
    Pseudo(&'s str),
    Star,
    Dot,
    Comma,
    Colon,
    LParen,
    RParen,
    LBracket,
    RBracket,
    Semi,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    End,
}

fn err<T>(offset: usize, kind: QlErrorKind) -> Result<T, QlError> {
    Err(QlError { offset, kind })
}

/// Tokenize the whole statement. The trailing `End` token carries the
/// text length so "expected X at offset" points past the last byte.
#[allow(
    clippy::arithmetic_side_effects,
    reason = "bound: the loop guard holds `at < text.len()`, so `next = at + 1 <= text.len()`; \
              `next + 1` is taken only in an arm whose guard read `text[next]`, so it is at \
              most `text.len() <= isize::MAX`"
)]
pub(crate) fn lex(text: &[u8]) -> Result<Vec<Token<'_>>, QlError> {
    let mut tokens = Vec::with_capacity(32);
    let mut at = 0;
    while at < text.len() {
        let b = text[at];
        let next = at + 1;
        if matches!(b, b' ' | b'\t' | b'\r' | b'\n') {
            at = next;
            continue;
        }
        let start = at;
        let peek = text.get(next).copied();
        let kind = match b {
            b'*' => step(&mut at, next, Tok::Star),
            b'.' => step(&mut at, next, Tok::Dot),
            b',' => step(&mut at, next, Tok::Comma),
            b':' => step(&mut at, next, Tok::Colon),
            b'(' => step(&mut at, next, Tok::LParen),
            b')' => step(&mut at, next, Tok::RParen),
            b'[' => step(&mut at, next, Tok::LBracket),
            b']' => step(&mut at, next, Tok::RBracket),
            b';' => step(&mut at, next, Tok::Semi),
            b'=' => step(&mut at, next, Tok::Eq),
            b'!' if peek == Some(b'=') => step(&mut at, next + 1, Tok::Ne),
            b'<' if peek == Some(b'>') => step(&mut at, next + 1, Tok::Ne),
            b'<' if peek == Some(b'=') => step(&mut at, next + 1, Tok::Le),
            b'<' => step(&mut at, next, Tok::Lt),
            b'>' if peek == Some(b'=') => step(&mut at, next + 1, Tok::Ge),
            b'>' => step(&mut at, next, Tok::Gt),
            b'\'' => lex_string(text, &mut at, next)?,
            b'"' => lex_quoted(text, &mut at, next)?,
            b'$' => lex_pseudo(text, &mut at, next)?,
            b'-' | b'0'..=b'9' => lex_number(text, &mut at)?,
            _ if starts_ident(b) => lex_ident(text, &mut at),
            _ => return err(at, QlErrorKind::UnexpectedChar),
        };
        tokens.push(Token { at: start, kind });
    }
    tokens.push(Token { at: text.len(), kind: Tok::End });
    Ok(tokens)
}

/// A fixed-width token: the cursor moves to `to`, which [`lex`] derived
/// from bytes it has read.
fn step<'s>(at: &mut usize, to: usize, tok: Tok<'s>) -> Tok<'s> {
    *at = to;
    tok
}

fn starts_ident(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b >= 0x80
}

fn continues_ident(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-') || b >= 0x80
}

/// The run of identifier bytes at `*at`. A start byte is also a
/// continuation byte, so one scan covers both.
#[allow(
    clippy::arithmetic_side_effects,
    reason = "bound: `len` counts bytes of `text[start..]`, so `start + len <= text.len()`"
)]
fn lex_ident<'s>(text: &'s [u8], at: &mut usize) -> Tok<'s> {
    let start = *at;
    let len = text[start..].iter().take_while(|&&b| continues_ident(b)).count();
    *at = start + len;
    Tok::Ident(core::str::from_utf8(&text[start..*at]).expect("statement pre-validated as UTF-8"))
}

/// `next` is the offset after the `$` at `*at`.
fn lex_pseudo<'s>(text: &'s [u8], at: &mut usize, next: usize) -> Result<Tok<'s>, QlError> {
    let dollar = *at;
    *at = next;
    if !matches!(text.get(*at), Some(&b) if starts_ident(b)) {
        return err(dollar, QlErrorKind::UnexpectedChar);
    }
    let Tok::Ident(name) = lex_ident(text, at) else { unreachable!("ident start checked") };
    Ok(Tok::Pseudo(name))
}

/// `'…'` with `''` doubling (SQL) — no backslash escapes.
fn lex_string<'s>(text: &'s [u8], at: &mut usize, next: usize) -> Result<Tok<'s>, QlError> {
    let open = *at;
    let out = lex_delimited(text, at, next, b'\'')
        .ok_or(QlError { offset: open, kind: QlErrorKind::UnterminatedString })?;
    Ok(Tok::Str(out))
}

/// `"…"` with `""` doubling — quoted names.
fn lex_quoted<'s>(text: &'s [u8], at: &mut usize, next: usize) -> Result<Tok<'s>, QlError> {
    let open = *at;
    let out = lex_delimited(text, at, next, b'"')
        .ok_or(QlError { offset: open, kind: QlErrorKind::UnterminatedString })?;
    Ok(Tok::Quoted(out))
}

/// The body after the opening quote; `body_at` is its first byte's offset.
#[allow(
    clippy::arithmetic_side_effects,
    reason = "bound: `i + 1` and `i += 1` follow `text.get(i)` returning a byte, and `i += 2` \
              follows `text.get(i + 1)` returning one, so each sum is at most `text.len()`"
)]
fn lex_delimited(text: &[u8], at: &mut usize, body_at: usize, quote: u8) -> Option<String> {
    let mut out = Vec::new();
    let mut i = body_at;
    loop {
        match text.get(i) {
            None => return None,
            Some(&b) if b == quote => {
                if text.get(i + 1) == Some(&quote) {
                    out.push(quote);
                    i += 2;
                } else {
                    *at = i + 1;
                    // Byte-splitting a pre-validated UTF-8 statement at
                    // ASCII quotes keeps every piece valid UTF-8.
                    return Some(String::from_utf8(out).expect("UTF-8 pre-validated"));
                }
            }
            Some(&b) => {
                out.push(b);
                i += 1;
            }
        }
    }
}

/// `-? digits ( '.' digits )? ( [eE] [+-]? digits )?` — no `.5`, no
/// `5.`. Lexical type is the value's type: no dot/exponent ⇒ i64
/// (overflow rejects — silent f64 promotion loses exactness at 2⁵³),
/// otherwise f64 (must stay finite).
#[allow(
    clippy::arithmetic_side_effects,
    reason = "bound: each `*at += 1` follows a read of `text[*at]` (the indexed sign byte, or \
              `text.get(*at)` returning a byte), so the sum is at most `text.len()`"
)]
fn lex_number<'s>(text: &'s [u8], at: &mut usize) -> Result<Tok<'s>, QlError> {
    let start = *at;
    let negative = text[*at] == b'-';
    if negative {
        *at += 1;
        if !matches!(text.get(*at), Some(b'0'..=b'9')) {
            return err(start, QlErrorKind::UnexpectedChar);
        }
    }
    take_digits(text, at);
    let mut integral = true;
    if text.get(*at) == Some(&b'.') {
        integral = false;
        *at += 1;
        if take_digits(text, at) == 0 {
            return err(start, QlErrorKind::BadNumber);
        }
    }
    if matches!(text.get(*at), Some(b'e' | b'E')) {
        integral = false;
        *at += 1;
        if matches!(text.get(*at), Some(b'+' | b'-')) {
            *at += 1;
        }
        if take_digits(text, at) == 0 {
            return err(start, QlErrorKind::BadNumber);
        }
    }
    let slice = core::str::from_utf8(&text[start..*at]).expect("ASCII number bytes");
    if integral {
        return match parse_i64(slice) {
            Some(v) => Ok(Tok::Int(v)),
            None => err(start, QlErrorKind::IntegerOutOfRange),
        };
    }
    let value: f64 = slice.parse().expect("lexed float shape parses");
    if !value.is_finite() {
        return err(start, QlErrorKind::NonFiniteNumber);
    }
    Ok(Tok::Float(value))
}

/// Consumes a digit run; returns its length.
#[allow(
    clippy::arithmetic_side_effects,
    reason = "bound: each `*at += 1` follows `text.get(*at)` returning a byte, so the sum is at \
              most `text.len()`; `*at` only grows from `start`, so the difference is >= 0"
)]
fn take_digits(text: &[u8], at: &mut usize) -> usize {
    let start = *at;
    while matches!(text.get(*at), Some(b'0'..=b'9')) {
        *at += 1;
    }
    *at - start
}

/// Accumulate negative so `i64::MIN` is representable before the sign
/// applies (the M3 parser's rule).
fn parse_i64(s: &str) -> Option<i64> {
    let (negative, digits) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s),
    };
    let mut value: i64 = 0;
    for b in digits.bytes() {
        let digit = char::from(b).to_digit(10)?;
        value = value.checked_mul(10)?.checked_sub(i64::from(digit))?;
    }
    if negative { Some(value) } else { value.checked_neg() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(text: &str) -> Vec<Tok<'_>> {
        lex(text.as_bytes()).expect("lexes").into_iter().map(|token| token.kind).collect()
    }

    fn refusal(text: &str) -> (usize, QlErrorKind) {
        let e = lex(text.as_bytes()).expect_err("refuses");
        (e.offset, e.kind)
    }

    /// Every lookahead at the last byte of the statement: the cursor
    /// lands exactly on `text.len()`, never past it.
    #[test]
    fn operators_at_the_end_of_text() {
        assert_eq!(kinds("<"), [Tok::Lt, Tok::End]);
        assert_eq!(kinds(">"), [Tok::Gt, Tok::End]);
        assert_eq!(kinds("<="), [Tok::Le, Tok::End]);
        assert_eq!(kinds(">="), [Tok::Ge, Tok::End]);
        assert_eq!(kinds("<>"), [Tok::Ne, Tok::End]);
        assert_eq!(kinds("!="), [Tok::Ne, Tok::End]);
        assert_eq!(kinds("<<="), [Tok::Lt, Tok::Le, Tok::End]);
        assert_eq!(refusal("!"), (0, QlErrorKind::UnexpectedChar));
        assert_eq!(refusal("= !"), (2, QlErrorKind::UnexpectedChar));
        let tokens = lex(b"a <=").expect("lexes");
        assert_eq!(tokens.iter().map(|token| token.at).collect::<Vec<_>>(), [0, 2, 4]);
    }

    #[test]
    fn names_and_pseudo_paths_at_the_end_of_text() {
        assert_eq!(kinds(""), [Tok::End]);
        assert_eq!(kinds("a"), [Tok::Ident("a"), Tok::End]);
        assert_eq!(kinds("a-b_1 c"), [Tok::Ident("a-b_1"), Tok::Ident("c"), Tok::End]);
        assert_eq!(kinds("é"), [Tok::Ident("é"), Tok::End]);
        assert_eq!(kinds("$key"), [Tok::Pseudo("key"), Tok::End]);
        assert_eq!(refusal("$"), (0, QlErrorKind::UnexpectedChar));
        assert_eq!(refusal("a $1"), (2, QlErrorKind::UnexpectedChar));
    }

    #[test]
    fn delimited_bodies_at_the_end_of_text() {
        assert_eq!(kinds("''"), [Tok::Str(String::new()), Tok::End]);
        assert_eq!(kinds("'a''b'"), [Tok::Str("a'b".into()), Tok::End]);
        assert_eq!(kinds("''''"), [Tok::Str("'".into()), Tok::End]);
        assert_eq!(kinds("\"n\"\"s\""), [Tok::Quoted("n\"s".into()), Tok::End]);
        for text in ["'", "'a", "'a''", "'''", "x '"] {
            let open = text.find('\'').expect("fixture has a quote");
            assert_eq!(refusal(text), (open, QlErrorKind::UnterminatedString), "{text}");
        }
        assert_eq!(refusal("\""), (0, QlErrorKind::UnterminatedString));
    }

    #[test]
    fn number_shapes_at_the_end_of_text() {
        assert_eq!(kinds("0"), [Tok::Int(0), Tok::End]);
        assert_eq!(kinds("-1"), [Tok::Int(-1), Tok::End]);
        assert_eq!(kinds("1.5e+3"), [Tok::Float(1500.0), Tok::End]);
        assert_eq!(refusal("-"), (0, QlErrorKind::UnexpectedChar));
        assert_eq!(refusal("1 -x"), (2, QlErrorKind::UnexpectedChar));
        for text in ["1.", "1e", "1e+", "1.e1", "-1e-"] {
            assert_eq!(refusal(text), (0, QlErrorKind::BadNumber), "{text}");
        }
        assert_eq!(refusal("1e999"), (0, QlErrorKind::NonFiniteNumber));
    }

    #[test]
    fn integers_at_the_i64_edges() {
        assert_eq!(kinds("9223372036854775807"), [Tok::Int(i64::MAX), Tok::End]);
        assert_eq!(kinds("-9223372036854775808"), [Tok::Int(i64::MIN), Tok::End]);
        assert_eq!(refusal("9223372036854775808"), (0, QlErrorKind::IntegerOutOfRange));
        assert_eq!(refusal("-9223372036854775809"), (0, QlErrorKind::IntegerOutOfRange));
        assert_eq!(refusal("99999999999999999999999"), (0, QlErrorKind::IntegerOutOfRange));
        // Total on its own: a byte that is no digit is a refusal, not a wrapped digit.
        assert_eq!(parse_i64("12"), Some(12));
        assert_eq!(parse_i64("1/"), None);
        assert_eq!(parse_i64("1a"), None);
        assert_eq!(parse_i64(""), Some(0));
    }
}
