//! JSON text → canonical `idoc` tape (M3-S05): the simdjson technique,
//! stage-fused (ADR-0047 D2 item 3). Stage 1's SIMD classification stays
//! batched (`inf_simd::json_classify_blocks` — raw 32 B masks per 64 B
//! block, one tight pass), but escape/string resolution and token
//! consumption stream through `inf_simd::JsonTokenCursor`: the grammar
//! machine pulls each token offset straight out of the per-block emit
//! mask in-register, instead of materializing a `Vec<u32>` structural
//! index and reloading every entry (the two costs the batch shape paid
//! between stages). The grammar machine walks those tokens and emits
//! canonical bytes directly through the shared [`emit`](crate::emit)
//! primitives — no intermediate DOM and no second bookkeeping stack: the
//! grammar machine itself is the invariant authority (key/value
//! alternation, depth, and the S07 idoc-byte size guard, enforced
//! incrementally), and separators (`:`/`,`) are consumed fused with the
//! values they frame instead of costing dispatch round trips. Width
//! selection lives in `emit` alone, so the tape stays canonical by
//! construction (L7).
//!
//! Decisions this module pins (oracle-verified at S21 where the local
//! oracle lacks the JSON module — see the S05 ledger entry):
//! - **Numbers** (ADR-0036 D4): integral and in i64 range → integer;
//!   everything else f64 (std's Eisel–Lemire parse — round-trip-correct).
//!   `-0` stays `-0.0f64` (the serde_json/RedisJSON lineage rule);
//!   overflowing exponents (`1e400`) are typed errors, never ±Inf;
//!   leading zeros and `+` signs reject per JSON.
//! - **Duplicate keys** (ADR-0036 D5): last occurrence wins, first
//!   position kept (IndexMap semantics) — detected per object, repaired
//!   by one body splice on the rare object that actually has them.
//! - **Strings**: `\uXXXX` escapes incl. surrogate pairs; lone/out-of-
//!   order surrogates reject; raw control bytes (< 0x20) reject; content
//!   must be valid UTF-8. Unescaped strings borrow straight from the
//!   input into the tape (zero copy).
//!
//! Errors carry byte offsets (`unexpected character at offset N` family);
//! the wire layer maps them to RESP phrasing at S11 against the oracle.

use core::fmt;

use crate::apply::Number;
use crate::emit;
use crate::header;
use crate::limits::{DEPTH_MAX, DOC_BYTES_MAX};
use crate::tape::{FIXINT_MAX, FIXINT_MIN, FIXSTR_BASE, FIXSTR_MAX_LEN, TAG_ARR, TAG_OBJ};

/// Objects up to this many entries detect duplicates by per-insert byte
/// compare (length prefilter via slice equality); larger objects defer to
/// one O(k log k) sort-based pass at close (bounded-everything: no
/// quadratic blowup on hostile wide objects). Hashing was profiled out:
/// per-key hash64 cost more than the compares it saved on real shapes.
const LINEAR_SCAN_MAX: usize = 256;

/// A typed parse failure at a byte offset.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct JsonParseError {
    pub offset: usize,
    pub kind: JsonErrorKind,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum JsonErrorKind {
    /// A token that no grammar position admits.
    UnexpectedCharacter(u8),
    /// Input ended mid-value.
    UnexpectedEnd,
    /// Bytes after the root value.
    TrailingCharacters,
    /// Malformed number (leading zero, bare `.`/`e`, `+`, lone `-`, …).
    InvalidNumber,
    /// A finite-unrepresentable number (overflowing exponent).
    NumberOutOfRange,
    /// Unknown `\x` escape.
    InvalidEscape,
    /// `\u` not followed by four hex digits.
    InvalidUnicodeEscape,
    /// Lone or out-of-order UTF-16 surrogate escape.
    LoneSurrogate,
    /// String content is not valid UTF-8.
    InvalidUtf8,
    /// Raw control byte (< 0x20) inside a string.
    ControlCharacter,
    /// String never closed.
    UnterminatedString,
    /// Nesting beyond the depth cap (default 128, RedisJSON parity).
    DepthExceeded,
    /// Encoded document exceeds the idoc byte cap (the S07 seam).
    DocumentTooLarge,
}

impl fmt::Display for JsonParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let offset = self.offset;
        match self.kind {
            JsonErrorKind::UnexpectedCharacter(b) => {
                write!(f, "unexpected character '{}' at offset {offset}", b.escape_ascii())
            }
            JsonErrorKind::UnexpectedEnd => write!(f, "unexpected end of input at offset {offset}"),
            JsonErrorKind::TrailingCharacters => {
                write!(f, "trailing characters at offset {offset}")
            }
            JsonErrorKind::InvalidNumber => write!(f, "invalid number at offset {offset}"),
            JsonErrorKind::NumberOutOfRange => write!(f, "number out of range at offset {offset}"),
            JsonErrorKind::InvalidEscape => write!(f, "invalid escape at offset {offset}"),
            JsonErrorKind::InvalidUnicodeEscape => {
                write!(f, "invalid unicode escape at offset {offset}")
            }
            JsonErrorKind::LoneSurrogate => {
                write!(f, "lone surrogate in unicode escape at offset {offset}")
            }
            JsonErrorKind::InvalidUtf8 => write!(f, "invalid UTF-8 at offset {offset}"),
            JsonErrorKind::ControlCharacter => {
                write!(f, "control character in string at offset {offset}")
            }
            JsonErrorKind::UnterminatedString => {
                write!(f, "unterminated string at offset {offset}")
            }
            JsonErrorKind::DepthExceeded => {
                write!(f, "document nesting too deep at offset {offset}")
            }
            JsonErrorKind::DocumentTooLarge => write!(f, "document too large at offset {offset}"),
        }
    }
}

impl core::error::Error for JsonParseError {}

fn err<T>(offset: usize, kind: JsonErrorKind) -> Result<T, JsonParseError> {
    Err(JsonParseError { offset, kind })
}

/// The parser's output tape: bytes plus the incremental S07 idoc-byte
/// guard. The grammar machine enforces structure; this type carries only
/// the size cap (checked exactly before every payload copy, so peak
/// memory during a rejection stays bounded by the cap plus one token).
/// The buffer is caller-owned (`parse_into`), so the ingest seam recycles
/// one allocation across parses — and after a rejection the caller can
/// observe exactly how much memory the refused parse held (S07).
struct Tape<'o> {
    out: &'o mut Vec<u8>,
    max_body: usize,
}

impl Tape<'_> {
    #[inline]
    fn fits(&self, extra: usize) -> bool {
        self.out.len() - header::HEADER_LEN + extra <= self.max_body
    }
}

/// One emitted object entry (duplicate-key bookkeeping): the entry's
/// offset in the output plus its key's byte span — recorded at emit time
/// so duplicate scans never re-decode tags (profiled at 13%). A cached
/// padded key word (u64 compare instead of memcmp) lost its A/B in the
/// optimization slice — the per-key word build outweighed the scan
/// savings on wide shapes (−11.7%) — and stays out.
#[derive(Copy, Clone, Debug)]
struct ObjEntry {
    entry_at: u32,
    key_at: u32,
    key_len: u16,
}

/// Per-object frame state, pooled across parses.
#[derive(Default, Debug)]
struct ObjFrame {
    entries: Vec<ObjEntry>,
    /// Output offset where this object's body begins.
    body_start: u32,
    dup_found: bool,
    /// 64-bit key-fingerprint filter: one bit per key hash
    /// ([`key_fingerprint`]). A clear bit at insert proves the key is new,
    /// skipping the linear dup scan — the common all-distinct-keys object
    /// pays one AND per key instead of O(k) prior-entry compares. A set
    /// bit only means "possible duplicate": the memcmp scan stays the
    /// authority, so accept/reject behavior is untouched. (A lazier
    /// variant — count + filter only, entries materialized by body walk
    /// on demand — lost its A/B on the budget shape across two
    /// fingerprint designs and is recorded, not merged: stage-fusion
    /// artifact d4/d5 rows.)
    fp: u64,
}

impl ObjFrame {
    fn reset(&mut self, body_start: u32) {
        self.entries.clear();
        self.body_start = body_start;
        self.dup_found = false;
        self.fp = 0;
    }
}

/// Key fingerprint for the [`ObjFrame::fp`] filter: length, first and
/// last byte — the fields distinct sibling keys differ in essentially
/// always, and all loads the insert path already owns. Equal keys always
/// fingerprint equal (the filter's correctness half); unequal keys
/// usually differ (its effectiveness half). Known cheap-by-design
/// collision: English near-twins (`"name"`/`"note"`) share len/first/
/// last — they cost one short memcmp scan, which measured cheaper than
/// every stronger-hash variant tried (d5 rows, same artifact).
#[inline]
fn key_fingerprint(key: &[u8]) -> u64 {
    let (first, last) = match key {
        [] => (0u32, 0u32),
        [b] => (u32::from(*b), u32::from(*b)),
        [first, .., last] => (u32::from(*first), u32::from(*last)),
    };
    let h = (key.len() as u32)
        .wrapping_mul(131)
        .wrapping_add(first.wrapping_mul(31))
        .wrapping_add(last.wrapping_mul(7));
    1u64 << (h & 63)
}

/// What the grammar machine expects next. `:` and `,` never appear here:
/// separators are consumed fused with the token before them.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum Expect {
    Value,
    ValueOrArrClose,
    KeyOrObjClose,
    Key,
}

/// Container-frame word on the parser's stack: bit 31 distinguishes
/// object from array; the low bits hold the u24 length-placeholder
/// offset (≤ header + 16 MiB cap < 2³¹, by the format ceiling).
const OBJ_BIT: u32 = 1 << 31;
const LEN_AT_MASK: u32 = OBJ_BIT - 1;

/// Ingest limits (M3-S07): the per-namespace configuration surface.
/// Every field is clamped to its format ceiling at parser construction —
/// configuration lowers bounds, never raises them (`limits` module law).
#[derive(Copy, Clone, Debug)]
pub struct ParseLimits {
    /// Maximum container nesting depth (ceiling [`DEPTH_MAX`]; RedisJSON
    /// parity default 128).
    pub max_depth: usize,
    /// Maximum input **text** bytes — enforced before UTF-8 validation
    /// and before the structural index allocates (reject-before-allocate:
    /// scratch growth is proportional to input, so the text bound is what
    /// bounds scratch). Note a pretty-printed text of a cap-passing
    /// document can exceed this; the wire frame is bounded regardless.
    pub max_text: usize,
    /// Maximum encoded idoc **body** bytes — enforced incrementally
    /// during stage 2 (ceiling [`DOC_BYTES_MAX`]). Independent of
    /// `max_text` by design: small-token documents encode larger than
    /// their text (`1e1,` is 4 text bytes and 9 tape bytes), so the text
    /// bound alone does not bound memory.
    pub max_body: usize,
}

impl Default for ParseLimits {
    fn default() -> ParseLimits {
        ParseLimits { max_depth: DEPTH_MAX, max_text: DOC_BYTES_MAX, max_body: DOC_BYTES_MAX }
    }
}

/// A reusable JSON parser: one per cell — every scratch buffer (block
/// masks, container-frame stack, unescape buffer, splice buffer, object
/// frames, the scalar tier's structural index) is retained across calls,
/// so the hot ingest path allocates only the output tape (and
/// [`parse_into`](JsonParser::parse_into) recycles even that).
#[derive(Debug)]
pub struct JsonParser {
    blocks: Vec<inf_simd::BlockMasks>,
    indices: Vec<u32>,
    frames: Vec<u32>,
    unescape: Vec<u8>,
    rebuild: Vec<u8>,
    obj_frames: Vec<ObjFrame>,
    limits: ParseLimits,
}

/// The grammar machine's view of stage 1: one-token lookahead over the
/// structural stream. Two monomorphizations exist — the stage-fused
/// [`inf_simd::JsonTokenCursor`] (the hot path) and [`IndexTokens`] over
/// the scalar oracle's batch index (the L4 off arm / portability proof) —
/// so the grammar machine stays single-homed and the differential suite
/// exercises both through one code path.
trait TokenSource {
    fn peek(&mut self) -> Option<u32>;
    fn bump(&mut self);
}

impl TokenSource for inf_simd::JsonTokenCursor<'_> {
    #[inline]
    fn peek(&mut self) -> Option<u32> {
        inf_simd::JsonTokenCursor::peek(self)
    }
    #[inline]
    fn bump(&mut self) {
        inf_simd::JsonTokenCursor::bump(self)
    }
}

/// Batch-index adapter: feeds a pre-materialized structural index through
/// the [`TokenSource`] interface.
struct IndexTokens<'a> {
    indices: &'a [u32],
    cursor: usize,
}

impl TokenSource for IndexTokens<'_> {
    #[inline]
    fn peek(&mut self) -> Option<u32> {
        self.indices.get(self.cursor).copied()
    }
    #[inline]
    fn bump(&mut self) {
        self.cursor += 1;
    }
}

impl Default for JsonParser {
    fn default() -> JsonParser {
        JsonParser::new()
    }
}

impl JsonParser {
    /// Format-ceiling limits (depth 128, 16 MiB − 1 both axes).
    pub fn new() -> JsonParser {
        JsonParser::with_limits(ParseLimits::default())
    }

    /// Namespace-configured limits (M3-S07), clamped to the format
    /// ceilings — a config value can only lower a bound.
    pub fn with_limits(limits: ParseLimits) -> JsonParser {
        let limits = ParseLimits {
            max_depth: limits.max_depth.min(DEPTH_MAX),
            max_text: limits.max_text,
            max_body: limits.max_body.min(DOC_BYTES_MAX),
        };
        JsonParser {
            blocks: Vec::new(),
            indices: Vec::new(),
            frames: Vec::new(),
            unescape: Vec::new(),
            rebuild: Vec::new(),
            obj_frames: Vec::new(),
            limits,
        }
    }

    /// Re-point a recycled parser at another namespace's resolved limits
    /// (M3-S11: one per-cell parser serves every store; two stores and a
    /// clamp per command beat rebuilding the scratch). Same ceiling
    /// clamps as [`with_limits`](Self::with_limits).
    pub fn set_limits(&mut self, limits: ParseLimits) {
        self.limits = ParseLimits {
            max_depth: limits.max_depth.min(DEPTH_MAX),
            max_text: limits.max_text,
            max_body: limits.max_body.min(DOC_BYTES_MAX),
        };
    }

    /// Bytes of retained scratch (block masks, structural index, frame
    /// stacks, string and splice buffers). The parser is per-cell state
    /// and its memory belongs to the document domain (L5) — S19 wires
    /// this into attribution; the S07 pathological suite asserts
    /// rejections keep it bounded by the text cap, never by the would-be
    /// document.
    pub fn scratch_bytes(&self) -> usize {
        self.blocks.capacity() * size_of::<inf_simd::BlockMasks>()
            + self.indices.capacity() * size_of::<u32>()
            + self.frames.capacity() * size_of::<u32>()
            + self.unescape.capacity()
            + self.rebuild.capacity()
            + self.obj_frames.capacity() * size_of::<ObjFrame>()
            + self
                .obj_frames
                .iter()
                .map(|f| f.entries.capacity() * size_of::<ObjEntry>())
                .sum::<usize>()
    }

    /// Parse JSON text into canonical idoc bytes (header included) —
    /// exactly what `TapeDoc::from_bytes` accepts and `json_set` stores.
    ///
    /// Allocates a fresh output; the ingest hot path uses [`parse_into`]
    /// to recycle one buffer across parses.
    ///
    /// [`parse_into`]: JsonParser::parse_into
    pub fn parse(&mut self, input: &[u8]) -> Result<Vec<u8>, JsonParseError> {
        let mut out = Vec::new();
        self.parse_into(input, &mut out)?;
        Ok(out)
    }

    /// Parse JSON text into `out` (cleared first) — the S03/S11 ingest
    /// seam: the caller keeps one buffer per cell, so the hot path
    /// allocates nothing once the buffer has grown to workload size.
    /// After a `DocumentTooLarge` rejection, `out.capacity()` is the
    /// refused parse's held memory (bounded by the cap plus one token
    /// plus `Vec` growth slack — asserted by the S07 pathological suite).
    ///
    /// The whole input is UTF-8-validated **once** up front (the simdjson
    /// hoisting: any substring of valid UTF-8 bounded by ASCII quotes is
    /// itself valid UTF-8, so per-string re-validation vanishes — profiled
    /// at 13% of the gate row). Consequence: invalid UTF-8 anywhere
    /// reports `InvalidUtf8` before any grammar error.
    pub fn parse_into(&mut self, input: &[u8], out: &mut Vec<u8>) -> Result<(), JsonParseError> {
        // Text bound first (M3-S07 reject-before-allocate): nothing below
        // — not UTF-8 validation, not the block classification, not the
        // output reserve — runs on an over-cap input.
        if input.len() > self.limits.max_text {
            return err(0, JsonErrorKind::DocumentTooLarge);
        }
        validate_input(input)?;
        inf_simd::json_classify_blocks(input, &mut self.blocks);
        // Move the scratch out so `self`'s other buffers stay borrowable.
        let blocks = core::mem::take(&mut self.blocks);
        let mut frames = core::mem::take(&mut self.frames);
        let mut tokens = inf_simd::JsonTokenCursor::new(&blocks);
        let result = self.parse_tokens(input, &mut tokens, &mut frames, out);
        self.blocks = blocks;
        self.frames = frames;
        result
    }

    /// Full parse over the scalar stage-1 tier (the portability fallback)
    /// — the off arm of the L4 SIMD A/B, fed through the batch-index
    /// [`TokenSource`] arm. Bench-only; identical semantics.
    #[doc(hidden)]
    pub fn parse_scalar_stage1(&mut self, input: &[u8]) -> Result<Vec<u8>, JsonParseError> {
        if input.len() > self.limits.max_text {
            return err(0, JsonErrorKind::DocumentTooLarge);
        }
        validate_input(input)?;
        let n = inf_simd::scalar_json_scan_structurals(input, &mut self.indices);
        let indices = core::mem::take(&mut self.indices);
        let mut frames = core::mem::take(&mut self.frames);
        let mut out = Vec::new();
        let mut tokens = IndexTokens { indices: &indices[..n], cursor: 0 };
        let result = self.parse_tokens(input, &mut tokens, &mut frames, &mut out);
        self.indices = indices;
        self.frames = frames;
        result.map(|()| out)
    }

    /// `input` is whole-input UTF-8-validated (`validate_input` at both
    /// callers) — string content slices are valid by construction.
    fn parse_tokens<T: TokenSource>(
        &mut self,
        input: &[u8],
        tokens: &mut T,
        frames: &mut Vec<u32>,
        out: &mut Vec<u8>,
    ) -> Result<(), JsonParseError> {
        let max_depth = self.limits.max_depth;
        let capacity = input.len().min(self.limits.max_body).saturating_add(16);
        out.clear();
        out.reserve(capacity);
        let mut tape = Tape { out, max_body: self.limits.max_body };
        tape.out.resize(header::HEADER_LEN, 0);
        frames.clear();
        let mut live_obj_frames = 0usize;
        let mut expect = Expect::Value;

        // The closing quote of the string opening at `$open`. By stage-1
        // mask arithmetic the token after an open quote is ALWAYS an
        // unescaped quote or nothing: the open flips `in_string`, which
        // masks every op/scalar bit until the next unescaped quote — the
        // one byte class whose bits always emit. (Grammar quote parity
        // and mask parity cannot diverge: every quote token the grammar
        // consumes is consumed as an open/close pair right here.) So
        // `None` is exactly "unterminated"; the byte re-check the batch
        // parser did is a debug assertion now — proven by the same
        // equivalence proptests, exercised per-parse by the differential
        // and fuzz suites. Consumes the open quote; the close quote stays
        // peeked for the post-emit bump.
        macro_rules! string_close {
            ($open:expr) => {{
                tokens.bump();
                match tokens.peek() {
                    Some(close) => {
                        debug_assert_eq!(
                            input[close as usize], b'"',
                            "token after an open quote is its close quote"
                        );
                        close as usize
                    }
                    None => return err($open, JsonErrorKind::UnterminatedString),
                }
            }};
        }

        // Dispatch one value-start token: containers push a frame and
        // re-enter the grammar loop; scalars emit and fall through to the
        // code after the macro (the fused entry/element loops, or the
        // after-value cascade). Every arm consumes the token(s) it parsed.
        macro_rules! begin_value {
            ($at:expr, $c:expr, $grammar:lifetime) => {{
                match $c {
                    b'{' => {
                        if frames.len() == max_depth {
                            return err($at, JsonErrorKind::DepthExceeded);
                        }
                        if !tape.fits(emit::CONTAINER_OPEN_LEN) {
                            return err($at, JsonErrorKind::DocumentTooLarge);
                        }
                        let len_at = emit::begin(tape.out, TAG_OBJ) as u32;
                        frames.push(len_at | OBJ_BIT);
                        self.open_obj_frame(&mut live_obj_frames, tape.out.len());
                        expect = Expect::KeyOrObjClose;
                        tokens.bump();
                        continue $grammar;
                    }
                    b'[' => {
                        if frames.len() == max_depth {
                            return err($at, JsonErrorKind::DepthExceeded);
                        }
                        if !tape.fits(emit::CONTAINER_OPEN_LEN) {
                            return err($at, JsonErrorKind::DocumentTooLarge);
                        }
                        let len_at = emit::begin(tape.out, TAG_ARR) as u32;
                        frames.push(len_at);
                        expect = Expect::ValueOrArrClose;
                        tokens.bump();
                        continue $grammar;
                    }
                    b'"' => {
                        let close = string_close!($at);
                        self.emit_string(&mut tape, input, $at, close)?;
                        tokens.bump();
                    }
                    b't' | b'f' | b'n' => {
                        parse_literal(input, $at, &mut tape)?;
                        tokens.bump();
                    }
                    b'-' | b'0'..=b'9' => {
                        parse_number(input, $at, &mut tape)?;
                        tokens.bump();
                    }
                    other => return err($at, JsonErrorKind::UnexpectedCharacter(other)),
                }
            }};
        }

        // Fetch the next structural token into (`$at`, `$c`) without
        // consuming it, or fail with the end-of-input error.
        macro_rules! fetch {
            ($at:ident, $c:ident) => {{
                let Some(token) = tokens.peek() else {
                    return err(input.len(), JsonErrorKind::UnexpectedEnd);
                };
                $at = token as usize;
                $c = input[$at];
            }};
        }

        'grammar: loop {
            let mut at: usize;
            let mut c: u8;
            fetch!(at, c);
            match expect {
                Expect::Value => begin_value!(at, c, 'grammar),
                Expect::ValueOrArrClose => {
                    if c == b']' {
                        // Empty array.
                        let frame = frames.pop().expect("ValueOrArrClose implies an open array");
                        emit::end(tape.out, (frame & LEN_AT_MASK) as usize);
                        tokens.bump();
                    } else {
                        // Fused element loop: `scalar, scalar, …` runs cost
                        // no dispatch round trip — the container kind is
                        // static here, so the separator check needs no
                        // frame load. Container elements exit to the
                        // grammar loop; the close falls to the cascade.
                        loop {
                            begin_value!(at, c, 'grammar);
                            fetch!(at, c);
                            if c == b',' {
                                tokens.bump();
                                fetch!(at, c);
                                continue;
                            }
                            if c == b']' {
                                let frame = frames.pop().expect("element loop owns an open array");
                                emit::end(tape.out, (frame & LEN_AT_MASK) as usize);
                                tokens.bump();
                                break;
                            }
                            return err(at, JsonErrorKind::UnexpectedCharacter(c));
                        }
                    }
                }
                Expect::KeyOrObjClose | Expect::Key => {
                    if c == b'}' && expect == Expect::KeyOrObjClose {
                        // Empty object.
                        let frame = frames.pop().expect("KeyOrObjClose implies an open object");
                        self.close_obj_frame(&mut live_obj_frames, &mut tape);
                        emit::end(tape.out, (frame & LEN_AT_MASK) as usize);
                        tokens.bump();
                    } else {
                        // Fused entry loop: `key : value ,` in one pass —
                        // separators and the next key cost no dispatch
                        // round trip. Container values exit to the grammar
                        // loop; the object close falls to the cascade.
                        loop {
                            if c != b'"' {
                                return err(at, JsonErrorKind::UnexpectedCharacter(c));
                            }
                            let close = string_close!(at);
                            let entry_at = tape.out.len();
                            let key_len = self.emit_string(&mut tape, input, at, close)?;
                            self.note_key(live_obj_frames, tape.out, entry_at, key_len);
                            tokens.bump();
                            fetch!(at, c);
                            if c != b':' {
                                return err(at, JsonErrorKind::UnexpectedCharacter(c));
                            }
                            tokens.bump();
                            fetch!(at, c);
                            begin_value!(at, c, 'grammar);
                            fetch!(at, c);
                            if c == b',' {
                                tokens.bump();
                                fetch!(at, c);
                                continue;
                            }
                            if c == b'}' {
                                let frame = frames.pop().expect("entry loop owns an open object");
                                self.close_obj_frame(&mut live_obj_frames, &mut tape);
                                emit::end(tape.out, (frame & LEN_AT_MASK) as usize);
                                tokens.bump();
                                break;
                            }
                            return err(at, JsonErrorKind::UnexpectedCharacter(c));
                        }
                    }
                }
            }
            // After-value cascade: close every container ending here, then
            // consume exactly one separator — closers cost no dispatch
            // round trip (`]]}` is three iterations of this inner loop).
            loop {
                let Some(&frame) = frames.last() else {
                    // Root complete.
                    if let Some(trailing) = tokens.peek() {
                        return err(trailing as usize, JsonErrorKind::TrailingCharacters);
                    }
                    debug_assert_eq!(live_obj_frames, 0);
                    let body_len = (tape.out.len() - header::HEADER_LEN) as u32;
                    header::patch(tape.out, 0, body_len);
                    return Ok(());
                };
                let Some(token) = tokens.peek() else {
                    return err(input.len(), JsonErrorKind::UnexpectedEnd);
                };
                let at = token as usize;
                let c = input[at];
                let is_obj = frame & OBJ_BIT != 0;
                if c == b',' {
                    tokens.bump();
                    expect = if is_obj { Expect::Key } else { Expect::Value };
                    break;
                }
                if is_obj && c == b'}' {
                    frames.pop();
                    // Splice before backpatch: a duplicate-key rebuild can
                    // shrink the body the u24 must describe.
                    self.close_obj_frame(&mut live_obj_frames, &mut tape);
                    emit::end(tape.out, (frame & LEN_AT_MASK) as usize);
                    tokens.bump();
                    continue;
                }
                if !is_obj && c == b']' {
                    frames.pop();
                    emit::end(tape.out, (frame & LEN_AT_MASK) as usize);
                    tokens.bump();
                    continue;
                }
                return err(at, JsonErrorKind::UnexpectedCharacter(c));
            }
        }
    }

    /// Decode the string opening at `at` (content up to `close`) and emit
    /// it onto the tape; returns the decoded byte length (key spans).
    #[inline]
    fn emit_string(
        &mut self,
        tape: &mut Tape<'_>,
        input: &[u8],
        at: usize,
        close: usize,
    ) -> Result<usize, JsonParseError> {
        // Fixstr fast path: short strings dominate keys and small values,
        // so scan and copy fuse into one word pass (the scan's loads feed
        // the stores). Any special byte, missing word slack, or cap miss
        // falls through — the general path owns escapes and typed errors.
        let len = close - (at + 1);
        if len <= FIXSTR_MAX_LEN && try_fixstr_fast(tape, input, at, len) {
            return Ok(len);
        }
        // Outlined: the grammar loop inlines `emit_string` at two sites,
        // and growing it past the fixstr try regressed the string-light
        // shapes (deep/wide) through sheer code size — the general path
        // costs a call it amortizes over ≥ 32-byte payloads.
        self.emit_string_general(tape, input, at, close, len)
    }

    #[inline(never)]
    fn emit_string_general(
        &mut self,
        tape: &mut Tape<'_>,
        input: &[u8],
        at: usize,
        close: usize,
        len: usize,
    ) -> Result<usize, JsonParseError> {
        let content = &input[at + 1..close];
        // Fused general path (ADR-0047 K1): one `inf-simd` pass scans for
        // specials while copying, replacing the separate `find_special` +
        // `append_from_input` passes on escape-free content. A special
        // (escape → walk, control → typed error) or a raw-length cap edge
        // falls through to the decode path below, which owns every error —
        // accept/reject behavior stays byte-identical (an escaped string
        // whose raw length busts the cap may still fit unescaped, so the
        // cap gate here only *enters* the fast path, never rejects).
        if tape.fits(emit::str_header_len(len) + len) {
            let header_at = tape.out.len();
            emit::str_header(tape.out, len);
            if inf_simd::json_copy_unescaped(content, tape.out).is_none() {
                return Ok(len);
            }
            tape.out.truncate(header_at);
        }
        match decode_string(content, at + 1, &mut self.unescape)? {
            Some(s) => {
                // Escape-free but past the fast path (cap edge): the
                // content is a slice of the input, so the payload copies
                // as overlapped words riding the input's own slack.
                let len = s.len();
                if !tape.fits(emit::str_header_len(len) + len) {
                    return err(at, JsonErrorKind::DocumentTooLarge);
                }
                emit::str_header(tape.out, len);
                emit::append_overlapped(tape.out, input, at + 1, len);
                Ok(len)
            }
            None => {
                let len = self.unescape.len();
                if !tape.fits(emit::str_header_len(len) + len) {
                    return err(at, JsonErrorKind::DocumentTooLarge);
                }
                emit::str(tape.out, &self.unescape);
                Ok(len)
            }
        }
    }

    fn open_obj_frame(&mut self, live: &mut usize, body_start: usize) {
        if *live == self.obj_frames.len() {
            self.obj_frames.push(ObjFrame::default());
        }
        self.obj_frames[*live].reset(body_start as u32);
        *live += 1;
    }

    /// Record an emitted key; small objects detect duplicates on insert
    /// (fingerprint filter, then memcmp on the recorded spans), large
    /// ones defer to the close-time sort.
    #[inline]
    fn note_key(&mut self, live: usize, out: &[u8], entry_at: usize, key_len: usize) {
        // The key bytes sit right after their canonical string header.
        let key_at = (entry_at + emit::str_header_len(key_len)) as u32;
        let key_len16 = key_len.min(u16::MAX as usize) as u16;
        let frame = &mut self.obj_frames[live - 1];
        if !frame.dup_found
            && frame.entries.len() <= LINEAR_SCAN_MAX
            && key_len <= u16::MAX as usize
        {
            let key = &out[key_at as usize..key_at as usize + key_len];
            // Fingerprint filter first: a clear bit proves no prior entry
            // has this key, so the scan is skipped outright (equal keys
            // always collide in the filter; see `key_fingerprint`).
            let bit = key_fingerprint(key);
            let possible_dup = frame.fp & bit != 0;
            frame.fp |= bit;
            // First-byte prefilter ahead of the memcmp call: distinct keys
            // usually differ immediately, and the byte is a load the scan
            // already owns — unlike the cached-key-word variant this slice
            // rejected, there is no per-key build cost to amortize.
            frame.dup_found = possible_dup
                && frame.entries.iter().any(|e| {
                    if e.key_len != key_len16 {
                        return false;
                    }
                    if key_len16 == 0 {
                        return true;
                    }
                    let ka = e.key_at as usize;
                    out[ka] == key[0] && &out[ka..ka + key_len] == key
                });
        } else if key_len > u16::MAX as usize {
            // Keys longer than 64 KiB fall back to close-time detection.
            frame.dup_found = true;
        }
        frame.entries.push(ObjEntry { entry_at: entry_at as u32, key_at, key_len: key_len16 });
    }

    /// Close the innermost object: if duplicates exist (or the object was
    /// too large for insert-time detection), rebuild the body with
    /// last-occurrence-wins / first-position-kept semantics and splice it
    /// over the original (ADR-0036 D5). Cold path: it runs only for
    /// objects that contained duplicates or exceeded the linear-scan cap.
    fn close_obj_frame(&mut self, live: &mut usize, tape: &mut Tape<'_>) {
        *live -= 1;
        let frame = &mut self.obj_frames[*live];
        let must_scan = frame.entries.len() > LINEAR_SCAN_MAX;
        if !frame.dup_found && !must_scan {
            return;
        }
        let body_start = frame.body_start as usize;
        let body_end = tape.out.len();
        let entries = &frame.entries;
        // Sort entry ids by key bytes (O(k log k) memcmp compares), then
        // group equal runs: first occurrence keeps the position, last
        // occurrence supplies the entry bytes.
        let key_of = |e: &ObjEntry| -> &[u8] {
            if e.key_len == u16::MAX {
                // Possibly-truncated span: decode the full key from its tag.
                key_bytes_at(tape.out, e.entry_at as usize)
            } else {
                &tape.out[e.key_at as usize..(e.key_at + u32::from(e.key_len)) as usize]
            }
        };
        let mut by_key: Vec<u32> = (0..entries.len() as u32).collect();
        by_key.sort_unstable_by(|&a, &b_idx| {
            key_of(&entries[a as usize]).cmp(key_of(&entries[b_idx as usize]))
        });
        // replace_with[idx]: idx (emit own span), the last dup's idx (emit
        // that span at this position), or SKIP.
        const SKIP: u32 = u32::MAX;
        let mut replace_with: Vec<u32> = (0..entries.len() as u32).collect();
        let mut any_dup = false;
        let mut run = 0usize;
        while run < by_key.len() {
            let key = key_of(&entries[by_key[run] as usize]);
            let mut end = run + 1;
            while end < by_key.len() && key_of(&entries[by_key[end] as usize]) == key {
                end += 1;
            }
            if end - run > 1 {
                any_dup = true;
                let first = by_key[run..end].iter().copied().min().expect("non-empty run");
                let last = by_key[run..end].iter().copied().max().expect("non-empty run");
                for &idx in &by_key[run..end] {
                    replace_with[idx as usize] = if idx == first { last } else { SKIP };
                }
            }
            run = end;
        }
        if !any_dup {
            return;
        }
        let entry_end = |idx: usize| -> usize {
            entries.get(idx + 1).map_or(body_end, |e| e.entry_at as usize)
        };
        self.rebuild.clear();
        for &target in replace_with.iter() {
            if target == SKIP {
                continue;
            }
            let target = target as usize;
            let span = entries[target].entry_at as usize..entry_end(target);
            self.rebuild.extend_from_slice(&tape.out[span]);
        }
        // Splice the rebuilt body over the original. Every open
        // placeholder (this object's and its ancestors') precedes
        // `body_start`, so no u24 moves — the D3 backpatch argument.
        debug_assert!(body_start <= body_end && body_end == tape.out.len());
        tape.out.truncate(body_start);
        tape.out.extend_from_slice(&self.rebuild);
    }
}

/// Decode the key bytes of the entry starting at `at` in the output tape
/// (the parser wrote it, so the encoding is canonical fixstr/str8/str24).
fn key_bytes_at(out: &[u8], at: usize) -> &[u8] {
    let tag = out[at];
    if (0x80..=0x9F).contains(&tag) {
        let len = (tag - 0x80) as usize;
        &out[at + 1..at + 1 + len]
    } else if tag == 0xA5 {
        let len = out[at + 1] as usize;
        &out[at + 2..at + 2 + len]
    } else {
        debug_assert_eq!(tag, 0xA6, "parser keys are canonical string forms");
        let len = out[at + 1] as usize | (out[at + 2] as usize) << 8 | (out[at + 3] as usize) << 16;
        &out[at + 4..at + 4 + len]
    }
}

/// `true` / `false` / `null`, with a hard terminator check (`truex` is a
/// grammar error even though stage 1 emits one token for it).
fn parse_literal(input: &[u8], at: usize, tape: &mut Tape<'_>) -> Result<(), JsonParseError> {
    let (text, len): (&[u8], usize) = match input[at] {
        b't' => (b"true", 4),
        b'f' => (b"false", 5),
        _ => (b"null", 4),
    };
    if input.len() < at + len || &input[at..at + len] != text {
        return err(at, JsonErrorKind::UnexpectedCharacter(input[at]));
    }
    check_scalar_terminator(input, at + len)?;
    if !tape.fits(1) {
        return err(at, JsonErrorKind::DocumentTooLarge);
    }
    match input[at] {
        b't' => emit::bool(tape.out, true),
        b'f' => emit::bool(tape.out, false),
        _ => emit::null(tape.out),
    }
    Ok(())
}

/// The byte after a scalar must end the token (ws / structural / quote /
/// EOF) — `123abc` and `truex` are single stage-1 tokens and must reject.
fn check_scalar_terminator(input: &[u8], end: usize) -> Result<(), JsonParseError> {
    match input.get(end) {
        None
        | Some(b' ' | b'\t' | b'\n' | b'\r' | b'{' | b'}' | b'[' | b']' | b':' | b',' | b'"') => {
            Ok(())
        }
        Some(&b) => err(end, JsonErrorKind::UnexpectedCharacter(b)),
    }
}

/// Digits-only slice → u64 via 8-digit SWAR chunks (caller guarantees
/// ASCII digits and length ≤ 19, so no overflow is possible).
fn parse_digits(bytes: &[u8]) -> u64 {
    debug_assert!(bytes.len() <= 19 && bytes.iter().all(u8::is_ascii_digit));
    let mut acc: u64 = 0;
    let mut chunks = bytes.chunks_exact(8);
    for chunk in &mut chunks {
        let raw =
            u64::from_le_bytes(chunk.try_into().expect("8-byte chunk")) & 0x0F0F_0F0F_0F0F_0F0F;
        // Classic SWAR pairwise combine: 8 digits → one u64 in 3 mul-adds.
        let pairs = (raw.wrapping_mul(10) + (raw >> 8)) & 0x00FF_00FF_00FF_00FF;
        let quads = (pairs.wrapping_mul(100) + (pairs >> 16)) & 0x0000_FFFF_0000_FFFF;
        let octet = quads.wrapping_mul(10_000) + (quads >> 32);
        acc = acc * 100_000_000 + (octet & 0xFFFF_FFFF);
    }
    for &d in chunks.remainder() {
        acc = acc * 10 + u64::from(d - b'0');
    }
    acc
}

/// Exact powers of ten as f64 (10²² is the largest exact one) — the
/// Clinger fast-path multipliers.
const F64_POW10: [f64; 23] = [
    1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10, 1e11, 1e12, 1e13, 1e14, 1e15, 1e16,
    1e17, 1e18, 1e19, 1e20, 1e21, 1e22,
];

/// Powers of ten as u64 (mantissa recombination: `int × 10^frac_len`).
const U64_POW10: [u64; 20] = [
    1,
    10,
    100,
    1_000,
    10_000,
    100_000,
    1_000_000,
    10_000_000,
    100_000_000,
    1_000_000_000,
    10_000_000_000,
    100_000_000_000,
    1_000_000_000_000,
    10_000_000_000_000,
    100_000_000_000_000,
    1_000_000_000_000_000,
    10_000_000_000_000_000,
    100_000_000_000_000_000,
    1_000_000_000_000_000_000,
    10_000_000_000_000_000_000,
];

/// Leading ASCII-digit count of one LE word (0–8). The non-digit test
/// flags each byte's high bit; a byte-sum carry can only false-flag a
/// byte *after* a genuine non-digit, and `trailing_zeros` reports the
/// first flag, so the leading count is exact.
#[inline]
fn digit_run_len(w: u64) -> usize {
    const LO: u64 = 0x0101_0101_0101_0101;
    let v = w ^ (LO * 0x30);
    let nondigit = (v.wrapping_add(LO * 0x76) | v) & (LO * 0x80);
    if nondigit == 0 { 8 } else { (nondigit.trailing_zeros() / 8) as usize }
}

/// End of the ASCII-digit run starting at `i` — word-at-a-time (numbers
/// are scanned twice nowhere: this is the only classification pass).
#[inline]
fn digit_run_end(input: &[u8], mut i: usize) -> usize {
    while i + 8 <= input.len() {
        let w = u64::from_le_bytes(input[i..i + 8].try_into().expect("8-byte chunk"));
        let n = digit_run_len(w);
        i += n;
        if n < 8 {
            return i;
        }
    }
    while i < input.len() && input[i].is_ascii_digit() {
        i += 1;
    }
    i
}

/// Strict JSON number → canonical i64/f64 emission (module-doc rules).
fn parse_number(input: &[u8], at: usize, tape: &mut Tape<'_>) -> Result<(), JsonParseError> {
    match parse_number_value(input, at)?.0 {
        Number::I64(value) => emit_i64_checked(tape, value, at),
        Number::F64(value) => emit_f64_checked(tape, value, at),
    }
}

/// Parse a standalone JSON number token through the ingest parser's one
/// grammar, without constructing an idoc. Leading/trailing JSON whitespace
/// is accepted exactly as it is for a scalar document.
pub fn parse_number_token(input: &[u8]) -> Result<Number, JsonParseError> {
    let start = input
        .iter()
        .position(|b| !matches!(b, b' ' | b'\t' | b'\n' | b'\r'))
        .unwrap_or(input.len());
    let end = input
        .iter()
        .rposition(|b| !matches!(b, b' ' | b'\t' | b'\n' | b'\r'))
        .map_or(start, |at| at + 1);
    if start == end {
        return err(start, JsonErrorKind::InvalidNumber);
    }
    let token = &input[start..end];
    let (number, used) = parse_number_value(token, 0)
        .map_err(|error| JsonParseError { offset: error.offset + start, kind: error.kind })?;
    if used != token.len() {
        return err(start + used, JsonErrorKind::UnexpectedCharacter(token[used]));
    }
    Ok(number)
}

fn parse_number_value(input: &[u8], at: usize) -> Result<(Number, usize), JsonParseError> {
    let mut i = at;
    let neg = input[i] == b'-';
    if neg {
        i += 1;
    }
    let int_start = i;
    i = digit_run_end(input, i);
    let int_digits = &input[int_start..i];
    if int_digits.is_empty() {
        return err(at, JsonErrorKind::InvalidNumber); // lone '-' or '.5' style
    }
    if int_digits[0] == b'0' && int_digits.len() > 1 {
        return err(at, JsonErrorKind::InvalidNumber); // leading zero
    }
    let mut frac: &[u8] = &[];
    let mut is_float = false;
    if input.get(i) == Some(&b'.') {
        is_float = true;
        i += 1;
        let frac_start = i;
        i = digit_run_end(input, i);
        if i == frac_start {
            return err(at, JsonErrorKind::InvalidNumber); // "1."
        }
        frac = &input[frac_start..i];
    }
    // Exponent value, saturated far past the f64 range — the fast path
    // only reads |exp| ≤ 22, and the std fallback re-parses the text.
    let mut exp10: i32 = 0;
    if matches!(input.get(i), Some(&b'e') | Some(&b'E')) {
        is_float = true;
        i += 1;
        let exp_neg = match input.get(i) {
            Some(&b'-') => {
                i += 1;
                true
            }
            Some(&b'+') => {
                i += 1;
                false
            }
            _ => false,
        };
        let exp_start = i;
        i = digit_run_end(input, i);
        if i == exp_start {
            return err(at, JsonErrorKind::InvalidNumber); // "1e"
        }
        let mut v: i32 = 0;
        for &d in &input[exp_start..i.min(exp_start + 9)] {
            v = v * 10 + i32::from(d - b'0');
        }
        if i - exp_start > 9 {
            v = 100_000; // saturate: the fallback decides overflow/underflow
        }
        exp10 = if exp_neg { -v } else { v };
    }
    check_scalar_terminator(input, i)?;
    if !is_float {
        // Integral: i64 when it fits; `-0` keeps its sign as f64;
        // otherwise the ADR-0036 D4 f64 fallback (S21 measures the
        // oracle's u64-range behavior — candidate deviation).
        if int_digits.len() <= 19 {
            let magnitude = parse_digits(int_digits);
            if neg && magnitude == 0 {
                return Ok((Number::F64(-0.0), i));
            }
            if neg && magnitude <= (i64::MAX as u64) + 1 {
                return Ok((Number::I64((magnitude as i64).wrapping_neg()), i));
            }
            if !neg && magnitude <= i64::MAX as u64 {
                return Ok((Number::I64(magnitude as i64), i));
            }
        }
    } else {
        // Clinger fast path: a mantissa exact in f64 (≤ 2⁵³) scaled by an
        // exact power of ten (|10^e| ≤ 10²²) rounds exactly once — bit-
        // identical to the Eisel–Lemire fallback, minus its full re-parse.
        let total = int_digits.len() + frac.len();
        if total <= 19 {
            let m = parse_digits(int_digits) * U64_POW10[frac.len()] + parse_digits(frac);
            let e = exp10.saturating_sub(frac.len() as i32);
            if m <= (1u64 << 53) && (-22..=22).contains(&e) {
                let scaled = if e < 0 {
                    m as f64 / F64_POW10[(-e) as usize]
                } else {
                    m as f64 * F64_POW10[e as usize]
                };
                return Ok((Number::F64(if neg { -scaled } else { scaled }), i));
            }
        }
    }
    // Fallback (float outside the fast bounds, or integral overflow):
    // std's Eisel–Lemire parse is round-trip-correct; the slice is
    // validated ASCII.
    let text = core::str::from_utf8(&input[at..i]).expect("number slices are ASCII");
    let value: f64 = text
        .parse()
        .map_err(|_| JsonParseError { offset: at, kind: JsonErrorKind::InvalidNumber })?;
    if !value.is_finite() {
        return err(at, JsonErrorKind::NumberOutOfRange);
    }
    Ok((Number::F64(value), i))
}

/// Cap-checked i64 emission (fixints cost 1, the rest the varint worst
/// case — the same accounting the checked builder applies).
#[inline]
fn emit_i64_checked(tape: &mut Tape<'_>, v: i64, at: usize) -> Result<(), JsonParseError> {
    let worst = if (FIXINT_MIN..=FIXINT_MAX).contains(&v) { 1 } else { emit::I64_MAX_LEN };
    if !tape.fits(worst) {
        return err(at, JsonErrorKind::DocumentTooLarge);
    }
    emit::i64(tape.out, v);
    Ok(())
}

/// Cap-checked f64 emission; `v` is finite (callers typed the refusal).
#[inline]
fn emit_f64_checked(tape: &mut Tape<'_>, v: f64, at: usize) -> Result<(), JsonParseError> {
    if !tape.fits(emit::F64_LEN) {
        return err(at, JsonErrorKind::DocumentTooLarge);
    }
    emit::f64(tape.out, v);
    Ok(())
}

/// Fused scan+copy for fixstr-width (≤ 31-byte) strings: one 32-byte
/// AVX2 load/classify/store (ADR-0047 K2 — the kernel call plus its
/// dispatcher beat every inlined-SWAR variant, including an ADR-0049
/// `emit::fixstr_swar` attempt that lost −5% on the gate shape:
/// Rejected and removed, rows in the stage-fusion artifact). Needs 32
/// readable bytes
/// from the content start (the closing quote guarantees one; strings
/// inside the last 31 bytes of the document fall to the general path —
/// correct, colder). Returns `false` without emitting when the fast path
/// cannot decide (special byte → escape walk or typed error; no window
/// slack; cap exceeded) — the general path is the single source of
/// errors, so rejection behavior stays byte-identical.
#[inline]
fn try_fixstr_fast(tape: &mut Tape<'_>, input: &[u8], at: usize, len: usize) -> bool {
    debug_assert!(len <= FIXSTR_MAX_LEN);
    if !tape.fits(1 + len) {
        return false;
    }
    let s = at + 1;
    if len == 0 {
        tape.out.push(FIXSTR_BASE);
        return true;
    }
    if s + 32 > input.len() {
        return false;
    }
    inf_simd::json_copy_unescaped_fixstr(&input[s..s + 32], len, FIXSTR_BASE + len as u8, tape.out)
}

/// Whole-input validation: UTF-8 once, through the `inf-simd` kernel
/// (M3-S05 slice 3 — replaces std's word-at-a-time pass). The reject
/// path re-runs std's validator for the exact error offset and defers to
/// its verdict, so a hypothetical kernel false-negative costs one wasted
/// pass, never a wrong answer; the false-accept direction is proptested
/// in `inf-simd` and cross-checked continuously by the serde_json fuzz
/// differential (which rejects invalid UTF-8 itself).
fn validate_input(input: &[u8]) -> Result<(), JsonParseError> {
    if inf_simd::utf8_is_valid(input) {
        return Ok(());
    }
    match core::str::from_utf8(input) {
        Err(e) => err(e.valid_up_to(), JsonErrorKind::InvalidUtf8),
        Ok(_) => {
            debug_assert!(false, "utf8 kernel false-negative (std accepts)");
            Ok(())
        }
    }
}

/// SWAR special-byte detector for one LE word: high bit set per byte that
/// is a backslash or a raw control (< 0x20).
#[inline]
fn word_hit(w: u64) -> u64 {
    const LO: u64 = 0x0101_0101_0101_0101;
    const HI: u64 = 0x8080_8080_8080_8080;
    // Unsigned byte < 0x20 (classic SWAR range check).
    let control = w.wrapping_sub(LO * 0x20) & !w & HI;
    // Byte == 0x5C via zero-byte detection on w ^ 0x5C…5C.
    let x = w ^ (LO * 0x5C);
    let backslash = x.wrapping_sub(LO) & !x & HI;
    control | backslash
}

/// First byte that is a backslash or a raw control (< 0x20) — the
/// per-string hot scan. Word-at-a-time; strings of ≥ 8 bytes finish
/// with an overlapped word over the last 8 (the prefix it re-covers
/// already scanned clean, so any hit is genuinely new); shorter ones
/// take the predicted byte loop (a masked stack word lost its A/B —
/// the variable-length copy outweighed ≤ 7 predicted iterations).
#[inline]
fn find_special(bytes: &[u8]) -> Option<usize> {
    let len = bytes.len();
    if len < 8 {
        let mut i = 0;
        while i < len {
            if bytes[i] < 0x20 || bytes[i] == b'\\' {
                return Some(i);
            }
            i += 1;
        }
        return None;
    }
    let mut i = 0;
    while i + 8 <= len {
        let w = u64::from_le_bytes(bytes[i..i + 8].try_into().expect("8-byte chunk"));
        let hit = word_hit(w);
        if hit != 0 {
            return Some(i + (hit.trailing_zeros() / 8) as usize);
        }
        i += 8;
    }
    if i < len {
        let w = u64::from_le_bytes(bytes[len - 8..].try_into().expect("8-byte tail"));
        let hit = word_hit(w);
        if hit != 0 {
            return Some(len - 8 + (hit.trailing_zeros() / 8) as usize);
        }
    }
    None
}

/// Decode string content: `Ok(Some(s))` borrows the input (no escapes —
/// UTF-8 was validated input-wide), `Ok(None)` means the unescaped text
/// is in `scratch` (valid UTF-8 by construction: validated input runs
/// plus `char`-encoded escapes). `base` is the input offset of
/// `content[0]`.
fn decode_string<'a>(
    content: &'a [u8],
    base: usize,
    scratch: &mut Vec<u8>,
) -> Result<Option<&'a [u8]>, JsonParseError> {
    let bytes = content;
    let Some(first) = find_special(bytes) else {
        return Ok(Some(content));
    };
    if bytes[first] < 0x20 {
        return err(base + first, JsonErrorKind::ControlCharacter);
    }
    scratch.clear();
    scratch.reserve(content.len());
    // Safe prefix (bounded by an ASCII backslash), then the escape walk.
    scratch.extend_from_slice(&content[..first]);
    let mut i = first;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => {
                let Some(&esc) = bytes.get(i + 1) else {
                    return err(base + i, JsonErrorKind::InvalidEscape);
                };
                i += 2;
                match esc {
                    b'"' => scratch.push(b'"'),
                    b'\\' => scratch.push(b'\\'),
                    b'/' => scratch.push(b'/'),
                    b'b' => scratch.push(0x08),
                    b'f' => scratch.push(0x0C),
                    b'n' => scratch.push(b'\n'),
                    b'r' => scratch.push(b'\r'),
                    b't' => scratch.push(b'\t'),
                    b'u' => {
                        let at = base + i - 2;
                        let hi = parse_hex4(bytes, i).ok_or(JsonParseError {
                            offset: at,
                            kind: JsonErrorKind::InvalidUnicodeEscape,
                        })?;
                        i += 4;
                        let code = if (0xD800..=0xDBFF).contains(&hi) {
                            // High surrogate: a low one must follow.
                            if bytes.get(i) != Some(&b'\\') || bytes.get(i + 1) != Some(&b'u') {
                                return err(at, JsonErrorKind::LoneSurrogate);
                            }
                            let lo = parse_hex4(bytes, i + 2).ok_or(JsonParseError {
                                offset: base + i,
                                kind: JsonErrorKind::InvalidUnicodeEscape,
                            })?;
                            if !(0xDC00..=0xDFFF).contains(&lo) {
                                return err(at, JsonErrorKind::LoneSurrogate);
                            }
                            i += 6;
                            0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)
                        } else if (0xDC00..=0xDFFF).contains(&hi) {
                            return err(at, JsonErrorKind::LoneSurrogate);
                        } else {
                            hi
                        };
                        let ch = char::from_u32(code)
                            .expect("surrogates handled above; BMP/astral scalar remains");
                        let mut utf8 = [0u8; 4];
                        scratch.extend_from_slice(ch.encode_utf8(&mut utf8).as_bytes());
                    }
                    _ => return err(base + i - 2, JsonErrorKind::InvalidEscape),
                }
            }
            b if b < 0x20 => return err(base + i, JsonErrorKind::ControlCharacter),
            _ => {
                // Maximal raw run to the next escape/control byte; run
                // boundaries are ASCII, so &str slicing is boundary-safe.
                let run_end = find_special(&bytes[i..]).map_or(bytes.len(), |p| i + p);
                scratch.extend_from_slice(&content[i..run_end]);
                i = run_end;
            }
        }
    }
    Ok(None)
}

/// Four hex digits at `content[i..i+4]` → code unit.
fn parse_hex4(content: &[u8], i: usize) -> Option<u32> {
    if content.len() < i + 4 {
        return None;
    }
    let mut v = 0u32;
    for &b in &content[i..i + 4] {
        let d = (b as char).to_digit(16)?;
        v = v << 4 | d;
    }
    Some(v)
}
