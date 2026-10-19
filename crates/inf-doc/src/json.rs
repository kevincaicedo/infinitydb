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

use core::fmt;

use crate::apply::Number;
use crate::emit;
use crate::header;
use crate::limits::{DOC_BYTES_MAX, DocLimits};
use crate::tape::CanonicalDoc;
use crate::tape::{FIXINT_MAX, FIXINT_MIN, FIXSTR_BASE, FIXSTR_MAX_LEN, TAG_ARR, TAG_OBJ};

/// Objects up to this many entries detect duplicates by per-insert byte
/// compare (length prefilter via slice equality); larger objects defer to
/// one O(k log k) sort-based pass at close (bounded-everything: no
/// quadratic blowup on hostile wide objects). Hashing was profiled out:
/// per-key hash64 cost more than the compares it saved on real shapes.
const LINEAR_SCAN_MAX: usize = 256;

/// Entries a pooled object frame (and each dup-scan vector) keeps after a
/// parse: 65 536 × 12 B = 768 KiB per frame. Objects wider than this are
/// the pathological end of the S07 corpus; a steady state past it pays one
/// regrow per parse instead of pinning its peak (see `trim_scratch`).
const OBJ_ENTRIES_RETAIN_MAX: usize = 1 << 16;

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
    /// Nesting beyond the namespace's depth cap, at most
    /// [`DEPTH_MAX`](crate::limits::DEPTH_MAX).
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
    /// Header + body cap: the longest `out` a parse may produce.
    max_len: usize,
}

impl<'o> Tape<'o> {
    fn new(out: &'o mut Vec<u8>, max_body: usize) -> Tape<'o> {
        Tape { out, max_len: max_body.min(DOC_BYTES_MAX).saturating_add(header::HEADER_LEN) }
    }

    /// An `extra` whose sum is unrepresentable cannot fit: refused, never wrapped.
    #[inline]
    fn fits(&self, extra: usize) -> bool {
        self.out.len().checked_add(extra).is_some_and(|len| len <= self.max_len)
    }

    /// Check the frame word and byte budget before emitting its placeholder.
    /// Always inlined: as a call it cost the fused parse of nested shapes
    /// ~3% of its instructions (L4; measured in the ARCH-W0.3 ticket).
    #[inline(always)]
    fn open_container(&mut self, tag: u8, kind_bit: u32, at: usize) -> Result<u32, JsonParseError> {
        if !self.fits(emit::CONTAINER_OPEN_LEN) {
            return err(at, JsonErrorKind::DocumentTooLarge);
        }
        let Some(word) = self.out.len().checked_add(1).and_then(|len| frame_word(len, kind_bit))
        else {
            return err(at, JsonErrorKind::DocumentTooLarge);
        };
        emit::begin(self.out, tag);
        Ok(word)
    }

    /// The finished body's length as the header's `u32`; `None` past it.
    fn body_len(&self) -> Option<u32> {
        let body_len = self.out.len().checked_sub(header::HEADER_LEN)?;
        u32::try_from(body_len).ok()
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
    body_start: usize,
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
    fn reset(&mut self, body_start: usize) {
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
        [] => (0u64, 0u64),
        [b] => (u64::from(*b), u64::from(*b)),
        [first, .., last] => (u64::from(*first), u64::from(*last)),
    };
    // Only the low six bits are read, so the mix runs at the length's width.
    let h = (key.len() as u64)
        .wrapping_mul(131)
        .wrapping_add(first.wrapping_mul(31))
        .wrapping_add(last.wrapping_mul(7));
    1u64 << (h & 63)
}

/// Bytes a `Vec`'s capacity holds (scratch attribution).
fn vec_bytes<T>(v: &Vec<T>) -> usize {
    v.capacity().saturating_mul(size_of::<T>())
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

/// The frame word of a container whose placeholder sits at `len_at`;
/// `None` when the offset does not fit the word's 31 bits.
#[inline]
fn frame_word(len_at: usize, kind_bit: u32) -> Option<u32> {
    let len_at = u32::try_from(len_at).ok()?;
    (len_at & OBJ_BIT == 0).then_some(len_at | kind_bit)
}

/// Ingest limits (M3-S07): the per-namespace configuration surface. The
/// document bounds are a [`DocLimits`], which clamps them to the format
/// ceilings when it is built — configuration lowers bounds, never raises
/// them (ADR-0169 D2).
#[derive(Copy, Clone, Debug)]
pub struct ParseLimits {
    /// Container nesting depth (at most
    /// [`DEPTH_MAX`](crate::limits::DEPTH_MAX)) and encoded idoc
    /// **body** bytes (at most [`DOC_BYTES_MAX`]), the latter enforced
    /// incrementally during stage 2. The body axis is independent of
    /// `max_text` by design: small-token documents encode larger than their
    /// text (`1e1,` is 4 text bytes and 9 tape bytes), so the text bound
    /// alone does not bound memory.
    pub doc: DocLimits,
    /// Maximum input **text** bytes — enforced before UTF-8 validation
    /// and before the structural index allocates (reject-before-allocate:
    /// scratch growth is proportional to input, so the text bound is what
    /// bounds scratch). Note a pretty-printed text of a cap-passing
    /// document can exceed this; the wire frame is bounded regardless.
    pub max_text: usize,
}

impl Default for ParseLimits {
    fn default() -> ParseLimits {
        ParseLimits { doc: DocLimits::FORMAT, max_text: DOC_BYTES_MAX }
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
    /// Close-time duplicate scan scratch (entry ids by key, then the
    /// replacement map), pooled like the frames: an object past
    /// `LINEAR_SCAN_MAX` used to allocate both fresh on every parse
    /// (lane L10 perf row, batch 59).
    dup_order: Vec<u32>,
    dup_target: Vec<u32>,
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
    /// The tokens not yet consumed.
    rest: &'a [u32],
}

impl TokenSource for IndexTokens<'_> {
    #[inline]
    fn peek(&mut self) -> Option<u32> {
        self.rest.first().copied()
    }
    #[inline]
    fn bump(&mut self) {
        if let Some((_, rest)) = self.rest.split_first() {
            self.rest = rest;
        }
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

    /// Namespace-configured limits (M3-S07); their [`DocLimits`] already
    /// sits within the format ceilings.
    pub fn with_limits(limits: ParseLimits) -> JsonParser {
        JsonParser {
            blocks: Vec::new(),
            indices: Vec::new(),
            frames: Vec::new(),
            unescape: Vec::new(),
            rebuild: Vec::new(),
            obj_frames: Vec::new(),
            dup_order: Vec::new(),
            dup_target: Vec::new(),
            limits,
        }
    }

    /// Re-point a recycled parser at another namespace's resolved limits
    /// (M3-S11: one per-cell parser serves every store; one store per
    /// command beats rebuilding the scratch).
    pub fn set_limits(&mut self, limits: ParseLimits) {
        self.limits = limits;
    }

    /// Bytes of retained scratch (block masks, structural index, frame
    /// stacks, string and splice buffers). The parser is per-cell state
    /// and its memory belongs to the document domain (L5) — S19 wires
    /// this into attribution; the S07 pathological suite asserts
    /// rejections keep it bounded by the text cap, never by the would-be
    /// document.
    pub fn scratch_bytes(&self) -> usize {
        // An attribution metric: it saturates rather than wraps.
        let fixed = [
            vec_bytes(&self.blocks),
            vec_bytes(&self.indices),
            vec_bytes(&self.frames),
            vec_bytes(&self.unescape),
            vec_bytes(&self.rebuild),
            vec_bytes(&self.obj_frames),
            vec_bytes(&self.dup_order),
            vec_bytes(&self.dup_target),
        ];
        let entries = self.obj_frames.iter().map(|f| vec_bytes(&f.entries));
        fixed.into_iter().chain(entries).fold(0, usize::saturating_add)
    }

    /// After a parse: the per-object scratch keeps at most
    /// `OBJ_ENTRIES_RETAIN_MAX` entries per frame (and the dup-scan
    /// vectors the same), so one wide object never pins its peak — 12 B
    /// per key, about 2× the text of a `{"k":0,…}` shape, for the life of
    /// the cell (lane L10 perf row, batch 59). A steady state wider than
    /// the cap regrows once per parse: one `Vec` doubling from the cap.
    fn trim_scratch(&mut self) {
        for frame in &mut self.obj_frames {
            if frame.entries.capacity() > OBJ_ENTRIES_RETAIN_MAX {
                frame.entries.clear();
                frame.entries.shrink_to(OBJ_ENTRIES_RETAIN_MAX);
            }
        }
        for v in [&mut self.dup_order, &mut self.dup_target] {
            if v.capacity() > OBJ_ENTRIES_RETAIN_MAX {
                v.clear();
                v.shrink_to(OBJ_ENTRIES_RETAIN_MAX);
            }
        }
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
    ///
    /// The accepted document comes back as a [`CanonicalDoc`] receipt that
    /// borrows `out`: the parser's limits made it true (ADR-0169 D4).
    pub fn parse_into<'o>(
        &mut self,
        input: &[u8],
        out: &'o mut Vec<u8>,
    ) -> Result<CanonicalDoc<'o>, JsonParseError> {
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
        self.trim_scratch();
        result?;
        let out: &'o Vec<u8> = out;
        Ok(CanonicalDoc::parsed(out))
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
        let mut tokens = IndexTokens { rest: &indices[..n] };
        let result = self.parse_tokens(input, &mut tokens, &mut frames, &mut out);
        self.indices = indices;
        self.frames = frames;
        self.trim_scratch();
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
        let max_depth = self.limits.doc.depth_max();
        let max_body = self.limits.doc.body_bytes_max();
        let capacity = input.len().min(max_body).saturating_add(16);
        out.clear();
        out.reserve(capacity);
        let mut tape = Tape::new(out, max_body);
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
                        frames.push(tape.open_container(TAG_OBJ, OBJ_BIT, $at)?);
                        self.open_obj_frame(&mut live_obj_frames, tape.out.len());
                        expect = Expect::KeyOrObjClose;
                        tokens.bump();
                        continue $grammar;
                    }
                    b'[' => {
                        if frames.len() == max_depth {
                            return err($at, JsonErrorKind::DepthExceeded);
                        }
                        frames.push(tape.open_container(TAG_ARR, 0, $at)?);
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
                        self.close_obj_frame(&mut live_obj_frames, &mut tape, at)?;
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
                            if !self.note_key(live_obj_frames, tape.out, entry_at, key_len) {
                                return err(at, JsonErrorKind::DocumentTooLarge);
                            }
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
                                self.close_obj_frame(&mut live_obj_frames, &mut tape, at)?;
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
                    let Some(body_len) = tape.body_len() else {
                        return err(input.len(), JsonErrorKind::DocumentTooLarge);
                    };
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
                    self.close_obj_frame(&mut live_obj_frames, &mut tape, at)?;
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
    #[allow(
        clippy::arithmetic_side_effects,
        reason = "bound: `close <= at` returns above the difference, so at + 1 <= close"
    )]
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
        if close <= at {
            // Stage 1 orders its tokens; a close at or before its open is refused.
            return err(at, JsonErrorKind::UnterminatedString);
        }
        let len = close - (at + 1);
        if len <= FIXSTR_MAX_LEN && try_fixstr_fast(tape, input, at, len) {
            return Ok(len);
        }
        // Outlined: the grammar loop inlines `emit_string` at two sites,
        // and growing it past the fixstr try regressed the string-light
        // shapes (deep/wide) through sheer code size — the general path
        // costs a call it amortizes over ≥ 32-byte payloads.
        self.emit_string_general(tape, input, at, close)
    }

    #[inline(never)]
    #[allow(
        clippy::arithmetic_side_effects,
        reason = "bound: input[at..close][1..] is sliced first, so at < close <= input.len() <= \
                  isize::MAX; every len is a slice or Vec length (<= isize::MAX) and a string \
                  header is at most 4 bytes"
    )]
    fn emit_string_general(
        &mut self,
        tape: &mut Tape<'_>,
        input: &[u8],
        at: usize,
        close: usize,
    ) -> Result<usize, JsonParseError> {
        let content = &input[at..close][1..];
        let len = content.len();
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

    #[allow(
        clippy::arithmetic_side_effects,
        reason = "bound: obj_frames[*live] is indexed on the line above the increment, so \
                  *live < obj_frames.len() <= isize::MAX"
    )]
    fn open_obj_frame(&mut self, live: &mut usize, body_start: usize) {
        if *live == self.obj_frames.len() {
            self.obj_frames.push(ObjFrame::default());
        }
        self.obj_frames[*live].reset(body_start);
        *live += 1;
    }

    /// Record an emitted key; small objects detect duplicates on insert
    /// (fingerprint filter, then memcmp on the recorded spans), large
    /// ones defer to the close-time sort. `false` when an offset is past
    /// the frame word's 31 bits — far beyond the tape cap.
    #[inline]
    #[allow(
        clippy::arithmetic_side_effects,
        clippy::cast_possible_truncation,
        reason = "bound: key_at <= LEN_AT_MASK (2^31 - 1) is checked first and entry_at <= \
                  key_at (a saturating sum), so both fit u32; `ka` widens a u32 and key_len <= \
                  u16::MAX is the guard of the branch that adds them, so that sum fits the \
                  64-bit usize inf-foundation const-asserts"
    )]
    fn note_key(&mut self, live: usize, out: &[u8], entry_at: usize, key_len: usize) -> bool {
        // The key bytes sit right after their canonical string header. One
        // compare bounds both offsets, at the frame word's 31 bits (far above
        // the 2^24 + 7 tape cap); a conversion each cost the fused parse ~3%
        // on key-heavy shapes.
        let key_at = entry_at.saturating_add(emit::str_header_len(key_len));
        if key_at > LEN_AT_MASK as usize {
            return false;
        }
        let key_len16 = u16::try_from(key_len).unwrap_or(u16::MAX);
        // `live == 0` wraps to an index no frame has: refused, like a missing frame.
        let Some(frame) = self.obj_frames.get_mut(live.wrapping_sub(1)) else {
            return false;
        };
        if !frame.dup_found
            && frame.entries.len() <= LINEAR_SCAN_MAX
            && key_len <= u16::MAX as usize
        {
            let key = &out[key_at..][..key_len];
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
        frame.entries.push(ObjEntry {
            entry_at: entry_at as u32,
            key_at: key_at as u32,
            key_len: key_len16,
        });
        true
    }

    /// Close the innermost object: if duplicates exist (or the object was
    /// too large for insert-time detection), rebuild it
    /// ([`rebuild_obj_frame`](Self::rebuild_obj_frame)). `at` is the
    /// closing token's offset, for the typed refusal. Kept small and marked
    /// `#[inline]` for the grammar's three object closes (a whole-function
    /// call cost nested shapes ~3% of the parse).
    #[inline]
    fn close_obj_frame(
        &mut self,
        live: &mut usize,
        tape: &mut Tape<'_>,
        at: usize,
    ) -> Result<(), JsonParseError> {
        let Some(top) = live.checked_sub(1) else {
            return err(at, JsonErrorKind::DocumentTooLarge);
        };
        let Some(frame) = self.obj_frames.get(top) else {
            return err(at, JsonErrorKind::DocumentTooLarge);
        };
        if !frame.dup_found && frame.entries.len() <= LINEAR_SCAN_MAX {
            *live = top;
            return Ok(());
        }
        self.rebuild_obj_frame(live, top, tape, at)
    }

    /// Rebuild the closing object's body with last-occurrence-wins /
    /// first-position-kept semantics and splice it over the original
    /// (ADR-0036 D5). Cold path: it runs only for objects that contained
    /// duplicates or exceeded the linear-scan cap.
    #[inline(never)]
    fn rebuild_obj_frame(
        &mut self,
        live: &mut usize,
        top: usize,
        tape: &mut Tape<'_>,
        at: usize,
    ) -> Result<(), JsonParseError> {
        let Some(frame) = self.obj_frames.get_mut(top) else {
            return err(at, JsonErrorKind::DocumentTooLarge);
        };
        // Refusal leaves the frame stack and tape unchanged.
        let Ok(entry_count) = u32::try_from(frame.entries.len()) else {
            return err(at, JsonErrorKind::DocumentTooLarge);
        };
        *live = top;
        let body_start = frame.body_start;
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
                &tape.out[e.key_at as usize..][..usize::from(e.key_len)]
            }
        };
        let by_key = &mut self.dup_order;
        by_key.clear();
        by_key.extend(0..entry_count);
        by_key.sort_unstable_by(|&a, &b_idx| {
            key_of(&entries[a as usize]).cmp(key_of(&entries[b_idx as usize]))
        });
        let replace_with = &mut self.dup_target;
        replace_with.clear();
        replace_with.extend(0..entry_count);
        let same_key =
            |a: u32, b: u32| key_of(&entries[a as usize]) == key_of(&entries[b as usize]);
        if !mark_duplicate_runs(by_key, replace_with, same_key) {
            return Ok(());
        }
        self.rebuild.clear();
        for &target in replace_with.iter() {
            if target == SKIP {
                continue;
            }
            let target = target as usize;
            let begin = entries[target].entry_at as usize;
            let end = entries[target..].get(1).map_or(body_end, |e| e.entry_at as usize);
            self.rebuild.extend_from_slice(&tape.out[begin..end]);
        }
        // Splice the rebuilt body over the original. Every open
        // placeholder (this object's and its ancestors') precedes
        // `body_start`, so no u24 moves — the D3 backpatch argument.
        debug_assert!(body_start <= body_end);
        debug_assert_eq!(body_end, tape.out.len());
        tape.out.truncate(body_start);
        tape.out.extend_from_slice(&self.rebuild);
        Ok(())
    }
}

/// `replace_with[idx]` for an entry the rebuild drops (an earlier
/// occurrence of a duplicated key that is not the first).
const SKIP: u32 = u32::MAX;

/// Group `by_key` (entry ids sorted by key) into equal-key runs and mark
/// each run of two or more in `replace_with`: the first occurrence takes
/// the last one's id (emit that span at this position), the others
/// [`SKIP`]. Returns whether any run had a duplicate.
#[allow(
    clippy::arithmetic_side_effects,
    reason = "bound: `run + 1` and `end += 1` sit under their loops' `< by_key.len()` guards \
              (<= isize::MAX), and end >= run + 1 where the difference is taken"
)]
fn mark_duplicate_runs(
    by_key: &[u32],
    replace_with: &mut [u32],
    same_key: impl Fn(u32, u32) -> bool,
) -> bool {
    let mut any_dup = false;
    let mut run = 0usize;
    while run < by_key.len() {
        let mut end = run + 1;
        while end < by_key.len() && same_key(by_key[run], by_key[end]) {
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
    any_dup
}

/// Decode the key bytes of the entry starting at `at` in the output tape
/// (the parser wrote it, so the encoding is canonical fixstr/str8/str24).
fn key_bytes_at(out: &[u8], at: usize) -> &[u8] {
    let entry = &out[at..];
    let tag = entry[0];
    if (0x80..=0x9F).contains(&tag) {
        // A fixstr tag carries its length in the low five bits.
        &entry[1..][..usize::from(tag & 0x1F)]
    } else if tag == 0xA5 {
        &entry[2..][..usize::from(entry[1])]
    } else {
        debug_assert_eq!(tag, 0xA6, "parser keys are canonical string forms");
        let len = usize::from(entry[1]) | usize::from(entry[2]) << 8 | usize::from(entry[3]) << 16;
        &entry[4..][..len]
    }
}

/// `true` / `false` / `null`, with a hard terminator check (`truex` is a
/// grammar error even though stage 1 emits one token for it).
#[allow(
    clippy::arithmetic_side_effects,
    reason = "bound: input[at..] starts with `text` where the sum is taken, so at + text.len() \
              <= input.len() <= isize::MAX"
)]
fn parse_literal(input: &[u8], at: usize, tape: &mut Tape<'_>) -> Result<(), JsonParseError> {
    let text: &[u8] = match input[at] {
        b't' => b"true",
        b'f' => b"false",
        _ => b"null",
    };
    if !input[at..].starts_with(text) {
        return err(at, JsonErrorKind::UnexpectedCharacter(input[at]));
    }
    check_scalar_terminator(input, at + text.len())?;
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

/// The longest digit run [`parse_digits`] accumulates: 10¹⁹ − 1 < 2⁶⁴.
const DIGITS_U64_MAX: usize = 19;

/// Digits-only slice → u64 via 8-digit SWAR chunks; `None` past
/// [`DIGITS_U64_MAX`] digits (the caller takes its f64 fallback). Every
/// byte contributes its low nibble, so the sum is bounded whatever the
/// bytes are; callers pass ASCII digit runs, for which it is the value.
#[allow(
    clippy::arithmetic_side_effects,
    reason = "bound: bytes.len() <= 19 is checked first and every byte is masked to a nibble \
              (<= 15): the SWAR lanes hold at most 165, 16 665 and 166 666 665 in their 8, 16 \
              and 32 bits, and acc <= 15 * (10^19 - 1) / 9 < 1.67e19 < u64::MAX"
)]
fn parse_digits(bytes: &[u8]) -> Option<u64> {
    if bytes.len() > DIGITS_U64_MAX {
        return None;
    }
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
        acc = acc * 10 + u64::from(d & 0x0F);
    }
    Some(acc)
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
#[allow(
    clippy::arithmetic_side_effects,
    reason = "bound: a full word was just read at input[i..], so i + 8 <= input.len() and \
              digit_run_len returns <= 8; `i += 1` follows input[i] under its `i < len` guard"
)]
fn digit_run_end(input: &[u8], mut i: usize) -> usize {
    while let Some(word) = input.get(i..).and_then(|tail| tail.first_chunk::<8>()) {
        let n = digit_run_len(u64::from_le_bytes(*word));
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
        .and_then(|at| at.checked_add(1))
        .unwrap_or(start);
    if start == end {
        return err(start, JsonErrorKind::InvalidNumber);
    }
    let token = &input[start..end];
    let (number, used) = parse_number_value(token, 0).map_err(|error| JsonParseError {
        offset: error.offset.saturating_add(start),
        kind: error.kind,
    })?;
    if used != token.len() {
        return err(start.saturating_add(used), JsonErrorKind::UnexpectedCharacter(token[used]));
    }
    Ok(number)
}

#[allow(
    clippy::arithmetic_side_effects,
    reason = "bound: each `i += 1` follows input[i] being indexed or input.get(i) being Some, \
              so i < input.len() <= isize::MAX"
)]
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
    let mut exp10: i32 = 0;
    if matches!(input.get(i), Some(&b'e') | Some(&b'E')) {
        is_float = true;
        i += 1;
        let exp_neg = input.get(i) == Some(&b'-');
        if matches!(input.get(i), Some(&b'-') | Some(&b'+')) {
            i += 1;
        }
        let exp_start = i;
        i = digit_run_end(input, i);
        if i == exp_start {
            return err(at, JsonErrorKind::InvalidNumber); // "1e"
        }
        exp10 = exponent_value(&input[exp_start..i], exp_neg);
    }
    check_scalar_terminator(input, i)?;
    let fast = if is_float {
        clinger_fast(int_digits, frac, exp10, neg).map(Number::F64)
    } else {
        integral_fast(int_digits, neg)
    };
    if let Some(number) = fast {
        return Ok((number, i));
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

/// Exponent digits past this many saturate (10⁹ − 1 < 2³¹).
const EXPONENT_DIGITS_MAX: usize = 9;
/// The saturated exponent: far past the f64 range — the fast path only
/// reads |exp| ≤ 22, and the std fallback re-parses the text.
const EXPONENT_SATURATED: i32 = 100_000;

/// The signed exponent of a digit run (nibbles, as [`parse_digits`]).
#[allow(
    clippy::arithmetic_side_effects,
    reason = "bound: digits.len() <= 9 is checked first and every byte is masked to a nibble \
              (<= 15), so v <= 15 * 111 111 111 < 1.67e9 < i32::MAX, and negating a \
              non-negative i32 cannot overflow"
)]
fn exponent_value(digits: &[u8], negative: bool) -> i32 {
    let mut v: i32 = EXPONENT_SATURATED;
    if digits.len() <= EXPONENT_DIGITS_MAX {
        v = 0;
        for &d in digits {
            v = v * 10 + i32::from(d & 0x0F);
        }
    }
    if negative { -v } else { v }
}

/// Integral token: i64 when it fits; `-0` keeps its sign as f64; `None`
/// is the ADR-0036 D4 f64 fallback (S21 measures the oracle's u64-range
/// behavior — candidate deviation).
fn integral_fast(int_digits: &[u8], neg: bool) -> Option<Number> {
    let magnitude = parse_digits(int_digits)?;
    if !neg {
        return i64::try_from(magnitude).ok().map(Number::I64);
    }
    if magnitude == 0 {
        return Some(Number::F64(-0.0));
    }
    // `0 - magnitude`: reaches `i64::MIN`, which negating an i64 cannot.
    0i64.checked_sub_unsigned(magnitude).map(Number::I64)
}

/// Clinger fast path: a mantissa exact in f64 (≤ 2⁵³) scaled by an exact
/// power of ten (|10^e| ≤ 10²²) rounds exactly once — bit-identical to
/// the Eisel–Lemire fallback, minus its full re-parse. `None` is that
/// fallback; an unrepresentable mantissa takes it too.
fn clinger_fast(int_digits: &[u8], frac: &[u8], exp10: i32, neg: bool) -> Option<f64> {
    // The mantissa is the two runs read as one: at most 19 digits in all.
    if int_digits.len().checked_add(frac.len())? > DIGITS_U64_MAX {
        return None;
    }
    let scale = *U64_POW10.get(frac.len())?;
    let m = parse_digits(int_digits)?.checked_mul(scale)?.checked_add(parse_digits(frac)?)?;
    let e = exp10.saturating_sub(i32::try_from(frac.len()).ok()?);
    if m > (1u64 << 53) || !(-22..=22).contains(&e) {
        return None;
    }
    let power = *F64_POW10.get(usize::try_from(e.unsigned_abs()).ok()?)?;
    let scaled = if e < 0 { m as f64 / power } else { m as f64 * power };
    Some(if neg { -scaled } else { scaled })
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
#[allow(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    reason = "bound: `len > FIXSTR_MAX_LEN` (31) returns first, so 1 + len <= 32, len fits u8 \
              and FIXSTR_BASE (0x80) + len <= 0x9F"
)]
fn try_fixstr_fast(tape: &mut Tape<'_>, input: &[u8], at: usize, len: usize) -> bool {
    if len > FIXSTR_MAX_LEN || !tape.fits(1 + len) {
        return false;
    }
    if len == 0 {
        tape.out.push(FIXSTR_BASE);
        return true;
    }
    // The 32-byte window that opens after the quote at `at`: one bounds
    // check (two chained `get`s cost the fused parse ~1–2%); a wrapped end is
    // a reversed range, refused like a short input.
    let Some(quoted) = input.get(at..at.wrapping_add(33)) else {
        return false;
    };
    let window = &quoted[1..];
    inf_simd::json_copy_unescaped_fixstr(window, len, FIXSTR_BASE + len as u8, tape.out)
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
#[allow(
    clippy::arithmetic_side_effects,
    reason = "bound: i <= len throughout (it steps by 1 under `i < len`, by 8 only after \
              `i + 8 <= len`), len <= isize::MAX, `len - 8` runs only past the `len < 8` \
              return, and a hit's byte index (<= 7) lies inside the word just read"
)]
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
#[allow(
    clippy::arithmetic_side_effects,
    reason = "bound: i < bytes.len() under the loop guard; `i += 2` follows bytes.get(i + 1) \
              being Some, `i += 4` and `i += 6` follow parse_hex4 reading four digits at i and \
              at i + 2, and find_special's index is inside bytes[i..] — every cursor is <= \
              bytes.len() <= isize::MAX; hi and lo are range-checked above the surrogate sum \
              (<= 0x10FFFF)"
)]
fn decode_string<'a>(
    content: &'a [u8],
    base: usize,
    scratch: &mut Vec<u8>,
) -> Result<Option<&'a [u8]>, JsonParseError> {
    let bytes = content;
    let Some(first) = find_special(bytes) else {
        return Ok(Some(content));
    };
    // Error offsets are diagnostics: they saturate, never wrap.
    let offset = |i: usize| base.saturating_add(i);
    if bytes[first] < 0x20 {
        return err(offset(first), JsonErrorKind::ControlCharacter);
    }
    scratch.clear();
    scratch.reserve(content.len());
    // Safe prefix (bounded by an ASCII backslash), then the escape walk.
    scratch.extend_from_slice(&content[..first]);
    let mut i = first;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => {
                let at = offset(i);
                let Some(&esc) = bytes.get(i + 1) else {
                    return err(at, JsonErrorKind::InvalidEscape);
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
                                offset: offset(i),
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
                    _ => return err(at, JsonErrorKind::InvalidEscape),
                }
            }
            b if b < 0x20 => return err(offset(i), JsonErrorKind::ControlCharacter),
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
    let mut v = 0u32;
    for &b in content.get(i..)?.first_chunk::<4>()? {
        let d = (b as char).to_digit(16)?;
        v = v << 4 | d;
    }
    Some(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_object_frame_refuses_without_changing_the_tape() {
        let mut parser = JsonParser::new();
        let mut out = vec![0; header::HEADER_LEN];
        emit::str(&mut out, b"k");
        let before = out.clone();
        assert!(!parser.note_key(0, &out, header::HEADER_LEN, 1));
        assert_eq!(out, before);
        let mut live = 0;
        let mut tape = Tape::new(&mut out, DOC_BYTES_MAX);
        let error = parser.close_obj_frame(&mut live, &mut tape, 7).unwrap_err();
        assert_eq!(error.kind, JsonErrorKind::DocumentTooLarge);
        assert_eq!(error.offset, 7);
        assert_eq!(live, 0);
        assert_eq!(out, before);
    }

    #[test]
    fn key_offsets_refuse_before_bookkeeping_changes() {
        let mut parser = JsonParser::new();
        let mut live = 0;
        parser.open_obj_frame(&mut live, header::HEADER_LEN);
        for entry_at in [LEN_AT_MASK as usize, u32::MAX as usize, u32::MAX as usize + 1, usize::MAX]
        {
            assert!(!parser.note_key(live, &[], entry_at, 1));
            assert!(parser.obj_frames[0].entries.is_empty());
            assert_eq!(parser.obj_frames[0].fp, 0);
        }
        // The last offset the bound accepts (a key past u16::MAX takes the
        // 4-byte header and skips the span read): key_at == LEN_AT_MASK.
        let long = 1 << 16;
        assert!(!parser.note_key(live, &[], LEN_AT_MASK as usize - 3, long));
        assert!(parser.note_key(live, &[], LEN_AT_MASK as usize - 4, long));
        assert_eq!(parser.obj_frames[0].entries[0].key_at, LEN_AT_MASK);
    }

    #[test]
    fn reversed_string_tokens_refuse() {
        let mut parser = JsonParser::new();
        for close in [2, 1] {
            let mut out = vec![0; header::HEADER_LEN];
            let before = out.clone();
            let mut tape = Tape::new(&mut out, DOC_BYTES_MAX);
            let error = parser.emit_string(&mut tape, b"  \"\"", 2, close).unwrap_err();
            assert_eq!(error.kind, JsonErrorKind::UnterminatedString);
            assert_eq!(error.offset, 2);
            assert_eq!(out, before);
        }
    }

    #[test]
    fn string_diagnostics_keep_the_input_offset() {
        let cases: &[(&[u8], usize, JsonErrorKind)] = &[
            (br"ab\uD800\uZZZZ", 9, JsonErrorKind::InvalidUnicodeEscape),
            (br"\uD800\u", 7, JsonErrorKind::InvalidUnicodeEscape),
            (br"\uD800\uDC0", 7, JsonErrorKind::InvalidUnicodeEscape),
            (br"\q", 1, JsonErrorKind::InvalidEscape),
            (br"x\u12", 2, JsonErrorKind::InvalidUnicodeEscape),
        ];
        for &(content, offset, kind) in cases {
            let error = decode_string(content, 1, &mut Vec::new()).unwrap_err();
            assert_eq!((error.offset, error.kind), (offset, kind), "{content:?}");
        }
        let error = decode_string(br"x\q", usize::MAX, &mut Vec::new()).unwrap_err();
        assert_eq!(error.offset, usize::MAX);
    }

    /// `parse_digits` owns its bound: 19 digits is the last length it
    /// accumulates, and no 19 bytes — digits or not — overflow the sum
    /// (a debug build panics on overflow, so this test is the proof's canary).
    #[test]
    fn parse_digits_bound_is_nineteen() {
        assert_eq!(parse_digits(b""), Some(0));
        assert_eq!(parse_digits(b"7"), Some(7));
        assert_eq!(parse_digits(b"12345678"), Some(12_345_678));
        assert_eq!(parse_digits(b"123456789"), Some(123_456_789));
        assert_eq!(parse_digits(b"1234567890123456"), Some(1_234_567_890_123_456));
        assert_eq!(parse_digits(&[b'9'; 18]), Some(999_999_999_999_999_999));
        assert_eq!(parse_digits(&[b'9'; 19]), Some(9_999_999_999_999_999_999));
        assert_eq!(parse_digits(&[b'9'; 20]), None);
        assert_eq!(parse_digits(&[b'0'; 20]), None);
        for len in 0..=19 {
            assert!(parse_digits(&vec![0xFF; len]).is_some(), "{len} hostile bytes stay bounded");
        }
    }

    #[test]
    fn exponent_saturates_past_nine_digits() {
        assert_eq!(exponent_value(b"0", false), 0);
        assert_eq!(exponent_value(b"22", true), -22);
        assert_eq!(exponent_value(b"999999999", false), 999_999_999);
        assert_eq!(exponent_value(b"999999999", true), -999_999_999);
        assert_eq!(exponent_value(b"0000000001", false), EXPONENT_SATURATED);
        assert_eq!(exponent_value(b"0000000001", true), -EXPONENT_SATURATED);
        // Nine hostile bytes stay inside i32 (the bound's canary).
        assert!(exponent_value(&[0xFF; 9], true) < 0);
    }

    #[test]
    fn frame_word_holds_thirty_one_bits() {
        assert_eq!(frame_word(0, 0), Some(0));
        assert_eq!(frame_word(9, OBJ_BIT), Some(9 | OBJ_BIT));
        assert_eq!(frame_word(LEN_AT_MASK as usize, OBJ_BIT), Some(u32::MAX));
        assert_eq!(frame_word(LEN_AT_MASK as usize + 1, 0), None);
        assert_eq!(frame_word(u32::MAX as usize + 1, 0), None);
        assert_eq!(frame_word(usize::MAX, OBJ_BIT), None);
    }

    #[test]
    fn tape_cap_refuses_at_the_limit_and_on_overflow() {
        let mut out = vec![0; header::HEADER_LEN];
        let tape = Tape::new(&mut out, 4);
        assert!(tape.fits(0));
        assert!(tape.fits(3));
        assert!(tape.fits(4));
        assert!(!tape.fits(5));
        assert!(!tape.fits(usize::MAX));
        assert_eq!(tape.body_len(), Some(0));
        // A configured cap past the format ceiling is clamped to it.
        let mut out = vec![0; header::HEADER_LEN];
        let tape = Tape::new(&mut out, usize::MAX);
        assert!(tape.fits(DOC_BYTES_MAX));
        assert!(!tape.fits(DOC_BYTES_MAX + 1));
        // A buffer shorter than its header has no body length.
        let mut short = vec![0; header::HEADER_LEN - 1];
        assert_eq!(Tape::new(&mut short, 4).body_len(), None);
    }

    #[test]
    fn container_open_reserves_before_writing() {
        for (tag, kind) in [(TAG_OBJ, OBJ_BIT), (TAG_ARR, 0)] {
            let mut out = vec![0; header::HEADER_LEN];
            let before = out.clone();
            let mut tape = Tape::new(&mut out, emit::CONTAINER_OPEN_LEN - 1);
            let error = tape.open_container(tag, kind, 11).unwrap_err();
            assert_eq!((error.offset, error.kind), (11, JsonErrorKind::DocumentTooLarge));
            assert_eq!(out, before);
            let mut tape = Tape::new(&mut out, emit::CONTAINER_OPEN_LEN);
            let word = tape.open_container(tag, kind, 11).unwrap();
            assert_eq!(word & OBJ_BIT, kind);
            assert_eq!(word & LEN_AT_MASK, header::HEADER_LEN as u32 + 1);
            assert_eq!(out.len(), header::HEADER_LEN + emit::CONTAINER_OPEN_LEN);
            assert_eq!(out[header::HEADER_LEN], tag);
        }
    }

    #[test]
    fn index_tokens_bump_past_the_end_stays_empty() {
        let mut tokens = IndexTokens { rest: &[3, 7] };
        assert_eq!(tokens.peek(), Some(3));
        tokens.bump();
        assert_eq!(tokens.peek(), Some(7));
        tokens.bump();
        tokens.bump();
        assert_eq!(tokens.peek(), None);
    }

    #[test]
    fn key_bytes_at_reads_each_string_form() {
        let mut out = vec![0xEE];
        emit::str(&mut out, b"k");
        assert_eq!(key_bytes_at(&out, 1), b"k");
        for len in [0usize, 31, 32, 255, 256, 70_000] {
            let key = vec![b'a'; len];
            let mut out = vec![0xEE, 0xEE];
            emit::str(&mut out, &key);
            out.push(0xEE);
            assert_eq!(key_bytes_at(&out, 2), &key[..], "key of {len} bytes");
        }
    }
}
