//! The serializer as it stood before the resumable walk, kept whole for the
//! tests: its entries, its two walkers, the reply wrapper's walker and the
//! string and number writers. It shares no code with the module it checks,
//! only the plain option and reply types, so a defect in the step, in the
//! bounded mapping or in a windowed writer reads as a difference of bytes
//! or of verdicts against this copy.

use crate::cursor::{ArrEntries, DocValue, ObjEntries};
use crate::ser::{Reply, ReplyTooLarge, SerializeOpts};

/// Serialize `value` as JSON text, appending to `out`.
pub fn serialize_into(value: DocValue<'_>, opts: &SerializeOpts<'_>, out: &mut Vec<u8>) {
    let Ok(()) = (if opts.is_compact() {
        compact::<false>(value, out, 0)
    } else {
        formatted::<false>(value, opts, out, 0, 0)
    }) else {
        unreachable!("unbounded serialization never refuses");
    };
}

/// [`serialize_into`] under a byte budget: refuses once `out` would pass
/// `limit` (counted over the whole buffer, so a caller measures from its
/// own baseline by passing `out.len() + budget`). On `Err` the buffer
/// holds a truncated prefix — callers roll the frame back
/// (`RespWriter::try_bulk_patched`) and answer an error. The overshoot
/// past `limit` is at most one scalar token (≤ ~32 B): every slice
/// append pre-checks, only single-byte structure and number tokens land
/// between checks.
pub fn serialize_into_bounded(
    value: DocValue<'_>,
    opts: &SerializeOpts<'_>,
    out: &mut Vec<u8>,
    limit: usize,
) -> Result<(), ReplyTooLarge> {
    if opts.is_compact() {
        compact::<true>(value, out, limit)
    } else {
        formatted::<true>(value, opts, out, 0, limit)
    }
}

/// Serialize a reply tree, appending to `out` (see [`Reply`]).
pub fn serialize_reply_into(reply: &Reply<'_>, opts: &SerializeOpts<'_>, out: &mut Vec<u8>) {
    let Ok(()) = reply_walk::<false>(reply, opts, out, 0) else {
        unreachable!("unbounded serialization never refuses");
    };
}

/// [`serialize_reply_into`] under a byte budget — the contract of
/// [`serialize_into_bounded`], for the reply-wrapper tree.
pub fn serialize_reply_into_bounded(
    reply: &Reply<'_>,
    opts: &SerializeOpts<'_>,
    out: &mut Vec<u8>,
    limit: usize,
) -> Result<(), ReplyTooLarge> {
    reply_walk::<true>(reply, opts, out, limit)
}

fn reply_walk<const BOUNDED: bool>(
    reply: &Reply<'_>,
    opts: &SerializeOpts<'_>,
    out: &mut Vec<u8>,
    limit: usize,
) -> Result<(), ReplyTooLarge> {
    enum Wrap<'a, 'r> {
        Arr { items: &'r [Reply<'a>], at: usize },
        Obj { items: &'r [(&'a [u8], Reply<'a>)], at: usize },
    }
    let compact_mode = opts.is_compact();
    let mut stack: Vec<Wrap<'_, '_>> = Vec::new();
    let mut next: Option<&Reply<'_>> = Some(reply);
    loop {
        check::<BOUNDED>(out, limit)?;
        if let Some(r) = next.take() {
            match r {
                Reply::Value(v) => {
                    if compact_mode {
                        compact::<BOUNDED>(*v, out, limit)?;
                    } else {
                        formatted::<BOUNDED>(*v, opts, out, stack.len(), limit)?;
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
            return Ok(());
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
                        break_indent::<BOUNDED>(opts, depth, out, limit)?;
                    }
                    next = Some(item);
                } else {
                    if !compact_mode && !items.is_empty() {
                        break_indent::<BOUNDED>(opts, depth - 1, out, limit)?;
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
                        break_indent::<BOUNDED>(opts, depth, out, limit)?;
                    }
                    write_string::<BOUNDED>(out, key, limit)?;
                    out.push(b':');
                    if !compact_mode {
                        budget_extend::<BOUNDED>(out, opts.space, limit)?;
                    }
                    next = Some(item);
                } else {
                    if !compact_mode && !items.is_empty() {
                        break_indent::<BOUNDED>(opts, depth - 1, out, limit)?;
                    }
                    out.push(b'}');
                    stack.pop();
                }
            }
        }
    }
}

/// Budget check over the whole buffer — compiled out entirely on the
/// unbounded paths (`BOUNDED = false` monomorphizes to a no-op, so the
/// canonical/index/replay serializers are byte-for-byte the pre-ADR-0099
/// code).
#[inline(always)]
fn check<const BOUNDED: bool>(out: &[u8], limit: usize) -> Result<(), ReplyTooLarge> {
    if BOUNDED && out.len() > limit {
        return Err(ReplyTooLarge);
    }
    Ok(())
}

/// Slice append with a pre-check: refuses before growing when the bytes
/// would pass the budget — a run that busts the limit proves the whole
/// reply does, so refusing early is exact, never a false refusal.
#[inline(always)]
fn budget_extend<const BOUNDED: bool>(
    out: &mut Vec<u8>,
    bytes: &[u8],
    limit: usize,
) -> Result<(), ReplyTooLarge> {
    if BOUNDED && out.len() + bytes.len() > limit {
        return Err(ReplyTooLarge);
    }
    out.extend_from_slice(bytes);
    Ok(())
}

/// `newline + indent×depth` — the element-break rule shared by the
/// document walker and the reply wrapper (module docs). `INDENT` repeats
/// per depth level, so the bounded arm checks per repetition — a 1 MiB
/// separator at depth 128 is 128 MiB from one call.
fn break_indent<const BOUNDED: bool>(
    opts: &SerializeOpts<'_>,
    depth: usize,
    out: &mut Vec<u8>,
    limit: usize,
) -> Result<(), ReplyTooLarge> {
    budget_extend::<BOUNDED>(out, opts.newline, limit)?;
    for _ in 0..depth {
        budget_extend::<BOUNDED>(out, opts.indent, limit)?;
    }
    Ok(())
}

/// One open container on the walk stack. `first` distinguishes the entry
/// that takes no separator; iterators never pre-count (tape `len()` is a
/// walk — the flag costs nothing).
enum Frame<'a> {
    Obj { it: ObjEntries<'a>, first: bool },
    Arr { it: ArrEntries<'a>, first: bool },
}

fn compact<const BOUNDED: bool>(
    value: DocValue<'_>,
    out: &mut Vec<u8>,
    limit: usize,
) -> Result<(), ReplyTooLarge> {
    let mut stack: Vec<Frame<'_>> = Vec::new();
    let mut next = Some(value);
    loop {
        check::<BOUNDED>(out, limit)?;
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
                scalar @ (DocValue::Null
                | DocValue::Bool(_)
                | DocValue::I64(_)
                | DocValue::F64(_)
                | DocValue::Str(_)) => write_scalar::<BOUNDED>(scalar, out, limit)?,
            }
            continue;
        }
        let Some(frame) = stack.last_mut() else {
            return Ok(());
        };
        match frame {
            Frame::Obj { it, first } => match it.next() {
                Some((key, v)) => {
                    if !core::mem::take(first) {
                        out.push(b',');
                    }
                    write_string::<BOUNDED>(out, key.as_bytes(), limit)?;
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
fn formatted<const BOUNDED: bool>(
    value: DocValue<'_>,
    opts: &SerializeOpts<'_>,
    out: &mut Vec<u8>,
    base: usize,
    limit: usize,
) -> Result<(), ReplyTooLarge> {
    let mut stack: Vec<Frame<'_>> = Vec::new();
    let mut next = Some(value);
    loop {
        check::<BOUNDED>(out, limit)?;
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
                scalar @ (DocValue::Null
                | DocValue::Bool(_)
                | DocValue::I64(_)
                | DocValue::F64(_)
                | DocValue::Str(_)) => write_scalar::<BOUNDED>(scalar, out, limit)?,
            }
            continue;
        }
        let depth = base + stack.len();
        let Some(frame) = stack.last_mut() else {
            return Ok(());
        };
        match frame {
            Frame::Obj { it, first } => match it.next() {
                Some((key, v)) => {
                    if !core::mem::take(first) {
                        out.push(b',');
                    }
                    break_indent::<BOUNDED>(opts, depth, out, limit)?;
                    write_string::<BOUNDED>(out, key.as_bytes(), limit)?;
                    out.push(b':');
                    budget_extend::<BOUNDED>(out, opts.space, limit)?;
                    next = Some(v);
                }
                None => {
                    if !*first {
                        break_indent::<BOUNDED>(opts, depth - 1, out, limit)?;
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
                    break_indent::<BOUNDED>(opts, depth, out, limit)?;
                    next = Some(v);
                }
                None => {
                    if !*first {
                        break_indent::<BOUNDED>(opts, depth - 1, out, limit)?;
                    }
                    out.push(b']');
                    stack.pop();
                }
            },
        }
    }
}

fn write_scalar<const BOUNDED: bool>(
    v: DocValue<'_>,
    out: &mut Vec<u8>,
    limit: usize,
) -> Result<(), ReplyTooLarge> {
    match v {
        DocValue::Null => out.extend_from_slice(b"null"),
        DocValue::Bool(true) => out.extend_from_slice(b"true"),
        DocValue::Bool(false) => out.extend_from_slice(b"false"),
        DocValue::I64(i) => write_i64(out, i),
        DocValue::F64(f) => write_f64(out, f),
        DocValue::Str(s) => write_string::<BOUNDED>(out, s.as_bytes(), limit)?,
        DocValue::Obj(_) | DocValue::Arr(_) => unreachable!("containers handled by the walker"),
    }
    Ok(())
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
/// bytes; clean runs copy as slices. The bounded arm pre-checks every
/// run and escape — a string is the one scalar with no intrinsic size
/// bound (`\u00xx` amplifies control bytes 6×), so it must not overshoot
/// the budget by more than one escape.
fn write_string<const BOUNDED: bool>(
    out: &mut Vec<u8>,
    bytes: &[u8],
    limit: usize,
) -> Result<(), ReplyTooLarge> {
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
        budget_extend::<BOUNDED>(out, &bytes[start..i], limit)?;
        let escape: &[u8] = match b {
            b'"' => b"\\\"",
            b'\\' => b"\\\\",
            0x08 => b"\\b",
            0x09 => b"\\t",
            0x0A => b"\\n",
            0x0C => b"\\f",
            0x0D => b"\\r",
            control => &[
                b'\\',
                b'u',
                b'0',
                b'0',
                HEX[(control >> 4) as usize],
                HEX[(control & 0x0F) as usize],
            ],
        };
        budget_extend::<BOUNDED>(out, escape, limit)?;
        i += 1;
        start = i;
    }
    budget_extend::<BOUNDED>(out, &bytes[start..], limit)?;
    out.push(b'"');
    Ok(())
}
