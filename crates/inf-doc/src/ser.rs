//! JSON text serializer (M3-S06): tape/arena cursors → JSON text, written
//! straight into a caller-supplied sink — no intermediate string, no
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
//! **One walker of tape values.** A tape value's text is written by the
//! resumable [`step`] walk, whose state is positions and never a borrow:
//! the five `serialize_*` entries run it to its end inside one call, and
//! [`OwnedEmit`] steps it over bytes it owns, under a byte budget and a
//! work budget. A reply wrapper ([`Reply`]) is a borrowed Rust tree no
//! step outlives, so its brackets and path keys are written here, around
//! the values.
//!
//! **A second walk order, for tree-form values** (a deviation, owner Kevin
//! Caicedo, expiry 2027-03-31): an arena value is written by this module's
//! tree walker, not by the step. It shares the escaping, separator and
//! number writers, so only the order of the walk is doubled, and the
//! fixpoint suite holds the two forms to the same bytes. Tree form is off
//! at the shipped default; the copy leaves when tree form is removed or
//! gated out.
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

use core::num::NonZeroU32;

use crate::apply::Number;
use crate::cursor::{ArrCursor, ArrEntries, DocValue, ObjCursor, ObjEntries};
use crate::limits::{ESCAPE_BYTES_MAX, I64_TEXT_BYTES_MAX};
use crate::tape::Dict;

mod step;

use step::{Bytes, Emitter, Opener, Unbounded};
pub use step::{
    Check, CheckStep, Emit, EmitError, EmitStep, Measure, MeasureStep, OwnedEmit, StepBudget,
};
#[cfg(any(test, fuzzing))]
pub use step::{SlicedStep, measure_sliced, serialize_sliced};

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
    pub(crate) fn is_compact(&self) -> bool {
        self.indent.is_empty() && self.newline.is_empty() && self.space.is_empty()
    }
}

/// Serialized text would exceed the caller's byte budget (ADR-0099,
/// review 2026-08-30 C9): a reply is not bounded by the document cap —
/// path repetition, `$..*` match amplification, formatting separators
/// and `\u00xx` escapes all multiply it — so reply construction carries
/// an explicit budget instead of building an unbounded buffer.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct ReplyTooLarge;

/// Where serialized text goes: bytes appended in order, and nothing read
/// back. A sink of fixed capacity sizes itself to its step's byte budget
/// plus [`EMIT_OVERSHOOT_BYTES_MAX`](crate::limits::EMIT_OVERSHOOT_BYTES_MAX),
/// what one step may write past it.
pub trait EmitSink {
    fn put(&mut self, bytes: &[u8]);

    /// One byte; a sink with a cheaper single-byte append overrides it.
    #[inline]
    fn put_byte(&mut self, byte: u8) {
        self.put(&[byte]);
    }

    /// A separator's bytes, most often one or two. A sink whose append of
    /// a slice is a call overrides it.
    #[inline]
    fn put_short(&mut self, bytes: &[u8]) {
        self.put(bytes);
    }
}

impl EmitSink for Vec<u8> {
    #[inline]
    fn put(&mut self, bytes: &[u8]) {
        self.extend_from_slice(bytes);
    }

    #[inline]
    fn put_byte(&mut self, byte: u8) {
        self.push(byte);
    }

    /// One or two bytes are pushed: a copy of a length known only at run
    /// time is a call, which costs more than the bytes. So is this
    /// function, at a separator an entry: it is inlined where it is used.
    #[inline(always)]
    fn put_short(&mut self, bytes: &[u8]) {
        match *bytes {
            [byte] => self.push(byte),
            [first, second] => {
                self.push(first);
                self.push(second);
            }
            _ => self.extend_from_slice(bytes),
        }
    }
}

/// The sink of a measure pass: it counts what the serializer would write
/// and keeps no byte. A count past `u64::MAX` is refused, not wrapped: the
/// sink stops counting and says so.
#[derive(Copy, Clone, Debug, Default)]
pub(crate) struct CountingSink {
    bytes: u64,
    overflowed: bool,
}

impl CountingSink {
    pub(crate) fn starting_at(bytes: u64) -> CountingSink {
        CountingSink { bytes, overflowed: false }
    }

    /// The bytes counted, or `None` once a count passed `u64::MAX`.
    pub(crate) fn total(&self) -> Option<u64> {
        if self.overflowed { None } else { Some(self.bytes) }
    }
}

impl EmitSink for CountingSink {
    #[inline]
    fn put(&mut self, bytes: &[u8]) {
        let more = u64::try_from(bytes.len()).ok();
        match more.and_then(|more| self.bytes.checked_add(more)) {
            Some(total) => self.bytes = total,
            None => self.overflowed = true,
        }
    }
}

/// Serialize `value` as JSON text, appending to `out`.
pub fn serialize_into(value: DocValue<'_>, opts: &SerializeOpts<'_>, out: &mut Vec<u8>) {
    let mut emitter = Emitter::new();
    let compact_mode = opts.is_compact();
    let Ok(()) = value_text::<false>(&mut emitter, value, opts, compact_mode, 0, out, 0) else {
        unreachable!("unbounded serialization never refuses");
    };
}

/// [`serialize_into`] under a byte budget: refuses once `out` would pass
/// `limit` (counted over the whole buffer, so a caller measures from its
/// own baseline by passing `out.len() + budget`). On `Err` the buffer
/// holds a truncated prefix — callers roll the frame back
/// (`RespWriter::try_bulk_patched`) and answer an error. The prefix ends
/// at most [`EMIT_OVERSHOOT_BYTES_MAX`](crate::limits::EMIT_OVERSHOOT_BYTES_MAX)
/// past `limit`: one escape sequence or one number, boolean or `null`
/// token, never a string or a separator.
pub fn serialize_into_bounded(
    value: DocValue<'_>,
    opts: &SerializeOpts<'_>,
    out: &mut Vec<u8>,
    limit: usize,
) -> Result<(), ReplyTooLarge> {
    let mut emitter = Emitter::new();
    value_text::<true>(&mut emitter, value, opts, opts.is_compact(), 0, out, limit)?;
    check::<true>(out, limit)
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
    let Ok(()) = reply_text::<false>(reply, opts, out, 0) else {
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
    reply_text::<true>(reply, opts, out, limit)
}

/// A reply's text. A reply of one value has no wrapper, so it never enters
/// the wrapper's walk: nothing is stacked for it. Each arm is one call, the
/// entry's last, so either kind of reply pays for its own walk alone.
#[inline(always)]
fn reply_text<const BOUNDED: bool>(
    reply: &Reply<'_>,
    opts: &SerializeOpts<'_>,
    out: &mut Vec<u8>,
    limit: usize,
) -> Result<(), ReplyTooLarge> {
    match reply {
        Reply::Value(value) => reply_value::<BOUNDED>(*value, opts, out, limit),
        Reply::Array(_) | Reply::Object(_) => reply_walk::<BOUNDED>(reply, opts, out, limit),
    }
}

/// A reply of one value: what [`reply_walk`] does for it, the budget
/// checked before the value and after, with no wrapper to keep.
#[inline(never)]
fn reply_value<const BOUNDED: bool>(
    value: DocValue<'_>,
    opts: &SerializeOpts<'_>,
    out: &mut Vec<u8>,
    limit: usize,
) -> Result<(), ReplyTooLarge> {
    check::<BOUNDED>(out, limit)?;
    let mut emitter = Emitter::new();
    value_text::<BOUNDED>(&mut emitter, value, opts, opts.is_compact(), 0, out, limit)?;
    check::<BOUNDED>(out, limit)
}

/// Canonical mode: compact, fixed number formatting — the byte-exact
/// comparator for the E8 replay-equivalence oracle and the default
/// (option-less) reply shape.
pub fn serialize_canonical_into(value: DocValue<'_>, out: &mut Vec<u8>) {
    let mut emitter = Emitter::new();
    let opts = SerializeOpts::default();
    let Ok(()) = value_text::<false>(&mut emitter, value, &opts, true, 0, out, 0) else {
        unreachable!("unbounded serialization never refuses");
    };
}

/// One open wrapper container of a [`Reply`] tree.
enum Wrap<'a, 'r> {
    Arr { items: &'r [Reply<'a>], at: usize },
    Obj { items: &'r [(&'a [u8], Reply<'a>)], at: usize },
}

/// The walker of the reply wrapper: a borrowed Rust tree, whose brackets,
/// commas, colons, separators and path keys are written here. Each value
/// goes through one emitter, re-rooted per value, with the wrapper's depth
/// as the base of its indentation. A function of its own: a reply of one
/// value ([`reply_value`]) pays for none of its state.
#[inline(never)]
#[allow(
    clippy::arithmetic_side_effects,
    reason = "bound: `at` counts the items of a slice below its length; `depth - 1` is under \
              a frame on the stack, so depth >= 1"
)]
fn reply_walk<const BOUNDED: bool>(
    reply: &Reply<'_>,
    opts: &SerializeOpts<'_>,
    out: &mut Vec<u8>,
    limit: usize,
) -> Result<(), ReplyTooLarge> {
    let compact_mode = opts.is_compact();
    let mut stack: Vec<Wrap<'_, '_>> = Vec::new();
    let mut emitter = Emitter::new();
    let mut next: Option<&Reply<'_>> = Some(reply);
    loop {
        check::<BOUNDED>(out, limit)?;
        if let Some(r) = next.take() {
            match r {
                Reply::Value(v) => {
                    let depth = stack.len();
                    value_text::<BOUNDED>(&mut emitter, *v, opts, compact_mode, depth, out, limit)?;
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
        let (count, at, closer) = match frame {
            Wrap::Arr { items, at } => (items.len(), at, b']'),
            Wrap::Obj { items, at } => (items.len(), at, b'}'),
        };
        if *at == count {
            if !compact_mode && count > 0 {
                break_whole::<BOUNDED>(opts, depth - 1, out, limit)?;
            }
            out.push(closer);
            stack.pop();
            continue;
        }
        if *at > 0 {
            out.push(b',');
        }
        let index = *at;
        *at += 1;
        if !compact_mode {
            break_whole::<BOUNDED>(opts, depth, out, limit)?;
        }
        next = Some(match frame {
            Wrap::Arr { items, .. } => &items[index],
            Wrap::Obj { items, .. } => {
                let (key, item) = &items[index];
                string_whole::<BOUNDED>(out, key, limit)?;
                out.push(b':');
                if !compact_mode {
                    space_whole::<BOUNDED>(opts, out, limit)?;
                }
                item
            }
        });
    }
}

/// Budget check over the whole buffer — compiled out entirely on the
/// unbounded paths (`BOUNDED = false` monomorphizes to a no-op, so the
/// canonical/index/replay serializers pay nothing for it).
#[inline(always)]
fn check<const BOUNDED: bool>(out: &[u8], limit: usize) -> Result<(), ReplyTooLarge> {
    if BOUNDED && out.len() > limit {
        return Err(ReplyTooLarge);
    }
    Ok(())
}

/// What `limit` leaves a one-shot writer: its whole remaining budget, the
/// window it hands a windowed writer. Unbounded, every window fits.
#[inline(always)]
fn budget_left<const BOUNDED: bool>(out: &[u8], limit: usize) -> usize {
    if BOUNDED { limit.saturating_sub(out.len()) } else { usize::MAX }
}

/// A tape value's source: the root value's bytes and its document's key
/// dictionary. Every position a walk holds is an offset into one of them.
#[derive(Copy, Clone, Debug)]
pub(crate) struct TapeSource<'a> {
    pub(crate) region: &'a [u8],
    pub(crate) dict: Dict<'a>,
}

/// How one value is written: the one function every entry builds its root
/// by.
pub(crate) enum Root<'a> {
    /// `null`, a boolean or a number: at most 32 B of text, written whole.
    Atom(Atom),
    /// A tape container or any string: the resumable walk. A container's
    /// region is its body and a string's its payload, neither with a tag.
    Tape { source: TapeSource<'a>, opener: Opener },
    /// A tree-form container: the tree walker.
    Tree(DocValue<'a>),
}

impl<'a> Root<'a> {
    pub(crate) fn of(value: DocValue<'a>) -> Root<'a> {
        match value {
            DocValue::Null => Root::Atom(Atom::Null),
            DocValue::Bool(b) => Root::Atom(Atom::Bool(b)),
            DocValue::I64(i) => Root::Atom(Atom::I64(i)),
            DocValue::F64(f) => Root::Atom(Atom::F64(f)),
            DocValue::Str(s) => {
                let source = TapeSource { region: s.as_bytes(), dict: Dict::empty() };
                Root::Tape { source, opener: Opener::Str }
            }
            DocValue::Obj(ObjCursor::Tape(o)) => {
                let source = TapeSource { region: o.entries, dict: o.dict };
                Root::Tape { source, opener: Opener::Obj }
            }
            DocValue::Arr(ArrCursor::Tape(a)) => {
                let source = TapeSource { region: a.body, dict: a.dict };
                Root::Tape { source, opener: Opener::Arr }
            }
            tree @ (DocValue::Obj(ObjCursor::Arena(_)) | DocValue::Arr(ArrCursor::Arena(_))) => {
                Root::Tree(tree)
            }
        }
    }
}

/// One value's text into `out`. `compact_mode` is `opts.is_compact()`,
/// read once a reply. `base_depth` is the depth of the wrapper the value
/// nests in: it offsets the value's indentation and nothing else.
#[inline(always)]
fn value_text<const BOUNDED: bool>(
    emitter: &mut Emitter,
    value: DocValue<'_>,
    opts: &SerializeOpts<'_>,
    compact_mode: bool,
    base_depth: usize,
    out: &mut Vec<u8>,
    limit: usize,
) -> Result<(), ReplyTooLarge> {
    match Root::of(value) {
        Root::Atom(atom) => {
            put_atom(out, atom);
            Ok(())
        }
        Root::Tape { source, opener } => {
            emitter
                .root(opener, source, base_depth)
                .expect("a value's bytes lie in one document, below the u32 position width");
            if compact_mode {
                run_steps::<BOUNDED, false>(emitter, source, opts, out, limit)
            } else {
                run_steps::<BOUNDED, true>(emitter, source, opts, out, limit)
            }
        }
        Root::Tree(tree) => {
            if compact_mode {
                compact::<BOUNDED>(tree, out, limit)
            } else {
                formatted::<BOUNDED>(tree, opts, out, base_depth, limit)
            }
        }
    }
}

/// Runs a rooted emitter to its end. Bounded, this is the one mapping of
/// `limit` onto step budgets:
///
/// 1. nothing left under `limit` refuses before any step: every value
///    writes at least one byte, and a step that yields has bytes left;
/// 2. a step's byte budget is what `limit` leaves, at most `u32::MAX`, and
///    it has no work budget: a value holds fewer than 2³² values;
/// 3. a step that yields returns to 1;
/// 4. a step that ends is `Ok` exactly when the buffer is within `limit`.
///
/// So `Ok` means the total is at most `limit`, and on `Err` the buffer
/// ends at most one unsplit token past it.
///
/// One function a form, never a part of its caller: inlined into an
/// entry, the same loop reads 3 % more instructions on a kibibyte value
/// (`benches/ser.rs`), and which entry takes it changes with every edit
/// near one.
#[inline(never)]
fn run_steps<const BOUNDED: bool, const FORMATTED: bool>(
    emitter: &mut Emitter,
    source: TapeSource<'_>,
    opts: &SerializeOpts<'_>,
    out: &mut Vec<u8>,
    limit: usize,
) -> Result<(), ReplyTooLarge> {
    if !BOUNDED {
        let end = emitter.step::<Unbounded, FORMATTED>(source, opts, out, Unbounded);
        debug_assert_eq!(end, EmitStep::Done, "a step without a budget runs to its end");
        return Ok(());
    }
    loop {
        let Some(bytes_max) = step_bytes_max(out.len(), limit) else {
            return Err(ReplyTooLarge);
        };
        match emitter.step::<Bytes, FORMATTED>(source, opts, out, Bytes(bytes_max)) {
            // Planted: a step that stopped short is taken for the value's end.
            EmitStep::Yield if cfg!(inf_canary_ser_yield_is_ok) => return Ok(()),
            EmitStep::Yield => {}
            EmitStep::Done => return check::<true>(out, limit),
        }
    }
}

/// Rule 2 of the bounded mapping; `None` is rule 1's refusal.
#[inline]
fn step_bytes_max(len: usize, limit: usize) -> Option<NonZeroU32> {
    let left = limit.checked_sub(len)?;
    NonZeroU32::new(u32::try_from(left).unwrap_or(u32::MAX))
}

/// What one windowed write did: the bytes it wrote and the source bytes it
/// consumed. A caller resumes at `from + consumed`.
#[derive(Copy, Clone, Debug)]
pub(crate) struct Windowed {
    pub(crate) written: usize,
    pub(crate) consumed: usize,
    /// Source bytes the scan read, for the harnesses that hold a step to
    /// its window: counted in the scan loop's own body.
    #[cfg(any(test, fuzzing))]
    pub(crate) examined: usize,
}

/// serde_json-parity JSON string escaping (module docs) of `bytes[from..]`,
/// without its quotes, into at most `window` bytes of output: the one
/// escaping function. It stops once `window` bytes are written, and never
/// inside an escape sequence, so it passes the window by at most one
/// escape less a byte. It never reads ahead of what the window can take: a
/// clean byte writes one byte, so a step examines no more source than it
/// may write. A string whose worst case fits the window (6 B a byte) is
/// written by the loop with no check inside.
#[inline(always)]
pub(crate) fn put_escaped<S: EmitSink>(
    sink: &mut S,
    bytes: &[u8],
    from: usize,
    window: usize,
) -> Windowed {
    let rest = &bytes[from..];
    // A shift where the exact test divides: 8 B a byte is past the worst case.
    if rest.len() <= window >> WORST_CASE_SHIFT {
        let written = put_escaped_whole(sink, rest);
        return Windowed {
            written,
            consumed: rest.len(),
            #[cfg(any(test, fuzzing))]
            examined: rest.len(),
        };
    }
    put_escaped_window(sink, rest, window)
}

/// A string of `n` bytes writes at most `n << WORST_CASE_SHIFT`.
const WORST_CASE_SHIFT: u32 = 3;
const _: () = assert!(ESCAPE_BYTES_MAX <= 1 << WORST_CASE_SHIFT);

/// Whether a byte is written as an escape sequence: `"`, `\`, and every
/// control byte below 0x20.
static NEEDS_ESCAPE: [bool; 256] = {
    let mut table = [false; 256];
    let mut byte = 0;
    while byte < 0x20 {
        table[byte] = true;
        byte += 1;
    }
    table[b'"' as usize] = true;
    table[b'\\' as usize] = true;
    table
};

#[inline(always)]
fn is_clean(byte: u8) -> bool {
    !NEEDS_ESCAPE[usize::from(byte)]
}

/// The escape of one byte that is not clean; returns its length.
#[inline(always)]
fn put_escape<S: EmitSink>(sink: &mut S, byte: u8) -> usize {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let escape: &[u8] = match byte {
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
            HEX[usize::from(control >> 4)],
            HEX[usize::from(control & 0x0F)],
        ],
    };
    sink.put(escape);
    escape.len()
}

/// A whole string under one check: clean runs copy as slices.
#[inline(always)]
#[allow(
    clippy::arithmetic_side_effects,
    reason = "bound: `clean` is a position in `run`, so `clean + 1 <= run.len()`; an escape \
              is at least 2 bytes; `written` is at most 6 bytes of output a byte of a slice in \
              memory"
)]
fn put_escaped_whole<S: EmitSink>(sink: &mut S, bytes: &[u8]) -> usize {
    let mut written = bytes.len();
    let mut run = bytes;
    while let Some(clean) = run.iter().position(|&byte| !is_clean(byte)) {
        sink.put(&run[..clean]);
        written += put_escape(sink, run[clean]) - 1;
        run = &run[clean + 1..];
    }
    sink.put(run);
    written
}

/// A string past what the window can take whole: each scan is of at most
/// the bytes the window has left.
#[inline(never)]
#[allow(
    clippy::arithmetic_side_effects,
    reason = "bound: `at <= bytes.len()` and `written < window` under the loop guard; a run is \
              at most what each has left; the counts are of bytes of a slice in memory"
)]
fn put_escaped_window<S: EmitSink>(sink: &mut S, bytes: &[u8], window: usize) -> Windowed {
    let mut written = 0;
    let mut at = 0;
    #[cfg(any(test, fuzzing))]
    let mut examined = 0;
    while at < bytes.len() && written < window {
        let run_max = (window - written).min(bytes.len() - at);
        // Planted: the scan runs to the string's next escape, however far.
        let scan_max = if cfg!(inf_canary_ser_scan_to_escape) { bytes.len() - at } else { run_max };
        let found = bytes[at..at + scan_max].iter().position(|&byte| !is_clean(byte));
        #[cfg(any(test, fuzzing))]
        {
            examined += found.map_or(scan_max, |clean| clean + 1);
        }
        let clean = found.unwrap_or(scan_max).min(run_max);
        sink.put(&bytes[at..at + clean]);
        written += clean;
        at += clean;
        if clean < run_max {
            written += put_escape(sink, bytes[at]);
            at += 1;
        }
    }
    Windowed {
        written,
        consumed: at,
        #[cfg(any(test, fuzzing))]
        examined,
    }
}

/// One whole string with its quotes, for a caller that does not suspend:
/// its window is its whole remaining budget, and a string the window ends
/// inside is the budget's refusal.
#[inline]
fn string_whole<const BOUNDED: bool>(
    out: &mut Vec<u8>,
    bytes: &[u8],
    limit: usize,
) -> Result<(), ReplyTooLarge> {
    out.push(b'"');
    let window = budget_left::<BOUNDED>(out, limit);
    let did = put_escaped(out, bytes, 0, window);
    if did.consumed < bytes.len() {
        return Err(ReplyTooLarge);
    }
    out.push(b'"');
    Ok(())
}

/// The three separator strings of a formatted reply.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Part {
    Newline,
    Indent,
    Space,
}

/// A position inside a separator: the copy of `part` being written and the
/// next byte of that copy. A break is `newline` once, then `indent` once
/// per depth level; `space` is one copy. `copy` is at most the depth and
/// `at` below one option's length, so neither narrows.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct SeparatorPosition {
    pub(crate) part: Part,
    pub(crate) copy: usize,
    pub(crate) at: usize,
}

impl SeparatorPosition {
    /// The start of a break.
    pub(crate) const BREAK: SeparatorPosition =
        SeparatorPosition { part: Part::Newline, copy: 0, at: 0 };
    /// The start of the space after a key's colon.
    pub(crate) const SPACE: SeparatorPosition =
        SeparatorPosition { part: Part::Space, copy: 0, at: 0 };
}

/// Writes a separator from `position` into at most `window` bytes of output, a
/// partial copy included: the one separator function. A break repeats
/// `indent` `depth` times. Returns the bytes written and whether the
/// separator ended, and leaves `position` where the next call resumes: at the
/// separator's end once it ended, where a call writes nothing and says so
/// again. A separator that fits the window whole is written with no check
/// inside.
#[inline(always)]
pub(crate) fn put_separator<S: EmitSink>(
    sink: &mut S,
    opts: &SerializeOpts<'_>,
    depth: usize,
    position: &mut SeparatorPosition,
    window: usize,
) -> (usize, bool) {
    if *position == SeparatorPosition::BREAK
        && let Some(total) = break_bytes(opts, depth)
        && total <= window
    {
        sink.put_short(opts.newline);
        for _ in 0..depth {
            sink.put_short(opts.indent);
        }
        *position = SeparatorPosition { part: Part::Indent, copy: depth, at: 0 };
        return (total, true);
    }
    if *position == SeparatorPosition::SPACE && opts.space.len() <= window {
        sink.put_short(opts.space);
        *position = SeparatorPosition { part: Part::Space, copy: 1, at: 0 };
        return (opts.space.len(), true);
    }
    put_separator_window(sink, opts, depth, position, window)
}

/// A separator the window cannot take whole, or one resumed inside.
#[inline(never)]
#[allow(
    clippy::arithmetic_side_effects,
    reason = "bound: `at` is below its part's length and `written` below `window` where each is \
              subtracted; `copy` is below `depth`; `written` counts bytes taken from options \
              in memory, a window's worth at most"
)]
pub(crate) fn put_separator_window<S: EmitSink>(
    sink: &mut S,
    opts: &SerializeOpts<'_>,
    depth: usize,
    position: &mut SeparatorPosition,
    window: usize,
) -> (usize, bool) {
    let mut written = 0;
    loop {
        let (bytes, copies) = match position.part {
            Part::Newline => (opts.newline, 1),
            // An empty indent has no copy to walk.
            Part::Indent => (opts.indent, if opts.indent.is_empty() { 0 } else { depth }),
            Part::Space => (opts.space, 1),
        };
        if position.copy >= copies {
            match position.part {
                Part::Newline => {
                    *position = SeparatorPosition { part: Part::Indent, copy: 0, at: 0 }
                }
                Part::Indent | Part::Space => return (written, true),
            }
            continue;
        }
        let take = (bytes.len() - position.at).min(window - written);
        sink.put(&bytes[position.at..position.at + take]);
        written += take;
        position.at += take;
        if position.at < bytes.len() {
            return (written, false);
        }
        *position = SeparatorPosition { part: position.part, copy: position.copy + 1, at: 0 };
    }
}

/// `newline + indent × depth` in bytes, or `None` past `usize`.
#[inline]
fn break_bytes(opts: &SerializeOpts<'_>, depth: usize) -> Option<usize> {
    opts.indent.len().checked_mul(depth)?.checked_add(opts.newline.len())
}

/// `newline + indent×depth` — the element-break rule shared by the tree
/// walker and the reply wrapper (module docs), for a caller that does not
/// suspend: a break the window ends inside is the budget's refusal.
#[inline]
fn break_whole<const BOUNDED: bool>(
    opts: &SerializeOpts<'_>,
    depth: usize,
    out: &mut Vec<u8>,
    limit: usize,
) -> Result<(), ReplyTooLarge> {
    let mut position = SeparatorPosition::BREAK;
    separator_whole::<BOUNDED>(opts, depth, &mut position, out, limit)
}

/// The space after a key's colon, for a caller that does not suspend.
#[inline]
fn space_whole<const BOUNDED: bool>(
    opts: &SerializeOpts<'_>,
    out: &mut Vec<u8>,
    limit: usize,
) -> Result<(), ReplyTooLarge> {
    let mut position = SeparatorPosition::SPACE;
    separator_whole::<BOUNDED>(opts, 0, &mut position, out, limit)
}

#[inline]
fn separator_whole<const BOUNDED: bool>(
    opts: &SerializeOpts<'_>,
    depth: usize,
    position: &mut SeparatorPosition,
    out: &mut Vec<u8>,
    limit: usize,
) -> Result<(), ReplyTooLarge> {
    let window = budget_left::<BOUNDED>(out, limit);
    let (_, ended) = put_separator(out, opts, depth, position, window);
    if ended { Ok(()) } else { Err(ReplyTooLarge) }
}

/// One open container on the tree walker's stack. `first` distinguishes
/// the entry that takes no separator.
enum Frame<'a> {
    Obj { it: ObjEntries<'a>, first: bool },
    Arr { it: ArrEntries<'a>, first: bool },
}

/// The tree walker's value arm: an atom or a string is written whole, a
/// container is opened onto the stack.
#[inline]
fn tree_value<'a, const BOUNDED: bool>(
    value: DocValue<'a>,
    stack: &mut Vec<Frame<'a>>,
    out: &mut Vec<u8>,
    limit: usize,
) -> Result<(), ReplyTooLarge> {
    match value {
        DocValue::Obj(o) => {
            out.push(b'{');
            stack.push(Frame::Obj { it: o.iter(), first: true });
        }
        DocValue::Arr(a) => {
            out.push(b'[');
            stack.push(Frame::Arr { it: a.iter(), first: true });
        }
        DocValue::Null => {
            put_atom(out, Atom::Null);
        }
        DocValue::Bool(b) => {
            put_atom(out, Atom::Bool(b));
        }
        DocValue::I64(i) => {
            put_atom(out, Atom::I64(i));
        }
        DocValue::F64(f) => {
            put_atom(out, Atom::F64(f));
        }
        DocValue::Str(s) => string_whole::<BOUNDED>(out, s.as_bytes(), limit)?,
    }
    Ok(())
}

/// The compact walk of a tree-form value: the second walk order the module
/// docs date.
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
            tree_value::<BOUNDED>(v, &mut stack, out, limit)?;
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
                    string_whole::<BOUNDED>(out, key.as_bytes(), limit)?;
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

/// The formatted walk of a tree-form value. `base` offsets the indentation
/// when the value nests inside a [`Reply`] wrapper; `depth == base +
/// stack.len()` with the element's own frame already on the stack.
#[allow(
    clippy::arithmetic_side_effects,
    reason = "bound: `base` is a wrapper's stack length and `stack.len()` at most a \
              document's depth; `depth - 1` is under a frame on the stack, so depth >= 1"
)]
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
            tree_value::<BOUNDED>(v, &mut stack, out, limit)?;
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
                    break_whole::<BOUNDED>(opts, depth, out, limit)?;
                    string_whole::<BOUNDED>(out, key.as_bytes(), limit)?;
                    out.push(b':');
                    space_whole::<BOUNDED>(opts, out, limit)?;
                    next = Some(v);
                }
                None => {
                    if !*first {
                        break_whole::<BOUNDED>(opts, depth - 1, out, limit)?;
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
                    break_whole::<BOUNDED>(opts, depth, out, limit)?;
                    next = Some(v);
                }
                None => {
                    if !*first {
                        break_whole::<BOUNDED>(opts, depth - 1, out, limit)?;
                    }
                    out.push(b']');
                    stack.pop();
                }
            },
        }
    }
}

/// A token no step splits: at most 32 B of text by its type
/// ([`EMIT_OVERSHOOT_BYTES_MAX`](crate::limits::EMIT_OVERSHOOT_BYTES_MAX)),
/// written whole. A string is never one.
#[derive(Copy, Clone, Debug)]
pub(crate) enum Atom {
    Null,
    Bool(bool),
    I64(i64),
    F64(f64),
}

/// Writes one atom whole; returns its length.
#[inline(always)]
pub(crate) fn put_atom<S: EmitSink>(sink: &mut S, atom: Atom) -> usize {
    match atom {
        Atom::Null => {
            sink.put(b"null");
            4
        }
        Atom::Bool(true) => {
            sink.put(b"true");
            4
        }
        Atom::Bool(false) => {
            sink.put(b"false");
            5
        }
        Atom::I64(v) => {
            // One digit, the commonest number, is one byte and no copy.
            if let Ok(digit @ 0..=9) = u8::try_from(v) {
                sink.put_byte(b'0' | digit);
                return 1;
            }
            let mut digits = [0u8; I64_TEXT_BYTES_MAX];
            let text = format_i64(v, &mut digits);
            sink.put(text);
            text.len()
        }
        Atom::F64(v) => {
            debug_assert!(v.is_finite(), "idoc f64 payloads are finite (validator rule)");
            let mut buffer = zmij::Buffer::new();
            let text = buffer.format_finite(v).as_bytes();
            sink.put(text);
            text.len()
        }
    }
}

/// Canonical JSON text for one mutation result into caller stack storage.
/// Returns the initialized prefix length (no allocation).
pub fn serialize_number_text(number: Number, out: &mut [u8; 32]) -> usize {
    match number {
        Number::I64(value) => {
            let mut tmp = [0u8; I64_TEXT_BYTES_MAX];
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

#[allow(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    reason = "bound: an `i64` has at most 19 digits and a sign, so `position` steps down from \
              20 at most 20 times; `magnitude % 10` is below 10, and `b'0' + 9` is a `u8`"
)]
#[inline]
fn format_i64(v: i64, buf: &mut [u8; I64_TEXT_BYTES_MAX]) -> &[u8] {
    let mut position = buf.len();
    let mut magnitude = v.unsigned_abs();
    loop {
        position -= 1;
        buf[position] = b'0' + (magnitude % 10) as u8;
        magnitude /= 10;
        if magnitude == 0 {
            break;
        }
    }
    if v < 0 {
        position -= 1;
        buf[position] = b'-';
    }
    &buf[position..]
}
