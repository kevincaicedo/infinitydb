//! JSON text serializer (M3-S06): tape/arena cursors → JSON text, written
//! straight into a caller-supplied buffer — no intermediate string, no
//! recursion (explicit frame stack, like every decoder/encoder here). One
//! serializer feeds three consumers:
//!
//! - **RESP replies** (S11): a bulk string needs its byte length *before*
//!   its bytes, and measuring serialized length costs as much as
//!   serializing — so the wire layer reserves the maximal length-header
//!   width, calls [`serialize_into`] once (it appends), back-patches the
//!   actual length, and adjusts the iovec start. Never serialize twice;
//!   never build an intermediate string. This module owns the append
//!   contract; the reserve/back-patch half lands with the S11 seam.
//! - **Formatted replies**: `INDENT`/`NEWLINE`/`SPACE` (RedisJSON
//!   `JSON.GET` options — arbitrary byte strings, not just whitespace).
//! - **Canonical mode** ([`serialize_canonical_into`]): compact output
//!   with fixed number formatting — the E8 delta-replay comparator. The
//!   physical form (tape vs arena, interned or not) never affects the
//!   bytes; the fixpoint suite asserts it.
//!
//! Formatting shape pins the serde_json lineage RedisJSON inherits (its
//! reply formatter implements serde_json's `Formatter` with these three
//! strings): non-empty containers put `newline + indent×depth` before
//! every element and `newline + indent×(depth−1)` before the closer; keys
//! are followed by `:` + `space`; empty containers print `{}`/`[]` with
//! nothing inside. With `INDENT "  " NEWLINE "\n" SPACE " "` the output is
//! byte-identical to `serde_json::to_string_pretty` (test-pinned).
//!
//! Numbers: i64 as plain integer text; f64 via `zmij` shortest
//! round-trip — the exact crate serde_json (≥ 1.0.144) formats floats
//! through, so local differential parity is by construction, and
//! `parse(serialize(doc)) == doc` holds bit-exactly (the fixpoint AC).
//! The RedisJSON number-parity fixup table (integral doubles, exponent
//! thresholds) starts **empty** and grows only from S21 oracle diffs
//! (L8: deviations are measured, never guessed). NaN/±Inf are
//! unrepresentable in idoc (validator-enforced at the trust boundary),
//! so `format_finite`'s precondition holds for every `DocValue::F64`.
//!
//! String escaping is serde_json-parity: `"` and `\` get two-byte
//! escapes; control bytes < 0x20 get `\b \t \n \f \r` or `\u00xx`
//! (lowercase hex); everything else — including DEL and raw multi-byte
//! UTF-8 — passes through verbatim (idoc strings are valid UTF-8 by the
//! same trust boundary).

use crate::apply::Number;
use crate::cursor::{ArrEntries, DocValue, ObjEntries};

/// `JSON.GET` formatting options (RedisJSON `INDENT` / `NEWLINE` /
/// `SPACE`). All-empty means compact — the canonical mode.
#[derive(Copy, Clone, Default, Debug)]
pub struct SerializeOpts<'a> {
    /// Repeated once per depth level before each element (RedisJSON
    /// `INDENT`).
    pub indent: &'a [u8],
    /// Written before each element and each container closer (RedisJSON
    /// `NEWLINE`).
    pub newline: &'a [u8],
    /// Written after each object key's `:` (RedisJSON `SPACE`).
    pub space: &'a [u8],
}

impl SerializeOpts<'_> {
    fn is_compact(&self) -> bool {
        self.indent.is_empty() && self.newline.is_empty() && self.space.is_empty()
    }
}

/// Serialize `value` as JSON text, appending to `out`.
pub fn serialize_into(value: DocValue<'_>, opts: &SerializeOpts<'_>, out: &mut Vec<u8>) {
    if opts.is_compact() {
        compact(value, out);
    } else {
        formatted(value, opts, out, 0);
    }
}

/// A reply-shaped JSON tree (M3-S11; ADR-0041 D7): `$`-mode replies wrap
/// match sets in synthetic arrays (`JSON.GET $..x` → `[v, v]`) and
/// multi-path replies in a synthetic object keyed by the path strings as
/// given. The wrapper must format byte-identically to document
/// containers under `INDENT`/`NEWLINE`/`SPACE`, so it serializes here —
/// never hand-assembled at the command layer (which also must not own
/// JSON string escaping for the path keys).
#[derive(Debug)]
pub enum Reply<'a> {
    Value(DocValue<'a>),
    Array(Vec<Reply<'a>>),
    /// Keys are raw path text; escaped like any JSON string.
    Object(Vec<(&'a [u8], Reply<'a>)>),
}

/// Serialize a reply tree, appending to `out` (see [`Reply`]).
pub fn serialize_reply_into(reply: &Reply<'_>, opts: &SerializeOpts<'_>, out: &mut Vec<u8>) {
    enum Wrap<'a, 'r> {
        Arr { items: &'r [Reply<'a>], at: usize },
        Obj { items: &'r [(&'a [u8], Reply<'a>)], at: usize },
    }
    let compact_mode = opts.is_compact();
    let mut stack: Vec<Wrap<'_, '_>> = Vec::new();
    let mut next: Option<&Reply<'_>> = Some(reply);
    loop {
        if let Some(r) = next.take() {
            match r {
                Reply::Value(v) => {
                    if compact_mode {
                        compact(*v, out);
                    } else {
                        formatted(*v, opts, out, stack.len());
                    }
                }
                Reply::Array(items) => {
                    out.push(b'[');
                    stack.push(Wrap::Arr { items, at: 0 });
                }
                Reply::Object(items) => {
                    out.push(b'{');
                    stack.push(Wrap::Obj { items, at: 0 });
                }
            }
            continue;
        }
        let depth = stack.len();
        let Some(frame) = stack.last_mut() else {
            return;
        };
        match frame {
            Wrap::Arr { items, at } => {
                if *at < items.len() {
                    let item = &items[*at];
                    if *at > 0 {
                        out.push(b',');
                    }
                    *at += 1;
                    if !compact_mode {
                        break_indent(opts, depth, out);
                    }
                    next = Some(item);
                } else {
                    if !compact_mode && !items.is_empty() {
                        break_indent(opts, depth - 1, out);
                    }
                    out.push(b']');
                    stack.pop();
                }
            }
            Wrap::Obj { items, at } => {
                if *at < items.len() {
                    let (key, item) = &items[*at];
                    if *at > 0 {
                        out.push(b',');
                    }
                    *at += 1;
                    if !compact_mode {
                        break_indent(opts, depth, out);
                    }
                    write_string(out, key);
                    out.push(b':');
                    if !compact_mode {
                        out.extend_from_slice(opts.space);
                    }
                    next = Some(item);
                } else {
                    if !compact_mode && !items.is_empty() {
                        break_indent(opts, depth - 1, out);
                    }
                    out.push(b'}');
                    stack.pop();
                }
            }
        }
    }
}

/// `newline + indent×depth` — the element-break rule shared by the
/// document walker and the reply wrapper (module docs).
fn break_indent(opts: &SerializeOpts<'_>, depth: usize, out: &mut Vec<u8>) {
    out.extend_from_slice(opts.newline);
    for _ in 0..depth {
        out.extend_from_slice(opts.indent);
    }
}

/// Canonical mode: compact, fixed number formatting — the byte-exact
/// comparator for the E8 replay-equivalence oracle and the default
/// (option-less) reply shape.
pub fn serialize_canonical_into(value: DocValue<'_>, out: &mut Vec<u8>) {
    compact(value, out);
}

/// One open container on the walk stack. `first` distinguishes the entry
/// that takes no separator; iterators never pre-count (tape `len()` is a
/// walk — the flag costs nothing).
enum Frame<'a> {
    Obj { it: ObjEntries<'a>, first: bool },
    Arr { it: ArrEntries<'a>, first: bool },
}

fn compact(value: DocValue<'_>, out: &mut Vec<u8>) {
    let mut stack: Vec<Frame<'_>> = Vec::new();
    let mut next = Some(value);
    loop {
        if let Some(v) = next.take() {
            match v {
                DocValue::Obj(o) => {
                    out.push(b'{');
                    stack.push(Frame::Obj { it: o.iter(), first: true });
                }
                DocValue::Arr(a) => {
                    out.push(b'[');
                    stack.push(Frame::Arr { it: a.iter(), first: true });
                }
                scalar => write_scalar(scalar, out),
            }
            continue;
        }
        let Some(frame) = stack.last_mut() else {
            return;
        };
        match frame {
            Frame::Obj { it, first } => match it.next() {
                Some((key, v)) => {
                    if !core::mem::take(first) {
                        out.push(b',');
                    }
                    write_string(out, key.as_bytes());
                    out.push(b':');
                    next = Some(v);
                }
                None => {
                    out.push(b'}');
                    stack.pop();
                }
            },
            Frame::Arr { it, first } => match it.next() {
                Some(v) => {
                    if !core::mem::take(first) {
                        out.push(b',');
                    }
                    next = Some(v);
                }
                None => {
                    out.push(b']');
                    stack.pop();
                }
            },
        }
    }
}

/// `base` offsets the indentation when the value nests inside a
/// [`Reply`] wrapper; `depth == base + stack.len()` with the element's
/// own frame already on the stack.
fn formatted(value: DocValue<'_>, opts: &SerializeOpts<'_>, out: &mut Vec<u8>, base: usize) {
    let mut stack: Vec<Frame<'_>> = Vec::new();
    let mut next = Some(value);
    loop {
        if let Some(v) = next.take() {
            match v {
                DocValue::Obj(o) => {
                    out.push(b'{');
                    stack.push(Frame::Obj { it: o.iter(), first: true });
                }
                DocValue::Arr(a) => {
                    out.push(b'[');
                    stack.push(Frame::Arr { it: a.iter(), first: true });
                }
                scalar => write_scalar(scalar, out),
            }
            continue;
        }
        let depth = base + stack.len();
        let Some(frame) = stack.last_mut() else {
            return;
        };
        match frame {
            Frame::Obj { it, first } => match it.next() {
                Some((key, v)) => {
                    if !core::mem::take(first) {
                        out.push(b',');
                    }
                    break_indent(opts, depth, out);
                    write_string(out, key.as_bytes());
                    out.push(b':');
                    out.extend_from_slice(opts.space);
                    next = Some(v);
                }
                None => {
                    if !*first {
                        break_indent(opts, depth - 1, out);
                    }
                    out.push(b'}');
                    stack.pop();
                }
            },
            Frame::Arr { it, first } => match it.next() {
                Some(v) => {
                    if !core::mem::take(first) {
                        out.push(b',');
                    }
                    break_indent(opts, depth, out);
                    next = Some(v);
                }
                None => {
                    if !*first {
                        break_indent(opts, depth - 1, out);
                    }
                    out.push(b']');
                    stack.pop();
                }
            },
        }
    }
}

fn write_scalar(v: DocValue<'_>, out: &mut Vec<u8>) {
    match v {
        DocValue::Null => out.extend_from_slice(b"null"),
        DocValue::Bool(true) => out.extend_from_slice(b"true"),
        DocValue::Bool(false) => out.extend_from_slice(b"false"),
        DocValue::I64(i) => write_i64(out, i),
        DocValue::F64(f) => write_f64(out, f),
        DocValue::Str(s) => write_string(out, s.as_bytes()),
        DocValue::Obj(_) | DocValue::Arr(_) => unreachable!("containers handled by the walker"),
    }
}

/// Canonical JSON text for one mutation result into caller stack storage.
/// Returns the initialized prefix length (no allocation).
pub fn serialize_number_text(number: Number, out: &mut [u8; 32]) -> usize {
    match number {
        Number::I64(value) => {
            let mut tmp = [0u8; 20];
            let text = format_i64(value, &mut tmp);
            let len = text.len();
            out[..len].copy_from_slice(text);
            len
        }
        Number::F64(value) => {
            debug_assert!(value.is_finite());
            let mut buffer = zmij::Buffer::new();
            let text = buffer.format_finite(value).as_bytes();
            out[..text.len()].copy_from_slice(text);
            text.len()
        }
    }
}

fn write_i64(out: &mut Vec<u8>, v: i64) {
    let mut buf = [0u8; 20]; // "-9223372036854775808"
    out.extend_from_slice(format_i64(v, &mut buf));
}

fn format_i64(v: i64, buf: &mut [u8; 20]) -> &[u8] {
    let mut pos = buf.len();
    let mut magnitude = v.unsigned_abs();
    loop {
        pos -= 1;
        buf[pos] = b'0' + (magnitude % 10) as u8;
        magnitude /= 10;
        if magnitude == 0 {
            break;
        }
    }
    if v < 0 {
        pos -= 1;
        buf[pos] = b'-';
    }
    &buf[pos..]
}

fn write_f64(out: &mut Vec<u8>, v: f64) {
    debug_assert!(v.is_finite(), "idoc f64 payloads are finite (validator rule)");
    let mut buf = zmij::Buffer::new();
    out.extend_from_slice(buf.format_finite(v).as_bytes());
}

/// serde_json-parity JSON string escaping (module docs) over the raw
/// bytes; clean runs copy as slices.
fn write_string(out: &mut Vec<u8>, bytes: &[u8]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    out.push(b'"');
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b >= 0x20 && b != b'"' && b != b'\\' {
            i += 1;
            continue;
        }
        out.extend_from_slice(&bytes[start..i]);
        match b {
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            0x08 => out.extend_from_slice(b"\\b"),
            0x09 => out.extend_from_slice(b"\\t"),
            0x0A => out.extend_from_slice(b"\\n"),
            0x0C => out.extend_from_slice(b"\\f"),
            0x0D => out.extend_from_slice(b"\\r"),
            control => out.extend_from_slice(&[
                b'\\',
                b'u',
                b'0',
                b'0',
                HEX[(control >> 4) as usize],
                HEX[(control & 0x0F) as usize],
            ]),
        }
        i += 1;
        start = i;
    }
    out.extend_from_slice(&bytes[start..]);
    out.push(b'"');
}
