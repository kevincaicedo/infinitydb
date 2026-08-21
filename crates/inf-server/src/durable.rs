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
    FrameId, FramePlan, FsyncClass, FsyncTicket, GroupCommit, IdxSidecarMeta, MutationEffect, NsId,
    RecordView, SegmentConfig, SegmentRotor, StagingConfig, StagingRing, ZERO_FILL_SLICE_BYTES,
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
use crate::log_bytes;

/// Timer-wheel key for the everysec tick (plane-armed, injected clock).
pub(crate) const EVERYSEC_TIMER_KEY: u64 = 0xE5EC_0001;

/// POSIX `EIO` (this crate carries no libc dep): the errno the
/// `durable_fsync_eio` fault point injects (M2-S17).
const EIO: i32 = 5;

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
}

impl Default for RecoverConfig {
    fn default() -> RecoverConfig {
        // 8 MiB ≈ single-digit-ms steps at the ≥ 1 GB/s replay gate: the
        // loop keeps answering -LOADING while paying < 0.1% step overhead.
        RecoverConfig { step_bytes: 8 << 20, throttle_bytes_per_sec: None }
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
    /// Instantaneous log quiescence gauges: frames sealed and awaiting
    /// `LogWritten`, and records staged but not yet sealed. Both zero ⇒
    /// every executed durable effect has reached the file — the DST's
    /// shadow-replay oracle waits on exactly this (ADR-0087 D7).
    pub frames_in_flight_now: u64,
    pub records_staged: u64,
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
    /// `ceil_milli((log_frame_bytes + ckpt_bytes_total +
    /// manifest_bytes_total) / append_bytes)` — cell scope, boot life;
    /// undefined (0, with `_undefined = 1`) until the first checkpoint
    /// publishes so a log-only ratio is never read as the figure.
    /// Zero-fill is excluded and reported beside (`zero_fill_bytes`).
    pub write_amp_milli_log_checkpoint: u64,
    pub write_amp_log_checkpoint_undefined: u64,
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
    /// The cell's zero window (ADR-0086 D4): 1 MiB of zeros, 4 KiB-aligned,
    /// never written — the source of every zero-fill `LogWrite`.
    /// Attributed to the log-staging domain.
    zero_window: AlignedBox,
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
    frame_held: bool,
    /// M4.5-S36 (ADR-0088 D2): the cell's device budget — refilled at
    /// every MAINTAIN entry from the injected clock, consulted by the
    /// background issuing sites, charged by the foreground ones.
    budget: DeviceBudget,
    /// ADR-0088 D2b: the frame-seal pacer (a due frame behind in-flight
    /// frames seals only when the device's barrier rate allows).
    seal_pace: SealPace,
    /// Last durable seq assigned (0 = none; the gate starts at 0).
    last_seq: u64,
    /// Last seq the ack gate advanced to (group-formation bookkeeping).
    acked_seq: u64,
    /// Records newly covered per durability-fsync completion (M2.5-S07):
    /// the group-formation distribution the ≥ 0.8× gate reads.
    group_hist_records: inf_foundation::LogHistogram,
    /// Frames queued but not yet durable: (exclusive-end LSN, last seq).
    frame_seqs: VecDeque<(u64, u64)>,
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
        staging: StagingConfig,
        flush_bound: u8,
        fua_p50_us_probed: u64,
        device: DeviceConfig,
        rotor: SegmentRotor<F>,
        ckpt: CkptCell<F>,
        manifest: ManifestCell<F>,
    ) -> DurableCell<F> {
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
            zero_fill_ticket: None,
            fua_p50_us_probed,
            fua_degraded_windows: 0,
            fua_tick_count_prev: 0,
            fua_tick_window_sum_us: 0,
            write_stall_hist: inf_foundation::LogHistogram::new(),
            parked_total: 0,
            frame_waits_barrier: 0,
            frame_waits_rotation: 0,
            frame_held: false,
            budget,
            seal_pace,
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
    pub fn maintain(&mut self, cx: &mut LoopCx<'_>) {
        if self.failed {
            return;
        }
        // ADR-0088 D2: one refill per MAINTAIN entry, injected clock.
        self.budget.refill(cx.now);
        match self.rotor.maintain_deferred(cx.now.as_millis()) {
            Ok((_report, Some(barrier))) => {
                // Fd-less tiers (MemFs) have process-KILL physics — no
                // barrier needed (completed writes survive by construction).
                if let Some(fd) = inf_log::fs::SegmentFile::raw_fd(&barrier.dir) {
                    let ticket = self.commit.register_prealloc_barrier(barrier.dir, cx.now);
                    self.budget.charge(IoClass::LogFrame, 0, 1); // ledger-class barrier (ADR-0088 D7)
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
        if self.rotor.zero_fill_pending()
            && self.budget.admit(IoClass::ZeroFill, bound, 1) == Admission::Granted
        {
            let Some(slice) = self.rotor.next_zero_slice(ZERO_FILL_SLICE_BYTES) else {
                self.budget.refund(IoClass::ZeroFill, bound, 1);
                return;
            };
            self.budget.refund(IoClass::ZeroFill, bound - u64::from(slice.len), 0);
            cx.push(IoOp::LogWrite {
                fd: slice.fd,
                offset: slice.offset,
                data: log_bytes::zero_window(&self.zero_window, slice.len),
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
    /// a rotation while frames are in flight, or when a sync is due that
    /// neither write-through nor a linked fdatasync may carry yet.
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
                self.frame_waits_barrier += u64::from(!self.frame_held);
                self.frame_held = true;
                return;
            }
            self.frame_held = false;
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
            let ticket = self.commit.register_standalone_fsync(cx.now);
            let fd = self.rotor.active_raw_fd().expect("std segment tier has fds");
            self.budget.charge(IoClass::LogFrame, 0, 1); // ledger-class barrier (ADR-0088 D7)
            cx.push(IoOp::Fdatasync { fd, token: fsync_token(ticket) });
        }
    }

    /// Seal the pending records into `slot`, register the planned barrier,
    /// and hand the frame to the driver. The frame's on-device extent
    /// (padding included on aligned segments) is what the cursor, the
    /// ledger, and the barrier all advance by (ADR-0086 D3).
    fn queue_frame(&mut self, cx: &mut LoopCx<'_>, slot: inf_log::FrameSlot, plan: FramePlan) {
        let end = slot.base().advance(slot.len());
        let covered = self.commit.watermark().map_or(0, |lsn| lsn.to_u64());
        let lease = self.staging.seal(slot.first_record_lsn(), covered, slot.layout());
        debug_assert_eq!(lease.frame_len(), slot.len(), "sealed bytes match the reservation");
        // A pending ckpt-begin marker rides this frame: its LSN is now
        // real (ADR-0016 D3).
        self.ckpt.on_frame_sealed(&lease);
        self.frame_seqs.push_back((end.to_u64(), self.last_seq));
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
        self.staging.release(frame.lease);
        self.drained.wake_all(());
        match frame.barrier {
            FrameBarrier::None => {}
            FrameBarrier::Linked(ticket) => self.commit.rebase_clock(ticket, now),
            FrameBarrier::WriteThrough(ticket) => self.on_synced(cx, fsync_token(ticket)),
        }
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
            let watermark = end.to_u64();
            let mut last_covered = None;
            while self.frame_seqs.front().is_some_and(|&(end_lsn, _)| end_lsn <= watermark) {
                last_covered = self.frame_seqs.pop_front().map(|(_, seq)| seq);
            }
            if let Some(seq) = last_covered {
                // Group formation (M2.5-S07): records newly covered by
                // this completion — the distribution behind the
                // ≥ 0.8× available-in-flight-writes gate.
                debug_assert!(seq >= self.acked_seq, "ack seq regressed — frame_seqs FIFO broken");
                self.group_hist_records.record(seq - self.acked_seq);
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
            let _ = self.commit.on_fsync_error(token_ticket(token));
        }
        self.fail_stop("I/O", &format!("errno {errno} on {:?}", token.class()))
    }

    /// One fuzzy-checkpoint slice (M2-S10, ADR-0016 D5): runs under the
    /// `GroupClass::Checkpoint` deficit — `budget_units` convert at
    /// 1 unit ≈ 1 KiB streamed, hard-capped by `CkptConfig::slice_bytes`.
    /// Returns the units to charge. All data writes ride the driver; the
    /// only blocking ops are file create/rename/dir-fsync metadata (the
    /// rotor-prealloc class).
    pub fn ckpt_slice(
        &mut self,
        ks: &mut Keyspace,
        tier: Option<&mut crate::tier_cell::TierCell<F>>,
        cx: &mut LoopCx<'_>,
        budget_units: u32,
        anchor: WallAnchor,
    ) -> u32 {
        let mut tier = tier;
        if self.failed {
            return 0;
        }
        // Idle: trigger check → stage the begin marker (one record; its
        // frame seals at this iteration's LOG step). A pending MANIFEST
        // blocks the next trigger: one recovery-unit transition in flight,
        // ever (M2-S11 — the pending swap resolves within one everysec
        // window, so this never starves the trigger).
        if matches!(self.ckpt.phase, CkptPhase::Idle) {
            // ADR-0088 D4: the accumulator is on-disk frame bytes (header,
            // trailer, v3 padding — what the device saw); the record cap
            // rides beside it; before the first checkpoint the interval is
            // the floor alone (the published file's size is the only
            // measurement — a content estimate chased a growing dataset
            // and never fired, the node_e2e threshold test's finding).
            let total = self.commit.stats().frame_bytes_queued;
            if self.manifest.idle() && self.ckpt.should_begin(total, self.records_appended) {
                let id = self.ckpt.pending_id();
                let effect = MutationEffect::CkptBegin { ckpt_id: id };
                if self.staging.would_fit(effect.encoded_len()) {
                    let at = self.staging.stage(&effect).expect("admission pre-checked");
                    self.commit.note_staged(FsyncClass::Everysec);
                    self.records_appended += 1;
                    self.last_seq += 1;
                    self.ckpt.requested = false;
                    // The epoch this checkpoint satisfies (M2-S20) — one
                    // transition in flight, so a single value suffices.
                    self.ckpt.epoch_in_flight = self.ckpt.req_epoch;
                    // Trigger re-base is begin-anchored (`bytes_at_last`
                    // docs): everything staged after this instant is tail
                    // the new checkpoint does not cover.
                    self.ckpt.bytes_at_begin = total;
                    self.ckpt.records_at_begin = self.records_appended;
                    self.ckpt.phase = CkptPhase::AwaitBeginLsn { id, at };
                }
            }
            return 0;
        }
        // Begin LSN resolves at LOG (`on_frame_sealed`); nothing to do yet.
        if matches!(self.ckpt.phase, CkptPhase::AwaitBeginLsn { .. }) {
            return 0;
        }
        // Begun: create the .ick.new file + queue the header write.
        if let CkptPhase::Begun { id, begin_lsn } = self.ckpt.phase {
            let ns_ids = ks.durable_ns_ids();
            // v2 iff tiered namespaces or index declarations on durable
            // namespaces exist (ADR-0073 D2 / ADR-0078 D7) — cells with
            // neither keep writing v1 byte-identically.
            let v2 =
                ns_ids.iter().any(|&raw| ks.is_tiered(NsId(raw))) || ks.idx_declared_on_durable();
            if let Err(err) = self.ckpt.open_stream(id, begin_lsn, ns_ids, v2, cx.now) {
                self.ckpt.abort("create", &err.to_string());
                return 1;
            }
            let CkptPhase::Stream(st) = &mut self.ckpt.phase else { unreachable!("just opened") };
            let lease = st.in_flight.as_ref().expect("header staged by open_stream");
            // The header is one block, charged unconditionally (the file
            // is already created); the class deficit absorbs it and the
            // first section offer pays for it (ADR-0088 D2).
            self.budget.charge(IoClass::Checkpoint, u64::from(lease.len()), 1);
            st.write_seq += 1;
            cx.push(IoOp::LogWrite {
                fd: st.fd,
                offset: lease.offset(),
                data: log_bytes::ckpt_block(&st.stream, lease),
                token: ckpt_token(TokenClass::CkptWrite, st.write_seq),
                barrier: WriteBarrier::None,
            });
            return 1;
        }
        let cfg = self.ckpt.cfg;
        // Publish once the completion fdatasync landed (rename+dir-fsync).
        // A published `.ick` hands off to the MANIFEST driver: the swap
        // waits for the watermark to cover begin (M2-S11 publication
        // guard), then runs in `manifest_slice`.
        if matches!(&self.ckpt.phase, CkptPhase::Stream(st) if st.sync_done) {
            let CkptPhase::Stream(st) = std::mem::replace(&mut self.ckpt.phase, CkptPhase::Idle)
            else {
                unreachable!("matched above")
            };
            let (id, begin_lsn) = (st.id, st.begin_lsn);
            let unix_now = anchor.unix_from_internal(cx.now);
            match self.ckpt.publish(st, unix_now) {
                Ok(()) => {
                    self.manifest.note_published_ick(id, begin_lsn, self.ckpt.epoch_in_flight);
                }
                Err(err) => self.ckpt.abort("publish", &err.to_string()),
            }
            return 1;
        }
        let CkptPhase::Stream(st) = &mut self.ckpt.phase else { return 0 };
        // One deref, split fields (borrow splitting doesn't cross `Box`).
        let crate::ckpt::Streaming {
            id,
            stream,
            fd,
            ns_ids,
            ns_idx,
            cursor,
            tier_pass,
            walk_done,
            v2,
            sidecar_plan,
            sidecar_at,
            sidecar_cursor,
            sidecar_emitted,
            sidecar_done,
            footer_staged,
            sync_issued,
            in_flight,
            write_seq,
            opened_at,
            streamed_bytes,
            ..
        } = &mut **st;
        // One section in flight max: wait for its completion.
        if in_flight.is_some() {
            return 0;
        }
        // Pacing (ADR-0017): the walk streams at most `stream_bytes_per_sec
        // × elapsed` — an unpaced walk dirties pages at memcpy speed and
        // the kernel's writeback throttling then stalls the log write's
        // CQE path (the S12-measured foreground cliff). Injected time, so
        // DST compresses it (L7). Sidecar emission is walk output like
        // any other (M4.5-S06) and rides the same meter.
        // ADR-0088 D5: with a device model present the budget governs the
        // rate (the pace's reason — writeback throttling — is gone with
        // the direct `.ick`); the pace is the unprobed fallback only.
        if self.budget.model_absent()
            && !(*walk_done && *sidecar_done)
            && cfg.stream_bytes_per_sec > 0
        {
            let elapsed_ms = cx.now.saturating_sub(*opened_at).as_millis();
            let allowed = (u64::from(cfg.stream_bytes_per_sec) * elapsed_ms.max(1)) / 1000;
            if *streamed_bytes >= allowed {
                return 0;
            }
        }
        // Footer written+released → the completion barrier.
        if *footer_staged {
            if !*sync_issued {
                *sync_issued = true;
                *write_seq += 1;
                self.budget.charge(IoClass::Checkpoint, 0, 1);
                cx.push(IoOp::Fdatasync {
                    fd: *fd,
                    token: ckpt_token(TokenClass::CkptSync, *write_seq),
                });
            }
            return 0;
        }
        // Fill: pull post-images under the byte budget (the walker — the
        // resize-stable SCAN cursor, ADR-0016 D2).
        let mut emitted: u32 = 0;
        let mut force_seal = false;
        let slice_cap = cfg.slice_bytes.min(budget_units.saturating_mul(1024)).max(1);
        if !*walk_done {
            while emitted < slice_cap && !*walk_done && !stream.section_full() && !force_seal {
                let Some(&ns_raw) = ns_ids.get(*ns_idx) else {
                    *walk_done = true;
                    break;
                };
                let ns = NsId(ns_raw);
                // Tiered namespaces walk the ADR-0057 hybrid (M4-S26):
                // refs, images, live-set + blob sections, retirement scan.
                if ks.is_tiered(ns) {
                    let step = match (ks.tiered_store_mut(ns), tier.as_deref_mut()) {
                        (Some(table), Some(tc)) => match tc.ns_mut(ns) {
                            Some(t) => tier_walk_step(
                                stream,
                                table,
                                t,
                                ns,
                                *id,
                                cursor,
                                tier_pass,
                                slice_cap,
                                &mut emitted,
                            ),
                            None => TierStep::NsDone,
                        },
                        // Dropped mid-walk (or a plane without tier state
                        // — unreachable when tiered namespaces exist).
                        _ => TierStep::NsDone,
                    };
                    match step {
                        TierStep::Progress => {}
                        TierStep::SealFirst => force_seal = true,
                        TierStep::NsDone => {
                            *ns_idx += 1;
                            *cursor = 0;
                            *tier_pass = 0;
                            if *ns_idx == ns_ids.len() {
                                *walk_done = true;
                            }
                        }
                    }
                    continue;
                }
                match ks.ns_store_mut(ns) {
                    // Dropped mid-walk: its records replay as skips anyway.
                    None => {
                        *ns_idx += 1;
                        *cursor = 0;
                    }
                    Some(store) => {
                        let bytes = &mut emitted;
                        let next = store.scan_checkpoint_images(
                            *cursor,
                            SCAN_CHUNK_ENTRIES,
                            cx.now,
                            |key, image, expire_ms| {
                                let rec = match image {
                                    CheckpointImage::String(value) => {
                                        RecordView::StringPostImage { ns, key, value }
                                    }
                                    #[cfg(feature = "doc")]
                                    CheckpointImage::JsonDoc { lineage, version, idoc } => {
                                        RecordView::DocFull { ns, key, lineage, version, idoc }
                                    }
                                };
                                *bytes = bytes.saturating_add(rec.encoded_len() as u32);
                                stream.stage_record(&rec);
                                if let Some(ms) = expire_ms {
                                    let at_unix_ms =
                                        anchor.unix_from_internal(Nanos::from_millis(ms));
                                    let rec = RecordView::ExpireAt { ns, at_unix_ms, key };
                                    *bytes = bytes.saturating_add(rec.encoded_len() as u32);
                                    stream.stage_record(&rec);
                                }
                            },
                        );
                        if next == 0 {
                            *ns_idx += 1;
                            *cursor = 0;
                        } else {
                            *cursor = next;
                        }
                        if *ns_idx == ns_ids.len() {
                            *walk_done = true;
                        }
                    }
                }
            }
        } else if !*sidecar_done {
            // Sidecar phase (M4.5-S06, ADR-0078 D1): derived data last —
            // converged trees stream after every record image. A v1
            // stream cannot represent them (an index converging mid-walk
            // waits for the next checkpoint, which opens v2).
            if *v2 {
                force_seal = sidecar_walk_step(
                    stream,
                    ks,
                    ns_ids,
                    sidecar_plan,
                    sidecar_at,
                    sidecar_cursor,
                    sidecar_emitted,
                    sidecar_done,
                    slice_cap,
                    &mut emitted,
                );
            } else {
                *sidecar_done = true;
            }
        }
        // Queue at most one block per slice: a full (or final partial)
        // section, a class-boundary seal (M4-S26 tiered passes / S06
        // index boundaries), or — once everything drained — the footer.
        // ADR-0088 D2/D3: each block is offered to the budget at its
        // padded length *before* it seals — `Deferred` leaves the section
        // staged (the walk simply does not advance past it this tick)
        // and the offer repeats next slice.
        if stream.section_full()
            || (force_seal && stream.can_seal())
            || (*walk_done && *sidecar_done && stream.can_seal())
        {
            let block = stream.pending_block_len() as u64;
            if self.budget.admit(IoClass::Checkpoint, block, 1) != Admission::Granted {
                *streamed_bytes += u64::from(emitted);
                return emitted.div_ceil(1024).max(1);
            }
            let lease = stream.seal_section();
            *write_seq += 1;
            cx.push(IoOp::LogWrite {
                fd: *fd,
                offset: lease.offset(),
                data: log_bytes::ckpt_block(stream, &lease),
                token: ckpt_token(TokenClass::CkptWrite, *write_seq),
                barrier: WriteBarrier::None,
            });
            *in_flight = Some(lease);
        } else if *walk_done && *sidecar_done {
            let block = stream.footer_block_len() as u64;
            if self.budget.admit(IoClass::Checkpoint, block, 1) != Admission::Granted {
                return 1;
            }
            let lease = stream.finish();
            *write_seq += 1;
            cx.push(IoOp::LogWrite {
                fd: *fd,
                offset: lease.offset(),
                data: log_bytes::ckpt_block(stream, &lease),
                token: ckpt_token(TokenClass::CkptWrite, *write_seq),
                barrier: WriteBarrier::None,
            });
            *in_flight = Some(lease);
            *footer_staged = true;
        }
        *streamed_bytes += u64::from(emitted);
        emitted.div_ceil(1024).max(1)
    }

    /// REAP: a checkpoint section/header/footer write completed — release
    /// the lease (the `StableBytes` custody point).
    pub fn on_ckpt_written(&mut self) {
        let CkptPhase::Stream(st) = &mut self.ckpt.phase else {
            panic!("CkptWrite completion with no checkpoint streaming")
        };
        let lease = st.in_flight.take().expect("CkptWrite with no in-flight lease");
        st.stream.release(lease);
    }

    /// REAP: the checkpoint's completion fdatasync landed — publish next
    /// MAINTAIN slice.
    pub fn on_ckpt_synced(&mut self) {
        let CkptPhase::Stream(st) = &mut self.ckpt.phase else {
            panic!("CkptSync completion with no checkpoint streaming")
        };
        st.sync_done = true;
    }

    /// REAP: a checkpoint op failed — abort the checkpoint, never the
    /// process (the old checkpoint and the whole log stay valid; the
    /// milestone's "checkpoints abort cleanly" rule).
    pub fn on_ckpt_error(&mut self, errno: i32) {
        self.ckpt.abort("I/O", &format!("errno {errno}"));
    }

    /// The MANIFEST + truncation slice (M2-S11/S12, ADR-0017), from
    /// MAINTAIN — never the hot path, **never a device barrier on the
    /// loop**, and **never a large unlink on the loop** (freeing a
    /// truncated file's pages is O(size) in the kernel — a measured
    /// multi-ms stall):
    ///
    /// 1. **Swap machine**: `.ick` dir-fsync → watermark guard →
    ///    `MANIFEST.new` stage+fdatasync → rename → shard dir-fsync →
    ///    commit (floor advance). Barriers ride the driver
    ///    (`TokenClass::ManifestSync`); one in flight, ever.
    /// 2. **Truncate**: forget sealed segments below the floor (fully
    ///    covered by the named checkpoint; the M5 retention hook can exempt
    ///    topic segments) and delegate their unlinks to the control thread.
    ///    Budget adapts to the covered backlog (M2.5-S11, ADR-0022 D8.4):
    ///    ≥ [`MAX_UNLINKS_PER_SLICE`], ≤ [`MAX_TRUNC_PER_SLICE_ADAPTIVE`].
    /// 3. **GC**: delegate ≤ [`MAX_UNLINKS_PER_SLICE`] stale-`.ick`/orphan
    ///    unlinks queued at commit.
    ///
    /// `control = None` (planeless/test tiers) falls back to inline
    /// unlinks — those tiers have no foreground tail to protect.
    /// Returns Maintenance units to charge (≈ one per file op).
    pub fn manifest_slice(
        &mut self,
        cx: &mut LoopCx<'_>,
        control: Option<&crate::ControlHandle>,
        unix_now_ms: u64,
        ks: &mut Keyspace,
        tier: Option<&mut crate::tier_cell::TierCell<F>>,
    ) -> u32 {
        if self.failed {
            return 0;
        }
        let watermark = self.commit.watermark().map(|l| l.to_u64());
        let before = self.manifest.stats();
        let mut units =
            self.manifest.swap_slice(cx, watermark, self.rotor.active_segment(), ks, tier);
        // ADR-0088 D5/D7: the swap's barriers and the MANIFEST envelope are
        // metered under `Checkpoint`, never deferred (a checkpoint that
        // wrote its bytes must publish).
        // The envelope bytes ride a synchronous `SegmentFile::write_at`
        // (not the driver): they are disclosed as `manifest_bytes_total`
        // and enter the write-amplification figure, but not the class
        // ledger, whose identity is "what the driver saw".
        let after = self.manifest.stats();
        let syncs = after.syncs_issued.saturating_sub(before.syncs_issued);
        if syncs > 0 {
            self.budget.charge(IoClass::Checkpoint, 0, syncs);
        }
        // A MANIFEST just committed (dir-fsync durable): publish the
        // control-board slot — the `INF.CKPT WAIT`/`LASTSAVE` observable
        // (M2-S20, ADR-0021 D6).
        if let Some((epoch, ckpt_id)) = self.manifest.take_published()
            && let Some(control) = control
        {
            control.ckpt_board().slot(self.manifest.cell()).publish(epoch, ckpt_id, unix_now_ms);
        }
        if let Some(floor) = self.manifest.floor() {
            // Adaptive drain (M2.5-S11, ADR-0022 D8.4): the budget follows
            // the covered backlog — half of it per slice, floored at the
            // fixed cap, ceilinged at MAX_TRUNC_PER_SLICE_ADAPTIVE — so a
            // fast writer cannot grow retained log unboundedly while the
            // drain plods at 2/slice. Still a bounded slice, never a burst.
            let backlog = self
                .rotor
                .sealed_below(floor)
                .iter()
                .filter(|&&id| !self.manifest.truncation_exempt(id))
                .count();
            let budget =
                MAX_UNLINKS_PER_SLICE.max(backlog.div_ceil(2)).min(MAX_TRUNC_PER_SLICE_ADAPTIVE);
            for _ in 0..budget {
                let Some(&id) = self
                    .rotor
                    .sealed_below(floor)
                    .iter()
                    .find(|&&id| !self.manifest.truncation_exempt(id))
                else {
                    break;
                };
                // Forget-then-unlink: the rotor drops the segment first so
                // a failed/late unlink can never resurrect it in the live
                // set (boot GC re-collects survivors below the floor).
                let path = self.rotor.forget_sealed(id);
                match control {
                    Some(control) => {
                        if !control.request_unlink(path.clone()) {
                            // Queue full: the path joins the GC queue and
                            // retries next slice (bounded, never a stall).
                            self.manifest.defer_unlink(path);
                        }
                    }
                    None => self.manifest.unlink_now(&path),
                }
                self.manifest.note_truncated(1);
                units += 1;
            }
        }
        units + self.manifest.gc_slice(MAX_UNLINKS_PER_SLICE, control)
    }

    /// REAP: a MANIFEST-swap barrier (`TokenClass::ManifestSync`) landed —
    /// phase flip only; follow-up metadata ops run next MAINTAIN slice.
    pub fn on_manifest_synced(&mut self) {
        self.manifest.on_synced();
    }

    /// REAP: a MANIFEST-swap barrier failed — the old recovery unit stays
    /// authoritative (the checkpoint-abort class, ADR-0017).
    pub fn on_manifest_error(&mut self, errno: i32) {
        self.manifest.on_sync_error(errno);
    }

    /// Manual trigger latch (`INF.CKPT`/`BGSAVE`, M2-S20): `epoch` is the
    /// control-board request this checkpoint will satisfy; it publishes
    /// back at the MANIFEST swap's dir-fsync commit.
    pub fn request_ckpt(&mut self, epoch: u64) {
        self.ckpt.requested = true;
        self.ckpt.req_epoch = self.ckpt.req_epoch.max(epoch);
    }

    /// Checkpoint gauges for the MAINTAIN stats flush.
    pub fn ckpt_stats(&self) -> CkptStats {
        self.ckpt.stats(self.records_appended)
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
        let (write_amp, undefined) = if ckpt.completed == 0 || append_bytes == 0 {
            (0, 1)
        } else {
            let written = u128::from(log_frame_bytes)
                + u128::from(ckpt.bytes_total)
                + u128::from(manifest.bytes_written);
            let milli = (written * 1000).div_ceil(u128::from(append_bytes));
            (u64::try_from(milli).unwrap_or(u64::MAX), 0)
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
            fsyncs_fua: self.commit.stats().fsyncs_write_through,
            fua_p50_us: self.commit.write_through_latency_hist().percentile(50.0),
            fua_p99_us: self.commit.write_through_latency_hist().percentile(99.0),
            log_padding_bytes: self.staging.stats().padding_bytes,
            zero_fill_bytes: self.rotor.stats().zero_fill_bytes,
            rotations_unzeroed: self.rotor.stats().rotations_unzeroed,
            rotations_upgrade: self.rotor.stats().rotations_upgrade,
            barrier_class_degraded: u64::from(self.fua_degraded_windows >= 3),
            frames_in_flight: u64::from(self.staging.frames_in_flight()),
            frames_in_flight_max: u64::from(self.staging.stats().in_flight_max),
            frame_waits_barrier: self.frame_waits_barrier,
            frame_waits_rotation: self.frame_waits_rotation,
            frames_in_flight_now: u64::from(self.staging.in_flight()),
            records_staged: u64::from(self.staging.pending_records()),
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
            write_amp_milli_log_checkpoint: write_amp,
            write_amp_log_checkpoint_undefined: undefined,
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

// ---- tiered checkpoint walk (M4-S26; ADR-0057 D1/D3, ADR-0059 D3) ----

/// One tiered walk step's verdict.
enum TierStep {
    /// Staged entries (or budget/section bounds hit) — continue.
    Progress,
    /// A pending section of another class must seal before this pass
    /// stages — the caller queues the block now (class purity).
    SealFirst,
    /// This namespace's walk is complete (retirement scan included).
    NsDone,
}

/// Entries pulled per `ckpt_walk_slice` call (home-group granular).
const TIER_WALK_CHUNK: usize = 256;

/// One bounded step of a tiered namespace's hybrid walk. Passes: 0 =
/// address refs (cold majority, zero record touches), 1 = RAM images,
/// 2 = per-file live-set entries, 3 = cold blob references, 4 = end
/// walk + retirement scan (ADR-0059 D3 phase 1 — between walk end and
/// the manifest). Section classes never mix inside a pass, so seals
/// happen only at pass boundaries.
#[allow(clippy::too_many_arguments)] // the fill loop's split fields
fn tier_walk_step<F: SegmentFs>(
    stream: &mut inf_log::IckStream,
    table: &mut inf_store::TieredTable,
    tier_ns: &mut crate::tier_cell::TierNs<F>,
    ns: NsId,
    ckpt_id: u64,
    cursor: &mut u64,
    tier_pass: &mut u8,
    slice_cap: u32,
    emitted: &mut u32,
) -> TierStep {
    // Pass boundaries: seal any pending section before a class change.
    if *cursor == 0 && stream.can_seal() {
        return TierStep::SealFirst;
    }
    match *tier_pass {
        0 => {
            if table.space().walk_watermark().is_none() {
                table.begin_ckpt_walk(ckpt_id);
            }
            let w = table.space().walk_watermark().expect("begun above").to_raw();
            let next = table.ckpt_walk_slice(
                *cursor,
                TIER_WALK_CHUNK,
                |hash, addr| {
                    stream.stage_addr_ref(ns.0, w, hash, addr.to_raw());
                    *emitted = emitted.saturating_add(24);
                },
                |_image| {},
            );
            advance_pass(cursor, tier_pass, next);
            TierStep::Progress
        }
        1 => {
            let next = table.ckpt_walk_slice(
                *cursor,
                TIER_WALK_CHUNK,
                |_hash, _addr| {},
                |parts| {
                    let rec = match parts.type_tag {
                        inf_store::TypeTag::String => {
                            RecordView::StringPostImage { ns, key: parts.key, value: parts.value }
                        }
                        inf_store::TypeTag::StringExtent => {
                            let ext = inf_store::ExtentRef::decode(parts.value);
                            RecordView::StringExtentRef {
                                ns,
                                key: parts.key,
                                extent_id: ext.extent_id,
                                offset: ext.offset,
                                len: ext.len,
                            }
                        }
                        // Documents are not command-reachable on tiered
                        // namespaces in M4 — a doc image here is a bug.
                        other => {
                            debug_assert!(false, "tiered walk met a {other:?} record");
                            return;
                        }
                    };
                    *emitted = emitted.saturating_add(rec.encoded_len() as u32);
                    stream.stage_record(&rec);
                },
            );
            advance_pass(cursor, tier_pass, next);
            TierStep::Progress
        }
        2 => {
            let files: Vec<_> = table
                .live_set()
                .files()
                .iter()
                .skip(*cursor as usize)
                .take(TIER_WALK_CHUNK)
                .map(|f| (f.id, f.data_len, f.dead_bytes, f.byte_exact))
                .collect();
            if files.is_empty() {
                *cursor = 0;
                *tier_pass = 3;
                return TierStep::Progress;
            }
            let mut staged = 0u64;
            for (file_id, data_len, dead_bytes, byte_exact) in &files {
                stream.stage_live_set(ns.0, *file_id, *data_len, *dead_bytes, *byte_exact);
                *emitted = emitted.saturating_add(24);
                staged += 1;
                if stream.section_full() || *emitted >= slice_cap {
                    break;
                }
            }
            *cursor += staged;
            TierStep::Progress
        }
        3 => {
            let entries: Vec<(u64, u64, u64)> =
                table.extent_ckpt_entries().skip(*cursor as usize).take(TIER_WALK_CHUNK).collect();
            if entries.is_empty() {
                *cursor = 0;
                *tier_pass = 4;
                return TierStep::Progress;
            }
            let mut staged = 0u64;
            for (addr, extent_id, len) in &entries {
                stream.stage_blob_ref(ns.0, *addr, *extent_id, *len);
                *emitted = emitted.saturating_add(24);
                staged += 1;
                if stream.section_full() || *emitted >= slice_cap {
                    break;
                }
            }
            *cursor += staged;
            TierStep::Progress
        }
        _ => {
            // End the walk (release debt drains next MAINTAIN), then the
            // retirement scan stamps fully-dead candidates against this
            // checkpoint (ADR-0059 D3 phase 1).
            table.end_ckpt_walk();
            let _ = table.retire_scan(ckpt_id, &tier_ns.flush);
            TierStep::NsDone
        }
    }
}

/// Walk-slice cursor bookkeeping: 0 = this pass finished.
fn advance_pass(cursor: &mut u64, tier_pass: &mut u8, next: u64) {
    if next == 0 {
        *cursor = 0;
        *tier_pass += 1;
    } else {
        *cursor = next;
    }
}

// ---- index-sidecar walk (M4.5-S06; ADR-0078 D1/D2) ----

/// Pairs pulled per sidecar emission call — bounds the section-target
/// overshoot to one chunk (the staging buffer absorbs it).
const SIDECAR_CHUNK_ENTRIES: u32 = 256;

/// One bounded step of the sidecar phase. The plan captures once at
/// entry (converged, non-degraded indexes on the walk's namespaces —
/// ADR-0078 D1); each index streams through its re-seek cursor and
/// closes with a FINAL marker. Eligibility is re-checked every step:
/// a drop, rebuild, or degrade mid-emission abandons the stream — no
/// FINAL, and the loader discards it as incomplete. Returns `true`
/// when the pending section must seal now (index boundary, FINAL, or
/// a foreign class left from the record walk).
#[allow(clippy::too_many_arguments)] // the fill loop's split fields
fn sidecar_walk_step(
    stream: &mut inf_log::IckStream,
    ks: &Keyspace,
    ns_ids: &[u32],
    plan: &mut Option<Vec<IdxSidecarMeta>>,
    at: &mut usize,
    cursor: &mut inf_store::OrderedCursor,
    emitted_pairs: &mut u64,
    done: &mut bool,
    slice_cap: u32,
    emitted: &mut u32,
) -> bool {
    let plan = plan.get_or_insert_with(|| {
        let mut entries = Vec::new();
        for &ns_raw in ns_ids {
            for (id, generation, fixed8, _entries) in ks.idx_sidecar_candidates(NsId(ns_raw)) {
                entries.push(IdxSidecarMeta {
                    ns: ns_raw,
                    index_id: id.0,
                    generation,
                    key_encoding_version: inf_store::INDEX_KEY_ENCODING_VERSION,
                    fixed8,
                });
            }
        }
        entries
    });
    loop {
        let Some(meta) = plan.get(*at).copied() else {
            *done = true;
            // A pending tail section seals through the caller's
            // walk-complete condition.
            return false;
        };
        // Class purity: a pending foreign section (the record walk's
        // tail, or a previous index's) seals before this index stages.
        let key = (meta.ns, meta.index_id, meta.generation);
        if stream.has_pending_section() && stream.pending_idx_stream() != Some(key) {
            return true;
        }
        if !ks.idx_sidecar_eligible(NsId(meta.ns), IndexId(meta.index_id), meta.generation) {
            *at += 1;
            *cursor = inf_store::OrderedCursor::from_start();
            *emitted_pairs = 0;
            if stream.has_pending_section() {
                return true; // flush the abandoned partial section
            }
            continue;
        }
        let pulled = ks.idx_sidecar_emit(
            NsId(meta.ns),
            IndexId(meta.index_id),
            cursor,
            SIDECAR_CHUNK_ENTRIES,
            |key_bytes, entry_ref| {
                stream.stage_idx_entry(&meta, *emitted_pairs, key_bytes, entry_ref);
                *emitted_pairs += 1;
                let entry_bytes = if meta.fixed8 { 16 } else { 2 + key_bytes.len() + 8 };
                *emitted = emitted.saturating_add(entry_bytes as u32);
            },
        );
        if pulled < SIDECAR_CHUNK_ENTRIES {
            // Exhausted at this instant (fuzzy — tail catch-up owns any
            // later drift): close the stream and seal the FINAL section.
            stream.stage_idx_final(&meta, *emitted_pairs);
            *at += 1;
            *cursor = inf_store::OrderedCursor::from_start();
            *emitted_pairs = 0;
            return true;
        }
        if stream.section_full() || *emitted >= slice_cap {
            return false; // a full section seals below; budget ends the slice
        }
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
