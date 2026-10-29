//! The sequential flush pipeline's file side (M4-S11, ADR-0056 D2/D3) —
//! rotation, early-seal, ring-top gaps, and the durability arithmetic
//! that feeds `advance_flushed`.
//!
//! [`TierFlush`] owns everything file-shaped: which `tier-NNNNNN.itier`
//! is active, when it seals (capacity at a flush-range boundary, an
//! ADR-0052 D2 ring-top gap, shutdown), and the catalog of sealed files
//! S12's MANIFEST v2 consumes. What it does **not** own: addresses,
//! records, watermarks — the store side pulls record-aligned ranges from
//! its address space and pushes them here (`inf-store → inf-log` is the
//! existing vocabulary edge). Two drives (M4.5-S31, ADR-0084 — the
//! ADR-0056 D3 deviation discharged): the **seam drive**
//! (`TieredTable::flush_slice` — blocking `SegmentFs` writes, the
//! `SyncIckWriter` pattern; recovery, orderly drains, component
//! tests/DST) and the **reactor drive** — a round is planned
//! ([`TierFlush::plan_round`]: its positions parsed, its windows reserved,
//! its file created) before it is staged ([`TierFlush::stage_round`]) into
//! the pipeline's capped [`TierRound`]; the plane rides its ops as
//! `IoOp::LogWrite`/`Fdatasync`, and every durability fact defers to a
//! [`RoundEffect`] applied at the round's last barrier completion.
//!
//! fsync failure is fatal-by-default (§8.4, ADR-0056 D4): it surfaces as
//! [`TierFlushError::Fsync`] and the flushed watermark freezes — no
//! caller may catch and continue past it.

use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use inf_foundation::LogicalAddr;
use inf_foundation::bounded::{CapCensus, CapError, CappedDeque, Reserve};
use inf_foundation::limits::FILE_OFFSET_BYTES_MAX;

use crate::fs::{SegmentFile, SegmentFs, TierIoMode};
use crate::limits::{TIER_PENDING_SEALS, TIER_ROUND_DIR_HOLDS, TIER_SPAN_BYTES_MAX};
use crate::record::NsId;
use crate::tier::{
    FrameStaging, OpState, RoundEffects, SealReason, TIER_KEY_WINDOW_BYTES, TierOpView, TierRound,
    TierWriteFailure, TierWriter, WindowPool, tier_extract, tier_frame_offset, tier_frame_span,
};

mod plan;

pub use plan::{Planned, RoundPlan, Selection};

/// How a pipeline's I/O reaches the device (M4.5-S31, ADR-0084 D1).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum TierDrive {
    /// Blocking `SegmentFs` calls at the call site — recovery, orderly
    /// drains, component tests/DST, fd-less filesystems (`MemFs`).
    Seam,
    /// Staged intents ride the cell driver (`IoOp::LogWrite`/
    /// `Fdatasync`); durability facts apply at completion CQEs. Plane
    /// pipelines only; requires fd-backed files.
    Reactor,
}

/// Default file-capacity target: 1 GiB of data bytes (ADR-0056 D2 —
/// knob joins S19's `INF.NS` ADR; construction parameter until then).
pub const TIER_FILE_CAPACITY_DEFAULT: u64 = 1 << 30;

/// What an unsealed active file lets `flushed` claim (ADR-0056 D5; the
/// boot value ADR-0174 D2 rule 4). One field of the pipeline, read by
/// [`TierFlush::confirmable_end`] alone; [`BootFlush`] is the only
/// constructor of `Barrier` and its one exit restores `FullFrames`, so
/// the plane never receives a pipeline carrying the boot value.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum ClaimRule {
    /// Full, final frames only: the partial tail frame is rewritten in
    /// place as appends extend it, and a torn rewrite after a crash
    /// would destroy bytes `flushed` claimed once the covering log is
    /// truncated.
    FullFrames,
    /// Every byte the barrier covered, the partial tail frame included:
    /// no manifest names a boot-written byte before the next checkpoint
    /// publishes, that publication is what truncates the log, and the
    /// pipeline reaches the plane only after its active file is sealed
    /// — neither half of the hazard exists for a boot pipeline.
    Barrier,
}

/// The seam drive's file side (ADR-0084 D1): what the store's flush body
/// drives with blocking calls — the live pipeline between MAINTAIN
/// rounds and the boot pipeline during replay. The body is written once
/// against this surface; neither wrapper hands out the other's handles.
pub trait SeamFlush {
    /// Files sealed so far, in seal order.
    fn sealed(&self) -> &[TierFileMeta];
    /// The active file: `(id, base, data_len, durable_len, path)`.
    fn active(&self) -> Option<(u32, LogicalAddr, u64, u64, &Path)>;
    /// The next append address while a file is active.
    fn append_cursor(&self) -> Option<u64>;
    /// The highest address the drive may confirm right now.
    fn confirmable_end(&self) -> Option<u64>;
    /// Device bytes handed over this boot life (monotone).
    fn device_bytes(&self) -> u64;
    /// Appends one record-aligned range at the write cursor.
    ///
    /// # Errors
    /// [`TierFlushError`]; nothing is claimable beyond the last barrier.
    fn append_range(&mut self, addr: LogicalAddr, bytes: &[u8]) -> Result<(), TierFlushError>;
    /// Seals the active file ahead of a sealed-dead interval.
    ///
    /// # Errors
    /// [`TierFlushError`]: the gap is not yet crossable.
    fn seal_for_gap(&mut self) -> Result<(), TierFlushError>;
    /// The slice barrier.
    ///
    /// # Errors
    /// [`TierFlushError::Fsync`] is fatal (§8.4).
    fn sync(&mut self) -> Result<(), TierFlushError>;
    /// Seals the active file for an orderly close.
    ///
    /// # Errors
    /// [`TierFlushError`] as for any seal.
    fn seal_shutdown(&mut self) -> Result<(), TierFlushError>;
}

/// One sealed tier file — the MANIFEST v2 entry's input (S12) and the
/// per-file live-counter key (S14): the file's exact logical range is
/// `[base, base + data_len)`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TierFileMeta {
    /// File id (`tier-NNNNNN.itier`).
    pub id: u32,
    /// First logical address of the file's range.
    pub base: LogicalAddr,
    /// Exact data bytes (= logical range length; no padding, no holes).
    pub data_len: u64,
    /// Why it sealed.
    pub reason: SealReason,
    /// The file's path (unlink is S15's, gated by the §3.1 deletion rule).
    pub path: PathBuf,
}

/// Construction parameters for one namespace's flush pipeline.
#[derive(Clone, Debug)]
pub struct TierFlushConfig {
    /// `shard-k/` — tier files live under `cold/` inside it (§4 layout).
    pub shard_dir: PathBuf,
    /// Owning cell (header identity).
    pub cell: u32,
    /// Owning namespace (header identity).
    pub ns: NsId,
    /// I/O mode for every file this pipeline creates (ADR-0054: per-file,
    /// fixed at open, default `Direct` on real filesystems).
    pub mode: TierIoMode,
    /// File-capacity target in data bytes (ADR-0056 D2). Early-seal cuts
    /// at the flush-range boundary that would overflow it.
    pub file_capacity: u64,
    /// Flush slice budget in bytes — one fdatasync barrier per slice
    /// quantum (ADR-0053 MAINTAIN vocabulary; ADR-0056 D3).
    pub slice_bytes: u64,
    /// `M`: the largest encoded record the namespace admits this life —
    /// the format's largest, at most half the ring (ADR-0155 D1). A span is
    /// at most `max(slice_bytes, record_len_max)`, which sizes the window
    /// pool the pipeline builds.
    pub record_len_max: u64,
}

/// A flush-pipeline failure. Three causes are fatal — the caller freezes
/// the flushed watermark and stops the cell: `Fsync`, a failed barrier
/// (constructing it is audited by `check-fsync-fail-stop.sh`, ADR-0056
/// D4); `Unaddressable`, a planned write position outside the driver's
/// range (ADR-0167 D4); and `SpanOverPool`, a selected span the
/// pipeline's window pool cannot hold. A retry of either of the last two
/// plans the same round.
#[derive(Debug)]
pub enum TierFlushError {
    /// An fdatasync-class barrier failed — non-recoverable by contract.
    Fsync {
        /// The file whose barrier failed.
        path: PathBuf,
        /// The device error.
        source: io::Error,
    },
    /// A device write or file operation failed (the append never
    /// happened; the watermark is simply not advanced).
    Io {
        /// The file the operation targeted.
        path: PathBuf,
        /// The device error.
        source: io::Error,
    },
    /// A planned reactor-drive write position is above the driver's range
    /// (`FILE_OFFSET_BYTES_MAX`, ADR-0167 D4 and A1): the plan stages
    /// nothing and the cell stops. Fatal because a retry plans the same
    /// position. Not storage-full: no space was refused.
    Unaddressable {
        /// The namespace's tier directory the round writes into.
        path: PathBuf,
        /// The refused position.
        offset_bytes: u64,
    },
    /// The pipeline's file ids are spent: the next id would wrap, and an id
    /// is never reused within a namespace lifetime. Refused before any
    /// effect and not fatal, but permanent for the lifetime: the flush
    /// stops and the namespace's writers end on their tail-stall timeout.
    /// Not storage-full: no space was refused.
    FileIdsExhausted {
        /// The namespace's tier directory.
        path: PathBuf,
    },
    /// A selected span needs more windows than the pipeline's pool holds:
    /// the pool was built for a smaller `max(S, M)` than the table admits.
    /// Fatal: a retry selects the same span. Not storage-full.
    SpanOverPool {
        /// The namespace's tier directory.
        path: PathBuf,
        /// The selected span's record bytes.
        span_bytes: u64,
        /// The largest span the pool serves.
        span_bytes_max: u64,
    },
}

impl core::fmt::Display for TierFlushError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            // fsync-fail-stop-allow: Display arm: renders, never handles
            TierFlushError::Fsync { path, source } => write!(
                f,
                "FATAL: tier fsync failed on {} — cell must stop: {source}",
                path.display()
            ),
            TierFlushError::Io { path, source } => {
                write!(f, "tier flush I/O failed on {}: {source}", path.display())
            }
            TierFlushError::Unaddressable { path, offset_bytes } => write!(
                f,
                "FATAL: tier write position {offset_bytes} on {} is above \
                 {FILE_OFFSET_BYTES_MAX}, the largest a driver op may carry — cell must stop",
                path.display()
            ),
            TierFlushError::FileIdsExhausted { path } => write!(
                f,
                "tier file ids exhausted on {}: the namespace flushes no further this lifetime",
                path.display()
            ),
            TierFlushError::SpanOverPool { path, span_bytes, span_bytes_max } => write!(
                f,
                "FATAL: a tier span of {span_bytes} B on {} is above its pool's {span_bytes_max} \
                 B — cell must stop",
                path.display()
            ),
        }
    }
}

impl std::error::Error for TierFlushError {}

impl TierFlushError {
    /// True for the fatal class — a failed fsync, an unaddressable write
    /// position (ADR-0167 D4) or a span above the window pool — so callers
    /// route to the terminal fail-stop handler without naming the variant.
    #[must_use]
    pub fn is_fatal(&self) -> bool {
        match self {
            // fsync-fail-stop-allow: is_fatal classifier: answers true, takes no action
            TierFlushError::Fsync { .. } => true,
            TierFlushError::Unaddressable { .. } => true,
            TierFlushError::SpanOverPool { .. } => true,
            TierFlushError::Io { .. } => false,
            TierFlushError::FileIdsExhausted { .. } => false,
        }
    }

    /// True when the failed operation was a write-time space refusal
    /// (M4-S21, ADR-0063 D4) — the store's device-full latch keys on
    /// this. Deliberately `Io`-only: fsync-time exhaustion stays in the
    /// fatal class (state unknowable — the fsyncgate rule).
    #[must_use]
    pub fn is_storage_full(&self) -> bool {
        match self {
            TierFlushError::Io { source, .. } => crate::fs::is_storage_exhausted(source),
            // fsync-fail-stop-allow: is_retryable classifier: answers false — the rule that forbids
            // the fsyncgate retry
            TierFlushError::Fsync { .. } => false,
            // A position fault refused no space (ADR-0167 D4); neither did
            // the id width or the pool's bound.
            TierFlushError::Unaddressable { .. }
            | TierFlushError::FileIdsExhausted { .. }
            | TierFlushError::SpanOverPool { .. } => false,
        }
    }
}

/// The per-namespace flush pipeline (one per (cell, tiered namespace) —
/// L1: cell-local, single owner). Its reactor-drive holders — the window
/// pool, the round's ops and effects, its staged seals and directory
/// holds — are capped, built whole with the pipeline and rows of its
/// cell's census, so a round allocates nothing.
pub struct TierFlush<F: SegmentFs> {
    fs: F,
    config: TierFlushConfig,
    writer: Option<TierWriter<F>>,
    next_id: u32,
    sealed: Vec<TierFileMeta>,
    active_id: u32,
    /// Device bytes of the files this pipeline has already sealed
    /// (M4-S13). The active writer's own tally is added on read, so
    /// [`device_bytes`](Self::device_bytes) is monotone across rotation.
    /// Reseeding through [`with_catalog`](Self::with_catalog) starts at
    /// zero: the counter is per boot life, exactly like every other
    /// tiering counter (§3.1 "addresses are per-life").
    sealed_device_bytes: u64,
    /// Open handles of the files the seam drive sealed since the last
    /// [`take_sealed_handles`](Self::take_sealed_handles): the boot
    /// pipeline's catalogue handles, handed to the plane at the hand-over
    /// (ADR-0174 D5). The reactor drive's seals return their handle from
    /// the completion that commits them instead
    /// ([`commit_oldest_seal`](Self::commit_oldest_seal)). Undrained
    /// handles simply close when the pipeline drops.
    sealed_handles: Vec<(u32, F::File)>,
    /// The drive (M4.5-S31, ADR-0084 D1). `Seam` by construction; the
    /// plane flips to `Reactor` before the first flush.
    drive: TierDrive,
    /// Reactor-drive window pool (ADR-0155 D2/D3), sized by `max(S, M)`
    /// at the build; unused on the seam drive.
    pool: WindowPool,
    /// The one round, staged here and executed by the plane (ADR-0084 D2
    /// — the explicit bound: one per namespace). Its holders outlive each
    /// round; a round is staged while it holds an effect.
    round: TierRound,
    /// Directory handles a round's dir-fsync barriers target — held
    /// until the round finishes (the fd must outlive the op). At most two.
    round_dir_holds: CappedDeque<F::File, Reserve>,
    /// Files whose seal is staged in the in-flight round: catalog
    /// commit happens at the barrier completion ([`RoundEffect::
    /// SealCommit`]); until then the open handle serves cold reads on
    /// the confirmed prefix and the file stays manifest-visible as an
    /// unsealed range. Empty whenever no round is in flight; at most two.
    pending_seals: CappedDeque<PendingSeal<F>, Reserve>,
    /// The claim rule for the unsealed active file (ADR-0056 D5 live,
    /// ADR-0174 D2 rule 4 at boot).
    claim: ClaimRule,
}

/// A seal staged but not yet completion-committed (ADR-0084 D2).
struct PendingSeal<F: SegmentFs> {
    id: u32,
    base: LogicalAddr,
    data_len: u64,
    /// Confirmed durable prefix at stage time (cold reads' bound).
    confirmed_len: u64,
    reason: SealReason,
    path: PathBuf,
    device_bytes: u64,
    file: F::File,
}

/// Read-side view of a pending seal (manifest, cold reads, disk usage).
#[derive(Copy, Clone, Debug)]
pub struct PendingSealView {
    /// File id (`tier-NNNNNN.itier`).
    pub id: u32,
    /// First logical address of the file's range.
    pub base: LogicalAddr,
    /// Exact data bytes staged (the sealed length once committed).
    pub data_len: u64,
    /// Confirmed durable prefix at stage time — the cold-read bound.
    pub confirmed_len: u64,
    /// Backend fd for cold-read ops.
    pub fd: Option<std::os::fd::RawFd>,
}

impl<F: SegmentFs> TierFlush<F> {
    /// A fresh pipeline. File ids start at `next_id` (0 for a fresh
    /// namespace; recovery passes the first free id — S12). Its capped
    /// holders are rows of `census`, the owning cell's.
    ///
    /// # Panics
    /// As [`with_catalog`](Self::with_catalog).
    pub fn new(
        fs: F,
        config: TierFlushConfig,
        next_id: u32,
        census: &Rc<CapCensus>,
    ) -> TierFlush<F> {
        Self::with_catalog(fs, config, next_id, Vec::new(), census)
    }

    /// A recovered pipeline (M4-S12, ADR-0057 D6): `sealed` seeds the
    /// catalog with the previous lives' manifested files (the recovered
    /// unsealed file re-sealed `Recovered` included), so the next
    /// checkpoint's manifest names old and new files uniformly and cold
    /// reads resolve through one catalog. Entries carry **manifested**
    /// lengths — a sealed file's inert physical excess is not readable
    /// address space.
    ///
    /// # Panics
    /// Panics on a zero capacity/slice config, a catalog not strictly
    /// ascending by id and base, or `next_id` not above every seeded id;
    /// and when the reactor holders' backing is refused — the allocation
    /// fail-stop every window allocation took before the pool was built
    /// whole (`max(S, M)` above `TIER_SPAN_BYTES_MAX` is the layout
    /// refusal, which `TierSpec::validate` rules out).
    pub fn with_catalog(
        fs: F,
        config: TierFlushConfig,
        next_id: u32,
        sealed: Vec<TierFileMeta>,
        census: &Rc<CapCensus>,
    ) -> TierFlush<F> {
        assert!(config.file_capacity > 0, "zero file capacity");
        assert!(config.slice_bytes > 0, "zero slice budget");
        for pair in sealed.windows(2) {
            assert!(pair[1].id > pair[0].id, "catalog ids ascend");
            assert!(
                pair[1].base.to_raw() >= pair[0].base.to_raw() + pair[0].data_len,
                "catalog ranges must not overlap"
            );
        }
        if let Some(last) = sealed.last() {
            assert!(next_id > last.id, "next_id collides with a seeded file");
        }
        let span_bytes_max = config.slice_bytes.max(config.record_len_max);
        let holders = ReactorHolders::new(span_bytes_max, census)
            .unwrap_or_else(|refused| panic!("tier flush pipeline holders refused: {refused}"));
        TierFlush {
            fs,
            config,
            writer: None,
            next_id,
            sealed,
            active_id: 0,
            sealed_device_bytes: 0,
            sealed_handles: Vec::new(),
            drive: TierDrive::Seam,
            pool: holders.pool,
            round: holders.round,
            round_dir_holds: holders.dir_holds,
            pending_seals: holders.pending_seals,
            claim: ClaimRule::FullFrames,
        }
    }

    /// Drains the open handles of files sealed since the last drain
    /// (M4-S26): the caller owns them from here — the plane parks them
    /// in its cold-read file table; dropping one closes the fd.
    pub fn take_sealed_handles(&mut self) -> Vec<(u32, F::File)> {
        core::mem::take(&mut self.sealed_handles)
    }

    // ---- reactor drive (M4.5-S31, ADR-0084) ----

    /// Switches the drive. Plane-only; called once per pipeline life,
    /// before any flush work (fresh creation or recovery install).
    ///
    /// # Panics
    /// Panics with a round in flight, pending seals, or staged frames —
    /// the drive never changes mid-flight.
    pub fn set_drive(&mut self, drive: TierDrive) {
        assert!(!self.round.is_staged(), "drive change with a round in flight");
        assert!(self.pending_seals.is_empty(), "drive change with pending seals");
        if let Some(w) = &mut self.writer {
            w.release_batch_window(&mut self.pool);
        }
        self.drive = drive;
    }

    /// The pipeline's drive.
    #[must_use]
    pub fn drive(&self) -> TierDrive {
        self.drive
    }

    /// Windows the pool has out (tests: the count is exact after every
    /// round — F-L04-13).
    #[cfg(test)]
    pub(crate) fn pool_outstanding(&self) -> usize {
        self.pool.outstanding()
    }

    /// The bytes the window pool reserved at its build (the cell's
    /// `tier_window_capacity_bytes` sums them).
    #[must_use]
    pub fn window_capacity_bytes(&self) -> u64 {
        self.pool.capacity_bytes()
    }

    /// The bytes of the pool's windows written at least once — the high
    /// water of windows out, as the lists are taken LIFO (the cell's
    /// `tier_window_written_bytes` sums them).
    #[must_use]
    pub fn window_written_bytes(&self) -> u64 {
        self.pool.written_bytes()
    }

    /// Whether a staged round exists (in flight or awaiting `finish_round`).
    #[must_use]
    pub fn round_active(&self) -> bool {
        self.round.is_staged()
    }

    /// Ops of the staged round: total, leading writes, barriers.
    #[must_use]
    pub fn round_op_count(&self) -> usize {
        self.round.op_count()
    }

    /// Leading ops of the round that are data writes.
    #[must_use]
    pub fn round_write_count(&self) -> usize {
        self.round.write_count()
    }

    /// Barrier ops of the round.
    #[must_use]
    pub fn round_barrier_count(&self) -> usize {
        self.round.barrier_count()
    }

    /// True when every op of the staged round names an fd this pipeline
    /// still holds — the active writer's, a pending seal's, or a round
    /// directory hold's. A round failing this would hand the driver a
    /// closed (or reused) fd (review 2026-08-30, F-L01-02).
    #[must_use]
    pub fn round_handles_owned(&self) -> bool {
        let owned = |fd: std::os::fd::RawFd| {
            self.writer.as_ref().and_then(TierWriter::raw_fd) == Some(fd)
                || self.pending_seals.iter().any(|s| s.file.raw_fd() == Some(fd))
                || self.round_dir_holds.iter().any(|h| h.raw_fd() == Some(fd))
        };
        (0..self.round.op_count())
            .all(|i| self.round.op(i, &self.pool).is_some_and(|op| owned(op.fd)))
    }

    /// The round op at `index`, its state included — the plane converts
    /// writes to `IoOp::LogWrite` and barriers to `IoOp::Fdatasync`. The
    /// returned window bytes stay valid (pool-owned, heap-stable) until
    /// [`finish_round`](Self::finish_round).
    ///
    /// # Panics
    /// Panics past the round's op count: the plane reads only the ops its
    /// own round's tokens and cursor name.
    #[must_use]
    pub fn round_op(&self, index: usize) -> TierOpView<'_> {
        self.round.op(index, &self.pool).expect("a round op view inside the round")
    }

    /// The state of round op `index`; `None` past the round's ops (a
    /// completion token of an earlier, larger round).
    #[must_use]
    pub fn round_op_state(&self, index: usize) -> Option<OpState> {
        self.round.op(index, &self.pool).map(|op| op.state)
    }

    /// Op `index` is handed to the driver: `Unsent` to `Sent`. `false`
    /// (nothing changed) for an op not `Unsent`.
    pub fn mark_op_sent(&mut self, index: usize) -> bool {
        let sent = self.round.transition(index, OpState::Unsent, OpState::Sent);
        // A planted canary: the round's windows return to the pool at its
        // last write's send, before their completions (a held completion
        // must read a recycled window in the custody test).
        if cfg!(inf_canary_tier_window_early_put)
            && sent
            && index < self.round.write_count()
            && self.round.writes_all_sent()
        {
            self.pool.recycle_early();
        }
        sent
    }

    /// Op `index`'s completion arrived: `Sent` to `Done`, or to `Failed`
    /// with an error. `false` (nothing changed) for an op not `Sent` — a
    /// stale or repeated completion.
    pub fn complete_op(&mut self, index: usize, failed: bool) -> bool {
        let to = if failed { OpState::Failed } else { OpState::Done };
        self.round.transition(index, OpState::Sent, to)
    }

    /// Every failed write is `Unsent` again, byte-identical, for its retry
    /// group; returns how many.
    pub fn retry_failed_writes(&mut self) -> usize {
        self.round.requeue_failed_writes()
    }

    /// Finishes the completed round: returns its windows to the pool,
    /// releases the directory holds, and yields the deferred effects in
    /// stage order for the store to apply (at most three, held inline).
    /// Callable only once every op reached a terminal completion (the
    /// plane's custody obligation).
    #[must_use]
    pub fn finish_round(&mut self) -> RoundEffects {
        while self.round_dir_holds.pop_front().is_some() {}
        self.round.finish(&mut self.pool)
    }

    /// Applies a completed round's `DurableTo` fact to the active file.
    ///
    /// # Panics
    /// Panics without an active writer — the effect was generated by it.
    pub fn confirm_durable_to(&mut self, data_len: u64) {
        self.writer
            .as_mut()
            .expect("DurableTo without an active writer")
            .confirm_durable_to(data_len);
    }

    /// Applies a completed round's `SealCommit` fact: the oldest pending
    /// seal joins the catalog exactly as a seam seal would have, and its
    /// open handle — the creation-mode fd cold reads reuse (ADR-0054) —
    /// is this completion's return value, for the plane's file table.
    ///
    /// # Panics
    /// Panics without a pending seal — effects mirror stage order.
    #[must_use = "the sealed file's handle serves its cold reads"]
    pub fn commit_oldest_seal(&mut self) -> (u32, F::File) {
        let seal = self.pending_seals.pop_front().expect("SealCommit without a pending seal");
        self.sealed_device_bytes += seal.device_bytes;
        let handle = (seal.id, seal.file);
        self.sealed.push(TierFileMeta {
            id: seal.id,
            base: seal.base,
            data_len: seal.data_len,
            reason: seal.reason,
            path: seal.path,
        });
        handle
    }

    /// Seals staged in the in-flight round, not yet committed — the
    /// manifest names them as unsealed ranges, cold reads may target
    /// their confirmed prefix, disk usage counts them. Empty whenever no
    /// round is in flight.
    pub fn pending_seals(&self) -> impl Iterator<Item = PendingSealView> + '_ {
        self.pending_seals.iter().map(|s| PendingSealView {
            id: s.id,
            base: s.base,
            data_len: s.data_len,
            confirmed_len: s.confirmed_len,
            fd: s.file.raw_fd(),
        })
    }

    /// Pending (staged, uncommitted) seals in the in-flight round.
    #[must_use]
    pub fn pending_seal_count(&self) -> usize {
        self.pending_seals.len()
    }

    /// The active file's raw fd, when one is open and the tier has real
    /// fds — the cold-read path for addresses already released beneath
    /// `flushed` inside the active file (M4-S26).
    #[must_use]
    pub fn active_raw_fd(&self) -> Option<std::os::fd::RawFd> {
        self.writer.as_ref().and_then(TierWriter::raw_fd)
    }

    /// The per-slice byte budget (the drive loop's bound).
    #[must_use]
    pub fn slice_bytes(&self) -> u64 {
        self.config.slice_bytes
    }

    /// The next file id this pipeline would create (recovery/handoff
    /// bookkeeping — a successor pipeline starts here).
    #[must_use]
    pub fn next_file_id(&self) -> u32 {
        self.next_id
    }

    /// Files sealed so far, in seal order (S12's MANIFEST input; tests'
    /// observability).
    #[must_use]
    pub fn sealed(&self) -> &[TierFileMeta] {
        &self.sealed
    }

    /// Detaches a retired file from the sealed catalog (M4-S15, ADR-0059
    /// D3): the manifest swap that excluded it has landed, so no durable
    /// artifact names it any more — the returned meta drives the
    /// pin-gated [`unlink_tier_file`]. `None` when the id is not in the
    /// catalog (idempotent — a retried commit is legal). The remaining
    /// catalog stays strictly ascending; the range gap is legal
    /// (ADR-0059 D5).
    pub fn detach_sealed(&mut self, id: u32) -> Option<TierFileMeta> {
        let pos = self.sealed.iter().position(|m| m.id == id)?;
        Some(self.sealed.remove(pos))
    }

    /// Bytes this namespace's flush has handed the device this boot life
    /// (M4-S13 `flush_bytes`): sealed files plus the active one — header
    /// blocks, frame writes (partial-tail rewrites included), footers.
    /// Monotone; ≥ the data bytes appended, and the gap **is** the tier
    /// leg of write amplification (S16 divides by user bytes).
    #[must_use]
    pub fn device_bytes(&self) -> u64 {
        self.sealed_device_bytes + self.writer.as_ref().map_or(0, TierWriter::device_bytes)
    }

    /// On-disk bytes the pipeline's files hold **right now** (M4-S19,
    /// ADR-0062 D5 — the tier-file half of a namespace's disk usage).
    /// Deliberately not [`device_bytes`](Self::device_bytes): that is
    /// the cumulative write tally (rewritten partial tails included, the
    /// write-amplification numerator), while a disk budget bounds
    /// occupancy. Computed from the format arithmetic — header + whole
    /// CRC frames + (sealed) footer per file — so no `stat` syscalls on
    /// the scrape path.
    #[must_use]
    pub fn disk_bytes(&self) -> u64 {
        use crate::tier::{
            TIER_FOOTER_BYTES, TIER_FRAME_BYTES, TIER_FRAME_DATA, TIER_HEADER_BYTES,
        };
        let file_bytes = |data_len: u64, sealed: bool| {
            (TIER_HEADER_BYTES as u64)
                + data_len.div_ceil(TIER_FRAME_DATA as u64) * TIER_FRAME_BYTES as u64
                + if sealed { TIER_FOOTER_BYTES as u64 } else { 0 }
        };
        let sealed: u64 = self.sealed.iter().map(|m| file_bytes(m.data_len, true)).sum();
        let pending: u64 = self.pending_seals.iter().map(|s| file_bytes(s.data_len, true)).sum();
        sealed + pending + self.writer.as_ref().map_or(0, |w| file_bytes(w.data_len(), false))
    }

    /// The active file, if any: `(id, base, data_len, durable_len, path)`.
    #[must_use]
    pub fn active(&self) -> Option<(u32, LogicalAddr, u64, u64, &std::path::Path)> {
        self.writer
            .as_ref()
            .map(|w| (self.active_id, w.base(), w.data_len(), w.durable_len(), w.path()))
    }

    /// The active file's data bytes a reader may rely on while it is
    /// unsealed (ADR-0056 D5): the last full, final frame boundary its
    /// barriers covered. Bytes past it are staged, in flight, or in the
    /// partial tail frame a later append rewrites in place. `None` with
    /// no active file.
    #[must_use]
    pub fn active_confirmable_len(&self) -> Option<u64> {
        self.writer.as_ref().map(TierWriter::confirmable_len)
    }

    /// The next append address, when a file is active — the drive loop's
    /// resume cursor (bytes staged ahead of `flushed` must never be
    /// re-appended). `None` when no file is active (fresh pipeline, or
    /// right after a gap/shutdown seal — the drive resumes at `flushed`).
    #[must_use]
    pub fn append_cursor(&self) -> Option<u64> {
        self.writer.as_ref().map(|w| w.base().to_raw() + w.data_len())
    }

    /// Appends one record-aligned flush range at `addr`. Ranges arrive
    /// contiguously except across ring-top gaps, which the drive loop
    /// announces via [`seal_for_gap`](Self::seal_for_gap) first — a
    /// non-contiguous append without one is a programmer error. Seals the
    /// active file first when this range would overflow the capacity
    /// target (early-seal at a range boundary — ADR-0056 D2).
    ///
    /// # Errors
    /// [`TierFlushError`]; on error nothing is claimable beyond what the
    /// last barrier covered.
    ///
    /// # Panics
    /// Panics when `addr` is not the active file's write cursor (the
    /// contiguity contract above).
    pub fn append_range(&mut self, addr: LogicalAddr, bytes: &[u8]) -> Result<(), TierFlushError> {
        assert!(!self.round.is_staged(), "seam append while a reactor round is in flight");
        if let Some(w) = &self.writer {
            let cursor = w.base().to_raw() + w.data_len();
            assert_eq!(
                addr.to_raw(),
                cursor,
                "flush ranges are contiguous; gaps go through seal_for_gap"
            );
        }
        let capacity = self.config.file_capacity;
        if let Some(writer) = self
            .writer
            .take_if(|w| w.data_len() > 0 && w.data_len() + bytes.len() as u64 > capacity)
        {
            self.seal_writer(writer, SealReason::Capacity)?;
        }
        if self.writer.is_none() {
            self.create_file(addr)?;
        }
        let w = self.writer.as_mut().expect("created above");
        w.append(addr, bytes)
            .map_err(|source| TierFlushError::Io { path: w.path().to_path_buf(), source })
    }

    /// Announces an ADR-0052 D2 ring-top gap at the drive cursor: seals
    /// the active file (footer + fdatasync + close) so `flushed` may
    /// advance across the dead interval without writing padding. The
    /// next [`append_range`](Self::append_range) starts a new file at
    /// the post-gap address. No active file (gap at a file boundary) is
    /// a no-op.
    ///
    /// # Errors
    /// [`TierFlushError`] — a failed seal means the gap (and everything
    /// after it) is not yet crossable.
    pub fn seal_for_gap(&mut self) -> Result<(), TierFlushError> {
        assert!(!self.round.is_staged(), "seam gap seal while a reactor round is in flight");
        if let Some(writer) = self.writer.take() {
            self.seal_writer(writer, SealReason::RingTopGap)?;
        }
        Ok(())
    }

    /// The slice barrier: fdatasyncs the active file. After it,
    /// [`confirmable_end`](Self::confirmable_end) says exactly how far
    /// `flushed` may advance.
    ///
    /// # Errors
    /// [`TierFlushError::Fsync`] is fatal (§8.4).
    pub fn sync(&mut self) -> Result<(), TierFlushError> {
        assert!(!self.round.is_staged(), "seam sync while a reactor round is in flight");
        if let Some(w) = &mut self.writer {
            let path = w.path().to_path_buf();
            w.sync().map_err(|failure| classify(failure, path))?;
        }
        Ok(())
    }

    /// Seals the active file for an orderly close (shutdown, tests).
    ///
    /// # Errors
    /// [`TierFlushError`] as for any seal.
    pub fn seal_shutdown(&mut self) -> Result<(), TierFlushError> {
        assert!(!self.round.is_staged(), "seam seal while a reactor round is in flight");
        if let Some(writer) = self.writer.take() {
            self.seal_writer(writer, SealReason::Shutdown)?;
        }
        Ok(())
    }

    /// Barrier seal under backpressure (ADR-0056 D8): the stall driver
    /// calls this when a tail-allocation stall is outstanding and the
    /// pipeline is dry — the partial-frame holdback would otherwise
    /// wedge the stalled writer forever (it is the writer that would
    /// have filled the frame). No active file is a no-op.
    ///
    /// # Errors
    /// [`TierFlushError`] as for any seal.
    pub fn seal_stall(&mut self) -> Result<(), TierFlushError> {
        assert!(!self.round.is_staged(), "seam seal while a reactor round is in flight");
        if let Some(writer) = self.writer.take() {
            self.seal_writer(writer, SealReason::Stall)?;
        }
        Ok(())
    }

    /// The planted canary's early seal (`inf_canary_replay_seal_every_range`):
    /// a non-empty active file sealed at capacity whatever the next range,
    /// where [`append_range`](Self::append_range) seals only before a range
    /// that would take it past the capacity target. Compiled only into the
    /// canary's build.
    ///
    /// # Errors
    /// As any seal.
    #[cfg(inf_canary_replay_seal_every_range)]
    pub fn seal_capacity_planted(&mut self) -> Result<(), TierFlushError> {
        if let Some(writer) = self.writer.take_if(|w| w.data_len() > 0) {
            self.seal_writer(writer, SealReason::Capacity)?;
        }
        Ok(())
    }

    /// The highest address the drive loop may confirm right now: the
    /// active file's claimable end under the pipeline's claim rule —
    /// full, final frames only on the live rule (the partial tail frame
    /// is claimable at seal, ADR-0056 D5), every barrier-covered byte on
    /// a boot pipeline (ADR-0174 D2 rule 4) — or the last sealed file's
    /// exact end when no file is active. `None` before anything was
    /// written.
    #[must_use]
    pub fn confirmable_end(&self) -> Option<u64> {
        if let Some(w) = &self.writer {
            let claimed = match self.claim {
                ClaimRule::FullFrames => w.confirmable_len(),
                ClaimRule::Barrier => w.durable_len(),
            };
            return Some(w.base().to_raw() + claimed);
        }
        self.sealed.last().map(|m| m.base.to_raw() + m.data_len)
    }

    fn create_file(&mut self, base: LogicalAddr) -> Result<(), TierFlushError> {
        let id = self.next_id;
        // No id is ever reused within a namespace lifetime: the last one
        // is refused before any effect.
        let Some(next) = id.checked_add(1) else {
            return Err(TierFlushError::FileIdsExhausted {
                path: self.config.shard_dir.join("cold"),
            });
        };
        // `tier_dir_open_fail` on the seam drive: the create refused before
        // the file exists (EMFILE physics) — the boot demote step's create
        // refusal (ADR-0174 D5), typed, with no file left behind.
        if inf_foundation::fault::fire(crate::fault::TIER_DIR_OPEN_FAIL) {
            return Err(TierFlushError::Io {
                path: self.config.shard_dir.join("cold"),
                source: crate::fault::injected(crate::fault::TIER_DIR_OPEN_FAIL),
            });
        }
        let writer = TierWriter::create_with_capacity(
            &self.fs,
            &self.config.shard_dir,
            id,
            self.config.cell,
            self.config.ns,
            base,
            self.config.mode,
            self.config.file_capacity,
        )
        .map_err(|source| TierFlushError::Io {
            path: self.config.shard_dir.join("cold"),
            source,
        })?;
        self.next_id = next;
        self.active_id = id;
        self.writer = Some(writer);
        Ok(())
    }

    fn seal_writer(
        &mut self,
        writer: TierWriter<F>,
        reason: SealReason,
    ) -> Result<(), TierFlushError> {
        let base = writer.base();
        let path_hint = writer.path().to_path_buf();
        let (sealed, handle) =
            writer.seal(reason).map_err(|failure| classify(failure, path_hint))?;
        self.sealed_device_bytes += sealed.device_bytes;
        self.sealed_handles.push((self.active_id, handle));
        self.sealed.push(TierFileMeta {
            id: self.active_id,
            base,
            data_len: sealed.data_len,
            reason,
            path: sealed.path,
        });
        Ok(())
    }
}

/// The reactor drive's capped holders, built together so a refusal of
/// any one builds none (ADR-0155 D2: the reserve precedes serving).
struct ReactorHolders<F: SegmentFs> {
    pool: WindowPool,
    round: TierRound,
    dir_holds: CappedDeque<F::File, Reserve>,
    pending_seals: CappedDeque<PendingSeal<F>, Reserve>,
}

impl<F: SegmentFs> ReactorHolders<F> {
    fn new(span_bytes_max: u64, census: &Rc<CapCensus>) -> Result<ReactorHolders<F>, CapError> {
        if span_bytes_max > TIER_SPAN_BYTES_MAX {
            return Err(CapError::Layout);
        }
        Ok(ReactorHolders {
            pool: WindowPool::new(span_bytes_max, census)?,
            round: TierRound::new(census)?,
            dir_holds: CappedDeque::new(TIER_ROUND_DIR_HOLDS, census)?,
            pending_seals: CappedDeque::new(TIER_PENDING_SEALS, census)?,
        })
    }
}

fn classify(failure: TierWriteFailure, path: PathBuf) -> TierFlushError {
    match failure {
        TierWriteFailure::Write(source) => TierFlushError::Io { path, source },
        // fsync-fail-stop-allow: conversion between the two typed fsync errors; still propagating
        TierWriteFailure::Fsync(source) => TierFlushError::Fsync { path, source },
    }
}

impl<F: SegmentFs> SeamFlush for TierFlush<F> {
    fn sealed(&self) -> &[TierFileMeta] {
        TierFlush::sealed(self)
    }

    fn active(&self) -> Option<(u32, LogicalAddr, u64, u64, &Path)> {
        TierFlush::active(self)
    }

    fn append_cursor(&self) -> Option<u64> {
        TierFlush::append_cursor(self)
    }

    fn confirmable_end(&self) -> Option<u64> {
        TierFlush::confirmable_end(self)
    }

    fn device_bytes(&self) -> u64 {
        TierFlush::device_bytes(self)
    }

    fn append_range(&mut self, addr: LogicalAddr, bytes: &[u8]) -> Result<(), TierFlushError> {
        TierFlush::append_range(self, addr, bytes)
    }

    fn seal_for_gap(&mut self) -> Result<(), TierFlushError> {
        TierFlush::seal_for_gap(self)
    }

    fn sync(&mut self) -> Result<(), TierFlushError> {
        TierFlush::sync(self)
    }

    fn seal_shutdown(&mut self) -> Result<(), TierFlushError> {
        TierFlush::seal_shutdown(self)
    }
}

// ---- the boot pipeline (ADR-0174 D2 rule 4, D3, D5) ----

/// A tiered namespace's flush pipeline during boot replay: the recovered
/// [`TierFlush`] under the barrier claim rule, holding the open
/// creation-mode handle of every sealed catalogue file (the manifested
/// ones, opened at recovery; the boot-sealed ones, as the pipeline seals
/// them) and one aligned two-frame buffer for the settle read. It drives
/// like the live pipeline through [`SeamFlush`]; its one exit is
/// [`hand_over`](Self::hand_over), the only path from a boot pipeline to
/// a `TierFlush` and to the handles — so the plane never installs a
/// pipeline that carries the boot value, and a boot-demoted key is
/// readable by the first command after `Ready`.
pub struct BootFlush<F: SegmentFs> {
    flush: TierFlush<F>,
    /// The settle read's window: [`SETTLE_WINDOW_FRAMES`] aligned frames.
    window: FrameStaging,
    /// The extracted key window (≤ the window length a read asks for;
    /// allocated once).
    extracted: Vec<u8>,
}

/// What the boot's exit hands the plane (ADR-0174 D5): the pipeline
/// under the live claim rule, with no open writer, and the creation-mode
/// handle of every sealed file in its catalogue.
pub struct HandedOver<F: SegmentFs> {
    pub flush: TierFlush<F>,
    pub handles: Vec<(u32, F::File)>,
}

/// The settle read's answer (ADR-0174 D3, the `read` step): the record's
/// key window, borrowed from the pipeline's buffer, and `left`, the bytes
/// from the record's address to its file's claimed end, which bounds the
/// record's length. The caller parses it under the slot's hash.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SettleWindow<'a> {
    /// The window's bytes: `min(asked, left)` of them.
    pub bytes: &'a [u8],
    /// Bytes from the address to the file's claimed end.
    pub left: u64,
}

/// Why a settle read did not answer (the `replay_settle_read_fail`
/// point's failure among them): each a typed boot refusal for the
/// caller, naming the address; never "distinct".
#[derive(Debug)]
pub enum SettleReadError {
    /// No catalogued file covers the address: a retired range, a hole,
    /// or an address past the active file's claimed end.
    NoRange { addr: u64 },
    /// The catalogue names the file but the pipeline holds no handle
    /// for it — a construction defect, answered typed.
    NoHandle { addr: u64, id: u32 },
    /// The handle's read failed (the device, or the injected point).
    Io { addr: u64, path: PathBuf, source: io::Error },
    /// The file ends inside the window the catalogue says it covers.
    Short { addr: u64, path: PathBuf },
    /// A frame of the window fails its CRC.
    Corrupt { addr: u64, path: PathBuf, frame: u64 },
}

impl core::fmt::Display for SettleReadError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            SettleReadError::NoRange { addr } => {
                write!(f, "settle read at {addr}: no catalogued tier range covers it")
            }
            SettleReadError::NoHandle { addr, id } => {
                write!(f, "settle read at {addr}: no held handle for tier file {id}")
            }
            SettleReadError::Io { addr, path, source } => {
                write!(f, "settle read at {addr} on {}: {source}", path.display())
            }
            SettleReadError::Short { addr, path } => {
                write!(f, "settle read at {addr}: {} ends inside the key window", path.display())
            }
            SettleReadError::Corrupt { addr, path, frame } => {
                write!(f, "settle read at {addr}: frame {frame} of {} fails CRC", path.display())
            }
        }
    }
}

impl std::error::Error for SettleReadError {}

/// Aligned frames the settle read's buffer holds: a key window of
/// [`TIER_KEY_WINDOW_BYTES`] — at most one frame's payload — starts in
/// one frame and ends in the next at the latest. Owner: [`BootFlush`]'s
/// buffer, allocated once. Crossing: unreachable — the const assert on
/// the window bound beside it.
pub const SETTLE_WINDOW_FRAMES: usize = 2;

impl<F: SegmentFs> BootFlush<F> {
    /// Puts a recovered pipeline under the boot claim rule. `handles`
    /// are the open creation-mode handles of its sealed catalogue files,
    /// in catalogue order — one per file; the boot adds its own as it
    /// seals, and the settle read answers from these alone.
    #[must_use]
    pub fn new(mut flush: TierFlush<F>, handles: Vec<(u32, F::File)>) -> BootFlush<F> {
        debug_assert!(
            flush.sealed.iter().map(|m| m.id).eq(handles.iter().map(|(id, _)| *id)),
            "one held handle per sealed catalogue file, in order"
        );
        debug_assert!(flush.sealed_handles.is_empty(), "a recovered pipeline sealed nothing yet");
        debug_assert_eq!(flush.drive, TierDrive::Seam, "boot drives the seam");
        flush.sealed_handles = handles;
        flush.claim = ClaimRule::Barrier;
        BootFlush {
            flush,
            window: FrameStaging::new(SETTLE_WINDOW_FRAMES),
            extracted: Vec::with_capacity(TIER_KEY_WINDOW_BYTES),
        }
    }

    /// The settle read's locate and read steps (ADR-0174 D3): the frames
    /// covering `min(TIER_KEY_WINDOW_BYTES, left)` bytes at `addr` — one,
    /// or two when the window crosses a frame — by one blocking read on
    /// the covering file's **held** handle into the aligned buffer, every
    /// frame's CRC checked. Opens nothing: a file this boot writes is
    /// never open a second way (ADR-0054 D1).
    ///
    /// # Errors
    /// [`SettleReadError`], each a typed boot refusal: no covering
    /// range, no held handle, the read's failure (or the injected
    /// `replay_settle_read_fail`), a short file, a CRC failure.
    pub fn read_key_window(&mut self, addr: u64) -> Result<SettleWindow<'_>, SettleReadError> {
        let Located { base, end, file, path } = locate_held(&self.flush, addr)?;
        let left = end - addr;
        let len =
            usize::try_from(left).map_or(TIER_KEY_WINDOW_BYTES, |l| l.min(TIER_KEY_WINDOW_BYTES));
        let (first, count, skip) = tier_frame_span(addr - base, len);
        debug_assert!(count as usize <= SETTLE_WINDOW_FRAMES, "the buffer covers the window");
        let frames = self.window.frames_mut(count as usize);
        if inf_foundation::fault::fire(crate::fault::REPLAY_SETTLE_READ_FAIL) {
            return Err(SettleReadError::Io {
                addr,
                path: path.to_path_buf(),
                source: crate::fault::injected(crate::fault::REPLAY_SETTLE_READ_FAIL),
            });
        }
        let from = tier_frame_offset(first);
        let mut done = 0usize;
        while done < frames.len() {
            let n = file
                .read_at(from + done as u64, &mut frames[done..])
                .map_err(|source| SettleReadError::Io { addr, path: path.to_path_buf(), source })?;
            if n == 0 {
                return Err(SettleReadError::Short { addr, path: path.to_path_buf() });
            }
            done += n;
        }
        tier_extract(frames, skip, len, &mut self.extracted).map_err(|e| {
            SettleReadError::Corrupt {
                addr,
                path: path.to_path_buf(),
                frame: first + u64::from(e.window_frame),
            }
        })?;
        Ok(SettleWindow { bytes: &self.extracted, left })
    }

    /// The boot's exit (ADR-0174 D5, R10): the active file sealed — a
    /// no-op once the store's drain sealed it; a pipeline handed
    /// over with barrier-claimed bytes in a rewritable frame would be
    /// the live hazard the rule exists for, so the exit itself closes
    /// that door — the claim rule back to full frames, and the pipeline
    /// with every handle it held, one per sealed catalogue file, for the
    /// plane's `install_recovered`.
    ///
    /// # Errors
    /// [`TierFlushError`] from the seal, as for any seal.
    pub fn hand_over(mut self) -> Result<HandedOver<F>, TierFlushError> {
        self.flush.seal_shutdown()?;
        self.flush.claim = ClaimRule::FullFrames;
        let handles = if cfg!(inf_canary_replay_handles_dropped) {
            // The planted canary breaks ADR-0174 D5: the boot-sealed
            // handles are closed instead of returned.
            Vec::new()
        } else {
            self.flush.take_sealed_handles()
        };
        Ok(HandedOver { flush: self.flush, handles })
    }

    /// The next file id this pipeline would create.
    #[must_use]
    pub fn next_file_id(&self) -> u32 {
        self.flush.next_file_id()
    }

    /// The per-slice byte budget of the recovered configuration.
    #[must_use]
    pub fn slice_bytes(&self) -> u64 {
        self.flush.slice_bytes()
    }

    /// On-disk bytes the pipeline's files hold right now.
    #[must_use]
    pub fn disk_bytes(&self) -> u64 {
        self.flush.disk_bytes()
    }

    /// Handles held: one per sealed catalogue file (ADR-0174 D5).
    #[must_use]
    pub fn held_handles(&self) -> usize {
        self.flush.sealed_handles.len()
    }

    /// The planted canary's stall seal (`inf_canary_replay_stall_seal`):
    /// a boot pipeline never seals a file to free a partial frame
    /// (ADR-0174 D2 rule 5); the seal-reason census is what sees one that
    /// does. Compiled only into the canary's build.
    ///
    /// # Errors
    /// As any seal.
    #[cfg(inf_canary_replay_stall_seal)]
    pub fn seal_stall_planted(&mut self) -> Result<(), TierFlushError> {
        self.flush.seal_stall()
    }
}

impl<F: SegmentFs> SeamFlush for BootFlush<F> {
    fn sealed(&self) -> &[TierFileMeta] {
        self.flush.sealed()
    }

    fn active(&self) -> Option<(u32, LogicalAddr, u64, u64, &Path)> {
        self.flush.active()
    }

    fn append_cursor(&self) -> Option<u64> {
        self.flush.append_cursor()
    }

    fn confirmable_end(&self) -> Option<u64> {
        self.flush.confirmable_end()
    }

    fn device_bytes(&self) -> u64 {
        self.flush.device_bytes()
    }

    fn append_range(&mut self, addr: LogicalAddr, bytes: &[u8]) -> Result<(), TierFlushError> {
        // The planted canary seals the boot's file before every range, not
        // only before one that would overflow it: the seal census's rule
        // for a capacity seal and its count against the files-sealed bound.
        #[cfg(inf_canary_replay_seal_every_range)]
        self.flush.seal_capacity_planted()?;
        self.flush.append_range(addr, bytes)
    }

    fn seal_for_gap(&mut self) -> Result<(), TierFlushError> {
        self.flush.seal_for_gap()
    }

    fn sync(&mut self) -> Result<(), TierFlushError> {
        self.flush.sync()
    }

    fn seal_shutdown(&mut self) -> Result<(), TierFlushError> {
        self.flush.seal_shutdown()
    }
}

/// Where the settle read's locate step found an address: the covering
/// file's base and claimed end (both logical addresses), its held handle
/// and its path.
struct Located<'a, File> {
    base: u64,
    end: u64,
    file: &'a File,
    path: &'a Path,
}

/// The settle read's locate step: the catalogue file holding `addr`, by
/// bisection over the ascending catalogue (the rebuild asks once per slot
/// against thousands of files), with its held handle and its claimed end
/// — a sealed file's exact end, the active file's barrier-covered end
/// (the boot claim rule).
fn locate_held<F: SegmentFs>(
    flush: &TierFlush<F>,
    addr: u64,
) -> Result<Located<'_, F::File>, SettleReadError> {
    let at = flush.sealed.partition_point(|m| {
        note_span_locate_step();
        m.base.to_raw() <= addr
    });
    if let Some(meta) = at.checked_sub(1).map(|i| &flush.sealed[i])
        && addr < meta.base.to_raw() + meta.data_len
    {
        note_span_locate_step();
        // Handles ascend by id like the catalogue (manifested in catalogue
        // order, then the boot's seals above every manifested id).
        let h = flush.sealed_handles.partition_point(|(id, _)| *id < meta.id);
        return match flush.sealed_handles.get(h) {
            Some((id, file)) if *id == meta.id => Ok(Located {
                base: meta.base.to_raw(),
                end: meta.base.to_raw() + meta.data_len,
                file,
                path: &meta.path,
            }),
            _ => Err(SettleReadError::NoHandle { addr, id: meta.id }),
        };
    }
    if let Some(w) = &flush.writer {
        let base = w.base().to_raw();
        let end = base + w.durable_len();
        if addr >= base && addr < end {
            return Ok(Located { base, end, file: w.file(), path: w.path() });
        }
    }
    Err(SettleReadError::NoRange { addr })
}

/// Unlinks a retired tier file (M4-S15, ADR-0059 D3) — the last step of
/// the retirement pipeline, executed by the plane only after the
/// covering MANIFEST swap landed **and** the file's read pins drained
/// (`ColdReads::inflight_on == 0`). Routed through [`SegmentFs`] so DST
/// faults it like every other file operation.
///
/// # Errors
/// The fs error, **non-fatal by design** (the one deliberate exception
/// to the tier pipeline's fail-stop posture): the durable truth already
/// excludes the file, so a failed unlink defers disk space, never
/// durability — the caller counts it and retries, and the boot GC
/// re-drives it after any crash (both idempotent).
pub fn unlink_tier_file<F: SegmentFs>(fs: &F, meta: &TierFileMeta) -> std::io::Result<()> {
    if inf_foundation::fault::fire(crate::fault::TIER_UNLINK_FAIL) {
        return Err(crate::fault::injected(crate::fault::TIER_UNLINK_FAIL));
    }
    fs.remove_file(&meta.path)
}

#[cfg(test)]
thread_local! {
    /// Catalog entries the settle read examined to locate an address —
    /// the L04 perf-row witness (O(log n), not O(n)).
    static SPAN_LOCATE_STEPS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn note_span_locate_step() {
    SPAN_LOCATE_STEPS.with(|c| c.set(c.get() + 1));
}

#[cfg(not(test))]
#[inline(always)]
fn note_span_locate_step() {}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::fs::mem::MemFs;
    use crate::tier::{TIER_FRAME_DATA, inspect_tier_bytes};

    /// A cell's census, its serve mark taken: the pipeline's rows live on
    /// it.
    pub(super) fn census() -> Rc<CapCensus> {
        let (census, serve_mark) = CapCensus::new(inf_foundation::CellId(0));
        drop(serve_mark);
        census
    }

    fn pipeline(fs: &MemFs, capacity: u64) -> TierFlush<MemFs> {
        TierFlush::new(
            fs.clone(),
            TierFlushConfig {
                shard_dir: Path::new("shard-0").to_path_buf(),
                cell: 0,
                ns: NsId(17),
                mode: TierIoMode::Buffered,
                file_capacity: capacity,
                slice_bytes: 4096,
                record_len_max: 4096,
            },
            0,
            &census(),
        )
    }

    /// Capacity rotation: a range that would overflow seals the file at
    /// the preceding range boundary (early-seal, ADR-0056 D2); ranges
    /// stay exact, adjacent, and footer-verified.
    #[test]
    fn capacity_rotation_seals_at_range_boundaries() {
        let fs = MemFs::new();
        let mut flush = pipeline(&fs, 1000);
        let a0 = LogicalAddr::ZERO;
        flush.append_range(a0, &[0xA0; 600]).expect("append");
        // 600 + 600 > 1000: the active file seals at 600 exactly.
        let a1 = a0.advanced(600).expect("fits");
        flush.append_range(a1, &[0xA1; 600]).expect("append");
        flush.sync().expect("sync");
        assert_eq!(flush.sealed().len(), 1);
        let first = &flush.sealed()[0];
        assert_eq!(first.base, a0);
        assert_eq!(first.data_len, 600, "sealed at the range boundary, no padding");
        assert_eq!(first.reason, SealReason::Capacity);
        assert_eq!(flush.append_cursor(), Some(1200), "second range in the next file");
        // A single range larger than the whole capacity still lands as
        // one valid file (D2's oversized rule).
        let a2 = a1.advanced(600).expect("fits");
        flush.append_range(a2, &[0xA2; 2000]).expect("append");
        flush.seal_shutdown().expect("seal");
        assert_eq!(flush.sealed().len(), 3);
        assert_eq!(flush.sealed()[2].data_len, 2000);
        for meta in flush.sealed() {
            let image = fs.contents(&meta.path).expect("file exists");
            let summary = inspect_tier_bytes(&image).expect("valid sealed image");
            assert_eq!(summary.sealed.expect("sealed").data_len, meta.data_len);
            assert_eq!(summary.first_bad_frame, None);
        }
    }

    /// A gap seal closes the file with `RingTopGap` and the next range
    /// starts a new file at the post-gap address; the confirmable end
    /// tracks the claim rule (full frames unsealed, everything at seal).
    #[test]
    fn gap_seal_and_confirmable_end() {
        let fs = MemFs::new();
        let mut flush = pipeline(&fs, 1 << 20);
        let payload = vec![0x5B; TIER_FRAME_DATA + 100];
        flush.append_range(LogicalAddr::ZERO, &payload).expect("append");
        assert_eq!(flush.confirmable_end(), Some(0), "nothing durable before sync");
        flush.sync().expect("sync");
        assert_eq!(
            flush.confirmable_end(),
            Some(TIER_FRAME_DATA as u64),
            "partial tail frame held back while unsealed"
        );
        flush.seal_for_gap().expect("gap seal");
        assert_eq!(
            flush.confirmable_end(),
            Some(payload.len() as u64),
            "seal makes the whole file claimable"
        );
        assert_eq!(flush.sealed()[0].reason, SealReason::RingTopGap);
        // Post-gap: the next range opens file 1 at its own base.
        let after_gap = LogicalAddr::from_raw(90_000).expect("fits");
        flush.append_range(after_gap, &[0x5C; 64]).expect("append");
        assert_eq!(flush.sealed().len(), 1);
        assert_eq!(flush.active().expect("active").1, after_gap);
    }

    /// Contiguity is a contract: skipping bytes without a gap seal is a
    /// programmer error, refused loudly.
    #[test]
    #[should_panic(expected = "contiguous")]
    fn non_contiguous_range_without_gap_seal_panics() {
        let fs = MemFs::new();
        let mut flush = pipeline(&fs, 1 << 20);
        flush.append_range(LogicalAddr::ZERO, &[0x11; 64]).expect("append");
        let skip = LogicalAddr::from_raw(1000).expect("fits");
        let _ = flush.append_range(skip, &[0x22; 64]);
    }

    /// Seals hand their open file handles to the cold-read table
    /// (M4-S26): one handle per seal, drained exactly once, ids matching
    /// the sealed catalog in seal order.
    #[test]
    fn sealed_handles_drain_once_in_seal_order() {
        let fs = MemFs::new();
        let mut flush = pipeline(&fs, 1000);
        flush.append_range(LogicalAddr::ZERO, &[0xA0; 600]).expect("append");
        let a1 = LogicalAddr::ZERO.advanced(600).expect("fits");
        flush.append_range(a1, &[0xA1; 600]).expect("append"); // capacity seal of file 0
        flush.seal_for_gap().expect("gap seal of file 1");
        let handles = flush.take_sealed_handles();
        let ids: Vec<u32> = handles.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, vec![0, 1], "one handle per seal, in seal order");
        assert!(flush.take_sealed_handles().is_empty(), "drained exactly once");
        assert!(flush.active_raw_fd().is_none(), "no active file after a gap seal");
    }

    /// The fatal fsync class classifies as `Fsync` (§8.4) and
    /// `is_fatal` routes it — the fail-stop contract's typed surface.
    #[test]
    fn fsync_failure_is_fatal_typed() {
        let fs = MemFs::new();
        let mut flush = pipeline(&fs, 1 << 20);
        flush.append_range(LogicalAddr::ZERO, &[0x33; 64]).expect("append");
        fs.fail_next_sync_data();
        let err = flush.sync().expect_err("injected fsync failure");
        assert!(err.is_fatal(), "fsync failures are the §8.4 class");
        assert!(err.to_string().contains("FATAL"), "the message says stop");
    }

    /// ADR-0167 D4: an unaddressable round write position is the second
    /// fatal cause — it routes to fail-stop, and it is never storage-full
    /// (the device-full latch would otherwise take it for a space refusal).
    #[test]
    fn unaddressable_is_fatal_not_storage_full() {
        let offset_bytes = i64::MAX.cast_unsigned() + 1;
        let err = TierFlushError::Unaddressable { path: Path::new("ns/cold").into(), offset_bytes };
        assert!(err.is_fatal(), "an unaddressable position stops the cell");
        assert!(!err.is_storage_full(), "no space was refused");
        let text = err.to_string();
        assert!(text.contains("FATAL"), "the message says stop: {text}");
        assert!(text.contains(&offset_bytes.to_string()), "the message names the value: {text}");
        // The refusal is the driver-op bound, not the kernel's range: a
        // position in (bound, i64::MAX] is a valid loff_t (ADR-0167 D1).
        let bound = FILE_OFFSET_BYTES_MAX.to_string();
        assert!(text.contains(&bound), "the message names the bound it crossed: {text}");
        assert!(!text.contains("loff_t"), "the message blames the bound, not loff_t: {text}");
    }

    // ---- reactor drive (M4.5-S31, ADR-0084) ----

    use crate::fs::sim::SimDisk;
    use crate::tier::RoundEffect;
    use inf_foundation::fault::FaultSpec;

    fn sim_config(capacity: u64, span_bytes_max: u64) -> TierFlushConfig {
        TierFlushConfig {
            shard_dir: Path::new("shard-0").to_path_buf(),
            cell: 0,
            ns: NsId(17),
            mode: TierIoMode::Buffered,
            file_capacity: capacity,
            slice_bytes: 4096,
            record_len_max: span_bytes_max,
        }
    }

    fn sim_seam_pipeline(disk: &SimDisk, capacity: u64) -> TierFlush<SimDisk> {
        TierFlush::new(disk.clone(), sim_config(capacity, 1 << 20), 0, &census())
    }

    /// A reactor-drive pipeline on the sim disk whose spans reach
    /// `span_bytes_max`, file ids from `next_id`.
    fn sim_reactor(
        disk: &SimDisk,
        capacity: u64,
        span_bytes_max: u64,
        next_id: u32,
    ) -> TierFlush<SimDisk> {
        let config = sim_config(capacity, span_bytes_max);
        let mut flush = TierFlush::new(disk.clone(), config, next_id, &census());
        flush.set_drive(TierDrive::Reactor);
        flush
    }

    fn sim_pipeline(disk: &SimDisk, capacity: u64) -> TierFlush<SimDisk> {
        sim_reactor(disk, capacity, 1 << 20, 0)
    }

    fn span(addr: u64, len: usize, gap_end: Option<u64>) -> Selection {
        Selection::Span {
            addr: LogicalAddr::from_raw(addr).expect("fits"),
            len: std::num::NonZeroU64::new(len as u64).expect("a span holds a byte"),
            gap_end,
        }
    }

    /// Plans and stages one round, the store's way: a refusal stages
    /// nothing and is returned.
    fn stage(
        flush: &mut TierFlush<SimDisk>,
        selection: Selection,
        bytes: &[u8],
    ) -> Result<(), TierFlushError> {
        match flush.plan_round(Some(selection)) {
            Planned::Round(plan) => {
                flush.stage_round(plan, bytes);
                Ok(())
            }
            Planned::Refused(err) => Err(err),
            Planned::Nothing => panic!("a selection plans a round or a refusal"),
        }
    }

    /// Executes a staged round the plane's way — every write, then every
    /// barrier (fdatasync covers only completed writes), each op sent and
    /// completed through the round's own states — and returns the deferred
    /// effects.
    fn run_round(disk: &SimDisk, flush: &mut TierFlush<SimDisk>) -> Vec<RoundEffect> {
        let writes = flush.round_write_count();
        for index in 0..writes {
            let op = flush.round_op(index);
            assert!(!op.is_barrier, "writes lead the op list");
            assert_eq!(op.state, OpState::Unsent);
            disk.driver_write_at(op.fd, op.offset, op.bytes).expect("driver write");
            assert!(flush.mark_op_sent(index));
            assert!(flush.complete_op(index, false));
        }
        for index in writes..flush.round_op_count() {
            let op = flush.round_op(index);
            assert!(op.is_barrier, "barriers trail the op list");
            disk.driver_fdatasync(op.fd).expect("driver barrier");
            assert!(flush.mark_op_sent(index));
            assert!(flush.complete_op(index, false));
        }
        flush.finish_round().collect()
    }

    /// Applies a round's effects the store's way; returns the sealed
    /// handles the completion handed back.
    fn apply(flush: &mut TierFlush<SimDisk>, effects: &[RoundEffect]) -> Vec<u32> {
        let mut sealed = Vec::new();
        for effect in effects {
            match *effect {
                RoundEffect::DurableTo { data_len } => flush.confirm_durable_to(data_len),
                RoundEffect::SealCommit => sealed.push(flush.commit_oldest_seal().0),
                RoundEffect::GapCross { .. } => {}
            }
        }
        sealed
    }

    fn image(disk: &SimDisk, path: &Path) -> Vec<u8> {
        let file = disk.open_read(path).expect("file exists");
        let size = file.file_size().expect("size") as usize;
        let mut bytes = vec![0u8; size];
        let mut read = 0;
        while read < size {
            let n = file.read_at(read as u64, &mut bytes[read..]).expect("read");
            assert!(n > 0, "no EOF inside the image");
            read += n;
        }
        bytes
    }

    /// A planned round performs no device I/O at stage time and advances
    /// no durability watermark until its effects apply — `durable_len`
    /// and the claim bound move only at the barrier's completion
    /// (ADR-0084 D2, the §3.1 chain).
    #[test]
    fn queued_round_defers_durability_to_completion() {
        let disk = SimDisk::new();
        let mut flush = sim_pipeline(&disk, 1 << 20);
        let payload = vec![0x5D; TIER_FRAME_DATA + 1908];
        stage(&mut flush, span(0, payload.len(), None), &payload).expect("stage");
        // Header + one full frame + the partial tail frame, one barrier
        // on the file, two dirent barriers from the creation.
        assert_eq!(flush.round_write_count(), 3, "header + batch + tail");
        assert_eq!(flush.round_barrier_count(), 3, "file + shard dir + cold dir");
        assert_eq!(flush.confirmable_end(), Some(0), "nothing claimable before completion");
        let effects = run_round(&disk, &mut flush);
        assert_eq!(effects, [RoundEffect::DurableTo { data_len: payload.len() as u64 }]);
        apply(&mut flush, &effects);
        assert_eq!(
            flush.confirmable_end(),
            Some(TIER_FRAME_DATA as u64),
            "claim rule holds: the partial tail frame waits for the seal"
        );
        assert_eq!(flush.pool_outstanding(), 0, "every window returned at the round's end");
    }

    /// A capacity seal staged in a round commits to the catalog only at
    /// effect application: mid-round the file is a pending seal (visible
    /// to manifest/cold lookups), afterwards it is sealed on disk with a
    /// verified footer and the completion hands its handle back.
    #[test]
    fn queued_capacity_seal_commits_at_completion() {
        let disk = SimDisk::new();
        let mut flush = sim_pipeline(&disk, 1000);
        stage(&mut flush, span(0, 600, None), &[0xA0; 600]).expect("file A");
        let effects = run_round(&disk, &mut flush);
        apply(&mut flush, &effects);
        stage(&mut flush, span(600, 600, None), &[0xA1; 600]).expect("the rotation to B");
        assert_eq!(flush.sealed().len(), 0, "no catalog commit at stage time");
        assert_eq!(flush.pending_seal_count(), 1, "the capacity seal is pending");
        let pending: Vec<_> = flush.pending_seals().collect();
        assert_eq!((pending[0].id, pending[0].data_len), (0, 600));
        let effects = run_round(&disk, &mut flush);
        assert_eq!(
            effects[0],
            RoundEffect::SealCommit,
            "the seal precedes the new file's durability in stage order"
        );
        let sealed = apply(&mut flush, &effects);
        assert_eq!(sealed, [0], "the completion hands A's fd to the cold-read table");
        assert_eq!(flush.pending_seal_count(), 0);
        assert_eq!(flush.sealed().len(), 1);
        assert_eq!(flush.sealed()[0].data_len, 600, "sealed at the span boundary");
        assert_eq!(flush.sealed()[0].reason, SealReason::Capacity);
        assert!(flush.take_sealed_handles().is_empty(), "the reactor drive parks no handle");
        let img = image(&disk, &flush.sealed()[0].path);
        let summary = crate::tier::inspect_tier_bytes(&img).expect("valid sealed image");
        assert_eq!(summary.sealed.expect("footer present").data_len, 600);
        assert_eq!(summary.first_bad_frame, None, "every frame verifies");
        assert_eq!(
            flush.confirmable_end(),
            Some(600),
            "file A is fully claimable; file B's partial tail frame is \
             held back until its own seal (ADR-0056 D5)"
        );
    }

    /// A ring-top gap stages `SealCommit` before `GapCross` — `flushed`
    /// may cross the hole only after the covering seal's barrier
    /// (ADR-0052 D2, completion-gated).
    #[test]
    fn queued_gap_orders_seal_before_crossing() {
        let disk = SimDisk::new();
        let mut flush = sim_pipeline(&disk, 1 << 20);
        stage(&mut flush, span(0, 700, Some(90_000)), &[0x5B; 700]).expect("stage");
        let effects = run_round(&disk, &mut flush);
        assert_eq!(effects, [RoundEffect::SealCommit, RoundEffect::GapCross { to: 90_000 }]);
        assert_eq!(apply(&mut flush, &effects), [0]);
        assert_eq!(flush.sealed()[0].reason, SealReason::RingTopGap);
        assert_eq!(
            flush.confirmable_end(),
            Some(700),
            "the whole sealed file is claimable after commit"
        );
        // A gap at the cursor with no file active is an effects-only round.
        stage(&mut flush, Selection::Gap { end: 95_000 }, &[]).expect("a gap alone");
        assert_eq!((flush.round_active(), flush.round_op_count()), (true, 0));
        assert_eq!(run_round(&disk, &mut flush), [RoundEffect::GapCross { to: 95_000 }]);
    }

    /// A refused creation stages nothing — no round, no window, no id, and
    /// so no staged op on a handle nobody owns — and the retry creates the
    /// file.
    #[test]
    fn a_failed_creation_stages_nothing() {
        let disk = SimDisk::new();
        let mut flush = sim_pipeline(&disk, 1 << 20);
        inf_foundation::fault::arm(crate::fault::TIER_DIR_OPEN_FAIL, FaultSpec::Nth(1));
        let err = stage(&mut flush, span(0, 64, None), &[0x22; 64])
            .expect_err("the directory hold is refused");
        assert!(matches!(err, TierFlushError::Io { .. }), "{err}");
        assert!(!flush.round_active(), "a refused plan left a round");
        assert!(flush.active().is_none(), "no file is active after a refused creation");
        assert_eq!((flush.next_file_id(), flush.pool_outstanding()), (0, 0));
        stage(&mut flush, span(0, 64, None), &[0x22; 64]).expect("the retry creates");
        assert!(flush.round_handles_owned());
        assert_eq!(flush.round_write_count(), 2, "header + tail frame");
        assert_eq!(flush.round_barrier_count(), 3, "file + shard dir + cold dir");
        let effects = run_round(&disk, &mut flush);
        assert_eq!(effects, [RoundEffect::DurableTo { data_len: 64 }]);
        assert_eq!(flush.next_file_id(), 1, "one file was created, once");
        inf_foundation::fault::disarm_all();
    }

    /// The rotation whose next file is refused: the plan stages nothing,
    /// so file A stays active and unsealed with every byte it held, no
    /// seal is pending and no id is consumed; the retry rotates and
    /// creates B. (Before the plan, the round kept A's seal, staged ahead
    /// of the refused creation.)
    #[test]
    fn a_refused_rotation_leaves_the_active_file_as_it_was() {
        let disk = SimDisk::new();
        let mut flush = sim_pipeline(&disk, 1000);
        stage(&mut flush, span(0, 600, None), &[0xA0; 600]).expect("file A");
        let effects = run_round(&disk, &mut flush);
        apply(&mut flush, &effects);
        inf_foundation::fault::arm(crate::fault::TIER_DIR_OPEN_FAIL, FaultSpec::Nth(1));
        stage(&mut flush, span(600, 600, None), &[0xA1; 600]).expect_err("B's hold is refused");
        assert!(!flush.round_active(), "nothing is staged");
        assert_eq!(flush.pending_seal_count(), 0, "A's seal is not staged");
        assert_eq!(flush.active().map(|(id, _, data, ..)| (id, data)), Some((0, 600)));
        assert_eq!((flush.next_file_id(), flush.pool_outstanding()), (1, 0));
        stage(&mut flush, span(600, 600, None), &[0xA1; 600]).expect("the retry rotates");
        assert!(flush.round_handles_owned());
        let effects = run_round(&disk, &mut flush);
        assert_eq!(
            effects[..2],
            [RoundEffect::SealCommit, RoundEffect::DurableTo { data_len: 600 }]
        );
        assert_eq!(apply(&mut flush, &effects), [0]);
        assert_eq!(flush.sealed()[0].data_len, 600);
        assert_eq!(flush.next_file_id(), 2);
        assert_eq!(flush.active().map(|(id, ..)| id), Some(1), "B is the active file");
        inf_foundation::fault::disarm_all();
    }

    /// A round stages at most two seals (ADR-0155 D3): a span far above a
    /// small file target rotates once and stays whole in one oversized
    /// file, and a trailing gap seals that file — never a seal per target.
    #[test]
    fn a_round_stages_at_most_two_seals() {
        let disk = SimDisk::new();
        let mut flush = sim_reactor(&disk, 4 << 10, 64 << 10, 0);
        stage(&mut flush, span(0, 3000, None), &[0x5A; 3000]).expect("the first file");
        let effects = run_round(&disk, &mut flush);
        apply(&mut flush, &effects);
        let len = 17 * 4000;
        let bytes = vec![0x5A; len];
        let gap_end = 3000 + len as u64 + 4096;
        stage(&mut flush, span(3000, len, Some(gap_end)), &bytes).expect("the span");
        assert_eq!(
            flush.pending_seal_count(),
            2,
            "a round holds at most 2 staged seals: the rotation's and the gap's, after a \
             {len} B span over a 4 KiB file target"
        );
        let effects = run_round(&disk, &mut flush);
        let expected = [
            RoundEffect::SealCommit,
            RoundEffect::SealCommit,
            RoundEffect::GapCross { to: gap_end },
        ];
        assert_eq!(effects, expected);
        assert_eq!(apply(&mut flush, &effects), [0, 1]);
        assert_eq!(flush.sealed()[1].data_len, len as u64, "the span is one oversized file");
    }

    /// File ids never wrap (ADR-0178's file-identity rule as T states it):
    /// creating a file at the last id is refused, typed and not fatal,
    /// before any effect — no file, no round, no window, no id consumed; one
    /// below it, one file is created and the next creation refused, on the
    /// reactor drive and on the seam.
    #[test]
    fn a_file_creation_at_the_last_id_is_refused_before_any_effect() {
        let disk = SimDisk::new();
        let mut flush = sim_reactor(&disk, 1 << 20, 1 << 20, u32::MAX);
        let inodes = disk.inode_count();
        let err = stage(&mut flush, span(0, 100, None), &[0x5A; 100])
            .expect_err("the creation is refused");
        assert!(matches!(err, TierFlushError::FileIdsExhausted { .. }), "{err}");
        assert!(!err.is_fatal() && !err.is_storage_full(), "{err}");
        assert_eq!(flush.next_file_id(), u32::MAX, "no id is consumed");
        assert!(!flush.round_active(), "no round is touched");
        assert_eq!(flush.pool_outstanding(), 0, "no window is held");
        assert_eq!(disk.inode_count(), inodes, "no file is created");
        let mut flush = sim_reactor(&disk, 100, 1 << 20, u32::MAX - 1);
        stage(&mut flush, span(0, 100, None), &[0x5A; 100]).expect("the last id's file");
        let effects = run_round(&disk, &mut flush);
        apply(&mut flush, &effects);
        assert_eq!(flush.next_file_id(), u32::MAX);
        let err = stage(&mut flush, span(100, 100, None), &[0x5A; 100])
            .expect_err("the rotation's file is refused");
        assert!(matches!(err, TierFlushError::FileIdsExhausted { .. }), "{err}");
        assert_eq!(flush.active().map(|(id, _, data, ..)| (id, data)), Some((u32::MAX - 1, 100)));
        assert_eq!(flush.pending_seal_count(), 0, "the rotation staged nothing");
        let mut seam =
            TierFlush::new(disk.clone(), sim_config(1 << 20, 1 << 20), u32::MAX, &census());
        let err = seam.append_range(LogicalAddr::ZERO, &[0x5A; 10]).expect_err("seam refuses too");
        assert!(matches!(err, TierFlushError::FileIdsExhausted { .. }), "{err}");
        assert_eq!(seam.next_file_id(), u32::MAX);
    }

    /// ADR-0167 A1: a planned write position the driver cannot carry
    /// refuses the plan, fatal, before any writer effect — no round, no
    /// window, no file, the active writer and its append cursor as they
    /// were — and the pool is free for the next plan.
    #[test]
    fn an_unaddressable_position_refuses_the_plan_before_any_effect() {
        let disk = SimDisk::new();
        let mut flush = sim_pipeline(&disk, 1 << 20);
        stage(&mut flush, span(0, 600, None), &[0x5C; 600]).expect("an active file");
        let effects = run_round(&disk, &mut flush);
        apply(&mut flush, &effects);
        let before = (flush.append_cursor(), flush.active().map(|a| a.0), flush.next_file_id());
        let inodes = disk.inode_count();
        let planted = (i64::MAX as u64) + 1;
        let planned =
            flush.plan_round_with(Some(span(600, 5000, None)), |index, writes, position| {
                if index + 1 == writes { planted } else { position }
            });
        let Planned::Refused(err) = planned else { panic!("the plan was not refused") };
        assert!(
            matches!(
                err,
                TierFlushError::Unaddressable { offset_bytes, .. } if offset_bytes == planted
            ),
            "{err}"
        );
        assert!(err.is_fatal() && !err.is_storage_full());
        assert!(!flush.round_active(), "no round");
        assert_eq!(flush.pool_outstanding(), 0, "no window");
        assert_eq!(disk.inode_count(), inodes, "no file");
        assert_eq!(
            (flush.append_cursor(), flush.active().map(|a| a.0), flush.next_file_id()),
            before,
            "the writer as it was"
        );
        stage(&mut flush, span(600, 5000, None), &[0x5C; 5000]).expect("the pool is free");
    }

    /// The round's resource envelope (ADR-0155 D3) over the writer's own
    /// staging: file targets of 4 KiB and 1 MiB; incoming writer lengths 0,
    /// 1, F − 1, F and F + 1; spans of 0, 1, D − 1, D, D + 1, Q − 1, Q,
    /// Q + 1, M and 64 MiB bytes; a trailing gap absent or present — 200
    /// cases. Each staged round is read against the formulas, then run and
    /// applied, and every window comes back. The reservation is the round plan's
    /// own count, so a plan that reserves fewer windows than its span takes
    /// stops at the stage's take.
    #[test]
    fn the_round_envelope_holds_over_its_two_hundred_cases() {
        const D: u64 = TIER_FRAME_DATA as u64;
        const V: u64 = crate::tier::TIER_FRAME_BYTES as u64;
        const Q: u64 = 256 * D;
        const M: u64 = 16_777_483;
        let mut cases = 0;
        for file in [4 << 10, 1 << 20] {
            for incoming in [0, 1, file - 1, file, file + 1] {
                for a in [0, 1, D - 1, D, D + 1, Q - 1, Q, Q + 1, M, 64 << 20] {
                    for gap in [false, true] {
                        envelope_case(file, incoming, a, gap);
                        cases += 1;
                    }
                }
            }
        }
        assert_eq!(cases, 200);
        let _ = V;
    }

    fn envelope_case(file: u64, incoming: u64, a: u64, gap: bool) {
        const D: u64 = TIER_FRAME_DATA as u64;
        const V: u64 = crate::tier::TIER_FRAME_BYTES as u64;
        const Q: u64 = 256 * D;
        let disk = SimDisk::new();
        let mut flush = sim_reactor(&disk, file, crate::limits::TIER_SPAN_BYTES_MAX, 0);
        if incoming > 0 {
            let bytes = vec![0x11; incoming as usize];
            stage(&mut flush, span(0, bytes.len(), None), &bytes).expect("the priming round");
            let effects = run_round(&disk, &mut flush);
            apply(&mut flush, &effects);
        }
        let gap_end = incoming + a + 4096;
        let selection = match (a, gap) {
            (0, false) => {
                // Nothing at the cursor: the plan answers so and changes
                // nothing.
                assert!(matches!(flush.plan_round(None), Planned::Nothing));
                assert!(!flush.round_active());
                assert_eq!(flush.pool_outstanding(), 0);
                return;
            }
            (0, true) => Selection::Gap { end: gap_end },
            (_, gap) => span(incoming, a as usize, gap.then_some(gap_end)),
        };
        let bytes = vec![0x22; a as usize];
        stage(&mut flush, selection, &bytes).expect("the round stages");
        let case = format!("F {file} incoming {incoming} A {a} gap {gap}");
        let batches = a.div_ceil(Q) as usize;
        let writes = flush.round_write_count();
        let barriers = flush.round_barrier_count();
        let device: u64 = (0..writes).map(|i| flush.round_op(i).bytes.len() as u64).sum();
        let windows = flush.pool_outstanding();
        assert!(windows <= batches + 5, "{case}: {windows} windows past ceil(A/Q) + 5");
        assert!(writes <= batches + 5, "{case}: {writes} writes");
        assert!(barriers <= 4, "{case}: {barriers} barriers");
        assert!(writes + barriers <= batches + 9, "{case}: ops");
        assert!(writes + barriers <= ROUND_OPS_MAX_TEST, "{case}: the token op index");
        assert!(device <= V * (a.div_ceil(D) + 4), "{case}: {device} device bytes");
        assert!(flush.pending_seal_count() <= 2, "{case}: seals");
        let effects = run_round(&disk, &mut flush);
        assert!(effects.len() <= 3, "{case}: {effects:?}");
        apply(&mut flush, &effects);
        assert_eq!(flush.pool_outstanding(), 0, "{case}: every window came back");
    }

    const ROUND_OPS_MAX_TEST: usize = crate::tier::ROUND_OPS_MAX;

    /// A held completion reads the window its op was staged in until the
    /// round ends (ADR-0155 D5): the pool takes a window back only at the
    /// round's finish, never at a send, so the pool's next user writing
    /// every window it holds free cannot reach a write still in flight.
    #[test]
    fn a_held_completion_reads_its_own_window_until_the_round_ends() {
        let disk = SimDisk::new();
        let mut flush = sim_pipeline(&disk, 1 << 20);
        let payload = vec![0x6C; 3 * TIER_FRAME_DATA + 10];
        stage(&mut flush, span(0, payload.len(), None), &payload).expect("stage");
        let writes = flush.round_write_count();
        let staged: Vec<Vec<u8>> = (0..writes).map(|i| flush.round_op(i).bytes.to_vec()).collect();
        for index in 0..writes {
            assert!(flush.mark_op_sent(index), "write {index} sent");
        }
        flush.pool.overwrite_free(0xEE);
        for (index, bytes) in staged.iter().enumerate() {
            assert!(
                flush.round_op(index).bytes == &bytes[..],
                "write {index}: a held completion reads a recycled window"
            );
        }
        for index in 0..writes {
            assert!(flush.complete_op(index, false));
        }
        for index in writes..flush.round_op_count() {
            assert!(flush.mark_op_sent(index) && flush.complete_op(index, false));
        }
        let _ = flush.finish_round();
        assert_eq!(flush.pool_outstanding(), 0);
    }

    /// Op states move only forward through the round's own transitions: a
    /// send needs `Unsent`, a completion `Sent`; a repeated or stale
    /// completion changes nothing; a failed write is made `Unsent` again
    /// for its retry, byte-identical.
    #[test]
    fn op_states_move_only_through_their_transitions() {
        let disk = SimDisk::new();
        let mut flush = sim_pipeline(&disk, 1 << 20);
        stage(&mut flush, span(0, 100, None), &[0x33; 100]).expect("stage");
        assert!(!flush.complete_op(0, false), "an unsent op cannot complete");
        assert!(flush.mark_op_sent(0));
        assert!(!flush.mark_op_sent(0), "a sent op is not sent twice");
        let bytes = flush.round_op(0).bytes.to_vec();
        assert!(flush.complete_op(0, true));
        assert_eq!(flush.round_op(0).state, OpState::Failed);
        assert!(!flush.complete_op(0, false), "a repeated completion changes nothing");
        assert_eq!(flush.retry_failed_writes(), 1);
        assert_eq!(flush.round_op(0).state, OpState::Unsent);
        assert_eq!(flush.round_op(0).bytes, &bytes[..], "the retry is byte-identical");
    }

    /// The pool's own state is no refusal: a plan made while an earlier
    /// plan's reservation is held (planned, never staged) stops the cell,
    /// where a `bool` answer once reported it as a span above the pool.
    #[test]
    #[should_panic(expected = "a tier window pool reserved twice")]
    fn a_plan_while_a_reservation_is_held_stops_the_cell() {
        let disk = SimDisk::new();
        let mut flush = sim_pipeline(&disk, 1 << 20);
        let Planned::Round(_held) = flush.plan_round(Some(span(0, 100, None))) else {
            panic!("the first plan is admitted")
        };
        let _ = flush.plan_round(Some(span(0, 100, None)));
    }

    /// A reservation with a window out stops the cell: the windows a round
    /// took come back only at its end, so a reservation beside them is a
    /// defect of the pool's owner (the round plan's own staged-round check
    /// precedes this one; the pool answers for itself).
    #[test]
    #[should_panic(expected = "a tier window pool reserved with windows out")]
    fn a_reservation_with_windows_out_stops_the_cell() {
        let mut pool = WindowPool::new(1 << 20, &census()).expect("a pool");
        let one = crate::tier::WindowNeeds { batch: 1, single: 0 };
        assert_eq!(pool.reserve(one), crate::tier::Reservation::Reserved);
        let _window = pool.take(crate::tier::WindowKind::Batch);
        let _ = pool.reserve(one);
    }

    /// A span the pool cannot hold is the round plan's typed refusal and changes
    /// nothing; the pool answers `OverPool`, and its own state is as it was.
    #[test]
    fn a_span_above_the_pool_is_refused_typed_before_any_effect() {
        let disk = SimDisk::new();
        let mut flush = sim_reactor(&disk, 1 << 30, 1 << 20, 0);
        let inodes = disk.inode_count();
        let over = 3 * crate::tier::TIER_BATCH_PAYLOAD_BYTES as usize;
        let Planned::Refused(err) = flush.plan_round(Some(span(0, over, None))) else {
            panic!("a span above the pool is refused")
        };
        assert!(matches!(err, TierFlushError::SpanOverPool { .. }), "{err}");
        assert!(err.is_fatal() && !err.is_storage_full());
        assert!(!flush.round_active(), "no round");
        assert_eq!(flush.pool_outstanding(), 0, "no window");
        assert_eq!(disk.inode_count(), inodes, "no file");
        stage(&mut flush, span(0, 100, None), &[0x44; 100]).expect("the pool holds no reservation");
    }

    /// A span continues the active file's stream whether or not it rotates
    /// the file: one off the write cursor stops the cell on the rotating
    /// arm too, where a rotation would make it the new file's base.
    #[test]
    #[should_panic(expected = "a tier span starts at the write cursor")]
    fn a_rotating_span_off_the_write_cursor_stops_the_cell() {
        let disk = SimDisk::new();
        let mut flush = sim_pipeline(&disk, 4096);
        stage(&mut flush, span(0, 600, None), &[0x5C; 600]).expect("an active file");
        let effects = run_round(&disk, &mut flush);
        apply(&mut flush, &effects);
        let _ = stage(&mut flush, span(5000, 4000, None), &[0x5C; 4000]);
    }

    /// Fd-less filesystems never take the reactor drive, and a pipeline
    /// refuses a drive flip while a round is staged.
    #[test]
    #[should_panic(expected = "drive change with a round in flight")]
    fn drive_flip_with_a_staged_round_panics() {
        let disk = SimDisk::new();
        let mut flush = sim_pipeline(&disk, 1 << 20);
        stage(&mut flush, span(0, 64, None), &[0x11; 64]).expect("stage");
        flush.set_drive(TierDrive::Seam);
    }

    /// F-L04-13: a writer that staged on the seam owns a private batch
    /// window. The drive switch must not let that window reach a round —
    /// `recycle` would return it to the pool and the count would go one
    /// below truth (a debug panic; `u32::MAX` in release, so the L5 bound
    /// assert misfires later with the wrong message).
    #[test]
    fn seam_window_never_reaches_the_pool_across_a_drive_switch() {
        let disk = SimDisk::new();
        let mut flush = sim_seam_pipeline(&disk, 1 << 20);
        let frame = vec![0xA5u8; TIER_FRAME_DATA];
        flush.append_range(LogicalAddr::ZERO, &frame).expect("seam append");
        flush.sync().expect("seam sync"); // batch flushed, the window retained
        flush.set_drive(TierDrive::Reactor);
        stage(&mut flush, span(TIER_FRAME_DATA as u64, frame.len(), None), &frame)
            .expect("queued append");
        let effects = run_round(&disk, &mut flush);
        apply(&mut flush, &effects);
        assert_eq!(flush.pool_outstanding(), 0, "every window a round carries is the pool's");
        assert_eq!(flush.confirmable_end(), Some(2 * TIER_FRAME_DATA as u64));
        let (_, _, _, _, path) = flush.active().expect("active");
        let back = image(&disk, path);
        let span = 2 * crate::tier::TIER_FRAME_BYTES;
        let frames = &back[TIER_HEADER_BYTES_TEST..TIER_HEADER_BYTES_TEST + span];
        let mut out = Vec::new();
        tier_extract(frames, 0, 2 * TIER_FRAME_DATA, &mut out).expect("both frames verify");
        assert!(out.iter().all(|&b| b == 0xA5), "both frames landed");
    }

    const TIER_HEADER_BYTES_TEST: usize = crate::tier::TIER_HEADER_BYTES;

    /// F-L04-13: the switch refuses staged, unflushed seam frames — a
    /// second named cause, so the pool never inherits half a batch.
    #[test]
    #[should_panic(expected = "drive change with staged frames")]
    fn drive_flip_with_staged_seam_frames_panics() {
        let disk = SimDisk::new();
        let mut flush = sim_seam_pipeline(&disk, 1 << 20);
        flush.append_range(LogicalAddr::ZERO, &[0x11; TIER_FRAME_DATA]).expect("seam append");
        flush.set_drive(TierDrive::Reactor);
    }
}

#[cfg(test)]
mod boot_tests {
    use std::path::Path;

    use super::*;
    use crate::fs::mem::MemFs;
    use crate::tier::{TIER_FRAME_DATA, tier_file_name};

    fn steps() -> u64 {
        SPAN_LOCATE_STEPS.with(|c| c.replace(0))
    }

    fn config(capacity: u64) -> TierFlushConfig {
        TierFlushConfig {
            shard_dir: Path::new("shard-0").to_path_buf(),
            cell: 0,
            ns: NsId(17),
            mode: TierIoMode::Buffered,
            file_capacity: capacity,
            slice_bytes: 4096,
            record_len_max: 4096,
        }
    }

    /// A catalogue of `files` manifested files, 900 bytes each at 1000-byte
    /// bases (a range gap after every file), with a handle per file (the
    /// files exist, empty: the locate cost is what is measured; a read
    /// past the header fails short).
    fn catalog(files: u32) -> BootFlush<MemFs> {
        let fs = MemFs::new();
        fs.create_dir_all(Path::new("shard-0/cold")).expect("dir");
        let mut sealed = Vec::with_capacity(files as usize);
        let mut handles = Vec::with_capacity(files as usize);
        for id in 0..files {
            let path = Path::new("shard-0/cold").join(tier_file_name(id));
            handles.push((id, fs.create_segment(&path, 0).expect("create")));
            sealed.push(TierFileMeta {
                id,
                base: LogicalAddr::ZERO.advanced(u64::from(id) * 1000).expect("fits"),
                data_len: 900,
                reason: SealReason::Capacity,
                path,
            });
        }
        BootFlush::new(
            TierFlush::with_catalog(fs, config(1000), files, sealed, &super::tests::census()),
            handles,
        )
    }

    /// L04 perf row: the rebuild and the replay settles ask once per
    /// slot against a catalogue of thousands — locating the covering
    /// file and its handle is a bisection, never a scan.
    #[test]
    fn locating_a_key_window_bisects_the_catalogue() {
        let mut boot = catalog(10_000);
        steps();
        // A hit in the last file: the file is empty, so the read is short
        // — the locate cost is what is measured.
        let err = boot.read_key_window(9_999 * 1000 + 10).expect_err("no file bytes");
        assert!(matches!(err, SettleReadError::Short { addr: 9_999_010, .. }), "{err}");
        let hit = steps();
        let err = boot.read_key_window(10_000_000).expect_err("past every file");
        assert!(matches!(err, SettleReadError::NoRange { addr: 10_000_000 }), "{err}");
        let miss_past = steps();
        let err = boot.read_key_window(4_999 * 1000 + 950).expect_err("in a range gap");
        assert!(matches!(err, SettleReadError::NoRange { addr: 4_999_950 }), "{err}");
        let miss_gap = steps();
        for (what, n) in [("hit", hit), ("miss past", miss_past), ("miss in a gap", miss_gap)] {
            assert!(n <= 16, "{what}: examined {n} catalogue entries for 10 000 files");
        }
    }

    /// ADR-0174 D2 rule 4: a boot pipeline claims every byte its barrier
    /// covered, the partial tail frame included; the settle read answers
    /// from that frame through the writer's own handle before and after
    /// the frame is rewritten in place; and the one exit restores the
    /// live rule, returning a handle per sealed file.
    #[test]
    fn a_boot_pipeline_claims_the_barrier_and_reads_its_own_tail_frame() {
        let fs = MemFs::new();
        let mut boot = BootFlush::new(
            TierFlush::new(fs.clone(), config(1 << 20), 0, &super::tests::census()),
            Vec::new(),
        );
        let payload = vec![0x5B; TIER_FRAME_DATA + 100];
        boot.append_range(LogicalAddr::ZERO, &payload).expect("append");
        assert_eq!(boot.confirmable_end(), Some(0), "nothing durable before the barrier");
        boot.sync().expect("barrier");
        assert_eq!(
            boot.confirmable_end(),
            Some(payload.len() as u64),
            "the barrier claim covers the partial tail frame (ADR-0174 D2 rule 4)"
        );
        // A read inside the partial tail frame, through the held writer.
        let window = boot.read_key_window(TIER_FRAME_DATA as u64 + 10).expect("reads");
        assert_eq!(window.left, 90, "left is the claimed end less the address");
        assert_eq!(window.bytes.len(), 90, "clamped to the claimed end");
        assert!(window.bytes.iter().all(|&b| b == 0x5B));
        // The next step extends the same frame in place; both the old
        // and the new bytes read back under the new CRC.
        let next = LogicalAddr::ZERO.advanced(payload.len() as u64).expect("fits");
        boot.append_range(next, &[0x6C; 200]).expect("append");
        boot.sync().expect("barrier");
        let window = boot.read_key_window(TIER_FRAME_DATA as u64 + 10).expect("reads");
        assert_eq!(window.left, 290);
        assert_eq!(window.bytes.len(), TIER_KEY_WINDOW_BYTES, "the whole window");
        assert!(window.bytes[..90].iter().all(|&b| b == 0x5B));
        assert!(window.bytes[90..].iter().all(|&b| b == 0x6C));
        let window = boot.read_key_window(next.to_raw()).expect("reads");
        assert_eq!(window.bytes.len(), 200, "clamped to the claimed end");
        assert!(window.bytes.iter().all(|&b| b == 0x6C));
        // A window that crosses a frame boundary: two frames, one read.
        let window = boot.read_key_window(TIER_FRAME_DATA as u64 - 10).expect("reads");
        assert_eq!(window.bytes.len(), TIER_KEY_WINDOW_BYTES);
        assert!(window.bytes[..110].iter().all(|&b| b == 0x5B), "both frames' bytes");
        assert!(window.bytes[110..].iter().all(|&b| b == 0x6C));
        // Beyond the claimed end: no range.
        let err = boot.read_key_window(next.to_raw() + 200).expect_err("unclaimed");
        assert!(matches!(err, SettleReadError::NoRange { .. }), "{err}");
        // The exit seals the open file, restores the live rule and hands
        // over one handle per sealed file.
        let HandedOver { flush, handles } = boot.hand_over().expect("seals and hands over");
        assert_eq!(flush.sealed().len(), 1);
        assert_eq!(flush.sealed()[0].reason, SealReason::Shutdown);
        assert_eq!(handles.len(), 1, "one held handle per sealed catalogue file");
        assert_eq!(flush.claim, ClaimRule::FullFrames, "the plane's rule");
        assert!(flush.active().is_none());
    }

    /// `tier_dir_open_fail` on the seam drive: the create is refused
    /// before the file exists — typed, no file left behind, the id not
    /// consumed — and the retry creates it. This is the boot pipeline's
    /// create (ADR-0174 D5's EMFILE row).
    #[test]
    fn a_refused_seam_create_leaves_no_file_and_the_retry_creates() {
        use inf_foundation::fault::FaultSpec;
        let fs = MemFs::new();
        let mut boot = BootFlush::new(
            TierFlush::new(fs.clone(), config(1 << 20), 0, &super::tests::census()),
            Vec::new(),
        );
        inf_foundation::fault::arm(crate::fault::TIER_DIR_OPEN_FAIL, FaultSpec::Nth(1));
        let err = boot.append_range(LogicalAddr::ZERO, &[0x11; 64]).expect_err("refused");
        inf_foundation::fault::disarm_all();
        assert!(matches!(err, TierFlushError::Io { .. }), "{err}");
        assert!(boot.active().is_none(), "no file is active");
        assert_eq!(boot.next_file_id(), 0, "the id is not consumed");
        let cold = Path::new("shard-0/cold");
        assert!(fs.list_dir(cold).unwrap_or_default().is_empty(), "no file left behind");
        boot.append_range(LogicalAddr::ZERO, &[0x11; 64]).expect("the retry creates");
        assert_eq!(boot.next_file_id(), 1);
    }

    /// The injected read failure (`replay_settle_read_fail`) answers
    /// typed, naming the address; a CRC failure answers typed too.
    #[test]
    fn a_failed_or_corrupt_settle_read_is_typed() {
        use inf_foundation::fault::FaultSpec;
        let fs = MemFs::new();
        let mut boot = BootFlush::new(
            TierFlush::new(fs.clone(), config(1 << 20), 0, &super::tests::census()),
            Vec::new(),
        );
        boot.append_range(LogicalAddr::ZERO, &[0x11; 500]).expect("append");
        boot.sync().expect("barrier");
        inf_foundation::fault::arm(crate::fault::REPLAY_SETTLE_READ_FAIL, FaultSpec::Nth(1));
        let err = boot.read_key_window(100).expect_err("the injected read failure");
        assert!(matches!(err, SettleReadError::Io { addr: 100, .. }), "{err}");
        assert!(err.to_string().contains("replay_settle_read_fail"), "{err}");
        inf_foundation::fault::disarm_all();
        boot.read_key_window(100).expect("the next read answers");
        // Flip a payload byte on disk: the frame's CRC refuses.
        let (_, _, _, _, path) = boot.active().expect("active");
        let path = path.to_path_buf();
        let at = crate::tier::TIER_HEADER_BYTES as u64 + 7;
        let mut planted = [0u8; 1];
        let mut corruptor = fs.open_write(&path).expect("the test's own handle");
        assert_eq!(corruptor.read_at(at, &mut planted).expect("read"), 1);
        corruptor.write_at(at, &[planted[0] ^ 0xFF]).expect("flip one payload byte");
        let err = boot.read_key_window(100).expect_err("CRC");
        assert!(matches!(err, SettleReadError::Corrupt { addr: 100, frame: 0, .. }), "{err}");
    }
}
