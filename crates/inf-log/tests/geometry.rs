//! The log's geometry (ADR-0191): a staging configuration and a segment
//! configuration are one checked pair, and a segment holds the largest frame
//! the ring can seal as it lies on the device — `capacity_bytes` packed under
//! `Buffered`, `align_up(capacity_bytes, 4096)` under `Direct`.
//!
//! Oracle: every refusal is compared with literals derived from the format's
//! constants, never with `max_sealed_frame_bytes`; the bound itself is judged
//! against the writer's own byte count — a real ring seals a frame and the
//! sealed bytes are measured. Canary: `--cfg inf_canary_log_geometry_unchecked`
//! skips the segment row, and the first test reads the missing refusal
//! (`scripts/sim-canaries.sh`).

use std::path::PathBuf;

use inf_log::fs::mem::MemFs;
use inf_log::fs::{SegmentFs, SegmentIoMode};
use inf_log::{
    DEFAULT_MAX_FRAME_LEN, FRAME_ALIGN, FRAME_HEADER_LEN, FRAME_TRAILER_LEN, FrameLayout,
    LogGeometry, LogGeometryError, MIN_FRAME_LEN, MutationEffect, NsId, SegmentConfig,
    SegmentRotor, StagingConfig, StagingRing, max_sealed_frame_bytes,
};

const FRAME_OVERHEAD: u32 = (FRAME_HEADER_LEN + FRAME_TRAILER_LEN) as u32;
const MIB: u32 = 1 << 20;
const MODES: [SegmentIoMode; 2] = [SegmentIoMode::Buffered, SegmentIoMode::Direct];
/// The largest `u32` segment size that ends on a block: 4 GiB − 4 KiB.
const TOP_BLOCK: u32 = u32::MAX - (FRAME_ALIGN - 1);

fn staging(capacity_bytes: u32) -> StagingConfig {
    StagingConfig::with_capacity(capacity_bytes)
}

fn segment(segment_bytes: u32, io_mode: SegmentIoMode) -> SegmentConfig {
    SegmentConfig { segment_bytes, io_mode, ..SegmentConfig::DEFAULT }
}

fn layout(io_mode: SegmentIoMode) -> FrameLayout {
    match io_mode {
        SegmentIoMode::Buffered => FrameLayout::Packed,
        SegmentIoMode::Direct => FrameLayout::Aligned,
    }
}

fn judge(capacity_bytes: u32, segment_bytes: u32, io_mode: SegmentIoMode) -> Result<(), String> {
    LogGeometry::new(staging(capacity_bytes), segment(segment_bytes, io_mode))
        .map(|_| ())
        .map_err(|refused| format!("{refused:?}"))
}

fn below(segment_bytes: u32, frame: u32, capacity: u32, io_mode: SegmentIoMode) -> String {
    let refused = LogGeometryError::SegmentBelowFrame {
        segment_bytes,
        frame_bytes: frame,
        capacity_bytes: capacity,
        layout: layout(io_mode),
    };
    format!("{refused:?}")
}

fn unaligned(segment_bytes: u32) -> String {
    format!("{:?}", LogGeometryError::SegmentNotAligned { segment_bytes, align: FRAME_ALIGN })
}

/// The pair the simulator first met — 4 MiB of staging beside 64 KiB
/// segments — booted and served until one LOG step sealed a 68,140-byte
/// frame (40 + 68,096 + 4) the rotor refused. It is refused at construction
/// now, in both modes, with the same numbers (the capacity is a multiple of
/// 4096).
#[test]
fn segment_below_the_largest_frame_is_refused() {
    for io_mode in MODES {
        let refused = LogGeometry::new(StagingConfig::default(), segment(65_536, io_mode))
            .expect_err("expected SegmentBelowFrame: a 64 KiB segment beside 4 MiB of staging");
        assert_eq!(
            refused,
            LogGeometryError::SegmentBelowFrame {
                segment_bytes: 65_536,
                frame_bytes: 4 * MIB,
                capacity_bytes: 4 * MIB,
                layout: layout(io_mode),
            },
            "{io_mode:?}"
        );
    }
}

/// The floor — a segment of exactly the largest sealable frame — is legal;
/// one byte short is row 5 under `Buffered` and row 4 under `Direct` (the
/// value is unaligned); the aligned value below the `Direct` floor is 0.
#[test]
fn the_floor_holds_and_one_byte_short_is_refused() {
    for io_mode in MODES {
        assert_eq!(judge(4 * MIB, 4 * MIB, io_mode), Ok(()), "{io_mode:?}");
    }
    assert_eq!(
        judge(4 * MIB, 4 * MIB - 1, SegmentIoMode::Buffered),
        Err(below(4 * MIB - 1, 4 * MIB, 4 * MIB, SegmentIoMode::Buffered))
    );
    assert_eq!(judge(4 * MIB, 4 * MIB - 1, SegmentIoMode::Direct), Err(unaligned(4 * MIB - 1)));
    // At the smallest capacity the floors part: 48 packed, one block aligned.
    assert_eq!(judge(48, 48, SegmentIoMode::Buffered), Ok(()));
    assert_eq!(judge(48, 4096, SegmentIoMode::Direct), Ok(()));
    assert_eq!(
        judge(48, 47, SegmentIoMode::Buffered),
        Err(below(47, 48, 48, SegmentIoMode::Buffered))
    );
    assert_eq!(judge(48, 4095, SegmentIoMode::Direct), Err(unaligned(4095)));
    assert_eq!(judge(48, 0, SegmentIoMode::Direct), Err(below(0, 4096, 48, SegmentIoMode::Direct)));
}

/// One unaligned pair, two answers: a 256-byte segment holds 48 packed
/// bytes, and is no `Direct` segment at all.
#[test]
fn the_two_modes_part_at_one_unaligned_pair() {
    assert_eq!(judge(48, 256, SegmentIoMode::Buffered), Ok(()));
    assert_eq!(judge(48, 256, SegmentIoMode::Direct), Err(unaligned(256)));
}

/// The staging edges: 0 and 47 below one minimal frame, 48 and 64 MiB legal,
/// one past the decoder bound and the integer maximum above it — the
/// maximum meets row 2 before the aligned length could saturate.
#[test]
fn the_staging_edges_are_rows_one_and_two() {
    let min = |capacity_bytes| {
        format!("{:?}", LogGeometryError::StagingBelowMinimalFrame { capacity_bytes, min: 48 })
    };
    let max = |capacity_bytes| {
        let refused = LogGeometryError::StagingAboveDecoderBound {
            capacity_bytes,
            max: DEFAULT_MAX_FRAME_LEN,
        };
        format!("{refused:?}")
    };
    assert_eq!(MIN_FRAME_LEN, 48);
    assert_eq!(DEFAULT_MAX_FRAME_LEN, 64 * MIB);
    for io_mode in MODES {
        assert_eq!(judge(0, 64 * MIB, io_mode), Err(min(0)));
        assert_eq!(judge(47, 64 * MIB, io_mode), Err(min(47)));
        assert_eq!(judge(64 * MIB + 1, TOP_BLOCK, io_mode), Err(max(64 * MIB + 1)));
        assert_eq!(judge(u32::MAX, TOP_BLOCK, io_mode), Err(max(u32::MAX)));
        assert_eq!(judge(64 * MIB, 64 * MIB, io_mode), Ok(()));
    }
    assert_eq!(judge(48, 48, SegmentIoMode::Buffered), Ok(()));
    assert_eq!(judge(48, 4096, SegmentIoMode::Direct), Ok(()));
}

/// The depth edges: 0 and 9 and 255 are outside `1..=8`.
#[test]
fn the_depth_edges_are_row_three() {
    for (frames_in_flight, legal) in [(0, false), (1, true), (8, true), (9, false), (255, false)] {
        let cfg = StagingConfig { capacity_bytes: 4 * MIB, frames_in_flight };
        let judged = LogGeometry::new(cfg, SegmentConfig::DEFAULT);
        if legal {
            assert!(judged.is_ok(), "{frames_in_flight}: {judged:?}");
        } else {
            let refused = judged.expect_err("outside 1..=8");
            assert_eq!(
                refused,
                LogGeometryError::FramesInFlightOutOfRange { frames_in_flight, max: 8 }
            );
        }
    }
}

/// Alignment binds `Direct` only: at 48 bytes of staging every value holds
/// the packed frame, and only multiples of 4096 are `Direct` segments; at
/// 4 MiB of staging the aligned values below the floor meet row 5.
#[test]
fn alignment_is_row_four_under_direct_alone() {
    for segment_bytes in [4095, 4096, 8191, 8192] {
        assert_eq!(judge(48, segment_bytes, SegmentIoMode::Buffered), Ok(()));
        let direct = judge(48, segment_bytes, SegmentIoMode::Direct);
        if segment_bytes % 4096 == 0 {
            assert_eq!(direct, Ok(()));
        } else {
            assert_eq!(direct, Err(unaligned(segment_bytes)));
        }
        assert_eq!(
            judge(4 * MIB, segment_bytes, SegmentIoMode::Buffered),
            Err(below(segment_bytes, 4 * MIB, 4 * MIB, SegmentIoMode::Buffered))
        );
        let direct = judge(4 * MIB, segment_bytes, SegmentIoMode::Direct);
        if segment_bytes % 4096 == 0 {
            assert_eq!(direct, Err(below(segment_bytes, 4 * MIB, 4 * MIB, SegmentIoMode::Direct)));
        } else {
            assert_eq!(direct, Err(unaligned(segment_bytes)));
        }
    }
}

/// A zero segment holds no frame, in either mode, whatever the capacity.
#[test]
fn a_zero_segment_is_row_five() {
    for capacity in [48, 4096, 4 * MIB] {
        let aligned = capacity.next_multiple_of(4096);
        assert_eq!(
            judge(capacity, 0, SegmentIoMode::Buffered),
            Err(below(0, capacity, capacity, SegmentIoMode::Buffered))
        );
        assert_eq!(
            judge(capacity, 0, SegmentIoMode::Direct),
            Err(below(0, aligned, capacity, SegmentIoMode::Direct))
        );
    }
}

/// Stages `bytes` of records into `ring` in one step: deletes whose key
/// lengths sum the records to exactly `bytes` (each at least the 4-byte
/// minimal record).
fn stage_exactly(ring: &mut StagingRing, bytes: u32) {
    let key = vec![0x5A; 8 << 10];
    let encoded =
        |len: usize| MutationEffect::Delete { ns: NsId(0), key: &key[..len] }.encoded_len();
    let mut left = bytes as usize;
    while left > 0 {
        let piece = if left > (8 << 10) + 256 {
            8 << 10
        } else if left > 128 {
            left - 64
        } else {
            left
        };
        let mut len = piece.min(key.len());
        while encoded(len) > piece {
            len -= 1;
        }
        let effect = MutationEffect::Delete { ns: NsId(0), key: &key[..len] };
        ring.stage(&effect).expect("the ring has room for the frame's records");
        left -= effect.encoded_len();
    }
    assert_eq!(ring.pending_frame_len(), FRAME_OVERHEAD + bytes, "the records sum exactly");
}

/// I4: the bound is the writer's. For every capacity edge and each mode, a
/// real ring seals a frame of exactly its capacity (`C − 44` bytes of
/// records) and a frame of one minimal record; laid out under the mode's
/// layout, each frame's sealed bytes are at most `max_sealed_frame_bytes`,
/// and the full frame meets it exactly.
#[test]
fn a_sealed_frame_never_passes_the_largest_sealable_frame() {
    let edges = [48, 49, 4095, 4096, 4097, 64 << 10, 4 * MIB, 64 * MIB];
    for capacity in edges {
        for io_mode in MODES {
            let geometry = LogGeometry::with_staging(staging(capacity)).expect("a legal capacity");
            let bound = max_sealed_frame_bytes(geometry.staging(), io_mode);
            for records in [capacity - FRAME_OVERHEAD, 4] {
                let mut ring = StagingRing::new(geometry);
                stage_exactly(&mut ring, records);
                let lease =
                    ring.seal(inf_log::Lsn::new(inf_log::SegmentId(0), 40), 0, layout(io_mode));
                let sealed = ring.leased_frame(&lease).len() as u32;
                assert_eq!(sealed, layout(io_mode).padded_len(FRAME_OVERHEAD + records));
                assert!(sealed <= bound, "{capacity} {io_mode:?}: {sealed} > {bound}");
                if records == capacity - FRAME_OVERHEAD {
                    assert_eq!(
                        sealed, bound,
                        "{capacity} {io_mode:?}: the full frame is the bound"
                    );
                }
                ring.release(lease);
            }
        }
    }
}

/// The floor pair in each mode: one LOG step of `C − 44` bytes of records
/// seals a frame of exactly `C` bytes, and an empty segment of the floor
/// size reserves it without rotation — the fit assertion holds at equality.
#[test]
fn the_floor_pair_reserves_a_full_frame_without_rotation() {
    for (capacity, io_mode, segment_bytes) in [
        (4 * MIB, SegmentIoMode::Buffered, 4 * MIB),
        (4 * MIB, SegmentIoMode::Direct, 4 * MIB),
        (48, SegmentIoMode::Buffered, 48),
        (48, SegmentIoMode::Direct, 4096),
    ] {
        let geometry = LogGeometry::new(staging(capacity), segment(segment_bytes, io_mode))
            .expect("the floor pair is legal");
        let fs = MemFs::new();
        let dir = PathBuf::from("log");
        fs.create_dir_all(&dir).expect("dir");
        let mut rotor = SegmentRotor::create_fresh(fs, dir, geometry).expect("fresh");
        let mut ring = StagingRing::new(geometry);
        stage_exactly(&mut ring, capacity - FRAME_OVERHEAD);
        let lease = ring.flush_into(&mut rotor, 0).expect("flush").expect("one frame");
        assert_eq!(lease.frame_len(), segment_bytes, "{capacity} {io_mode:?}");
        assert_eq!(rotor.active_written(), segment_bytes, "the frame fills the segment");
        assert_eq!(rotor.stats().rotations, 0, "no rotation for a frame of the floor");
        ring.release(lease);
    }
}
