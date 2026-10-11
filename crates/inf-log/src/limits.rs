//! Bounds owned by the log crate. Every crossing names its behavior here.

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
