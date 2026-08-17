//! Batch frame layouts **v1** (M2-S01, ADR-0011) and **v2** (M2.5-S12,
//! ADR-0031). One frame per reactor-loop iteration groups that iteration's
//! records so the log is written with one `writev` and replayed
//! validate-then-apply per frame (L3). The writer emits v2 only; the
//! decoder accepts both forever on the alpha line (ADR-0031 D2).
//!
//! ```text
//! frame   := header · body · trailer
//! header  := magic: [u8;4] = "IFR1" | "IFR2"
//!            frame_len: u32 LE       — total bytes, header+body+trailer
//!            record_count: u32 LE    — ≥ 1 (empty iterations emit no frame)
//!            first_lsn: u32 LE × 2   — (segment, offset) of the FIRST record
//!            -- v2 only (ADR-0031 D1) --
//!            epoch: u32 LE           — log life this frame belongs to (≥ 1)
//!            seq: u64 LE             — frame ordinal within the epoch (from 1)
//!            covered_lsn: u64 LE     — durability watermark at seal (Lsn::to_u64)
//! body    := record_count records (record.rs)
//! trailer := CRC32C(header · body): u32 LE
//! ```
//!
//! A record's LSN is the byte offset of its length prefix within the
//! segment; the first record therefore sits at `frame offset + header
//! length` (20 for v1, 40 for v2), and the iterator derives every
//! subsequent LSN from record extents. All-zero magic means preallocated,
//! never-written bytes — the end of the active segment's tail
//! (M2-S04/S14 build tail policy on that signal).
//!
//! The v2 stamp is what lets recovery distinguish a torn un-covered tail
//! from covered bytes the disk lost (the ADR-0021 D3 refusal class):
//! `covered_lsn` is a CRC-protected attestation of the watermark, `epoch`
//! separates log lives so discarded residue can never re-enter a replay
//! prefix, and `seq` pins writer continuity (ADR-0031 D3–D5).

use core::fmt;

use inf_simd::crc32c;

use crate::lsn::{Lsn, SegmentId};
use crate::record::{RecordDecodeError, RecordView, decode_record};

/// Legacy v1 magic — read support only (ADR-0031 D2).
pub const FRAME_MAGIC_V1: [u8; 4] = *b"IFR1";
/// Current (v2) magic — what the writer emits.
pub const FRAME_MAGIC: [u8; 4] = *b"IFR2";
pub const FRAME_HEADER_LEN_V1: usize = 20;
/// Header length of the current (v2) format.
pub const FRAME_HEADER_LEN: usize = 40;
pub const FRAME_TRAILER_LEN: usize = 4;
/// Smallest well-formed v1 frame: header + one minimal record + CRC.
pub const MIN_FRAME_LEN_V1: u32 = (FRAME_HEADER_LEN_V1 + 4 + FRAME_TRAILER_LEN) as u32;
/// Smallest well-formed frame of the current format.
pub const MIN_FRAME_LEN: u32 = (FRAME_HEADER_LEN + 4 + FRAME_TRAILER_LEN) as u32;
/// Default decoder bound on a single frame. Real frames are bounded by the
/// staging ring capacity (M2-S03); the decoder cap exists so a corrupt
/// length field cannot command absurd skips.
pub const DEFAULT_MAX_FRAME_LEN: u32 = 64 << 20;

/// The v2 per-frame stamp (ADR-0031 D1): which log life wrote the frame
/// (`epoch`), where in that life (`seq`, from 1, +1 per frame), and what
/// the group-commit durability watermark attested at seal time
/// (`covered_lsn` = `Lsn::to_u64`, 0 = nothing covered yet).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct FrameStamp {
    pub epoch: u32,
    pub seq: u64,
    pub covered_lsn: u64,
}

/// Header length for a frame carrying (or not carrying) a stamp.
#[must_use]
pub const fn frame_header_len(has_stamp: bool) -> usize {
    if has_stamp { FRAME_HEADER_LEN } else { FRAME_HEADER_LEN_V1 }
}

/// Accumulates one iteration's records and seals them into a (v2) frame.
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
    /// the first record — frame base + [`FRAME_HEADER_LEN`]) and the v2
    /// `stamp`, and append the CRC32C trailer. Returns the finished frame
    /// bytes.
    ///
    /// # Panics
    /// On an empty frame — record_count ≥ 1 is a format invariant; callers
    /// never seal an iteration that staged nothing. Also if the stamp is
    /// malformed (`epoch == 0` or `seq == 0` — reserved by ADR-0031 D1) or
    /// the covered watermark leads the frame's own records (it can never
    /// lead the append cursor).
    pub fn finalize(&mut self, first_record_lsn: Lsn, stamp: FrameStamp) -> &[u8] {
        assert!(self.record_count > 0, "finalize of an empty frame");
        assert!(!self.sealed, "double finalize (missing reset)");
        assert!(stamp.epoch > 0, "frame epoch 0 is reserved (ADR-0031 D1)");
        assert!(stamp.seq > 0, "frame seq 0 is reserved (ADR-0031 D1)");
        assert!(
            stamp.covered_lsn <= first_record_lsn.to_u64(),
            "covered watermark leads the frame's own records"
        );
        let frame_len = self.frame_len();
        self.buf[0..4].copy_from_slice(&FRAME_MAGIC);
        self.buf[4..8].copy_from_slice(&frame_len.to_le_bytes());
        self.buf[8..12].copy_from_slice(&self.record_count.to_le_bytes());
        self.buf[12..16].copy_from_slice(&first_record_lsn.segment.0.to_le_bytes());
        self.buf[16..20].copy_from_slice(&first_record_lsn.offset.to_le_bytes());
        self.buf[20..24].copy_from_slice(&stamp.epoch.to_le_bytes());
        self.buf[24..32].copy_from_slice(&stamp.seq.to_le_bytes());
        self.buf[32..40].copy_from_slice(&stamp.covered_lsn.to_le_bytes());
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
    /// `frame_len` below the version's minimum or above the configured cap.
    BadLength {
        len: u32,
    },
    /// CRC-valid writers never emit zero records; corruption.
    ZeroRecordCount,
    /// v2 stamps never carry epoch 0, seq 0, or an attestation past the
    /// frame's own first record (ADR-0031 D1); corruption.
    BadStamp,
    CrcMismatch {
        stored: u32,
        computed: u32,
    },
    /// `first_lsn.offset` cannot be a frame's own record cursor: it sits
    /// below one header length, or the frame's bytes would run past the
    /// `u32` offset ceiling. Honest writers derive it as `base + header_len`
    /// (20 for v1, 40 for v2) inside a segment — ADR-0011 D2 as restated
    /// per-version by ADR-0072 D1 — so either shape is corruption.
    BadFirstLsn {
        offset: u32,
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
            FrameDecodeError::BadStamp => write!(f, "frame stamp with reserved epoch/seq 0"),
            FrameDecodeError::CrcMismatch { stored, computed } => {
                write!(f, "frame CRC mismatch: stored {stored:#010x}, computed {computed:#010x}")
            }
            FrameDecodeError::BadFirstLsn { offset } => {
                write!(f, "frame first_lsn offset {offset} cannot address this frame's records")
            }
        }
    }
}

impl std::error::Error for FrameDecodeError {}

/// A validated frame borrowing the underlying bytes. Constructed only by
/// [`decode_frame`] after the CRC check — holding a `FrameRef` means
/// header+body integrity already passed. `stamp` is `None` for v1 frames
/// (they attest nothing — ADR-0031 D4).
#[derive(Copy, Clone, Debug)]
pub struct FrameRef<'a> {
    first_lsn: Lsn,
    record_count: u32,
    stamp: Option<FrameStamp>,
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

    /// The v2 stamp, `None` on a legacy v1 frame.
    #[must_use]
    pub fn stamp(&self) -> Option<FrameStamp> {
        self.stamp
    }

    /// This frame's header length (version-dependent) — the distance from
    /// the frame base to `first_lsn`.
    #[must_use]
    pub fn header_len(&self) -> usize {
        frame_header_len(self.stamp.is_some())
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

/// Header facts shared by the decoder and the readers' window sizing:
/// which version the magic names and that version's header/minimum bounds.
pub(crate) struct FrameShape {
    pub header_len: usize,
    pub min_frame_len: u32,
    pub has_stamp: bool,
}

/// Classify the 4-byte magic. `Ok(None)` = all-zero (preallocated tail).
pub(crate) fn frame_shape(magic: [u8; 4]) -> Result<Option<FrameShape>, FrameDecodeError> {
    if magic == [0; 4] {
        return Ok(None);
    }
    if magic == FRAME_MAGIC {
        return Ok(Some(FrameShape {
            header_len: FRAME_HEADER_LEN,
            min_frame_len: MIN_FRAME_LEN,
            has_stamp: true,
        }));
    }
    if magic == FRAME_MAGIC_V1 {
        return Ok(Some(FrameShape {
            header_len: FRAME_HEADER_LEN_V1,
            min_frame_len: MIN_FRAME_LEN_V1,
            has_stamp: false,
        }));
    }
    Err(FrameDecodeError::BadMagic { found: magic })
}

/// Decode and CRC-validate one frame (either format) from the front of
/// `buf`. Returns the frame view and total bytes consumed (`frame_len`).
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
    let Some(shape) = frame_shape(magic)? else {
        return Err(FrameDecodeError::ZeroMagic);
    };
    if buf.len() < shape.header_len {
        return Err(FrameDecodeError::Truncated { needed: shape.header_len, available: buf.len() });
    }
    let frame_len = u32::from_le_bytes(buf[4..8].try_into().expect("4-byte slice"));
    if frame_len < shape.min_frame_len || frame_len > max_frame_len {
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
    // An honest writer derives the first record's offset as `base +
    // header_len` — 20 for v1, 40 for v2 — and the whole frame sits inside a
    // u32-addressed segment (ADR-0011 D2 as restated per-version by
    // ADR-0072 D1; D2's own text says "+ 20", which is v1-era wording).
    // So the offset is at least one header in and the frame's own bytes fit
    // below the ceiling. Without this bound a CRC-valid frame declaring a
    // near-`u32::MAX` offset makes `RecordIter` advance the record cursor
    // past the ceiling and panic in `Lsn::advance`, and makes
    // `Phase::Replay`'s `first_lsn.offset - header_len` frame-base
    // subtraction underflow.
    let header_len_u32 = shape.header_len as u32;
    if first_lsn.offset < header_len_u32
        || (first_lsn.offset - header_len_u32).checked_add(frame_len).is_none()
    {
        return Err(FrameDecodeError::BadFirstLsn { offset: first_lsn.offset });
    }
    let stamp = if shape.has_stamp {
        let stamp = FrameStamp {
            epoch: u32::from_le_bytes(frame[20..24].try_into().expect("4-byte slice")),
            seq: u64::from_le_bytes(frame[24..32].try_into().expect("8-byte slice")),
            covered_lsn: u64::from_le_bytes(frame[32..40].try_into().expect("8-byte slice")),
        };
        // Epoch/seq 0 are reserved, and the covered watermark can never
        // lead the append cursor (ADR-0031 D1): a CRC-valid frame carrying
        // either was never written by an honest writer.
        if stamp.epoch == 0 || stamp.seq == 0 || stamp.covered_lsn > first_lsn.to_u64() {
            return Err(FrameDecodeError::BadStamp);
        }
        Some(stamp)
    } else {
        None
    };
    let body = &frame[shape.header_len..frame_len_usize - FRAME_TRAILER_LEN];
    Ok((FrameRef { first_lsn, record_count, stamp, body }, frame_len_usize))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::NsId;

    /// One honest single-record v2 frame, then `first_lsn.offset` rewritten
    /// to `offset` with the trailer repaired — the frame stays CRC-valid,
    /// which is exactly what a misdirected write delivers (ADR-0011 D2).
    fn frame_with_first_offset(offset: u32) -> Vec<u8> {
        let mut builder = FrameBuilder::new();
        builder.append(&RecordView::StringPostImage { ns: NsId(1), key: b"k", value: b"v" });
        let stamp = FrameStamp { epoch: 1, seq: 1, covered_lsn: 0 };
        let mut image =
            builder.finalize(Lsn::new(SegmentId(0), FRAME_HEADER_LEN as u32), stamp).to_vec();
        image[16..20].copy_from_slice(&offset.to_le_bytes());
        let end = image.len() - FRAME_TRAILER_LEN;
        let crc = crc32c(&image[..end]);
        image[end..].copy_from_slice(&crc.to_le_bytes());
        image
    }

    /// A CRC-valid frame whose record cursor would run past the `u32` offset
    /// ceiling is refused at decode. Before this bound `decode_frame`
    /// admitted it and `RecordIter` panicked in `Lsn::advance` on the first
    /// record — nightly fuzz `frame_decode`, 2026-08-17.
    #[test]
    fn first_lsn_offset_past_the_offset_ceiling_is_a_decode_error() {
        let image = frame_with_first_offset(u32::MAX);
        assert!(matches!(
            decode_frame(&image, DEFAULT_MAX_FRAME_LEN),
            Err(FrameDecodeError::BadFirstLsn { offset: u32::MAX })
        ));
    }

    /// The mirror bound: a frame's first record never sits inside the
    /// frame's own header, so `Phase::Replay`'s `offset - header_len`
    /// frame-base subtraction cannot underflow.
    #[test]
    fn first_lsn_offset_inside_the_header_is_a_decode_error() {
        for offset in [0, 1, FRAME_HEADER_LEN as u32 - 1] {
            let image = frame_with_first_offset(offset);
            assert!(
                matches!(
                    decode_frame(&image, DEFAULT_MAX_FRAME_LEN),
                    Err(FrameDecodeError::BadFirstLsn { .. })
                ),
                "offset {offset} must not decode"
            );
        }
    }

    /// The bound admits the whole legal range: a frame ending exactly at the
    /// ceiling decodes, and every record walks without panicking.
    #[test]
    fn frame_ending_at_the_offset_ceiling_decodes_and_walks() {
        let frame_len = frame_with_first_offset(FRAME_HEADER_LEN as u32).len() as u32;
        let last = u32::MAX - frame_len + FRAME_HEADER_LEN as u32;
        let image = frame_with_first_offset(last);
        let (frame, consumed) =
            decode_frame(&image, DEFAULT_MAX_FRAME_LEN).expect("legal offset decodes");
        assert_eq!(consumed, frame_len as usize);
        assert_eq!(frame.first_lsn().offset, last);
        assert_eq!(frame.records().filter(|r| r.is_ok()).count(), 1);
    }
}
