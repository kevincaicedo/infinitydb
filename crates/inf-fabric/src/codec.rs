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
//!
//! [`decode`] is **total**: any byte input either parses or returns a typed
//! [`CodecError`] — no panics, no UB (fuzzed by `fuzz/fuzz_targets/
//! fabric_codec.rs`, which runs in the CI fuzz job). Decoding borrows all
//! byte payloads from the input — zero copies (the only decode allocation is
//! the `Vec` of nested ops in `Batch`, flagged as an M1 optimization).

use core::fmt;

use inf_foundation::time::Nanos;
use inf_foundation::{KeySlot, varint};

use crate::msg::FabricToken;

/// Wire version emitted and accepted by this codec.
pub const CODEC_VERSION: u8 = 0;

/// Maximum number of argument slices in an [`Op::Apply`].
pub const MAX_APPLY_ARGS: usize = 16;

/// Maximum number of nested ops in an [`Op::Batch`].
pub const MAX_BATCH_OPS: usize = 256;

const HEADER_LEN: usize = 8;

const OP_READ: u8 = 1;
const OP_WRITE: u8 = 2;
const OP_APPLY: u8 = 3;
const OP_BATCH: u8 = 4;
const OP_REPLY: u8 = 5;

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

/// Argument slices for [`Op::Apply`] — at most [`MAX_APPLY_ARGS`], stored
/// inline (no allocation on encode or decode).
#[derive(Copy, Clone)]
pub struct ApplyArgs<'a> {
    args: [&'a [u8]; MAX_APPLY_ARGS],
    len: u8,
}

impl<'a> ApplyArgs<'a> {
    /// No arguments.
    pub const EMPTY: ApplyArgs<'static> = ApplyArgs { args: [&[]; MAX_APPLY_ARGS], len: 0 };

    /// Builds from a slice of slices; `None` if more than [`MAX_APPLY_ARGS`].
    pub fn new(args: &[&'a [u8]]) -> Option<ApplyArgs<'a>> {
        if args.len() > MAX_APPLY_ARGS {
            return None;
        }
        let mut packed: [&'a [u8]; MAX_APPLY_ARGS] = [&[]; MAX_APPLY_ARGS];
        packed[..args.len()].copy_from_slice(args);
        // Length fits in u8 because MAX_APPLY_ARGS < 256.
        Some(ApplyArgs { args: packed, len: args.len() as u8 })
    }

    /// The argument slices.
    #[inline]
    pub fn as_slice(&self) -> &[&'a [u8]] {
        &self.args[..usize::from(self.len)]
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
    /// `ExecOp`).
    Apply { token: FabricToken, slot: KeySlot, cmd: u8, args: ApplyArgs<'a> },
    /// Per-destination coalescing of non-batch data ops (one destination).
    /// `Reply` and nested `Batch` are rejected by [`encode`]/[`decode`].
    Batch { ops: Vec<Op<'a>> },
    /// Routed back to `token.origin_cell()`; returns one data-op credit.
    Reply { token: FabricToken, outcome: Outcome<'a> },
}

impl Op<'_> {
    fn opcode(&self) -> u8 {
        match self {
            Op::Read { .. } => OP_READ,
            Op::Write { .. } => OP_WRITE,
            Op::Apply { .. } => OP_APPLY,
            Op::Batch { .. } => OP_BATCH,
            Op::Reply { .. } => OP_REPLY,
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
    /// `Apply` declared more than [`MAX_APPLY_ARGS`] arguments.
    TooManyArgs(u64),
    /// `Batch` declared more than [`MAX_BATCH_OPS`] ops.
    TooManyBatchOps(u64),
    /// `Batch` nested inside `Batch`.
    NestedBatch,
    /// `Reply` nested inside `Batch`.
    ReplyInBatch,
}

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CodecError::Truncated => write!(f, "frame truncated"),
            CodecError::UnknownVersion(v) => write!(f, "unknown codec version {v}"),
            CodecError::UnknownOp(op) => write!(f, "unknown opcode {op}"),
            CodecError::ReservedFlags(flags) => write!(f, "reserved header flags {flags:#06x}"),
            CodecError::TrailingBytes => write!(f, "trailing bytes after frame"),
            CodecError::BadVarint => write!(f, "malformed varint"),
            CodecError::InvalidSlot(slot) => write!(f, "slot {slot} out of range"),
            CodecError::InvalidWriteFlags(bits) => write!(f, "invalid write flags {bits:#04x}"),
            CodecError::InvalidTag(tag) => write!(f, "invalid tag byte {tag}"),
            CodecError::TooManyArgs(n) => write!(f, "apply args {n} > {MAX_APPLY_ARGS}"),
            CodecError::TooManyBatchOps(n) => write!(f, "batch ops {n} > {MAX_BATCH_OPS}"),
            CodecError::NestedBatch => write!(f, "batch nested inside batch"),
            CodecError::ReplyInBatch => write!(f, "reply nested inside batch"),
        }
    }
}

impl std::error::Error for CodecError {}

/// Appends the complete encoded frame for `op` to `out`.
///
/// # Panics
///
/// Panics if a `Batch` nests another `Batch` or a `Reply` (rejected before
/// any bytes are written), or if a payload exceeds `u32::MAX` bytes (not
/// reachable with in-contract key/value sizes).
pub fn encode(op: &Op<'_>, out: &mut Vec<u8>) {
    if let Op::Batch { ops } = op {
        for nested in ops {
            match nested {
                Op::Batch { .. } => panic!("Batch must not nest Batch (codec v0)"),
                Op::Reply { .. } => panic!("Batch must not nest Reply (codec v0)"),
                _ => {}
            }
        }
    }
    let start = out.len();
    out.extend_from_slice(&[CODEC_VERSION, op.opcode(), 0, 0]); // flags:u16 = 0
    out.extend_from_slice(&[0; 4]); // len placeholder
    encode_payload(op, out);
    let len = out.len() - start - HEADER_LEN;
    let len = u32::try_from(len).expect("fabric frame payload exceeds u32::MAX");
    out[start + 4..start + 8].copy_from_slice(&len.to_le_bytes());
}

fn encode_payload(op: &Op<'_>, out: &mut Vec<u8>) {
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
        Op::Apply { token, slot, cmd, args } => {
            out.extend_from_slice(&token.0.to_le_bytes());
            out.extend_from_slice(&slot.get().to_le_bytes());
            out.push(*cmd);
            varint::encode_u64(args.len() as u64, out);
            for arg in args.as_slice() {
                encode_bytes(arg, out);
            }
        }
        Op::Batch { ops } => {
            varint::encode_u64(ops.len() as u64, out);
            for nested in ops {
                encode(nested, out);
            }
        }
        Op::Reply { token, outcome } => {
            out.extend_from_slice(&token.0.to_le_bytes());
            encode_outcome(outcome, out);
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
    let (op, used) = decode_frame(frame, false)?;
    if used != frame.len() {
        return Err(CodecError::TrailingBytes);
    }
    Ok(op)
}

/// Decodes one frame from the front of `buf`; returns the op and the bytes
/// consumed. `nested` is true when decoding inside a `Batch`.
fn decode_frame(buf: &[u8], nested: bool) -> Result<(Op<'_>, usize), CodecError> {
    if buf.len() < HEADER_LEN {
        return Err(CodecError::Truncated);
    }
    let version = buf[0];
    if version != CODEC_VERSION {
        return Err(CodecError::UnknownVersion(version));
    }
    let opcode = buf[1];
    let flags = u16::from_le_bytes([buf[2], buf[3]]);
    if flags != 0 {
        return Err(CodecError::ReservedFlags(flags));
    }
    let len = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;
    let end = HEADER_LEN.checked_add(len).ok_or(CodecError::Truncated)?;
    let payload = buf.get(HEADER_LEN..end).ok_or(CodecError::Truncated)?;

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
            let argc = reader.varint()?;
            if argc > MAX_APPLY_ARGS as u64 {
                return Err(CodecError::TooManyArgs(argc));
            }
            let mut packed: [&[u8]; MAX_APPLY_ARGS] = [&[]; MAX_APPLY_ARGS];
            for arg in packed.iter_mut().take(argc as usize) {
                *arg = reader.bytes()?;
            }
            // argc <= MAX_APPLY_ARGS < 256, so the cast is lossless.
            Op::Apply { token, slot, cmd, args: ApplyArgs { args: packed, len: argc as u8 } }
        }
        OP_BATCH => {
            if nested {
                return Err(CodecError::NestedBatch);
            }
            let count = reader.varint()?;
            if count > MAX_BATCH_OPS as u64 {
                return Err(CodecError::TooManyBatchOps(count));
            }
            let mut ops = Vec::with_capacity(count as usize);
            for _ in 0..count {
                let (nested_op, used) = decode_frame(reader.buf, true)?;
                if matches!(nested_op, Op::Reply { .. }) {
                    return Err(CodecError::ReplyInBatch);
                }
                reader.buf = &reader.buf[used..];
                ops.push(nested_op);
            }
            Op::Batch { ops }
        }
        OP_REPLY => {
            let token = reader.token()?;
            let outcome = reader.outcome()?;
            Op::Reply { token, outcome }
        }
        other => return Err(CodecError::UnknownOp(other)),
    };

    if !reader.buf.is_empty() {
        return Err(CodecError::TrailingBytes);
    }
    Ok((op, end))
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
        round_trip(&Op::Apply {
            token: token(7, 1),
            slot: slot(100),
            cmd: 0xEE,
            args: ApplyArgs::new(&[b"a".as_slice(), b"".as_slice(), b"ccc".as_slice()]).unwrap(),
        });
        round_trip(&Op::Batch {
            ops: vec![
                Op::Read { token: token(2, 5), slot: slot(7), key: b"x" },
                Op::Apply { token: token(2, 6), slot: slot(8), cmd: 1, args: ApplyArgs::EMPTY },
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
        bad_op[1] = 6;
        assert_eq!(decode(&bad_op), Err(CodecError::UnknownOp(6)));

        let mut bad_flags = good.clone();
        bad_flags[2] = 1;
        assert_eq!(decode(&bad_flags), Err(CodecError::ReservedFlags(1)));

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

    #[test]
    fn rejects_too_many_apply_args() {
        let mut payload = Vec::new();
        payload.extend_from_slice(&token(1, 1).0.to_le_bytes());
        payload.extend_from_slice(&1u16.to_le_bytes());
        payload.push(0); // cmd
        varint::encode_u64(MAX_APPLY_ARGS as u64 + 1, &mut payload);
        let frame = frame_with(OP_APPLY, &payload);
        assert_eq!(decode(&frame), Err(CodecError::TooManyArgs(MAX_APPLY_ARGS as u64 + 1)));
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
                OwnedOp::Apply { token, slot, cmd, args } => {
                    let slices: Vec<&[u8]> = args.iter().map(Vec::as_slice).collect();
                    Op::Apply {
                        token: FabricToken(*token),
                        slot: KeySlot::new(*slot).unwrap(),
                        cmd: *cmd,
                        args: ApplyArgs::new(&slices).unwrap(),
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
                prop::collection::vec(bytes.clone(), 0..MAX_APPLY_ARGS)
            )
                .prop_map(|(token, slot, cmd, args)| OwnedOp::Apply {
                    token,
                    slot,
                    cmd,
                    args
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

    proptest! {
        /// M0-S10 AC: arbitrary op sequences round-trip the codec byte-exact.
        #[test]
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
        fn decode_is_total(bytes in prop::collection::vec(any::<u8>(), 0..256)) {
            let _ = decode(&bytes);
        }
    }
}
