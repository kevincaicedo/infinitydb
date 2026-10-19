//! Bounds owned by the log crate. Every crossing names its behavior here.

use inf_foundation::bounded::{Cap, CapFill};

use crate::tier::{TIER_BATCH_FRAMES, TIER_FRAME_BYTES};

/// Device bytes one tier-flush offer carries (ADR-0178 D3's tier row,
/// ADR-0155 D4): a group of a round's unsent writes, whole writes only,
/// is at most this, so no offer is above the tier class's cap on the
/// byte axis — the tier class's declared byte slice, which the durable
/// cell hands the device budget, is this same constant (ADR-0178 T1).
/// Owner: the group builder in the plane's tier cell, and the tier
/// class's declared slice. Crossing: the group ends before the write
/// that would pass it; a single write never does (the assertion below).
pub const TIER_GROUP_BYTES_MAX: u64 = 1 << 20;

/// Ops one tier-flush offer carries (ADR-0178 D3's tier row, ADR-0155
/// D4): a group's write descriptors, or a barrier group's fdatasyncs.
/// Owner: the group builder. Crossing: the group ends before the op that
/// would pass it.
pub const TIER_GROUP_OPS_MAX: usize = 64;

// A full batch window is the largest write a round stages (one aligned
// device write of `TIER_BATCH_FRAMES` frames), so the group builder can
// always fit the next unsent write alone: a group is never empty.
const _: () = assert!(
    (TIER_BATCH_FRAMES * TIER_FRAME_BYTES) as u64 <= TIER_GROUP_BYTES_MAX,
    "one staged write fits one tier group"
);

/// Extent barriers one cell's fsync ledger holds unfolded at once: the
/// extent-seal window (ADR-0163 D2, ADR-0061 D3). A blob `SET` reserves a
/// unit before its extent file exists and gives it back when the extent's
/// barrier folds into the ledger's durable prefix, so at most this many
/// extent files are held open awaiting their fdatasync and MAINTAIN issues
/// at most this many in a tick: one submit's batch at the batching floor
/// (`sqes_per_submit` ≥ 16), the width of the ledger's write-through
/// window. Owner: the cell's `GroupCommit`, one window sized at its
/// construction. Crossing: pacing — a writer that finds no unit parks
/// before any effect, in arrival order, and is woken by the LOG step once
/// the frame held for the barriers has sealed; it sees latency, never a
/// reply of its own. Its ceiling is 16 blob writes a turnover (one extent
/// fdatasync and the iterations around it).
pub const EXTENT_SEAL_WINDOW_ENTRIES: u32 = 16;

/// The extent-seal window's cap: `EXTENT_SEAL_WINDOW_ENTRIES` entries under
/// the census name `extent-window`, filled while the cell serves.
pub const EXTENT_SEAL_WINDOW: Cap =
    Cap::entries::<EXTENT_SEAL_WINDOW_ENTRIES>("extent-window", CapFill::Serving);
