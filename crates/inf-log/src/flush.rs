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
//! tests/DST) and the **reactor drive** (`stage_flush_round` — intents
//! queue on a bounded [`TierRound`], the plane rides them as
//! `IoOp::LogWrite`/`Fdatasync`, and every durability fact defers to a
//! [`RoundEffect`] applied at the round's last barrier completion).
//!
//! fsync failure is fatal-by-default (§8.4, ADR-0056 D4): it surfaces as
//! [`TierFlushError::Fsync`] and the flushed watermark freezes — no
//! caller may catch and continue past it.

use std::io;
use std::path::{Path, PathBuf};

use inf_foundation::LogicalAddr;
use inf_foundation::limits::FILE_OFFSET_BYTES_MAX;

use crate::fs::{SegmentFile, SegmentFs, TierIoMode};
use crate::record::NsId;
use crate::tier::{
    FrameStaging, QueuedSeal, RoundEffect, SealReason, TIER_KEY_WINDOW_BYTES, TierOpView,
    TierRound, TierWriteFailure, TierWriter, WindowPool, tier_extract, tier_frame_offset,
    tier_frame_span,
};

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
}

/// A flush-pipeline failure. Two causes are fatal (§8.4): `Fsync` — the
/// caller must freeze the flushed watermark and stop; constructing this
/// variant is audited by `check-fsync-fail-stop.sh` (ADR-0056 D4) — and
/// `Unaddressable`, a staged write position outside the driver's range
/// (ADR-0167 D4).
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
    /// A staged reactor-drive write position is above the driver's range
    /// (`FILE_OFFSET_BYTES_MAX`, ADR-0167 D4): the round opens nothing and
    /// the cell stops. Fatal because the position was checked after
    /// staging, and a retry would stage the same position. Not
    /// storage-full: no space was refused.
    Unaddressable {
        /// The namespace's tier directory the round writes into.
        path: PathBuf,
        /// The refused position.
        offset_bytes: u64,
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
        }
    }
}

impl std::error::Error for TierFlushError {}

impl TierFlushError {
    /// True for the §8.4 fatal class — a failed fsync or an unaddressable
    /// write position (ADR-0167 D4) — so callers route to the terminal
    /// fail-stop handler without naming the variant.
    #[must_use]
    pub fn is_fatal(&self) -> bool {
        match self {
            // fsync-fail-stop-allow: is_fatal classifier: answers true, takes no action
            TierFlushError::Fsync { .. } => true,
            TierFlushError::Unaddressable { .. } => true,
            TierFlushError::Io { .. } => false,
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
            // A position fault refused no space (ADR-0167 D4).
            TierFlushError::Unaddressable { .. } => false,
        }
    }
}

/// The per-namespace flush pipeline (one per (cell, tiered namespace) —
/// L1: cell-local, single owner).
#[allow(clippy::disallowed_types, reason = "container: T")]
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
    /// Open handles of files sealed since the last
    /// [`take_sealed_handles`](Self::take_sealed_handles) — the plane's
    /// cold-read table drains these each MAINTAIN so cold reads reuse
    /// the creation-mode fd instead of reopening (ADR-0054; M4-S26).
    /// Undrained handles simply close when the pipeline drops.
    sealed_handles: Vec<(u32, F::File)>,
    /// The drive (M4.5-S31, ADR-0084 D1). `Seam` by construction; the
    /// plane flips to `Reactor` before the first flush.
    drive: TierDrive,
    /// Reactor-drive window pool (empty and unused on the seam drive).
    pool: WindowPool,
    /// The one in-flight round, staged here and executed by the plane
    /// (ADR-0084 D2 — the explicit bound: one per namespace).
    round: Option<TierRound>,
    /// Directory handles a round's dir-fsync barriers target — held
    /// until the round finishes (the fd must outlive the op).
    round_dir_holds: Vec<F::File>,
    /// Files whose seal is staged in the in-flight round: catalog
    /// commit happens at the barrier completion ([`RoundEffect::
    /// SealCommit`]); until then the open handle serves cold reads on
    /// the confirmed prefix and the file stays manifest-visible as an
    /// unsealed range. Empty whenever no round is in flight.
    pending_seals: std::collections::VecDeque<PendingSeal<F>>,
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
    /// namespace; recovery passes the first free id — S12).
    pub fn new(fs: F, config: TierFlushConfig, next_id: u32) -> TierFlush<F> {
        Self::with_catalog(fs, config, next_id, Vec::new())
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
    /// ascending by id and base, or `next_id` not above every seeded id.
    #[allow(clippy::disallowed_types, reason = "container: T")]
    pub fn with_catalog(
        fs: F,
        config: TierFlushConfig,
        next_id: u32,
        sealed: Vec<TierFileMeta>,
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
            pool: WindowPool::new(),
            round: None,
            round_dir_holds: Vec::new(),
            pending_seals: std::collections::VecDeque::new(),
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
        assert!(self.round.is_none(), "drive change with a round in flight");
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
    pub(crate) fn pool_outstanding(&self) -> u32 {
        self.pool.outstanding()
    }

    /// Whether a staged round exists (in flight or awaiting `finish_round`).
    #[must_use]
    pub fn round_active(&self) -> bool {
        self.round.is_some()
    }

    /// Ops of the staged round: total, leading writes, barriers.
    #[must_use]
    pub fn round_op_count(&self) -> usize {
        self.round.as_ref().map_or(0, TierRound::op_count)
    }

    /// Leading ops of the round that are data writes (wave 1).
    #[must_use]
    pub fn round_write_count(&self) -> usize {
        self.round.as_ref().map_or(0, TierRound::write_count)
    }

    /// Barrier ops of the round (wave 2).
    #[must_use]
    pub fn round_barrier_count(&self) -> usize {
        self.round.as_ref().map_or(0, TierRound::barrier_count)
    }

    /// True when every op of the staged round names an fd this pipeline
    /// still holds — the active writer's, a pending seal's, or a round
    /// directory hold's. A round failing this would hand the driver a
    /// closed (or reused) fd (review 2026-08-30, F-L01-02).
    #[must_use]
    pub fn round_handles_owned(&self) -> bool {
        let Some(round) = &self.round else { return true };
        let owned = |fd: std::os::fd::RawFd| {
            self.writer.as_ref().and_then(TierWriter::raw_fd) == Some(fd)
                || self.pending_seals.iter().any(|s| s.file.raw_fd() == Some(fd))
                || self.round_dir_holds.iter().any(|h| h.raw_fd() == Some(fd))
        };
        (0..round.op_count()).all(|i| owned(round.op(i).fd))
    }

    /// The round op at `index` — the plane converts writes to
    /// `IoOp::LogWrite` and barriers to `IoOp::Fdatasync`. The returned
    /// window bytes stay valid (pool-owned, heap-stable) until
    /// [`finish_round`](Self::finish_round).
    ///
    /// # Panics
    /// Panics without a round or past its op count.
    #[must_use]
    pub fn round_op(&self, index: usize) -> TierOpView<'_> {
        self.round.as_ref().expect("round op view without a round").op(index)
    }

    /// Queued twin of [`append_range`](Self::append_range): identical
    /// rotation/early-seal decisions, but every device intent lands on
    /// the round and every durability fact defers to a round effect.
    ///
    /// # Errors
    /// File-creation metadata I/O only (the open — ADR-0084 D2); all
    /// staged work is infallible.
    ///
    /// # Panics
    /// Panics off the write cursor (the contiguity contract) or on the
    /// seam drive.
    pub fn append_range_queued(
        &mut self,
        addr: LogicalAddr,
        bytes: &[u8],
    ) -> Result<(), TierFlushError> {
        assert_eq!(self.drive, TierDrive::Reactor, "queued append on the seam drive");
        if let Some(w) = &self.writer {
            let cursor = w.base().to_raw() + w.data_len();
            assert_eq!(
                addr.to_raw(),
                cursor,
                "flush ranges are contiguous; gaps go through seal_for_gap_queued"
            );
        }
        let capacity = self.config.file_capacity;
        if let Some(writer) = self
            .writer
            .take_if(|w| w.data_len() > 0 && w.data_len() + bytes.len() as u64 > capacity)
        {
            self.seal_writer_queued(writer, SealReason::Capacity);
        }
        if self.writer.is_none() {
            self.create_file_queued(addr)?;
        }
        let round = self.round.get_or_insert_with(TierRound::new);
        let w = self.writer.as_mut().expect("created above");
        w.append_queued(addr, bytes, round, &mut self.pool);
        Ok(())
    }

    /// Queued twin of [`seal_for_gap`](Self::seal_for_gap): stages the
    /// gap seal (when a file is active) and the [`RoundEffect::GapCross`]
    /// fact — `flushed` crosses the hole only at the covering barrier's
    /// completion (ADR-0052 D2, completion-gated).
    pub fn seal_for_gap_queued(&mut self, gap_end: u64) {
        assert_eq!(self.drive, TierDrive::Reactor, "queued gap seal on the seam drive");
        if let Some(writer) = self.writer.take() {
            self.seal_writer_queued(writer, SealReason::RingTopGap);
        }
        let round = self.round.get_or_insert_with(TierRound::new);
        round.push_effect(RoundEffect::GapCross { to: gap_end });
    }

    /// Queued twin of [`sync`](Self::sync): stages the slice barrier and
    /// its `DurableTo` fact. No-op without an active file.
    pub fn sync_queued(&mut self) {
        assert_eq!(self.drive, TierDrive::Reactor, "queued sync on the seam drive");
        if let Some(w) = &mut self.writer {
            let round = self.round.get_or_insert_with(TierRound::new);
            w.sync_queued(round, &mut self.pool);
        }
    }

    /// Finishes the completed round: recycles its windows into the pool,
    /// releases the directory holds, and yields the deferred effects in
    /// stage order for the store to apply. Callable only once every op
    /// reached a terminal completion (the plane's custody obligation).
    #[must_use]
    pub fn finish_round(&mut self) -> Vec<RoundEffect> {
        self.round_dir_holds.clear();
        match self.round.take() {
            Some(round) => round.recycle(&mut self.pool),
            None => Vec::new(),
        }
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
    /// seal joins the catalog exactly as a seam seal would have.
    ///
    /// # Panics
    /// Panics without a pending seal — effects mirror stage order.
    pub fn commit_oldest_seal(&mut self) {
        let seal = self.pending_seals.pop_front().expect("SealCommit without a pending seal");
        self.sealed_device_bytes += seal.device_bytes;
        self.sealed_handles.push((seal.id, seal.file));
        self.sealed.push(TierFileMeta {
            id: seal.id,
            base: seal.base,
            data_len: seal.data_len,
            reason: seal.reason,
            path: seal.path,
        });
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

    fn seal_writer_queued(&mut self, writer: TierWriter<F>, reason: SealReason) {
        let round = self.round.get_or_insert_with(TierRound::new);
        let sealed: QueuedSeal<F::File> = writer.seal_queued(reason, round, &mut self.pool);
        round.push_effect(RoundEffect::SealCommit);
        self.pending_seals.push_back(PendingSeal {
            id: self.active_id,
            base: sealed.base,
            data_len: sealed.outcome.data_len,
            confirmed_len: sealed.confirmed_len,
            reason,
            path: sealed.outcome.path,
            device_bytes: sealed.outcome.device_bytes,
            file: sealed.file,
        });
    }

    /// Creates the next tier file on the reactor drive. Every step that
    /// can fail — the cold directory, both directory holds, the file —
    /// precedes the first touch of the round, so a refused creation
    /// leaves no staged op whose handle nobody owns (review 2026-08-30,
    /// F-L01-02: a hold refused after the header write was staged sent
    /// the driver a write on the dropped writer's closed fd).
    fn create_file_queued(&mut self, base: LogicalAddr) -> Result<(), TierFlushError> {
        let id = self.next_id;
        let cold = self.config.shard_dir.join("cold");
        self.fs
            .create_dir_all(&cold)
            .map_err(|source| TierFlushError::Io { path: cold.clone(), source })?;
        let mut holds = Vec::with_capacity(2);
        for dir in [self.config.shard_dir.clone(), cold.clone()] {
            if inf_foundation::fault::fire(crate::fault::TIER_DIR_OPEN_FAIL) {
                return Err(TierFlushError::Io {
                    path: dir,
                    source: crate::fault::injected(crate::fault::TIER_DIR_OPEN_FAIL),
                });
            }
            let handle = self
                .fs
                .open_dir(&dir)
                .map_err(|source| TierFlushError::Io { path: dir, source })?;
            holds.push(handle);
        }
        let (writer, header) = TierWriter::create_queued(
            &self.fs,
            &self.config.shard_dir,
            id,
            self.config.cell,
            self.config.ns,
            base,
            self.config.mode,
            self.config.file_capacity,
            &mut self.pool,
        )
        .map_err(|source| TierFlushError::Io { path: cold, source })?;
        // Nothing below can fail: the round is touched only now.
        let round = self.round.get_or_insert_with(TierRound::new);
        round.push_write(writer.queued_fd(), 0, header, 1);
        // The segment-create rule, completion-gated (ADR-0084 D2): both
        // dirent barriers join the round; the confirm waits on them, so
        // no manifest can name the file before its name is durable.
        for handle in holds {
            let fd = handle.raw_fd().expect("reactor drive requires fd-backed dirs (ADR-0084)");
            round.push_barrier(fd);
            self.round_dir_holds.push(handle);
        }
        self.next_id += 1;
        self.active_id = id;
        self.writer = Some(writer);
        Ok(())
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
        assert!(self.round.is_none(), "seam append while a reactor round is in flight");
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
        assert!(self.round.is_none(), "seam gap seal while a reactor round is in flight");
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
        assert!(self.round.is_none(), "seam sync while a reactor round is in flight");
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
        assert!(self.round.is_none(), "seam seal while a reactor round is in flight");
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
        assert!(self.round.is_none(), "seam seal while a reactor round is in flight");
        if let Some(writer) = self.writer.take() {
            self.seal_writer(writer, SealReason::Stall)?;
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
        self.next_id += 1;
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

// ---- the boot pipeline (ADR-0174 D2 rule 4, D5; DRR FCR-STTIER-01 I11, I18) ----

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

/// Why a settle read did not answer (DRR FCR-STTIER-01 §2, the
/// `REPLAY_SETTLE_READ_FAIL` row): each a typed boot refusal for the
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
    /// frame's CRC checked. Opens nothing (I18).
    ///
    /// # Errors
    /// [`SettleReadError`], each a typed boot refusal: no covering
    /// range, no held handle, the read's failure (or the injected
    /// `replay_settle_read_fail`), a short file, a CRC failure.
    pub fn read_key_window(&mut self, addr: u64) -> Result<SettleWindow<'_>, SettleReadError> {
        let (base, end, file, path) = locate_held(&self.flush, addr)?;
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
    /// no-op once the store's drain sealed it (E13); a pipeline handed
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
            // The planted canary (DRR FCR-STTIER-01 §6): the boot-sealed
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

    /// Handles held: one per sealed catalogue file (tests, I11).
    #[must_use]
    pub fn held_handles(&self) -> usize {
        self.flush.sealed_handles.len()
    }

    /// The planted canary's stall seal (DRR FCR-STTIER-01 §6,
    /// `inf_canary_replay_stall_seal`): a boot pipeline never seals a
    /// file to free a partial frame (ADR-0174 D2 rule 5); the seal-reason
    /// census is what sees one that does.
    ///
    /// # Errors
    /// As any seal.
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

/// The settle read's locate step: the catalogue file holding `addr`, by
/// bisection over the ascending catalogue (the L04 perf row: the rebuild
/// asks once per slot against thousands of files), with its held handle
/// and its claimed end — a sealed file's exact end, the active file's
/// barrier-covered end (the boot claim rule).
#[allow(clippy::type_complexity)] // one locate answer: base, end, handle, path
fn locate_held<F: SegmentFs>(
    flush: &TierFlush<F>,
    addr: u64,
) -> Result<(u64, u64, &F::File, &Path), SettleReadError> {
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
            Some((id, file)) if *id == meta.id => {
                Ok((meta.base.to_raw(), meta.base.to_raw() + meta.data_len, file, &meta.path))
            }
            _ => Err(SettleReadError::NoHandle { addr, id: meta.id }),
        };
    }
    if let Some(w) = &flush.writer {
        let base = w.base().to_raw();
        let end = base + w.durable_len();
        if addr >= base && addr < end {
            return Ok((base, end, w.file(), w.path()));
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
            },
            0,
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
    use inf_foundation::FileOffset;
    use inf_foundation::fault::FaultSpec;

    fn sim_seam_pipeline(disk: &SimDisk, capacity: u64) -> TierFlush<SimDisk> {
        TierFlush::new(
            disk.clone(),
            TierFlushConfig {
                shard_dir: Path::new("shard-0").to_path_buf(),
                cell: 0,
                ns: NsId(17),
                mode: TierIoMode::Buffered,
                file_capacity: capacity,
                slice_bytes: 4096,
            },
            0,
        )
    }

    fn sim_pipeline(disk: &SimDisk, capacity: u64) -> TierFlush<SimDisk> {
        let mut flush = TierFlush::new(
            disk.clone(),
            TierFlushConfig {
                shard_dir: Path::new("shard-0").to_path_buf(),
                cell: 0,
                ns: NsId(17),
                mode: TierIoMode::Buffered,
                file_capacity: capacity,
                slice_bytes: 4096,
            },
            0,
        );
        flush.set_drive(TierDrive::Reactor);
        flush
    }

    /// Executes a staged round the plane's way — every write, then every
    /// barrier (fdatasync covers only completed writes) — and returns
    /// the deferred effects.
    fn run_round(disk: &SimDisk, flush: &mut TierFlush<SimDisk>) -> Vec<RoundEffect> {
        let writes = flush.round_write_count();
        for index in 0..writes {
            let op = flush.round_op(index);
            assert!(!op.is_barrier, "writes lead the op list");
            let offset = FileOffset::new(op.offset).expect("staged positions are addressable");
            disk.driver_write_at(op.fd, offset, op.bytes).expect("driver write");
        }
        for index in writes..flush.round_op_count() {
            let op = flush.round_op(index);
            assert!(op.is_barrier, "barriers trail the op list");
            disk.driver_fdatasync(op.fd).expect("driver barrier");
        }
        flush.finish_round()
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

    /// A queued round performs no device I/O at stage time and advances
    /// no durability watermark until its effects apply — `durable_len`
    /// and the claim bound move only at the barrier's completion
    /// (ADR-0084 D2, the §3.1 chain).
    #[test]
    fn queued_round_defers_durability_to_completion() {
        let disk = SimDisk::new();
        let mut flush = sim_pipeline(&disk, 1 << 20);
        let payload = vec![0x5D; TIER_FRAME_DATA + 1908];
        flush.append_range_queued(LogicalAddr::ZERO, &payload).expect("stage");
        flush.sync_queued();
        // Header + one full frame + the partial tail frame, one barrier
        // on the file, two dirent barriers from the creation.
        assert_eq!(flush.round_write_count(), 3, "header + batch + tail");
        assert_eq!(flush.round_barrier_count(), 3, "file + shard dir + cold dir");
        assert_eq!(flush.confirmable_end(), Some(0), "nothing claimable before completion");
        let effects = run_round(&disk, &mut flush);
        assert_eq!(effects.len(), 1);
        let RoundEffect::DurableTo { data_len } = effects[0] else {
            panic!("sync stages DurableTo, got {:?}", effects[0]);
        };
        assert_eq!(data_len, payload.len() as u64);
        flush.confirm_durable_to(data_len);
        assert_eq!(
            flush.confirmable_end(),
            Some(TIER_FRAME_DATA as u64),
            "claim rule holds: the partial tail frame waits for the seal"
        );
    }

    /// A capacity seal staged in a round commits to the catalog only at
    /// effect application: mid-round the file is a pending seal (visible
    /// to manifest/cold lookups), afterwards it is sealed on disk with a
    /// verified footer and its handle drains to the cold-read table.
    #[test]
    fn queued_capacity_seal_commits_at_completion() {
        let disk = SimDisk::new();
        let mut flush = sim_pipeline(&disk, 1000);
        flush.append_range_queued(LogicalAddr::ZERO, &[0xA0; 600]).expect("stage");
        let a1 = LogicalAddr::ZERO.advanced(600).expect("fits");
        flush.append_range_queued(a1, &[0xA1; 600]).expect("stage");
        flush.sync_queued();
        assert_eq!(flush.sealed().len(), 0, "no catalog commit at stage time");
        assert_eq!(flush.pending_seal_count(), 1, "the capacity seal is pending");
        let pending: Vec<_> = flush.pending_seals().collect();
        assert_eq!(pending[0].id, 0);
        assert_eq!(pending[0].data_len, 600);
        let effects = run_round(&disk, &mut flush);
        assert!(
            matches!(effects[0], RoundEffect::SealCommit),
            "the seal precedes the new file's durability in stage order"
        );
        for effect in effects {
            match effect {
                RoundEffect::DurableTo { data_len } => flush.confirm_durable_to(data_len),
                RoundEffect::SealCommit => flush.commit_oldest_seal(),
                RoundEffect::GapCross { .. } => panic!("no gap staged"),
            }
        }
        assert_eq!(flush.pending_seal_count(), 0);
        assert_eq!(flush.sealed().len(), 1);
        assert_eq!(flush.sealed()[0].data_len, 600, "sealed at the range boundary");
        assert_eq!(flush.sealed()[0].reason, SealReason::Capacity);
        let handles = flush.take_sealed_handles();
        assert_eq!(handles.len(), 1, "the seal hands its fd to the cold-read table");
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
        flush.append_range_queued(LogicalAddr::ZERO, &[0x5B; 700]).expect("stage");
        flush.seal_for_gap_queued(90_000);
        flush.sync_queued(); // no active file: a no-op, stages nothing
        let effects = run_round(&disk, &mut flush);
        assert_eq!(effects.len(), 2);
        assert!(matches!(effects[0], RoundEffect::SealCommit));
        let RoundEffect::GapCross { to } = effects[1] else { panic!("gap crossing follows") };
        assert_eq!(to, 90_000);
        flush.commit_oldest_seal();
        assert_eq!(flush.sealed()[0].reason, SealReason::RingTopGap);
        assert_eq!(
            flush.confirmable_end(),
            Some(700),
            "the whole sealed file is claimable after commit"
        );
    }

    /// F-L01-02 (review of 2026-08-30): a creation whose directory hold
    /// fails after the header write was staged left that write in the
    /// round while the writer — and its fd — dropped. A failed creation
    /// stages nothing; the retry creates the file.
    #[test]
    fn a_failed_creation_stages_nothing() {
        let disk = SimDisk::new();
        let mut flush = sim_pipeline(&disk, 1 << 20);
        inf_foundation::fault::arm(crate::fault::TIER_DIR_OPEN_FAIL, FaultSpec::Nth(1));
        let err = flush
            .append_range_queued(LogicalAddr::ZERO, &[0x22; 64])
            .expect_err("the directory hold is refused");
        assert!(matches!(err, TierFlushError::Io { .. }), "{err}");
        assert!(
            !flush.round_active(),
            "a failed creation left a round: {} op(s), active fd {:?}, handles owned {}",
            flush.round_op_count(),
            flush.active_raw_fd(),
            flush.round_handles_owned()
        );
        assert!(flush.active().is_none(), "no file is active after a failed creation");
        flush.append_range_queued(LogicalAddr::ZERO, &[0x22; 64]).expect("the retry creates");
        flush.sync_queued();
        assert!(flush.round_handles_owned());
        assert_eq!(flush.round_write_count(), 2, "header + tail frame");
        assert_eq!(flush.round_barrier_count(), 3, "file + shard dir + cold dir");
        let effects = run_round(&disk, &mut flush);
        assert!(matches!(effects[..], [RoundEffect::DurableTo { data_len: 64 }]));
        assert_eq!(flush.next_file_id(), 1, "one file was created, once");
        inf_foundation::fault::disarm_all();
    }

    /// The mid-pull shape: file A seals at capacity and file B's creation
    /// fails. The round keeps exactly A's seal — every op on a handle the
    /// pipeline owns — executes clean, and B is created on the retry.
    /// Pre-fix the round carried B's header write on a closed fd.
    #[test]
    fn a_failed_rotation_keeps_only_the_seal_it_staged() {
        let disk = SimDisk::new();
        let mut flush = sim_pipeline(&disk, 1000);
        flush.append_range_queued(LogicalAddr::ZERO, &[0xA0; 600]).expect("stage");
        let a1 = LogicalAddr::ZERO.advanced(600).expect("fits");
        inf_foundation::fault::arm(crate::fault::TIER_DIR_OPEN_FAIL, FaultSpec::Nth(1));
        flush.append_range_queued(a1, &[0xA1; 600]).expect_err("B's directory hold is refused");
        assert!(
            flush.round_handles_owned(),
            "the round carries an op on a handle nobody owns ({} ops, active fd {:?})",
            flush.round_op_count(),
            flush.active_raw_fd()
        );
        assert_eq!(flush.pending_seal_count(), 1, "A's capacity seal is pending");
        assert!(flush.active().is_none(), "B was never created");
        assert_eq!(flush.next_file_id(), 1, "B's id was not consumed");
        let effects = run_round(&disk, &mut flush);
        assert!(matches!(effects[..], [RoundEffect::SealCommit]), "{effects:?}");
        flush.commit_oldest_seal();
        assert_eq!(flush.sealed()[0].data_len, 600);
        assert_eq!(flush.confirmable_end(), Some(600));
        // The retry: B is created, the second range lands behind A.
        flush.append_range_queued(a1, &[0xA1; 600]).expect("the retry creates B");
        flush.sync_queued();
        assert!(flush.round_handles_owned());
        let effects = run_round(&disk, &mut flush);
        assert!(matches!(effects[..], [RoundEffect::DurableTo { data_len: 600 }]), "{effects:?}");
        flush.confirm_durable_to(600);
        assert_eq!(flush.next_file_id(), 2);
        assert_eq!(flush.active().map(|(id, ..)| id), Some(1), "B is the active file");
        inf_foundation::fault::disarm_all();
    }

    /// Fd-less filesystems never take the reactor drive: the queued
    /// funnels are unreachable on `MemFs` by the drive contract, and the
    /// seam pipeline refuses a drive flip while work is staged.
    #[test]
    #[should_panic(expected = "drive change with a round in flight")]
    fn drive_flip_with_a_staged_round_panics() {
        let disk = SimDisk::new();
        let mut flush = sim_pipeline(&disk, 1 << 20);
        flush.append_range_queued(LogicalAddr::ZERO, &[0x11; 64]).expect("stage");
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
        let next = LogicalAddr::ZERO.advanced(TIER_FRAME_DATA as u64).expect("fits");
        flush.append_range_queued(next, &frame).expect("queued append");
        flush.sync_queued();
        let effects = run_round(&disk, &mut flush);
        for effect in effects {
            if let RoundEffect::DurableTo { data_len } = effect {
                flush.confirm_durable_to(data_len);
            }
        }
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
        BootFlush::new(TierFlush::with_catalog(fs, config(1000), files, sealed), handles)
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
        let mut boot = BootFlush::new(TierFlush::new(fs.clone(), config(1 << 20), 0), Vec::new());
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

    /// The injected read failure (`replay_settle_read_fail`) answers
    /// typed, naming the address; a CRC failure answers typed too.
    #[test]
    fn a_failed_or_corrupt_settle_read_is_typed() {
        use inf_foundation::fault::FaultSpec;
        let fs = MemFs::new();
        let mut boot = BootFlush::new(TierFlush::new(fs.clone(), config(1 << 20), 0), Vec::new());
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
