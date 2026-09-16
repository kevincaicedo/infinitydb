//! Fabric op codec **v0** (M0-S10) — frozen at M0 exit (interfaces-m0.md §4).
//!
//! Frame layout (all integers little-endian):
//!
//! ```text
//! header  { version: u8, op: u8, flags: u16, len: u32 }   // 8 bytes
//! payload { per-op, `len` bytes }
//! ```
//!
//! Header `flags` are reserved and must be zero at v0. Payload fields use
//! fixed-width encodings for routing-hot fields (token, slot) and
//! `inf_foundation::varint` for byte-slice lengths and counts. `Batch`
//! payloads nest complete non-batch frames, so M4's `LockOp`/`ExecOp` extend
//! the vocabulary by adding opcodes, not by reshaping the transport.
//! [`Op::ApplyNs`] (M2-S08, ADR-0015 D1) is the first such additive opcode:
//! existing wire layouts are untouched, v0 stays the frame version.
//!
//! [`decode`] is **total**: any byte input either parses or returns a typed
//! [`CodecError`] — no panics, no UB (fuzzed by `fuzz/fuzz_targets/
//! fabric_codec.rs`, which runs in the CI fuzz job). Decoding borrows all
//! byte payloads from the input — zero copies (the decode allocations are
//! the `Vec` of nested ops in `Batch`, flagged as an M1 optimization, and
//! the slice table of an `Apply`/`ApplyNs` wider than
//! [`MAX_INLINE_APPLY_ARGS`] — ADR-0120 D2, sized by the frame).

use core::fmt;

use inf_foundation::time::Nanos;
use inf_foundation::{KeySlot, varint};

use crate::msg::FabricToken;

/// Wire version emitted and accepted by this codec.
pub const CODEC_VERSION: u8 = 0;

/// Argument slices an [`ApplyArgs`] stores inline — the allocation-free
/// width every plane-composed program and every client command up to
/// this width rides (ADR-0120 D2; the codec's whole width before it).
pub const MAX_INLINE_APPLY_ARGS: usize = 16;

/// Maximum number of argument slices in an [`Op::Apply`] / [`Op::ApplyNs`]
/// — the client parser's argv bound (`inf_wire::ParserLimits::max_args`;
/// `inf-server` asserts this is not narrower), so every argv a client
/// can present ships whole to its owner whatever cell it lands on
/// (ADR-0120 D1, review of 2026-08-30 F-L17-15). Wider argvs beyond
/// the inline width spill to one exact-sized table (D2).
pub const MAX_APPLY_ARGS: usize = 1024;

/// Maximum number of nested ops in an [`Op::Batch`].
pub const MAX_BATCH_OPS: usize = 256;

const HEADER_LEN: usize = 8;

/// Header flag bit 0 (ADR-0115): the `Apply`/`ApplyNs` argv was composed
/// by a plane program, not copied from a client. Every other bit stays
/// reserved; the bit on any other opcode is [`CodecError::FlagNotApplicable`].
pub const FLAG_PROGRAM: u16 = 1;

const OP_READ: u8 = 1;
const OP_WRITE: u8 = 2;
const OP_APPLY: u8 = 3;
const OP_BATCH: u8 = 4;
const OP_REPLY: u8 = 5;
const OP_APPLY_NS: u8 = 6;
/// An accepted socket handed to another cell of the same process
/// (ADR-0128, lane L11 N19): the additive opcode reserved by ADR-0009 §4.
const OP_ADOPT_CONN: u8 = 7;

/// Smallest namespace id an [`Op::ApplyNs`] may carry: ids `0..16` are the
/// default namespaces (`db0..db15`) and ride [`Op::Apply`]'s packed `cmd`
/// byte — one canonical encoding per op (ADR-0015 D1).
const APPLY_NS_MIN: u32 = 16;

const OUTCOME_OK: u8 = 0;
const OUTCOME_BYTES: u8 = 1;
const OUTCOME_INT: u8 = 2;
const OUTCOME_NIL: u8 = 3;
const OUTCOME_BOOL: u8 = 4;
const OUTCOME_ERR: u8 = 5;

const EXPIRE_NONE: u8 = 0;
const EXPIRE_AT: u8 = 1;

/// Write-op condition/behavior flags (wire-stable u8 bitset).
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug, Default)]
pub struct WriteFlags(u8);

impl WriteFlags {
    /// Unconditional write.
    pub const NONE: WriteFlags = WriteFlags(0);
    /// Apply only if the key is absent (`SETNX` shape).
    pub const IF_ABSENT: WriteFlags = WriteFlags(1);
    /// Apply only if the key is present (`SET XX` shape).
    pub const IF_PRESENT: WriteFlags = WriteFlags(1 << 1);
    /// Return the previous value in the reply (`SET GET` shape).
    pub const GET_OLD: WriteFlags = WriteFlags(1 << 2);

    const ALL: u8 = 0b111;

    /// Raw wire bits.
    #[inline]
    pub fn bits(self) -> u8 {
        self.0
    }

    /// Validates wire bits; `None` if any unknown bit is set.
    #[inline]
    pub fn from_bits(bits: u8) -> Option<WriteFlags> {
        (bits & !Self::ALL == 0).then_some(WriteFlags(bits))
    }

    /// True if every flag in `other` is set in `self`.
    #[inline]
    pub fn contains(self, other: WriteFlags) -> bool {
        self.0 & other.0 == other.0
    }
}

impl core::ops::BitOr for WriteFlags {
    type Output = WriteFlags;
    #[inline]
    fn bitor(self, rhs: WriteFlags) -> WriteFlags {
        WriteFlags(self.0 | rhs.0)
    }
}

/// Typed engine error carried in [`Outcome::Err`]. Wire values are stable;
/// codes this build does not know decode as [`ErrCode::Unknown`] (known codes
/// canonicalize on decode).
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum ErrCode {
    /// Operation applied to a value of the wrong type.
    WrongType,
    /// Value is not an integer (INCR family).
    NotInt,
    /// Integer overflow/underflow.
    Overflow,
    /// Cell memory budget exhausted.
    OutOfMemory,
    /// Forward-compatibility escape hatch: an unrecognized wire code.
    Unknown(u16),
}

impl ErrCode {
    /// Stable wire value.
    #[inline]
    pub fn to_u16(self) -> u16 {
        match self {
            ErrCode::WrongType => 1,
            ErrCode::NotInt => 2,
            ErrCode::Overflow => 3,
            ErrCode::OutOfMemory => 4,
            ErrCode::Unknown(raw) => raw,
        }
    }

    /// Decodes a wire value; known codes canonicalize to their variant.
    #[inline]
    pub fn from_u16(raw: u16) -> ErrCode {
        match raw {
            1 => ErrCode::WrongType,
            2 => ErrCode::NotInt,
            3 => ErrCode::Overflow,
            4 => ErrCode::OutOfMemory,
            _ => ErrCode::Unknown(raw),
        }
    }
}

/// Result of a fabric data op, carried by [`Op::Reply`].
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Outcome<'a> {
    /// Success with no payload.
    Ok,
    /// Success with a value (borrowed from the encoded frame on decode).
    Bytes(&'a [u8]),
    /// Success with an integer.
    Int(i64),
    /// Key/value absent.
    Nil,
    /// Boolean result (EXISTS, EXPIRE, ...).
    Bool(bool),
    /// Typed failure.
    Err(ErrCode),
}

/// Argument slices for [`Op::Apply`] / [`Op::ApplyNs`] — at most
/// [`MAX_APPLY_ARGS`]. Up to [`MAX_INLINE_APPLY_ARGS`] live inline (no
/// allocation on encode or decode — the shipped hot path, unchanged);
/// a wider argv lives in one exact-sized table (ADR-0120 D2).
#[derive(Clone)]
pub struct ApplyArgs<'a> {
    inline: [&'a [u8]; MAX_INLINE_APPLY_ARGS],
    len: u16,
    /// Every slice when `len > MAX_INLINE_APPLY_ARGS`; empty (never
    /// allocated) otherwise.
    spill: Vec<&'a [u8]>,
}

impl<'a> ApplyArgs<'a> {
    /// No arguments.
    pub const EMPTY: ApplyArgs<'static> =
        ApplyArgs { inline: [&[]; MAX_INLINE_APPLY_ARGS], len: 0, spill: Vec::new() };

    /// Builds from a slice of slices; `None` if more than [`MAX_APPLY_ARGS`].
    pub fn new(args: &[&'a [u8]]) -> Option<ApplyArgs<'a>> {
        if args.len() > MAX_APPLY_ARGS {
            return None;
        }
        let mut inline: [&'a [u8]; MAX_INLINE_APPLY_ARGS] = [&[]; MAX_INLINE_APPLY_ARGS];
        let spill = if args.len() <= MAX_INLINE_APPLY_ARGS {
            inline[..args.len()].copy_from_slice(args);
            Vec::new()
        } else {
            args.to_vec()
        };
        // Length fits in u16 because MAX_APPLY_ARGS < 65536.
        Some(ApplyArgs { inline, len: args.len() as u16, spill })
    }

    /// The argument slices.
    #[inline]
    pub fn as_slice(&self) -> &[&'a [u8]] {
        if self.spill.is_empty() { &self.inline[..usize::from(self.len)] } else { &self.spill }
    }

    /// Number of arguments.
    #[inline]
    pub fn len(&self) -> usize {
        usize::from(self.len)
    }

    /// True when there are no arguments.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl fmt::Debug for ApplyArgs<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.as_slice()).finish()
    }
}

impl PartialEq for ApplyArgs<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.as_slice() == other.as_slice()
    }
}

impl Eq for ApplyArgs<'_> {}

/// Fabric op vocabulary v0 (master plan §6.2). Decoded values borrow all
/// byte payloads from the input frame.
// `Write` dominates the size; boxing it would put an allocation on the
// decode path of every fabric write — ops are transient stack values
// consumed inside one drain callback, so the size spread is free.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Op<'a> {
    /// Point read of `key` on the owning cell.
    Read { token: FabricToken, slot: KeySlot, key: &'a [u8] },
    /// Point write of `key` on the owning cell.
    Write {
        token: FabricToken,
        slot: KeySlot,
        key: &'a [u8],
        value: &'a [u8],
        expire_at: Option<Nanos>,
        flags: WriteFlags,
    },
    /// Generic remote command execution — M0-experimental (M4 reshapes into
    /// `ExecOp`). `program` is the header's [`FLAG_PROGRAM`] (ADR-0115).
    Apply { token: FabricToken, slot: KeySlot, cmd: u8, args: ApplyArgs<'a>, program: bool },
    /// Generic remote command execution in a **named** namespace (M2-S08,
    /// ADR-0015 D1) — the additive opcode reserved by ADR-0009 §4. Mirrors
    /// [`Op::Apply`] with the namespace id as an explicit `u32`
    /// (little-endian, fixed width) placed right after `cmd`; `cmd` keeps
    /// the `{0:4 | proto:4}` packing (the db nibble is always zero here).
    ///
    /// `ns` must be `>= 16`: ids `0..16` are the default namespaces and
    /// ride [`Op::Apply`] — one canonical encoding per op (ADR-0015 D1).
    /// [`decode`] rejects `ns < 16` with [`CodecError::ApplyNsDefault`].
    ApplyNs {
        token: FabricToken,
        slot: KeySlot,
        cmd: u8,
        ns: u32,
        args: ApplyArgs<'a>,
        program: bool,
    },
    /// Per-destination coalescing of non-batch data ops (one destination).
    /// `Reply` and nested `Batch` are rejected by [`encode`]/[`decode`].
    /// The op count is bounded on the **receiver** only: [`decode`]
    /// refuses more than [`MAX_BATCH_OPS`], while construction and
    /// [`encode`] accept any `Vec` (review B64-65-R05 — no constructor
    /// enforces the cap). No shipped sender builds a `Batch` today; the
    /// story that activates one (M6's lock/unlock hops, L3) owns a
    /// construction-side bound, ADR-first, before the first send.
    Batch { ops: Vec<Op<'a>> },
    /// Routed back to `token.origin_cell()`; returns one data-op credit.
    Reply { token: FabricToken, outcome: Outcome<'a> },
    /// An accepted socket the origin cell hands to `to` (ADR-0128): the
    /// fd is process-global, the adopter registers it as its own accept
    /// and answers `Reply { Ok }` to return the credit. A data op for
    /// credit purposes; never nested in a `Batch` (a control op, one
    /// canonical shape).
    AdoptConn { token: FabricToken, fd: u32 },
}

impl Op<'_> {
    fn header_flags(&self) -> u16 {
        match self {
            Op::Apply { program: true, .. } | Op::ApplyNs { program: true, .. } => FLAG_PROGRAM,
            _ => 0,
        }
    }

    fn opcode(&self) -> u8 {
        match self {
            Op::Read { .. } => OP_READ,
            Op::Write { .. } => OP_WRITE,
            Op::Apply { .. } => OP_APPLY,
            Op::ApplyNs { .. } => OP_APPLY_NS,
            Op::Batch { .. } => OP_BATCH,
            Op::Reply { .. } => OP_REPLY,
            Op::AdoptConn { .. } => OP_ADOPT_CONN,
        }
    }
}

/// Typed decode failure — the full set of ways arbitrary bytes can fail to
/// be a v0 frame.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum CodecError {
    /// Input ends before the header or declared payload length.
    Truncated,
    /// Frame version is not [`CODEC_VERSION`].
    UnknownVersion(u8),
    /// Unrecognized opcode.
    UnknownOp(u8),
    /// Reserved header flags were non-zero.
    ReservedFlags(u16),
    /// The program flag on an opcode that has no program origin.
    FlagNotApplicable(u8),
    /// Bytes remain after a complete frame (or inside a payload).
    TrailingBytes,
    /// Malformed varint.
    BadVarint,
    /// Slot value outside `0..16384`.
    InvalidSlot(u16),
    /// Unknown [`WriteFlags`] bits.
    InvalidWriteFlags(u8),
    /// Invalid tag byte (expire/outcome/bool).
    InvalidTag(u8),
    /// `Apply`/`ApplyNs` declared more than [`MAX_APPLY_ARGS`] arguments.
    TooManyArgs(u64),
    /// `ApplyNs` named a default namespace (`ns < 16`). Defaults ride
    /// [`Op::Apply`] — one canonical encoding per op (ADR-0015 D1).
    ApplyNsDefault(u32),
    /// `Batch` declared more than [`MAX_BATCH_OPS`] ops.
    TooManyBatchOps(u64),
    /// `Batch` nested inside `Batch`.
    NestedBatch,
    /// `Reply` nested inside `Batch`.
    ReplyInBatch,
    /// `AdoptConn` nested inside `Batch` (ADR-0128: one canonical shape).
    AdoptConnInBatch,
}

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CodecError::Truncated => write!(f, "frame truncated"),
            CodecError::UnknownVersion(v) => write!(f, "unknown codec version {v}"),
            CodecError::UnknownOp(op) => write!(f, "unknown opcode {op}"),
            CodecError::ReservedFlags(flags) => write!(f, "reserved header flags {flags:#06x}"),
            CodecError::FlagNotApplicable(op) => write!(f, "program flag on opcode {op}"),
            CodecError::TrailingBytes => write!(f, "trailing bytes after frame"),
            CodecError::BadVarint => write!(f, "malformed varint"),
            CodecError::InvalidSlot(slot) => write!(f, "slot {slot} out of range"),
            CodecError::InvalidWriteFlags(bits) => write!(f, "invalid write flags {bits:#04x}"),
            CodecError::InvalidTag(tag) => write!(f, "invalid tag byte {tag}"),
            CodecError::TooManyArgs(n) => write!(f, "apply args {n} > {MAX_APPLY_ARGS}"),
            CodecError::ApplyNsDefault(ns) => {
                write!(f, "apply-ns namespace {ns} is a default (< {APPLY_NS_MIN})")
            }
            CodecError::TooManyBatchOps(n) => write!(f, "batch ops {n} > {MAX_BATCH_OPS}"),
            CodecError::NestedBatch => write!(f, "batch nested inside batch"),
            CodecError::ReplyInBatch => write!(f, "reply nested inside batch"),
            CodecError::AdoptConnInBatch => write!(f, "adopt-conn nested inside batch"),
        }
    }
}

impl std::error::Error for CodecError {}

/// Appends the complete encoded frame for `op` to `out`.
///
/// # Panics
///
/// Panics if a `Batch` nests another `Batch`, a `Reply` or an
/// `AdoptConn`, or if an
/// `ApplyNs` names a default namespace (`ns < 16` — defaults ride
/// `Op::Apply`; ADR-0015 D1); both are rejected before any bytes are
/// written, mirroring what [`decode`] refuses. Also panics if a payload
/// exceeds `u32::MAX` bytes (not reachable with in-contract key/value
/// sizes).
pub fn encode(op: &Op<'_>, out: &mut Vec<u8>) {
    if let Op::Batch { ops } = op {
        for nested in ops {
            match nested {
                Op::Batch { .. } => panic!("Batch must not nest Batch (codec v0)"),
                Op::Reply { .. } => panic!("Batch must not nest Reply (codec v0)"),
                Op::AdoptConn { .. } => panic!("Batch must not nest AdoptConn (ADR-0128)"),
                _ => {}
            }
        }
    }
    if let Op::ApplyNs { ns, .. } = op {
        assert!(*ns >= APPLY_NS_MIN, "ApplyNs ns {ns} is a default namespace (rides Op::Apply)");
    }
    let start = begin_frame(op, out);
    encode_payload(op, out);
    finish_frame(out, start);
}

/// Writes the header with a `len` placeholder; returns the frame start.
fn begin_frame(op: &Op<'_>, out: &mut Vec<u8>) -> usize {
    let start = out.len();
    out.extend_from_slice(&[CODEC_VERSION, op.opcode()]);
    out.extend_from_slice(&op.header_flags().to_le_bytes());
    out.extend_from_slice(&[0; 4]); // len placeholder
    start
}

/// Patches the `len` field once the payload is written.
fn finish_frame(out: &mut [u8], start: usize) {
    let len = out.len() - start - HEADER_LEN;
    let len = u32::try_from(len).expect("fabric frame payload exceeds u32::MAX");
    out[start + 4..start + 8].copy_from_slice(&len.to_le_bytes());
}

/// A `Batch` payload is a loop of framed leaves — one level, never a
/// self-call (ADR-0125 A4: encoder and decoder are both two-level walks).
fn encode_payload(op: &Op<'_>, out: &mut Vec<u8>) {
    let Op::Batch { ops } = op else { return encode_leaf_payload(op, out) };
    varint::encode_u64(ops.len() as u64, out);
    for nested in ops {
        let start = begin_frame(nested, out);
        encode_leaf_payload(nested, out);
        finish_frame(out, start);
    }
}

fn encode_leaf_payload(op: &Op<'_>, out: &mut Vec<u8>) {
    match op {
        Op::Read { token, slot, key } => {
            out.extend_from_slice(&token.0.to_le_bytes());
            out.extend_from_slice(&slot.get().to_le_bytes());
            encode_bytes(key, out);
        }
        Op::Write { token, slot, key, value, expire_at, flags } => {
            out.extend_from_slice(&token.0.to_le_bytes());
            out.extend_from_slice(&slot.get().to_le_bytes());
            out.push(flags.bits());
            match expire_at {
                None => out.push(EXPIRE_NONE),
                Some(at) => {
                    out.push(EXPIRE_AT);
                    out.extend_from_slice(&at.0.to_le_bytes());
                }
            }
            encode_bytes(key, out);
            encode_bytes(value, out);
        }
        Op::Apply { token, slot, cmd, args, .. } => {
            out.extend_from_slice(&token.0.to_le_bytes());
            out.extend_from_slice(&slot.get().to_le_bytes());
            out.push(*cmd);
            varint::encode_u64(args.len() as u64, out);
            for arg in args.as_slice() {
                encode_bytes(arg, out);
            }
        }
        Op::ApplyNs { token, slot, cmd, ns, args, .. } => {
            out.extend_from_slice(&token.0.to_le_bytes());
            out.extend_from_slice(&slot.get().to_le_bytes());
            out.push(*cmd);
            out.extend_from_slice(&ns.to_le_bytes());
            varint::encode_u64(args.len() as u64, out);
            for arg in args.as_slice() {
                encode_bytes(arg, out);
            }
        }
        Op::Batch { .. } => unreachable!("encode refused a nested batch before writing"),
        Op::Reply { token, outcome } => {
            out.extend_from_slice(&token.0.to_le_bytes());
            encode_outcome(outcome, out);
        }
        Op::AdoptConn { token, fd } => {
            out.extend_from_slice(&token.0.to_le_bytes());
            out.extend_from_slice(&fd.to_le_bytes());
        }
    }
}

fn encode_bytes(bytes: &[u8], out: &mut Vec<u8>) {
    varint::encode_u64(bytes.len() as u64, out);
    out.extend_from_slice(bytes);
}

fn encode_outcome(outcome: &Outcome<'_>, out: &mut Vec<u8>) {
    match outcome {
        Outcome::Ok => out.push(OUTCOME_OK),
        Outcome::Bytes(bytes) => {
            out.push(OUTCOME_BYTES);
            encode_bytes(bytes, out);
        }
        Outcome::Int(v) => {
            out.push(OUTCOME_INT);
            out.extend_from_slice(&v.to_le_bytes());
        }
        Outcome::Nil => out.push(OUTCOME_NIL),
        Outcome::Bool(b) => {
            out.push(OUTCOME_BOOL);
            out.push(u8::from(*b));
        }
        Outcome::Err(code) => {
            out.push(OUTCOME_ERR);
            out.extend_from_slice(&code.to_u16().to_le_bytes());
        }
    }
}

/// Decodes exactly one frame; all byte payloads borrow from `frame`.
///
/// Total over arbitrary input: every failure is a typed [`CodecError`] —
/// unknown versions/opcodes, truncated frames, lengths exceeding the buffer,
/// reserved flags, and trailing bytes are all rejected.
///
/// # Errors
///
/// Returns the first [`CodecError`] encountered, including
/// [`CodecError::TrailingBytes`] when `frame` extends past the encoded frame.
pub fn decode(frame: &[u8]) -> Result<Op<'_>, CodecError> {
    let (op, used) = decode_frame(frame)?;
    if used != frame.len() {
        return Err(CodecError::TrailingBytes);
    }
    Ok(op)
}

/// Decodes one frame from the front of `buf`, returning the op and the
/// bytes consumed — the transport-packing entry (M0-R1): a ring slot may
/// carry several concatenated frames, each self-delimiting via the header
/// `len`. The frame *format* is exactly [`decode`]'s; only the
/// one-frame-per-buffer expectation is relaxed.
///
/// # Errors
/// Same conditions as [`decode`], except trailing bytes are the caller's
/// remaining frames, not an error.
pub fn decode_prefix(buf: &[u8]) -> Result<(Op<'_>, usize), CodecError> {
    decode_frame(buf)
}

/// Decodes one frame from the front of `buf`; returns the op and the
/// bytes consumed. A `Batch` payload is a loop over [`decode_leaf`] —
/// one level, never a self-call, so the decoder is iterative as
/// INFINITY_STYLE requires (ADR-0125 A4); the codec's nesting rule is
/// enforced where a leaf meets `OP_BATCH`.
fn decode_frame(buf: &[u8]) -> Result<(Op<'_>, usize), CodecError> {
    let header = decode_header(buf)?;
    if header.opcode != OP_BATCH {
        let op = decode_leaf(header)?;
        return Ok((op, header.end));
    }
    let mut reader = Reader { buf: header.payload };
    let count = reader.varint()?;
    if count > MAX_BATCH_OPS as u64 {
        return Err(CodecError::TooManyBatchOps(count));
    }
    let mut ops = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let inner = decode_header(reader.buf)?;
        if inner.opcode == OP_BATCH {
            return Err(CodecError::NestedBatch);
        }
        let nested_op = decode_leaf(inner)?;
        if matches!(nested_op, Op::Reply { .. }) {
            return Err(CodecError::ReplyInBatch);
        }
        if matches!(nested_op, Op::AdoptConn { .. }) {
            return Err(CodecError::AdoptConnInBatch);
        }
        reader.buf = &reader.buf[inner.end..];
        ops.push(nested_op);
    }
    if !reader.buf.is_empty() {
        return Err(CodecError::TrailingBytes);
    }
    Ok((Op::Batch { ops }, header.end))
}

/// One validated frame header: the opcode, the program flag and the
/// payload it delimits; `end` is the frame's total length in the buffer.
#[derive(Clone, Copy)]
struct FrameHeader<'a> {
    opcode: u8,
    program: bool,
    payload: &'a [u8],
    end: usize,
}

fn decode_header(buf: &[u8]) -> Result<FrameHeader<'_>, CodecError> {
    if buf.len() < HEADER_LEN {
        return Err(CodecError::Truncated);
    }
    let version = buf[0];
    if version != CODEC_VERSION {
        return Err(CodecError::UnknownVersion(version));
    }
    let opcode = buf[1];
    let flags = u16::from_le_bytes([buf[2], buf[3]]);
    if flags & !FLAG_PROGRAM != 0 {
        return Err(CodecError::ReservedFlags(flags));
    }
    let program = flags == FLAG_PROGRAM;
    if program && !matches!(opcode, OP_APPLY | OP_APPLY_NS) {
        return Err(CodecError::FlagNotApplicable(opcode));
    }
    let len = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;
    let end = HEADER_LEN.checked_add(len).ok_or(CodecError::Truncated)?;
    let payload = buf.get(HEADER_LEN..end).ok_or(CodecError::Truncated)?;
    Ok(FrameHeader { opcode, program, payload, end })
}

/// Decodes a non-batch payload; the caller has already routed `OP_BATCH`.
fn decode_leaf(header: FrameHeader<'_>) -> Result<Op<'_>, CodecError> {
    let FrameHeader { opcode, program, payload, .. } = header;
    let mut reader = Reader { buf: payload };
    let op = match opcode {
        OP_READ => {
            let token = reader.token()?;
            let slot = reader.slot()?;
            let key = reader.bytes()?;
            Op::Read { token, slot, key }
        }
        OP_WRITE => {
            let token = reader.token()?;
            let slot = reader.slot()?;
            let flags = WriteFlags::from_bits(reader.u8()?).ok_or_else(|| {
                CodecError::InvalidWriteFlags(payload[10]) // byte just read
            })?;
            let expire_at = match reader.u8()? {
                EXPIRE_NONE => None,
                EXPIRE_AT => Some(Nanos(reader.u64_le()?)),
                tag => return Err(CodecError::InvalidTag(tag)),
            };
            let key = reader.bytes()?;
            let value = reader.bytes()?;
            Op::Write { token, slot, key, value, expire_at, flags }
        }
        OP_APPLY => {
            let token = reader.token()?;
            let slot = reader.slot()?;
            let cmd = reader.u8()?;
            let args = reader.apply_args()?;
            Op::Apply { token, slot, cmd, args, program }
        }
        OP_APPLY_NS => {
            let token = reader.token()?;
            let slot = reader.slot()?;
            let cmd = reader.u8()?;
            let ns = reader.u32_le()?;
            if ns < APPLY_NS_MIN {
                return Err(CodecError::ApplyNsDefault(ns));
            }
            let args = reader.apply_args()?;
            Op::ApplyNs { token, slot, cmd, ns, args, program }
        }
        OP_BATCH => return Err(CodecError::NestedBatch),
        OP_REPLY => {
            let token = reader.token()?;
            let outcome = reader.outcome()?;
            Op::Reply { token, outcome }
        }
        OP_ADOPT_CONN => {
            let token = reader.token()?;
            let fd = reader.u32_le()?;
            Op::AdoptConn { token, fd }
        }
        other => return Err(CodecError::UnknownOp(other)),
    };
    if !reader.buf.is_empty() {
        return Err(CodecError::TrailingBytes);
    }
    Ok(op)
}

/// Cursor over a frame payload; every read is bounds-checked and returns a
/// typed error on truncation.
struct Reader<'a> {
    buf: &'a [u8],
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], CodecError> {
        if self.buf.len() < n {
            return Err(CodecError::Truncated);
        }
        let (head, rest) = self.buf.split_at(n);
        self.buf = rest;
        Ok(head)
    }

    fn u8(&mut self) -> Result<u8, CodecError> {
        Ok(self.take(1)?[0])
    }

    fn u16_le(&mut self) -> Result<u16, CodecError> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn u32_le(&mut self) -> Result<u32, CodecError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn u64_le(&mut self) -> Result<u64, CodecError> {
        let b = self.take(8)?;
        Ok(u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]))
    }

    fn i64_le(&mut self) -> Result<i64, CodecError> {
        Ok(self.u64_le()? as i64)
    }

    fn varint(&mut self) -> Result<u64, CodecError> {
        let (value, used) = varint::decode_u64(self.buf).ok_or(CodecError::BadVarint)?;
        self.buf = &self.buf[used..];
        Ok(value)
    }

    fn bytes(&mut self) -> Result<&'a [u8], CodecError> {
        let len = self.varint()?;
        let len = usize::try_from(len).map_err(|_| CodecError::Truncated)?;
        self.take(len)
    }

    /// `argc` then `argc` byte slices. The spill table (ADR-0120 D2) is
    /// sized only after the frame proves it can carry that many slices
    /// — every slice costs at least its one-byte length — so a hostile
    /// argc is `Truncated`, never an allocation.
    fn apply_args(&mut self) -> Result<ApplyArgs<'a>, CodecError> {
        let argc = self.varint()?;
        if argc > MAX_APPLY_ARGS as u64 {
            return Err(CodecError::TooManyArgs(argc));
        }
        let argc = argc as usize;
        if argc > self.buf.len() {
            return Err(CodecError::Truncated);
        }
        let mut inline: [&'a [u8]; MAX_INLINE_APPLY_ARGS] = [&[]; MAX_INLINE_APPLY_ARGS];
        let spill = if argc <= MAX_INLINE_APPLY_ARGS {
            for arg in inline.iter_mut().take(argc) {
                *arg = self.bytes()?;
            }
            Vec::new()
        } else {
            let mut spill = Vec::with_capacity(argc);
            for _ in 0..argc {
                spill.push(self.bytes()?);
            }
            spill
        };
        // argc <= MAX_APPLY_ARGS < 65536, so the cast is lossless.
        Ok(ApplyArgs { inline, len: argc as u16, spill })
    }

    fn token(&mut self) -> Result<FabricToken, CodecError> {
        Ok(FabricToken(self.u64_le()?))
    }

    fn slot(&mut self) -> Result<KeySlot, CodecError> {
        let raw = self.u16_le()?;
        KeySlot::new(raw).ok_or(CodecError::InvalidSlot(raw))
    }

    fn outcome(&mut self) -> Result<Outcome<'a>, CodecError> {
        match self.u8()? {
            OUTCOME_OK => Ok(Outcome::Ok),
            OUTCOME_BYTES => Ok(Outcome::Bytes(self.bytes()?)),
            OUTCOME_INT => Ok(Outcome::Int(self.i64_le()?)),
            OUTCOME_NIL => Ok(Outcome::Nil),
            OUTCOME_BOOL => match self.u8()? {
                0 => Ok(Outcome::Bool(false)),
                1 => Ok(Outcome::Bool(true)),
                tag => Err(CodecError::InvalidTag(tag)),
            },
            OUTCOME_ERR => Ok(Outcome::Err(ErrCode::from_u16(self.u16_le()?))),
            tag => Err(CodecError::InvalidTag(tag)),
        }
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use inf_foundation::CellId;
    use proptest::prelude::*;

    use super::*;

    fn slot(raw: u16) -> KeySlot {
        KeySlot::new(raw).unwrap()
    }

    fn token(origin: u16, seq: u64) -> FabricToken {
        FabricToken::new(CellId(origin), seq)
    }

    fn round_trip(op: &Op<'_>) -> Vec<u8> {
        let mut out = Vec::new();
        encode(op, &mut out);
        let decoded = decode(&out).expect("round trip decode");
        assert_eq!(&decoded, op);
        out
    }

    #[test]
    fn round_trip_every_variant() {
        round_trip(&Op::Read { token: token(3, 9), slot: slot(42), key: b"user:1" });
        round_trip(&Op::Write {
            token: token(0, u64::from(u32::MAX)),
            slot: slot(16383),
            key: b"k",
            value: &[0u8; 300],
            expire_at: Some(Nanos::from_secs(30)),
            flags: WriteFlags::IF_ABSENT | WriteFlags::GET_OLD,
        });
        round_trip(&Op::Write {
            token: token(1, 0),
            slot: slot(0),
            key: b"",
            value: b"",
            expire_at: None,
            flags: WriteFlags::NONE,
        });
        for program in [false, true] {
            round_trip(&Op::Apply {
                token: token(7, 1),
                slot: slot(100),
                cmd: 0xEE,
                args: ApplyArgs::new(&[b"a".as_slice(), b"".as_slice(), b"ccc".as_slice()])
                    .unwrap(),
                program,
            });
            round_trip(&Op::ApplyNs {
                token: token(7, 2),
                slot: slot(100),
                cmd: 0x02,
                ns: 16,
                args: ApplyArgs::new(&[b"a".as_slice(), b"".as_slice(), b"ccc".as_slice()])
                    .unwrap(),
                program,
            });
        }
        round_trip(&Op::Batch {
            ops: vec![
                Op::Read { token: token(2, 5), slot: slot(7), key: b"x" },
                Op::Apply {
                    token: token(2, 6),
                    slot: slot(8),
                    cmd: 1,
                    args: ApplyArgs::EMPTY,
                    program: true,
                },
                Op::ApplyNs {
                    token: token(2, 7),
                    slot: slot(9),
                    cmd: 2,
                    ns: u32::MAX,
                    program: false,
                    args: ApplyArgs::EMPTY,
                },
            ],
        });
        round_trip(&Op::Batch { ops: Vec::new() });
        for outcome in [
            Outcome::Ok,
            Outcome::Bytes(b"value"),
            Outcome::Int(-42),
            Outcome::Int(i64::MIN),
            Outcome::Nil,
            Outcome::Bool(true),
            Outcome::Bool(false),
            Outcome::Err(ErrCode::WrongType),
            Outcome::Err(ErrCode::Unknown(999)),
        ] {
            round_trip(&Op::Reply { token: token(9, 1 << 40), outcome });
        }
    }

    #[test]
    fn rejects_malformed_frames() {
        let mut good = Vec::new();
        encode(&Op::Read { token: token(1, 2), slot: slot(3), key: b"key" }, &mut good);

        assert_eq!(decode(&[]), Err(CodecError::Truncated));
        assert_eq!(decode(&good[..7]), Err(CodecError::Truncated));
        assert_eq!(decode(&good[..good.len() - 1]), Err(CodecError::Truncated));

        let mut bad_version = good.clone();
        bad_version[0] = 1;
        assert_eq!(decode(&bad_version), Err(CodecError::UnknownVersion(1)));

        let mut bad_op = good.clone();
        bad_op[1] = 0;
        assert_eq!(decode(&bad_op), Err(CodecError::UnknownOp(0)));
        bad_op[1] = 8; // 6 is OP_APPLY_NS (M2-S08), 7 OP_ADOPT_CONN (ADR-0128)
        assert_eq!(decode(&bad_op), Err(CodecError::UnknownOp(8)));

        // Bit 0 is the Apply-only program mark (ADR-0115); on a Read it is
        // not applicable, and every higher bit stays reserved.
        let mut bad_flags = good.clone();
        bad_flags[2] = 1;
        assert_eq!(decode(&bad_flags), Err(CodecError::FlagNotApplicable(OP_READ)));
        bad_flags[2] = 2;
        assert_eq!(decode(&bad_flags), Err(CodecError::ReservedFlags(2)));

        let mut trailing = good.clone();
        trailing.push(0);
        assert_eq!(decode(&trailing), Err(CodecError::TrailingBytes));

        // Slot 16384 is out of range: bytes 8..16 token, 16..18 slot.
        let mut bad_slot = good.clone();
        bad_slot[16..18].copy_from_slice(&16384u16.to_le_bytes());
        assert_eq!(decode(&bad_slot), Err(CodecError::InvalidSlot(16384)));

        // Declared length larger than the buffer.
        let mut huge_len = good;
        huge_len[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(decode(&huge_len), Err(CodecError::Truncated));
    }

    #[test]
    fn rejects_invalid_payload_fields() {
        let mut frame = Vec::new();
        encode(
            &Op::Write {
                token: token(1, 1),
                slot: slot(1),
                key: b"k",
                value: b"v",
                expire_at: None,
                flags: WriteFlags::NONE,
            },
            &mut frame,
        );
        // Payload layout: token(8) slot(2) flags(1) expire_tag(1) ...
        let mut bad_wflags = frame.clone();
        bad_wflags[HEADER_LEN + 10] = 0xFF;
        assert_eq!(decode(&bad_wflags), Err(CodecError::InvalidWriteFlags(0xFF)));

        let mut bad_expire = frame;
        bad_expire[HEADER_LEN + 11] = 9;
        assert_eq!(decode(&bad_expire), Err(CodecError::InvalidTag(9)));

        let mut reply = Vec::new();
        encode(&Op::Reply { token: token(1, 1), outcome: Outcome::Bool(true) }, &mut reply);
        let mut bad_bool = reply.clone();
        *bad_bool.last_mut().unwrap() = 2;
        assert_eq!(decode(&bad_bool), Err(CodecError::InvalidTag(2)));
        let mut bad_outcome = reply;
        bad_outcome[HEADER_LEN + 8] = 77;
        assert_eq!(decode(&bad_outcome), Err(CodecError::InvalidTag(77)));
    }

    #[test]
    fn rejects_invalid_batches() {
        // Hand-build a batch nesting a reply: count=1 then a Reply frame.
        let mut inner = Vec::new();
        encode(&Op::Reply { token: token(1, 1), outcome: Outcome::Ok }, &mut inner);
        let mut payload = Vec::new();
        varint::encode_u64(1, &mut payload);
        payload.extend_from_slice(&inner);
        let frame = frame_with(OP_BATCH, &payload);
        assert_eq!(decode(&frame), Err(CodecError::ReplyInBatch));

        // Batch nesting a batch.
        let mut inner = Vec::new();
        encode(&Op::Batch { ops: Vec::new() }, &mut inner);
        let mut payload = Vec::new();
        varint::encode_u64(1, &mut payload);
        payload.extend_from_slice(&inner);
        let frame = frame_with(OP_BATCH, &payload);
        assert_eq!(decode(&frame), Err(CodecError::NestedBatch));

        // Count over the cap.
        let mut payload = Vec::new();
        varint::encode_u64(MAX_BATCH_OPS as u64 + 1, &mut payload);
        let frame = frame_with(OP_BATCH, &payload);
        assert_eq!(decode(&frame), Err(CodecError::TooManyBatchOps(MAX_BATCH_OPS as u64 + 1)));
    }

    /// ADR-0115: the program mark is header bit 0 on `Apply`/`ApplyNs`,
    /// canonical (set iff `program`), refused on every other opcode and
    /// alongside any reserved bit.
    #[test]
    fn program_flag_is_canonical_and_apply_only() {
        let op = Op::Apply {
            token: token(2, 0x91),
            slot: slot(5),
            cmd: 0x23,
            args: ApplyArgs::new(&[b"ab".as_slice()]).unwrap(),
            program: true,
        };
        let golden: &[u8] = &[
            0, 3, 1, 0, 15, 0, 0, 0, // header: v0, Apply, flags 1 (program), len 15
            0x91, 0, 0, 0, 0, 0, 2, 0, // token: seq 0x91, origin 2
            5, 0,    // slot
            0x23, // cmd
            1,    // argc
            2, b'a', b'b', // arg0
        ];
        let mut out = Vec::new();
        encode(&op, &mut out);
        assert_eq!(out.as_slice(), golden);
        assert_eq!(decode(golden), Ok(op));
        let mut unmarked = golden.to_vec();
        unmarked[2] = 0;
        assert!(matches!(decode(&unmarked), Ok(Op::Apply { program: false, .. })));
        // Bit 0 on a Read / Reply / Batch frame: not applicable.
        for op in [
            Op::Read { token: token(1, 2), slot: slot(3), key: b"key" },
            Op::Reply { token: token(1, 1), outcome: Outcome::Ok },
            Op::Batch { ops: Vec::new() },
        ] {
            let mut frame = Vec::new();
            encode(&op, &mut frame);
            frame[2] = 1;
            assert_eq!(decode(&frame), Err(CodecError::FlagNotApplicable(frame[1])), "{op:?}");
        }
        // A reserved bit beside the program bit is still reserved.
        let mut reserved = golden.to_vec();
        reserved[2] = 3;
        assert_eq!(decode(&reserved), Err(CodecError::ReservedFlags(3)));
    }

    #[test]
    fn rejects_too_many_apply_args() {
        let mut payload = Vec::new();
        payload.extend_from_slice(&token(1, 1).0.to_le_bytes());
        payload.extend_from_slice(&1u16.to_le_bytes());
        payload.push(0); // cmd
        varint::encode_u64(MAX_APPLY_ARGS as u64 + 1, &mut payload);
        let frame = frame_with(OP_APPLY, &payload);
        assert_eq!(decode(&frame), Err(CodecError::TooManyArgs(MAX_APPLY_ARGS as u64 + 1)));

        // Same cap on the ApplyNs path (argc sits after the ns word).
        let mut payload = Vec::new();
        payload.extend_from_slice(&token(1, 1).0.to_le_bytes());
        payload.extend_from_slice(&1u16.to_le_bytes());
        payload.push(0); // cmd
        payload.extend_from_slice(&16u32.to_le_bytes()); // ns
        varint::encode_u64(MAX_APPLY_ARGS as u64 + 1, &mut payload);
        let frame = frame_with(OP_APPLY_NS, &payload);
        assert_eq!(decode(&frame), Err(CodecError::TooManyArgs(MAX_APPLY_ARGS as u64 + 1)));
    }

    /// M2-S08 AC: `ApplyNs` round-trips at the argument extremes (0, 1, and
    /// [`MAX_APPLY_ARGS`] args) and at both ns boundaries (16, `u32::MAX`).
    #[test]
    fn apply_ns_round_trips_arg_extremes() {
        round_trip(&Op::ApplyNs {
            token: token(3, 9),
            slot: slot(42),
            cmd: 0x03,
            ns: 16,
            program: false,
            args: ApplyArgs::EMPTY,
        });
        round_trip(&Op::ApplyNs {
            token: token(0, u64::from(u32::MAX)),
            slot: slot(16383),
            cmd: 0x02,
            ns: u32::MAX,
            program: false,
            args: ApplyArgs::new(&[b"key".as_slice()]).unwrap(),
        });
        let owned: Vec<Vec<u8>> = (0..MAX_APPLY_ARGS).map(|i| vec![i as u8; i]).collect();
        let slices: Vec<&[u8]> = owned.iter().map(Vec::as_slice).collect();
        round_trip(&Op::ApplyNs {
            token: token(1, 0),
            slot: slot(0),
            cmd: 0,
            ns: 1 << 20,
            program: false,
            args: ApplyArgs::new(&slices).unwrap(),
        });
    }

    /// Review of 2026-08-30, F-L17-15 (ADR-0120): the apply bound follows
    /// the client parser, not the inline array — an argv one past the
    /// inline width, and one at the full bound, ship whole on both opcodes
    /// (byte-exact round trip; the spilled slices decode to the same
    /// values). Red on the 16-slice codec: `ApplyArgs::new` answered
    /// `None` and `decode` answered `TooManyArgs(17)`.
    #[test]
    fn apply_args_beyond_the_inline_width_round_trip() {
        for argc in [17, 64, MAX_APPLY_ARGS] {
            let owned: Vec<Vec<u8>> = (0..argc).map(|i| vec![(i % 251) as u8; i % 7]).collect();
            let slices: Vec<&[u8]> = owned.iter().map(Vec::as_slice).collect();
            let args = ApplyArgs::new(&slices).unwrap_or_else(|| panic!("{argc} args refused"));
            assert_eq!(args.len(), argc);
            assert_eq!(args.as_slice(), slices.as_slice());
            let frame = round_trip(&Op::Apply {
                token: token(2, 7),
                slot: slot(9),
                cmd: 0x13,
                args: args.clone(),
                program: false,
            });
            let decoded = decode(&frame).expect("decodes");
            let Op::Apply { args: back, .. } = decoded else { panic!("opcode") };
            assert_eq!(back.as_slice(), slices.as_slice(), "argc {argc}");
            round_trip(&Op::ApplyNs {
                token: token(2, 8),
                slot: slot(9),
                cmd: 0x03,
                ns: 16,
                args,
                program: true,
            });
        }
    }

    /// The decoder's spill is bounded by the frame: an argc past the bytes
    /// that could carry it is `Truncated` before any allocation (a hostile
    /// argc never sizes a table — L9).
    #[test]
    fn hostile_argc_is_truncated_before_any_spill() {
        let mut payload = Vec::new();
        payload.extend_from_slice(&token(1, 1).0.to_le_bytes());
        payload.extend_from_slice(&1u16.to_le_bytes());
        payload.push(0); // cmd
        varint::encode_u64(MAX_APPLY_ARGS as u64, &mut payload);
        payload.push(0); // one empty arg; the other 1023 are missing
        let frame = frame_with(OP_APPLY, &payload);
        assert_eq!(decode(&frame), Err(CodecError::Truncated));
    }

    /// ADR-0015 D1: defaults ride `Op::Apply` — an `ApplyNs` frame naming a
    /// default namespace (`ns < 16`) has no canonical meaning and is a typed
    /// decode error, not a silent alias.
    #[test]
    fn rejects_apply_ns_default_namespace() {
        for ns in [0u32, 15] {
            let mut payload = Vec::new();
            payload.extend_from_slice(&token(1, 1).0.to_le_bytes());
            payload.extend_from_slice(&1u16.to_le_bytes());
            payload.push(0x02); // cmd
            payload.extend_from_slice(&ns.to_le_bytes());
            varint::encode_u64(0, &mut payload); // argc
            let frame = frame_with(OP_APPLY_NS, &payload);
            assert_eq!(decode(&frame), Err(CodecError::ApplyNsDefault(ns)));
        }
    }

    /// Golden wire layout for `ApplyNs` (M2-S08, ADR-0015 D1): pins the
    /// byte-level encoding — `token(8) slot(2) cmd(1) ns(4, LE) argc(varint)
    /// args…` — so drift is caught as a diff here, not on a peer.
    #[test]
    fn apply_ns_golden_wire_layout() {
        let op = Op::ApplyNs {
            token: token(2, 0x91),
            slot: slot(5),
            cmd: 0x23,
            ns: 16,
            program: false,
            args: ApplyArgs::new(&[b"ab".as_slice()]).unwrap(),
        };
        let golden: &[u8] = &[
            0, 6, 0, 0, 19, 0, 0, 0, // header: v0, ApplyNs, flags 0, len 19
            145, 0, 0, 0, 0, 0, 2, 0, // token: {origin:16, seq:48} = cell 2, seq 0x91
            5, 0,    // slot
            0x23, // cmd {0:4 | proto:4}
            16, 0, 0, 0, // ns (u32 LE) — first non-default id
            1, // argc (varint)
            2, // arg0 len (varint)
            b'a', b'b', // arg0
        ];
        let mut out = Vec::new();
        encode(&op, &mut out);
        assert_eq!(out.as_slice(), golden);
        assert_eq!(decode(golden), Ok(op));
    }

    #[test]
    #[should_panic(expected = "default namespace")]
    fn encode_rejects_apply_ns_default() {
        let mut out = Vec::new();
        encode(
            &Op::ApplyNs {
                token: token(0, 0),
                slot: slot(0),
                cmd: 0,
                ns: 15,
                program: false,
                args: ApplyArgs::EMPTY,
            },
            &mut out,
        );
    }

    /// ADR-0128: the hand-off op round-trips and never rides a batch.
    #[test]
    fn adopt_conn_round_trips_and_is_refused_in_a_batch() {
        round_trip(&Op::AdoptConn { token: token(2, 77), fd: 4_000_000_001 });
        let mut out = Vec::new();
        encode(&Op::AdoptConn { token: token(0, 1), fd: 9 }, &mut out);
        assert_eq!(out[1], OP_ADOPT_CONN);
        assert_eq!(out.len(), HEADER_LEN + 8 + 4, "token + fd, nothing else");
        // A batch whose one op is the adopt frame, spliced by hand (the
        // encoder refuses to build it).
        let mut spliced = vec![CODEC_VERSION, OP_BATCH, 0, 0, 0, 0, 0, 0, 1];
        spliced.extend_from_slice(&out);
        let len = (spliced.len() - HEADER_LEN) as u32;
        spliced[4..8].copy_from_slice(&len.to_le_bytes());
        assert_eq!(decode(&spliced), Err(CodecError::AdoptConnInBatch));
        // A program mark on the control op is not applicable.
        let mut flagged = out.clone();
        flagged[2] = FLAG_PROGRAM as u8;
        assert_eq!(decode(&flagged), Err(CodecError::FlagNotApplicable(OP_ADOPT_CONN)));
    }

    #[test]
    #[should_panic(expected = "Batch must not nest AdoptConn")]
    fn encode_rejects_adopt_conn_in_batch() {
        let mut out = Vec::new();
        encode(&Op::Batch { ops: vec![Op::AdoptConn { token: token(0, 1), fd: 3 }] }, &mut out);
    }

    #[test]
    #[should_panic(expected = "Batch must not nest Reply")]
    fn encode_rejects_reply_in_batch() {
        let mut out = Vec::new();
        encode(
            &Op::Batch { ops: vec![Op::Reply { token: token(0, 0), outcome: Outcome::Ok }] },
            &mut out,
        );
    }

    fn frame_with(opcode: u8, payload: &[u8]) -> Vec<u8> {
        let mut frame = vec![CODEC_VERSION, opcode, 0, 0];
        frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        frame.extend_from_slice(payload);
        frame
    }

    // ---- proptest: arbitrary op sequences round-trip byte-exact (M0-S10 AC)

    #[derive(Debug, Clone)]
    enum OwnedOp {
        Read {
            token: u64,
            slot: u16,
            key: Vec<u8>,
        },
        Write {
            token: u64,
            slot: u16,
            key: Vec<u8>,
            value: Vec<u8>,
            expire: Option<u64>,
            flags: u8,
        },
        Apply {
            token: u64,
            slot: u16,
            cmd: u8,
            args: Vec<Vec<u8>>,
            program: bool,
        },
        ApplyNs {
            token: u64,
            slot: u16,
            cmd: u8,
            ns: u32,
            args: Vec<Vec<u8>>,
            program: bool,
        },
        Batch {
            ops: Vec<OwnedOp>,
        },
        Reply {
            token: u64,
            outcome: OwnedOutcome,
        },
    }

    #[derive(Debug, Clone)]
    enum OwnedOutcome {
        Ok,
        Bytes(Vec<u8>),
        Int(i64),
        Nil,
        Bool(bool),
        Err(u16),
    }

    impl OwnedOp {
        fn to_op(&self) -> Op<'_> {
            match self {
                OwnedOp::Read { token, slot, key } => {
                    Op::Read { token: FabricToken(*token), slot: KeySlot::new(*slot).unwrap(), key }
                }
                OwnedOp::Write { token, slot, key, value, expire, flags } => Op::Write {
                    token: FabricToken(*token),
                    slot: KeySlot::new(*slot).unwrap(),
                    key,
                    value,
                    expire_at: expire.map(Nanos),
                    flags: WriteFlags::from_bits(*flags).unwrap(),
                },
                OwnedOp::Apply { token, slot, cmd, args, program } => {
                    let slices: Vec<&[u8]> = args.iter().map(Vec::as_slice).collect();
                    Op::Apply {
                        token: FabricToken(*token),
                        slot: KeySlot::new(*slot).unwrap(),
                        cmd: *cmd,
                        args: ApplyArgs::new(&slices).unwrap(),
                        program: *program,
                    }
                }
                OwnedOp::ApplyNs { token, slot, cmd, ns, args, program } => {
                    let slices: Vec<&[u8]> = args.iter().map(Vec::as_slice).collect();
                    Op::ApplyNs {
                        token: FabricToken(*token),
                        slot: KeySlot::new(*slot).unwrap(),
                        cmd: *cmd,
                        ns: *ns,
                        args: ApplyArgs::new(&slices).unwrap(),
                        program: *program,
                    }
                }
                OwnedOp::Batch { ops } => Op::Batch { ops: ops.iter().map(Self::to_op).collect() },
                OwnedOp::Reply { token, outcome } => Op::Reply {
                    token: FabricToken(*token),
                    outcome: match outcome {
                        OwnedOutcome::Ok => Outcome::Ok,
                        OwnedOutcome::Bytes(b) => Outcome::Bytes(b),
                        OwnedOutcome::Int(v) => Outcome::Int(*v),
                        OwnedOutcome::Nil => Outcome::Nil,
                        OwnedOutcome::Bool(b) => Outcome::Bool(*b),
                        OwnedOutcome::Err(raw) => Outcome::Err(ErrCode::from_u16(*raw)),
                    },
                },
            }
        }
    }

    fn leaf_op() -> impl Strategy<Value = OwnedOp> {
        let bytes = prop::collection::vec(any::<u8>(), 0..64);
        prop_oneof![
            (any::<u64>(), 0..16384u16, bytes.clone())
                .prop_map(|(token, slot, key)| { OwnedOp::Read { token, slot, key } }),
            (any::<u64>(), 0..16384u16, bytes.clone(), bytes.clone(), any::<Option<u64>>(), 0..8u8)
                .prop_map(|(token, slot, key, value, expire, flags)| OwnedOp::Write {
                    token,
                    slot,
                    key,
                    value,
                    expire,
                    flags,
                }),
            (
                any::<u64>(),
                0..16384u16,
                any::<u8>(),
                prop::collection::vec(bytes.clone(), 0..3 * MAX_INLINE_APPLY_ARGS),
                any::<bool>()
            )
                .prop_map(|(token, slot, cmd, args, program)| OwnedOp::Apply {
                    token,
                    slot,
                    cmd,
                    args,
                    program
                }),
            (
                any::<u64>(),
                0..16384u16,
                any::<u8>(),
                16..=u32::MAX, // ns < 16 is not encodable (ADR-0015 D1)
                prop::collection::vec(bytes.clone(), 0..3 * MAX_INLINE_APPLY_ARGS),
                any::<bool>()
            )
                .prop_map(|(token, slot, cmd, ns, args, program)| OwnedOp::ApplyNs {
                    token,
                    slot,
                    cmd,
                    ns,
                    args,
                    program
                }),
            (any::<u64>(), outcome())
                .prop_map(|(token, outcome)| OwnedOp::Reply { token, outcome }),
        ]
    }

    fn outcome() -> impl Strategy<Value = OwnedOutcome> {
        prop_oneof![
            Just(OwnedOutcome::Ok),
            prop::collection::vec(any::<u8>(), 0..64).prop_map(OwnedOutcome::Bytes),
            any::<i64>().prop_map(OwnedOutcome::Int),
            Just(OwnedOutcome::Nil),
            any::<bool>().prop_map(OwnedOutcome::Bool),
            any::<u16>().prop_map(OwnedOutcome::Err),
        ]
    }

    fn data_op() -> impl Strategy<Value = OwnedOp> {
        leaf_op()
            .prop_filter("batch nests data ops only", |op| !matches!(op, OwnedOp::Reply { .. }))
    }

    fn any_op() -> impl Strategy<Value = OwnedOp> {
        prop_oneof![
            4 => leaf_op(),
            1 => prop::collection::vec(data_op(), 0..8).prop_map(|ops| OwnedOp::Batch { ops }),
        ]
    }

    /// Fuzz regression (2026-06-11, first Linux hour): an `Apply` frame whose
    /// second arg length was the non-minimal varint `[0x80, 0x00]` decoded
    /// fine but re-encoded canonically — breaking decode→encode byte-exactness.
    /// Non-minimal varints are now `BadVarint`.
    #[test]
    fn non_minimal_varint_in_frame_is_rejected() {
        let crash: &[u8] = &[
            0, 3, 0, 0, 15, 0, 0, 0, // header: v0, Apply, flags 0, len 15
            0, 0, 145, 0, 0, 0, 2, 0, // token
            0, 0, // slot
            0, // cmd
            2, // argc
            0, // arg0 len (canonical 0)
            128, 0, // arg1 len: NON-MINIMAL encoding of 0
        ];
        assert_eq!(decode(crash), Err(CodecError::BadVarint));
    }

    proptest! {
        /// M0-S10 AC: arbitrary op sequences round-trip the codec byte-exact.
        /// Ignored under Miri: proptest's failure persistence needs `getcwd`,
        /// which Miri isolation forbids — and the codec is safe code whose
        /// totality is fuzz-covered (`fuzz_targets/fabric_codec.rs`); Miri's
        /// scope here is the unsafe ring (M0 §17.2 ladder).
        #[test]
        #[cfg_attr(miri, ignore)]
        fn op_sequences_round_trip_byte_exact(ops in prop::collection::vec(any_op(), 1..16)) {
            let mut stream = Vec::new();
            let mut frame_ends = Vec::new();
            for owned in &ops {
                encode(&owned.to_op(), &mut stream);
                frame_ends.push(stream.len());
            }
            let mut start = 0;
            for (owned, end) in ops.iter().zip(frame_ends) {
                let frame = &stream[start..end];
                let decoded = decode(frame).expect("decode");
                prop_assert_eq!(&decoded, &owned.to_op());
                let mut reencoded = Vec::new();
                encode(&decoded, &mut reencoded);
                prop_assert_eq!(reencoded.as_slice(), frame);
                start = end;
            }
        }

        /// Decode is total: arbitrary bytes never panic.
        #[test]
        #[cfg_attr(miri, ignore)] // see round-trip note: getcwd under isolation
        fn decode_is_total(bytes in prop::collection::vec(any::<u8>(), 0..256)) {
            let _ = decode(&bytes);
        }
    }
}
