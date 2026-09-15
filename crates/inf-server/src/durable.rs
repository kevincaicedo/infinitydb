//! Per-cell durable-namespace machinery (M2-S08): the staging ring, the
//! segment rotor, the group-commit ledger, and the **seq-keyed ack gate**,
//! driven from the plane's EXECUTE/LOG/REAP/timer steps exactly per the
//! ADR-0013 D2 choreography (the `inf-log/tests/support` `DurablePlane` is
//! the reference implementation this productionizes).
//!
//! ## Why the ack gate is keyed by sequence number, not LSN
//!
//! An `always` response future must register its wait *at dispatch*, but a
//! record's LSN exists only after LOG seals the frame. Every staged record
//! therefore gets a monotone **durable seq** synchronously; frames are
//! FIFO, so seq order equals LSN order, and when the fsync watermark covers
//! a frame's exclusive-end LSN it covers every seq staged into it — the
//! gate advances to that frame's last seq. The S06 oracle ("no ack before
//! the watermark covers its LSN") holds structurally: `ack_gate` only
//! advances from `GroupCommit::on_fsync_complete`, which only fires on
//! `Synced` completions (ADR-0013 D3).

use std::collections::VecDeque;
use std::path::PathBuf;

use inf_alloc::AlignedBox;
use inf_foundation::time::Nanos;
use inf_log::fs::SegmentFs;
use inf_log::{
    FRAME_ALIGN, FillSource, FrameBuilder, FrameId, FramePlan, FsyncClass, FsyncTicket,
    GroupCommit, IdxSidecarMeta, Lsn, MutationEffect, NsId, RecordView, SealedDisposal,
    SegmentConfig, SegmentRotor, StagingConfig, StagingRing, ZERO_FILL_SLICE_BYTES,
    build_recycle_sentinel,
};
use inf_runtime::{
    Admission, ClassCounters, ClassSlice, CompletionToken, DeviceBudget, DeviceModel, IoClass,
    IoOp, LoopCx, SealPace, TokenClass, WaitList, WatermarkGate, WriteBarrier,
};
use inf_store::{CheckpointImage, IndexId, Keyspace, WallAnchor};

use crate::ckpt::{
    CkptCell, CkptPhase, CkptStats, MAX_TRUNC_PER_SLICE_ADAPTIVE, MAX_UNLINKS_PER_SLICE,
    ManifestCell, ManifestStats, SCAN_CHUNK_ENTRIES, ckpt_token,
};
use crate::io_properties::IoProvenance;
use crate::log_bytes;

mod ckpt;

// ---- shared data definitions (behaviour lives in the child modules) ----------

/// Timer-wheel key for the everysec tick (plane-armed, injected clock).
pub(crate) const EVERYSEC_TIMER_KEY: u64 = 0xE5EC_0001;
/// Timer-wheel key for the frame-fill window (M4.5-S39a): armed once per
/// hold episode at the window's deadline so a parked loop wakes to seal
/// the held frame; the handler is a no-op — the LOG step of that
/// iteration does the sealing.
pub(crate) const FILL_TIMER_KEY: u64 = 0xF111_0001;
/// Timer-wheel key for the FLUSH-class group hold (M4.5-S43, ADR-0092):
/// armed once per hold episode at the window's deadline, the same
/// no-op-handler shape as the fill timer — the wake is the effect.
pub(crate) const GROUP_TIMER_KEY: u64 = 0x6A0B_0001;

/// POSIX `EIO` (this crate carries no libc dep): the errno the
/// `durable_fsync_eio` fault point injects (M2-S17).
const EIO: i32 = 5;
/// POSIX `EINVAL`: the kernel's refusal of a direct write the filesystem
/// cannot take (ADR-0088 D3 as amended) — the checkpoint's in-band
/// downgrade signal, never a fault.
const EINVAL: i32 = 22;

/// Pinned retryable reply for staging backpressure the caller cannot
/// park on. After ADR-0083 D1 (M4.5-S27) the only emitter left is the
/// doc path's exact late admission — every parkable path paces instead.
#[cfg(feature = "doc")]
pub(crate) const STAGING_BUSY_ERROR: &str = "BUSY durable log staging is full, retry";

/// Typed non-retryable refusal for a write whose staged record can never
/// fit the staging domain (M4.5-S27, ADR-0083 D2) — the up-front bound
/// check `staging.rs` demands of admission: no drain can ever admit it,
/// so parking or client retry is a livelock, never backpressure.
pub(crate) const STAGING_OVERSIZED_ERROR: &str = "ERR write exceeds durable log staging capacity";

/// Durable-path configuration one cell receives from the node assembly.
/// Absent config means a memory-only cell: durable DDL is refused with a
/// documented error and none of this module's code runs (M2-S09's zero-cost
/// branch is `Option::is_none`).
#[derive(Clone, Debug)]
pub struct DurableConfig {
    /// Node data directory; cell `k` owns `<data_dir>/shard-k/`.
    pub data_dir: PathBuf,
    pub staging: StagingConfig,
    pub segment: SegmentConfig,
    /// Fuzzy-checkpoint policy (M2-S10, ADR-0016).
    pub ckpt: inf_log::CkptConfig,
    /// Boot-recovery stepping (M2-S15).
    pub recover: RecoverConfig,
    /// FLUSH-class durability fsyncs allowed in flight per cell (ADR-0022
    /// D3): 1 = the shipped discipline; 2 = the M2.5-S07 measured arm
    /// (no flag since ADR-0087 D5 — tests and `inf-bench` arms construct
    /// it). Never more. Write-through frames (ADR-0086 D5) are bounded by
    /// `StagingConfig::frames_in_flight` instead.
    pub flush_bound: u8,
    /// The device's probed write-through p50 at 4 KiB (µs) from
    /// `io-properties.toml` (ADR-0086 D7) — the `barrier_class_degraded`
    /// tripwire's reference. 0 = unknown (tripwire disarmed; the FLUSH
    /// class needs none).
    pub fua_p50_us_probed: u64,
    /// M4.5-S36 (ADR-0088 D2/D2b/D6): the cell's static share of the
    /// probed device model and the frame-seal pace. `Default` = absent
    /// model = unbudgeted, unpaced — the pre-S36 behaviour byte-for-byte.
    pub device: DeviceConfig,
    /// M4.5-S39a (ADR-0089): the frame-fill policy on aligned segments.
    /// `Default` = off — one seal per LOG step, the pre-S39a cadence
    /// byte-for-byte; the product default is [`FillConfig::DESIGN_POINT`]
    /// (`infinityd --fill-window-us 1000`), accepted on the reference box
    /// 2026-08-22.
    pub fill: FillConfig,
    /// M4.5-S43 (ADR-0092): the FLUSH-class group hold — a measured arm,
    /// `Default` = off (`infinityd --flush-group-window-us 0`); a due
    /// frame on a packed segment waits, bounded, for the round it just
    /// acked. Never engages on the FUA class (write-through plans).
    pub group: GroupHoldConfig,
}

/// The frame-fill policy (M4.5-S39a). On an aligned (`Direct`, v3)
/// segment every frame pads to the next 4 KiB, and a LOG step that seals
/// whatever arrived emits ~2.4 KiB payloads into 4 KiB blocks at small
/// groups — 32–42 % of log bytes as padding (ADR-0088 amendment,
/// 2026-08-21). A **barrier-less** pending frame (plan `Plain`: no sync
/// due, no ack waiting on it) is held until it reaches `target_bytes`
/// on-device or `window` elapses since the LOG step first saw it; the
/// window is timer-armed so an idle park never outlives it. A frame
/// whose plan carries a barrier is **never** held — an `always` ack
/// waits on it, the E4.7 rejection of a batch window on the barrier
/// path stands, and the measured arm that held such frames behind
/// in-flight ones (ADR-0089 D2, "arm B") read p50 ÷ barrier 1.84–1.89
/// against the 1.3 gate and was `Rejected` with its flag. Packed (v2)
/// segments have no padding and keep the per-step cadence.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct FillConfig {
    /// Hold bound since the first LOG step that saw the pending frame.
    /// `Nanos(0)` = policy off.
    pub window: Nanos,
    /// The on-device frame length (padded) at which a held frame seals.
    pub target_bytes: u32,
}

impl FillConfig {
    /// The accepted default (ADR-0089 D6, reference box 2026-08-22): a
    /// 1 ms window and a 16 KiB target — padding 24–30 % → 12.5 % and
    /// +17–38 % `everysec` throughput on the S36 row, the `always` row
    /// untouched.
    pub const DESIGN_POINT: FillConfig =
        FillConfig { window: Nanos::from_micros(1_000), target_bytes: 16 << 10 };

    /// The policy is engaged.
    #[must_use]
    pub const fn enabled(&self) -> bool {
        self.window.0 > 0
    }

    /// The pure decision (unit-pinned; the LOG step adds the timer):
    /// hold the pending frame when the policy is on, the frame lands on
    /// an aligned segment, its plan is `Plain` (no barrier to carry), it
    /// is below `target_bytes` on-device, and — once the episode's clock
    /// `since` is running — the window has not elapsed. `since == None`
    /// is the episode's first sight: the hold starts now.
    #[must_use]
    pub fn decide(
        &self,
        plan: FramePlan,
        layout: inf_log::FrameLayout,
        frame_len: u32,
        since: Option<Nanos>,
        now: Nanos,
    ) -> FillDecision {
        if !self.enabled() || layout != inf_log::FrameLayout::Aligned {
            return FillDecision::Seal;
        }
        if plan != FramePlan::Plain || layout.padded_len(frame_len) >= self.target_bytes {
            return FillDecision::Seal;
        }
        match since {
            Some(since) if now >= since + self.window => FillDecision::Seal,
            _ => FillDecision::Hold,
        }
    }
}

/// What the fill policy says about the pending frame this LOG step.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FillDecision {
    Seal,
    Hold,
}

/// The FLUSH-class group hold (M4.5-S43, ADR-0092 D1). At K = 1 with
/// closed-loop clients the connections split into the set whose records
/// are in the in-flight frame and the set whose records arrived during
/// its flight; the LOG step seals the second set the instant the first
/// completes — before the first set's acks have reached the clients —
/// so every record waits two barrier windows and a cell frame carries
/// half its population (v6 tri-bench, FLUSH default, 4 cells, 32 conns:
/// client p50 6.17 ms ÷ in-band fdatasync p50 3.26 ms = 1.89; ≈ 3.7
/// records per frame of ≈ 8 outstanding). A due `LinkedFsync` frame on
/// a **packed** segment with a drained pipeline — or the standalone
/// fdatasync that would cover already-written plain frames — therefore
/// holds while the round is still re-arriving: `uncovered_records <
/// round_target`, where *uncovered* is every record assigned but not
/// yet covered by a completed barrier (`last_seq − acked_seq`: the plain
/// frames written while the FLUSH slot was busy plus the staging
/// buffer) and the target is the **population** the last barrier's
/// completion revealed — the records it acked plus the records assigned
/// during its flight (campaigns A and B, 2026-08-25, ADR-0092 D1
/// amended twice: the last frame's size never holds in the steady
/// state, and `LogWritten` sees 1–2-record plain frames because the
/// FLUSH slot's busy window seals arrivals at once and covers them with
/// the *next* barrier; a trickle of one never holds) — bounded by
/// `window`, timer-armed. Write-through plans are never held: ADR-0089's arm B
/// measured that shape on the FUA class (p50 ÷ barrier 1.84–1.89) and
/// the FUA class stays byte-identical. Off by default until the
/// reference-box A/B (ADR-0092 D4).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct GroupHoldConfig {
    /// Hold bound since the first LOG step that saw the pending frame.
    /// `Nanos(0)` = policy off.
    pub window: Nanos,
}

impl GroupHoldConfig {
    /// The measured arm (ADR-0092 D4): 250 µs — ≤ 8 % of the four-cell
    /// FLUSH window the v6 rows read, ≥ 2 × the client round trip.
    pub const ARM: GroupHoldConfig = GroupHoldConfig { window: Nanos::from_micros(250) };
    /// A round smaller than this is a trickle, never a round to wait for.
    pub const MIN_GROUP: u64 = 2;

    /// The policy is engaged.
    #[must_use]
    pub const fn enabled(&self) -> bool {
        self.window.0 > 0
    }

    /// The pure decision (unit-pinned; the LOG step adds the timer):
    /// hold iff the policy is on, the barrier is FLUSH-class (`plan ==
    /// LinkedFsync` — the standalone fdatasync passes the same plan), the
    /// layout is packed, the round is a group (`round_target ≥
    /// MIN_GROUP`), the round has not re-arrived (`uncovered_records <
    /// round_target`), and the window has not elapsed since `since`
    /// (`None` = the episode's first sight). `round_target` is the
    /// population the last barrier's completion measured (ADR-0092 D1
    /// as amended by campaigns A and B).
    #[must_use]
    pub fn decide(
        &self,
        plan: FramePlan,
        layout: inf_log::FrameLayout,
        uncovered_records: u64,
        round_target: u64,
        since: Option<Nanos>,
        now: Nanos,
    ) -> GroupDecision {
        if !self.enabled() || layout != inf_log::FrameLayout::Packed {
            return GroupDecision::Seal;
        }
        if plan != FramePlan::LinkedFsync || round_target < Self::MIN_GROUP {
            return GroupDecision::Seal;
        }
        if uncovered_records >= round_target {
            return GroupDecision::Seal;
        }
        match since {
            Some(since) if now >= since + self.window => GroupDecision::Seal,
            _ => GroupDecision::Hold,
        }
    }
}

/// What the group hold says about the pending frame this LOG step.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum GroupDecision {
    Seal,
    Hold,
}

/// The per-cell device budget inputs (ADR-0088 D6), computed once at
/// boot from the device model and the cell count (L1: static shares).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct DeviceConfig {
    /// The cell's share of the device model (`DeviceModel::share`).
    pub model_share: DeviceModel,
    /// The cell's share of the device's concurrent barrier rate
    /// (`write_ops_per_s_4k_qd4 / cells`) — the seal pacer's refill
    /// (ADR-0088 D2b). 0 = unpaced.
    pub seal_barriers_per_s: u64,
    /// M4.5-S42 (ADR-0091 D5): where the model came from — surfaced in
    /// `INFO persistence` so a row on an unprobed node is never mistaken
    /// for the product. `Default` = absent (the dev tier).
    pub provenance: IoProvenance,
}

/// Boot-recovery stepping policy (M2-S15). The default replays flat-out
/// in large MAINTAIN steps; the throttle exists so tests can hold a node
/// in its `-LOADING` window long enough to observe it (never a
/// production tuning knob — recovery throughput is a gate, not a dial).
#[derive(Copy, Clone, Debug)]
pub struct RecoverConfig {
    /// Max checkpoint/replay bytes one MAINTAIN recovery step consumes
    /// before yielding to the loop — bounds `-LOADING` reply latency
    /// during boot (one frame/section may overshoot).
    pub step_bytes: u64,
    /// Test-only pacing: cap recovery at roughly this rate against the
    /// injected loop clock (`None` = flat out).
    pub throttle_bytes_per_sec: Option<u64>,
    /// Boot-read prefetch (M2.5-S08; ADR-0109): recovery's segment,
    /// checkpoint and audit readers ride a per-file prefetch thread that
    /// pulls the next advised window into the page cache, so cold
    /// replay's device read overlaps apply. **Boot-scoped by type**: the
    /// wrapper lives inside [`Recovery`](crate::Recovery) and never
    /// reaches the serving plane (L17-02 of the 2026-08-30 review — the
    /// wrapper used to be the plane's filesystem for the node's life).
    /// Off by default: the DST and every in-memory tier never spawn a
    /// thread; `infinityd` turns it on for a single recovering cell (the
    /// S08 A/B's regime split — N parallel recovering cells already
    /// saturate the device).
    pub boot_prefetch: bool,
}

impl Default for RecoverConfig {
    fn default() -> RecoverConfig {
        // 8 MiB ≈ single-digit-ms steps at the ≥ 1 GB/s replay gate: the
        // loop keeps answering -LOADING while paying < 0.1% step overhead.
        RecoverConfig { step_bytes: 8 << 20, throttle_bytes_per_sec: None, boot_prefetch: false }
    }
}

/// Cumulative durable counters flushed into `NodeInfo` by MAINTAIN (the
/// S21 vocabulary, born cell-local — no atomics).
#[derive(Copy, Clone, Debug, Default)]
pub struct DurableStats {
    pub records_appended: u64,
    pub acks_gated: u64,
    pub pending_log_bytes: u64,
    pub last_durable_lsn: u64,
    pub watermark_lag_lsn: u64,
    pub fsyncs_completed: u64,
    /// M2-S22: cumulative frames queued (one per LOG writev — the
    /// `log_writes_per_iter` tripwire numerator) and the staging
    /// domain's resident bytes (the L5 attribution observable).
    pub frames_queued: u64,
    pub staging_resident_bytes: u64,
    /// MANIFEST swaps + truncation (M2-S11, ADR-0017).
    pub manifests_published: u64,
    pub manifests_aborted: u64,
    /// M2-S21: last-window rates + fsync latency percentiles (µs).
    pub fsyncs_per_sec: u64,
    pub acks_per_sec: u64,
    pub fsync_p50_us: u64,
    pub fsync_p99_us: u64,
    pub fsync_p999_us: u64,
    /// M2.5-S07 group formation: records newly covered per
    /// durability-fsync completion (distribution percentiles).
    pub fsync_group_p50: u64,
    pub fsync_group_p99: u64,
    /// M4.5-S27 (ADR-0083 D5): the staging drain's binding variable —
    /// frame-write submit → `LogWritten` latency (µs percentiles). Under
    /// kernel writeback throttling this is what starves the staging
    /// domain; fsync latency is the correlated symptom.
    pub write_stall_p50_us: u64,
    pub write_stall_p99_us: u64,
    pub write_stall_p999_us: u64,
    /// M4.5-S27: the configured per-buffer staging capacity (the
    /// admission bound; `staging_resident_bytes` = 2 × this).
    pub staging_capacity_bytes: u64,
    /// M4.5-S27: commands currently parked on the drain waitlist, and
    /// cumulative park episodes — pacing made visible (ADR-0083 D5).
    pub admission_parked: u64,
    pub admission_parked_total: u64,
    /// M4.5-S27: per-reason durability-fsync counts (the S29 named
    /// observability gap — `CommitStats` had them, nothing exported them).
    pub fsyncs_linked: u64,
    pub fsyncs_seal: u64,
    pub fsyncs_standalone: u64,
    pub fsyncs_completion: u64,
    pub segments_truncated: u64,
    /// 1 while a checkpoint is streaming (`rdb_bgsave_in_progress`).
    pub ckpt_in_progress: u64,
    /// On-disk segments the rotor tracks (sealed + active + prealloc'd
    /// next) — the reclamation-bound observable.
    pub log_segments_live: u64,
    /// M4.5-S34 (ADR-0086): 1 while the active segment writes
    /// write-through (FUA-class) frames for due syncs, 0 under FLUSH.
    pub barrier_class_fua: u64,
    /// The configured class (M4.5-S42 follow-up, `io_class_configured`):
    /// 1 = the rotor creates `Direct` segments — what the probe/flag
    /// decided, independent of the active segment's upgrade state.
    pub io_class_configured_fua: u64,
    /// Write-through frame tickets completed (`fsyncs_fua`).
    pub fsyncs_fua: u64,
    /// Write-through barrier latency percentiles (µs) — submission →
    /// `LogWritten`, the barrier an `always` client waits on.
    pub fua_p50_us: u64,
    pub fua_p99_us: u64,
    /// v3 alignment padding sealed so far (`log_padding_bytes`) and zero
    /// bytes written to pre-zero direct segments (`zero_fill_bytes`) —
    /// the two write-amplification disclosures of the direct class.
    pub log_padding_bytes: u64,
    pub zero_fill_bytes: u64,
    /// Rotations onto a not-yet-zeroed direct segment (FLUSH-class until
    /// the next upgrade) and class-upgrade rotations.
    pub rotations_unzeroed: u64,
    pub rotations_upgrade: u64,
    /// A packed tail reopened `Buffered` under a `Direct` rotor (ADR-0086
    /// D4 as amended): the FLUSH→FUA transition on an existing log.
    pub reopened_packed_tails: u64,
    /// Tripwire (ADR-0086 D7): 1 once three consecutive everysec windows
    /// measured a write-through p50 above 3× the probed value. The device
    /// is not delivering the class it was probed for — visible, never an
    /// automatic class flip.
    pub barrier_class_degraded: u64,
    /// M4.5-S35 (ADR-0087 D5): the configured pipeline depth and the most
    /// frames observed in flight at once — a gate run proves the pipeline
    /// filled by the second, not by the first.
    pub frames_in_flight: u64,
    pub frames_in_flight_max: u64,
    /// Wait episodes: a staged frame held behind in-flight writes because
    /// its due barrier was inadmissible (`FramePlan::Wait`, ADR-0087 D3),
    /// and a staged frame held for a rotation drain (ADR-0087 D4) — the
    /// two bounded waits the pipeline introduces, counted per episode
    /// (one per held frame, not per LOG step) so they are never invisible
    /// and never inflated by the loop's iteration rate.
    pub frame_waits_barrier: u64,
    pub frame_waits_rotation: u64,
    /// A staged frame held because the ledger's reorder window is full
    /// (ADR-0087 D2 as amended): the front write is late and
    /// `REORDER_WINDOW_FRAMES` frames landed behind it. Counted per
    /// episode like the other two; a non-zero value names a wedging
    /// device, never an engine regression.
    pub frame_waits_reorder: u64,
    /// M4.5-S39a: frames held by the fill policy (per episode), and the
    /// policy in force (`fill_window_us` 0 = off).
    pub frame_waits_fill: u64,
    pub fill_window_us: u64,
    pub fill_target_bytes: u64,
    /// M4.5-S43 (ADR-0092): the FLUSH-class group hold — episodes, the
    /// window in force (0 = off) and the adaptive target (the records
    /// the last sealed frame carried).
    pub frame_waits_group: u64,
    pub flush_group_window_us: u64,
    pub frame_records_last: u64,
    /// The round target the hold waits for (the population the cell last
    /// measured at a frame's completion, ADR-0092 D1 as amended).
    pub group_round_target: u64,
    /// M4.5-S42 (ADR-0091 D5): the device model's provenance.
    pub io_provenance: IoProvenance,
    /// Instantaneous log quiescence gauges: frames sealed and awaiting
    /// `LogWritten`, and records staged but not yet sealed. Both zero ⇒
    /// every executed durable effect has reached the file — the DST's
    /// shadow-replay oracle waits on exactly this (ADR-0087 D7).
    pub frames_in_flight_now: u64,
    pub records_staged: u64,
    /// Everysec ticks the ledger counted idle (nothing dirty, nothing
    /// staged): the DST's tick-contract oracle (F-L01-01, batch 42).
    pub everysec_idle_ticks: u64,
    /// Frames queued and not yet fsync-covered in the ack map, and the
    /// ledger's unfolded entries: the map is bounded by the reorder
    /// window plus the entries plus one (F-L01-03, batch 42).
    pub frames_awaiting_watermark: u64,
    pub fsync_entries: u64,
    /// Batch 44: unfolded write-through tickets — bounded at the ledger's
    /// `WRITE_THROUGH_WINDOW_ENTRIES` (ADR-0087 D2 third amendment).
    pub write_through_entries: u64,
    /// Batch 43 (F-L01-04): `hold_open` = 1 while the LOG step is holding
    /// a frame or a standalone (an episode of `frame_waits_*`); the other
    /// two = 1 while the fill / group-hold episode clock is open. A clock
    /// is set by a hold and cleared by the issue, so an open clock is a
    /// hold — the DST's hold-episode oracle.
    pub hold_open: u64,
    pub fill_hold_open: u64,
    pub group_hold_open: u64,
    /// M4.5-S36 (ADR-0088 D7): the device budget's ledger — model
    /// presence, the cell's byte shares, per-class spent bytes/ops and
    /// deferrals (`IoClass::ALL` order) — and the seal pacer's wait
    /// episodes.
    pub io_budget_model_absent: u64,
    pub io_budget_write_bytes_per_s: u64,
    pub io_budget_read_bytes_per_s: u64,
    pub io_budget: [ClassCounters; IoClass::COUNT],
    pub frame_waits_pace: u64,
    /// On-disk log frame bytes (`CommitStats::frame_bytes_queued`,
    /// surfaced — header, trailer, v3 padding included), the checkpoint
    /// domain's bytes, and the derived trigger (ADR-0088 D4/D7).
    pub log_frame_bytes: u64,
    pub ckpt_bytes_total: u64,
    pub ckpt_bytes_last: u64,
    pub ckpt_padding_bytes: u64,
    pub manifest_bytes_total: u64,
    pub ckpt_interval_bytes: u64,
    pub ckpt_records_since_begin: u64,
    /// ADR-0088 D4 as amended: the cap's replay term this cell runs
    /// (probed read row ÷ cells, or the D4 constant) and the byte cap
    /// it derives (`replay_bytes_per_s × replay_budget_s`).
    pub ckpt_replay_bytes_per_s: u64,
    pub ckpt_cap_bytes: u64,
    /// 1 when checkpoint staging runs buffered (the probed `O_DIRECT`
    /// fallback, ADR-0088 D3 as amended).
    pub ckpt_io_mode_buffered: u64,
    pub ckpt_io_mode_downgrades: u64,
    /// ADR-0117: sections sealed for the section bound (the DST's
    /// engagement witness for the in-chain resume).
    pub ckpt_bound_splits: u64,
    /// `ceil_milli((log_frame_bytes + ckpt_bytes_total +
    /// manifest_bytes_total) / append_bytes)` — cell scope, boot life;
    /// undefined (0, with `_undefined = 1`) until the first checkpoint
    /// publishes so a log-only ratio is never read as the figure.
    /// Zero-fill is excluded and reported beside (`zero_fill_bytes`).
    pub write_amp_milli_log_checkpoint: u64,
    pub write_amp_log_checkpoint_undefined: u64,
    /// M4.5-S39b (ADR-0090 D4 as amended): every byte this cell handed
    /// the host for its durable state — log frames + zero-fill +
    /// checkpoint + MANIFEST. **Not** device traffic: journal/metadata,
    /// rename and extent work, SSD garbage collection and NAND
    /// amplification are invisible here (the S39b row samples the block
    /// device's sectors-written beside it). Cell scope, boot life.
    pub accounted_host_write_bytes: u64,
    /// `ceil_milli(accounted_host_write_bytes / append_bytes)` — the
    /// figure segment recycling is measured by (zero-fill included, which
    /// `write_amp_milli_log_checkpoint` excludes by ADR-0088 D7); 0 with
    /// `write_amp_log_checkpoint_undefined = 1` until the first
    /// checkpoint publishes, by the same rule.
    pub write_amp_milli_accounted_host: u64,
    /// Segment recycling (ADR-0090 D1/D4): next segments produced by
    /// renaming a covered pre-zeroed sealed segment; preallocs that found
    /// the pool empty (each followed by a zero-fill); pooled files that
    /// could not be reused ready (rename failed or read sparse); disk
    /// held by the pool now (`pooled × segment_bytes`).
    pub segments_recycled: u64,
    pub recycle_misses: u64,
    pub recycle_fallbacks: u64,
    pub recycle_pool_bytes: u64,
    /// Covered candidates offered to a full pool (unlinked) — a nonzero
    /// count with misses beside it says truncation comes in bursts the
    /// bound cannot hold (ADR-0090 A5), not that the pool never fills.
    pub recycle_pool_full: u64,
    /// Recycle sentinels written (ADR-0090 A15) — one per recycled take.
    pub recycle_sentinels: u64,
    /// Rotations and preallocs (MAINTAIN + inline) this boot life — the
    /// denominators the S39b row and the `m2-recycle` oracle read
    /// `segments_recycled` against.
    pub segment_rotations: u64,
    pub segment_preallocs: u64,
    /// The two MAINTAIN-cadence facts the D9 row reads beside the waits:
    /// rotations that found no next segment (a blocking prealloc on the
    /// loop) and preallocs refused for space.
    pub segment_inline_preallocs: u64,
    pub segment_prealloc_failures: u64,
    /// The pool wait (ADR-0090 D9): waits begun, fed before the bound,
    /// expired into a fresh prealloc, and how deep into the active
    /// segment the latest-ending wait ran.
    pub recycle_waits_started: u64,
    pub recycle_waits_satisfied: u64,
    pub recycle_waits_expired: u64,
    pub recycle_wait_active_bytes_max: u64,
    /// The worst frame-write submit → `LogWritten` latency (µs) — the
    /// `m2-device-budget` foreground-bound oracle's input (ADR-0088 D8);
    /// percentiles under-read a bound violation rarer than 1/1000.
    pub write_stall_max_us: u64,
}

/// One sealed frame awaiting its `LogWritten` (ADR-0087 D2): the lease
/// (the `StableBytes` custody point), the submit time for the write-stall
/// sample, and the barrier ticket the completion settles — a linked
/// sync's clock rebases at `LogWritten` (ADR-0083 D4), a write-through
/// ticket completes there (ADR-0086 D5). Keyed by `FrameId` == the write
/// token's sequence.
struct InFlightFrame {
    id: FrameId,
    lease: inf_log::FrameLease,
    submitted_at: Nanos,
    barrier: FrameBarrier,
}

/// The ticket a frame's `LogWritten` must settle, if any.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum FrameBarrier {
    None,
    Linked(FsyncTicket),
    WriteThrough(FsyncTicket),
}

/// One cell's durable plane state (plane-owned; `inf-store` never sees it).
pub(crate) struct DurableCell<F: SegmentFs> {
    pub staging: StagingRing,
    pub rotor: SegmentRotor<F>,
    pub commit: GroupCommit<F::File>,
    /// Ack gate keyed by durable seq (see module docs).
    pub ack_gate: WatermarkGate,
    /// Wakes pump futures parked on staging backpressure (`StagingFull`)
    /// once the in-flight frame releases.
    pub drained: WaitList<()>,
    /// Fuzzy-checkpoint driver (M2-S10, ADR-0016).
    pub ckpt: CkptCell<F>,
    /// MANIFEST + truncation driver (M2-S11, ADR-0017).
    pub manifest: ManifestCell<F>,
    /// Frames sealed and awaiting `LogWritten`, queue order; bounded by
    /// the ring's `frames_in_flight` and allocated once (never grows).
    in_flight: VecDeque<InFlightFrame>,
    /// The cell's zero window (ADR-0086 D4): `ZERO_FILL_SLICE_BYTES` (256
    /// KiB) of zeros, 4 KiB-aligned,
    /// never written — the source of every zero-fill `LogWrite`.
    /// Attributed to the log-staging domain.
    zero_window: AlignedBox,
    /// The recycle-sentinel window (ADR-0090 A15): one block, 4 KiB-
    /// aligned, rewritten from `sentinel_builder` just before each
    /// sentinel `LogWrite` is pushed and never while one is in flight
    /// (the rotor hands out one fill slice at a time).
    sentinel_window: AlignedBox,
    sentinel_builder: FrameBuilder,
    /// The zero-fill barrier's ticket while in flight: its `Synced` makes
    /// the next segment ready.
    zero_fill_ticket: Option<FsyncTicket>,
    /// Tripwire state (ADR-0086 D7): the probed reference and the number
    /// of consecutive everysec windows over 3× it.
    fua_p50_us_probed: u64,
    fua_degraded_windows: u32,
    fua_tick_count_prev: u64,
    fua_tick_window_sum_us: u64,
    /// Frame-write submit → `LogWritten` latency (µs) — the staging
    /// drain's binding variable (ADR-0083 D5).
    write_stall_hist: inf_foundation::LogHistogram,
    /// Cumulative admission park episodes (local pump + fabric pump).
    parked_total: u64,
    /// The two bounded waits of the frame pipeline (ADR-0087 D3/D4),
    /// counted per episode: `frame_held` is true from the first LOG step
    /// that held the staged frame until it seals.
    frame_waits_barrier: u64,
    frame_waits_rotation: u64,
    frame_waits_reorder: u64,
    frame_held: bool,
    /// M4.5-S39a: the fill policy and the hold it is in — `fill_since`
    /// is the instant the LOG step first saw the pending frame (set at
    /// the first hold decision, cleared at the seal).
    fill: FillConfig,
    fill_since: Option<Nanos>,
    frame_waits_fill: u64,
    /// M4.5-S43 (ADR-0092): the FLUSH-class group hold, its episode
    /// clock, its episodes, and the adaptive target — the records the
    /// last sealed frame carried.
    group: GroupHoldConfig,
    group_since: Option<Nanos>,
    frame_waits_group: u64,
    last_frame_records: u32,
    /// The population the last barrier's completion measured: the records
    /// it acked plus the records assigned during its flight (ADR-0092 D1
    /// as amended by campaigns A and B).
    group_round_target: u64,
    /// M4.5-S42 (ADR-0091 D5): the device model's provenance.
    io_provenance: IoProvenance,
    /// M4.5-S36 (ADR-0088 D2): the cell's device budget — refilled at
    /// every MAINTAIN entry from the injected clock, consulted by the
    /// background issuing sites, charged by the foreground ones.
    budget: DeviceBudget,
    /// ADR-0088 D2b: the frame-seal pacer (a due frame behind in-flight
    /// frames seals only when the device's barrier rate allows).
    seal_pace: SealPace,
    /// Whether a write-through consumer (an `always` namespace) exists on
    /// the cell this tick — the zero-fill gate (ADR-0088 D5 amended).
    write_through_wanted: bool,
    /// Last durable seq assigned (0 = none; the gate starts at 0).
    last_seq: u64,
    /// Last seq the ack gate advanced to (group-formation bookkeeping).
    acked_seq: u64,
    /// Records newly covered per durability-fsync completion (M2.5-S07):
    /// the group-formation distribution the ≥ 0.8× gate reads.
    group_hist_records: inf_foundation::LogHistogram,
    /// Frames queued but not yet durable: (exclusive-end LSN, last seq).
    /// Bounded (F-L01-03, batch 42): written entries across which no
    /// ledger coverage point lies coalesce at every `LogWritten`
    /// ([`coalesce_ack_map`]), so the map holds at most the frames behind
    /// the written prefix (the reorder window), one entry per unfolded
    /// ledger entry, and one more — never one per frame sealed behind a
    /// stalled barrier or between two ticks.
    frame_seqs: VecDeque<(Lsn, u64)>,
    write_seq: u64,
    records_appended: u64,
    acks_gated: u64,
    /// M2-S21 windowed rates: counters snapshotted at the everysec tick;
    /// the delta is "per second" against the injected clock.
    tick_fsyncs_prev: u64,
    tick_acks_prev: u64,
    fsyncs_last_sec: u64,
    acks_last_sec: u64,
    /// §8.4: a terminal log-I/O error freezes the cell's durable plane
    /// (checked before fail-stop so tests can observe the frozen state).
    pub failed: bool,
}

impl<F: SegmentFs> DurableCell<F> {
    pub fn new(
        cfg: &DurableConfig,
        rotor: SegmentRotor<F>,
        ckpt: CkptCell<F>,
        manifest: ManifestCell<F>,
    ) -> DurableCell<F> {
        let (staging, flush_bound, fua_p50_us_probed, device, fill, group) =
            (cfg.staging, cfg.flush_bound, cfg.fua_p50_us_probed, cfg.device, cfg.fill, cfg.group);
        // ADR-0031 D5/D6: frames sealed here stamp the recovery-derived
        // log life (1 on fresh logs).
        let mut staging = StagingRing::new(staging);
        staging.set_frame_epoch(rotor.resume_epoch());
        let in_flight = VecDeque::with_capacity(usize::from(staging.frames_in_flight()));
        // ADR-0088 D2: each background class's smallest offer — its
        // deficit cap can never be below one slice.
        let mut slices = [ClassSlice { bytes: 0, ops: 0 }; IoClass::COUNT];
        slices[IoClass::ZeroFill.index()] =
            ClassSlice { bytes: u64::from(ZERO_FILL_SLICE_BYTES), ops: 1 };
        slices[IoClass::TierFlush.index()] = ClassSlice {
            bytes: inf_store::TierSpec::for_budget(0).maintain_slice_bytes,
            ops: crate::tier_cell::TIER_ROUND_MAX_OPS,
        };
        slices[IoClass::Checkpoint.index()] = ClassSlice {
            bytes: inf_log::ckpt::ick_align_up(ckpt.cfg.section_bytes as usize + 16) as u64,
            ops: 1,
        };
        slices[IoClass::ColdReadMaintain.index()] =
            ClassSlice { bytes: crate::tier_cell::COLD_POOL_BUF as u64, ops: 1 };
        let budget = DeviceBudget::new(device.model_share, slices, ckpt.cfg.alpha, Nanos(0));
        let seal_pace = SealPace::new(
            device.seal_barriers_per_s,
            u32::from(staging.frames_in_flight()),
            Nanos(0),
        );
        DurableCell {
            staging,
            rotor,
            commit: GroupCommit::with_flush_bound(usize::from(flush_bound)),
            ack_gate: WatermarkGate::new(),
            drained: WaitList::new(),
            ckpt,
            manifest,
            in_flight,
            zero_window: AlignedBox::new(ZERO_FILL_SLICE_BYTES as usize),
            sentinel_window: AlignedBox::new(FRAME_ALIGN as usize),
            sentinel_builder: FrameBuilder::with_capacity(FRAME_ALIGN as usize),
            zero_fill_ticket: None,
            fua_p50_us_probed,
            fua_degraded_windows: 0,
            fua_tick_count_prev: 0,
            fua_tick_window_sum_us: 0,
            write_stall_hist: inf_foundation::LogHistogram::new(),
            parked_total: 0,
            frame_waits_barrier: 0,
            frame_waits_rotation: 0,
            frame_waits_reorder: 0,
            frame_held: false,
            fill,
            fill_since: None,
            frame_waits_fill: 0,
            group,
            group_since: None,
            frame_waits_group: 0,
            last_frame_records: 0,
            group_round_target: 0,
            io_provenance: device.provenance,
            budget,
            seal_pace,
            write_through_wanted: true,
            last_seq: 0,
            acked_seq: 0,
            group_hist_records: inf_foundation::LogHistogram::new(),
            frame_seqs: VecDeque::new(),
            write_seq: 0,
            records_appended: 0,
            acks_gated: 0,
            tick_fsyncs_prev: 0,
            tick_acks_prev: 0,
            fsyncs_last_sec: 0,
            acks_last_sec: 0,
            failed: false,
        }
    }

    /// Arm the boot-metadata barriers (M2.5-S01): one driver-ridden
    /// fdatasync per boot directory handle plus one on the active segment
    /// fd, registered at the head of the commit ledger so the done-prefix
    /// rule fences every durable ack (and the manifest's watermark guard)
    /// behind boot-metadata durability. Boot-ready never waits on them —
    /// that is the fix for the ADR-0022 D7 wedge: the old blocking
    /// dir-fsyncs could stall a reactor thread for minutes behind foreign
    /// journal writeback. Barrier failure surfaces as an fsync-error CQE
    /// → fail-stop (§8.4), same as any durability sync.
    pub fn arm_boot_barriers(&mut self, cx: &mut LoopCx<'_>, dirs: Vec<F::File>) {
        let floor = self.rotor.append_cursor();
        for handle in dirs {
            // Fd-less tiers (MemFs) have process-KILL physics — completed
            // writes survive by construction, so they carry no barriers.
            let Some(fd) = inf_log::fs::SegmentFile::raw_fd(&handle) else { continue };
            let ticket = self.commit.register_boot_barrier(floor, Some(handle), cx.now);
            self.budget.charge(IoClass::LogFrame, 0, 1); // ledger-class barrier (ADR-0088 D7)
            cx.push(IoOp::Fdatasync { fd, token: fsync_token(ticket) });
        }
        if let Some(fd) = self.rotor.active_raw_fd() {
            let ticket = self.commit.register_boot_barrier(floor, None, cx.now);
            self.budget.charge(IoClass::LogFrame, 0, 1); // ledger-class barrier (ADR-0088 D7)
            cx.push(IoOp::Fdatasync { fd, token: fsync_token(ticket) });
        }
    }

    /// Admission check for one command's worth of effects (the caller's
    /// conservative byte estimate). `false` = park on [`Self::drained`].
    pub fn would_fit(&self, bytes: usize) -> bool {
        self.staging.would_fit(bytes)
    }

    /// Current aggregate append budget and the absolute single-record
    /// ceiling. Durable document handlers use both before committing an
    /// exact planned full image (ADR-0043 D8).
    #[cfg(feature = "doc")]
    pub fn staging_limits(&self) -> (usize, usize) {
        (self.staging.remaining_capacity() as usize, self.staging.max_record_len() as usize)
    }

    /// Stage one effect. Admission was checked via [`Self::would_fit`]
    /// with a conservative estimate, so refusal here is an invariant
    /// violation, not backpressure. Returns the record's durable seq.
    pub fn stage(&mut self, effect: &MutationEffect<'_>, class: FsyncClass) -> u64 {
        assert!(!self.failed, "staging into a failed durable cell");
        let _at = self.staging.stage(effect).expect("admission pre-checked by would_fit");
        self.commit.note_staged(class);
        self.records_appended += 1;
        self.last_seq += 1;
        self.last_seq
    }

    /// Stage one tiered-namespace effect through the table's accounting
    /// funnel (M4-S26): [`inf_store::TieredTable::stage_wal`] charges the
    /// namespace's `wal_bytes` and stamps the extent reclaim epoch —
    /// exactly what a bare [`stage`](Self::stage) would silently skip.
    /// Seq and commit-ledger bookkeeping match [`stage`](Self::stage).
    pub fn stage_tiered(
        &mut self,
        table: &mut inf_store::TieredTable,
        effect: &MutationEffect<'_>,
        class: FsyncClass,
    ) -> u64 {
        assert!(!self.failed, "staging into a failed durable cell");
        let _at =
            table.stage_wal(&mut self.staging, effect).expect("admission pre-checked by would_fit");
        self.commit.note_staged(class);
        self.records_appended += 1;
        self.last_seq += 1;
        self.last_seq
    }

    /// True when no checkpoint is streaming and no MANIFEST swap is
    /// pending — the tiered MAINTAIN's reconciliation signal (M4-S26):
    /// in this state a still-pinned walk or a stamped-but-uncommitted
    /// retirement can only mean the transition aborted.
    pub fn ckpt_transition_idle(&self) -> bool {
        matches!(self.ckpt.phase, CkptPhase::Idle) && self.manifest.idle()
    }

    /// Registers one gated `always` ack (counter only; the waiter itself
    /// is `ack_gate.waiter(seq)` at the dispatch site).
    pub fn note_gated_ack(&mut self) {
        self.acks_gated += 1;
    }

    /// MAINTAIN slice: keep the next segment preallocated (rotation stays
    /// a pointer swap — S02). The prealloc is unsynced (M2.5-S01): its
    /// log-dir fdatasync rides the driver as a coverage-neutral ledger
    /// barrier instead of blocking the reactor behind the journal. Under
    /// `Direct` the next segment is then pre-zeroed through the driver,
    /// one slice per completion, and its zero-fill barrier registered
    /// coverage-neutral (ADR-0086 D4) — never a blocking write here.
    /// `write_through_wanted` is whether any `always` namespace exists on
    /// the cell (ADR-0088 D5 amended): pre-zeroing exists to make frames
    /// write-through eligible, so a cell with no write-through consumer
    /// skips it — the next segment is taken un-zeroed (`rotations_
    /// unzeroed`, FLUSH-class seal syncs, plain `O_DIRECT` frames), and
    /// the first `always` namespace created later restarts the fill and
    /// upgrades the class at the next rotation (ADR-0086 D4's machinery).
    /// The S36 row measured the second write at 35–50 % of the device's
    /// bytes on an `everysec`-only cell.
    pub fn maintain(&mut self, cx: &mut LoopCx<'_>, write_through_wanted: bool) {
        if self.failed {
            return;
        }
        // ADR-0088 D2: one refill per MAINTAIN entry, injected clock.
        self.budget.refill(cx.now);
        self.write_through_wanted = write_through_wanted;
        match self.rotor.maintain_deferred(cx.now.as_millis()) {
            Ok((_report, Some(barrier))) => {
                // Fd-less tiers (MemFs) have process-KILL physics — no
                // barrier needed (completed writes survive by construction).
                if let Some(fd) = inf_log::fs::SegmentFile::raw_fd(&barrier.dir) {
                    let ticket = self.commit.register_prealloc_barrier(barrier.dir, cx.now);
                    // ledger-class barrier (ADR-0088 D7)
                    self.budget.charge(IoClass::LogFrame, 0, 1);
                    cx.push(IoOp::Fdatasync { fd, token: fsync_token(ticket) });
                }
            }
            Ok((_report, None)) => {}
            Err(err) => {
                // ENOSPC discipline (S02): surfaced before any write needs
                // the space; `space_exhausted()` gates admission at the
                // command layer. Other I/O errors are fail-stop territory.
                if !self.rotor.space_exhausted() {
                    self.fail_stop("segment maintain", &err.to_string());
                }
            }
        }
        self.zero_fill(cx);
    }

    /// Drive the next segment's pre-zeroing (ADR-0086 D4): issue the next
    /// zero slice when none is in flight; register and issue the barrier
    /// once every zero byte landed.
    fn zero_fill(&mut self, cx: &mut LoopCx<'_>) {
        // ADR-0088 D5: the head-start bound says "no further"; the budget
        // says "not this slice". The budget is asked with the slice bound
        // *before* the slice is taken — `next_zero_slice` marks it in
        // flight, and a taken-but-unissued slice is a phantom the
        // rotation waits on forever (the sweep's finding) — and the
        // unissued remainder of the bound is refunded.
        let bound = u64::from(ZERO_FILL_SLICE_BYTES);
        if self.write_through_wanted
            && self.rotor.zero_fill_pending()
            && self.budget.admit(IoClass::ZeroFill, bound, 1) == Admission::Granted
        {
            let Some(slice) = self.rotor.next_zero_slice(ZERO_FILL_SLICE_BYTES) else {
                self.budget.refund(IoClass::ZeroFill, bound, 1);
                return;
            };
            self.budget.refund(IoClass::ZeroFill, bound - u64::from(slice.len), 0);
            let data = match slice.source {
                FillSource::Zeros => log_bytes::zero_window(&self.zero_window, slice.len),
                // ADR-0090 A15: the recycled file's one-block sentinel,
                // built for its old id and copied into the aligned window.
                FillSource::RecycleSentinel { old } => {
                    let image = build_recycle_sentinel(
                        &mut self.sentinel_builder,
                        old,
                        self.rotor.segment_bytes(),
                    );
                    self.sentinel_window.bytes_mut().copy_from_slice(image);
                    log_bytes::zero_window(&self.sentinel_window, slice.len)
                }
            };
            cx.push(IoOp::LogWrite {
                fd: slice.fd,
                offset: slice.offset,
                data,
                token: CompletionToken::new(TokenClass::ZeroFillWrite, 0, 0),
                barrier: WriteBarrier::None,
            });
        }
        if self.zero_fill_ticket.is_none()
            && let Some(fd) = self.rotor.take_zero_fill_barrier()
        {
            let ticket = self.commit.register_zero_fill_barrier(cx.now);
            self.zero_fill_ticket = Some(ticket);
            self.budget.charge(IoClass::LogFrame, 0, 1); // ledger-class barrier (ADR-0088 D7)
            cx.push(IoOp::Fdatasync { fd, token: fsync_token(ticket) });
        }
    }

    /// REAP: a zero-fill slice's `LogWritten` — advance the rotor's
    /// cursor; the next slice issues at the next MAINTAIN.
    pub fn on_zero_fill_written(&mut self) {
        self.rotor.note_zero_slice_written();
    }

    /// The LOG step (ADR-0013 D2, ADR-0087 D3/D4): seal at most one frame
    /// into one positional write carrying the barrier the ledger's plan
    /// allows, or issue a standalone fdatasync for a frameless everysec
    /// tick. A frame waits — bounded, one write latency — when it needs
    /// a rotation while frames are in flight, when a sync is due that
    /// neither write-through nor a linked fdatasync may carry yet, or
    /// when the ledger's reorder window is full behind a late front
    /// write (ADR-0087 D2 as amended).
    pub fn seal_log(&mut self, cx: &mut LoopCx<'_>) {
        if self.failed {
            return;
        }
        if self.staging.can_seal() {
            // ADR-0061 D3 (amended 2026-08-06): a pending extent-seal
            // barrier holds this frame. The staged frame may carry the
            // barrier's referencing record, and a durable record naming
            // torn extent bytes replays as a dangling reference — the
            // ledger barrier fences the ack, never the device order.
            // The barrier's fdatasync rode this iteration's MAINTAIN
            // push, so the hold is one device sync; the frame waits
            // exactly like the ENOSPC branch below.
            if self.commit.extent_barrier_pending() {
                return;
            }
            // ADR-0088 D2b: a second frame behind in-flight ones seals at
            // the device's barrier rate; a drained cell always seals.
            if !self.staging.drained() && !self.seal_pace.take(cx.now, self.frame_held) {
                self.frame_held = true;
                return;
            }
            let frame_len = self.staging.pending_frame_len();
            // Rotation is a drain point (ADR-0087 D4): the seal fdatasync
            // covers the old segment only once every write into it has
            // completed.
            let rotation_due = self.rotor.rotation_due(frame_len);
            if rotation_due && !self.staging.drained() {
                self.frame_waits_rotation += u64::from(!self.frame_held);
                self.frame_held = true;
                return;
            }
            // Barrier plan before any state moves (ADR-0087 D3).
            let write_through_ok = self.rotor.next_frame_write_through_ok(frame_len);
            let plan = self.commit.frame_plan(write_through_ok, rotation_due);
            if plan == FramePlan::Wait {
                // Two reasons, two counters: the reorder window is full
                // (ADR-0087 D2 as amended — a device row), or the due
                // barrier is inadmissible behind in-flight writes (D3).
                if self.commit.reorder_window_full() {
                    self.frame_waits_reorder += u64::from(!self.frame_held);
                } else {
                    self.frame_waits_barrier += u64::from(!self.frame_held);
                }
                self.frame_held = true;
                return;
            }
            // M4.5-S39a: the fill policy — the last hold, after every
            // correctness hold above, on aligned segments only.
            if self.fill_holds(cx, plan, frame_len, rotation_due) {
                self.frame_waits_fill += u64::from(!self.frame_held);
                self.frame_held = true;
                return;
            }
            // M4.5-S43 (ADR-0092): the FLUSH-class group hold — after the
            // fill (which never holds a barrier frame), on packed
            // segments only, off unless the arm is on.
            if self.group_holds(cx, plan, rotation_due) {
                self.frame_waits_group += u64::from(!self.frame_held);
                self.frame_held = true;
                return;
            }
            // The hold episode ends at the issue (`note_issued`), not
            // here: a reservation that waits below keeps the episode.
            let deferred = match self.rotor.begin_frame_deferred(frame_len, cx.now.as_millis()) {
                Ok(deferred) => deferred,
                Err(inf_log::LogError::NextNotReady { .. }) => {
                    // A zero-fill op is in flight on the segment rotation
                    // needs (ADR-0086 D4): the frame waits one completion,
                    // exactly like the ENOSPC branch below.
                    return;
                }
                Err(err) => {
                    if self.rotor.space_exhausted() {
                        // Typed refusal already gates new admissions; the
                        // staged frame waits for space (maintain retries).
                        return;
                    }
                    self.fail_stop("frame reserve", &err.to_string());
                }
            };
            let (slot, seal) = deferred;
            // The plan was made from the same rotor state the reservation
            // reports — a disagreement would seal a frame into a barrier
            // it cannot have.
            assert_eq!(seal.is_some(), rotation_due, "rotation plan matches the reservation");
            assert_eq!(slot.write_through_ok(), write_through_ok, "barrier plan matches the slot");
            if let Some(handoff) = seal {
                let fd = handoff.raw_fd().expect("std segment tier has fds");
                let ticket = self.commit.register_seal_fsync(handoff, cx.now);
                self.budget.charge(IoClass::LogFrame, 0, 1); // ledger-class barrier (ADR-0088 D7)
                cx.push(IoOp::Fdatasync { fd, token: fsync_token(ticket) });
            }
            self.queue_frame(cx, slot, plan);
        } else if self.commit.standalone_fsync_due() {
            // M4.5-S43 (ADR-0092 D1 as amended by campaign B): a barrier
            // completing with nothing pending would issue a standalone
            // fdatasync for the plain frames at once — the same round the
            // linked-fsync frame waits for, so it waits by the same rule.
            if self.group_holds(cx, FramePlan::LinkedFsync, false) {
                self.frame_waits_group += u64::from(!self.frame_held);
                self.frame_held = true;
                return;
            }
            self.note_issued();
            let ticket = self.commit.register_standalone_fsync(cx.now);
            let fd = self.rotor.active_raw_fd().expect("std segment tier has fds");
            self.budget.charge(IoClass::LogFrame, 0, 1); // ledger-class barrier (ADR-0088 D7)
            cx.push(IoOp::Fdatasync { fd, token: fsync_token(ticket) });
        }
    }

    /// The LOG step issued what it was holding — a frame or a standalone
    /// fdatasync: the hold episode and both policy clocks end here, and
    /// only here (batch 43, F-L01-04: the standalone path left the group
    /// clock open, so the next episode's first sight read as elapsed and
    /// sealed at once; the fill and group episodes end at the issue, not
    /// at the decision — a reservation that waits keeps its episode, and
    /// a hold that follows it is the same episode, never a second one in
    /// `frame_waits_*`).
    fn note_issued(&mut self) {
        self.frame_held = false;
        self.fill_since = None;
        self.group_since = None;
    }

    /// M4.5-S39a: should the pending frame be held for fill? The pure
    /// decision is [`FillConfig::decide`]; this adds the episode clock
    /// (`fill_since`, the instant the LOG step first saw the frame) and,
    /// at the first hold of an episode, arms the fill timer at the
    /// deadline so a parked loop wakes to seal. Asked last, after every
    /// correctness hold — a frame the plan would `Wait` never gets here.
    fn fill_holds(
        &mut self,
        cx: &mut LoopCx<'_>,
        plan: FramePlan,
        frame_len: u32,
        rotation_due: bool,
    ) -> bool {
        if !self.fill.enabled() {
            return false;
        }
        // Release assert (batch 43): an open episode clock is a held
        // frame — the clock is set by a hold and cleared by the issue.
        assert!(
            self.fill_since.is_none() || self.frame_held,
            "open fill episode without a held frame"
        );
        let layout =
            if rotation_due { self.rotor.next_layout() } else { self.rotor.active_layout() };
        match self.fill.decide(plan, layout, frame_len, self.fill_since, cx.now) {
            FillDecision::Seal => false,
            FillDecision::Hold => {
                if self.fill_since.is_none() {
                    self.fill_since = Some(cx.now);
                    cx.timers.insert(cx.now + self.fill.window, FILL_TIMER_KEY);
                }
                true
            }
        }
    }

    /// M4.5-S43 (ADR-0092 D1): should the pending frame wait for the
    /// round it just acked? The pure decision is
    /// [`GroupHoldConfig::decide`]; this adds the episode clock and, at
    /// the first hold of an episode, arms the group timer so a parked
    /// loop wakes to seal. Asked last — after the fill policy, which
    /// never holds a barrier-carrying frame, so the two holds never
    /// compete for one frame.
    fn group_holds(&mut self, cx: &mut LoopCx<'_>, plan: FramePlan, rotation_due: bool) -> bool {
        if !self.group.enabled() {
            return false;
        }
        // Release assert (batch 43, F-L01-04): an open episode clock is a
        // held frame or standalone — a stale clock reads as elapsed and
        // seals the next episode's first sight at once.
        assert!(
            self.group_since.is_none() || self.frame_held,
            "open group-hold episode without a held frame"
        );
        let layout =
            if rotation_due { self.rotor.next_layout() } else { self.rotor.active_layout() };
        let uncovered = self.last_seq.saturating_sub(self.acked_seq);
        let decision = self.group.decide(
            plan,
            layout,
            uncovered,
            self.group_round_target,
            self.group_since,
            cx.now,
        );
        match decision {
            GroupDecision::Seal => false,
            GroupDecision::Hold => {
                if self.group_since.is_none() {
                    self.group_since = Some(cx.now);
                    cx.timers.insert(cx.now + self.group.window, GROUP_TIMER_KEY);
                }
                true
            }
        }
    }

    /// Seal the pending records into `slot`, register the planned barrier,
    /// and hand the frame to the driver. The frame's on-device extent
    /// (padding included on aligned segments) is what the cursor, the
    /// ledger, and the barrier all advance by (ADR-0086 D3).
    fn queue_frame(&mut self, cx: &mut LoopCx<'_>, slot: inf_log::FrameSlot, plan: FramePlan) {
        self.note_issued();
        self.last_frame_records = self.staging.pending_records();
        let end = slot.base().advance(slot.len());
        let covered = self.commit.watermark().map_or(0, |lsn| lsn.to_u64());
        let lease = self.staging.seal(slot.first_record_lsn(), covered, slot.layout());
        debug_assert_eq!(lease.frame_len(), slot.len(), "sealed bytes match the reservation");
        // A pending ckpt-begin marker rides this frame: its LSN is now
        // real (ADR-0016 D3).
        self.ckpt.on_frame_sealed(&lease);
        self.frame_seqs.push_back((end, self.last_seq));
        let id = self.commit.note_frame_queued(end, slot.len());
        // Barrier class per frame (ADR-0086 D1, ADR-0087 D3): the plan
        // decided before the seal; `Wait` never reaches here.
        let (barrier, ticket) = match plan {
            FramePlan::WriteThrough => {
                let ticket = self.commit.register_write_through(cx.now);
                (WriteBarrier::WriteThrough, FrameBarrier::WriteThrough(ticket))
            }
            FramePlan::LinkedFsync => {
                let ticket = self.commit.register_linked_fsync(cx.now);
                // The linked sync's clock rebases at `LogWritten`
                // (ADR-0083 D4): its SQE starts only after the write.
                (
                    WriteBarrier::LinkedFsync { fsync_token: fsync_token(ticket) },
                    FrameBarrier::Linked(ticket),
                )
            }
            FramePlan::Plain => (WriteBarrier::None, FrameBarrier::None),
            FramePlan::Wait => unreachable!("a waiting frame is never sealed"),
        };
        let offset = u64::from(slot.base().offset);
        let fd = self.rotor.active_raw_fd().expect("std segment tier has fds");
        // ADR-0088 D2: the foreground is metered (one write, plus the
        // linked barrier when the plan carries one), never deferred.
        let ops = 1 + u64::from(matches!(ticket, FrameBarrier::Linked(_)));
        self.budget.charge(IoClass::LogFrame, u64::from(slot.len()), ops);
        self.rotor.commit_frame_queued(slot);
        self.write_seq += 1;
        debug_assert_eq!(self.write_seq, id.0, "write token sequence is the frame id");
        let data = log_bytes::sealed_frame(&self.staging, &lease);
        debug_assert!(
            self.in_flight.len() < usize::from(self.staging.frames_in_flight()),
            "in-flight table sized to the ring"
        );
        self.in_flight.push_back(InFlightFrame {
            id,
            lease,
            submitted_at: cx.now,
            barrier: ticket,
        });
        cx.push(IoOp::LogWrite { fd, offset, data, token: write_token(self.write_seq), barrier });
    }

    /// REAP: a frame write's terminal completion — release its lease (the
    /// `StableBytes` custody point) and wake staging-parked pumps. Any
    /// order relative to other frames (ADR-0087 D2): the ledger advances
    /// its written prefix, the ring frees the lease's buffer. Records the
    /// write-stall sample (submit → `LogWritten`, the staging drain's
    /// binding variable) and settles the frame's barrier: a linked sync's
    /// latency clock rebases here so the fsync histogram measures the
    /// sync, not the chain (ADR-0083 D4); a write-through ticket completes
    /// here — this completion IS the durability fact, routed through the
    /// same done-prefix path a `Synced` takes so acks stay a prefix behind
    /// any earlier entry (ADR-0086 D5).
    pub fn on_log_written(&mut self, cx: &mut LoopCx<'_>, token: CompletionToken) {
        let now = cx.now;
        let id = FrameId(write_seq_of(token));
        let index = self
            .in_flight
            .iter()
            .position(|frame| frame.id == id)
            .expect("LogWritten for a frame not in flight");
        let frame = self.in_flight.remove(index).expect("index from position");
        self.write_stall_hist.record(now.saturating_sub(frame.submitted_at).as_micros());
        self.commit.note_frame_written(frame.id);
        self.coalesce_frame_seqs();
        self.staging.release(frame.lease);
        self.drained.wake_all(());
        match frame.barrier {
            FrameBarrier::None => {}
            FrameBarrier::Linked(ticket) => self.commit.rebase_clock(ticket, now),
            FrameBarrier::WriteThrough(ticket) => self.on_synced(cx, fsync_token(ticket)),
        }
    }

    /// F-L01-03 (batch 42): coalesce the ack map over the written prefix
    /// and release-assert its bound — the frames behind the prefix plus
    /// one entry per unfolded ledger entry plus one, by construction of
    /// [`coalesce_ack_map`]. Per `LogWritten`, over the entries the
    /// prefix newly reached: O(coverage points) per call.
    fn coalesce_frame_seqs(&mut self) {
        let Some(written) = self.commit.written_up_to() else { return };
        let commit = &self.commit;
        coalesce_ack_map(&mut self.frame_seqs, written, |lo, hi| {
            commit.pending_covers_within(lo, hi)
        });
        assert!(
            self.frame_seqs.len()
                <= self.commit.frames_behind_prefix() + self.commit.pending_entries() + 1,
            "ack map outgrew the reorder window plus the ledger's coverage points"
        );
    }

    /// One admission park episode began (local pump or fabric pump) —
    /// the pacing observable's cumulative half (ADR-0083 D5).
    pub fn note_parked(&mut self) {
        self.parked_total += 1;
    }

    /// REAP: an fsync completed — advance the durability watermark, wake
    /// every ack whose frame it covers (FIFO by seq == by LSN), record the
    /// group-formation sample, and (M2.5-S07, pipeline bound ≥ 2) issue
    /// the deferred sync immediately at this CQE instead of waiting for
    /// the next LOG step.
    pub fn on_synced(&mut self, cx: &mut LoopCx<'_>, token: CompletionToken) {
        // M2-S17 fsyncgate: a firing point turns this completion into the
        // device-reported-EIO path — deterministic stand-in for an fsync
        // error CQE (the reactor tier's analog of `fsync_err`, ADR-0020).
        if inf_foundation::fault::fire(crate::fault::DURABLE_FSYNC_EIO) {
            self.on_log_error(token, EIO);
        }
        let ticket = token_ticket(token);
        if self.zero_fill_ticket == Some(ticket) {
            // The next segment's extent metadata is committed: it is
            // pre-zeroed and ready (ADR-0086 D4).
            self.zero_fill_ticket = None;
            self.rotor.note_zero_fill_synced();
        }
        if let Some(end) = self.commit.on_fsync_complete(ticket, cx.now) {
            let mut last_covered = None;
            while self.frame_seqs.front().is_some_and(|&(end_lsn, _)| end_lsn <= end) {
                last_covered = self.frame_seqs.pop_front().map(|(_, seq)| seq);
            }
            if let Some(seq) = last_covered {
                // Group formation (M2.5-S07): records newly covered by
                // this completion — the distribution behind the
                // ≥ 0.8× available-in-flight-writes gate.
                debug_assert!(seq >= self.acked_seq, "ack seq regressed — frame_seqs FIFO broken");
                // Saturating like the round target below: a regressed seq
                // is a wrong sample, never a wrapped one (F-L16-02).
                self.group_hist_records.record(seq.saturating_sub(self.acked_seq));
                // ADR-0092 D1 (amended by campaigns A and B): the round the
                // group hold waits for is the population this barrier's
                // completion reveals — the records it acks plus every
                // record assigned during its flight (the plain frames the
                // busy FLUSH slot let through, and the staging buffer).
                self.group_round_target = self.last_seq.saturating_sub(self.acked_seq);
                self.acked_seq = seq;
                self.ack_gate.advance(seq);
            }
        }
        if self.commit.completion_fsync_due() {
            let ticket = self.commit.register_completion_fsync(cx.now);
            let fd = self.rotor.active_raw_fd().expect("std segment tier has fds");
            self.budget.charge(IoClass::LogFrame, 0, 1); // ledger-class barrier (ADR-0088 D7)
            cx.push(IoOp::Fdatasync { fd, token: fsync_token(ticket) });
        }
    }

    /// Timer: the everysec tick (idle ticks are free — counted, no I/O).
    pub fn on_everysec_tick(&mut self, cx: &mut LoopCx<'_>) {
        // M2-S21: the tick doubles as the 1 s rate window (injected clock).
        let fsyncs = self.commit.stats().fsyncs_completed;
        self.fsyncs_last_sec = fsyncs - self.tick_fsyncs_prev;
        self.tick_fsyncs_prev = fsyncs;
        self.acks_last_sec = self.acks_gated - self.tick_acks_prev;
        self.tick_acks_prev = self.acks_gated;
        self.check_barrier_class_tripwire();
        self.commit.note_everysec_tick();
        cx.timers.insert(cx.now + Nanos::from_secs(1), EVERYSEC_TIMER_KEY);
    }

    /// The stop drain's final sync (ADR-0124 D2 step 4): the everysec
    /// tick's ledger rule now, without waiting for the wheel — a dirty
    /// ledger seals its frame and issues the barrier at the next LOG
    /// step; a clean one is free (counted idle).
    pub fn request_final_sync(&mut self) {
        self.commit.note_everysec_tick();
    }

    /// Nothing durable is in motion: no record staged, no frame in
    /// flight, no fsync pending, no sync owed, no checkpoint or MANIFEST
    /// transition open. The stop drain's exit condition (ADR-0124 D2).
    pub fn quiescent(&self) -> bool {
        self.staging.is_empty()
            && self.staging.drained()
            && self.in_flight.is_empty()
            && self.commit.pending_fsyncs() == 0
            && !self.commit.sync_due()
            && self.ckpt_transition_idle()
    }

    /// `barrier_class_degraded` (ADR-0086 D7): the window's mean
    /// write-through latency against 3× the probed p50, three consecutive
    /// breaching windows set the flag, one healthy window clears it. The
    /// histogram is cumulative, so the window is a sum/count delta (the
    /// mean is the honest per-window statistic available without a
    /// per-window histogram; it is ≥ the window's p50, so it trips no
    /// later than a p50 rule would).
    fn check_barrier_class_tripwire(&mut self) {
        if self.fua_p50_us_probed == 0 {
            return;
        }
        let hist = self.commit.write_through_latency_hist();
        let count = hist.count();
        let sum_us = hist.sum();
        let window_count = count - self.fua_tick_count_prev;
        let window_sum = sum_us - self.fua_tick_window_sum_us;
        self.fua_tick_count_prev = count;
        self.fua_tick_window_sum_us = sum_us;
        if window_count == 0 {
            return;
        }
        let mean_us = window_sum / window_count;
        if mean_us > 3 * self.fua_p50_us_probed {
            self.fua_degraded_windows = self.fua_degraded_windows.saturating_add(1);
        } else {
            self.fua_degraded_windows = 0;
        }
    }

    /// §8.4 fail-stop: a terminal error on the durable path. The watermark
    /// freezes (no ack for the affected batch can ever fire), the typed
    /// error goes to stderr, and the process exits with
    /// [`EXIT_DURABLE_FAILSTOP`](crate::EXIT_DURABLE_FAILSTOP) — fsync
    /// failure is never caught-and-continued and never retried against
    /// possibly-clean pages (the fsyncgate rule; exit codes formalized at
    /// M2-S17, ADR-0020 D3).
    pub fn fail_stop(&mut self, what: &str, detail: &str) -> ! {
        self.failed = true;
        eprintln!("durable-path {what} failed (fail-stop, §8.4): {detail}");
        std::process::exit(crate::EXIT_DURABLE_FAILSTOP);
    }

    /// Terminal error routed from REAP (write or fsync token).
    pub fn on_log_error(&mut self, token: CompletionToken, errno: i32) -> ! {
        if token.class() == TokenClass::Fsync {
            // fsync-fail-stop-allow: records the freeze for the ledger; the very next statement is
            // fail_stop() -> !, so the SyncReason has no reader and no path continues
            let _ = self.commit.on_fsync_error(token_ticket(token));
        }
        self.fail_stop("I/O", &format!("errno {errno} on {:?}", token.class()))
    }

    /// Tier-flush and compaction offer their slices here (ADR-0088 D5);
    /// the plane owns the tier cell, the cell owns the budget.
    pub fn admit_background(&mut self, class: IoClass, bytes: u64, ops: u64) -> Admission {
        debug_assert!(!class.is_foreground(), "foreground classes charge, never ask");
        self.budget.admit(class, bytes, ops)
    }

    /// Return a granted offer's unissued remainder (ADR-0088 D5).
    pub fn refund_background(&mut self, class: IoClass, bytes: u64, ops: u64) {
        debug_assert!(!class.is_foreground());
        self.budget.refund(class, bytes, ops);
    }

    /// The cold-read drain's refund: either class (a foreground refund
    /// only corrects the counters).
    pub fn refund_background_or_foreground(&mut self, class: IoClass, bytes: u64, ops: u64) {
        self.budget.refund(class, bytes, ops);
    }

    /// Foreground charges from the plane (cold reads, blob writes): metered,
    /// never deferred.
    pub fn charge_foreground(&mut self, class: IoClass, bytes: u64, ops: u64) {
        debug_assert!(class.is_foreground(), "background classes ask, never charge");
        self.budget.charge(class, bytes, ops);
    }

    /// Counters for the MAINTAIN stats flush (S21 vocabulary).
    pub fn stats(&self) -> DurableStats {
        let durable = self.commit.watermark().map_or(0, |l| l.to_u64());
        let queued = self.commit.queued_up_to().map_or(0, |l| l.to_u64());
        let manifest = self.manifest.stats();
        let ckpt = self.ckpt.stats(self.records_appended);
        let log_frame_bytes = self.commit.stats().frame_bytes_queued;
        // ADR-0088 D7: undefined until a checkpoint published (a log-only
        // ratio would read as the figure); ceiling milli-units (ADR-0060
        // D1: a reported figure may overstate, never understate).
        let append_bytes = self.staging.stats().append_bytes;
        let rotor = self.rotor.stats();
        let accounted_host_write_bytes = log_frame_bytes
            .saturating_add(rotor.zero_fill_bytes)
            .saturating_add(ckpt.bytes_total)
            .saturating_add(manifest.bytes_written);
        let milli_of = |written: u64| {
            let milli = (u128::from(written) * 1000).div_ceil(u128::from(append_bytes));
            u64::try_from(milli).unwrap_or(u64::MAX)
        };
        let (write_amp, write_amp_host, undefined) = if ckpt.completed == 0 || append_bytes == 0 {
            (0, 0, 1)
        } else {
            let log_ckpt = log_frame_bytes
                .saturating_add(ckpt.bytes_total)
                .saturating_add(manifest.bytes_written);
            (milli_of(log_ckpt), milli_of(accounted_host_write_bytes), 0)
        };
        let mut io_budget = [ClassCounters::default(); IoClass::COUNT];
        for class in IoClass::ALL {
            io_budget[class.index()] = self.budget.counters(class);
        }
        let (write_share, read_share) = self.budget.share_bytes_per_s();
        DurableStats {
            records_appended: self.records_appended,
            acks_gated: self.acks_gated,
            pending_log_bytes: self.commit.pending_log_bytes(),
            last_durable_lsn: durable,
            watermark_lag_lsn: queued.saturating_sub(durable),
            fsyncs_completed: self.commit.stats().fsyncs_completed,
            frames_queued: self.commit.stats().frames_queued,
            staging_resident_bytes: self.staging.resident_bytes() as u64,
            manifests_published: manifest.published,
            manifests_aborted: manifest.aborted,
            fsyncs_per_sec: self.fsyncs_last_sec,
            acks_per_sec: self.acks_last_sec,
            fsync_p50_us: self.commit.fsync_latency_hist().percentile(50.0),
            fsync_p99_us: self.commit.fsync_latency_hist().percentile(99.0),
            fsync_p999_us: self.commit.fsync_latency_hist().percentile(99.9),
            fsync_group_p50: self.group_hist_records.percentile(50.0),
            fsync_group_p99: self.group_hist_records.percentile(99.0),
            write_stall_p50_us: self.write_stall_hist.percentile(50.0),
            write_stall_p99_us: self.write_stall_hist.percentile(99.0),
            write_stall_p999_us: self.write_stall_hist.percentile(99.9),
            staging_capacity_bytes: u64::from(self.staging.capacity_bytes()),
            admission_parked: self.drained.waiting() as u64,
            admission_parked_total: self.parked_total,
            fsyncs_linked: self.commit.stats().fsyncs_linked,
            fsyncs_seal: self.commit.stats().fsyncs_seal,
            fsyncs_standalone: self.commit.stats().fsyncs_standalone,
            fsyncs_completion: self.commit.stats().fsyncs_completion,
            segments_truncated: manifest.truncated_segments,
            ckpt_in_progress: u64::from(!matches!(self.ckpt.phase, CkptPhase::Idle)),
            log_segments_live: self.rotor.sealed().len() as u64
                + 1
                + u64::from(self.rotor.next_ready().is_some()),
            barrier_class_fua: u64::from(self.rotor.active_write_through()),
            io_class_configured_fua: u64::from(self.rotor.configured_write_through()),
            fsyncs_fua: self.commit.stats().fsyncs_write_through,
            fua_p50_us: self.commit.write_through_latency_hist().percentile(50.0),
            fua_p99_us: self.commit.write_through_latency_hist().percentile(99.0),
            log_padding_bytes: self.staging.stats().padding_bytes,
            zero_fill_bytes: self.rotor.stats().zero_fill_bytes,
            rotations_unzeroed: self.rotor.stats().rotations_unzeroed,
            rotations_upgrade: self.rotor.stats().rotations_upgrade,
            reopened_packed_tails: self.rotor.stats().reopened_packed_tails,
            barrier_class_degraded: u64::from(self.fua_degraded_windows >= 3),
            frames_in_flight: u64::from(self.staging.frames_in_flight()),
            frames_in_flight_max: u64::from(self.staging.stats().in_flight_max),
            frame_waits_barrier: self.frame_waits_barrier,
            frame_waits_rotation: self.frame_waits_rotation,
            frame_waits_reorder: self.frame_waits_reorder,
            frame_waits_fill: self.frame_waits_fill,
            fill_window_us: self.fill.window.as_micros(),
            fill_target_bytes: u64::from(self.fill.target_bytes),
            frame_waits_group: self.frame_waits_group,
            flush_group_window_us: self.group.window.as_micros(),
            frame_records_last: u64::from(self.last_frame_records),
            group_round_target: self.group_round_target,
            io_provenance: self.io_provenance,
            frames_in_flight_now: u64::from(self.staging.in_flight()),
            records_staged: u64::from(self.staging.pending_records()),
            everysec_idle_ticks: self.commit.stats().idle_ticks,
            frames_awaiting_watermark: self.frame_seqs.len() as u64,
            fsync_entries: self.commit.pending_entries() as u64,
            write_through_entries: self.commit.write_through_entries() as u64,
            hold_open: u64::from(self.frame_held),
            fill_hold_open: u64::from(self.fill_since.is_some()),
            group_hold_open: u64::from(self.group_since.is_some()),
            io_budget_model_absent: u64::from(self.budget.model_absent()),
            io_budget_write_bytes_per_s: write_share,
            io_budget_read_bytes_per_s: read_share,
            io_budget,
            frame_waits_pace: self.seal_pace.waits(),
            log_frame_bytes,
            ckpt_bytes_total: ckpt.bytes_total,
            ckpt_bytes_last: ckpt.bytes_last,
            ckpt_padding_bytes: ckpt.padding_bytes,
            manifest_bytes_total: manifest.bytes_written,
            ckpt_interval_bytes: ckpt.interval_bytes,
            ckpt_records_since_begin: ckpt.records_since_begin,
            ckpt_replay_bytes_per_s: self.ckpt.cfg.replay_bytes_per_s,
            ckpt_cap_bytes: self.ckpt.cfg.cap_bytes(),
            ckpt_io_mode_buffered: ckpt.io_mode_buffered,
            ckpt_io_mode_downgrades: ckpt.io_mode_downgrades,
            ckpt_bound_splits: ckpt.bound_splits,
            write_amp_milli_log_checkpoint: write_amp,
            write_amp_log_checkpoint_undefined: undefined,
            accounted_host_write_bytes,
            write_amp_milli_accounted_host: write_amp_host,
            segments_recycled: rotor.segments_recycled,
            recycle_misses: rotor.recycle_misses,
            recycle_fallbacks: rotor.recycle_fallbacks,
            recycle_pool_bytes: self.rotor.recycle_pool_bytes(),
            recycle_pool_full: rotor.recycle_pool_full,
            recycle_sentinels: rotor.recycle_sentinels,
            segment_rotations: rotor.rotations,
            segment_preallocs: rotor.preallocs + rotor.inline_preallocs,
            segment_inline_preallocs: rotor.inline_preallocs,
            segment_prealloc_failures: rotor.prealloc_failures,
            recycle_waits_started: rotor.recycle_waits_started,
            recycle_waits_satisfied: rotor.recycle_waits_satisfied,
            recycle_waits_expired: rotor.recycle_waits_expired,
            recycle_wait_active_bytes_max: rotor.recycle_wait_active_bytes_max,
            write_stall_max_us: self.write_stall_hist.max(),
        }
    }

    /// Manifest/truncation gauges (tests, INFO).
    pub fn manifest_stats(&self) -> ManifestStats {
        self.manifest.stats()
    }

    /// Admission gate for durable writes when preallocation failed
    /// (ENOSPC — degrade loudly, never corrupt).
    pub fn space_exhausted(&self) -> bool {
        self.rotor.space_exhausted()
    }
}

// ---- ticket ↔ token packing (plane-side detail; inf-log never sees tokens) --

pub(crate) fn fsync_token(ticket: FsyncTicket) -> CompletionToken {
    let raw = ticket.as_u64();
    assert!(raw < 1 << 56, "ticket fits slot+gen");
    CompletionToken::new(TokenClass::Fsync, (raw & 0xFF_FFFF) as u32, (raw >> 24) as u32)
}

fn token_ticket(token: CompletionToken) -> FsyncTicket {
    FsyncTicket::from_u64(u64::from(token.slot()) | (u64::from(token.generation()) << 24))
}

fn write_token(seq: u64) -> CompletionToken {
    CompletionToken::new(TokenClass::LogWrite, (seq & 0xFF_FFFF) as u32, (seq >> 24) as u32)
}

/// Inverse of [`write_token`]: the frame's write sequence (== `FrameId`).
fn write_seq_of(token: CompletionToken) -> u64 {
    u64::from(token.slot()) | (u64::from(token.generation()) << 24)
}

/// The LSN→seq ack map's coalescing rule (F-L01-03, batch 42). The gate
/// consumes only the *last* entry a watermark covers, so an entry `a`
/// is redundant once its successor `b` is covered by every future
/// watermark that covers `a`. That holds for two written frames with no
/// ledger coverage point in `a.end..b.end`: every barrier registered
/// after `b` landed covers `b` — a linked or write-through sync covers
/// `queued_up_to ≥ b`, a standalone or completion sync covers
/// `written_up_to ≥ b`, a rotation's seal covers the old segment's end
/// (`≥ b` for a written `b` queued before it), and the ledger-only
/// barriers reuse the coverage tail — and the ledger's coverage is
/// monotone in submission order. A coverage point between them (the
/// linked sync on `a` with plain `b` behind it) keeps `a`: `a`'s acks
/// must not wait for `b`'s barrier. An unwritten `b` is never merged
/// into — a standalone may still cover exactly `a`.
fn coalesce_ack_map(
    map: &mut VecDeque<(Lsn, u64)>,
    written: Lsn,
    covers_within: impl Fn(Lsn, Lsn) -> bool,
) {
    let mut i = 0;
    while i + 1 < map.len() {
        let (a, _) = map[i];
        let (b, _) = map[i + 1];
        if b > written {
            break;
        }
        if covers_within(a, b) {
            i += 1;
        } else {
            map.remove(i);
        }
    }
}

#[cfg(test)]
mod ack_map_tests {
    use std::collections::VecDeque;

    use inf_log::{Lsn, SegmentId};

    use super::coalesce_ack_map;

    fn lsn(off: u32) -> Lsn {
        Lsn::new(SegmentId(1), off)
    }

    fn map(ends: &[u32]) -> VecDeque<(Lsn, u64)> {
        ends.iter().map(|&e| (lsn(e), u64::from(e))).collect()
    }

    /// Plain frames landing between two barriers collapse to the last
    /// written one: the gate reads the same seq at the next watermark.
    #[test]
    fn written_plain_frames_without_a_coverage_point_collapse_to_one() {
        let mut m = map(&[64, 128, 192, 256]);
        coalesce_ack_map(&mut m, lsn(256), |_, _| false);
        assert_eq!(m, map(&[256]));
    }

    /// A coverage point inside `a..b` (the linked sync on `a`, plain
    /// `b` behind it) keeps `a`: its acks release at `a`'s barrier.
    #[test]
    fn a_coverage_point_between_two_written_frames_keeps_the_earlier() {
        let mut m = map(&[64, 128, 192]);
        coalesce_ack_map(&mut m, lsn(192), |lo, hi| lo <= lsn(64) && lsn(64) < hi);
        assert_eq!(m, map(&[64, 192]));
    }

    /// Entries behind the written prefix never merge — a standalone may
    /// still cover exactly the last written end.
    #[test]
    fn unwritten_frames_are_never_merged_into() {
        let mut m = map(&[64, 128, 192]);
        coalesce_ack_map(&mut m, lsn(64), |_, _| false);
        assert_eq!(m, map(&[64, 128, 192]));
        coalesce_ack_map(&mut m, lsn(128), |_, _| false);
        assert_eq!(m, map(&[128, 192]));
    }
}

#[cfg(test)]
mod fill_tests {
    use super::*;
    use inf_log::FrameLayout;

    /// ADR-0089 D1: a barrier-less frame below the target holds inside
    /// the window and seals at the target, at the window, off the
    /// policy, and on a packed segment.
    #[test]
    fn barrier_less_frame_holds_until_target_or_window() {
        let fill = FillConfig::DESIGN_POINT;
        let t0 = Nanos::from_micros(10);
        let d = |frame_len: u32, since: Option<Nanos>, now: Nanos| {
            fill.decide(FramePlan::Plain, FrameLayout::Aligned, frame_len, since, now)
        };
        assert_eq!(d(2_400, None, t0), FillDecision::Hold, "first sight: the hold starts");
        assert_eq!(d(2_400, Some(t0), t0 + Nanos::from_micros(999)), FillDecision::Hold);
        assert_eq!(d(2_400, Some(t0), t0 + Nanos::from_micros(1_000)), FillDecision::Seal);
        assert_eq!(d(16 << 10, None, t0), FillDecision::Seal, "at the target");
        assert_eq!(d((16 << 10) - 100, None, t0), FillDecision::Seal, "padded to the target");
        assert_eq!(d((8 << 10) + 1, None, t0), FillDecision::Hold, "padded to 12 KiB: below");
        assert_eq!(d((12 << 10) + 1, None, t0), FillDecision::Seal, "padded to 16 KiB: at");
        assert_eq!(
            FillConfig::default().decide(FramePlan::Plain, FrameLayout::Aligned, 100, None, t0),
            FillDecision::Seal,
            "policy off"
        );
        assert_eq!(
            fill.decide(FramePlan::Plain, FrameLayout::Packed, 100, None, t0),
            FillDecision::Seal,
            "no padding on a packed segment — nothing to fill against"
        );
    }

    /// A frame whose plan carries a barrier is never held (ADR-0089 D1;
    /// the arm that held them was measured and `Rejected`), and a
    /// waiting frame is the plan's hold, never the policy's.
    #[test]
    fn barrier_carrying_and_waiting_frames_are_never_held() {
        let t0 = Nanos::from_micros(10);
        for plan in [FramePlan::WriteThrough, FramePlan::LinkedFsync, FramePlan::Wait] {
            assert_eq!(
                FillConfig::DESIGN_POINT.decide(plan, FrameLayout::Aligned, 100, None, t0),
                FillDecision::Seal,
                "{plan:?}"
            );
        }
    }
}

/// M4.5-S43 (ADR-0092 D3): the group hold's pure decision, pinned.
#[cfg(test)]
mod group_hold_tests {
    use super::*;
    use inf_log::FrameLayout;

    const ARM: GroupHoldConfig = GroupHoldConfig::ARM;
    const T0: Nanos = Nanos(1_000_000);

    /// Off, a non-barrier plan, a write-through plan, and an aligned
    /// layout all seal: the FUA class and the fill policy's territory
    /// are never touched.
    #[test]
    fn only_a_flush_class_barrier_on_a_packed_segment_can_hold() {
        let off = GroupHoldConfig::default();
        assert!(!off.enabled());
        assert_eq!(
            off.decide(FramePlan::LinkedFsync, FrameLayout::Packed, 1, 8, None, T0),
            GroupDecision::Seal
        );
        for plan in [FramePlan::Plain, FramePlan::WriteThrough] {
            assert_eq!(
                ARM.decide(plan, FrameLayout::Packed, 1, 8, None, T0),
                GroupDecision::Seal,
                "{plan:?}"
            );
        }
        assert_eq!(
            ARM.decide(FramePlan::LinkedFsync, FrameLayout::Aligned, 1, 8, None, T0),
            GroupDecision::Seal
        );
    }

    /// A trickle (a round of one) never holds; a round that has
    /// re-arrived seals at once; a round still arriving holds from its
    /// first sight until the window elapses. The steady K = 1 FLUSH
    /// cadence is the case that matters: a barrier acked 4 of a
    /// population of 8 and the other 4 are uncovered (written as plain
    /// frames while the slot was busy) — the target is the population
    /// the completion measured, never the last frame's size (campaign
    /// A) and never what `LogWritten` sees (campaign B).
    #[test]
    fn holds_only_while_a_group_is_still_arriving_and_inside_the_window() {
        // Uncovered 4 against a population of 8 ⇒ hold; a target equal to
        // the uncovered count (the flaws) ⇒ seal — pinned so neither can
        // return.
        assert_eq!(
            ARM.decide(FramePlan::LinkedFsync, FrameLayout::Packed, 4, 8, None, T0),
            GroupDecision::Hold
        );
        assert_eq!(
            ARM.decide(FramePlan::LinkedFsync, FrameLayout::Packed, 4, 4, None, T0),
            GroupDecision::Seal
        );
        let plan = FramePlan::LinkedFsync;
        let packed = FrameLayout::Packed;
        assert_eq!(ARM.decide(plan, packed, 0, 1, None, T0), GroupDecision::Seal);
        assert_eq!(ARM.decide(plan, packed, 0, 0, None, T0), GroupDecision::Seal);
        assert_eq!(ARM.decide(plan, packed, 8, 8, None, T0), GroupDecision::Seal);
        assert_eq!(ARM.decide(plan, packed, 9, 8, None, T0), GroupDecision::Seal);
        // The episode's first sight holds; inside the window holds; at
        // the deadline seals whatever arrived.
        assert_eq!(ARM.decide(plan, packed, 3, 8, None, T0), GroupDecision::Hold);
        let since = Some(T0);
        assert_eq!(
            ARM.decide(plan, packed, 3, 8, since, T0 + Nanos::from_micros(249)),
            GroupDecision::Hold
        );
        assert_eq!(
            ARM.decide(plan, packed, 3, 8, since, T0 + Nanos::from_micros(250)),
            GroupDecision::Seal
        );
        // The round arriving mid-window seals before the deadline.
        assert_eq!(
            ARM.decide(plan, packed, 8, 8, since, T0 + Nanos::from_micros(100)),
            GroupDecision::Seal
        );
    }
}
