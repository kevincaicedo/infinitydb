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
//! existing vocabulary edge; the drive loop is
//! `TieredTable::flush_slice`). Blocking seam writes, the `SyncIckWriter`
//! pattern — driven from MAINTAIN slices in tests/DST/benches; the
//! reactor-tier drive rides `IoOp::LogWrite`/`Fdatasync` on the same
//! staged intents when command wiring lands (recorded deviation,
//! ADR-0056 D3).
//!
//! fsync failure is fatal-by-default (§8.4, ADR-0056 D4): it surfaces as
//! [`TierFlushError::Fsync`] and the flushed watermark freezes — no
//! caller may catch and continue past it.

use std::io;
use std::path::PathBuf;

use inf_foundation::LogicalAddr;

use crate::fs::{SegmentFs, TierIoMode};
use crate::record::NsId;
use crate::tier::{SealReason, TierWriteFailure, TierWriter};

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
        }
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
        sealed + self.writer.as_ref().map_or(0, |w| file_bytes(w.data_len(), false))
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
        let sealed = writer.seal(reason).map_err(|failure| classify(failure, path_hint))?;
        self.sealed_device_bytes += sealed.device_bytes;
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
}
