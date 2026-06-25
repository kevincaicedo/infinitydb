use core::fmt;

use inf_simd::crc32c;

use crate::{Lsn, SegmentCatalog, SegmentId};

pub const RECOVERY_MANIFEST_FILE: &str = "MANIFEST";
pub const RECOVERY_MANIFEST_TEMP_FILE: &str = "MANIFEST.tmp";
pub const CHECKPOINT_FILE_PREFIX: &str = "ckpt-";
pub const CHECKPOINT_FILE_SUFFIX: &str = ".ick";
pub const CHECKPOINT_FILE_DIGITS: usize = 6;
pub const CHECKPOINT_FILE_NAME_LEN: usize =
    CHECKPOINT_FILE_PREFIX.len() + CHECKPOINT_FILE_DIGITS + CHECKPOINT_FILE_SUFFIX.len();
pub const MAX_CHECKPOINT_ID: u32 = 999_999;
pub const RECOVERY_MANIFEST_MAGIC: u32 = u32::from_le_bytes(*b"IMF1");
pub const RECOVERY_MANIFEST_VERSION: u16 = 1;
pub const RECOVERY_MANIFEST_HEADER_LEN: usize = 24;
pub const RECOVERY_MANIFEST_TRAILER_LEN: usize = 4;
pub const MAX_RECOVERY_MANIFEST_BYTES: usize = 64 * 1024;
pub const MAX_RECOVERY_MANIFEST_SEGMENTS: usize =
    (MAX_RECOVERY_MANIFEST_BYTES - RECOVERY_MANIFEST_HEADER_LEN - RECOVERY_MANIFEST_TRAILER_LEN)
        / 4;

/// Per-cell checkpoint identifier used by `ckpt-NNNNNN.ick`.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct CheckpointId(u32);

impl CheckpointId {
    pub const ZERO: CheckpointId = CheckpointId(0);
    pub const FIRST_LIVE: CheckpointId = CheckpointId(1);
    pub const MAX: CheckpointId = CheckpointId(MAX_CHECKPOINT_ID);

    #[inline]
    pub const fn new(raw: u32) -> Option<CheckpointId> {
        if raw <= MAX_CHECKPOINT_ID { Some(CheckpointId(raw)) } else { None }
    }

    #[inline]
    pub const fn get(self) -> u32 {
        self.0
    }

    #[inline]
    pub const fn next(self) -> Option<CheckpointId> {
        if self.0 < MAX_CHECKPOINT_ID { Some(CheckpointId(self.0 + 1)) } else { None }
    }

    #[inline]
    pub fn file_name(self) -> String {
        format!(
            "{CHECKPOINT_FILE_PREFIX}{:0width$}{CHECKPOINT_FILE_SUFFIX}",
            self.0,
            width = CHECKPOINT_FILE_DIGITS
        )
    }

    #[inline]
    pub fn parse_file_name(name: &str) -> Result<CheckpointId, CheckpointNameError> {
        parse_checkpoint_name(name)
    }
}

impl fmt::Display for CheckpointId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:0width$}", self.0, width = CHECKPOINT_FILE_DIGITS)
    }
}

/// The checkpoint named by one durable recovery manifest.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct CheckpointRef {
    id: CheckpointId,
    begin_lsn: Lsn,
}

impl CheckpointRef {
    #[inline]
    pub const fn new(id: CheckpointId, begin_lsn: Lsn) -> CheckpointRef {
        CheckpointRef { id, begin_lsn }
    }

    #[inline]
    pub const fn id(self) -> CheckpointId {
        self.id
    }

    #[inline]
    pub const fn begin_lsn(self) -> Lsn {
        self.begin_lsn
    }
}

/// Atomic recovery root: checkpoint image plus the live log segment set.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RecoveryManifest {
    checkpoint: CheckpointRef,
    segments: SegmentCatalog,
}

impl RecoveryManifest {
    pub fn new(
        checkpoint: CheckpointRef,
        segments: SegmentCatalog,
    ) -> Result<RecoveryManifest, RecoveryManifestError> {
        validate_manifest_segments(checkpoint.begin_lsn(), &segments)?;
        Ok(RecoveryManifest { checkpoint, segments })
    }

    #[inline]
    pub const fn checkpoint(&self) -> CheckpointRef {
        self.checkpoint
    }

    #[inline]
    pub const fn begin_lsn(&self) -> Lsn {
        self.checkpoint.begin_lsn
    }

    #[inline]
    pub const fn checkpoint_id(&self) -> CheckpointId {
        self.checkpoint.id
    }

    #[inline]
    pub fn segments(&self) -> &SegmentCatalog {
        &self.segments
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum CheckpointNameError {
    TruncatedCheckpointName { name: String },
    InvalidCheckpointName { name: String },
    InvalidCheckpointId { raw: u32 },
}

impl fmt::Display for CheckpointNameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CheckpointNameError::TruncatedCheckpointName { name } => {
                write!(f, "truncated checkpoint name {name:?}")
            }
            CheckpointNameError::InvalidCheckpointName { name } => {
                write!(f, "invalid checkpoint name {name:?}")
            }
            CheckpointNameError::InvalidCheckpointId { raw } => {
                write!(f, "checkpoint id {raw} exceeds v1 max {MAX_CHECKPOINT_ID}")
            }
        }
    }
}

impl std::error::Error for CheckpointNameError {}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum RecoveryManifestError {
    EmptySegmentSet,
    BeginLsnSegmentInvalid { raw: u32 },
    BeginLsnOutsideSegmentSet { begin: SegmentId, first: SegmentId, last: SegmentId },
    ImageTooLarge { len: usize, max_len: usize },
    ImageTooShort { len: usize, min_len: usize },
    LengthMismatch { expected: usize, got: usize },
    BadMagic { got: u32 },
    UnsupportedVersion { got: u16 },
    BadHeaderLen { got: u16 },
    BadCrc { expected: u32, got: u32 },
    InvalidCheckpointId { raw: u32 },
    SegmentCountTooLarge { count: u32, max_count: usize },
    InvalidSegmentId { raw: u32 },
    NonContiguousSegmentSet { expected: SegmentId, found: SegmentId },
}

impl fmt::Display for RecoveryManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RecoveryManifestError::EmptySegmentSet => {
                write!(f, "recovery manifest segment set must be non-empty")
            }
            RecoveryManifestError::BeginLsnSegmentInvalid { raw } => {
                write!(f, "begin LSN segment {raw} is outside the v1 segment id range")
            }
            RecoveryManifestError::BeginLsnOutsideSegmentSet { begin, first, last } => write!(
                f,
                "begin LSN segment {} is outside manifest segment range {}..={}",
                begin.file_name(),
                first.file_name(),
                last.file_name()
            ),
            RecoveryManifestError::ImageTooLarge { len, max_len } => {
                write!(f, "recovery manifest image is {len} bytes, above max {max_len}")
            }
            RecoveryManifestError::ImageTooShort { len, min_len } => {
                write!(f, "recovery manifest image is {len} bytes, below minimum {min_len}")
            }
            RecoveryManifestError::LengthMismatch { expected, got } => {
                write!(f, "recovery manifest image is {got} bytes, expected {expected}")
            }
            RecoveryManifestError::BadMagic { got } => {
                write!(f, "bad recovery manifest magic 0x{got:08x}")
            }
            RecoveryManifestError::UnsupportedVersion { got } => {
                write!(f, "unsupported recovery manifest version {got}")
            }
            RecoveryManifestError::BadHeaderLen { got } => {
                write!(f, "bad recovery manifest header length {got}")
            }
            RecoveryManifestError::BadCrc { expected, got } => write!(
                f,
                "bad recovery manifest crc32c: expected 0x{expected:08x}, got 0x{got:08x}"
            ),
            RecoveryManifestError::InvalidCheckpointId { raw } => {
                write!(f, "checkpoint id {raw} exceeds v1 max {MAX_CHECKPOINT_ID}")
            }
            RecoveryManifestError::SegmentCountTooLarge { count, max_count } => {
                write!(f, "recovery manifest segment count {count} exceeds max {max_count}")
            }
            RecoveryManifestError::InvalidSegmentId { raw } => {
                write!(f, "segment id {raw} exceeds v1 max")
            }
            RecoveryManifestError::NonContiguousSegmentSet { expected, found } => write!(
                f,
                "manifest segment set gap: expected {}, found {}",
                expected.file_name(),
                found.file_name()
            ),
        }
    }
}

impl std::error::Error for RecoveryManifestError {}

pub fn encode_recovery_manifest(
    manifest: &RecoveryManifest,
    out: &mut Vec<u8>,
) -> Result<(), RecoveryManifestError> {
    validate_manifest_segments(manifest.begin_lsn(), manifest.segments())?;

    out.clear();
    out.reserve_exact(
        RECOVERY_MANIFEST_HEADER_LEN
            + manifest.segments().len() * 4
            + RECOVERY_MANIFEST_TRAILER_LEN,
    );
    put_u32(out, RECOVERY_MANIFEST_MAGIC);
    put_u16(out, RECOVERY_MANIFEST_VERSION);
    put_u16(out, RECOVERY_MANIFEST_HEADER_LEN as u16);
    put_u32(out, manifest.checkpoint_id().get());
    put_u32(out, manifest.begin_lsn().segment());
    put_u32(out, manifest.begin_lsn().offset());
    put_u32(out, manifest.segments().len() as u32);
    for segment in manifest.segments().iter() {
        put_u32(out, segment.get());
    }
    append_crc(out);
    debug_assert!(out.len() <= MAX_RECOVERY_MANIFEST_BYTES);
    Ok(())
}

pub fn decode_recovery_manifest(bytes: &[u8]) -> Result<RecoveryManifest, RecoveryManifestError> {
    validate_manifest_len(bytes)?;
    validate_manifest_header(bytes)?;

    let segment_count = read_u32(bytes, 20);
    if segment_count as usize > MAX_RECOVERY_MANIFEST_SEGMENTS {
        return Err(RecoveryManifestError::SegmentCountTooLarge {
            count: segment_count,
            max_count: MAX_RECOVERY_MANIFEST_SEGMENTS,
        });
    }
    let expected_len =
        RECOVERY_MANIFEST_HEADER_LEN + segment_count as usize * 4 + RECOVERY_MANIFEST_TRAILER_LEN;
    if bytes.len() != expected_len {
        return Err(RecoveryManifestError::LengthMismatch {
            expected: expected_len,
            got: bytes.len(),
        });
    }

    validate_manifest_crc(bytes)?;

    let checkpoint_id = CheckpointId::new(read_u32(bytes, 8))
        .ok_or(RecoveryManifestError::InvalidCheckpointId { raw: read_u32(bytes, 8) })?;
    let begin_lsn = Lsn::new(read_u32(bytes, 12), read_u32(bytes, 16));
    let checkpoint = CheckpointRef::new(checkpoint_id, begin_lsn);
    let segments = decode_segment_catalog(bytes, segment_count as usize)?;

    RecoveryManifest::new(checkpoint, segments)
}

fn validate_manifest_segments(
    begin_lsn: Lsn,
    segments: &SegmentCatalog,
) -> Result<(), RecoveryManifestError> {
    if segments.is_empty() {
        return Err(RecoveryManifestError::EmptySegmentSet);
    }
    let begin = SegmentId::new(begin_lsn.segment())
        .ok_or(RecoveryManifestError::BeginLsnSegmentInvalid { raw: begin_lsn.segment() })?;
    let first = segments.first().expect("non-empty manifest segment set has first");
    let last = segments.last().expect("non-empty manifest segment set has last");
    if begin < first || begin > last {
        return Err(RecoveryManifestError::BeginLsnOutsideSegmentSet { begin, first, last });
    }
    Ok(())
}

fn validate_manifest_len(bytes: &[u8]) -> Result<(), RecoveryManifestError> {
    if bytes.len() > MAX_RECOVERY_MANIFEST_BYTES {
        return Err(RecoveryManifestError::ImageTooLarge {
            len: bytes.len(),
            max_len: MAX_RECOVERY_MANIFEST_BYTES,
        });
    }
    let min_len = RECOVERY_MANIFEST_HEADER_LEN + RECOVERY_MANIFEST_TRAILER_LEN;
    if bytes.len() < min_len {
        return Err(RecoveryManifestError::ImageTooShort { len: bytes.len(), min_len });
    }
    Ok(())
}

fn validate_manifest_header(bytes: &[u8]) -> Result<(), RecoveryManifestError> {
    let magic = read_u32(bytes, 0);
    if magic != RECOVERY_MANIFEST_MAGIC {
        return Err(RecoveryManifestError::BadMagic { got: magic });
    }
    let version = read_u16(bytes, 4);
    if version != RECOVERY_MANIFEST_VERSION {
        return Err(RecoveryManifestError::UnsupportedVersion { got: version });
    }
    let header_len = read_u16(bytes, 6);
    if header_len as usize != RECOVERY_MANIFEST_HEADER_LEN {
        return Err(RecoveryManifestError::BadHeaderLen { got: header_len });
    }
    Ok(())
}

fn validate_manifest_crc(bytes: &[u8]) -> Result<(), RecoveryManifestError> {
    let crc_at = bytes.len() - RECOVERY_MANIFEST_TRAILER_LEN;
    let expected = crc32c(&bytes[..crc_at]);
    let got = read_u32(bytes, crc_at);
    if got != expected {
        return Err(RecoveryManifestError::BadCrc { expected, got });
    }
    Ok(())
}

fn decode_segment_catalog(
    bytes: &[u8],
    count: usize,
) -> Result<SegmentCatalog, RecoveryManifestError> {
    if count == 0 {
        return Err(RecoveryManifestError::EmptySegmentSet);
    }

    let mut segments: Vec<SegmentId> = Vec::with_capacity(count);
    for index in 0..count {
        let raw = read_u32(bytes, RECOVERY_MANIFEST_HEADER_LEN + index * 4);
        let segment = SegmentId::new(raw).ok_or(RecoveryManifestError::InvalidSegmentId { raw })?;
        if let Some(previous) = segments.last().copied() {
            let expected =
                previous.checked_next().ok_or(RecoveryManifestError::NonContiguousSegmentSet {
                    expected: previous,
                    found: segment,
                })?;
            if segment != expected {
                return Err(RecoveryManifestError::NonContiguousSegmentSet {
                    expected,
                    found: segment,
                });
            }
        }
        segments.push(segment);
    }

    Ok(SegmentCatalog::from_validated_contiguous(segments))
}

fn parse_checkpoint_name(name: &str) -> Result<CheckpointId, CheckpointNameError> {
    if name.starts_with(CHECKPOINT_FILE_PREFIX) && name.len() < CHECKPOINT_FILE_NAME_LEN {
        return Err(CheckpointNameError::TruncatedCheckpointName { name: name.to_owned() });
    }
    if name.len() != CHECKPOINT_FILE_NAME_LEN
        || !name.starts_with(CHECKPOINT_FILE_PREFIX)
        || !name.ends_with(CHECKPOINT_FILE_SUFFIX)
    {
        return Err(CheckpointNameError::InvalidCheckpointName { name: name.to_owned() });
    }

    let digits = &name.as_bytes()
        [CHECKPOINT_FILE_PREFIX.len()..CHECKPOINT_FILE_PREFIX.len() + CHECKPOINT_FILE_DIGITS];
    let mut raw = 0u32;
    for digit in digits {
        if !digit.is_ascii_digit() {
            return Err(CheckpointNameError::InvalidCheckpointName { name: name.to_owned() });
        }
        raw = raw * 10 + u32::from(digit - b'0');
    }

    CheckpointId::new(raw).ok_or(CheckpointNameError::InvalidCheckpointId { raw })
}

fn append_crc(out: &mut Vec<u8>) {
    let crc = crc32c(out);
    put_u32(out, crc);
}

fn put_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn read_u16(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn read_u32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SegmentId;

    fn segment_catalog(start: u32, len: u32) -> SegmentCatalog {
        let names: Vec<String> =
            (start..start + len).map(|raw| SegmentId::new(raw).unwrap().file_name()).collect();
        crate::scan_segment_names(names.iter().map(String::as_str)).unwrap()
    }

    fn manifest() -> RecoveryManifest {
        RecoveryManifest::new(
            CheckpointRef::new(CheckpointId::new(7).unwrap(), Lsn::new(3, 128)),
            segment_catalog(2, 4),
        )
        .unwrap()
    }

    fn encode(mut manifest: RecoveryManifest) -> Vec<u8> {
        let mut out = Vec::new();
        encode_recovery_manifest(&manifest, &mut out).unwrap();
        manifest = decode_recovery_manifest(&out).unwrap();
        assert_eq!(manifest.checkpoint_id(), CheckpointId::new(7).unwrap());
        out
    }

    fn rewrite_crc(bytes: &mut Vec<u8>) {
        bytes.truncate(bytes.len() - RECOVERY_MANIFEST_TRAILER_LEN);
        append_crc(bytes);
    }

    #[test]
    fn checkpoint_names_round_trip() {
        let checkpoint = CheckpointId::new(42).unwrap();
        let name = checkpoint.file_name();

        assert_eq!(name, "ckpt-000042.ick");
        assert_eq!(CheckpointId::parse_file_name(&name), Ok(checkpoint));
        assert_eq!(
            CheckpointId::parse_file_name("ckpt-00001"),
            Err(CheckpointNameError::TruncatedCheckpointName { name: "ckpt-00001".to_string() })
        );
        assert!(CheckpointId::parse_file_name("ckpt-00x042.ick").is_err());
    }

    #[test]
    fn checkpoint_id_next_stops_at_max() {
        assert_eq!(CheckpointId::ZERO.next(), Some(CheckpointId::FIRST_LIVE));
        assert_eq!(CheckpointId::new(42).unwrap().next(), CheckpointId::new(43));
        assert_eq!(CheckpointId::MAX.next(), None);
    }

    #[test]
    fn recovery_manifest_round_trips_checkpoint_and_segments() {
        let expected = manifest();
        let mut bytes = Vec::new();

        encode_recovery_manifest(&expected, &mut bytes).unwrap();
        let got = decode_recovery_manifest(&bytes).unwrap();

        assert_eq!(got, expected);
        assert_eq!(got.checkpoint_id().file_name(), "ckpt-000007.ick");
        assert_eq!(got.begin_lsn(), Lsn::new(3, 128));
        assert_eq!(got.segments().as_slice(), segment_catalog(2, 4).as_slice());
    }

    #[test]
    fn recovery_manifest_rejects_bad_crc() {
        let mut bytes = encode(manifest());
        bytes[12] ^= 0x80;

        assert!(matches!(
            decode_recovery_manifest(&bytes),
            Err(RecoveryManifestError::BadCrc { .. })
        ));
    }

    #[test]
    fn recovery_manifest_rejects_bad_magic_and_version() {
        let mut bad_magic = encode(manifest());
        bad_magic[0..4].copy_from_slice(&u32::to_le_bytes(0));
        rewrite_crc(&mut bad_magic);
        assert_eq!(
            decode_recovery_manifest(&bad_magic),
            Err(RecoveryManifestError::BadMagic { got: 0 })
        );

        let mut bad_version = encode(manifest());
        bad_version[4..6].copy_from_slice(&u16::to_le_bytes(2));
        rewrite_crc(&mut bad_version);
        assert_eq!(
            decode_recovery_manifest(&bad_version),
            Err(RecoveryManifestError::UnsupportedVersion { got: 2 })
        );
    }

    #[test]
    fn recovery_manifest_rejects_truncated_and_trailing_images() {
        let bytes = encode(manifest());
        assert!(matches!(
            decode_recovery_manifest(&bytes[..bytes.len() - 1]),
            Err(RecoveryManifestError::LengthMismatch { .. })
                | Err(RecoveryManifestError::BadCrc { .. })
        ));

        let mut trailing = bytes;
        trailing.extend_from_slice(&[0]);
        assert!(matches!(
            decode_recovery_manifest(&trailing),
            Err(RecoveryManifestError::LengthMismatch { .. })
        ));
    }

    #[test]
    fn recovery_manifest_rejects_empty_and_noncontiguous_segment_sets() {
        let error = RecoveryManifest::new(
            CheckpointRef::new(CheckpointId::ZERO, Lsn::new(0, 0)),
            SegmentCatalog::empty(),
        )
        .unwrap_err();
        assert_eq!(error, RecoveryManifestError::EmptySegmentSet);

        let mut bytes = encode(manifest());
        bytes[RECOVERY_MANIFEST_HEADER_LEN + 4..RECOVERY_MANIFEST_HEADER_LEN + 8]
            .copy_from_slice(&u32::to_le_bytes(4));
        rewrite_crc(&mut bytes);

        assert_eq!(
            decode_recovery_manifest(&bytes),
            Err(RecoveryManifestError::NonContiguousSegmentSet {
                expected: SegmentId::new(3).unwrap(),
                found: SegmentId::new(4).unwrap(),
            })
        );
    }

    #[test]
    fn recovery_manifest_rejects_begin_lsn_outside_segment_set() {
        let error = RecoveryManifest::new(
            CheckpointRef::new(CheckpointId::ZERO, Lsn::new(10, 0)),
            segment_catalog(2, 4),
        )
        .unwrap_err();

        assert_eq!(
            error,
            RecoveryManifestError::BeginLsnOutsideSegmentSet {
                begin: SegmentId::new(10).unwrap(),
                first: SegmentId::new(2).unwrap(),
                last: SegmentId::new(5).unwrap(),
            }
        );
    }
}
