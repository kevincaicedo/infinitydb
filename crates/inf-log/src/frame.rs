//! Batch frame layout **v1** (M2-S01; freezes at M2 exit — milestone §3.2,
//! ADR-0011). One frame per reactor-loop iteration groups that iteration's
//! records so the log is written with one `writev` and replayed
//! validate-then-apply per frame (L3).
//!
//! ```text
//! frame   := header · body · trailer
//! header  := magic: [u8;4] = "IFR1"
//!            frame_len: u32 LE       — total bytes, header+body+trailer
//!            record_count: u32 LE    — ≥ 1 (empty iterations emit no frame)
//!            first_lsn: u32 LE × 2   — (segment, offset) of the FIRST record
//! body    := record_count records (record.rs)
//! trailer := CRC32C(header · body): u32 LE
//! ```
//!
//! A record's LSN is the byte offset of its length prefix within the
//! segment; the first record therefore sits at `frame offset +
//! FRAME_HEADER_LEN`, and the iterator derives every subsequent LSN from
//! record extents. All-zero magic means preallocated, never-written bytes —
//! the end of the active segment's tail (M2-S04/S14 build tail policy on
//! that signal).

use core::fmt;

use inf_simd::crc32c;

use crate::lsn::{Lsn, SegmentId};
use crate::record::{RecordDecodeError, RecordView, decode_record};

pub const FRAME_MAGIC: [u8; 4] = *b"IFR1";
pub const FRAME_HEADER_LEN: usize = 20;
pub const FRAME_TRAILER_LEN: usize = 4;
/// Smallest well-formed frame: header + one minimal record (4 bytes) + CRC.
pub const MIN_FRAME_LEN: u32 = (FRAME_HEADER_LEN + 4 + FRAME_TRAILER_LEN) as u32;
/// Default decoder bound on a single frame. Real frames are bounded by the
/// staging ring capacity (M2-S03); the decoder cap exists so a corrupt
/// length field cannot command absurd skips.
pub const DEFAULT_MAX_FRAME_LEN: u32 = 64 << 20;

/// Accumulates one iteration's records and seals them into a frame.
/// The buffer is reused across iterations (`reset`) — zero steady-state
/// allocation on the append path (L5; asserted end-to-end in M2-S03).
#[derive(Debug, Default)]
pub struct FrameBuilder {
    buf: Vec<u8>,
    record_count: u32,
    sealed: bool,
}

impl FrameBuilder {
    #[must_use]
    pub fn new() -> FrameBuilder {
        FrameBuilder::with_capacity(0)
    }

    #[must_use]
    pub fn with_capacity(capacity: usize) -> FrameBuilder {
        let mut buf = Vec::with_capacity(capacity.max(FRAME_HEADER_LEN));
        buf.resize(FRAME_HEADER_LEN, 0);
        FrameBuilder { buf, record_count: 0, sealed: false }
    }

    #[must_use]
    pub fn record_count(&self) -> u32 {
        self.record_count
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.record_count == 0
    }

    /// Total frame size (header + records so far + trailer) — what
    /// `SegmentRotor::begin_frame` reserves.
    #[must_use]
    pub fn frame_len(&self) -> u32 {
        u32::try_from(self.buf.len() + FRAME_TRAILER_LEN).expect("frame exceeds u32")
    }

    /// Append one record, encoding directly into the frame buffer.
    ///
    /// # Panics
    /// If called after `finalize` without `reset` — an internal invariant
    /// of the LOG step, not a runtime condition.
    pub fn append(&mut self, record: &RecordView<'_>) {
        assert!(!self.sealed, "append on a sealed frame (missing reset)");
        record.encode_into(&mut self.buf);
        self.record_count += 1;
    }

    /// Seal the frame: patch the header with `first_record_lsn` (the LSN of
    /// the first record — frame base + [`FRAME_HEADER_LEN`]) and append the
    /// CRC32C trailer. Returns the finished frame bytes.
    ///
    /// # Panics
    /// On an empty frame — record_count ≥ 1 is a format invariant; callers
    /// never seal an iteration that staged nothing.
    pub fn finalize(&mut self, first_record_lsn: Lsn) -> &[u8] {
        assert!(self.record_count > 0, "finalize of an empty frame");
        assert!(!self.sealed, "double finalize (missing reset)");
        let frame_len = self.frame_len();
        self.buf[0..4].copy_from_slice(&FRAME_MAGIC);
        self.buf[4..8].copy_from_slice(&frame_len.to_le_bytes());
        self.buf[8..12].copy_from_slice(&self.record_count.to_le_bytes());
        self.buf[12..16].copy_from_slice(&first_record_lsn.segment.0.to_le_bytes());
        self.buf[16..20].copy_from_slice(&first_record_lsn.offset.to_le_bytes());
        let crc = crc32c(&self.buf);
        self.buf.extend_from_slice(&crc.to_le_bytes());
        self.sealed = true;
        &self.buf
    }

    /// The finished frame bytes, available from `finalize` until `reset` —
    /// what an in-flight write (the M2-S03 staging lease) hands to the
    /// `writev`.
    ///
    /// # Panics
    /// If the frame has not been finalized — an internal invariant of the
    /// LOG step.
    #[must_use]
    pub fn sealed_frame(&self) -> &[u8] {
        assert!(self.sealed, "sealed_frame before finalize");
        &self.buf
    }

    /// Clear for the next iteration, keeping the allocation.
    pub fn reset(&mut self) {
        self.buf.truncate(0);
        self.buf.resize(FRAME_HEADER_LEN, 0);
        self.record_count = 0;
        self.sealed = false;
    }
}

/// Why a frame failed to decode. `ZeroMagic` is the *expected* end of an
/// active segment's preallocated tail; every other variant is a torn write
/// or corruption, and M2-S14 owns the torn-final vs interior-corruption
/// policy split.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum FrameDecodeError {
    /// Fewer bytes available than the frame (or header) claims — the
    /// torn-tail candidate.
    Truncated {
        needed: usize,
        available: usize,
    },
    /// Magic is all zeroes: preallocated, never-written region.
    ZeroMagic,
    BadMagic {
        found: [u8; 4],
    },
    /// `frame_len` below the minimum or above the configured cap.
    BadLength {
        len: u32,
    },
    /// CRC-valid writers never emit zero records; corruption.
    ZeroRecordCount,
    CrcMismatch {
        stored: u32,
        computed: u32,
    },
}

impl fmt::Display for FrameDecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FrameDecodeError::Truncated { needed, available } => {
                write!(f, "frame truncated: needs {needed} bytes, {available} available")
            }
            FrameDecodeError::ZeroMagic => write!(f, "zero magic (preallocated tail)"),
            FrameDecodeError::BadMagic { found } => write!(f, "bad frame magic {found:02x?}"),
            FrameDecodeError::BadLength { len } => write!(f, "bad frame length {len}"),
            FrameDecodeError::ZeroRecordCount => write!(f, "frame with zero records"),
            FrameDecodeError::CrcMismatch { stored, computed } => {
                write!(f, "frame CRC mismatch: stored {stored:#010x}, computed {computed:#010x}")
            }
        }
    }
}

impl std::error::Error for FrameDecodeError {}

/// A validated frame borrowing the underlying bytes. Constructed only by
/// [`decode_frame`] after the CRC check — holding a `FrameRef` means
/// header+body integrity already passed.
#[derive(Copy, Clone, Debug)]
pub struct FrameRef<'a> {
    first_lsn: Lsn,
    record_count: u32,
    body: &'a [u8],
}

impl<'a> FrameRef<'a> {
    #[must_use]
    pub fn first_lsn(&self) -> Lsn {
        self.first_lsn
    }

    #[must_use]
    pub fn record_count(&self) -> u32 {
        self.record_count
    }

    /// Iterate `(lsn, record)` pairs. Any `Err` inside a CRC-valid frame is
    /// corruption-or-bug: replay fail-stops (§8.4), never skips.
    #[must_use]
    pub fn records(&self) -> RecordIter<'a> {
        RecordIter {
            body: self.body,
            offset: 0,
            declared: self.record_count,
            remaining: self.record_count,
            next_lsn: self.first_lsn,
            failed: false,
        }
    }
}

/// Record-level errors surfaced while walking a CRC-valid frame.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum FrameRecordError {
    Record(RecordDecodeError),
    /// Body bytes remain after `record_count` records.
    TrailingBytes {
        at: usize,
    },
    /// Body ended before `record_count` records were read.
    MissingRecords {
        decoded: u32,
        declared: u32,
    },
}

impl fmt::Display for FrameRecordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FrameRecordError::Record(err) => write!(f, "record error in frame: {err}"),
            FrameRecordError::TrailingBytes { at } => {
                write!(f, "trailing bytes after last record at body offset {at}")
            }
            FrameRecordError::MissingRecords { decoded, declared } => {
                write!(f, "frame body ended after {decoded} of {declared} records")
            }
        }
    }
}

impl std::error::Error for FrameRecordError {}

/// Iterator over the records of one frame. Fused: after the first error or
/// the final record it yields `None` forever.
#[derive(Debug)]
pub struct RecordIter<'a> {
    body: &'a [u8],
    offset: usize,
    declared: u32,
    remaining: u32,
    next_lsn: Lsn,
    failed: bool,
}

impl<'a> Iterator for RecordIter<'a> {
    type Item = Result<(Lsn, RecordView<'a>), FrameRecordError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed {
            return None;
        }
        if self.remaining == 0 {
            if self.offset != self.body.len() {
                self.failed = true;
                return Some(Err(FrameRecordError::TrailingBytes { at: self.offset }));
            }
            return None;
        }
        if self.offset == self.body.len() {
            self.failed = true;
            return Some(Err(FrameRecordError::MissingRecords {
                decoded: self.declared - self.remaining,
                declared: self.declared,
            }));
        }
        match decode_record(&self.body[self.offset..]) {
            Ok((view, consumed)) => {
                let lsn = self.next_lsn;
                self.offset += consumed;
                self.next_lsn = self.next_lsn.advance(consumed as u32);
                self.remaining -= 1;
                Some(Ok((lsn, view)))
            }
            Err(err) => {
                self.failed = true;
                Some(Err(FrameRecordError::Record(err)))
            }
        }
    }
}

/// Decode and CRC-validate one frame from the front of `buf`. Returns the
/// frame view and total bytes consumed (`frame_len`).
pub fn decode_frame(
    buf: &[u8],
    max_frame_len: u32,
) -> Result<(FrameRef<'_>, usize), FrameDecodeError> {
    if buf.len() < 4 {
        if buf.iter().all(|&b| b == 0) {
            return Err(FrameDecodeError::ZeroMagic);
        }
        return Err(FrameDecodeError::Truncated { needed: 4, available: buf.len() });
    }
    let magic: [u8; 4] = buf[0..4].try_into().expect("4-byte slice");
    if magic == [0; 4] {
        return Err(FrameDecodeError::ZeroMagic);
    }
    if magic != FRAME_MAGIC {
        return Err(FrameDecodeError::BadMagic { found: magic });
    }
    if buf.len() < FRAME_HEADER_LEN {
        return Err(FrameDecodeError::Truncated { needed: FRAME_HEADER_LEN, available: buf.len() });
    }
    let frame_len = u32::from_le_bytes(buf[4..8].try_into().expect("4-byte slice"));
    if frame_len < MIN_FRAME_LEN || frame_len > max_frame_len {
        return Err(FrameDecodeError::BadLength { len: frame_len });
    }
    let frame_len_usize = frame_len as usize;
    if buf.len() < frame_len_usize {
        return Err(FrameDecodeError::Truncated { needed: frame_len_usize, available: buf.len() });
    }
    let frame = &buf[..frame_len_usize];
    let (covered, trailer) = frame.split_at(frame_len_usize - FRAME_TRAILER_LEN);
    let stored = u32::from_le_bytes(trailer.try_into().expect("4-byte trailer"));
    let computed = crc32c(covered);
    if stored != computed {
        return Err(FrameDecodeError::CrcMismatch { stored, computed });
    }
    let record_count = u32::from_le_bytes(frame[8..12].try_into().expect("4-byte slice"));
    if record_count == 0 {
        return Err(FrameDecodeError::ZeroRecordCount);
    }
    let first_lsn = Lsn::new(
        SegmentId(u32::from_le_bytes(frame[12..16].try_into().expect("4-byte slice"))),
        u32::from_le_bytes(frame[16..20].try_into().expect("4-byte slice")),
    );
    let body = &frame[FRAME_HEADER_LEN..frame_len_usize - FRAME_TRAILER_LEN];
    Ok((FrameRef { first_lsn, record_count, body }, frame_len_usize))
}

/// Sequential frame iterator over a contiguous byte region (a segment
/// image). Validates-then-yields whole frames; stops cleanly at zero magic
/// (the preallocated tail) and fuses after the first error. `offset()`
/// reports bytes consumed — the tail-scan input for M2-S04/S14.
#[derive(Debug)]
pub struct FrameIter<'a> {
    buf: &'a [u8],
    offset: usize,
    max_frame_len: u32,
    done: bool,
}

impl<'a> FrameIter<'a> {
    #[must_use]
    pub fn new(buf: &'a [u8], max_frame_len: u32) -> FrameIter<'a> {
        FrameIter { buf, offset: 0, max_frame_len, done: false }
    }

    /// Bytes consumed by fully-validated frames so far.
    #[must_use]
    pub fn offset(&self) -> usize {
        self.offset
    }
}

impl<'a> Iterator for FrameIter<'a> {
    type Item = Result<(usize, FrameRef<'a>), FrameDecodeError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done || self.offset == self.buf.len() {
            self.done = true;
            return None;
        }
        match decode_frame(&self.buf[self.offset..], self.max_frame_len) {
            Ok((frame, consumed)) => {
                let at = self.offset;
                self.offset += consumed;
                Some(Ok((at, frame)))
            }
            Err(FrameDecodeError::ZeroMagic) => {
                self.done = true;
                None
            }
            Err(err) => {
                self.done = true;
                Some(Err(err))
            }
        }
    }
}
