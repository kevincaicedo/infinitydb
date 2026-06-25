//! `inf-log` — per-cell log spine (master plan §8, M2-S01).
//!
//! This crate owns the durability byte contract: per-cell LSNs, typed log
//! records, and CRC32C-protected batch frames. Segment files, fsync policy,
//! checkpoints, and recovery build on these bytes in later M2 stories.
#![forbid(unsafe_code)]

use core::fmt;

use inf_foundation::varint::{decode_u64, encode_u64};
use inf_simd::crc32c;

mod checkpoint;
pub mod fault;
mod manifest;
mod reader;
mod segment;
mod staging;

pub use checkpoint::{
    CHECKPOINT_FOOTER_DIGEST_ALGORITHM, CHECKPOINT_FOOTER_LEN, CHECKPOINT_FOOTER_MAGIC,
    CHECKPOINT_IMAGE_HEADER_FIXED_LEN, CHECKPOINT_IMAGE_HEADER_MIN_LEN,
    CHECKPOINT_IMAGE_HEADER_TRAILER_LEN, CHECKPOINT_IMAGE_MAGIC, CHECKPOINT_IMAGE_VERSION,
    CHECKPOINT_SECTION_HEADER_LEN, CHECKPOINT_SECTION_MAGIC, CHECKPOINT_SECTION_TRAILER_LEN,
    CheckpointDigest, CheckpointFooter, CheckpointHeader, CheckpointImageError,
    CheckpointNamespaceIter, CheckpointSectionFrameParts, CheckpointSectionKind,
    CheckpointSectionMeta, CheckpointSectionRef, DecodedCheckpointHeader, DecodedCheckpointSection,
    DecodedCheckpointSectionHeader, MAX_CHECKPOINT_HEADER_NAMESPACES,
    MAX_CHECKPOINT_IMAGE_SECTIONS, MAX_CHECKPOINT_SECTION_PAYLOAD_LEN,
    checkpoint_header_len_from_prefix, decode_checkpoint_footer, decode_checkpoint_header,
    decode_checkpoint_section, decode_checkpoint_section_frame_header, encode_checkpoint_footer,
    encode_checkpoint_header, encode_checkpoint_section, encode_checkpoint_section_frame_parts,
    validate_checkpoint_footer,
};
pub use manifest::{
    CHECKPOINT_FILE_DIGITS, CHECKPOINT_FILE_NAME_LEN, CHECKPOINT_FILE_PREFIX,
    CHECKPOINT_FILE_SUFFIX, CheckpointId, CheckpointNameError, CheckpointRef, MAX_CHECKPOINT_ID,
    MAX_RECOVERY_MANIFEST_BYTES, MAX_RECOVERY_MANIFEST_SEGMENTS, RECOVERY_MANIFEST_FILE,
    RECOVERY_MANIFEST_HEADER_LEN, RECOVERY_MANIFEST_MAGIC, RECOVERY_MANIFEST_TEMP_FILE,
    RECOVERY_MANIFEST_TRAILER_LEN, RECOVERY_MANIFEST_VERSION, RecoveryManifest,
    RecoveryManifestError, decode_recovery_manifest, encode_recovery_manifest,
};
pub use reader::{
    DEFAULT_SEGMENT_READ_CHUNK_BYTES, SegmentFileReader, SegmentFrame, SegmentFrameError,
    SegmentFrameIter, SegmentFrameSink, SegmentReadConfig, SegmentReadConfigError,
    SegmentReadError, SegmentReadRequest, SegmentReadTerminal, SegmentTailPolicy,
    find_frame_magic_offset, iter_segment_frames,
};
pub use segment::{
    DEFAULT_PREALLOCATE_THRESHOLD_BYTES, DEFAULT_SEGMENT_FRAME_MAX_BYTES,
    DEFAULT_SEGMENT_SIZE_BYTES, FramePlacement, MAX_SEGMENT_ID, SEGMENT_FILE_DIGITS,
    SEGMENT_FILE_NAME_LEN, SEGMENT_FILE_PREFIX, SEGMENT_FILE_SUFFIX, SegmentCatalog, SegmentConfig,
    SegmentDirectory, SegmentDirectoryError, SegmentId, SegmentLifecycle, SegmentLifecycleError,
    SegmentMaintenance, SegmentScanError, SegmentSeal, scan_segment_directory, scan_segment_names,
};
pub use staging::{LogStaging, LogStagingError, MAX_STAGING_BODY_LEN};

pub const FRAME_MAGIC: u32 = u32::from_le_bytes(*b"ILG1");
pub const FRAME_HEADER_LEN: usize = 20;
pub const FRAME_TRAILER_LEN: usize = 4;
pub const RECORD_HEADER_LEN: usize = 7;
/// Largest M2 inline record payload:
/// max u24 string value, max u8 key, canonical key/value length varints, and
/// the optional u64 expiry carried by string post-image records.
pub const MAX_RECORD_PAYLOAD_LEN: usize = ((1 << 24) - 1) + u8::MAX as usize + 2 + 4 + 8;
pub const MAX_RECORD_ENCODED_LEN: usize =
    RECORD_HEADER_LEN + MAX_RECORD_PAYLOAD_LEN + varint_len_u64(MAX_RECORD_BODY_LEN as u64);
const MAX_RECORD_BODY_LEN: usize = RECORD_HEADER_LEN + MAX_RECORD_PAYLOAD_LEN;
pub const MAX_FRAME_LEN: usize = 64 * 1024 * 1024;
pub const LOG_CONTROL_NAMESPACE: NamespaceId = NamespaceId::new(u32::MAX);

/// Per-cell log sequence number: `(segment, offset)`.
///
/// There is no global LSN. Segment advancement is owned by the segment
/// lifecycle code; this type deliberately only advances within one segment.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Lsn {
    segment: u32,
    offset: u32,
}

impl Lsn {
    #[inline]
    pub const fn new(segment: u32, offset: u32) -> Lsn {
        Lsn { segment, offset }
    }

    #[inline]
    pub const fn segment(self) -> u32 {
        self.segment
    }

    #[inline]
    pub const fn offset(self) -> u32 {
        self.offset
    }

    #[inline]
    pub fn checked_add_bytes(self, bytes: usize) -> Result<Lsn, LogCodecError> {
        let bytes = u32::try_from(bytes).map_err(|_| LogCodecError::OffsetOverflow)?;
        let offset = self.offset.checked_add(bytes).ok_or(LogCodecError::OffsetOverflow)?;
        Ok(Lsn { segment: self.segment, offset })
    }
}

impl fmt::Display for Lsn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.segment, self.offset)
    }
}

/// Cell-local namespace identifier as serialized in log records.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct NamespaceId(u32);

impl NamespaceId {
    #[inline]
    pub const fn new(raw: u32) -> NamespaceId {
        NamespaceId(raw)
    }

    #[inline]
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// M2 log record vocabulary. Later milestones may add variants by ADR.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum RecordKind {
    StringPostImage,
    Delete,
    ExpireAt,
    Namespace,
    CheckpointBegin,
}

impl RecordKind {
    #[inline]
    pub const fn tag(self) -> u8 {
        match self {
            RecordKind::StringPostImage => 1,
            RecordKind::Delete => 2,
            RecordKind::ExpireAt => 3,
            RecordKind::Namespace => 4,
            RecordKind::CheckpointBegin => 5,
        }
    }

    #[inline]
    pub const fn from_tag(tag: u8) -> Option<RecordKind> {
        match tag {
            1 => Some(RecordKind::StringPostImage),
            2 => Some(RecordKind::Delete),
            3 => Some(RecordKind::ExpireAt),
            4 => Some(RecordKind::Namespace),
            5 => Some(RecordKind::CheckpointBegin),
            _ => None,
        }
    }
}

/// Borrowed record ready for frame encoding or yielded by a decoded frame.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct RecordRef<'a> {
    kind: RecordKind,
    namespace: NamespaceId,
    flags: u16,
    payload: &'a [u8],
}

impl<'a> RecordRef<'a> {
    #[inline]
    pub fn new(
        kind: RecordKind,
        namespace: NamespaceId,
        flags: u16,
        payload: &'a [u8],
    ) -> Result<RecordRef<'a>, LogCodecError> {
        if payload.len() > MAX_RECORD_PAYLOAD_LEN {
            return Err(LogCodecError::RecordTooLarge);
        }
        Ok(RecordRef { kind, namespace, flags, payload })
    }

    #[inline]
    pub const fn kind(self) -> RecordKind {
        self.kind
    }

    #[inline]
    pub const fn namespace(self) -> NamespaceId {
        self.namespace
    }

    #[inline]
    pub const fn flags(self) -> u16 {
        self.flags
    }

    #[inline]
    pub const fn payload(self) -> &'a [u8] {
        self.payload
    }

    #[inline]
    pub fn encoded_len(self) -> usize {
        record_encoded_len(self.payload.len()).expect("RecordRef validates payload length")
    }
}

/// One decoded record with its per-cell LSN.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct DecodedRecord<'a> {
    lsn: Lsn,
    record: RecordRef<'a>,
}

impl<'a> DecodedRecord<'a> {
    #[inline]
    pub const fn lsn(self) -> Lsn {
        self.lsn
    }

    #[inline]
    pub const fn record(self) -> RecordRef<'a> {
        self.record
    }
}

/// Metadata returned by frame encoding.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct FrameMeta {
    frame_start: Lsn,
    first_lsn: Lsn,
    frame_end: Lsn,
    frame_len: u32,
    record_count: u32,
}

impl FrameMeta {
    #[inline]
    pub const fn frame_start(self) -> Lsn {
        self.frame_start
    }

    #[inline]
    pub const fn first_lsn(self) -> Lsn {
        self.first_lsn
    }

    #[inline]
    pub const fn frame_end(self) -> Lsn {
        self.frame_end
    }

    #[inline]
    pub const fn frame_len(self) -> u32 {
        self.frame_len
    }

    #[inline]
    pub const fn record_count(self) -> u32 {
        self.record_count
    }
}

/// A fully validated decoded frame. Construction validates CRC and every
/// record boundary before [`DecodedFrame::records`] can yield anything.
#[derive(Copy, Clone, Debug)]
pub struct DecodedFrame<'a> {
    bytes: &'a [u8],
    body_start: usize,
    body_end: usize,
    first_lsn: Lsn,
    record_count: u32,
}

impl<'a> DecodedFrame<'a> {
    #[inline]
    pub const fn first_lsn(self) -> Lsn {
        self.first_lsn
    }

    #[inline]
    pub const fn frame_len(self) -> u32 {
        self.bytes.len() as u32
    }

    #[inline]
    pub const fn record_count(self) -> u32 {
        self.record_count
    }

    #[inline]
    pub fn records(self) -> RecordIter<'a> {
        RecordIter {
            body: &self.bytes[self.body_start..self.body_end],
            first_lsn: self.first_lsn,
            offset: 0,
            remaining: self.record_count,
        }
    }
}

/// Iterator over records from a validated frame.
#[derive(Clone, Debug)]
pub struct RecordIter<'a> {
    body: &'a [u8],
    first_lsn: Lsn,
    offset: usize,
    remaining: u32,
}

impl<'a> Iterator for RecordIter<'a> {
    type Item = DecodedRecord<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        let lsn = self.first_lsn.checked_add_bytes(self.offset).ok()?;
        let (record, used) = decode_record_at(&self.body[self.offset..], lsn).ok()?;
        self.offset += used;
        self.remaining -= 1;
        Some(record)
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum LogCodecError {
    EmptyFrame,
    FrameTooShort,
    FrameTooLarge,
    BadMagic { got: u32 },
    LengthMismatch { header: u32, actual: usize },
    CrcMismatch { stored: u32, computed: u32 },
    InvalidRecordLength,
    RecordTooShort { len: usize },
    RecordTooLarge,
    RecordTruncated { needed: usize, available: usize },
    UnknownRecordKind { tag: u8 },
    InvalidFirstLsn { first_lsn: Lsn },
    RecordCountMismatch { expected: u32, actual: u32 },
    TrailingBytes { bytes: usize },
    OffsetOverflow,
}

impl fmt::Display for LogCodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LogCodecError::EmptyFrame => write!(f, "log frame has no records"),
            LogCodecError::FrameTooShort => write!(f, "log frame is shorter than the v1 header"),
            LogCodecError::FrameTooLarge => write!(f, "log frame exceeds the configured bound"),
            LogCodecError::BadMagic { got } => write!(f, "bad log frame magic {got:#010x}"),
            LogCodecError::LengthMismatch { header, actual } => {
                write!(f, "log frame length header {header} != actual {actual}")
            }
            LogCodecError::CrcMismatch { stored, computed } => {
                write!(
                    f,
                    "log frame crc mismatch: stored {stored:#010x}, computed {computed:#010x}"
                )
            }
            LogCodecError::InvalidRecordLength => write!(f, "invalid log record length varint"),
            LogCodecError::RecordTooShort { len } => write!(f, "log record body too short: {len}"),
            LogCodecError::RecordTooLarge => write!(f, "log record exceeds the configured bound"),
            LogCodecError::RecordTruncated { needed, available } => {
                write!(f, "truncated log record: need {needed}, have {available}")
            }
            LogCodecError::UnknownRecordKind { tag } => write!(f, "unknown log record kind {tag}"),
            LogCodecError::InvalidFirstLsn { first_lsn } => {
                write!(f, "log frame first_lsn {first_lsn} precedes the frame header")
            }
            LogCodecError::RecordCountMismatch { expected, actual } => {
                write!(f, "log frame record count {expected} != decoded {actual}")
            }
            LogCodecError::TrailingBytes { bytes } => {
                write!(f, "log frame has {bytes} trailing body bytes")
            }
            LogCodecError::OffsetOverflow => write!(f, "log LSN offset overflow"),
        }
    }
}

impl std::error::Error for LogCodecError {}

#[inline]
pub fn record_encoded_len(payload_len: usize) -> Result<usize, LogCodecError> {
    if payload_len > MAX_RECORD_PAYLOAD_LEN {
        return Err(LogCodecError::RecordTooLarge);
    }
    let body_len =
        RECORD_HEADER_LEN.checked_add(payload_len).ok_or(LogCodecError::RecordTooLarge)?;
    Ok(varint_len_u64(body_len as u64) + body_len)
}

/// Encode one non-empty batch frame at `frame_start`.
///
/// `frame_start` is the LSN of the frame header. The header stores the LSN of
/// the first encoded record: `frame_start + FRAME_HEADER_LEN`.
pub fn encode_batch_frame(
    frame_start: Lsn,
    records: &[RecordRef<'_>],
    out: &mut Vec<u8>,
) -> Result<FrameMeta, LogCodecError> {
    let start_len = out.len();
    match encode_batch_frame_inner(frame_start, records, out) {
        Ok(meta) => Ok(meta),
        Err(error) => {
            out.truncate(start_len);
            Err(error)
        }
    }
}

/// Decode and validate one exact batch frame image.
pub fn decode_batch_frame(bytes: &[u8]) -> Result<DecodedFrame<'_>, LogCodecError> {
    if bytes.len() < FRAME_HEADER_LEN + FRAME_TRAILER_LEN {
        return Err(LogCodecError::FrameTooShort);
    }
    if bytes.len() > MAX_FRAME_LEN {
        return Err(LogCodecError::FrameTooLarge);
    }

    let magic = read_u32(bytes, 0);
    if magic != FRAME_MAGIC {
        return Err(LogCodecError::BadMagic { got: magic });
    }

    let frame_len = read_u32(bytes, 4);
    if frame_len as usize != bytes.len() {
        return Err(LogCodecError::LengthMismatch { header: frame_len, actual: bytes.len() });
    }

    let record_count = read_u32(bytes, 8);
    if record_count == 0 {
        return Err(LogCodecError::EmptyFrame);
    }

    let stored = read_u32(bytes, bytes.len() - FRAME_TRAILER_LEN);
    let computed = crc32c(&bytes[..bytes.len() - FRAME_TRAILER_LEN]);
    if stored != computed {
        return Err(LogCodecError::CrcMismatch { stored, computed });
    }

    let first_lsn = Lsn::new(read_u32(bytes, 12), read_u32(bytes, 16));
    if first_lsn.offset() < FRAME_HEADER_LEN as u32 {
        return Err(LogCodecError::InvalidFirstLsn { first_lsn });
    }
    let body_start = FRAME_HEADER_LEN;
    let body_end = bytes.len() - FRAME_TRAILER_LEN;
    let body = &bytes[body_start..body_end];
    validate_records(body, first_lsn, record_count)?;
    Ok(DecodedFrame { bytes, body_start, body_end, first_lsn, record_count })
}

/// Decode an exact sequence of length-prefixed log records without a frame
/// header/trailer.
///
/// Checkpoint record sections use this shape: the section frame supplies
/// payload integrity, and the payload itself is the same per-record byte
/// contract used inside ordinary log frames.
pub fn decode_record_sequence(
    bytes: &[u8],
    first_lsn: Lsn,
    mut visit: impl FnMut(DecodedRecord<'_>),
) -> Result<u32, LogCodecError> {
    let mut offset = 0usize;
    let mut count = 0u32;
    while offset < bytes.len() {
        let lsn = first_lsn.checked_add_bytes(offset)?;
        let (record, used) = decode_record_at(&bytes[offset..], lsn)?;
        visit(record);
        offset += used;
        count = count.checked_add(1).ok_or(LogCodecError::FrameTooLarge)?;
    }
    Ok(count)
}

fn encode_batch_frame_inner(
    frame_start: Lsn,
    records: &[RecordRef<'_>],
    out: &mut Vec<u8>,
) -> Result<FrameMeta, LogCodecError> {
    if records.is_empty() {
        return Err(LogCodecError::EmptyFrame);
    }

    let record_count = u32::try_from(records.len()).map_err(|_| LogCodecError::FrameTooLarge)?;
    let first_lsn = frame_start.checked_add_bytes(FRAME_HEADER_LEN)?;
    let frame_at = out.len();
    out.resize(frame_at + FRAME_HEADER_LEN, 0);

    for record in records {
        encode_record(*record, out)?;
        if out.len() - frame_at + FRAME_TRAILER_LEN > MAX_FRAME_LEN {
            return Err(LogCodecError::FrameTooLarge);
        }
    }

    let frame_len = out.len() - frame_at + FRAME_TRAILER_LEN;
    let frame_len_u32 = u32::try_from(frame_len).map_err(|_| LogCodecError::FrameTooLarge)?;
    write_header(
        &mut out[frame_at..frame_at + FRAME_HEADER_LEN],
        frame_len_u32,
        record_count,
        first_lsn,
    );

    let crc = crc32c(&out[frame_at..]);
    out.extend_from_slice(&crc.to_le_bytes());

    let frame_end = frame_start.checked_add_bytes(frame_len)?;
    Ok(FrameMeta { frame_start, first_lsn, frame_end, frame_len: frame_len_u32, record_count })
}

pub(crate) fn encode_record(record: RecordRef<'_>, out: &mut Vec<u8>) -> Result<(), LogCodecError> {
    if record.payload.len() > MAX_RECORD_PAYLOAD_LEN {
        return Err(LogCodecError::RecordTooLarge);
    }
    let body_len = RECORD_HEADER_LEN + record.payload.len();
    encode_u64(body_len as u64, out);
    out.push(record.kind.tag());
    out.extend_from_slice(&record.flags.to_le_bytes());
    out.extend_from_slice(&record.namespace.get().to_le_bytes());
    out.extend_from_slice(record.payload);
    Ok(())
}

fn validate_records(body: &[u8], first_lsn: Lsn, expected: u32) -> Result<(), LogCodecError> {
    let mut offset = 0usize;
    let mut actual = 0u32;

    while actual < expected {
        if offset == body.len() {
            return Err(LogCodecError::RecordCountMismatch { expected, actual });
        }
        let lsn = first_lsn.checked_add_bytes(offset)?;
        let (_, used) = decode_record_at(&body[offset..], lsn)?;
        offset += used;
        actual += 1;
    }

    if offset != body.len() {
        return Err(LogCodecError::TrailingBytes { bytes: body.len() - offset });
    }
    Ok(())
}

fn decode_record_at(buf: &[u8], lsn: Lsn) -> Result<(DecodedRecord<'_>, usize), LogCodecError> {
    let (body_len, prefix_len) = decode_u64(buf).ok_or(LogCodecError::InvalidRecordLength)?;
    let body_len = usize::try_from(body_len).map_err(|_| LogCodecError::RecordTooLarge)?;
    if body_len < RECORD_HEADER_LEN {
        return Err(LogCodecError::RecordTooShort { len: body_len });
    }
    if body_len - RECORD_HEADER_LEN > MAX_RECORD_PAYLOAD_LEN {
        return Err(LogCodecError::RecordTooLarge);
    }

    let needed = prefix_len + body_len;
    if buf.len() < needed {
        return Err(LogCodecError::RecordTruncated { needed, available: buf.len() });
    }

    let body = &buf[prefix_len..needed];
    let kind =
        RecordKind::from_tag(body[0]).ok_or(LogCodecError::UnknownRecordKind { tag: body[0] })?;
    let flags = u16::from_le_bytes([body[1], body[2]]);
    let namespace = NamespaceId::new(u32::from_le_bytes([body[3], body[4], body[5], body[6]]));
    let payload = &body[RECORD_HEADER_LEN..];
    let record = RecordRef::new(kind, namespace, flags, payload)?;
    Ok((DecodedRecord { lsn, record }, needed))
}

pub(crate) fn write_header(header: &mut [u8], frame_len: u32, record_count: u32, first_lsn: Lsn) {
    debug_assert_eq!(header.len(), FRAME_HEADER_LEN);
    header[0..4].copy_from_slice(&FRAME_MAGIC.to_le_bytes());
    header[4..8].copy_from_slice(&frame_len.to_le_bytes());
    header[8..12].copy_from_slice(&record_count.to_le_bytes());
    header[12..16].copy_from_slice(&first_lsn.segment().to_le_bytes());
    header[16..20].copy_from_slice(&first_lsn.offset().to_le_bytes());
}

fn read_u32(buf: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([buf[at], buf[at + 1], buf[at + 2], buf[at + 3]])
}

const fn varint_len_u64(mut value: u64) -> usize {
    let mut len = 1;
    while value >= 0x80 {
        value >>= 7;
        len += 1;
    }
    len
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[derive(Clone, Debug)]
    struct OwnedRecord {
        kind: RecordKind,
        namespace: NamespaceId,
        flags: u16,
        payload: Vec<u8>,
    }

    impl OwnedRecord {
        fn as_ref(&self) -> RecordRef<'_> {
            RecordRef::new(self.kind, self.namespace, self.flags, &self.payload).unwrap()
        }
    }

    fn any_record() -> impl Strategy<Value = OwnedRecord> {
        let kind = prop_oneof![
            Just(RecordKind::StringPostImage),
            Just(RecordKind::Delete),
            Just(RecordKind::ExpireAt),
            Just(RecordKind::Namespace),
            Just(RecordKind::CheckpointBegin),
        ];
        (kind, any::<u32>(), any::<u16>(), prop::collection::vec(any::<u8>(), 0..512)).prop_map(
            |(kind, namespace, flags, payload)| OwnedRecord {
                kind,
                namespace: NamespaceId::new(namespace),
                flags,
                payload,
            },
        )
    }

    proptest! {
        #[test]
        fn arbitrary_record_sequences_round_trip(records in prop::collection::vec(any_record(), 1..128)) {
            let refs: Vec<_> = records.iter().map(OwnedRecord::as_ref).collect();
            let mut frame = Vec::new();
            let meta = encode_batch_frame(Lsn::new(7, 4096), &refs, &mut frame)?;
            prop_assert_eq!(meta.record_count() as usize, records.len());
            prop_assert_eq!(meta.frame_len() as usize, frame.len());

            let decoded = decode_batch_frame(&frame)?;
            prop_assert_eq!(decoded.first_lsn(), meta.first_lsn());
            prop_assert_eq!(decoded.record_count() as usize, records.len());
            let got: Vec<_> = decoded.records().collect();
            prop_assert_eq!(got.len(), records.len());

            for (index, decoded) in got.iter().enumerate() {
                let expected = &records[index];
                prop_assert_eq!(decoded.record().kind(), expected.kind);
                prop_assert_eq!(decoded.record().namespace(), expected.namespace);
                prop_assert_eq!(decoded.record().flags(), expected.flags);
                prop_assert_eq!(decoded.record().payload(), expected.payload.as_slice());
            }
        }
    }

    #[test]
    fn record_lsns_follow_encoded_offsets() {
        let one = OwnedRecord {
            kind: RecordKind::StringPostImage,
            namespace: NamespaceId::new(1),
            flags: 0,
            payload: b"abc".to_vec(),
        };
        let two = OwnedRecord {
            kind: RecordKind::Delete,
            namespace: NamespaceId::new(1),
            flags: 9,
            payload: Vec::new(),
        };
        let refs = [one.as_ref(), two.as_ref()];
        let mut frame = Vec::new();
        let meta = encode_batch_frame(Lsn::new(3, 100), &refs, &mut frame).unwrap();
        let decoded = decode_batch_frame(&frame).unwrap();
        let records: Vec<_> = decoded.records().collect();

        assert_eq!(meta.first_lsn(), Lsn::new(3, 120));
        assert_eq!(records[0].lsn(), Lsn::new(3, 120));
        assert_eq!(records[1].lsn(), Lsn::new(3, 120 + refs[0].encoded_len() as u32));
        assert_eq!(meta.frame_end(), Lsn::new(3, 100 + frame.len() as u32));
    }

    #[test]
    fn corrupt_crc_rejects_before_iteration() {
        let record = OwnedRecord {
            kind: RecordKind::Namespace,
            namespace: NamespaceId::new(42),
            flags: 1,
            payload: b"ns".to_vec(),
        };
        let mut frame = Vec::new();
        encode_batch_frame(Lsn::new(0, 0), &[record.as_ref()], &mut frame).unwrap();
        frame[FRAME_HEADER_LEN + 1] ^= 0x80;

        assert!(matches!(decode_batch_frame(&frame), Err(LogCodecError::CrcMismatch { .. })));
    }

    #[test]
    fn frame_decoder_rejects_invalid_record_even_with_valid_crc() {
        let mut body = Vec::new();
        encode_u64((RECORD_HEADER_LEN - 1) as u64, &mut body);
        body.extend_from_slice(&[RecordKind::Delete.tag(), 0, 0, 0, 0, 0]);

        let frame = frame_from_body(body, 1);
        assert!(matches!(decode_batch_frame(&frame), Err(LogCodecError::RecordTooShort { .. })));
    }

    #[test]
    fn noncanonical_record_length_is_rejected() {
        let mut body = vec![0x87, 0x00];
        body.extend_from_slice(&[RecordKind::Delete.tag(), 0, 0, 0, 0, 0, 0]);

        let frame = frame_from_body(body, 1);
        assert!(matches!(decode_batch_frame(&frame), Err(LogCodecError::InvalidRecordLength)));
    }

    #[test]
    fn trailing_body_bytes_are_rejected() {
        let record = OwnedRecord {
            kind: RecordKind::Delete,
            namespace: NamespaceId::new(7),
            flags: 0,
            payload: Vec::new(),
        };
        let mut body = Vec::new();
        encode_record(record.as_ref(), &mut body).unwrap();
        body.push(0xAA);

        let frame = frame_from_body(body, 1);
        assert!(matches!(
            decode_batch_frame(&frame),
            Err(LogCodecError::TrailingBytes { bytes: 1 })
        ));
    }

    #[test]
    fn first_lsn_must_leave_room_for_the_header() {
        let record = OwnedRecord {
            kind: RecordKind::Delete,
            namespace: NamespaceId::new(0),
            flags: 0,
            payload: Vec::new(),
        };
        let mut body = Vec::new();
        encode_record(record.as_ref(), &mut body).unwrap();

        let frame_len = FRAME_HEADER_LEN + body.len() + FRAME_TRAILER_LEN;
        let mut frame = vec![0; FRAME_HEADER_LEN];
        write_header(&mut frame, frame_len as u32, 1, Lsn::new(0, 19));
        frame.extend_from_slice(&body);
        let crc = crc32c(&frame);
        frame.extend_from_slice(&crc.to_le_bytes());

        assert!(matches!(decode_batch_frame(&frame), Err(LogCodecError::InvalidFirstLsn { .. })));
    }

    #[test]
    fn empty_frames_are_rejected() {
        let mut out = Vec::new();
        assert_eq!(
            encode_batch_frame(Lsn::new(0, 0), &[], &mut out),
            Err(LogCodecError::EmptyFrame)
        );
        assert!(out.is_empty());
    }

    fn frame_from_body(body: Vec<u8>, record_count: u32) -> Vec<u8> {
        let frame_len = FRAME_HEADER_LEN + body.len() + FRAME_TRAILER_LEN;
        let mut frame = vec![0; FRAME_HEADER_LEN];
        write_header(&mut frame, frame_len as u32, record_count, Lsn::new(0, 20));
        frame.extend_from_slice(&body);
        let crc = crc32c(&frame);
        frame.extend_from_slice(&crc.to_le_bytes());
        frame
    }
}
