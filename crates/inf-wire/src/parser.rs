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
//! [`ParserLimits::max_frame_bytes`]; a declared bulk length over the cap is
//! rejected **from its header line** — a `$104857600` announcement on a
//! 1 MiB-cap connection fails immediately without buffering a byte of
//! payload (the bounded-accumulator AC).
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

use inf_simd::{find_crlf, swar_parse_int};

/// Most args carried without allocation (frozen contract: "no alloc ≤ 16").
pub const INLINE_ARGS: usize = 16;

/// Per-connection parser limits. Defaults are the M0 reference shape.
#[derive(Copy, Clone, Debug)]
pub struct ParserLimits {
    /// Hard cap for one frame (and the partial-frame accumulator).
    pub max_frame_bytes: usize,
    /// Maximum argv entries per command.
    pub max_args: usize,
}

impl Default for ParserLimits {
    fn default() -> ParserLimits {
        ParserLimits { max_frame_bytes: 1024 * 1024, max_args: 1024 }
    }
}

/// Typed protocol failure. Display matches Redis error phrasing where the
/// compat harness diffs replies.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum WireError {
    /// Declared or accumulated frame exceeds `max_frame_bytes`.
    FrameTooLarge { declared: usize, cap: usize },
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
    len: u32,
    spill: Vec<(u32, u32)>,
}

impl<'a> ArgvRef<'a> {
    /// Argument `i` (0 = command name).
    ///
    /// # Panics
    /// Panics if `i >= len()` — argv indexes come from arity-checked code.
    #[inline]
    pub fn arg(&self, i: usize) -> &'a [u8] {
        let (start, len) =
            if i < INLINE_ARGS { self.inline[i] } else { self.spill[i - INLINE_ARGS] };
        &self.frame[start as usize..(start + len) as usize]
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len as usize
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
                    self.pos += used;
                    let offsets = match kind {
                        // Zero-arg frames (`*0`, blank inline line) are
                        // consumed silently, per Redis semantics.
                        FrameKind::Empty => continue,
                        FrameKind::Args(offsets) => offsets,
                    };
                    let buf: &[u8] = match self.mode {
                        Mode::Direct => self.input,
                        Mode::Accumulated => &self.parser.acc,
                        Mode::Done => unreachable!("mode checked above"),
                    };
                    let is_inline = offsets.is_inline_frame;
                    let argv = offsets.materialize(&buf[base..]);
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
    len: u32,
    spill: Vec<(u32, u32)>,
    is_inline_frame: bool,
}

impl ArgOffsets {
    fn new(is_inline_frame: bool) -> ArgOffsets {
        ArgOffsets { inline: [(0, 0); INLINE_ARGS], len: 0, spill: Vec::new(), is_inline_frame }
    }

    fn push(&mut self, start: usize, len: usize) {
        let entry = (start as u32, len as u32);
        let i = self.len as usize;
        if i < INLINE_ARGS {
            self.inline[i] = entry;
        } else {
            self.spill.push(entry);
        }
        self.len += 1;
    }

    fn count(&self) -> usize {
        self.len as usize
    }

    /// Phase B: bind the offsets to the frame bytes — one struct move, no
    /// per-arg re-slicing (`ArgvRef::arg` slices on demand).
    fn materialize(self, frame: &[u8]) -> ArgvRef<'_> {
        ArgvRef { frame, inline: self.inline, len: self.len, spill: self.spill }
    }
}

enum FrameKind {
    Args(ArgOffsets),
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
    if argc < 0 {
        return ParseOne::Error(WireError::BadMultibulkLen);
    }
    if argc as u64 > limits.max_args as u64 {
        return ParseOne::Error(WireError::TooManyArgs {
            declared: argc as u64,
            cap: limits.max_args,
        });
    }
    if argc == 0 {
        return ParseOne::Frame(FrameKind::Empty, pos);
    }

    let mut offsets = ArgOffsets::new(false);
    for _ in 0..argc {
        let Some(&marker) = buf.get(pos) else { return ParseOne::Incomplete };
        if marker != b'$' {
            return ParseOne::Error(WireError::ExpectedBulk { found: marker });
        }
        let (len, after_len) = match parse_count_line(buf, pos + 1, WireError::BadBulkLen) {
            Ok(Some(v)) => v,
            Ok(None) => return ParseOne::Incomplete,
            Err(e) => return ParseOne::Error(e),
        };
        if len < 0 {
            return ParseOne::Error(WireError::BadBulkLen);
        }
        let len = len as usize;
        // Early reject from the header line: a 100 MB announcement on a
        // 1 MiB-cap connection dies HERE, before buffering any payload.
        if len > limits.max_frame_bytes {
            return ParseOne::Error(WireError::FrameTooLarge {
                declared: len,
                cap: limits.max_frame_bytes,
            });
        }
        let payload_end = after_len + len;
        if buf.len() < payload_end + 2 {
            return ParseOne::Incomplete;
        }
        if &buf[payload_end..payload_end + 2] != b"\r\n" {
            return ParseOne::Error(WireError::ExpectedCrlf);
        }
        offsets.push(after_len, len);
        pos = payload_end + 2;
    }
    ParseOne::Frame(FrameKind::Args(offsets), pos)
}

/// Parses `<digits>\r\n` starting at `from`. `Ok(Some((value, after_crlf)))`
/// on success, `Ok(None)` while the line could still complete, `Err` when it
/// can never become valid.
fn parse_count_line(
    buf: &[u8],
    from: usize,
    err: WireError,
) -> Result<Option<(i64, usize)>, WireError> {
    let line = &buf[from.min(buf.len())..];
    match swar_parse_int(line) {
        Some((value, used)) => match line.get(used..used + 2) {
            Some(b"\r\n") => Ok(Some((value, from + used + 2))),
            Some(_) => Err(err),
            // CRLF not fully arrived: incomplete only while the next bytes
            // could still be `\r\n`.
            None => {
                if line.len() == used || (line.len() == used + 1 && line[used] == b'\r') {
                    Ok(None)
                } else {
                    Err(err)
                }
            }
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
fn parse_inline(buf: &[u8], limits: &ParserLimits) -> ParseOne {
    let Some(end) = find_crlf(buf, 0) else {
        return if buf.len() > limits.max_frame_bytes {
            ParseOne::Error(WireError::FrameTooLarge {
                declared: buf.len(),
                cap: limits.max_frame_bytes,
            })
        } else {
            ParseOne::Incomplete
        };
    };
    let line = &buf[..end];
    let used = end + 2;
    let mut offsets = ArgOffsets::new(true);
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
            offsets.push(start, i - start);
        }
    }
    if offsets.count() == 0 {
        return ParseOne::Frame(FrameKind::Empty, used);
    }
    ParseOne::Frame(FrameKind::Args(offsets), used)
}
