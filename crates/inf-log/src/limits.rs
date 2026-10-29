//! Bounds owned by the log crate. Every crossing names its behavior here.

use inf_foundation::bounded::{Cap, CapFill};

use crate::tier::{ROUND_OPS_MAX, TIER_BATCH_FRAMES, TIER_BATCH_PAYLOAD_BYTES, TIER_FRAME_BYTES};

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

/// The largest span one tier round stages, in record bytes (ADR-0155 D1,
/// D3): `max(S, M)` at its widest, the 64 MiB `MAINTAIN-SLICE` ceiling
/// above the format's largest record (16,777,483 B). Owner: the window
/// pool, sized by a pipeline's own `max(S, M)` at its build. Crossing: a
/// pipeline whose bound passes it is not built (the pool's layout
/// refusal), so no plan can select past it.
pub const TIER_SPAN_BYTES_MAX: u64 = 64 << 20;

/// Batch windows one pipeline's pool holds at most: `⌈TIER_SPAN_BYTES_MAX
/// / Q⌉`, `Q` a batch window's 1,047,552 record bytes (ADR-0155 D3's 65
/// at 64 MiB). Owner: `TierFlush`'s window pool, whose batch windows are a
/// boxed slice built whole with the pipeline, `⌈max(S, M)/Q⌉` entries: a
/// count the owner fixes at the build is fixed storage, never a capped
/// container the build would fill to its cap. Crossing: the build refuses
/// a larger count as a layout error, which `TierSpec::validate`'s slice and
/// record bounds rule out; none at run time — the slice's length, and a
/// plan reserves its round's windows before any writer effect with one
/// round staged per pipeline, so no take finds the pool short.
pub const TIER_POOL_BATCH_WINDOWS_MAX: u32 = 65;

/// Single-block windows one pipeline's pool holds: a round writes at most
/// five blocks outside its batches — the rotated file's tail and footer,
/// the new file's header, the span's tail and a trailing gap's footer
/// (ADR-0155 D3). Owner: the window pool, a boxed slice of exactly this
/// many built with the pipeline. Crossing: none — the slice's length, and
/// a plan reserves its windows as for the batch windows.
pub const TIER_POOL_SINGLE_WINDOWS: u32 = 5;

/// Write intents one tier round stages: `⌈A/Q⌉ + 5`, 70 at the widest span
/// (ADR-0155 D3). Owner: `TierRound`, one holder per pipeline built with
/// it. Crossing: none at run time — the round plan's layout counts the writes
/// before any writer effect and the span is at most the pool's bound.
pub const TIER_ROUND_WRITES_MAX: u32 = TIER_POOL_BATCH_WINDOWS_MAX + TIER_POOL_SINGLE_WINDOWS;

/// The round's writes under the census name `tier-round-writes`.
pub const TIER_ROUND_WRITES: Cap =
    Cap::entries::<TIER_ROUND_WRITES_MAX>("tier-round-writes", CapFill::Serving);

/// Barrier intents one tier round stages: the rotated file's seal, the new
/// file's two dirent barriers, and the span's sync or its trailing gap's
/// seal (ADR-0155 D3). Owner and crossing as the writes'.
pub const TIER_ROUND_BARRIERS_MAX: u32 = 4;

/// The round's barriers under the census name `tier-round-barriers`.
pub const TIER_ROUND_BARRIERS: Cap =
    Cap::entries::<TIER_ROUND_BARRIERS_MAX>("tier-round-barriers", CapFill::Serving);

/// Deferred effects one tier round stages: a rotation's `SealCommit`, then
/// a trailing gap's `SealCommit` and `GapCross` or the span's `DurableTo`
/// (ADR-0155 D3). Owner and crossing as the writes'.
pub const TIER_ROUND_EFFECTS_MAX: u32 = 3;

/// The round's effects under the census name `tier-round-effects`.
pub const TIER_ROUND_EFFECTS: Cap =
    Cap::entries::<TIER_ROUND_EFFECTS_MAX>("tier-round-effects", CapFill::Serving);

/// Seals one tier round stages and holds until its barrier completes: one
/// capacity rotation's and one trailing gap's (ADR-0155 D3, ADR-0084 D2).
/// Owner: `TierFlush`, one holder built with the pipeline. Crossing: none
/// at run time — a plan holds at most one rotation and one gap, so a round
/// with a third seal cannot be built.
pub const TIER_PENDING_SEALS_MAX: u32 = 2;

/// The staged seals under the census name `tier-pending-seals`.
pub const TIER_PENDING_SEALS: Cap =
    Cap::entries::<TIER_PENDING_SEALS_MAX>("tier-pending-seals", CapFill::Serving);

/// Directory handles one tier round holds for its dirent barriers: the
/// namespace directory's and its `cold/` directory's, when the round
/// creates a file (ADR-0084 D2's segment-create rule). Owner: `TierFlush`,
/// an array of exactly this many held as one value from the created
/// file's stage to the round's end. Crossing: none — the array's length,
/// and a plan creates at most one file.
pub const TIER_ROUND_DIR_HOLDS_MAX: u32 = 2;

// ADR-0155 D3's envelope at the widest span: the batch windows cover it,
// and the round's ops (70 writes and 4 barriers, 74) fit the 8-bit token op
// index.
const _: () = assert!(
    TIER_SPAN_BYTES_MAX.div_ceil(TIER_BATCH_PAYLOAD_BYTES) == TIER_POOL_BATCH_WINDOWS_MAX as u64,
    "the pool's batch windows cover the widest span"
);
const _: () = assert!(
    (TIER_ROUND_WRITES_MAX + TIER_ROUND_BARRIERS_MAX) as usize <= ROUND_OPS_MAX,
    "a round's ops fit the token op index"
);
