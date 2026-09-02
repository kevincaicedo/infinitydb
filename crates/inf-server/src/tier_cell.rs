//! M4-S26 — the plane's tiered half: per-namespace flush pipelines, the
//! cell's cold-read custody engine, the sealed-file fd table, the four
//! MAINTAIN drivers (demote → flush → release, compaction, extent
//! reclaim, retirement unlink), and the disk-admission cadence
//! (ADR-0063 D2).
//!
//! Ownership: one [`TierCell`] per cell, inside the plane's `Shared`
//! (L1 — no cross-cell state). A node that never creates a tiered
//! namespace constructs `None` of this — the ADR-0062 D8 / S03
//! zero-cost posture is structural, not conditional.
//!
//! Drive shape (M4.5-S31, ADR-0084 — the ADR-0056 D3 deviation
//! discharged): the flush/seal legs ride the **reactor drive** — one
//! bounded round per namespace stages `{fd, offset, aligned window}`
//! intents that this module converts to `IoOp::LogWrite`/`Fdatasync`;
//! `advance_flushed`, seal catalog commits, and gap crossings apply at
//! the round's last barrier completion, never at submission. REAP only
//! records completions ([`TierCell::on_flush_completion`]); MAINTAIN
//! advances the state machine ([`drive_flush_round`]) — single-writer
//! state, deterministic under DST. Cold reads are fully async from day
//! one: intents queue on [`ColdReads`], drain once per reactor
//! iteration into `IoOp::TierRead`, and complete through the custody
//! table.

use std::collections::VecDeque;
use std::path::PathBuf;

use inf_alloc::AlignedPool;
use inf_foundation::LogHistogram;
use inf_log::blob::ExtentId;
use inf_log::flush::unlink_tier_file;
use inf_log::fs::{SegmentFile, SegmentFs};
use inf_log::{NsId, TierDrive, TierFileMeta, TierFlush, TierFlushConfig, TierFlushError};
use inf_runtime::{
    ColdReadConfig, ColdReads, CompletionToken, IoOp, TierFileId, TokenClass, WaitList,
    WriteBarrier,
};
use inf_store::{Keyspace, LogicalAddr, TierSpec, TieredTable};

/// `errno` values the completion handler classifies (ADR-0084 D4).
const ENOSPC: i32 = 28;
const EIO: i32 = 5;

/// Cold-read pool window: 4 tier frames (16 KiB) per buffer — covers a
/// typical record in one read; oversized records stage through chunked
/// continuation windows (the S08 `cold_hardened` shape).
pub(crate) const COLD_POOL_BUF: usize = 4 * inf_log::TIER_FRAME_BYTES;

/// The most driver ops one flush round can carry (the ADR-0084 D3
/// token op-index bound) — the tier-flush class's ops slice for the
/// device budget (ADR-0088 D2).
pub(crate) const TIER_ROUND_MAX_OPS: u64 = 256;

/// The device budget's answer to "may a flush round of up to `bytes`
/// bytes and `ops` ops stage now?" plus the refund of the unissued part
/// (ADR-0088 D5). Generic, not `dyn`: one monomorphized closure per
/// plane. `None` = no durable plane (the MemFs test tier) — always
/// granted, nothing metered.
pub(crate) trait FlushAdmission {
    fn admit(&mut self, bytes: u64, ops: u64) -> bool;
    fn refund(&mut self, bytes: u64, ops: u64);
}

/// Extent-reclaim candidates examined per MAINTAIN slice (ADR-0061 D5
/// — reclaim is a background sweep, never a burst).
const EXTENT_RECLAIM_PER_SLICE: usize = 8;

/// Retired tier files unlinked per MAINTAIN slice (bounded teardown).
const UNLINKS_PER_SLICE: usize = 8;

/// One tiered namespace's plane-side state.
pub(crate) struct TierNs<F: SegmentFs> {
    pub ns: NsId,
    pub flush: TierFlush<F>,
    /// Open handles of sealed files, ascending by id — cold reads reuse
    /// the creation-mode fd (ADR-0054: one fd, one mode) instead of
    /// reopening; retirement closes them after the pin drain.
    files: Vec<(u32, F::File)>,
    /// Retired metas awaiting `inflight_on == 0` before close + unlink
    /// (§3.3 — a file with in-flight cold reads is never deleted).
    retired: Vec<TierFileMeta>,
    /// `TAIL-STALL-TIMEOUT` (ADR-0053 D4), consumed at construction.
    pub tail_stall_timeout_ms: u32,
    /// `TIER-IO-MODE` (ADR-0054), consumed at construction — extent
    /// reads open in the same mode the writes used.
    pub io_mode: inf_log::TierIoMode,
    /// Writers parked on tail-allocation stalls. Woken on **head
    /// advancement** (the release leg) — never on flush confirmation
    /// (the recorded 0xA4C01D07 class: flush-keyed wakes wedge when
    /// confirmation lands but no page released) — plus every MAINTAIN
    /// tick while non-empty, so the typed timeout always fires.
    pub stall_waiters: WaitList<()>,
    /// `(durable seq, wal epoch)` marks: once the fsync watermark
    /// covers `seq`, every extent death stamped ≤ `epoch` is durable —
    /// the ADR-0061 D5 reclaim gate's plane-supplied input.
    epoch_marks: VecDeque<(u64, u64)>,
    /// The reclaim epoch derived from drained marks.
    durable_epoch: u64,
    /// One compaction read chain in flight at a time (bounded).
    pub compact_inflight: bool,
    /// Namespace directory (teardown unlink root).
    pub dir: PathBuf,
    /// Completion-token lane (M4.5-S31, ADR-0084 D3): stable for this
    /// namespace's plane life; recycled by [`TierCell`] once no op of it
    /// is in flight, the round sequence carried forward (batch 12).
    lane: u32,
    /// Round-identity generation for the lane (wrapping; stale
    /// completions mismatch and are counted, never applied).
    round_seq: u32,
    /// The one in-flight flush round's bookkeeping (ADR-0084 D2 — the
    /// explicit per-namespace bound; the staged intents live in
    /// [`TierFlush`]).
    round: Option<FlushRound>,
    /// Set when the namespace is dropped with a round in flight (the
    /// ADR-0084 D3 custody park): the catalog epoch its files release
    /// at once the round is terminal (ADR-0100 D5).
    release_epoch: u64,
}

/// Plane bookkeeping of one in-flight flush round (M4.5-S31): terminal
/// op states parallel to `TierFlush::round_op`, the wave cursor, and
/// the identity its completion tokens carry.
pub(crate) struct FlushRound {
    states: Vec<OpState>,
    /// Ops submitted, not yet terminal.
    pending: u32,
    /// Wave 2 submitted (barriers ride only after every write landed —
    /// fdatasync covers only completed writes).
    barriers_sent: bool,
    round_seq: u32,
    staged_at_us: u64,
    /// Worst write error of the current attempt (ENOSPC dominates).
    write_error: Option<i32>,
    /// A barrier failed — fatal at the next MAINTAIN (§8.4).
    fatal: Option<i32>,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum OpState {
    Unsent,
    Sent,
    Done,
    Failed,
}

impl FlushRound {
    fn new(op_count: usize, round_seq: u32, staged_at_us: u64) -> FlushRound {
        FlushRound {
            states: vec![OpState::Unsent; op_count],
            pending: 0,
            barriers_sent: false,
            round_seq,
            staged_at_us,
            write_error: None,
            fatal: None,
        }
    }
}

/// Cell-scope reactor-drive flush observables (ADR-0084 D6 — the
/// collapse must be visible from INFO).
#[derive(Default)]
pub(crate) struct TierFlushStats {
    pub rounds: u64,
    pub write_retries: u64,
    pub stale_completions: u64,
    pub round_us: LogHistogram,
    /// Rounds the device budget deferred (ADR-0088 D5) — the tier-flush
    /// face of `io_budget_deferrals_tier_flush`, per namespace tick.
    pub rounds_deferred: u64,
}

impl<F: SegmentFs> TierNs<F> {
    /// Cold-read window plan for `addr`: `(fd, file, disk offset, frame
    /// count skip)` — the caller turns it into a `ColdReads::enqueue`.
    /// Returns `None` when the address is not inside any catalogued
    /// file's range (a displaced-then-retired address raced the lookup;
    /// the caller re-resolves — never an error).
    pub fn plan_cold_read(
        &self,
        addr: LogicalAddr,
        len: usize,
    ) -> Option<(std::os::fd::RawFd, TierFileId, u64, u64, usize)> {
        let raw = addr.to_raw();
        let locate = |base: u64, data_len: u64| raw >= base && raw < base + data_len;
        // Sealed catalog first (ascending by base; linear scan — a cell
        // holds tens to hundreds of files and this runs per cold miss,
        // not per command).
        for meta in self.flush.sealed() {
            if locate(meta.base.to_raw(), meta.data_len) {
                let handle = self.files.iter().find(|(id, _)| *id == meta.id)?;
                let fd = handle.1.raw_fd()?;
                return Some(Self::window(
                    fd,
                    meta.id,
                    meta.base.to_raw(),
                    meta.data_len,
                    raw,
                    len,
                ));
            }
        }
        // Files whose seal is staged but not completion-committed
        // (M4.5-S31): the confirmed prefix stays servable through the
        // held handle — the window is clamped to it (frames past it may
        // still be in flight).
        for pending in self.flush.pending_seals() {
            if locate(pending.base.to_raw(), pending.confirmed_len) {
                let fd = pending.fd?;
                return Some(Self::window(
                    fd,
                    pending.id,
                    pending.base.to_raw(),
                    pending.confirmed_len,
                    raw,
                    len,
                ));
            }
        }
        let (id, base, data_len, durable_len, _) = self.flush.active()?;
        if locate(base.to_raw(), durable_len) {
            let fd = self.flush.active_raw_fd()?;
            return Some(Self::window(fd, id, base.to_raw(), data_len, raw, len));
        }
        None
    }

    fn window(
        fd: std::os::fd::RawFd,
        id: u32,
        base: u64,
        data_len: u64,
        raw: u64,
        len: usize,
    ) -> (std::os::fd::RawFd, TierFileId, u64, u64, usize) {
        let (first, _, skip) = inf_log::tier_frame_span(raw - base, len.max(1));
        let file_frames = data_len.div_ceil(inf_log::TIER_FRAME_DATA as u64);
        let window_frames =
            ((COLD_POOL_BUF / inf_log::TIER_FRAME_BYTES) as u64).min(file_frames - first);
        debug_assert!(window_frames > 0, "cold window inside the file's range");
        (fd, TierFileId::new(id), inf_log::tier_frame_offset(first), window_frames, skip)
    }
}

/// The cell's tiered plane state. Constructed by `enable_durable` (a
/// tiered namespace is a configuration of `MODE durable` — ADR-0062
/// D1); populated lazily as tiered namespaces materialize.
pub(crate) struct TierCell<F: SegmentFs> {
    fs: F,
    cell: u32,
    shard_dir: PathBuf,
    pub namespaces: Vec<TierNs<F>>,
    /// The custody engine (ADR-0055): one per cell, built at the first
    /// tiered materialization, sized from that namespace's
    /// `COLD-READ-QD` (CreateOnly). A later namespace with a different
    /// value keeps the standing engine — recorded in the ledger; the
    /// M4 harness/soak shape is one tiered namespace per node.
    pub cold: Option<ColdReads>,
    /// Split service histograms (ADR-0064 D3): command service time in
    /// µs on the loop clock, tagged by resolution lane. Worst cell
    /// binds; percentiles never merge across cells.
    pub ram_hit_us: LogHistogram,
    pub cold_us: LogHistogram,
    /// Dropped namespaces' teardown queues (pins drain before unlink).
    teardown: Vec<TeardownNs>,
    /// Extent-seal fdatasyncs awaiting the driver (ADR-0061 D3): the
    /// ledger barrier registered at stage time; the op itself rides the
    /// next MAINTAIN's push (ordering lives in the ledger, not the
    /// submission queue).
    pending_syncs: Vec<(std::os::fd::RawFd, inf_log::FsyncTicket)>,
    /// Next fresh completion-token lane (M4.5-S31). Lanes are 16 bits of
    /// the token slot (`MAX_SLOT >> 8`), so they recycle (batch 12 of the
    /// 2026-08-30 review): a lane returns to `free_lanes` only once its
    /// namespace has no op in flight — dropped with no round, or drained
    /// through `round_drain` — and the reuser continues the lane's round
    /// sequence, so the generation half still makes stale routing
    /// unrepresentable. Before this the counter was monotone and the
    /// 65,537th tiered `CREATE` on a cell's lifetime was a release assert.
    next_lane: u32,
    /// Recycled lanes with the round sequence the next holder starts
    /// above.
    free_lanes: Vec<(u32, u32)>,
    /// Dropped namespaces whose flush round is still in flight: the
    /// windows the driver may touch live in their pipelines, so the
    /// whole [`TierNs`] parks here until every op is terminal, then its
    /// files join the teardown queue (ADR-0084 D3 custody).
    round_drain: Vec<TierNs<F>>,
    /// Reactor-drive flush observables (ADR-0084 D6).
    pub(crate) flush_stats: TierFlushStats,
}

/// A dropped namespace's file half, unlinked in bounded slices
/// (ADR-0062 D7 — DROP's plane-side obligation) once the catalog swap
/// that carries the drop is durable (ADR-0100 D5: `release_epoch` is
/// that persist's epoch; a crash before it must find every file).
struct TeardownNs {
    files: Vec<TierFileMeta>,
    release_epoch: u64,
}

/// One compaction cold-read chain for the plane to issue
/// (`ReadClass::Maintain`; ADR-0059 D2 — chunks feed
/// `TieredTable::compaction_apply` back through the exact cursor).
#[derive(Copy, Clone, Debug)]
pub(crate) struct CompactRead {
    pub ns: NsId,
    pub file_id: u32,
    pub addr: LogicalAddr,
    pub len: u64,
}

impl<F: SegmentFs> TierNs<F> {
    /// Queues one detached (retirement-committed) file for the
    /// pin-gated unlink (ADR-0059 D3 phases 2-3; §3.3).
    pub fn note_retired(&mut self, meta: TierFileMeta) {
        self.retired.push(meta);
    }
}

impl<F: SegmentFs> TierCell<F> {
    pub fn ns(&self, ns: NsId) -> Option<&TierNs<F>> {
        self.namespaces.iter().find(|t| t.ns == ns)
    }

    /// Queues one registered extent-seal barrier's fdatasync for the
    /// next MAINTAIN drain (ADR-0061 D3 — M4-S26).
    pub fn queue_extent_sync(&mut self, fd: std::os::fd::RawFd, ticket: inf_log::FsyncTicket) {
        self.pending_syncs.push((fd, ticket));
    }

    /// Drains the queued extent-seal fdatasyncs (the plane pushes them
    /// as driver ops each MAINTAIN).
    pub fn take_pending_syncs(&mut self) -> Vec<(std::os::fd::RawFd, inf_log::FsyncTicket)> {
        core::mem::take(&mut self.pending_syncs)
    }

    /// Opens a blob extent for reading in the namespace's creation
    /// I/O mode (M4-S26 blob reads; the §3.3 open is metadata-cheap and
    /// the returned reader owns the fd across the chunked reads).
    ///
    /// # Errors
    /// Open/probe failures — the command layer answers typed.
    pub fn open_extent_reader(
        &self,
        ns: NsId,
        extent_id: u64,
    ) -> std::io::Result<inf_log::blob::ExtentReader<F::File>> {
        let t = self.ns(ns).ok_or_else(|| std::io::Error::other("namespace dropped"))?;
        inf_log::blob::open_extent(&self.fs, &t.dir, ExtentId(extent_id), t.io_mode)
    }

    /// The owning cell index (extent headers carry it).
    pub fn cell_index(&self) -> u32 {
        self.cell
    }

    /// The injected filesystem seam (extent creation on the write path).
    pub fn fs(&self) -> &F {
        &self.fs
    }

    pub fn ns_mut(&mut self, ns: NsId) -> Option<&mut TierNs<F>> {
        self.namespaces.iter_mut().find(|t| t.ns == ns)
    }
}

impl<F: SegmentFs + Clone> TierCell<F> {
    pub fn new(fs: F, cell: u32, shard_dir: PathBuf) -> TierCell<F> {
        TierCell {
            fs,
            cell,
            shard_dir,
            namespaces: Vec::new(),
            cold: None,
            ram_hit_us: LogHistogram::new(),
            cold_us: LogHistogram::new(),
            teardown: Vec::new(),
            pending_syncs: Vec::new(),
            next_lane: 0,
            free_lanes: Vec::new(),
            round_drain: Vec::new(),
            flush_stats: TierFlushStats::default(),
        }
    }

    /// Reconciles plane state with the keyspace's tiered set (runs each
    /// MAINTAIN; DDL is rare, the fast path is two length loads).
    /// Creation builds the flush pipeline + custody engine; a dropped
    /// namespace moves its file half to the bounded teardown queue,
    /// held until the catalog persist named in `releases` is durable
    /// (ADR-0100 D5 — the DROP program records `(ns, epoch)` there
    /// before this runs; the entry is consumed here).
    pub fn sync_namespaces(&mut self, ks: &Keyspace, releases: &mut Vec<(NsId, u64)>) {
        let live: Vec<(NsId, TierSpec)> =
            ks.ns_iter().filter_map(|spec| spec.tier.map(|t| (spec.id, t))).collect();
        if live.len() == self.namespaces.len()
            && live.iter().zip(&self.namespaces).all(|((id, _), t)| *id == t.ns)
        {
            return;
        }
        // Drops first (ids never recycle within a boot — registry rule).
        let mut i = 0;
        while i < self.namespaces.len() {
            if live.iter().any(|(id, _)| *id == self.namespaces[i].ns) {
                i += 1;
                continue;
            }
            let mut gone = self.namespaces.remove(i);
            let release_epoch = match releases.iter().position(|(ns, _)| *ns == gone.ns) {
                Some(at) => releases.swap_remove(at).1,
                None => {
                    // Every drop path records its epoch before MAINTAIN
                    // can run; a missing entry is a violated invariant,
                    // answered by today's immediate teardown in release.
                    debug_assert!(false, "dropped tiered ns {} without a release epoch", gone.ns.0);
                    0
                }
            };
            // A dropped namespace with a flush round in flight parks
            // whole: the driver may still touch its windows, so nothing
            // frees until every op is terminal (ADR-0084 D3 custody).
            if gone.round.is_some() {
                gone.release_epoch = release_epoch;
                self.round_drain.push(gone);
                continue;
            }
            // No op in flight on this lane: it recycles now, its round
            // sequence carried forward.
            self.free_lanes.push((gone.lane, gone.round_seq));
            self.teardown.push(TeardownNs { files: teardown_files(&gone), release_epoch });
        }
        for (id, spec) in live {
            if self.namespaces.iter().any(|t| t.ns == id) {
                continue;
            }
            self.create_ns(id, &spec);
        }
    }

    fn create_ns(&mut self, ns: NsId, spec: &TierSpec) {
        if self.cold.is_none() {
            let qd = usize::from(spec.cold_read_qd);
            let pool = AlignedPool::new(qd, COLD_POOL_BUF);
            self.cold = Some(ColdReads::with_config(
                pool,
                ColdReadConfig { qd_cap: qd, overflow_cap: 4 * qd, ..ColdReadConfig::default() },
            ));
        }
        let dir = self.shard_dir.join(format!("ns-{}", ns.0));
        let mut flush = TierFlush::new(
            self.fs.clone(),
            TierFlushConfig {
                shard_dir: dir.clone(),
                cell: self.cell,
                ns,
                mode: spec.tier_io_mode,
                file_capacity: inf_log::flush::TIER_FILE_CAPACITY_DEFAULT,
                slice_bytes: spec.maintain_slice_bytes,
            },
            0,
        );
        // The plane's filesystems are fd-backed (StdSegmentFs, SimDisk):
        // flush I/O rides the driver (M4.5-S31, ADR-0084 D1).
        flush.set_drive(TierDrive::Reactor);
        // A recycled lane first (its round sequence continues past the
        // previous holder's), else a fresh one. The bound is on lanes
        // held at once — live plus draining tiered namespaces — which
        // memory exhausts long before 2^16 (a pipeline is MiBs).
        let (lane, round_seq) = self.free_lanes.pop().unwrap_or_else(|| {
            let lane = self.next_lane;
            self.next_lane += 1;
            (lane, 0)
        });
        assert!(lane <= inf_runtime::MAX_SLOT >> 8, "tier flush lanes exhausted");
        self.namespaces.push(TierNs {
            ns,
            flush,
            files: Vec::new(),
            retired: Vec::new(),
            tail_stall_timeout_ms: spec.tail_stall_timeout_ms,
            io_mode: spec.tier_io_mode,
            stall_waiters: WaitList::new(),
            epoch_marks: VecDeque::new(),
            durable_epoch: 0,
            compact_inflight: false,
            dir,
            lane,
            round_seq,
            round: None,
            release_epoch: 0,
        })
    }

    /// Installs a recovered namespace's pipeline + file handles (boot —
    /// ADR-0057 D6; replaces the fresh pipeline `sync_namespaces` would
    /// otherwise build). Consumed by the recovery composition
    /// (`plane.rs` on `RecoveryProgress::Complete`).
    pub fn install_recovered(
        &mut self,
        ns: NsId,
        spec: &TierSpec,
        flush: TierFlush<F>,
        files: Vec<(u32, F::File)>,
    ) {
        if let Some(pos) = self.namespaces.iter().position(|t| t.ns == ns) {
            let replaced = self.namespaces.remove(pos);
            // Boot-time replacement: nothing is in flight before serving.
            self.free_lanes.push((replaced.lane, replaced.round_seq));
        }
        self.create_ns(ns, spec);
        let t = self.namespaces.last_mut().expect("just created");
        t.flush = flush;
        // Recovery built the pipeline on the seam (boot, pre-serving);
        // its flush work from here rides the driver (ADR-0084 D1).
        t.flush.set_drive(TierDrive::Reactor);
        t.files = files;
    }

    /// The four MAINTAIN drivers for one namespace, bounded per slice.
    /// Returns `(budget units used, compaction read to issue)` — the
    /// caller charges Maintenance and spawns the read chain (it owns
    /// the executor; this module owns no `Rc<Shared>`). `durable_mark`
    /// is `(last staged seq, fsync watermark seq)` from the durable
    /// cell — the extent-reclaim epoch handoff (ADR-0061 D5).
    #[allow(clippy::too_many_arguments)] // the MAINTAIN driver's split inputs (ADR-0088 D5)
    pub fn maintain_ns(
        &mut self,
        ks: &mut Keyspace,
        at: usize,
        durable_mark: Option<(u64, u64)>,
        transition_idle: bool,
        now_us: u64,
        ops: &mut Vec<IoOp>,
        admission: &mut impl FlushAdmission,
    ) -> Result<(u32, Option<CompactRead>), TierFlushError> {
        let cold = self.cold.clone();
        let stats = &mut self.flush_stats;
        let t = &mut self.namespaces[at];
        let Some(table) = ks.tiered_store_mut(t.ns) else {
            return Ok((0, None)); // dropped this tick; sync_namespaces reconciles next
        };
        let mut units = 0u32;

        // Aborted-transition reconciliation (M4-S26): with no checkpoint
        // streaming and no swap pending, a pinned walk means the
        // checkpoint aborted mid-walk (release debt would otherwise
        // never drain), and stamped retirement candidates lost their
        // covering swap — re-offer them (ADR-0059 D3 abort leg).
        if transition_idle {
            if table.space().walk_watermark().is_some() {
                table.end_ckpt_walk();
            }
            table.abort_retirement();
        }

        // ---- demote leg: seal → flush → release (§3.1 slice order).
        // The flush half rides the reactor drive (M4.5-S31, ADR-0084):
        // stage a round when idle, advance the in-flight one on the
        // completions REAP recorded, apply its effects at the last
        // barrier CQE — the reactor never waits on the device here.
        let sealed = if table.demote_due() { table.seal_slice() } else { 0 };
        let flush_bytes = drive_flush_round(table, t, stats, now_us, ops, admission)?;
        for (id, handle) in t.flush.take_sealed_handles() {
            t.files.push((id, handle));
        }
        let released = table.release_slice();
        if (sealed | released | flush_bytes) > 0 {
            units += 1 + ((sealed + released + flush_bytes) / 4096) as u32;
        }
        // Head advanced ⇒ ring space may have freed: wake stalled
        // writers. Also wake while any are parked so the typed timeout
        // is always reachable (bounded: waiters re-check, then repark).
        if released > 0 || t.stall_waiters.waiting() > 0 {
            t.stall_waiters.wake_all(());
        }

        // ---- admission cadence (ADR-0063 D2): both usage halves are
        // fresh exactly here (post-flush).
        table.refresh_disk_admission(t.flush.disk_bytes());

        // ---- extent reclaim epoch (ADR-0061 D5): a mark drains once
        // the fsync watermark covers its seq.
        if let Some((staged_seq, durable_seq)) = durable_mark {
            let epoch = table.wal_epoch();
            if t.epoch_marks.back().is_none_or(|&(s, e)| s < staged_seq && e < epoch) {
                t.epoch_marks.push_back((staged_seq, epoch));
            }
            while t.epoch_marks.front().is_some_and(|&(s, _)| s <= durable_seq) {
                let (_, epoch) = t.epoch_marks.pop_front().expect("checked front");
                t.durable_epoch = t.durable_epoch.max(epoch);
            }
        }
        let reclaim = table.extent_reclaim_work(t.durable_epoch, EXTENT_RECLAIM_PER_SLICE);
        for candidate in reclaim {
            units += 1;
            let extent_id = candidate.extent_id;
            let id = ExtentId(extent_id);
            /// How one candidate's disposal ended (drives the store ack).
            enum Disposal {
                Unlinked,
                Quarantined,
            }
            let outcome = match candidate.origin {
                // Refcount-proven death: the count is the proof
                // (ADR-0061 D5) — unlink as always.
                inf_store::ReclaimOrigin::Death => {
                    inf_log::blob::unlink_extent_file(&self.fs, &t.dir, id)
                        .map(|()| Disposal::Unlinked)
                }
                // Boot orphan (ADR-0096 D2): probe the header first. A
                // file that is not a well-formed extent of this id is
                // garbage nothing can reference — unlink. A verifying
                // header quarantines by rename: the bytes survive the
                // life, and a later boot delivers the second verdict.
                inf_store::ReclaimOrigin::BootOrphan => {
                    let path = t.dir.join("cold").join(inf_log::extent_file_name(id));
                    match inf_log::probe_extent_file(&self.fs, &path) {
                        Ok(header) if header.extent_id == id => {
                            inf_log::blob::quarantine_extent_file(&self.fs, &t.dir, id)
                                .map(|()| Disposal::Quarantined)
                        }
                        Ok(_) => inf_log::blob::unlink_extent_file(&self.fs, &t.dir, id)
                            .map(|()| Disposal::Unlinked),
                        Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                            inf_log::blob::unlink_extent_file(&self.fs, &t.dir, id)
                                .map(|()| Disposal::Unlinked)
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                            Ok(Disposal::Unlinked) // already gone — the goal state
                        }
                        Err(e) => Err(e),
                    }
                }
                // Second verdict (ADR-0096 D4): still unreferenced after
                // a full boot cycle — unlink the quarantine twin.
                inf_store::ReclaimOrigin::Quarantined => {
                    inf_log::blob::unlink_quarantined_file(&self.fs, &t.dir, id)
                        .map(|()| Disposal::Unlinked)
                }
            };
            match outcome {
                Ok(Disposal::Unlinked) => table.extent_reclaim_done(extent_id),
                Ok(Disposal::Quarantined) => table.extent_reclaim_quarantined(extent_id),
                // Non-fatal by contract (ADR-0061 D5): counted,
                // re-offered with its origin intact, boot-sweep
                // re-driven.
                Err(_) => table.extent_reclaim_deferred(extent_id),
            }
        }

        // ---- retirement unlink (§3.3: pins drain first; bounded).
        let cold_ref = cold.as_ref();
        let mut unlinked = 0;
        let mut u = 0;
        while u < t.retired.len() && unlinked < UNLINKS_PER_SLICE {
            let pinned =
                cold_ref.is_some_and(|c| c.inflight_on(TierFileId::new(t.retired[u].id)) > 0);
            if pinned {
                u += 1;
                continue;
            }
            let meta = t.retired.remove(u);
            t.files.retain(|(id, _)| *id != meta.id);
            let _ = unlink_tier_file(&self.fs, &meta); // non-fatal: re-queued by boot GC
            unlinked += 1;
            units += 1;
        }

        // ---- compaction (ADR-0059): one read chain in flight; the
        // caller spawns it with `ReadClass::Maintain`.
        let mut compact = None;
        if !t.compact_inflight {
            let pressure = table.compaction_pressure(t.flush.disk_bytes());
            let slice = table.compaction_config().slice_bytes;
            if let inf_store::CompactionWork::Read { file_id, addr, len } =
                table.compaction_work(&t.flush, pressure, slice)
            {
                t.compact_inflight = true;
                compact = Some(CompactRead { ns: t.ns, file_id, addr, len });
            }
        }
        Ok((units, compact))
    }

    /// Bounded teardown slices for dropped namespaces (ADR-0062 D7).
    /// Unlinks route through the injected fs (L7) and failure only
    /// defers disk space — the boot orphan sweep re-drives it.
    /// Also advances parked rounds of dropped namespaces (ADR-0084 D3):
    /// once every op is terminal, the windows recycle and the files
    /// join the teardown queue. A queue whose catalog persist is not yet
    /// durable (`persisted(release_epoch)` false — ADR-0100 D5) waits:
    /// a cut before the swap must find every file.
    pub fn maintain_teardown(&mut self, persisted: impl Fn(u64) -> bool) -> u32 {
        let mut i = 0;
        while i < self.round_drain.len() {
            let done = self.round_drain[i].round.as_ref().is_none_or(|r| r.pending == 0);
            if !done {
                i += 1;
                continue;
            }
            let mut gone = self.round_drain.remove(i);
            gone.round = None;
            // Every op of the lane is terminal: it recycles, its round
            // sequence carried forward (a completion for it can no longer
            // arrive, and the sequence keeps a stale one unroutable).
            self.free_lanes.push((gone.lane, gone.round_seq));
            // Effects are discarded — the table is gone; the pipeline's
            // catalog facts die with it. Windows return to the pool the
            // drop then frees.
            let _ = gone.flush.finish_round();
            self.teardown.push(TeardownNs {
                files: teardown_files(&gone),
                release_epoch: gone.release_epoch,
            });
        }
        let mut units = 0;
        let fs = self.fs.clone();
        let cold = self.cold.as_ref();
        for tn in &mut self.teardown {
            if !persisted(tn.release_epoch) {
                continue;
            }
            let mut i = 0;
            while i < tn.files.len() && units < UNLINKS_PER_SLICE as u32 {
                let id = tn.files[i].id;
                let pinned =
                    id != u32::MAX && cold.is_some_and(|c| c.inflight_on(TierFileId::new(id)) > 0);
                if pinned {
                    i += 1;
                    continue;
                }
                let meta = tn.files.remove(i);
                let _ = fs.remove_file(&meta.path);
                units += 1;
            }
        }
        self.teardown.retain(|tn| !tn.files.is_empty());
        units
    }

    /// REAP entry (M4.5-S31): records one tier-flush completion into its
    /// round — a counter update only, no store borrow, no effect
    /// application (MAINTAIN advances the machine — L7-deterministic
    /// single-writer state). Stale tokens (superseded round, dropped
    /// lane) are counted and ignored, never applied.
    pub fn on_flush_completion(&mut self, token: CompletionToken, errno: Option<i32>) {
        let lane = token.slot() >> 8;
        let index = (token.slot() & 0xFF) as usize;
        let all = self.namespaces.iter_mut().chain(self.round_drain.iter_mut());
        let Some(t) = all.into_iter().find(|t| t.lane == lane) else {
            self.flush_stats.stale_completions += 1;
            return;
        };
        let write_count = t.flush.round_write_count();
        let Some(round) = &mut t.round else {
            self.flush_stats.stale_completions += 1;
            return;
        };
        if round.round_seq != token.generation() || round.states[index] != OpState::Sent {
            self.flush_stats.stale_completions += 1;
            return;
        }
        round.pending -= 1;
        let is_barrier = index >= write_count;
        debug_assert_eq!(
            is_barrier,
            token.class() == TokenClass::TierFlushSync,
            "op kind and token class agree"
        );
        // Deterministic error stand-ins at the completion boundary
        // (ADR-0084 D4 — the ADR-0020 `DURABLE_FSYNC_EIO` shape; the
        // seam sites keep the same points for the component matrix).
        let errno = errno.or_else(|| {
            if is_barrier {
                inf_foundation::fault::fire(inf_log::fault::TIER_FSYNC_ERR).then_some(EIO)
            } else {
                inf_foundation::fault::fire(inf_log::fault::TIER_WRITE_NOSPACE).then_some(ENOSPC)
            }
        });
        match errno {
            None => round.states[index] = OpState::Done,
            Some(e) if is_barrier => {
                round.states[index] = OpState::Failed;
                round.fatal = Some(e);
            }
            Some(e) => {
                round.states[index] = OpState::Failed;
                // ENOSPC dominates: it must reach the admission latch.
                if e == ENOSPC || round.write_error.is_none() {
                    round.write_error = Some(e);
                }
            }
        }
    }

    /// Reactor-drive flush rounds currently in flight (INFO gauge).
    pub(crate) fn flush_rounds_inflight(&self) -> u64 {
        let live = self.namespaces.iter().filter(|t| t.round.is_some()).count();
        let draining = self.round_drain.len();
        (live + draining) as u64
    }
}

/// The teardown file list of a dropped namespace: the sealed catalog,
/// staged-but-uncommitted seals, and the active file.
fn teardown_files<F: SegmentFs>(gone: &TierNs<F>) -> Vec<TierFileMeta> {
    let mut files: Vec<TierFileMeta> = gone.flush.sealed().to_vec();
    for pending in gone.flush.pending_seals() {
        files.push(TierFileMeta {
            id: pending.id,
            base: pending.base,
            data_len: pending.data_len,
            reason: inf_log::SealReason::Shutdown,
            path: gone.dir.join("cold").join(inf_log::tier_file_name(pending.id)),
        });
    }
    if let Some((_, base, data_len, _, path)) = gone.flush.active() {
        files.push(TierFileMeta {
            id: u32::MAX,
            base,
            data_len,
            reason: inf_log::SealReason::Shutdown,
            path: path.to_path_buf(),
        });
    }
    files
}

/// Advances one namespace's flush state machine by one MAINTAIN step
/// (M4.5-S31, ADR-0084 D2/D4): completes a round whose barriers all
/// landed, escalates a barrier failure to the §8.4 fatal class,
/// resubmits failed writes byte-identical (the M4-S21 retained-batch
/// retry — ENOSPC latches admission first), promotes a fully-written
/// round to its barrier wave, or stages a fresh round when idle.
/// Returns the record bytes staged this step (the unit-charge basis).
fn drive_flush_round<F: SegmentFs>(
    table: &mut TieredTable,
    t: &mut TierNs<F>,
    stats: &mut TierFlushStats,
    now_us: u64,
    ops: &mut Vec<IoOp>,
    admission: &mut impl FlushAdmission,
) -> Result<u64, TierFlushError> {
    if let Some(round) = &mut t.round {
        if let Some(errno) = round.fatal {
            // A failed durability barrier freezes the watermark exactly
            // where the last good round left it (ADR-0056 D4).
            return Err(TierFlushError::Fsync {
                path: t.dir.join("cold"),
                source: std::io::Error::from_raw_os_error(errno),
            });
        }
        if round.pending > 0 {
            return Ok(0);
        }
        if let Some(errno) = round.write_error.take() {
            if errno == ENOSPC {
                table.note_flush_device_full();
            }
            stats.write_retries += 1;
            for state in &mut round.states {
                if *state == OpState::Failed {
                    *state = OpState::Unsent;
                }
            }
            emit_round_wave(&t.flush, round, t.lane, RoundWave::Writes, ops);
            return Ok(0);
        }
        if !round.barriers_sent {
            round.barriers_sent = true;
            emit_round_wave(&t.flush, round, t.lane, RoundWave::Barriers, ops);
            return Ok(0);
        }
        let round = t.round.take().expect("checked above");
        let _ = table.complete_flush_round(&mut t.flush);
        stats.rounds += 1;
        stats.round_us.record(now_us.saturating_sub(round.staged_at_us));
        return Ok(0);
    }
    // ADR-0088 D5: the round's byte bound is offered to the device budget
    // before anything stages; `Deferred` ⇒ not this tick — the sealed
    // backlog waits where it is (no state moved), re-offered next tick.
    // The bound is the slice; the unissued remainder is refunded after
    // staging reports the exact bytes.
    let slice_bound = t.flush.slice_bytes();
    if !admission.admit(slice_bound, TIER_ROUND_MAX_OPS) {
        stats.rounds_deferred += 1;
        return Ok(0);
    }
    let stage_result = table.stage_flush_round(&mut t.flush);
    let staged_bytes = *stage_result.as_ref().unwrap_or(&0);
    let issued_ops = if t.flush.round_active() { t.flush.round_op_count() as u64 } else { 0 };
    admission.refund(
        slice_bound.saturating_sub(staged_bytes),
        TIER_ROUND_MAX_OPS.saturating_sub(issued_ops),
    );
    if t.flush.round_active() {
        let op_count = t.flush.round_op_count();
        assert!(op_count <= 256, "flush round exceeds the token op-index bound (ADR-0084 D3)");
        if op_count == 0 {
            // Effects-only round (a gap at a file boundary): nothing to
            // wait on — the crossing applies now, as the seam drive did.
            let _ = table.complete_flush_round(&mut t.flush);
            stats.rounds += 1;
        } else {
            t.round_seq = t.round_seq.wrapping_add(1);
            let mut round = FlushRound::new(op_count, t.round_seq, now_us);
            emit_round_wave(&t.flush, &mut round, t.lane, RoundWave::Writes, ops);
            t.round = Some(round);
        }
    }
    stage_result?;
    Ok(staged_bytes)
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum RoundWave {
    Writes,
    Barriers,
}

/// Converts one wave of the staged round to driver ops. Windows stay
/// pipeline-owned until `finish_round` — the `StableBytes` custody
/// argument lives in `log_bytes::tier_round_bytes`.
fn emit_round_wave<F: SegmentFs>(
    flush: &TierFlush<F>,
    round: &mut FlushRound,
    lane: u32,
    wave: RoundWave,
    ops: &mut Vec<IoOp>,
) {
    let writes = flush.round_write_count();
    let range = match wave {
        RoundWave::Writes => 0..writes,
        RoundWave::Barriers => writes..flush.round_op_count(),
    };
    for index in range {
        if round.states[index] != OpState::Unsent {
            continue;
        }
        let view = flush.round_op(index);
        let slot = (lane << 8) | index as u32;
        if view.is_barrier {
            let token = CompletionToken::new(TokenClass::TierFlushSync, slot, round.round_seq);
            ops.push(IoOp::Fdatasync { fd: view.fd, token });
        } else {
            let token = CompletionToken::new(TokenClass::TierFlushWrite, slot, round.round_seq);
            ops.push(IoOp::LogWrite {
                fd: view.fd,
                offset: view.offset,
                data: crate::log_bytes::tier_round_bytes(&view),
                token,
                barrier: WriteBarrier::None,
            });
        }
        round.states[index] = OpState::Sent;
        round.pending += 1;
    }
}

#[cfg(test)]
mod lane_tests {
    use super::*;
    use inf_log::fs::mem::MemFs;
    use inf_store::{Keyspace, StoreConfig, TierSpec};

    fn spec() -> TierSpec {
        TierSpec::for_budget(64 << 20)
    }

    /// Batch 12 of the 2026-08-30 review: flush lanes recycle once their
    /// namespace has no op in flight, and the reuser continues the round
    /// sequence. Before the fix `next_lane` was monotone: with the counter
    /// at the width this test starts from, the second `CREATE` was the
    /// release assert `tier flush lanes exhausted` (a DDL count over the
    /// cell's lifetime as a node kill).
    #[test]
    fn dropped_namespace_lane_recycles_with_its_round_sequence() {
        let mut cell = TierCell::new(MemFs::new(), 0, PathBuf::from("/shard-0"));
        cell.next_lane = inf_runtime::MAX_SLOT >> 8;
        cell.create_ns(NsId(16), &spec());
        let first = &cell.namespaces[0];
        assert_eq!(first.lane, inf_runtime::MAX_SLOT >> 8, "the last fresh lane");
        cell.namespaces[0].round_seq = 7;
        // A keyspace without the namespace: MAINTAIN's reconcile drops it
        // (no round in flight → the lane recycles immediately).
        let ks = Keyspace::new(StoreConfig::default());
        let mut releases = vec![(NsId(16), 1)];
        cell.sync_namespaces(&ks, &mut releases);
        assert!(cell.namespaces.is_empty());
        assert_eq!(cell.free_lanes, vec![(inf_runtime::MAX_SLOT >> 8, 7)]);
        // The next tiered namespace takes the recycled lane — pre-fix this
        // call took lane `MAX_SLOT >> 8 + 1` and panicked.
        cell.create_ns(NsId(17), &spec());
        let second = &cell.namespaces[0];
        assert_eq!(second.lane, inf_runtime::MAX_SLOT >> 8);
        assert_eq!(second.round_seq, 7, "the round sequence continues past the last holder's");
        assert!(cell.free_lanes.is_empty());
        assert_eq!(cell.next_lane, (inf_runtime::MAX_SLOT >> 8) + 1, "no fresh lane was minted");
    }

    /// A drained round returns its lane too (the `round_drain` path).
    #[test]
    fn drained_namespace_lane_recycles_at_maintain() {
        let mut cell = TierCell::new(MemFs::new(), 0, PathBuf::from("/shard-0"));
        cell.create_ns(NsId(16), &spec());
        cell.namespaces[0].round_seq = 3;
        // Park a finished round on it so the drop routes through the drain.
        cell.namespaces[0].round = Some(FlushRound::new(0, 3, 0));
        let ks = Keyspace::new(StoreConfig::default());
        let mut releases = vec![(NsId(16), 1)];
        cell.sync_namespaces(&ks, &mut releases);
        assert_eq!(cell.round_drain.len(), 1, "parked until every op is terminal");
        assert!(cell.free_lanes.is_empty(), "not recycled while parked");
        cell.maintain_teardown(|_| false);
        assert!(cell.round_drain.is_empty());
        assert_eq!(cell.free_lanes, vec![(0, 3)]);
        cell.create_ns(NsId(17), &spec());
        assert_eq!(cell.namespaces[0].lane, 0);
        assert_eq!(cell.namespaces[0].round_seq, 3);
    }
}
