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

use inf_foundation::time::Nanos;
use inf_log::fs::SegmentFs;
use inf_log::{
    FsyncClass, FsyncTicket, GroupCommit, MutationEffect, NsId, RecordView, SegmentConfig,
    SegmentRotor, StagingConfig, StagingRing,
};
use inf_runtime::{CompletionToken, IoOp, LoopCx, TokenClass, WaitList, WatermarkGate};
use inf_store::{CheckpointImage, Keyspace, WallAnchor};

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

/// Pinned retryable reply for aggregate staging backpressure. Early
/// admission and late exact document admission must stay byte-identical.
pub(crate) const STAGING_BUSY_ERROR: &str = "BUSY durable log staging is full, retry";

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
    /// Durability fsyncs allowed in flight per cell (M2.5-S07): 1 = the
    /// ADR-0022 D3 discipline; 2 = the bounded two-in-flight pipeline
    /// under A/B evaluation. Never more.
    pub sync_pipeline: u8,
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
    pub segments_truncated: u64,
    /// 1 while a checkpoint is streaming (`rdb_bgsave_in_progress`).
    pub ckpt_in_progress: u64,
    /// On-disk segments the rotor tracks (sealed + active + prealloc'd
    /// next) — the reclamation-bound observable.
    pub log_segments_live: u64,
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
    in_flight: Option<inf_log::FrameLease>,
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
        sync_pipeline: u8,
        rotor: SegmentRotor<F>,
        ckpt: CkptCell<F>,
        manifest: ManifestCell<F>,
    ) -> DurableCell<F> {
        // ADR-0031 D5/D6: frames sealed here stamp the recovery-derived
        // log life (1 on fresh logs).
        let mut staging = StagingRing::new(staging);
        staging.set_frame_epoch(rotor.resume_epoch());
        DurableCell {
            staging,
            rotor,
            commit: GroupCommit::with_sync_pipeline(usize::from(sync_pipeline)),
            ack_gate: WatermarkGate::new(),
            drained: WaitList::new(),
            ckpt,
            manifest,
            in_flight: None,
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
            cx.push(IoOp::Fdatasync { fd, token: fsync_token(ticket) });
        }
        if let Some(fd) = self.rotor.active_raw_fd() {
            let ticket = self.commit.register_boot_barrier(floor, None, cx.now);
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
    /// barrier instead of blocking the reactor behind the journal.
    pub fn maintain(&mut self, cx: &mut LoopCx<'_>) {
        if self.failed {
            return;
        }
        match self.rotor.maintain_deferred(cx.now.as_millis()) {
            Ok((_report, Some(barrier))) => {
                // Fd-less tiers (MemFs) have process-KILL physics — no
                // barrier needed (completed writes survive by construction).
                if let Some(fd) = inf_log::fs::SegmentFile::raw_fd(&barrier.dir) {
                    let ticket = self.commit.register_prealloc_barrier(barrier.dir, cx.now);
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
    }

    /// The LOG step (ADR-0013 D2): seal at most one frame into one
    /// positional write (+ linked fdatasync when a sync is due), or issue
    /// a standalone fdatasync for a frameless everysec tick.
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
            let frame_len = self.staging.pending_frame_len();
            let deferred = match self.rotor.begin_frame_deferred(frame_len, cx.now.as_millis()) {
                Ok(deferred) => deferred,
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
            if let Some(handoff) = seal {
                let fd = handoff.raw_fd().expect("std segment tier has fds");
                let ticket = self.commit.register_seal_fsync(handoff, cx.now);
                cx.push(IoOp::Fdatasync { fd, token: fsync_token(ticket) });
            }
            let end = slot.base().advance(frame_len);
            let covered = self.commit.watermark().map_or(0, |lsn| lsn.to_u64());
            let lease = self.staging.seal(slot.first_record_lsn(), covered);
            // A pending ckpt-begin marker rides this frame: its LSN is now
            // real (ADR-0016 D3).
            self.ckpt.on_frame_sealed(&lease);
            self.frame_seqs.push_back((end.to_u64(), self.last_seq));
            self.commit.note_frame_queued(end, frame_len);
            let fsync = self
                .commit
                .frame_fsync_due()
                .then(|| fsync_token(self.commit.register_linked_fsync(cx.now)));
            let offset = u64::from(slot.base().offset);
            let fd = self.rotor.active_raw_fd().expect("std segment tier has fds");
            self.rotor.commit_frame_queued(slot);
            self.write_seq += 1;
            let data = log_bytes::sealed_frame(&self.staging, &lease);
            self.in_flight = Some(lease);
            cx.push(IoOp::LogWrite {
                fd,
                offset,
                data,
                token: write_token(self.write_seq),
                fsync_token: fsync,
            });
        } else if self.commit.standalone_fsync_due() {
            let ticket = self.commit.register_standalone_fsync(cx.now);
            let fd = self.rotor.active_raw_fd().expect("std segment tier has fds");
            cx.push(IoOp::Fdatasync { fd, token: fsync_token(ticket) });
        }
    }

    /// REAP: the frame write's terminal completion — release the lease
    /// (the `StableBytes` custody point) and wake staging-parked pumps.
    pub fn on_log_written(&mut self) {
        self.commit.note_frame_written();
        let lease = self.in_flight.take().expect("LogWritten with no in-flight lease");
        self.staging.release(lease);
        self.drained.wake_all(());
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
        if let Some(end) = self.commit.on_fsync_complete(token_ticket(token), cx.now) {
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
        self.commit.note_everysec_tick();
        cx.timers.insert(cx.now + Nanos::from_secs(1), EVERYSEC_TIMER_KEY);
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
            let total = self.staging.stats().append_bytes;
            if self.manifest.idle() && self.ckpt.should_begin(total) {
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
            let tiered_present = ns_ids.iter().any(|&raw| ks.is_tiered(NsId(raw)));
            if let Err(err) = self.ckpt.open_stream(id, begin_lsn, ns_ids, tiered_present, cx.now) {
                self.ckpt.abort("create", &err.to_string());
                return 1;
            }
            let CkptPhase::Stream(st) = &mut self.ckpt.phase else { unreachable!("just opened") };
            let lease = st.in_flight.as_ref().expect("header staged by open_stream");
            st.write_seq += 1;
            cx.push(IoOp::LogWrite {
                fd: st.fd,
                offset: lease.offset(),
                data: log_bytes::ckpt_block(&st.stream, lease),
                token: ckpt_token(TokenClass::CkptWrite, st.write_seq),
                fsync_token: None,
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
        // DST compresses it (L7).
        if !*walk_done && cfg.stream_bytes_per_sec > 0 {
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
        if !*walk_done {
            let slice_cap = cfg.slice_bytes.min(budget_units.saturating_mul(1024)).max(1);
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
        }
        // Queue at most one block per slice: a full (or final partial)
        // section, a class-boundary seal (M4-S26 tiered passes), or —
        // once everything drained — the footer.
        if stream.section_full()
            || (force_seal && stream.can_seal())
            || (*walk_done && stream.can_seal())
        {
            let lease = stream.seal_section();
            *write_seq += 1;
            cx.push(IoOp::LogWrite {
                fd: *fd,
                offset: lease.offset(),
                data: log_bytes::ckpt_block(stream, &lease),
                token: ckpt_token(TokenClass::CkptWrite, *write_seq),
                fsync_token: None,
            });
            *in_flight = Some(lease);
        } else if *walk_done {
            let lease = stream.finish();
            *write_seq += 1;
            cx.push(IoOp::LogWrite {
                fd: *fd,
                offset: lease.offset(),
                data: log_bytes::ckpt_block(stream, &lease),
                token: ckpt_token(TokenClass::CkptWrite, *write_seq),
                fsync_token: None,
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
        let mut units =
            self.manifest.swap_slice(cx, watermark, self.rotor.active_segment(), ks, tier);
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
        self.ckpt.stats()
    }

    /// Counters for the MAINTAIN stats flush (S21 vocabulary).
    pub fn stats(&self) -> DurableStats {
        let durable = self.commit.watermark().map_or(0, |l| l.to_u64());
        let queued = self.commit.queued_up_to().map_or(0, |l| l.to_u64());
        let manifest = self.manifest.stats();
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
            segments_truncated: manifest.truncated_segments,
            ckpt_in_progress: u64::from(!matches!(self.ckpt.phase, CkptPhase::Idle)),
            log_segments_live: self.rotor.sealed().len() as u64
                + 1
                + u64::from(self.rotor.next_ready().is_some()),
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
