//! Statement lexer (M4.5-S09): one left-to-right pass, no recursion
//! (L9), typed errors with byte offsets. Statements are ≤ 8 KiB and
//! UTF-8-validated by the entry, so the token vector is a bounded
//! cold-path allocation.

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
pub(crate) fn lex(text: &[u8]) -> Result<Vec<Token<'_>>, QlError> {
    let mut tokens = Vec::with_capacity(32);
    let mut at = 0;
    while at < text.len() {
        let b = text[at];
        if matches!(b, b' ' | b'\t' | b'\r' | b'\n') {
            at += 1;
            continue;
        }
        let start = at;
        let kind = match b {
            b'*' => one(&mut at, Tok::Star),
            b'.' => one(&mut at, Tok::Dot),
            b',' => one(&mut at, Tok::Comma),
            b':' => one(&mut at, Tok::Colon),
            b'(' => one(&mut at, Tok::LParen),
            b')' => one(&mut at, Tok::RParen),
            b'[' => one(&mut at, Tok::LBracket),
            b']' => one(&mut at, Tok::RBracket),
            b';' => one(&mut at, Tok::Semi),
            b'=' => one(&mut at, Tok::Eq),
            b'!' if text.get(at + 1) == Some(&b'=') => two(&mut at, Tok::Ne),
            b'<' if text.get(at + 1) == Some(&b'>') => two(&mut at, Tok::Ne),
            b'<' if text.get(at + 1) == Some(&b'=') => two(&mut at, Tok::Le),
            b'<' => one(&mut at, Tok::Lt),
            b'>' if text.get(at + 1) == Some(&b'=') => two(&mut at, Tok::Ge),
            b'>' => one(&mut at, Tok::Gt),
            b'\'' => lex_string(text, &mut at)?,
            b'"' => lex_quoted(text, &mut at)?,
            b'$' => lex_pseudo(text, &mut at)?,
            b'-' | b'0'..=b'9' => lex_number(text, &mut at)?,
            _ if starts_ident(b) => lex_ident(text, &mut at),
            _ => return err(at, QlErrorKind::UnexpectedChar),
        };
        tokens.push(Token { at: start, kind });
    }
    tokens.push(Token { at: text.len(), kind: Tok::End });
    Ok(tokens)
}

fn one<'s>(at: &mut usize, tok: Tok<'s>) -> Tok<'s> {
    *at += 1;
    tok
}

fn two<'s>(at: &mut usize, tok: Tok<'s>) -> Tok<'s> {
    *at += 2;
    tok
}

fn starts_ident(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b >= 0x80
}

fn continues_ident(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-') || b >= 0x80
}

fn lex_ident<'s>(text: &'s [u8], at: &mut usize) -> Tok<'s> {
    let start = *at;
    *at += 1;
    while matches!(text.get(*at), Some(&b) if continues_ident(b)) {
        *at += 1;
    }
    Tok::Ident(core::str::from_utf8(&text[start..*at]).expect("statement pre-validated as UTF-8"))
}

fn lex_pseudo<'s>(text: &'s [u8], at: &mut usize) -> Result<Tok<'s>, QlError> {
    let dollar = *at;
    *at += 1;
    if !matches!(text.get(*at), Some(&b) if starts_ident(b)) {
        return err(dollar, QlErrorKind::UnexpectedChar);
    }
    let Tok::Ident(name) = lex_ident(text, at) else { unreachable!("ident start checked") };
    Ok(Tok::Pseudo(name))
}

/// `'…'` with `''` doubling (SQL) — no backslash escapes.
fn lex_string<'s>(text: &'s [u8], at: &mut usize) -> Result<Tok<'s>, QlError> {
    let open = *at;
    let out = lex_delimited(text, at, b'\'')
        .ok_or(QlError { offset: open, kind: QlErrorKind::UnterminatedString })?;
    Ok(Tok::Str(out))
}

/// `"…"` with `""` doubling — quoted names.
fn lex_quoted<'s>(text: &'s [u8], at: &mut usize) -> Result<Tok<'s>, QlError> {
    let open = *at;
    let out = lex_delimited(text, at, b'"')
        .ok_or(QlError { offset: open, kind: QlErrorKind::UnterminatedString })?;
    Ok(Tok::Quoted(out))
}

fn lex_delimited(text: &[u8], at: &mut usize, quote: u8) -> Option<String> {
    let mut out = Vec::new();
    let mut i = *at + 1;
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
        value = value.checked_mul(10)?.checked_sub(i64::from(b - b'0'))?;
    }
    if negative { Some(value) } else { value.checked_neg() }
}
