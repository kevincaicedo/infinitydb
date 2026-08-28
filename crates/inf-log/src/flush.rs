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
use std::path::PathBuf;

use inf_foundation::LogicalAddr;

use crate::fs::{SegmentFile, SegmentFs, TierIoMode};
use crate::record::NsId;
use crate::tier::{
    QueuedSeal, RoundEffect, SealReason, TierOpView, TierRound, TierWriteFailure, TierWriter,
    WindowPool,
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

/// A flush-pipeline failure. `Fsync` is fatal-by-default (§8.4): the
/// caller must freeze the flushed watermark and stop — constructing this
/// variant is audited by `check-fsync-fail-stop.sh` (ADR-0056 D4).
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
}

impl core::fmt::Display for TierFlushError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            TierFlushError::Fsync { path, source } => write!(
                f,
                "FATAL: tier fsync failed on {} — cell must stop: {source}",
                path.display()
            ),
            TierFlushError::Io { path, source } => {
                write!(f, "tier flush I/O failed on {}: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for TierFlushError {}

impl TierFlushError {
    /// True for the §8.4 fatal class — callers use this to route to the
    /// terminal fail-stop handler without naming the variant.
    #[must_use]
    pub fn is_fatal(&self) -> bool {
        matches!(self, TierFlushError::Fsync { .. })
    }

    /// True when the failed operation was a write-time space refusal
    /// (M4-S21, ADR-0063 D4) — the store's device-full latch keys on
    /// this. Deliberately `Io`-only: fsync-time exhaustion stays in the
    /// fatal class (state unknowable — the fsyncgate rule).
    #[must_use]
    pub fn is_storage_full(&self) -> bool {
        match self {
            TierFlushError::Io { source, .. } => {
                source.kind() == io::ErrorKind::StorageFull || source.raw_os_error() == Some(28)
            }
            TierFlushError::Fsync { .. } => false,
        }
    }
}

/// The per-namespace flush pipeline (one per (cell, tiered namespace) —
/// L1: cell-local, single owner).
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
    pending_seals: Vec<PendingSeal<F>>,
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
            pending_seals: Vec::new(),
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
    /// Panics with a round in flight or pending seals — the drive never
    /// changes mid-flight.
    pub fn set_drive(&mut self, drive: TierDrive) {
        assert!(self.round.is_none(), "drive change with a round in flight");
        assert!(self.pending_seals.is_empty(), "drive change with pending seals");
        self.drive = drive;
    }

    /// The pipeline's drive.
    #[must_use]
    pub fn drive(&self) -> TierDrive {
        self.drive
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
            if w.data_len() > 0 && w.data_len() + bytes.len() as u64 > self.config.file_capacity {
                self.seal_active_queued(SealReason::Capacity);
            }
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
        if self.writer.is_some() {
            self.seal_active_queued(SealReason::RingTopGap);
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
        assert!(!self.pending_seals.is_empty(), "SealCommit without a pending seal");
        let seal = self.pending_seals.remove(0);
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

    fn seal_active_queued(&mut self, reason: SealReason) {
        let writer = self.writer.take().expect("caller checked an active file exists");
        let round = self.round.get_or_insert_with(TierRound::new);
        let sealed: QueuedSeal<F::File> = writer.seal_queued(reason, round, &mut self.pool);
        round.push_effect(RoundEffect::SealCommit);
        self.pending_seals.push(PendingSeal {
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

    fn create_file_queued(&mut self, base: LogicalAddr) -> Result<(), TierFlushError> {
        let id = self.next_id;
        let round = self.round.get_or_insert_with(TierRound::new);
        let writer = TierWriter::create_queued(
            &self.fs,
            &self.config.shard_dir,
            id,
            self.config.cell,
            self.config.ns,
            base,
            self.config.mode,
            self.config.file_capacity,
            round,
            &mut self.pool,
        )
        .map_err(|source| TierFlushError::Io {
            path: self.config.shard_dir.join("cold"),
            source,
        })?;
        // The segment-create rule, completion-gated (ADR-0084 D2): both
        // dirent barriers join the round; the confirm waits on them, so
        // no manifest can name the file before its name is durable.
        for dir in [self.config.shard_dir.clone(), self.config.shard_dir.join("cold")] {
            let handle = self
                .fs
                .open_dir(&dir)
                .map_err(|source| TierFlushError::Io { path: dir, source })?;
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

    /// Reads `len` bytes at `addr` straight from the tier bytes through
    /// this catalog — sealed files, or the active file's durable prefix
    /// — with a **blocking** read on a fresh buffered handle, CRC-verified
    /// frame by frame (M4.5-S37, ADR-0093 A4: the recovery boot's settle
    /// reads, before the cell serves; the DST harnesses' oracle reads —
    /// the caller sizes the record from its header window first). Never
    /// a serving-path primitive: the plane reads cold records through
    /// `ColdReads`. `Ok(None)` when no catalogued range covers the whole
    /// span (a retired file, or a hole).
    ///
    /// # Errors
    /// The filesystem's; a frame that fails its CRC (`InvalidData`); a
    /// file shorter than its catalogued range (`UnexpectedEof`).
    pub fn read_span_blocking(&self, addr: u64, len: usize) -> io::Result<Option<Vec<u8>>> {
        use crate::tier::{TIER_FRAME_BYTES, tier_extract, tier_frame_offset, tier_frame_span};
        let covers =
            |base: u64, data_len: u64| addr >= base && addr + len as u64 <= base + data_len;
        let located = self
            .sealed
            .iter()
            .find(|m| covers(m.base.to_raw(), m.data_len))
            .map(|m| (m.base.to_raw(), m.path.clone()))
            .or_else(|| {
                let (_, base, _, durable_len, path) = self.active()?;
                covers(base.to_raw(), durable_len).then(|| (base.to_raw(), path.to_path_buf()))
            });
        let Some((base, path)) = located else { return Ok(None) };
        let file = self.fs.open_read(&path)?;
        let (first, count, skip) = tier_frame_span(addr - base, len);
        let from = tier_frame_offset(first);
        let span = count as usize * TIER_FRAME_BYTES;
        let mut window = vec![0u8; span];
        let mut done = 0usize;
        while done < span {
            let n = file.read_at(from + done as u64, &mut window[done..])?;
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!("tier file {} ends inside the span at {addr}", path.display()),
                ));
            }
            done += n;
        }
        let mut out = Vec::with_capacity(len);
        tier_extract(&window, skip, len, &mut out).map_err(|e| {
            io::Error::new(io::ErrorKind::InvalidData, format!("tier frame at {addr}: {e:?}"))
        })?;
        Ok(Some(out))
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
            if w.data_len() > 0 && w.data_len() + bytes.len() as u64 > self.config.file_capacity {
                self.seal_active(SealReason::Capacity)?;
            }
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
        if self.writer.is_some() {
            self.seal_active(SealReason::RingTopGap)?;
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
        if self.writer.is_some() {
            self.seal_active(SealReason::Shutdown)?;
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
        if self.writer.is_some() {
            self.seal_active(SealReason::Stall)?;
        }
        Ok(())
    }

    /// The highest address the drive loop may confirm right now: the
    /// active file's claimable end (full, final frames only — the
    /// partial tail frame is claimable at seal, ADR-0056 D5), or the
    /// last sealed file's exact end when no file is active. `None`
    /// before anything was written.
    #[must_use]
    pub fn confirmable_end(&self) -> Option<u64> {
        if let Some(w) = &self.writer {
            return Some(w.base().to_raw() + w.confirmable_len());
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

    fn seal_active(&mut self, reason: SealReason) -> Result<(), TierFlushError> {
        let writer = self.writer.take().expect("caller checked an active file exists");
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
        TierWriteFailure::Fsync(source) => TierFlushError::Fsync { path, source },
    }
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

    // ---- reactor drive (M4.5-S31, ADR-0084) ----

    use crate::fs::sim::SimDisk;

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
            disk.driver_write_at(op.fd, op.offset, op.bytes).expect("driver write");
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
}
