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
//! Drive shape (M4.5-S31, ADR-0084; the groups and the turn of ADR-0178
//! D3 and ADR-0155 D4): the flush/seal legs ride the **reactor drive**.
//! In every MAINTAIN each namespace's round first advances on the
//! completions REAP recorded, then the cell's one turn runs: the **plan
//! pass** stages one round on the cell — the first namespace after the
//! last planned one, in id order, that is idle with work at its cursor
//! — and the **issue pass** offers each ready namespace's next
//! **group** (the longest run of its unsent ops within
//! [`inf_log::limits::TIER_GROUP_BYTES_MAX`] device bytes and
//! [`inf_log::limits::TIER_GROUP_OPS_MAX`] ops, whole writes) to the
//! device budget at its exact size; only a granted group reaches the
//! driver, and a refused one makes its namespace the holder, which ends
//! the pass for the tier class and is visited first next turn. Barriers
//! are their own group, offered once every write is done; a round's
//! effects (`advance_flushed`, seal catalog commits, gap crossings)
//! apply at its last barrier completion, never at submission. REAP only
//! records completions ([`TierCell::on_flush_completion`]); MAINTAIN
//! advances the state machine — single-writer state, deterministic
//! under DST. Cold reads are fully async from day one: intents queue on
//! [`ColdReads`], drain once per reactor iteration into
//! `IoOp::TierRead`, and complete through the custody table, each
//! window planned below its file's [`ReadableLen`].

use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::rc::Rc;

use inf_alloc::AlignedPool;
use inf_foundation::LogHistogram;
use inf_foundation::bounded::CapCensus;
use inf_log::blob::ExtentId;
use inf_log::flush::{PendingSealView, unlink_tier_file};
use inf_log::fs::{SegmentFile, SegmentFs};
use inf_log::limits::{TIER_GROUP_BYTES_MAX, TIER_GROUP_OPS_MAX};
use inf_log::{NsId, OpState, TierDrive, TierFileMeta, TierFlush, TierFlushConfig, TierFlushError};
use inf_runtime::{
    ColdReadConfig, ColdReads, CompletionToken, IoOp, Issue, TierFileId, TokenClass, WaitList,
    WriteBarrier,
};
use inf_store::{Keyspace, LogicalAddr, TierSpec, TieredTable};

use crate::durable::ack_class::AckClasses;
use crate::limits::{TIER_ISSUE_OPS_PER_TURN, TIER_PLANS_PER_TURN};

mod marks;
mod round;

pub(crate) use marks::EpochMarks;
use round::{FlushRound, RoundPhase, unsent};

/// `errno` values the completion handler classifies (ADR-0084 D4).
const ENOSPC: i32 = 28;
const EIO: i32 = 5;

/// Cold-read pool window: 4 tier frames (16 KiB) per buffer — covers a
/// typical record in one read; oversized records stage through chunked
/// continuation windows (the S08 `cold_hardened` shape).
pub(crate) const COLD_POOL_BUF: usize = 4 * inf_log::TIER_FRAME_BYTES;

// The window maximum fits one driver op, so `ColdReads::with_config`'s pool
// width check holds for this pool by construction (ADR-0167 D1).
const _: () = assert!(
    COLD_POOL_BUF as u64 <= inf_foundation::limits::DRIVER_OP_BYTES_MAX,
    "a cold-read pool buffer is one driver op"
);

/// The most driver ops one flush round can carry (the ADR-0084 D3
/// token op-index bound) — the tier-flush class's ops slice for the
/// device budget (ADR-0178 D2).
pub(crate) const TIER_ROUND_MAX_OPS: u64 = 256;

// A group is at most the tier class's declared slice on both axes
// (ADR-0178 T1), so no tier offer is above the class cap: on the byte
// axis the declared slice is `TIER_GROUP_BYTES_MAX` itself (one const,
// read by the durable cell's class slices); on the ops axis this
// assertion ties the group to the round's op bound.
const _: () =
    assert!(TIER_GROUP_OPS_MAX as u64 <= TIER_ROUND_MAX_OPS, "a group fits the ops slice");
// The plan permit is one value per MAINTAIN: the count the type carries.
const _: () = assert!(TIER_PLANS_PER_TURN == 1, "one plan permit per turn");

/// The device budget's answer to "may this group of `bytes` device bytes
/// and `ops` ops issue now?" (ADR-0178 D1's [`Issue`]). Every group is
/// offered at its exact size, so nothing is settled afterwards. Generic,
/// not `dyn`: one monomorphized closure per plane. `None` = no durable
/// plane (the MemFs test tier) — always `Now`, nothing metered.
pub(crate) trait FlushAdmission {
    fn admit(&mut self, bytes: u64, ops: u64) -> Issue;
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
    /// visit of the namespace while non-empty, whatever its flush leg
    /// returned, so the typed timeout always fires.
    pub stall_waiters: WaitList<()>,
    /// The extent-reclaim marks, one per ack class, and the reclaim epoch
    /// they yield: ADR-0061 D5's gate input, capped (ADR-0163 D2).
    epoch_marks: EpochMarks,
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

/// Cell-scope reactor-drive flush observables (ADR-0084 D6 — the
/// collapse must be visible from INFO), the turn's counters among them
/// (ADR-0178 D5).
#[derive(Default)]
pub(crate) struct TierFlushStats {
    pub rounds: u64,
    pub write_retries: u64,
    pub stale_completions: u64,
    pub round_us: LogHistogram,
    /// Groups the device budget refused (`NotThisSlice`) — the tier-flush
    /// face of `io_budget_deferrals_tier_flush`, per offer.
    pub groups_deferred: u64,
    /// The longest wait of one ready group from the moment it became
    /// ready to its grant, the pending one included, on the injected
    /// clock in microseconds (the fair turn's progress bound, `n × T_c`).
    pub group_wait_us_max: u64,
    /// Refusal episodes: a namespace's group refused while it was not
    /// already the holder.
    pub turn_waits: u64,
    /// Turns in which two or more namespaces held a ready group.
    pub turn_contended: u64,
    /// Turns a namespace idle with work at its cursor was passed over
    /// because the cell's one plan went to another, per namespace.
    pub plan_waits: u64,
    /// Plans the pass made — a span, a gap or a refusal — one per turn at
    /// most.
    pub rounds_planned: u64,
    /// Plans refused because a namespace's file ids are spent: its flush
    /// stops for the lifetime, and each turn that would plan it again
    /// counts one.
    pub file_ids_exhausted: u64,
}

/// A tier file's readable length: the data bytes below which a cold-read
/// window may be planned. Made
/// only by a sealed file's length, a staged seal's confirmed prefix, or
/// the active file's confirmable length (ADR-0056 D5) — never by a
/// staged length, which moves at stage time while its frames are still
/// unwritten or in flight.
#[derive(Copy, Clone, Debug)]
pub(crate) struct ReadableLen(u64);

impl ReadableLen {
    /// A sealed file: its whole data length, footer-terminated.
    fn sealed(meta: &TierFileMeta) -> ReadableLen {
        ReadableLen(meta.data_len)
    }

    /// A file whose seal is staged: the prefix its last completed barrier
    /// covered.
    fn confirmed(seal: &PendingSealView) -> ReadableLen {
        ReadableLen(seal.confirmed_len)
    }

    /// The active file: the last full frame boundary its barriers
    /// covered; `None` with no active file.
    fn active<F: SegmentFs>(flush: &TierFlush<F>) -> Option<ReadableLen> {
        // A planted canary: the active file's window planned from its
        // staged length, the rule this type exists to refuse (the cold
        // read below the confirmable length while a round is held must
        // go red).
        if cfg!(inf_canary_cold_window_staged_len) {
            return flush.active().map(|(_, _, data_len, _, _)| ReadableLen(data_len));
        }
        flush.active_confirmable_len().map(ReadableLen)
    }

    fn bytes(self) -> u64 {
        self.0
    }
}

impl<F: SegmentFs> TierNs<F> {
    /// Cold-read window plan for `addr`: `(fd, file, disk offset, frame
    /// count skip)` — the caller turns it into a `ColdReads::enqueue`.
    /// Returns `None` when the address is not inside any catalogued
    /// file's readable range (a displaced-then-retired address raced the
    /// lookup; the caller re-resolves — never an error).
    pub fn plan_cold_read(
        &self,
        addr: LogicalAddr,
        len: usize,
    ) -> Option<(std::os::fd::RawFd, TierFileId, u64, u64, usize)> {
        self.plan_cold_window(addr, len, false)
    }

    /// [`plan_cold_read`](Self::plan_cold_read) for a caller that needs
    /// only the record's first `len` bytes (`SCAN`'s key resolution): the
    /// window is the frames that cover them, not the whole pool buffer —
    /// a quarter of the bytes per key, and windows of neighbouring
    /// records overlap or touch, so the cold drain merges them (ADR-0055
    /// D4; review of 2026-08-30, F-L17-13).
    pub fn plan_cold_prefix(
        &self,
        addr: LogicalAddr,
        len: usize,
    ) -> Option<(std::os::fd::RawFd, TierFileId, u64, u64, usize)> {
        self.plan_cold_window(addr, len, true)
    }

    fn plan_cold_window(
        &self,
        addr: LogicalAddr,
        len: usize,
        prefix: bool,
    ) -> Option<(std::os::fd::RawFd, TierFileId, u64, u64, usize)> {
        let raw = addr.to_raw();
        let locate =
            |base: u64, readable: ReadableLen| raw >= base && raw < base + readable.bytes();
        // Sealed catalog first (ascending by base; linear scan — a cell
        // holds tens to hundreds of files and this runs per cold miss,
        // not per command).
        for meta in self.flush.sealed() {
            let readable = ReadableLen::sealed(meta);
            if locate(meta.base.to_raw(), readable) {
                let handle = self.files.iter().find(|(id, _)| *id == meta.id)?;
                let fd = handle.1.raw_fd()?;
                return Some(Self::window(
                    fd,
                    meta.id,
                    meta.base.to_raw(),
                    readable,
                    raw,
                    len,
                    prefix,
                ));
            }
        }
        // Files whose seal is staged but not completion-committed
        // (M4.5-S31): the confirmed prefix stays servable through the
        // held handle — the window is clamped to it (frames past it may
        // still be in flight).
        for pending in self.flush.pending_seals() {
            let readable = ReadableLen::confirmed(&pending);
            if locate(pending.base.to_raw(), readable) {
                let fd = pending.fd?;
                return Some(Self::window(
                    fd,
                    pending.id,
                    pending.base.to_raw(),
                    readable,
                    raw,
                    len,
                    prefix,
                ));
            }
        }
        // The active file: located and windowed by its readable length,
        // never by the staged one, so a window never reaches frames a
        // held round has not written.
        let (id, base, _, _, _) = self.flush.active()?;
        let readable = ReadableLen::active(&self.flush)?;
        if locate(base.to_raw(), readable) {
            let fd = self.flush.active_raw_fd()?;
            return Some(Self::window(fd, id, base.to_raw(), readable, raw, len, prefix));
        }
        None
    }

    fn window(
        fd: std::os::fd::RawFd,
        id: u32,
        base: u64,
        readable: ReadableLen,
        raw: u64,
        len: usize,
        prefix: bool,
    ) -> (std::os::fd::RawFd, TierFileId, u64, u64, usize) {
        let (first, span, skip) = inf_log::tier_frame_span(raw - base, len.max(1));
        let file_frames = readable.bytes().div_ceil(inf_log::TIER_FRAME_DATA as u64);
        let pool_frames = (COLD_POOL_BUF / inf_log::TIER_FRAME_BYTES) as u64;
        let want = if prefix { pool_frames.min(u64::from(span)) } else { pool_frames };
        let window_frames = want.min(file_frames - first);
        debug_assert!(window_frames > 0, "cold window inside the file's readable range");
        (fd, TierFileId::new(id), inf_log::tier_frame_offset(first), window_frames, skip)
    }

    /// Whether this namespace's round holds a group it may offer.
    fn has_ready_group(&self) -> bool {
        self.round.as_ref().is_some_and(|r| r.has_ready_group(&self.flush))
    }
}

/// The cell's one turn (ADR-0178 D3's tier row, D4): which namespace
/// plans, and which offers next. Ids, never indices, so a drop repairs
/// nothing: a dropped `served_last` or `planned_last` is passed in the
/// order (the next visit or plan goes to the first id above it), and a
/// dropped holder releases the turn (`holder` ← none at the drop), so
/// the next pass starts after the namespace served last.
#[derive(Default)]
struct TierTurn {
    /// The namespace whose group the budget refused last: it ends the
    /// pass for the tier class and is visited first on the next (ADR-0178
    /// T3). Cleared by its grant, or by its drop.
    holder: Option<NsId>,
    /// The namespace whose group was granted last: the next issue pass
    /// starts after it when there is no holder.
    served_last: Option<NsId>,
    /// The namespace that planned last: the next plan goes to the first
    /// idle namespace with work after it, in id order, wrapping (ADR-0178
    /// T13).
    planned_last: Option<NsId>,
}

/// One MAINTAIN's permit to plan: made once per turn, consumed by the
/// one planning call, so the cell plans at most
/// [`TIER_PLANS_PER_TURN`] rounds a turn whatever its namespace count.
pub(crate) struct PlanPermit(());

/// A run of one round's unsent ops the device budget granted (ADR-0178
/// T2): made only by a `Now` answer to an offer of its exact size,
/// consumed by the emit, which pushes exactly these ops.
pub(crate) struct GrantedGroup {
    first: usize,
    end: usize,
    ops: usize,
}

/// A candidate group: the longest run of unsent ops from the round's
/// cursor inside the phase's range that fits the group bounds and the
/// turn's remaining descriptor visits; `visits` is what building it
/// cost — its ops, and the first unsent op that did not fit.
struct Group {
    first: usize,
    end: usize,
    ops: usize,
    bytes: u64,
    visits: usize,
}

/// The cell's tiered plane state. Constructed by `enable_durable` (a
/// tiered namespace is a configuration of `MODE durable` — ADR-0062
/// D1); populated lazily as tiered namespaces materialize.
pub(crate) struct TierCell<F: SegmentFs> {
    fs: F,
    cell: u32,
    shard_dir: PathBuf,
    /// The cell's tiered namespaces, ascending by id: the turn's order.
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
    /// files join the teardown queue (ADR-0084 D3 custody). No further
    /// group is offered for a parked round.
    round_drain: Vec<TierNs<F>>,
    /// Reactor-drive flush observables (ADR-0084 D6).
    pub(crate) flush_stats: TierFlushStats,
    /// The cell's turn: one per cell, the two cell-wide orders.
    turn: TierTurn,
    /// The cell's cap census: each namespace's reclaim marks register
    /// their row on it.
    census: Rc<CapCensus>,
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

    /// The index at which the rotation of `namespaces` (ascending by id)
    /// begins for the first id above `after`, wrapping; 0 with no
    /// namespace, no `after`, or none above it.
    fn rotation_start(&self, after: Option<NsId>) -> usize {
        let n = self.namespaces.len();
        if n == 0 {
            return 0;
        }
        after.map_or(0, |after| self.namespaces.partition_point(|t| t.ns.0 <= after.0) % n)
    }
}

impl<F: SegmentFs + Clone> TierCell<F> {
    pub fn new(fs: F, cell: u32, shard_dir: PathBuf, census: Rc<CapCensus>) -> TierCell<F> {
        TierCell {
            fs,
            cell,
            shard_dir,
            namespaces: Vec::new(),
            cold: None,
            ram_hit_us: LogHistogram::new(),
            cold_us: LogHistogram::new(),
            teardown: Vec::new(),
            next_lane: 0,
            free_lanes: Vec::new(),
            round_drain: Vec::new(),
            flush_stats: TierFlushStats::default(),
            turn: TierTurn::default(),
            census,
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
            && live.iter().all(|(id, _)| self.namespaces.iter().any(|t| t.ns == *id))
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
            // A dropped holder releases the turn: no grant of its can
            // clear it, and the next pass starts after the one served last.
            if self.turn.holder == Some(gone.ns) {
                self.turn.holder = None;
            }
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

    /// Builds `ns`'s pipeline and inserts it at its id's position, so the
    /// turn's rotation is the vector's order. Returns that index.
    fn create_ns(&mut self, ns: NsId, spec: &TierSpec) -> usize {
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
                record_len_max: spec.record_len_max().unwrap_or(spec.maintain_slice_bytes),
            },
            0,
            &self.census,
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
        // The marks' one allocation, sized here and never grown. A refusal
        // is the allocation fail-stop each growth of this constructor takes;
        // their layout is a const assertion, and the census refuses only a
        // cap name declared with a second value, which a test's first boot
        // shows.
        let epoch_marks = EpochMarks::new(&self.census)
            .unwrap_or_else(|refused| panic!("tiered namespace reclaim marks refused: {refused}"));
        let at = self.namespaces.partition_point(|t| t.ns.0 < ns.0);
        self.namespaces.insert(
            at,
            TierNs {
                ns,
                flush,
                files: Vec::new(),
                retired: Vec::new(),
                tail_stall_timeout_ms: spec.tail_stall_timeout_ms,
                io_mode: spec.tier_io_mode,
                stall_waiters: WaitList::new(),
                epoch_marks,
                compact_inflight: false,
                dir,
                lane,
                round_seq,
                round: None,
                release_epoch: 0,
            },
        );
        at
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
        let at = self.create_ns(ns, spec);
        let t = &mut self.namespaces[at];
        t.flush = flush;
        // Recovery built the pipeline on the seam (boot, pre-serving);
        // its flush work from here rides the driver (ADR-0084 D1).
        t.flush.set_drive(TierDrive::Reactor);
        t.files = files;
    }

    /// The MAINTAIN drivers for one namespace, bounded per slice: the
    /// demote seal, the round's advance on its completions (no offer:
    /// the turn makes those, [`TierCell::turn`]), the release, the stall
    /// wake, the admission cadence, extent reclaim, the retirement
    /// unlink and compaction. Returns `(budget units used, compaction
    /// read to issue)` — the caller charges Maintenance and spawns the
    /// read chain (it owns the executor; this module owns no
    /// `Rc<Shared>`). `durable` is the cell's durable plane, read in this
    /// step for the extent-reclaim marks: its staged seq, its watermark and
    /// the ack class of a seq (ADR-0061 D5).
    pub fn maintain_ns(
        &mut self,
        ks: &mut Keyspace,
        at: usize,
        durable: Option<&impl AckClasses>,
        transition_idle: bool,
        now_us: u64,
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
        // advance the in-flight round on the completions REAP recorded
        // and apply its effects at the last barrier CQE — the reactor
        // never waits on the device here. Staging and issuing are the
        // turn's, after every namespace was visited.
        let sealed = if table.demote_due() { table.seal_slice() } else { 0 };
        let advanced = advance_round(table, t, stats, now_us);
        let released = table.release_slice();
        if (sealed | released) > 0 {
            units += 1 + ((sealed + released) / 4096) as u32;
        }
        // Head advanced ⇒ ring space may have freed: wake stalled
        // writers. Also wake while any are parked so the typed timeout
        // is always reachable (bounded: waiters re-check, then repark),
        // whatever the flush leg returned — a fatal barrier stops the
        // cell after this visit, never before the wake.
        if released > 0 || t.stall_waiters.any_waiting() {
            t.stall_waiters.wake_all(());
        }
        advanced?;

        // ---- admission cadence (ADR-0063 D2): both usage halves are
        // fresh exactly here (post-flush).
        table.refresh_disk_admission(t.flush.disk_bytes());

        // ---- extent reclaim epoch (ADR-0061 D5): a mark drains once
        // the fsync watermark covers its seq.
        if let Some(durable) = durable {
            t.epoch_marks.tick(table.wal_epoch(), durable);
        }
        units += reclaim_extents(&self.fs, table, t);
        units += unlink_retired(&self.fs, cold.as_ref(), t);

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

    /// The cell's turn, once per MAINTAIN after every namespace's advance:
    /// the plan pass under this turn's one permit, then the issue pass.
    /// Returns the record bytes the plan staged (the unit-charge basis).
    /// A fatal plan error (an unaddressable write position, ADR-0167 D4)
    /// returns before the issue pass; a refused file creation is returned
    /// after it, the namespace retrying at its next turn in the order.
    ///
    /// # Errors
    /// The plan pass's: file-creation I/O (not fatal) or an unaddressable
    /// position (fatal).
    pub fn turn(
        &mut self,
        ks: &mut Keyspace,
        now_us: u64,
        ops: &mut Vec<IoOp>,
        admission: &mut impl FlushAdmission,
    ) -> Result<u64, TierFlushError> {
        let permit = PlanPermit(());
        let (staged, planned) = self.plan_pass(ks, now_us, permit);
        if let Err(err) = &planned
            && err.is_fatal()
        {
            return planned.map(|()| staged);
        }
        self.issue_pass(now_us, ops, admission);
        planned.map(|()| staged)
    }

    /// The plan pass (ADR-0178 T13): the first namespace after `planned_last`, in
    /// id order, wrapping, that is idle with work at its cursor stages
    /// its round — the cell's one preparation this turn, which consumes
    /// the permit; every other idle namespace with work is counted
    /// passed over. An effects-only round (a gap at a file boundary)
    /// applies now; a refused creation that staged nothing leaves the
    /// namespace idle, the permit spent.
    fn plan_pass(
        &mut self,
        ks: &mut Keyspace,
        now_us: u64,
        permit: PlanPermit,
    ) -> (u64, Result<(), TierFlushError>) {
        let PlanPermit(()) = permit;
        let n = self.namespaces.len();
        let start = self.rotation_start(self.turn.planned_last);
        let stats = &mut self.flush_stats;
        let mut planned = false;
        let mut staged = 0u64;
        let mut result = Ok(());
        for i in 0..n {
            let t = &mut self.namespaces[(start + i) % n];
            if t.round.is_some() {
                continue;
            }
            let Some(table) = ks.tiered_store_mut(t.ns) else { continue };
            if !table.flush_pending(&t.flush) {
                continue;
            }
            // A planted canary: every idle namespace with work plans in
            // this same turn — the one-plan rule the permit carries
            // (the eight-namespace leg's per-iteration count must read
            // two or more).
            if planned && !cfg!(inf_canary_tier_plan_every_namespace) {
                stats.plan_waits += 1;
                continue;
            }
            planned = true;
            self.turn.planned_last = Some(t.ns);
            stats.rounds_planned += 1;
            // The plan parses every position, reserves its windows and
            // creates its file before it stages anything (ADR-0155 D2,
            // ADR-0167 A1): a refusal leaves no round behind.
            match table.stage_flush_round(&mut t.flush) {
                Ok(bytes) => staged = bytes,
                Err(err) => {
                    debug_assert!(!t.flush.round_active(), "a refused plan staged nothing");
                    if matches!(err, TierFlushError::FileIdsExhausted { .. }) {
                        stats.file_ids_exhausted += 1;
                    }
                    if err.is_fatal() {
                        return (staged, Err(err));
                    }
                    result = Err(err);
                    continue;
                }
            }
            if t.flush.round_op_count() == 0 {
                // Effects-only round (a gap at a file boundary): nothing to
                // wait on — the crossing applies now, as the seam drive did.
                let (_, sealed) = table.complete_flush_round(&mut t.flush);
                t.files.extend(sealed);
                stats.rounds += 1;
            } else {
                open_round(t, now_us);
            }
        }
        (staged, result)
    }

    /// The issue pass (ADR-0178 T3): from the holder, else after the
    /// namespace served last, in id order, wrapping, each namespace with
    /// a ready group offers its next group at its exact device size; a
    /// grant pushes it and the visit moves on, a refusal makes its
    /// namespace the holder and ends the pass. Rotations repeat while a
    /// group was issued and the turn's descriptor visits last; the pass
    /// ends where they are spent (`TIER_ISSUE_OPS_PER_TURN`'s crossing).
    fn issue_pass(
        &mut self,
        now_us: u64,
        ops: &mut Vec<IoOp>,
        admission: &mut impl FlushAdmission,
    ) {
        let n = self.namespaces.len();
        if n == 0 {
            return;
        }
        let mut start = match self.turn.holder {
            Some(holder) => self.namespaces.partition_point(|t| t.ns.0 < holder.0) % n,
            None => self.rotation_start(self.turn.served_last),
        };
        // A planted canary: the visit starts at index 0 and continues
        // past a refusal — the index-order visit the turn replaces (the
        // fair-turn oracle's wait bound must go red).
        if cfg!(inf_canary_tier_turn_index_order) {
            start = 0;
        }
        self.note_ready_groups(now_us);
        let mut visits_left = TIER_ISSUE_OPS_PER_TURN;
        loop {
            let mut issued = false;
            for i in 0..n {
                let Some(left) = NonZeroUsize::new(visits_left) else {
                    return; // the turn's visits are spent: the pass ends
                };
                let t = &mut self.namespaces[(start + i) % n];
                match visit_namespace(
                    &mut self.flush_stats,
                    &mut self.turn,
                    t,
                    left,
                    now_us,
                    ops,
                    admission,
                ) {
                    Visit::Idle => {}
                    Visit::Issued { visits } => {
                        visits_left -= visits;
                        issued = true;
                    }
                    Visit::Refused { visits } if cfg!(inf_canary_tier_turn_index_order) => {
                        visits_left -= visits;
                    }
                    Visit::Refused { .. } => return,
                }
            }
            if !issued {
                return;
            }
        }
    }

    /// Reads every ready group's wait into the gauge (the pending ones
    /// included) and counts the turn as contended when two or more
    /// namespaces hold one.
    fn note_ready_groups(&mut self, now_us: u64) {
        let mut ready = 0u32;
        for t in &self.namespaces {
            if !t.has_ready_group() {
                continue;
            }
            ready += 1;
            if let Some(since) = t.round.as_ref().and_then(|r| r.ready_since_us) {
                self.flush_stats.group_wait_us_max =
                    self.flush_stats.group_wait_us_max.max(now_us.saturating_sub(since));
            }
        }
        if ready >= 2 {
            self.flush_stats.turn_contended += 1;
        }
    }

    /// Bounded teardown slices for dropped namespaces (ADR-0062 D7).
    /// Unlinks route through the injected fs (L7) and failure only
    /// defers disk space — the boot orphan sweep re-drives it.
    /// Also advances parked rounds of dropped namespaces (ADR-0084 D3):
    /// once every op is terminal, the windows recycle and the files
    /// join the teardown queue; a parked round's unsent ops are never
    /// sent. A queue whose catalog persist is not yet durable
    /// (`persisted(release_epoch)` false — ADR-0100 D5) waits: a cut
    /// before the swap must find every file.
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
        let sent = t.flush.round_op_state(index) == Some(OpState::Sent);
        if round.round_seq != token.generation() || !sent {
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
        let completed = t.flush.complete_op(index, errno.is_some());
        debug_assert!(completed, "a sent op completes");
        match errno {
            None => {}
            Some(e) if is_barrier => round.fatal = Some(e),
            // ENOSPC dominates: it must reach the admission latch.
            Some(e) if e == ENOSPC || round.write_error.is_none() => round.write_error = Some(e),
            Some(_) => {}
        }
    }

    /// The cell's tier window pools, live and draining: the capacity
    /// reserved at their builds and the bytes of the windows written at
    /// least once (`tier_window_capacity_bytes`, `tier_window_written_bytes`).
    pub(crate) fn window_bytes(&self) -> (u64, u64) {
        let pipelines = self.namespaces.iter().chain(&self.round_drain).map(|t| &t.flush);
        pipelines.fold((0, 0), |(capacity, written), flush| {
            (capacity + flush.window_capacity_bytes(), written + flush.window_written_bytes())
        })
    }

    /// Reactor-drive flush rounds currently in flight (INFO gauge).
    pub(crate) fn flush_rounds_inflight(&self) -> u64 {
        let live = self.namespaces.iter().filter(|t| t.round.is_some()).count();
        let draining = self.round_drain.len();
        (live + draining) as u64
    }
}

/// How one namespace's visit in the issue pass ended: no ready group, so
/// no offer; its group granted and pushed; or its group refused. The
/// visits a group cost come back with it, so the pass charges what the
/// walk read.
enum Visit {
    Idle,
    Issued { visits: usize },
    Refused { visits: usize },
}

/// One namespace's visit in the issue pass: its next group, if its round
/// holds one, offered at its exact size and emitted on a grant. The
/// round and the group come from one borrow, so a visit of a namespace
/// with no round is `Idle` by construction, never a claim on the caller.
fn visit_namespace<F: SegmentFs>(
    stats: &mut TierFlushStats,
    turn: &mut TierTurn,
    t: &mut TierNs<F>,
    visits_left: NonZeroUsize,
    now_us: u64,
    ops: &mut Vec<IoOp>,
    admission: &mut impl FlushAdmission,
) -> Visit {
    let Some(round) = t.round.as_mut() else { return Visit::Idle };
    let Some(group) = next_group(&t.flush, round, visits_left) else { return Visit::Idle };
    // A planted canary: a retry group reaches the driver with no
    // offer — the grant every emit needs (the accounting oracle's
    // equality must go red on the retry arm).
    let granted = if cfg!(inf_canary_tier_retry_ungranted) && round.retry {
        Some(GrantedGroup { first: group.first, end: group.end, ops: group.ops })
    } else {
        offer_group(admission, &group)
    };
    let Some(granted) = granted else {
        stats.groups_deferred += 1;
        if turn.holder != Some(t.ns) {
            stats.turn_waits += 1;
        }
        turn.holder = Some(t.ns);
        return Visit::Refused { visits: group.visits };
    };
    if let Some(since) = round.ready_since_us {
        stats.group_wait_us_max = stats.group_wait_us_max.max(now_us.saturating_sub(since));
    }
    emit_group(&mut t.flush, round, t.lane, granted, ops);
    round.ready_since_us = round.has_ready_group(&t.flush).then_some(now_us);
    turn.holder = None;
    turn.served_last = Some(t.ns);
    Visit::Issued { visits: group.visits }
}

/// The extent-reclaim leg of one namespace's MAINTAIN (ADR-0061 D5,
/// ADR-0096 D2/D4): at most [`EXTENT_RECLAIM_PER_SLICE`] candidates, each
/// unlinked, quarantined or deferred by its origin. Returns the units.
fn reclaim_extents<F: SegmentFs>(fs: &F, table: &mut TieredTable, t: &TierNs<F>) -> u32 {
    /// How one candidate's disposal ended (drives the store ack).
    enum Disposal {
        Unlinked,
        Quarantined,
    }
    let mut units = 0;
    let reclaim =
        table.extent_reclaim_work(t.epoch_marks.durable_epoch(), EXTENT_RECLAIM_PER_SLICE);
    for candidate in reclaim {
        units += 1;
        let extent_id = candidate.extent_id;
        let id = ExtentId(extent_id);
        let outcome = match candidate.origin {
            // Refcount-proven death: the count is the proof
            // (ADR-0061 D5) — unlink as always.
            inf_store::ReclaimOrigin::Death => {
                inf_log::blob::unlink_extent_file(fs, &t.dir, id).map(|()| Disposal::Unlinked)
            }
            // Boot orphan (ADR-0096 D2): probe the header first. A
            // file that is not a well-formed extent of this id is
            // garbage nothing can reference — unlink. A verifying
            // header quarantines by rename: the bytes survive the
            // life, and a later boot delivers the second verdict.
            inf_store::ReclaimOrigin::BootOrphan => {
                let path = t.dir.join("cold").join(inf_log::extent_file_name(id));
                match inf_log::probe_extent_file(fs, &path) {
                    Ok(header) if header.extent_id == id => {
                        inf_log::blob::quarantine_extent_file(fs, &t.dir, id)
                            .map(|()| Disposal::Quarantined)
                    }
                    Ok(_) => inf_log::blob::unlink_extent_file(fs, &t.dir, id)
                        .map(|()| Disposal::Unlinked),
                    Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                        inf_log::blob::unlink_extent_file(fs, &t.dir, id)
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
                inf_log::blob::unlink_quarantined_file(fs, &t.dir, id).map(|()| Disposal::Unlinked)
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
    units
}

/// The retirement-unlink leg: a file with in-flight cold reads is never
/// deleted, so the pins drain first; bounded by [`UNLINKS_PER_SLICE`].
/// Returns the units.
fn unlink_retired<F: SegmentFs>(fs: &F, cold: Option<&ColdReads>, t: &mut TierNs<F>) -> u32 {
    let mut unlinked = 0;
    let mut u = 0;
    while u < t.retired.len() && unlinked < UNLINKS_PER_SLICE {
        let pinned = cold.is_some_and(|c| c.inflight_on(TierFileId::new(t.retired[u].id)) > 0);
        if pinned {
            u += 1;
            continue;
        }
        let meta = t.retired.remove(u);
        t.files.retain(|(id, _)| *id != meta.id);
        let _ = unlink_tier_file(fs, &meta); // non-fatal: re-queued by boot GC
        unlinked += 1;
    }
    unlinked as u32
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

/// Advances one namespace's round on the completions REAP recorded
/// (M4.5-S31, ADR-0084 D2/D4), making no offer: completes a round whose
/// barriers all landed, escalates a barrier failure to the fatal class
/// (fsync failure is fail-stop, ADR-0056 D4), turns failed writes unsent
/// again byte-identical once every
/// sent op is terminal (the M4-S21 retained-batch retry — ENOSPC latches
/// admission first; the retry group is offered at the turn), or enters
/// the barrier phase once every write is done.
fn advance_round<F: SegmentFs>(
    table: &mut TieredTable,
    t: &mut TierNs<F>,
    stats: &mut TierFlushStats,
    now_us: u64,
) -> Result<(), TierFlushError> {
    let Some(round) = &mut t.round else { return Ok(()) };
    if let Some(errno) = round.fatal {
        // A failed durability barrier freezes the watermark exactly
        // where the last good round left it (ADR-0056 D4).
        // fsync-fail-stop-allow: reactor-drive flush barrier: constructs and returns to
        // maintain_ns, which routes it to DurableCell::fail_stop (ADR-0084 D4)
        return Err(TierFlushError::Fsync {
            path: t.dir.join("cold"),
            source: std::io::Error::from_raw_os_error(errno),
        });
    }
    let (writes, op_count) = (t.flush.round_write_count(), t.flush.round_op_count());
    match round.phase() {
        RoundPhase::Writes => {
            if round.pending > 0 && !cfg!(inf_canary_tier_barrier_early) {
                return Ok(());
            }
            if round.pending == 0
                && let Some(errno) = round.write_error.take()
            {
                if errno == ENOSPC {
                    table.note_flush_device_full();
                }
                stats.write_retries += 1;
                let requeued = t.flush.retry_failed_writes();
                debug_assert!(requeued > 0, "a write error leaves a failed write");
                // The failed writes lie below the old cursor: the cursor
                // is placed from the round's start, at the first of them.
                round.place_cursor(0, &t.flush);
                round.ready_since_us = Some(now_us);
                round.retry = true;
                return Ok(());
            }
            let state = |index| t.flush.round_op_state(index);
            let all_done = (0..writes).all(|index| state(index) == Some(OpState::Done));
            // A planted canary: the barrier group is offered once every
            // write is sent, done or not — the write-then-barrier order
            // ADR-0178 T4 keeps (the cut's boot must refuse a tier frame
            // the device never held below the manifested watermark).
            let all_sent = (0..writes).all(|index| state(index) != Some(OpState::Unsent));
            if all_done || (cfg!(inf_canary_tier_barrier_early) && all_sent) {
                round.enter_barriers(&t.flush);
                round.ready_since_us = Some(now_us);
                round.retry = false;
            }
            Ok(())
        }
        RoundPhase::Barriers => {
            let all_done = (writes..op_count)
                .all(|index| t.flush.round_op_state(index) == Some(OpState::Done));
            if round.pending > 0 || !all_done {
                return Ok(());
            }
            let round = t.round.take().expect("matched above");
            // The completion hands back each file it sealed, with the
            // creation-mode handle cold reads reuse (ADR-0054).
            let (_, sealed) = table.complete_flush_round(&mut t.flush);
            t.files.extend(sealed);
            stats.rounds += 1;
            stats.round_us.record(now_us.saturating_sub(round.staged_at_us));
            Ok(())
        }
    }
}

/// Opens the staged round: its plan parsed every write position and
/// reserved every window before the stage (ADR-0167 A1), so opening it
/// cannot fail and checks nothing. Nothing is pushed: the round's groups
/// are offered at the turn.
fn open_round<F: SegmentFs>(t: &mut TierNs<F>, now_us: u64) {
    t.round_seq = t.round_seq.wrapping_add(1);
    t.round = Some(FlushRound::new(t.round_seq, now_us));
}

/// Builds the round's next group: the longest run of unsent ops from the
/// cursor inside the phase's range with at most `TIER_GROUP_BYTES_MAX`
/// device bytes and `TIER_GROUP_OPS_MAX` ops, whole ops, within
/// `visits_left` descriptor visits; `None` when the phase holds no
/// unsent op (no ready group). A visit is an unsent op's descriptor read
/// — the group's ops and the first that did not fit, as
/// `TIER_ISSUE_OPS_PER_TURN` defines it; a sent, done or failed op between
/// two unsent ones costs one state read and no visit, and the cursor
/// rule keeps such runs out of the walk's start, so a turn's walk over a
/// namespace reads each of its round's states at most once (bounded by
/// `TIER_ROUND_MAX_OPS`). At least one visit is left, so the first unsent
/// op always starts the group: a single write fits one (the assertion in
/// `inf_log::limits`).
fn next_group<F: SegmentFs>(
    flush: &TierFlush<F>,
    round: &FlushRound,
    visits_left: NonZeroUsize,
) -> Option<Group> {
    let (writes, op_count) = (flush.round_write_count(), flush.round_op_count());
    let first = round.first_unsent(flush)?;
    let mut group = Group { first, end: first, ops: 0, bytes: 0, visits: 0 };
    for index in first..round.phase_range(writes, op_count).end {
        if !unsent(flush, index) {
            continue;
        }
        if group.visits == visits_left.get() {
            break;
        }
        group.visits += 1;
        let view = flush.round_op(index);
        let bytes = if view.is_barrier { 0 } else { view.bytes.len() as u64 };
        // A planted canary: the group is every unsent write — the one
        // burst the bounds exist to split (the attainable oracle must
        // read an offer above the class cap).
        let fits = cfg!(inf_canary_tier_group_whole_round)
            || (group.ops < TIER_GROUP_OPS_MAX && group.bytes + bytes <= TIER_GROUP_BYTES_MAX);
        if !fits {
            break;
        }
        group.end = index + 1;
        group.ops += 1;
        group.bytes += bytes;
    }
    debug_assert!(group.ops >= 1, "the first unsent op starts the group");
    Some(group)
}

/// Offers `group` to the budget at its exact size: a `Now` answer is the
/// one way a [`GrantedGroup`] is made (ADR-0178 T2).
fn offer_group(admission: &mut impl FlushAdmission, group: &Group) -> Option<GrantedGroup> {
    match admission.admit(group.bytes, group.ops as u64) {
        Issue::Now => Some(GrantedGroup { first: group.first, end: group.end, ops: group.ops }),
        Issue::NotThisSlice => None,
    }
}

/// Token slot of round op `index` on `lane` (ADR-0084 D3: `lane × 256 + op_index`).
/// The index is narrowed, never masked: `TierRound::admit_op` bounds it at
/// staging, and a wider index here would alias another op's completion.
fn round_slot(lane: u32, index: usize) -> u32 {
    let index = u8::try_from(index).expect("round op index is 8-bit (ADR-0084 D3)");
    (lane << 8) | u32::from(index)
}

/// Converts one granted group to driver ops: exactly the unsent ops in
/// its range, each marked sent. Windows stay pipeline-owned until
/// `finish_round` — the `StableBytes` custody argument lives in
/// `log_bytes::tier_round_bytes`.
fn emit_group<F: SegmentFs>(
    flush: &mut TierFlush<F>,
    round: &mut FlushRound,
    lane: u32,
    group: GrantedGroup,
    ops: &mut Vec<IoOp>,
) {
    let mut emitted = 0usize;
    for index in group.first..group.end {
        if !unsent(flush, index) {
            continue;
        }
        let view = flush.round_op(index);
        let slot = round_slot(lane, index);
        if view.is_barrier {
            let token = CompletionToken::new(TokenClass::TierFlushSync, slot, round.round_seq);
            ops.push(IoOp::Fdatasync { fd: view.fd, token });
        } else {
            let token = CompletionToken::new(TokenClass::TierFlushWrite, slot, round.round_seq);
            // The position is the round plan's, parsed once (ADR-0167 A1).
            ops.push(IoOp::LogWrite {
                fd: view.fd,
                offset: view.offset,
                data: crate::log_bytes::tier_round_bytes(&view),
                token,
                barrier: WriteBarrier::None,
            });
        }
        let sent = flush.mark_op_sent(index);
        debug_assert!(sent, "an unsent op is sent once");
        round.pending += 1;
        emitted += 1;
    }
    debug_assert_eq!(emitted, group.ops, "the emit pushes exactly the granted group");
    round.place_cursor(group.end, flush);
}

#[cfg(test)]
#[path = "tier_cell_tests.rs"]
mod tier_cell_tests;
