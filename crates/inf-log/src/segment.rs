use core::fmt;

use crate::MAX_FRAME_LEN;

pub const SEGMENT_FILE_PREFIX: &str = "seg-";
pub const SEGMENT_FILE_SUFFIX: &str = ".ilog";
pub const SEGMENT_FILE_DIGITS: usize = 6;
pub const SEGMENT_FILE_NAME_LEN: usize =
    SEGMENT_FILE_PREFIX.len() + SEGMENT_FILE_DIGITS + SEGMENT_FILE_SUFFIX.len();
pub const MAX_SEGMENT_ID: u32 = 999_999;
pub const DEFAULT_SEGMENT_SIZE_BYTES: u32 = 256 * 1024 * 1024;
pub const DEFAULT_SEGMENT_FRAME_MAX_BYTES: u32 = MAX_FRAME_LEN as u32;
pub const DEFAULT_PREALLOCATE_THRESHOLD_BYTES: u32 = DEFAULT_SEGMENT_FRAME_MAX_BYTES;

/// Per-cell log segment identifier for the v1 segment filename contract.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct SegmentId(u32);

impl SegmentId {
    pub const ZERO: SegmentId = SegmentId(0);
    pub const MAX: SegmentId = SegmentId(MAX_SEGMENT_ID);

    #[inline]
    pub const fn new(raw: u32) -> Option<SegmentId> {
        if raw <= MAX_SEGMENT_ID { Some(SegmentId(raw)) } else { None }
    }

    #[inline]
    pub const fn get(self) -> u32 {
        self.0
    }

    #[inline]
    pub fn checked_next(self) -> Option<SegmentId> {
        SegmentId::new(self.0.checked_add(1)?)
    }

    #[inline]
    pub fn file_name(self) -> String {
        format!(
            "{SEGMENT_FILE_PREFIX}{:0width$}{SEGMENT_FILE_SUFFIX}",
            self.0,
            width = SEGMENT_FILE_DIGITS
        )
    }

    #[inline]
    pub fn parse_file_name(name: &str) -> Result<SegmentId, SegmentScanError> {
        parse_segment_name(name)
    }
}

impl fmt::Display for SegmentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:0width$}", self.0, width = SEGMENT_FILE_DIGITS)
    }
}

/// Cold boot view of the contiguous per-cell segment set.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SegmentCatalog {
    segments: Vec<SegmentId>,
}

impl SegmentCatalog {
    #[inline]
    pub fn empty() -> SegmentCatalog {
        SegmentCatalog { segments: Vec::new() }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.segments.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.segments.is_empty()
    }

    #[inline]
    pub fn first(&self) -> Option<SegmentId> {
        self.segments.first().copied()
    }

    #[inline]
    pub fn last(&self) -> Option<SegmentId> {
        self.segments.last().copied()
    }

    #[inline]
    pub fn next_segment_id(&self) -> Option<SegmentId> {
        match self.last() {
            Some(last) => last.checked_next(),
            None => Some(SegmentId::ZERO),
        }
    }

    #[inline]
    pub fn as_slice(&self) -> &[SegmentId] {
        &self.segments
    }

    #[inline]
    pub fn iter(&self) -> impl ExactSizeIterator<Item = SegmentId> + '_ {
        self.segments.iter().copied()
    }

    #[inline]
    pub fn into_vec(self) -> Vec<SegmentId> {
        self.segments
    }

    #[inline]
    pub(crate) fn from_validated_contiguous(segments: Vec<SegmentId>) -> SegmentCatalog {
        SegmentCatalog { segments }
    }
}

/// Cold-path directory reader injected by boot/recovery code.
///
/// `inf-log` validates segment names, but it does not perform filesystem I/O.
/// Runtime or boot code owns the concrete directory walk and pushes only entry
/// names into `out`.
pub trait SegmentDirectory {
    type Error;

    fn read_segment_names(&mut self, out: &mut Vec<String>) -> Result<(), Self::Error>;
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SegmentDirectoryError<E> {
    Read(E),
    Scan(SegmentScanError),
}

impl<E: fmt::Display> fmt::Display for SegmentDirectoryError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SegmentDirectoryError::Read(error) => {
                write!(f, "segment directory read failed: {error}")
            }
            SegmentDirectoryError::Scan(error) => error.fmt(f),
        }
    }
}

impl<E> std::error::Error for SegmentDirectoryError<E> where E: std::error::Error + 'static {}

/// Read and validate one segment directory through an injected reader.
///
/// The caller owns `scratch` so boot can reuse one allocation across cells.
pub fn scan_segment_directory<D>(
    directory: &mut D,
    scratch: &mut Vec<String>,
) -> Result<SegmentCatalog, SegmentDirectoryError<D::Error>>
where
    D: SegmentDirectory,
{
    scratch.clear();
    directory.read_segment_names(scratch).map_err(SegmentDirectoryError::Read)?;
    scan_segment_names(scratch.iter().map(String::as_str)).map_err(SegmentDirectoryError::Scan)
}

/// Segment rotation thresholds for one cell.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct SegmentConfig {
    segment_size_bytes: u32,
    frame_size_max_bytes: u32,
    preallocate_threshold_bytes: u32,
}

impl SegmentConfig {
    pub const DEFAULT: SegmentConfig = SegmentConfig {
        segment_size_bytes: DEFAULT_SEGMENT_SIZE_BYTES,
        frame_size_max_bytes: DEFAULT_SEGMENT_FRAME_MAX_BYTES,
        preallocate_threshold_bytes: DEFAULT_PREALLOCATE_THRESHOLD_BYTES,
    };

    pub fn new(
        segment_size_bytes: u32,
        frame_size_max_bytes: u32,
        preallocate_threshold_bytes: u32,
    ) -> Result<SegmentConfig, SegmentLifecycleError> {
        if segment_size_bytes == 0 {
            return Err(SegmentLifecycleError::ZeroSegmentSize);
        }
        if frame_size_max_bytes == 0 {
            return Err(SegmentLifecycleError::ZeroFrameSizeMax);
        }
        if frame_size_max_bytes >= segment_size_bytes {
            return Err(SegmentLifecycleError::FrameSizeMaxExceedsSegment {
                frame_size_max_bytes,
                segment_size_bytes,
            });
        }
        if preallocate_threshold_bytes < frame_size_max_bytes {
            return Err(SegmentLifecycleError::PreallocateThresholdTooSmall {
                preallocate_threshold_bytes,
                frame_size_max_bytes,
            });
        }
        if preallocate_threshold_bytes >= segment_size_bytes {
            return Err(SegmentLifecycleError::PreallocateThresholdTooLarge {
                preallocate_threshold_bytes,
                segment_size_bytes,
            });
        }
        Ok(SegmentConfig { segment_size_bytes, frame_size_max_bytes, preallocate_threshold_bytes })
    }

    #[inline]
    pub const fn segment_size_bytes(self) -> u32 {
        self.segment_size_bytes
    }

    #[inline]
    pub const fn frame_size_max_bytes(self) -> u32 {
        self.frame_size_max_bytes
    }

    #[inline]
    pub const fn preallocate_threshold_bytes(self) -> u32 {
        self.preallocate_threshold_bytes
    }
}

impl Default for SegmentConfig {
    fn default() -> SegmentConfig {
        SegmentConfig::DEFAULT
    }
}

/// File operation intent produced by cold maintenance logic.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum SegmentMaintenance {
    Preallocate { segment: SegmentId, len_bytes: u32 },
}

/// A segment made immutable by a placement decision.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct SegmentSeal {
    segment: SegmentId,
    used_bytes: u32,
}

impl SegmentSeal {
    #[inline]
    pub const fn segment(self) -> SegmentId {
        self.segment
    }

    #[inline]
    pub const fn used_bytes(self) -> u32 {
        self.used_bytes
    }
}

/// Where one encoded frame must be written.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct FramePlacement {
    segment: SegmentId,
    offset_bytes: u32,
    len_bytes: u32,
    sealed: Option<SegmentSeal>,
}

impl FramePlacement {
    #[inline]
    pub const fn segment(self) -> SegmentId {
        self.segment
    }

    #[inline]
    pub const fn offset_bytes(self) -> u32 {
        self.offset_bytes
    }

    #[inline]
    pub const fn len_bytes(self) -> u32 {
        self.len_bytes
    }

    #[inline]
    pub const fn sealed(self) -> Option<SegmentSeal> {
        self.sealed
    }
}

/// Deterministic segment placement and rotation state for one cell.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct SegmentLifecycle {
    config: SegmentConfig,
    active: SegmentId,
    offset_bytes: u32,
    prepared_next: Option<SegmentId>,
}

impl SegmentLifecycle {
    pub fn open(
        active: SegmentId,
        offset_bytes: u32,
        config: SegmentConfig,
    ) -> Result<SegmentLifecycle, SegmentLifecycleError> {
        if offset_bytes > config.segment_size_bytes {
            return Err(SegmentLifecycleError::ActiveOffsetPastSegment {
                offset_bytes,
                segment_size_bytes: config.segment_size_bytes,
            });
        }
        Ok(SegmentLifecycle { config, active, offset_bytes, prepared_next: None })
    }

    #[inline]
    pub const fn active_segment(self) -> SegmentId {
        self.active
    }

    #[inline]
    pub const fn active_offset_bytes(self) -> u32 {
        self.offset_bytes
    }

    #[inline]
    pub const fn prepared_next(self) -> Option<SegmentId> {
        self.prepared_next
    }

    #[inline]
    pub const fn config(self) -> SegmentConfig {
        self.config
    }

    #[inline]
    pub const fn remaining_bytes(self) -> u32 {
        self.config.segment_size_bytes - self.offset_bytes
    }

    pub fn maintenance_request(self) -> Result<Option<SegmentMaintenance>, SegmentLifecycleError> {
        if self.prepared_next.is_some() {
            return Ok(None);
        }
        if self.remaining_bytes() > self.config.preallocate_threshold_bytes {
            return Ok(None);
        }
        let segment = self
            .active
            .checked_next()
            .ok_or(SegmentLifecycleError::SegmentIdExhausted { active: self.active })?;
        Ok(Some(SegmentMaintenance::Preallocate {
            segment,
            len_bytes: self.config.segment_size_bytes,
        }))
    }

    pub fn mark_preallocated(&mut self, segment: SegmentId) -> Result<(), SegmentLifecycleError> {
        let expected = self
            .active
            .checked_next()
            .ok_or(SegmentLifecycleError::SegmentIdExhausted { active: self.active })?;
        if segment != expected {
            return Err(SegmentLifecycleError::UnexpectedPreparedSegment {
                expected,
                got: segment,
            });
        }
        self.prepared_next = Some(segment);
        Ok(())
    }

    pub fn place_frame(&mut self, len_bytes: u32) -> Result<FramePlacement, SegmentLifecycleError> {
        self.validate_frame_len(len_bytes)?;
        if len_bytes <= self.remaining_bytes() {
            return Ok(self.place_in_active(len_bytes));
        }

        let sealed = self.rotate_to_prepared()?;
        let placement = FramePlacement {
            segment: self.active,
            offset_bytes: 0,
            len_bytes,
            sealed: Some(sealed),
        };
        self.offset_bytes = len_bytes;
        Ok(placement)
    }

    fn validate_frame_len(self, len_bytes: u32) -> Result<(), SegmentLifecycleError> {
        if len_bytes == 0 {
            return Err(SegmentLifecycleError::ZeroFrame);
        }
        if len_bytes > self.config.frame_size_max_bytes {
            return Err(SegmentLifecycleError::FrameTooLarge {
                len_bytes,
                frame_size_max_bytes: self.config.frame_size_max_bytes,
            });
        }
        Ok(())
    }

    fn place_in_active(&mut self, len_bytes: u32) -> FramePlacement {
        let offset_bytes = self.offset_bytes;
        self.offset_bytes += len_bytes;
        let sealed = if self.offset_bytes == self.config.segment_size_bytes {
            Some(SegmentSeal { segment: self.active, used_bytes: self.offset_bytes })
        } else {
            None
        };
        FramePlacement { segment: self.active, offset_bytes, len_bytes, sealed }
    }

    fn rotate_to_prepared(&mut self) -> Result<SegmentSeal, SegmentLifecycleError> {
        let Some(next) = self.prepared_next.take() else {
            return Err(SegmentLifecycleError::NextSegmentNotReady {
                active: self.active,
                remaining_bytes: self.remaining_bytes(),
            });
        };
        let sealed = SegmentSeal { segment: self.active, used_bytes: self.offset_bytes };
        self.active = next;
        self.offset_bytes = 0;
        Ok(sealed)
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SegmentLifecycleError {
    ZeroSegmentSize,
    ZeroFrameSizeMax,
    ZeroFrame,
    FrameSizeMaxExceedsSegment { frame_size_max_bytes: u32, segment_size_bytes: u32 },
    PreallocateThresholdTooSmall { preallocate_threshold_bytes: u32, frame_size_max_bytes: u32 },
    PreallocateThresholdTooLarge { preallocate_threshold_bytes: u32, segment_size_bytes: u32 },
    ActiveOffsetPastSegment { offset_bytes: u32, segment_size_bytes: u32 },
    FrameTooLarge { len_bytes: u32, frame_size_max_bytes: u32 },
    NextSegmentNotReady { active: SegmentId, remaining_bytes: u32 },
    UnexpectedPreparedSegment { expected: SegmentId, got: SegmentId },
    SegmentIdExhausted { active: SegmentId },
}

impl fmt::Display for SegmentLifecycleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SegmentLifecycleError::ZeroSegmentSize => write!(f, "segment size must be nonzero"),
            SegmentLifecycleError::ZeroFrameSizeMax => write!(f, "frame size max must be nonzero"),
            SegmentLifecycleError::ZeroFrame => write!(f, "frame length must be nonzero"),
            SegmentLifecycleError::FrameSizeMaxExceedsSegment {
                frame_size_max_bytes,
                segment_size_bytes,
            } => write!(
                f,
                "frame size max {frame_size_max_bytes} must be below segment size {segment_size_bytes}"
            ),
            SegmentLifecycleError::PreallocateThresholdTooSmall {
                preallocate_threshold_bytes,
                frame_size_max_bytes,
            } => write!(
                f,
                "preallocate threshold {preallocate_threshold_bytes} is below frame max {frame_size_max_bytes}"
            ),
            SegmentLifecycleError::PreallocateThresholdTooLarge {
                preallocate_threshold_bytes,
                segment_size_bytes,
            } => write!(
                f,
                "preallocate threshold {preallocate_threshold_bytes} must be below segment size {segment_size_bytes}"
            ),
            SegmentLifecycleError::ActiveOffsetPastSegment { offset_bytes, segment_size_bytes } => {
                write!(f, "active offset {offset_bytes} exceeds segment size {segment_size_bytes}")
            }
            SegmentLifecycleError::FrameTooLarge { len_bytes, frame_size_max_bytes } => {
                write!(f, "frame length {len_bytes} exceeds configured max {frame_size_max_bytes}")
            }
            SegmentLifecycleError::NextSegmentNotReady { active, remaining_bytes } => write!(
                f,
                "next segment after {} is not preallocated; remaining bytes {remaining_bytes}",
                active.file_name()
            ),
            SegmentLifecycleError::UnexpectedPreparedSegment { expected, got } => {
                write!(f, "prepared segment {}, expected {}", got.file_name(), expected.file_name())
            }
            SegmentLifecycleError::SegmentIdExhausted { active } => {
                write!(f, "segment id exhausted at {}", active.file_name())
            }
        }
    }
}

impl std::error::Error for SegmentLifecycleError {}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SegmentScanError {
    InvalidSegmentId { raw: u32 },
    TruncatedSegmentName { name: String },
    InvalidSegmentName { name: String },
    DuplicateSegment { segment: SegmentId },
    SegmentGap { expected: SegmentId, found: SegmentId },
}

impl fmt::Display for SegmentScanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SegmentScanError::InvalidSegmentId { raw } => {
                write!(f, "segment id {raw} exceeds v1 max {MAX_SEGMENT_ID}")
            }
            SegmentScanError::TruncatedSegmentName { name } => {
                write!(f, "truncated log segment name {name:?}")
            }
            SegmentScanError::InvalidSegmentName { name } => {
                write!(f, "invalid log segment name {name:?}")
            }
            SegmentScanError::DuplicateSegment { segment } => {
                write!(f, "duplicate log segment {}", segment.file_name())
            }
            SegmentScanError::SegmentGap { expected, found } => {
                write!(
                    f,
                    "log segment gap: expected {}, found {}",
                    expected.file_name(),
                    found.file_name()
                )
            }
        }
    }
}

impl std::error::Error for SegmentScanError {}

/// Validate a cold boot segment directory listing.
///
/// Every supplied name must be a v1 segment filename. The returned catalog is
/// numerically sorted and gap-free. Empty listings are valid for first boot.
pub fn scan_segment_names<'a, I>(names: I) -> Result<SegmentCatalog, SegmentScanError>
where
    I: IntoIterator<Item = &'a str>,
{
    let mut segments = Vec::new();
    for name in names {
        segments.push(parse_segment_name(name)?);
    }

    if segments.is_empty() {
        return Ok(SegmentCatalog::empty());
    }

    segments.sort_unstable();
    validate_contiguous(&segments)?;
    Ok(SegmentCatalog { segments })
}

fn parse_segment_name(name: &str) -> Result<SegmentId, SegmentScanError> {
    if name.starts_with(SEGMENT_FILE_PREFIX) && name.len() < SEGMENT_FILE_NAME_LEN {
        return Err(SegmentScanError::TruncatedSegmentName { name: name.to_owned() });
    }
    if name.len() != SEGMENT_FILE_NAME_LEN
        || !name.starts_with(SEGMENT_FILE_PREFIX)
        || !name.ends_with(SEGMENT_FILE_SUFFIX)
    {
        return Err(SegmentScanError::InvalidSegmentName { name: name.to_owned() });
    }

    let digits = &name.as_bytes()
        [SEGMENT_FILE_PREFIX.len()..SEGMENT_FILE_PREFIX.len() + SEGMENT_FILE_DIGITS];
    let mut raw = 0u32;
    for digit in digits {
        if !digit.is_ascii_digit() {
            return Err(SegmentScanError::InvalidSegmentName { name: name.to_owned() });
        }
        raw = raw * 10 + u32::from(digit - b'0');
    }

    SegmentId::new(raw).ok_or(SegmentScanError::InvalidSegmentId { raw })
}

fn validate_contiguous(segments: &[SegmentId]) -> Result<(), SegmentScanError> {
    let mut previous = segments[0];
    for segment in &segments[1..] {
        if *segment == previous {
            return Err(SegmentScanError::DuplicateSegment { segment: *segment });
        }
        let expected = previous.checked_next().unwrap_or(SegmentId::MAX);
        if *segment != expected {
            return Err(SegmentScanError::SegmentGap { expected, found: *segment });
        }
        previous = *segment;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn name(raw: u32) -> String {
        SegmentId::new(raw).unwrap().file_name()
    }

    fn small_config() -> SegmentConfig {
        SegmentConfig::new(128, 32, 32).unwrap()
    }

    #[derive(Clone, PartialEq, Eq, Debug)]
    struct TestReadError;

    impl fmt::Display for TestReadError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "test read error")
        }
    }

    impl std::error::Error for TestReadError {}

    struct TestDirectory {
        names: Vec<String>,
        error: Option<TestReadError>,
    }

    impl SegmentDirectory for TestDirectory {
        type Error = TestReadError;

        fn read_segment_names(&mut self, out: &mut Vec<String>) -> Result<(), Self::Error> {
            if let Some(error) = self.error.take() {
                return Err(error);
            }
            out.extend(self.names.iter().cloned());
            Ok(())
        }
    }

    proptest! {
        #[test]
        fn segment_name_round_trips(raw in 0..=MAX_SEGMENT_ID) {
            let segment = SegmentId::new(raw).unwrap();
            let file_name = segment.file_name();

            prop_assert_eq!(file_name.len(), SEGMENT_FILE_NAME_LEN);
            prop_assert_eq!(SegmentId::parse_file_name(&file_name)?, segment);
        }

        #[test]
        fn contiguous_segment_sets_scan_sorted(start in 0..=999_900u32, len in 0usize..=64) {
            let end = (start as usize + len).min(MAX_SEGMENT_ID as usize + 1);
            let mut names: Vec<String> = (start..end as u32).map(name).collect();
            names.reverse();

            let catalog = scan_segment_names(names.iter().map(String::as_str))?;
            let expected: Vec<SegmentId> = (start..end as u32).map(|raw| SegmentId::new(raw).unwrap()).collect();

            prop_assert_eq!(catalog.as_slice(), expected.as_slice());
            prop_assert_eq!(catalog.len(), expected.len());
        }

        #[test]
        fn duplicate_segment_sets_are_rejected(start in 0..=999_950u32, len in 1usize..=32) {
            let end = (start as usize + len).min(MAX_SEGMENT_ID as usize + 1);
            let mut names: Vec<String> = (start..end as u32).map(name).collect();
            names.push(name(start));

            let error = scan_segment_names(names.iter().map(String::as_str)).unwrap_err();
            prop_assert_eq!(
                error,
                SegmentScanError::DuplicateSegment { segment: SegmentId::new(start).unwrap() }
            );
        }

        #[test]
        fn segment_gaps_are_rejected(start in 0..=999_950u32, len in 3usize..=32) {
            let end = (start as usize + len).min(MAX_SEGMENT_ID as usize + 1);
            prop_assume!(end as u32 > start + 1);
            let missing = start + 1;
            let names: Vec<String> = (start..end as u32)
                .filter(|raw| *raw != missing)
                .map(name)
                .collect();

            let error = scan_segment_names(names.iter().map(String::as_str)).unwrap_err();
            prop_assert_eq!(
                error,
                SegmentScanError::SegmentGap {
                    expected: SegmentId::new(missing).unwrap(),
                    found: SegmentId::new(missing + 1).unwrap(),
                }
            );
        }

        #[test]
        fn truncated_segment_names_are_rejected(digits in prop::collection::vec(0u8..=9, 0..SEGMENT_FILE_DIGITS)) {
            let mut segment_name = String::from(SEGMENT_FILE_PREFIX);
            for digit in digits {
                segment_name.push(char::from(b'0' + digit));
            }
            prop_assert!(segment_name.len() < SEGMENT_FILE_NAME_LEN);

            let error = scan_segment_names([segment_name.as_str()]).unwrap_err();
            prop_assert_eq!(
                error,
                SegmentScanError::TruncatedSegmentName { name: segment_name }
            );
        }

        #[test]
        fn frame_placements_never_cross_segment_boundaries(frames in prop::collection::vec(1u32..=32, 1..256)) {
            let config = small_config();
            let mut lifecycle = SegmentLifecycle::open(SegmentId::ZERO, 0, config).unwrap();
            let mut last_segment = SegmentId::ZERO;

            for frame_len in frames {
                if let Some(SegmentMaintenance::Preallocate { segment, len_bytes }) =
                    lifecycle.maintenance_request()?
                {
                    prop_assert_eq!(len_bytes, config.segment_size_bytes());
                    lifecycle.mark_preallocated(segment)?;
                }

                let placement = lifecycle.place_frame(frame_len)?;
                let placement_end = placement.offset_bytes() + placement.len_bytes();
                prop_assert!(placement_end <= config.segment_size_bytes());
                prop_assert!(placement.segment() >= last_segment);
                if placement.segment() > last_segment {
                    prop_assert_eq!(placement.offset_bytes(), 0);
                }
                last_segment = placement.segment();
            }
        }
    }

    #[test]
    fn edge_names_render_as_documented() {
        assert_eq!(SegmentId::ZERO.file_name(), "seg-000000.ilog");
        assert_eq!(SegmentId::new(17).unwrap().file_name(), "seg-000017.ilog");
        assert_eq!(SegmentId::MAX.file_name(), "seg-999999.ilog");
        assert_eq!(SegmentId::new(MAX_SEGMENT_ID + 1), None);
    }

    #[test]
    fn parse_rejects_malformed_names() {
        assert!(matches!(
            SegmentId::parse_file_name("seg-00001"),
            Err(SegmentScanError::TruncatedSegmentName { .. })
        ));
        assert!(matches!(
            SegmentId::parse_file_name("seg-000017.ilog.bak"),
            Err(SegmentScanError::InvalidSegmentName { .. })
        ));
        assert!(matches!(
            SegmentId::parse_file_name("seg-00001x.ilog"),
            Err(SegmentScanError::InvalidSegmentName { .. })
        ));
        assert!(matches!(
            SegmentId::parse_file_name("README"),
            Err(SegmentScanError::InvalidSegmentName { .. })
        ));
    }

    #[test]
    fn empty_segment_scan_is_valid_for_first_boot() {
        let catalog = scan_segment_names(core::iter::empty::<&str>()).unwrap();
        assert!(catalog.is_empty());
        assert_eq!(catalog.first(), None);
        assert_eq!(catalog.last(), None);
        assert_eq!(catalog.next_segment_id(), Some(SegmentId::ZERO));
    }

    #[test]
    fn contiguous_unsorted_scan_returns_numeric_order() {
        let names = [name(3), name(1), name(2)];
        let catalog = scan_segment_names(names.iter().map(String::as_str)).unwrap();

        assert_eq!(catalog.first(), Some(SegmentId::new(1).unwrap()));
        assert_eq!(catalog.last(), Some(SegmentId::new(3).unwrap()));
        assert_eq!(catalog.next_segment_id(), Some(SegmentId::new(4).unwrap()));
        assert_eq!(
            catalog.iter().collect::<Vec<_>>(),
            vec![
                SegmentId::new(1).unwrap(),
                SegmentId::new(2).unwrap(),
                SegmentId::new(3).unwrap()
            ]
        );
    }

    #[test]
    fn injected_directory_scan_reuses_scratch_and_returns_catalog() {
        let mut directory = TestDirectory { names: vec![name(2), name(0), name(1)], error: None };
        let mut scratch = vec!["stale".to_string()];

        let catalog = scan_segment_directory(&mut directory, &mut scratch).unwrap();

        assert_eq!(
            catalog.as_slice(),
            &[SegmentId::new(0).unwrap(), SegmentId::new(1).unwrap(), SegmentId::new(2).unwrap()]
        );
        assert_eq!(scratch, vec![name(2), name(0), name(1)]);
    }

    #[test]
    fn injected_directory_scan_error_is_preserved() {
        let mut directory = TestDirectory { names: vec![name(0), name(2)], error: None };
        let mut scratch = Vec::new();

        assert_eq!(
            scan_segment_directory(&mut directory, &mut scratch),
            Err(SegmentDirectoryError::Scan(SegmentScanError::SegmentGap {
                expected: SegmentId::new(1).unwrap(),
                found: SegmentId::new(2).unwrap()
            }))
        );
    }

    #[test]
    fn injected_directory_read_error_is_preserved() {
        let mut directory = TestDirectory { names: vec![name(0)], error: Some(TestReadError) };
        let mut scratch = vec!["stale".to_string()];

        assert_eq!(
            scan_segment_directory(&mut directory, &mut scratch),
            Err(SegmentDirectoryError::Read(TestReadError))
        );
        assert!(scratch.is_empty());
    }

    #[test]
    fn max_segment_catalog_has_no_next_id() {
        let segment = name(MAX_SEGMENT_ID);
        let catalog = scan_segment_names([segment.as_str()]).unwrap();

        assert_eq!(catalog.next_segment_id(), None);
    }

    #[test]
    fn max_segment_duplicate_is_rejected() {
        let segment = name(MAX_SEGMENT_ID);
        let error = scan_segment_names([segment.as_str(), segment.as_str()]).unwrap_err();

        assert_eq!(error, SegmentScanError::DuplicateSegment { segment: SegmentId::MAX });
    }

    #[test]
    fn segment_config_rejects_unsafe_thresholds() {
        assert_eq!(SegmentConfig::new(0, 1, 1), Err(SegmentLifecycleError::ZeroSegmentSize));
        assert_eq!(SegmentConfig::new(128, 0, 32), Err(SegmentLifecycleError::ZeroFrameSizeMax));
        assert_eq!(
            SegmentConfig::new(128, 128, 128),
            Err(SegmentLifecycleError::FrameSizeMaxExceedsSegment {
                frame_size_max_bytes: 128,
                segment_size_bytes: 128
            })
        );
        assert_eq!(
            SegmentConfig::new(128, 32, 31),
            Err(SegmentLifecycleError::PreallocateThresholdTooSmall {
                preallocate_threshold_bytes: 31,
                frame_size_max_bytes: 32
            })
        );
        assert_eq!(
            SegmentConfig::new(128, 32, 128),
            Err(SegmentLifecycleError::PreallocateThresholdTooLarge {
                preallocate_threshold_bytes: 128,
                segment_size_bytes: 128
            })
        );
    }

    #[test]
    fn lifecycle_requests_next_segment_inside_threshold() {
        let config = small_config();
        let lifecycle = SegmentLifecycle::open(SegmentId::new(7).unwrap(), 96, config).unwrap();

        assert_eq!(
            lifecycle.maintenance_request().unwrap(),
            Some(SegmentMaintenance::Preallocate {
                segment: SegmentId::new(8).unwrap(),
                len_bytes: config.segment_size_bytes()
            })
        );
    }

    #[test]
    fn lifecycle_rejects_wrong_preallocated_segment() {
        let config = small_config();
        let mut lifecycle = SegmentLifecycle::open(SegmentId::new(7).unwrap(), 96, config).unwrap();

        assert_eq!(
            lifecycle.mark_preallocated(SegmentId::new(9).unwrap()),
            Err(SegmentLifecycleError::UnexpectedPreparedSegment {
                expected: SegmentId::new(8).unwrap(),
                got: SegmentId::new(9).unwrap()
            })
        );
    }

    #[test]
    fn placement_requires_preallocated_next_segment() {
        let config = small_config();
        let mut lifecycle =
            SegmentLifecycle::open(SegmentId::new(7).unwrap(), 120, config).unwrap();

        assert_eq!(
            lifecycle.place_frame(16),
            Err(SegmentLifecycleError::NextSegmentNotReady {
                active: SegmentId::new(7).unwrap(),
                remaining_bytes: 8
            })
        );
    }

    #[test]
    fn placement_rotates_to_preallocated_segment() {
        let config = small_config();
        let mut lifecycle =
            SegmentLifecycle::open(SegmentId::new(7).unwrap(), 120, config).unwrap();
        lifecycle.mark_preallocated(SegmentId::new(8).unwrap()).unwrap();

        let placement = lifecycle.place_frame(16).unwrap();

        assert_eq!(placement.segment(), SegmentId::new(8).unwrap());
        assert_eq!(placement.offset_bytes(), 0);
        assert_eq!(placement.len_bytes(), 16);
        assert_eq!(
            placement.sealed(),
            Some(SegmentSeal { segment: SegmentId::new(7).unwrap(), used_bytes: 120 })
        );
        assert_eq!(lifecycle.active_segment(), SegmentId::new(8).unwrap());
        assert_eq!(lifecycle.active_offset_bytes(), 16);
    }

    #[test]
    fn placement_reports_exact_fill_seal() {
        let config = small_config();
        let mut lifecycle = SegmentLifecycle::open(SegmentId::new(7).unwrap(), 96, config).unwrap();

        let placement = lifecycle.place_frame(32).unwrap();

        assert_eq!(placement.segment(), SegmentId::new(7).unwrap());
        assert_eq!(placement.offset_bytes(), 96);
        assert_eq!(
            placement.sealed(),
            Some(SegmentSeal { segment: SegmentId::new(7).unwrap(), used_bytes: 128 })
        );
        assert_eq!(lifecycle.remaining_bytes(), 0);
    }

    #[test]
    fn max_segment_exhaustion_is_named() {
        let config = small_config();
        let lifecycle = SegmentLifecycle::open(SegmentId::MAX, 96, config).unwrap();

        assert_eq!(
            lifecycle.maintenance_request(),
            Err(SegmentLifecycleError::SegmentIdExhausted { active: SegmentId::MAX })
        );
    }
}
