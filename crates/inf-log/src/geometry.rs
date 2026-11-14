//! The log's geometry (ADR-0191): the staging ring's configuration and the
//! segment's, checked as one pair before either is built.
//!
//! The ring seals frames of up to `capacity_bytes`; the rotor stores each in
//! one segment. A pair whose segment cannot hold the largest frame the ring
//! can seal would boot, serve, and stop the node at the first LOG step that
//! sealed such a frame. [`LogGeometry`] makes that pair unrepresentable: its
//! one constructor judges five rows and refuses with a typed
//! [`LogGeometryError`] naming the numbers, and [`StagingRing::new`] and the
//! [`SegmentRotor`] constructors take nothing else. Configuration validates
//! limits and their arithmetic before allocating or serving: nothing is
//! allocated before the check.
//!
//! [`StagingRing::new`]: crate::StagingRing::new
//! [`SegmentRotor`]: crate::SegmentRotor

use core::fmt;

use crate::frame::{DEFAULT_MAX_FRAME_LEN, FRAME_ALIGN, FrameLayout, MIN_FRAME_LEN};
use crate::fs::SegmentIoMode;
use crate::segment::SegmentConfig;
use crate::staging::{MAX_FRAMES_IN_FLIGHT, StagingConfig};

/// The largest frame a ring of `staging` can seal, in bytes as it lies on a
/// segment of `io_mode` (ADR-0191 D2): `capacity_bytes` packed under
/// `Buffered`, `align_up(capacity_bytes, FRAME_ALIGN)` under `Direct`.
///
/// The ring never seals more than `capacity_bytes` of header, records and
/// trailer: `StagingRing::stage` refuses the record that would pass it. A
/// `Buffered` rotor packs every frame — it creates, rotates onto and reopens
/// every segment `Buffered` — so the frame takes `frame_len ≤ C` bytes there.
/// A `Direct` rotor aligns its frames, or packs them on a reopened packed
/// tail whose successor aligns, so a frame takes at most `align_up(frame_len)`
/// bytes, and `align_up(frame_len) ≤ align_up(C)` because rounding up is
/// monotone. Nothing adds to either: a write-through frame is the same padded
/// frame (`fua_max_frame_bytes` picks the barrier, never the size), a held
/// frame keeps collecting under `C`, a segment carries no header, and the
/// recycle sentinel is one block of a file's previous life, overwritten like
/// any other.
///
/// Under `Direct` a segment is a multiple of [`FRAME_ALIGN`] (row 4 of
/// [`LogGeometry::new`]), and `align_up(C)` is the least such multiple at or
/// above `C`, so there `segment_bytes ≥ align_up(C)` holds exactly when
/// `segment_bytes ≥ C`: both modes ask a segment to hold the capacity. The
/// bound is still taken per layout, so it is exact: a `Buffered` segment
/// smaller than one block is legal when it holds `C`.
///
/// Exact for every `capacity_bytes ≤ DEFAULT_MAX_FRAME_LEN`. Above
/// `u32::MAX − 4095` the aligned length saturates at `u32::MAX`, which no
/// geometry reaches: [`LogGeometry::new`] judges row 2 before row 5.
#[must_use]
pub const fn max_sealed_frame_bytes(staging: StagingConfig, io_mode: SegmentIoMode) -> u32 {
    io_mode.layout().padded_len(staging.capacity_bytes)
}

/// The log's checked pair (ADR-0191 D1): a staging configuration and a
/// segment configuration that fit each other. Private fields and one
/// constructor, [`LogGeometry::new`], so a value of this type is a pair that
/// passed every row; the staging ring and the segment rotor take only this.
#[derive(Copy, Clone, Debug)]
pub struct LogGeometry {
    staging: StagingConfig,
    segment: SegmentConfig,
}

impl LogGeometry {
    /// The shipped pair: 4 MiB of staging, one frame in flight, 256 MiB
    /// `Buffered` segments. [`LogGeometry::new`] evaluated at compile time,
    /// so the default is proven at build through the one constructor.
    pub const DEFAULT: LogGeometry =
        match LogGeometry::new(StagingConfig::DEFAULT, SegmentConfig::DEFAULT) {
            Ok(geometry) => geometry,
            Err(_) => panic!("the shipped log geometry is legal"),
        };

    /// Checks the pair, row by row, and refuses the first row it fails:
    ///
    /// 1. `capacity_bytes ≥ MIN_FRAME_LEN`: a buffer holds one minimal frame.
    /// 2. `capacity_bytes ≤ DEFAULT_MAX_FRAME_LEN`: every frame the ring
    ///    seals stays readable by a default reader (writer ≤ reader, one
    ///    `const`).
    /// 3. `frames_in_flight ∈ 1..=MAX_FRAMES_IN_FLIGHT`: the ring is bounded,
    ///    never a queue.
    /// 4. under `Direct`, `segment_bytes` is a multiple of `FRAME_ALIGN`: an
    ///    aligned frame needs an aligned segment end, or the last frame's
    ///    padding would run past the preallocation (ADR-0086 D3).
    /// 5. `segment_bytes ≥ max_sealed_frame_bytes(staging, io_mode)`: a
    ///    segment holds the largest frame the ring can seal. It subsumes a
    ///    zero segment in both modes and, with row 4, any `Direct` segment
    ///    under one block.
    ///
    /// # Errors
    /// The [`LogGeometryError`] of the first row the pair fails, carrying
    /// the numbers it compared.
    pub const fn new(
        staging: StagingConfig,
        segment: SegmentConfig,
    ) -> Result<LogGeometry, LogGeometryError> {
        let capacity_bytes = staging.capacity_bytes;
        if capacity_bytes < MIN_FRAME_LEN {
            return Err(LogGeometryError::StagingBelowMinimalFrame {
                capacity_bytes,
                min: MIN_FRAME_LEN,
            });
        }
        if capacity_bytes > DEFAULT_MAX_FRAME_LEN {
            return Err(LogGeometryError::StagingAboveDecoderBound {
                capacity_bytes,
                max: DEFAULT_MAX_FRAME_LEN,
            });
        }
        let frames_in_flight = staging.frames_in_flight;
        if frames_in_flight < 1 || frames_in_flight > MAX_FRAMES_IN_FLIGHT {
            return Err(LogGeometryError::FramesInFlightOutOfRange {
                frames_in_flight,
                max: MAX_FRAMES_IN_FLIGHT,
            });
        }
        let segment_bytes = segment.segment_bytes;
        let direct = matches!(segment.io_mode, SegmentIoMode::Direct);
        if direct && !segment_bytes.is_multiple_of(FRAME_ALIGN) {
            return Err(LogGeometryError::SegmentNotAligned { segment_bytes, align: FRAME_ALIGN });
        }
        let frame_bytes = max_sealed_frame_bytes(staging, segment.io_mode);
        // The canary build skips this row alone (ADR-0090 D5); the shipped
        // pair passes it either way, so `DEFAULT` still evaluates.
        let checked = !cfg!(inf_canary_log_geometry_unchecked);
        if checked && segment_bytes < frame_bytes {
            return Err(LogGeometryError::SegmentBelowFrame {
                segment_bytes,
                frame_bytes,
                capacity_bytes,
                layout: segment.io_mode.layout(),
            });
        }
        Ok(LogGeometry { staging, segment })
    }

    /// `new` with the default segment, for a ring that has no rotor (a
    /// harness that stages and seals without a log).
    ///
    /// # Errors
    /// As [`LogGeometry::new`].
    pub const fn with_staging(staging: StagingConfig) -> Result<LogGeometry, LogGeometryError> {
        LogGeometry::new(staging, SegmentConfig::DEFAULT)
    }

    /// The staging ring's half of the pair.
    #[must_use]
    pub const fn staging(self) -> StagingConfig {
        self.staging
    }

    /// The segment rotor's half of the pair.
    #[must_use]
    pub const fn segment(self) -> SegmentConfig {
        self.segment
    }
}

impl Default for LogGeometry {
    fn default() -> Self {
        LogGeometry::DEFAULT
    }
}

/// The row a pair failed, with the numbers it compared (ADR-0191 D1). One
/// variant per row of [`LogGeometry::new`], in its order.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum LogGeometryError {
    /// Row 1: a staging buffer smaller than one minimal frame.
    StagingBelowMinimalFrame { capacity_bytes: u32, min: u32 },
    /// Row 2: a staging buffer larger than the frame decoder's bound.
    StagingAboveDecoderBound { capacity_bytes: u32, max: u32 },
    /// Row 3: a pipeline depth outside `1..=MAX_FRAMES_IN_FLIGHT`.
    FramesInFlightOutOfRange { frames_in_flight: u8, max: u8 },
    /// Row 4: a `Direct` segment that does not end on a block.
    SegmentNotAligned { segment_bytes: u32, align: u32 },
    /// Row 5: a segment smaller than the largest frame the ring can seal,
    /// `frame_bytes`, taken from `capacity_bytes` under `layout`.
    SegmentBelowFrame {
        segment_bytes: u32,
        frame_bytes: u32,
        capacity_bytes: u32,
        layout: FrameLayout,
    },
}

impl fmt::Display for LogGeometryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            LogGeometryError::StagingBelowMinimalFrame { capacity_bytes, min } => write!(
                f,
                "staging capacity {capacity_bytes} bytes is below one minimal frame, {min} \
                 bytes: a staging buffer holds at least one record"
            ),
            LogGeometryError::StagingAboveDecoderBound { capacity_bytes, max } => write!(
                f,
                "staging capacity {capacity_bytes} bytes is above the frame decoder bound, \
                 {max} bytes: every frame the ring seals stays readable"
            ),
            LogGeometryError::FramesInFlightOutOfRange { frames_in_flight, max } => write!(
                f,
                "frames in flight {frames_in_flight} is outside 1..={max}: the ring is \
                 bounded, never a queue"
            ),
            LogGeometryError::SegmentNotAligned { segment_bytes, align } => write!(
                f,
                "segment_bytes {segment_bytes} is not a multiple of {align}: an aligned frame \
                 needs an aligned segment end"
            ),
            LogGeometryError::SegmentBelowFrame {
                segment_bytes,
                frame_bytes,
                capacity_bytes,
                layout,
            } => {
                write!(
                    f,
                    "segment_bytes {segment_bytes} is below the largest frame the staging ring \
                     can seal, {frame_bytes} bytes (a capacity of {capacity_bytes} bytes of \
                     records and framing, "
                )?;
                match layout {
                    FrameLayout::Packed => f.write_str("packed")?,
                    FrameLayout::Aligned => write!(f, "padded to {FRAME_ALIGN}-byte blocks")?,
                }
                f.write_str("): a segment holds at least one frame")
            }
        }
    }
}

impl std::error::Error for LogGeometryError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_is_the_shipped_pair() {
        let geometry = LogGeometry::default();
        assert_eq!(geometry.staging().capacity_bytes, crate::DEFAULT_STAGING_BYTES);
        assert_eq!(geometry.staging().frames_in_flight, 1);
        assert_eq!(geometry.segment().segment_bytes, crate::DEFAULT_SEGMENT_BYTES);
        assert_eq!(geometry.segment().io_mode, SegmentIoMode::Buffered);
        let staging = LogGeometry::with_staging(StagingConfig::with_capacity(1 << 20))
            .expect("a 1 MiB ring beside the default segment");
        assert_eq!(staging.segment().segment_bytes, crate::DEFAULT_SEGMENT_BYTES);
    }

    #[test]
    fn each_refusal_states_its_rule_and_numbers() {
        let below = LogGeometryError::SegmentBelowFrame {
            segment_bytes: 65_536,
            frame_bytes: 4_194_304,
            capacity_bytes: 4_194_304,
            layout: FrameLayout::Packed,
        };
        assert_eq!(
            below.to_string(),
            "segment_bytes 65536 is below the largest frame the staging ring can seal, 4194304 \
             bytes (a capacity of 4194304 bytes of records and framing, packed): a segment \
             holds at least one frame"
        );
        let aligned = LogGeometryError::SegmentBelowFrame {
            segment_bytes: 0,
            frame_bytes: 4096,
            capacity_bytes: 48,
            layout: FrameLayout::Aligned,
        };
        assert!(aligned.to_string().contains("48 bytes of records and framing, padded to 4096"));
        let unaligned = LogGeometryError::SegmentNotAligned { segment_bytes: 4095, align: 4096 };
        assert!(unaligned.to_string().starts_with("segment_bytes 4095 is not a multiple of 4096"));
    }
}
