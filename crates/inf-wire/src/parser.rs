//! RESP command parser (M0-S11): resumable per-connection state over
//! borrowed input slices.
//!
//! ## Mechanical sympathy
//!
//! Client→server RESP is exactly one shape — `*argc` then `argc` bulk
//! strings — so length prefixes tell the parser where **every** CRLF must
//! be. The hot path therefore performs *zero scanning*: SWAR-parse the
//! count/length digits (`inf_simd::swar_parse_int`), bounds-check the
//! expected `\r\n`, and slice the payload without ever reading it. SIMD CRLF
//! scanning (`inf_simd::find_crlf`) is needed only for inline commands (the
//! telnet/debug path).
//!
//! ## Buffer model (the Vortex lesson, adapted to provided buffers)
//!
//! Frames wholly inside one `feed` input parse **zero-copy** — argv slices
//! borrow the wire buffer. A frame spanning recv buffers falls back to a
//! bounded per-connection accumulator: the partial tail is copied in (the
//! only copy in the parser), completed by later feeds, and parsed from
//! there. The accumulator is hard-capped by
//! [`ParserLimits::max_frame_bytes`] — the frame as a whole, from its
//! declared layout (ADR-0122 D1) — and each bulk by
//! [`ParserLimits::max_bulk_bytes`] (`proto-max-bulk-len`); a declared
//! length over either cap is rejected **from its length line** — a
//! `$104857600` announcement on a 16 MiB-cap connection fails immediately
//! without buffering a byte of payload (the bounded-accumulator AC).
//!
//! ## Retention rule, enforced by lifetimes
//!
//! [`FrameIter`] is a *lending* iterator: each [`FrameIter::next`] item
//! borrows the iterator, so a frame **cannot** outlive the step that
//! produced it — the "frames never outlive EXECUTE unless copied" contract
//! is a compile error to violate, not a convention. (Recorded interface
//! deviation: the freeze sketched a plain `Iterator`, which would have let
//! borrowed frames dangle past accumulator maintenance.)
//!
//! A protocol error poisons the parser: the iterator yields the error once
//! and nothing after — the server closes the connection (RESP has no
//! resynchronization point).

#![cfg_attr(
    not(test),
    deny(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_possible_wrap,
        clippy::arithmetic_side_effects
    )
)]

use inf_simd::{find_crlf, swar_parse_int};

use crate::limits::{ARGV_ENTRIES_MAX, ARGV_FRAME_BYTES_MAX};

/// Most args carried without allocation (frozen contract: "no alloc ≤ 16").
pub const INLINE_ARGS: usize = 16;

/// The product bulk cap: `proto-max-bulk-len`'s default (ADR-0122) — the
/// record bound (`MAX_VAL_LEN` + 1), so every storable value is admitted
/// and nothing unstorable is buffered.
pub const DEFAULT_MAX_BULK_BYTES: usize = 16 << 20;
/// Headroom a frame gets past its largest bulk: the command name, a key at
/// its bound and the length lines (ADR-0122 D1).
pub const FRAME_HEADROOM_BYTES: usize = 64 << 10;

/// Per-connection parser limits. Defaults are the product shape
/// (`proto-max-bulk-len` mirrors [`DEFAULT_MAX_BULK_BYTES`]).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct ParserLimits {
    /// Hard cap for one bulk string, checked from its length line before
    /// any payload is buffered (`proto-max-bulk-len`).
    pub max_bulk_bytes: usize,
    /// Hard cap for one whole frame — every bulk plus the length lines —
    /// and therefore the partial-frame accumulator. Checked from the
    /// declared lengths, before the bytes arrive. The argv representation
    /// additionally limits each frame to `ARGV_FRAME_BYTES_MAX`.
    pub max_frame_bytes: usize,
    /// Maximum argv entries per command.
    pub max_args: usize,
}

impl ParserLimits {
    /// Limits for a bulk cap: the frame gets [`FRAME_HEADROOM_BYTES`] past
    /// it, the argv bound stays the registry's 1024.
    #[must_use]
    pub const fn for_bulk_cap(max_bulk_bytes: usize) -> ParserLimits {
        ParserLimits {
            max_bulk_bytes,
            max_frame_bytes: max_bulk_bytes.saturating_add(FRAME_HEADROOM_BYTES),
            max_args: 1024,
        }
    }
}

impl Default for ParserLimits {
    fn default() -> ParserLimits {
        ParserLimits::for_bulk_cap(DEFAULT_MAX_BULK_BYTES)
    }
}

/// Typed protocol failure. Display matches Redis error phrasing where the
/// compat harness diffs replies.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum WireError {
    /// A bulk's declared length (or an inline line) exceeds
    /// `max_bulk_bytes`.
    FrameTooLarge { declared: usize, cap: usize },
    /// The frame's declared layout exceeds `max_frame_bytes` (ADR-0122
    /// D1: the accumulator is bounded per frame, not per bulk).
    FrameTooLong { size: usize, cap: usize },
    /// Multibulk argc over `max_args`.
    TooManyArgs { declared: u64, cap: usize },
    /// `*` line is not a well-formed non-negative count.
    BadMultibulkLen,
    /// `$` line is not a well-formed non-negative length.
    BadBulkLen,
    /// Array element did not start with `$`.
    ExpectedBulk { found: u8 },
    /// Mandatory `\r\n` missing at a length-determined position.
    ExpectedCrlf,
    /// Inline command malformed.
    BadInline,
}

impl core::fmt::Display for WireError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            WireError::FrameTooLarge { declared, cap } => {
                write!(f, "invalid bulk length: {declared} exceeds limit {cap}")
            }
            WireError::FrameTooLong { size, cap } => {
                write!(f, "invalid frame length: {size} exceeds limit {cap}")
            }
            WireError::TooManyArgs { declared, cap } => {
                write!(f, "invalid multibulk count: {declared} exceeds limit {cap}")
            }
            WireError::BadMultibulkLen => write!(f, "invalid multibulk length"),
            WireError::BadBulkLen => write!(f, "invalid bulk length"),
            WireError::ExpectedBulk { found } => {
                write!(f, "expected '$', got '{}'", char::from(*found))
            }
            WireError::ExpectedCrlf => write!(f, "expected CRLF"),
            WireError::BadInline => write!(f, "invalid inline command"),
        }
    }
}

impl std::error::Error for WireError {}

/// One parsed command's argument vector. Offset-based over the frame slice
/// rather than an array of fat pointers: half the struct size (the enum
/// moves by value through the iterator), no re-slicing pass after parse, and
/// `arg(i)` is two `u32` loads + a bounds-elided slice. Inline up to
/// [`INLINE_ARGS`]; heap spill only beyond that (DEL with a long key list).
pub struct ArgvRef<'a> {
    frame: &'a [u8],
    inline: [(u32, u32); INLINE_ARGS],
    len: usize,
    spill: Vec<(u32, u32)>,
}

impl<'a> ArgvRef<'a> {
    /// Argument `i` (0 = command name).
    ///
    /// # Panics
    /// Panics if `i >= len()` — argv indexes come from arity-checked code.
    #[inline]
    #[allow(
        clippy::arithmetic_side_effects,
        reason = "bound: spill subtraction follows i >= INLINE_ARGS; start and len are u32 \
                  widened before addition, so their sum fits the required 64-bit usize"
    )]
    pub fn arg(&self, i: usize) -> &'a [u8] {
        let (start, len) =
            if i < INLINE_ARGS { self.inline[i] } else { self.spill[i - INLINE_ARGS] };
        &self.frame[start as usize..start as usize + len as usize]
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = &'a [u8]> + '_ {
        (0..self.len()).map(|i| self.arg(i))
    }
}

impl core::fmt::Debug for ArgvRef<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_list().entries(self.iter().map(String::from_utf8_lossy)).finish()
    }
}

/// One parse outcome (frozen shape). [`FrameIter::next`] yields
/// `Command`/`Inline`, surfaces `ProtocolError` once, then `None`;
/// `Incomplete` is the iterator's `None` (more bytes needed).
#[derive(Debug)]
pub enum Parsed<'a> {
    /// `*argc` array-of-bulks command — the standard client form.
    Command(ArgvRef<'a>),
    /// Whitespace-split inline command (telnet/debug path).
    Inline(ArgvRef<'a>),
    /// More bytes needed to complete the frame.
    Incomplete,
    /// Connection-fatal protocol error.
    ProtocolError(WireError),
}

/// Per-connection resumable parser state.
#[derive(Debug)]
pub struct ConnParser {
    limits: ParserLimits,
    /// Partial-frame carry between feeds. Always starts at a frame boundary;
    /// bounded by `limits.max_frame_bytes` (+ one recv buffer transiently).
    acc: Vec<u8>,
    poisoned: bool,
}

impl ConnParser {
    pub fn new(limits: ParserLimits) -> ConnParser {
        ConnParser { limits, acc: Vec::new(), poisoned: false }
    }

    /// Bytes currently held for a spanning frame (tests + memory asserts).
    pub fn buffered(&self) -> usize {
        self.acc.len()
    }

    /// The limits in force.
    pub fn limits(&self) -> ParserLimits {
        self.limits
    }

    /// Re-limits a live connection (`CONFIG SET proto-max-bulk-len`,
    /// ADR-0122 D2): the next frame parses under the new caps; a
    /// spanning frame already past a lowered cap fails at its next feed.
    pub fn set_limits(&mut self, limits: ParserLimits) {
        self.limits = limits;
    }

    /// True after a protocol error: the connection must be closed.
    pub fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    /// Feed one wire buffer; drain complete commands with
    /// `while let Some(parsed) = iter.next()`. Drive the iterator to `None`
    /// — remaining bytes are carried to the next feed when it finishes (or
    /// is dropped).
    pub fn feed<'p>(&'p mut self, input: &'p [u8]) -> FrameIter<'p> {
        let mode = if self.poisoned {
            Mode::Done
        } else if self.acc.is_empty() {
            Mode::Direct
        } else {
            // Spanning frame: complete it in the accumulator (the one copy).
            self.acc.extend_from_slice(input);
            Mode::Accumulated
        };
        FrameIter { parser: self, input, pos: 0, mode }
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum Mode {
    /// Zero-copy parse over `input`.
    Direct,
    /// Parse over `parser.acc` (input already appended).
    Accumulated,
    /// Iteration finished (tail stashed, error surfaced, or poisoned).
    Done,
}

/// Lending iterator over the complete commands of one feed: each item
/// borrows the iterator (`Parsed<'_>`), enforcing the retention rule at
/// compile time. Use `while let Some(p) = iter.next()`.
#[derive(Debug)]
pub struct FrameIter<'p> {
    parser: &'p mut ConnParser,
    input: &'p [u8],
    pos: usize,
    mode: Mode,
}

impl FrameIter<'_> {
    /// Next complete command, a one-time `ProtocolError`, or `None` (feed
    /// exhausted; partial tail carried to the next feed).
    ///
    /// Two-phase internally: phase A parses to *offsets* (no borrows held),
    /// phase B re-borrows the buffer to materialize argv slices — this is
    /// what lets the borrow checker accept mutation on the other paths.
    #[allow(clippy::should_implement_trait)] // lending shape: items borrow self
    #[allow(
        clippy::arithmetic_side_effects,
        reason = "bound: the frame slice at buf[base..][..used] is checked before pos + used"
    )]
    pub fn next(&mut self) -> Option<Parsed<'_>> {
        loop {
            let outcome = {
                let buf: &[u8] = match self.mode {
                    Mode::Direct => self.input,
                    Mode::Accumulated => &self.parser.acc,
                    Mode::Done => return None,
                };
                parse_one(&buf[self.pos..], &self.parser.limits)
            };
            match outcome {
                ParseOne::Frame(kind, used) => {
                    let base = self.pos;
                    let frame_len = {
                        let buf = match self.mode {
                            Mode::Direct => self.input,
                            Mode::Accumulated => &self.parser.acc,
                            Mode::Done => unreachable!("mode checked above"),
                        };
                        buf[base..][..used].len()
                    };
                    self.pos += frame_len;
                    let (offsets, is_inline) = match kind {
                        // Zero-arg frames (`*0`, blank inline line) are
                        // consumed silently, per Redis semantics.
                        FrameKind::Empty => continue,
                        FrameKind::Command(offsets) => (offsets, false),
                        FrameKind::Inline(offsets) => (offsets, true),
                    };
                    let frame = match self.mode {
                        Mode::Direct => &self.input[base..self.pos],
                        Mode::Accumulated => &self.parser.acc[base..self.pos],
                        Mode::Done => unreachable!("mode checked above"),
                    };
                    let argv = offsets.materialize(frame);
                    return Some(if is_inline {
                        Parsed::Inline(argv)
                    } else {
                        Parsed::Command(argv)
                    });
                }
                ParseOne::Incomplete => {
                    self.stash_tail();
                    return None;
                }
                ParseOne::Error(err) => {
                    self.parser.poisoned = true;
                    self.parser.acc = Vec::new();
                    self.mode = Mode::Done;
                    return Some(Parsed::ProtocolError(err));
                }
            }
        }
    }

    /// On Incomplete (or early drop): carry the unconsumed tail across
    /// feeds and end iteration.
    fn stash_tail(&mut self) {
        match self.mode {
            Mode::Direct => {
                let tail = &self.input[self.pos..];
                if !tail.is_empty() {
                    self.parser.acc.extend_from_slice(tail);
                }
            }
            Mode::Accumulated => {
                if self.pos == self.parser.acc.len() {
                    // Fully drained: release the spanning-frame storage so
                    // steady-state connections hold no parser memory.
                    self.parser.acc = Vec::new();
                } else {
                    self.parser.acc.drain(..self.pos);
                }
            }
            Mode::Done => {}
        }
        self.pos = 0;
        self.mode = Mode::Done;
    }
}

impl Drop for FrameIter<'_> {
    fn drop(&mut self) {
        // Early drop (e.g. caller hit its budget) must not lose bytes.
        self.stash_tail();
    }
}

/// Argument positions within one frame, offsets relative to the frame
/// start. Phase-A output: plain data, no borrows — what lets the lending
/// iterator mutate parser state on the non-yielding paths.
struct ArgOffsets {
    inline: [(u32, u32); INLINE_ARGS],
    /// Inline entries plus spill entries, maintained only by `push`.
    len: usize,
    spill: Vec<(u32, u32)>,
}

impl ArgOffsets {
    fn new() -> ArgOffsets {
        ArgOffsets { inline: [(0, 0); INLINE_ARGS], len: 0, spill: Vec::new() }
    }

    #[allow(
        clippy::arithmetic_side_effects,
        reason = "bound: len is the inline prefix plus spill.len(); the inline indexed write \
                  bounds its prefix to 16, and a successful Vec<(u32,u32)>::push keeps the \
                  spill below isize::MAX / 8, so the usize count plus one fits"
    )]
    fn push(&mut self, start: u32, len: u32) {
        let entry = (start, len);
        let i = self.len;
        if i < INLINE_ARGS {
            self.inline[i] = entry;
        } else {
            self.spill.push(entry);
        }
        self.len += 1;
    }

    fn count(&self) -> usize {
        self.len
    }

    /// Phase B: bind the offsets to the frame bytes — one struct move, no
    /// per-arg re-slicing (`ArgvRef::arg` slices on demand).
    fn materialize(self, frame: &[u8]) -> ArgvRef<'_> {
        ArgvRef { frame, inline: self.inline, len: self.len, spill: self.spill }
    }
}

enum FrameKind {
    Command(ArgOffsets),
    Inline(ArgOffsets),
    /// Consumed bytes but produced no command (`*0`, blank inline line).
    Empty,
}

enum ParseOne {
    Frame(FrameKind, usize),
    Incomplete,
    Error(WireError),
}

/// Parses one frame from the front of `buf`. The core loop — no scanning on
/// the multibulk path (lengths determine every CRLF position).
fn parse_one(buf: &[u8], limits: &ParserLimits) -> ParseOne {
    let Some(&first) = buf.first() else { return ParseOne::Incomplete };
    if first == b'*' { parse_multibulk(buf, limits) } else { parse_inline(buf, limits) }
}

/// `*argc\r\n` then `argc` × `$len\r\n<payload>\r\n`.
fn parse_multibulk(buf: &[u8], limits: &ParserLimits) -> ParseOne {
    let (argc, mut pos) = match parse_count_line(buf, 1, WireError::BadMultibulkLen) {
        Ok(Some(v)) => v,
        Ok(None) => return ParseOne::Incomplete,
        Err(e) => return ParseOne::Error(e),
    };
    let Ok(argc) = u64::try_from(argc) else {
        return ParseOne::Error(WireError::BadMultibulkLen);
    };
    let cap = limits.max_args.min(ARGV_ENTRIES_MAX);
    if argc > cap as u64 {
        return ParseOne::Error(WireError::TooManyArgs { declared: argc, cap });
    }
    if argc == 0 {
        let cap = limits.max_frame_bytes.min(ARGV_FRAME_BYTES_MAX);
        if pos > cap {
            return ParseOne::Error(WireError::FrameTooLong { size: pos, cap });
        }
        return ParseOne::Frame(FrameKind::Empty, pos);
    }

    let mut offsets = ArgOffsets::new();
    for _ in 0..argc {
        let (start, len, end) = match parse_bulk(buf, pos, limits) {
            BulkParse::Complete { start, len, end } => (start, len, end),
            BulkParse::Incomplete => return ParseOne::Incomplete,
            BulkParse::Error(error) => return ParseOne::Error(error),
        };
        offsets.push(start, len);
        pos = end;
    }
    ParseOne::Frame(FrameKind::Command(offsets), pos)
}

enum BulkParse {
    Complete { start: u32, len: u32, end: usize },
    Incomplete,
    Error(WireError),
}

/// Validate the declared layout before buffering the payload or narrowing its offsets.
#[inline]
#[allow(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    reason = "bound: pos + 1 follows buf.get(pos) succeeding; frame_end - 2 follows \
              checked_add(2) succeeding; checked start + len + 2 <= the u32 frame cap \
              precedes narrowing either nonnegative offset"
)]
fn parse_bulk(buf: &[u8], pos: usize, limits: &ParserLimits) -> BulkParse {
    let Some(&marker) = buf.get(pos) else { return BulkParse::Incomplete };
    if marker != b'$' {
        return BulkParse::Error(WireError::ExpectedBulk { found: marker });
    }
    let (len, start) = match parse_count_line(buf, pos + 1, WireError::BadBulkLen) {
        Ok(Some(value)) => value,
        Ok(None) => return BulkParse::Incomplete,
        Err(error) => return BulkParse::Error(error),
    };
    let Ok(len) = usize::try_from(len) else {
        return BulkParse::Error(WireError::BadBulkLen);
    };
    if len > limits.max_bulk_bytes {
        return BulkParse::Error(WireError::FrameTooLarge {
            declared: len,
            cap: limits.max_bulk_bytes,
        });
    }
    let cap = limits.max_frame_bytes.min(ARGV_FRAME_BYTES_MAX);
    let Some(frame_end) = start.checked_add(len).and_then(|end| end.checked_add(2)) else {
        return BulkParse::Error(WireError::FrameTooLong { size: usize::MAX, cap });
    };
    if frame_end > cap {
        return BulkParse::Error(WireError::FrameTooLong { size: frame_end, cap });
    }
    if buf.len() < frame_end {
        return BulkParse::Incomplete;
    }
    if &buf[frame_end - 2..frame_end] != b"\r\n" {
        return BulkParse::Error(WireError::ExpectedCrlf);
    }
    BulkParse::Complete { start: start as u32, len: len as u32, end: frame_end }
}

/// Parses `<digits>\r\n` starting at `from`. `Ok(Some((value, after_crlf)))`
/// on success, `Ok(None)` while the line could still complete, `Err` when it
/// can never become valid.
#[allow(
    clippy::arithmetic_side_effects,
    reason = "bound: matching two CRLF bytes at line[used..] proves from + used + 2 <= buf.len()"
)]
fn parse_count_line(
    buf: &[u8],
    from: usize,
    err: WireError,
) -> Result<Option<(i64, usize)>, WireError> {
    let line = &buf[from.min(buf.len())..];
    match swar_parse_int(line) {
        Some((value, used)) => match line.get(used..) {
            Some([b'\r', b'\n', ..]) => Ok(Some((value, from + used + 2))),
            // CRLF not fully arrived: incomplete only while the next bytes
            // could still be `\r\n`.
            Some([] | [b'\r']) => Ok(None),
            Some(_) | None => Err(err),
        },
        None => match line {
            // No digits yet: the line may still grow into a number.
            [] | [b'-'] | [b'+'] => Ok(None),
            _ => Err(err),
        },
    }
}

/// Inline command: one CRLF-terminated line, whitespace-split — the only
/// path that scans (`inf_simd::find_crlf`); bounded by the frame cap.
#[allow(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    reason = "bound: end + 2 <= isize::MAX + 2; i increments only below line.len(), \
              i - start follows i > start; offsets.count() <= u32::MAX before + 1; \
              used <= the u32 frame cap precedes narrowing start and i - start <= end"
)]
fn parse_inline(buf: &[u8], limits: &ParserLimits) -> ParseOne {
    let Some(end) = find_crlf(buf, 0) else {
        let cap = limits.max_frame_bytes.min(ARGV_FRAME_BYTES_MAX);
        return if buf.len() > limits.max_bulk_bytes {
            ParseOne::Error(WireError::FrameTooLarge {
                declared: buf.len(),
                cap: limits.max_bulk_bytes,
            })
        } else if buf.len() > cap {
            ParseOne::Error(WireError::FrameTooLong { size: buf.len(), cap })
        } else {
            ParseOne::Incomplete
        };
    };
    let line = &buf[..end];
    let used = end + 2;
    if end > limits.max_bulk_bytes {
        return ParseOne::Error(WireError::FrameTooLarge {
            declared: end,
            cap: limits.max_bulk_bytes,
        });
    }
    let cap = limits.max_frame_bytes.min(ARGV_FRAME_BYTES_MAX);
    if used > cap {
        return ParseOne::Error(WireError::FrameTooLong { size: used, cap });
    }
    let mut offsets = ArgOffsets::new();
    let mut i = 0;
    while i < line.len() {
        while i < line.len() && line[i].is_ascii_whitespace() {
            i += 1;
        }
        let start = i;
        while i < line.len() && !line[i].is_ascii_whitespace() {
            i += 1;
        }
        if i > start {
            if offsets.count() >= limits.max_args {
                return ParseOne::Error(WireError::TooManyArgs {
                    declared: offsets.count() as u64 + 1,
                    cap: limits.max_args,
                });
            }
            offsets.push(start as u32, (i - start) as u32);
        }
    }
    if offsets.count() == 0 {
        return ParseOne::Frame(FrameKind::Empty, used);
    }
    ParseOne::Frame(FrameKind::Inline(offsets), used)
}
