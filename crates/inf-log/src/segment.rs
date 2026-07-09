//! Segment lifecycle (M2-S02): `seg-NNNNNN.ilog` files preallocated and
//! rotated off the hot path, sealed at a size (or optional time) bound,
//! owned by exactly one cell (L1).
//!
//! The load-bearing property: the *next* segment is created and
//! preallocated in a MAINTAIN slice ([`SegmentRotor::maintain`]) well
//! before the active one seals, so rotation on the append path is a
//! pointer swap. Preallocation failure surfaces **before** any write needs
//! the space (`ENOSPC` discipline): [`SegmentRotor::space_exhausted`] tells
//! the store layer to stop admitting durable writes with a named error
//! while memory namespaces continue untouched (degrade loudly, never
//! corrupt).
//!
//! Appends use a two-step protocol that makes base/bytes mismatches
//! unrepresentable: [`SegmentRotor::begin_frame`] performs any rotation
//! and reserves the frame's base LSN (returned in a must-use
//! [`FrameSlot`]), the caller finalizes its `FrameBuilder` against
//! `slot.first_record_lsn()`, then [`SegmentRotor::commit_frame`] writes
//! the bytes at the reserved base.

use core::fmt;
use std::io;
use std::path::{Path, PathBuf};

use crate::frame::FRAME_HEADER_LEN;
use crate::fs::{SegmentFile, SegmentFs};
use crate::lsn::{Lsn, SegmentId};
use crate::scan::SegmentScan;

pub const DEFAULT_SEGMENT_BYTES: u32 = 256 << 20;

/// File name of a segment: `seg-{:06}.ilog` (wider ids grow digits
/// naturally; the parser accepts 6–10 digits).
#[must_use]
pub fn segment_file_name(id: SegmentId) -> String {
    format!("seg-{:06}.ilog", id.0)
}

/// Strict parse of a segment file name. Accepts `seg-<6..=10 digits>.ilog`
/// with a value fitting `u32`; any leading-zero padding parses (so a
/// non-canonically padded duplicate is *detected* as a duplicate id by the
/// boot scan, not skipped). Returns `None` for anything else.
#[must_use]
pub fn parse_segment_file_name(name: &str) -> Option<SegmentId> {
    let digits = name.strip_prefix("seg-")?.strip_suffix(".ilog")?;
    if !(6..=10).contains(&digits.len()) || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    u32::try_from(digits.parse::<u64>().ok()?).ok().map(SegmentId)
}

/// Lifecycle configuration. Values are per-cell config (M2 plan §4);
/// time-based sealing ships default-off (the M2 cut line — size sealing is
/// the load-bearing path).
#[derive(Copy, Clone, Debug)]
pub struct SegmentConfig {
    /// Preallocation size and seal threshold.
    pub segment_bytes: u32,
    /// Seal the active segment once this many milliseconds have passed
    /// since its first append (checked in MAINTAIN). `None` = size-only.
    pub seal_after_ms: Option<u64>,
}

impl Default for SegmentConfig {
    fn default() -> Self {
        SegmentConfig { segment_bytes: DEFAULT_SEGMENT_BYTES, seal_after_ms: None }
    }
}

/// Non-recoverable fsync failure (§8.4, the fsyncgate rule): once an
/// fsync-class operation fails, the covered bytes may or may not be
/// durable and the page cache can no longer be trusted — the cell must
/// fail-stop. **No caller may catch this and continue**; CI greps for this
/// type in non-fatal match arms (M2 §3.3, enforced from M2-S17).
#[derive(Debug)]
pub struct FsyncFailed {
    pub segment: SegmentId,
    pub source: io::Error,
}

impl fmt::Display for FsyncFailed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FATAL: fsync failed on {} — cell must stop: {}", self.segment, self.source)
    }
}

impl std::error::Error for FsyncFailed {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// Typed lifecycle failures. `NoSpace` is the documented admission error
/// for durable namespaces; `Fsync` is fatal (see [`FsyncFailed`]).
#[derive(Debug)]
pub enum LogError {
    /// Segment space could not be reserved (`ENOSPC`): durable writes must
    /// be refused until space returns; memory namespaces are unaffected.
    NoSpace {
        segment: SegmentId,
    },
    /// A frame larger than one segment can never be stored.
    FrameTooLarge {
        len: u32,
        max: u32,
    },
    Fsync(FsyncFailed),
    Io {
        segment: SegmentId,
        source: io::Error,
    },
}

impl fmt::Display for LogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LogError::NoSpace { segment } => {
                write!(f, "no space to preallocate {segment}: durable writes refused")
            }
            LogError::FrameTooLarge { len, max } => {
                write!(f, "frame of {len} bytes exceeds segment capacity {max}")
            }
            LogError::Fsync(err) => err.fmt(f),
            LogError::Io { segment, source } => write!(f, "log I/O error on {segment}: {source}"),
        }
    }
}

impl std::error::Error for LogError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            LogError::Fsync(err) => Some(err),
            LogError::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Reservation for exactly one frame append, produced by
/// [`SegmentRotor::begin_frame`]. Not `Copy`/`Clone`: one reservation, one
/// commit.
#[derive(Debug)]
#[must_use = "a reserved frame slot must be committed"]
pub struct FrameSlot {
    base: Lsn,
    len: u32,
}

impl FrameSlot {
    /// LSN of the frame's first byte (its position in the segment).
    #[must_use]
    pub fn base(&self) -> Lsn {
        self.base
    }

    /// LSN of the first *record* — what `FrameBuilder::finalize` takes.
    #[must_use]
    pub fn first_record_lsn(&self) -> Lsn {
        self.base.advance(FRAME_HEADER_LEN as u32)
    }
}

/// A segment leaving the rotor under **deferred seal** (M2-S05, ADR-0013
/// D4): rotation on the reactor tier must not fsync inline (a synchronous
/// fdatasync of up to one everysec window of dirty bytes is exactly the
/// foreground stall the S02 rotation AC forbids). The handoff carries the
/// old segment's write handle so the plane can issue the seal fdatasync
/// through `BackendDriver`; the commit ledger holds the file and drops it
/// when the sync completes — the seal is real, and the last write handle
/// gone, at the same moment. The rotor itself retains no path to the
/// segment either way.
#[derive(Debug)]
#[must_use = "a deferred seal must be fsynced through the driver and its handle held until Synced"]
pub struct SealHandoff<File> {
    segment: SegmentId,
    file: File,
    end_offset: u32,
}

impl<File: SegmentFile> SealHandoff<File> {
    #[must_use]
    pub fn segment(&self) -> SegmentId {
        self.segment
    }

    /// Exclusive end of the sealed segment's valid bytes — what the seal
    /// fsync covers (the watermark advances to `(segment, end_offset)`).
    #[must_use]
    pub fn end_offset(&self) -> u32 {
        self.end_offset
    }

    /// The fd the seal fdatasync targets (`None` only on in-memory tiers,
    /// which never reach the reactor path).
    #[must_use]
    pub fn raw_fd(&self) -> Option<std::os::fd::RawFd> {
        self.file.raw_fd()
    }
}

/// What [`SegmentRotor::begin_frame_deferred`] yields: the reserved frame
/// slot plus, when rotation happened, the deferred seal to fsync through
/// the driver (ADR-0013 D4).
pub type DeferredBegin<File> = (FrameSlot, Option<SealHandoff<File>>);

/// Lifecycle counters (cell-local, no atomics — L1). `inline_preallocs`
/// counts the slow path where rotation found no ready next segment; under
/// a healthy MAINTAIN cadence it stays zero.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct RotorStats {
    pub rotations: u64,
    pub preallocs: u64,
    pub inline_preallocs: u64,
    pub prealloc_failures: u64,
    pub time_seals: u64,
}

/// What one MAINTAIN slice did (observability + tests).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct MaintainReport {
    pub preallocated: Option<SegmentId>,
    pub prealloc_failed: bool,
    pub time_sealed: bool,
}

struct ActiveSegment<File> {
    id: SegmentId,
    file: File,
    written: u32,
    first_append_at_ms: Option<u64>,
}

/// Owner of one cell's active + preallocated-next segments. Sealed
/// segments retain **no write handle at all** — immutability is enforced
/// by construction (stronger than the plan's debug-only read-only reopen;
/// recorded in the M2 ledger).
pub struct SegmentRotor<F: SegmentFs> {
    fs: F,
    log_dir: PathBuf,
    cfg: SegmentConfig,
    active: ActiveSegment<F::File>,
    next: Option<(SegmentId, F::File)>,
    sealed: Vec<SegmentId>,
    space_exhausted: bool,
    stats: RotorStats,
}

impl<F: SegmentFs> fmt::Debug for SegmentRotor<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SegmentRotor")
            .field("log_dir", &self.log_dir)
            .field("active", &self.active.id)
            .field("written", &self.active.written)
            .field("next", &self.next.as_ref().map(|(id, _)| *id))
            .field("sealed", &self.sealed)
            .field("stats", &self.stats)
            .finish()
    }
}

impl<F: SegmentFs> SegmentRotor<F> {
    /// First boot of a cell: create `seg-000000.ilog` preallocated in an
    /// existing (already dir-fsynced — see `create_cell_dirs`) log dir.
    pub fn create_fresh(fs: F, log_dir: PathBuf, cfg: SegmentConfig) -> Result<Self, LogError> {
        let id = SegmentId(0);
        let file = create_prealloc(&fs, &log_dir, id, &cfg)?;
        Ok(SegmentRotor {
            fs,
            log_dir,
            cfg,
            active: ActiveSegment { id, file, written: 0, first_append_at_ms: None },
            next: None,
            sealed: Vec::new(),
            space_exhausted: false,
            stats: RotorStats::default(),
        })
    }

    /// [`create_fresh`](Self::create_fresh) with **no blocking syncs**
    /// (M2.5-S01): neither the segment file nor its directory entry is
    /// fsynced here — the caller registers boot barriers (driver-ridden
    /// fdatasyncs on the log-dir handle and the active segment fd) at the
    /// head of the group-commit ledger, fencing every durable ack behind
    /// them. `PREALLOC_NO_SPACE` still fires: ENOSPC admission is not a
    /// durability side effect.
    pub fn create_fresh_deferred(
        fs: F,
        log_dir: PathBuf,
        cfg: SegmentConfig,
    ) -> Result<Self, LogError> {
        let id = SegmentId(0);
        let file = create_prealloc_deferred(&fs, &log_dir, id, &cfg)?;
        Ok(SegmentRotor {
            fs,
            log_dir,
            cfg,
            active: ActiveSegment { id, file, written: 0, first_append_at_ms: None },
            next: None,
            sealed: Vec::new(),
            space_exhausted: false,
            stats: RotorStats::default(),
        })
    }

    /// The exclusive end of everything appended so far — the boot-barrier
    /// coverage floor (M2.5-S01): a barrier registered at this LSN orders
    /// before every future frame and sync in the commit ledger.
    #[must_use]
    pub fn append_cursor(&self) -> Lsn {
        Lsn::new(self.active.id, self.active.written)
    }

    /// Reopen after a boot scan: the highest-numbered segment is the tail;
    /// recovery (M2-S13/S14) computes `tail_offset` — the byte after the
    /// last valid frame — and hands it here.
    pub fn open_existing(
        fs: F,
        log_dir: PathBuf,
        cfg: SegmentConfig,
        scan: &SegmentScan,
        tail_offset: u32,
    ) -> Result<Self, LogError> {
        let Some(tail) = scan.tail() else {
            return Self::create_fresh(fs, log_dir, cfg);
        };
        let path = log_dir.join(segment_file_name(tail));
        let file = fs.open_write(&path).map_err(|source| LogError::Io { segment: tail, source })?;
        let sealed =
            scan.segments().split_last().map(|(_, rest)| rest.to_vec()).unwrap_or_default();
        Ok(SegmentRotor {
            fs,
            log_dir,
            cfg,
            active: ActiveSegment {
                id: tail,
                file,
                written: tail_offset,
                first_append_at_ms: None,
            },
            next: None,
            sealed,
            space_exhausted: false,
            stats: RotorStats::default(),
        })
    }

    /// MAINTAIN slice: preallocate the next segment if missing and perform
    /// a time-bound seal when due. Runs off the hot path (§5.1 step 5).
    pub fn maintain(&mut self, now_ms: u64) -> Result<MaintainReport, LogError> {
        let (report, _barrier) = self.maintain_inner(now_ms, false)?;
        Ok(report)
    }

    /// [`maintain`](Self::maintain) for the reactor tier (M2.5-S01): the
    /// next-segment prealloc runs **unsynced** — a blocking fsync here is
    /// the same reactor-stall class as the boot wedge — and the returned
    /// barrier (a log-dir handle) rides the driver as a ledger-fronted
    /// fdatasync. The prealloc file itself needs no separate sync: its
    /// first frame's linked fdatasync covers the data and the size needed
    /// to retrieve it, and an empty prealloc lost to a crash is a legal
    /// empty tail either way.
    pub fn maintain_deferred(&mut self, now_ms: u64) -> Result<DeferredMaintain<F>, LogError> {
        self.maintain_inner(now_ms, true)
    }

    fn maintain_inner(
        &mut self,
        now_ms: u64,
        deferred: bool,
    ) -> Result<DeferredMaintain<F>, LogError> {
        let mut report = MaintainReport::default();
        let mut barrier = None;
        if self.time_seal_due(now_ms) {
            self.rotate()?;
            self.stats.time_seals += 1;
            report.time_sealed = true;
        }
        if self.next.is_none() {
            let id = self.active.id.next();
            let created = if deferred {
                create_prealloc_deferred(&self.fs, &self.log_dir, id, &self.cfg)
            } else {
                create_prealloc(&self.fs, &self.log_dir, id, &self.cfg)
            };
            match created {
                Ok(file) => {
                    if deferred {
                        let dir = self
                            .fs
                            .open_dir(&self.log_dir)
                            .map_err(|source| LogError::Io { segment: id, source })?;
                        barrier = Some(PreallocBarrier { segment: id, dir });
                    }
                    self.next = Some((id, file));
                    self.stats.preallocs += 1;
                    self.space_exhausted = false;
                    report.preallocated = Some(id);
                }
                Err(LogError::NoSpace { .. }) => {
                    self.stats.prealloc_failures += 1;
                    self.space_exhausted = true;
                    report.prealloc_failed = true;
                }
                Err(other) => return Err(other),
            }
        }
        Ok((report, barrier))
    }

    /// True once preallocation has failed for lack of space and no next
    /// segment is ready: the store layer must refuse durable writes now,
    /// *before* the active segment fills (M2-S02 ENOSPC discipline).
    #[must_use]
    pub fn space_exhausted(&self) -> bool {
        self.space_exhausted
    }

    /// Reserve space for one frame of `frame_len` bytes, rotating first if
    /// it does not fit the active segment. Hot path: the fit check is one
    /// compare; rotation itself is a pointer swap onto the preallocated
    /// next segment.
    pub fn begin_frame(&mut self, frame_len: u32, now_ms: u64) -> Result<FrameSlot, LogError> {
        if frame_len > self.cfg.segment_bytes {
            return Err(LogError::FrameTooLarge { len: frame_len, max: self.cfg.segment_bytes });
        }
        if self.active.written.saturating_add(frame_len) > self.cfg.segment_bytes {
            self.rotate()?;
        }
        if self.active.first_append_at_ms.is_none() {
            self.active.first_append_at_ms = Some(now_ms);
        }
        Ok(FrameSlot { base: Lsn::new(self.active.id, self.active.written), len: frame_len })
    }

    /// `begin_frame` for the reactor tier (M2-S05): rotation, when due, is
    /// **deferred** — the pointer swap happens now, but the seal fdatasync
    /// rides `BackendDriver` via the returned [`SealHandoff`] instead of
    /// blocking the append path (ADR-0013 D4). Only the size seal rotates
    /// here; the reactor tier ships with `seal_after_ms = None` (the M2
    /// cut line — time seals remain a synchronous-tier feature until their
    /// config lands).
    pub fn begin_frame_deferred(
        &mut self,
        frame_len: u32,
        now_ms: u64,
    ) -> Result<DeferredBegin<F::File>, LogError> {
        if frame_len > self.cfg.segment_bytes {
            return Err(LogError::FrameTooLarge { len: frame_len, max: self.cfg.segment_bytes });
        }
        let handoff = if self.active.written.saturating_add(frame_len) > self.cfg.segment_bytes {
            Some(self.rotate_deferred()?)
        } else {
            None
        };
        if self.active.first_append_at_ms.is_none() {
            self.active.first_append_at_ms = Some(now_ms);
        }
        Ok((
            FrameSlot { base: Lsn::new(self.active.id, self.active.written), len: frame_len },
            handoff,
        ))
    }

    /// Advance the append cursor for a frame whose bytes ride the driver
    /// (`IoOp::LogWrite` at `slot.base()`, same bytes, same offset). The
    /// synchronous sibling is [`commit_frame`](Self::commit_frame).
    ///
    /// # Panics
    /// If the slot does not match the active segment tail — same LOG-step
    /// invariants as `commit_frame`.
    pub fn commit_frame_queued(&mut self, slot: FrameSlot) -> Lsn {
        assert_eq!(
            slot.base,
            Lsn::new(self.active.id, self.active.written),
            "frame slot is stale (out-of-order commit)"
        );
        self.active.written += slot.len;
        slot.base
    }

    /// Deferred rotation: swap in the next segment WITHOUT the seal fsync —
    /// the caller owns sealing through the driver. Sound only because the
    /// LOG step rotates exclusively while no write is in flight (the
    /// staging lease serializes writes; `can_seal` implies the previous
    /// write's CQE arrived), so the handed-off segment is complete.
    fn rotate_deferred(&mut self) -> Result<SealHandoff<F::File>, LogError> {
        let (next_id, next_file) = match self.next.take() {
            Some(ready) => ready,
            None => {
                let id = self.active.id.next();
                let file = create_prealloc(&self.fs, &self.log_dir, id, &self.cfg)?;
                self.stats.inline_preallocs += 1;
                (id, file)
            }
        };
        let old = core::mem::replace(
            &mut self.active,
            ActiveSegment { id: next_id, file: next_file, written: 0, first_append_at_ms: None },
        );
        self.sealed.push(old.id);
        self.stats.rotations += 1;
        Ok(SealHandoff { segment: old.id, file: old.file, end_offset: old.written })
    }

    /// The active segment's platform fd for driver-tier writes (`None` on
    /// in-memory tiers, which never reach the reactor path).
    #[must_use]
    pub fn active_raw_fd(&self) -> Option<std::os::fd::RawFd> {
        self.active.file.raw_fd()
    }

    /// Write a finalized frame at its reserved base. Returns the frame's
    /// base LSN.
    ///
    /// # Panics
    /// If `frame.len() != slot.len` or the slot does not match the active
    /// segment tail — internal invariants of the LOG step (a slot is used
    /// once, immediately, on the cell thread).
    pub fn commit_frame(&mut self, slot: FrameSlot, frame: &[u8]) -> Result<Lsn, LogError> {
        assert_eq!(frame.len() as u32, slot.len, "frame bytes differ from reservation");
        assert_eq!(
            slot.base,
            Lsn::new(self.active.id, self.active.written),
            "frame slot is stale (out-of-order commit)"
        );
        // M2-S16 `log_append_short_write`: the device accepts a prefix and
        // the append FAILS — the caller must treat the frame as never
        // written (the reactor tier's short-write resubmit contract is the
        // scripted driver's leg; this is the sync tier's).
        if inf_foundation::fault::fire(crate::fault::LOG_APPEND_SHORT_WRITE) {
            let cut = frame.len() / 2;
            let _ = self.active.file.write_at(u64::from(slot.base.offset), &frame[..cut]);
            return Err(LogError::Io {
                segment: self.active.id,
                source: crate::fault::injected(crate::fault::LOG_APPEND_SHORT_WRITE),
            });
        }
        // M2-S16 `torn_frame`: a prefix lands and the append *succeeds* —
        // lying-disk/power-cut physics. Only meaningful as the final write
        // before a crash (anything appended after it lands beyond a gap a
        // validating frame would turn into fail-stop corruption — exactly
        // the M2-S14 taxonomy).
        if inf_foundation::fault::fire(crate::fault::TORN_FRAME) {
            let cut = frame.len() * 2 / 3;
            self.active
                .file
                .write_at(u64::from(slot.base.offset), &frame[..cut.max(1)])
                .map_err(|source| LogError::Io { segment: self.active.id, source })?;
            self.active.written += slot.len;
            return Ok(slot.base);
        }
        self.active
            .file
            .write_at(u64::from(slot.base.offset), frame)
            .map_err(|source| LogError::Io { segment: self.active.id, source })?;
        self.active.written += slot.len;
        Ok(slot.base)
    }

    /// Seal the active segment and swap in the next. The preallocated-next
    /// path is a pointer swap; the inline fallback is counted
    /// (`inline_preallocs`) and is where `NoSpace` surfaces if MAINTAIN's
    /// early warning was ignored.
    fn rotate(&mut self) -> Result<(), LogError> {
        // M2-S16 `fsync_err`: the seal fsync fails — typed, non-recoverable
        // by contract (§8.4: no caller may catch and continue).
        if inf_foundation::fault::fire(crate::fault::FSYNC_ERR) {
            return Err(LogError::Fsync(FsyncFailed {
                segment: self.active.id,
                source: crate::fault::injected(crate::fault::FSYNC_ERR),
            }));
        }
        // Seal: durably flush, then drop the write handle — a sealed
        // segment is immutable by construction.
        self.active
            .file
            .sync_data()
            .map_err(|source| LogError::Fsync(FsyncFailed { segment: self.active.id, source }))?;
        // M2-S16 `power_cut_after_seal`: the seal is durable; the process
        // dies before anything after it exists (the pointer swap, the next
        // segment's first frame). The typed error stands in for death —
        // tests drop the rotor here and recover the surviving image.
        if inf_foundation::fault::fire(crate::fault::POWER_CUT_AFTER_SEAL) {
            return Err(LogError::Io {
                segment: self.active.id,
                source: crate::fault::injected(crate::fault::POWER_CUT_AFTER_SEAL),
            });
        }
        let (next_id, next_file) = match self.next.take() {
            Some(ready) => ready,
            None => {
                let id = self.active.id.next();
                let file = create_prealloc(&self.fs, &self.log_dir, id, &self.cfg)?;
                self.stats.inline_preallocs += 1;
                (id, file)
            }
        };
        self.sealed.push(self.active.id);
        self.active =
            ActiveSegment { id: next_id, file: next_file, written: 0, first_append_at_ms: None };
        self.stats.rotations += 1;
        Ok(())
    }

    fn time_seal_due(&self, now_ms: u64) -> bool {
        let (Some(bound), Some(first)) = (self.cfg.seal_after_ms, self.active.first_append_at_ms)
        else {
            return false;
        };
        self.active.written > 0 && now_ms.saturating_sub(first) >= bound
    }

    #[must_use]
    pub fn active_segment(&self) -> SegmentId {
        self.active.id
    }

    #[must_use]
    pub fn active_written(&self) -> u32 {
        self.active.written
    }

    #[must_use]
    pub fn next_ready(&self) -> Option<SegmentId> {
        self.next.as_ref().map(|(id, _)| *id)
    }

    #[must_use]
    pub fn sealed(&self) -> &[SegmentId] {
        &self.sealed
    }

    /// Sealed segments strictly below the truncation `floor` (M2-S11) —
    /// fully covered by a durable manifest, deletable. Ascending (the
    /// sealed list is append-ordered).
    #[must_use]
    pub fn sealed_below(&self, floor: SegmentId) -> &[SegmentId] {
        let end = self.sealed.partition_point(|&id| id < floor);
        &self.sealed[..end]
    }

    /// Truncation (M2-S11): forget one sealed segment and return its file
    /// path — the **caller** owns the unlink. On the reactor tier the
    /// unlink is delegated to the control thread (ADR-0017): freeing a
    /// segment's pages is O(size) in the kernel, a measured multi-ms loop
    /// stall when done in MAINTAIN. No dir-fsync follows the unlink: a
    /// power cut may resurrect the file, but it stays below the durable
    /// manifest's floor and is re-collected as stale at the next boot.
    ///
    /// # Panics
    /// If `id` is not in the sealed list — truncating the active or next
    /// segment is an internal invariant violation.
    pub fn forget_sealed(&mut self, id: SegmentId) -> PathBuf {
        let pos = self
            .sealed
            .iter()
            .position(|&s| s == id)
            .expect("forget_sealed targets a sealed segment");
        self.sealed.remove(pos);
        self.log_dir.join(segment_file_name(id))
    }

    #[must_use]
    pub fn stats(&self) -> RotorStats {
        self.stats
    }

    /// Read access to a segment's bytes (test/dev tier; the real read path
    /// is M2-S04). Sealed segments open read-only.
    pub fn open_segment_read(&self, id: SegmentId) -> Result<F::File, LogError> {
        self.fs
            .open_read(&self.log_dir.join(segment_file_name(id)))
            .map_err(|source| LogError::Io { segment: id, source })
    }
}

/// What one deferred MAINTAIN slice produced (M2.5-S01): the report plus
/// the prealloc's metadata barrier when a segment was preallocated.
pub type DeferredMaintain<F> = (MaintainReport, Option<PreallocBarrier<<F as SegmentFs>::File>>);

/// One deferred prealloc's metadata barrier (M2.5-S01): the log-dir handle
/// whose driver-ridden fdatasync makes the new segment's directory entry
/// durable. Registered coverage-neutral in the commit ledger — a dir sync
/// promises no log-data coverage.
pub struct PreallocBarrier<File> {
    /// The preallocated segment (log line / debugging).
    pub segment: SegmentId,
    /// Log-dir handle: the ledger holds it until its `Synced` arrives.
    pub dir: File,
}

/// Unsynced prealloc for the reactor tier (M2.5-S01): no file sync, no
/// dir sync — see [`SegmentRotor::maintain_deferred`].
fn create_prealloc_deferred<F: SegmentFs>(
    fs: &F,
    log_dir: &Path,
    id: SegmentId,
    cfg: &SegmentConfig,
) -> Result<F::File, LogError> {
    let path = log_dir.join(segment_file_name(id));
    if inf_foundation::fault::fire(crate::fault::PREALLOC_NO_SPACE) {
        return Err(LogError::NoSpace { segment: id });
    }
    fs.create_segment_unsynced(&path, u64::from(cfg.segment_bytes)).map_err(|source| {
        if source.kind() == io::ErrorKind::StorageFull || source.raw_os_error() == Some(28) {
            LogError::NoSpace { segment: id }
        } else {
            LogError::Io { segment: id, source }
        }
    })
}

fn create_prealloc<F: SegmentFs>(
    fs: &F,
    log_dir: &Path,
    id: SegmentId,
    cfg: &SegmentConfig,
) -> Result<F::File, LogError> {
    let path = log_dir.join(segment_file_name(id));
    // M2-S16 `prealloc_no_space`: the S02 ENOSPC discipline — surfaced
    // before any write needs the space, typed refusal downstream, memory
    // namespaces unaffected (the re-bound S02 observation row).
    if inf_foundation::fault::fire(crate::fault::PREALLOC_NO_SPACE) {
        return Err(LogError::NoSpace { segment: id });
    }
    let file = fs.create_segment(&path, u64::from(cfg.segment_bytes)).map_err(|source| {
        if source.kind() == io::ErrorKind::StorageFull || source.raw_os_error() == Some(28) {
            LogError::NoSpace { segment: id }
        } else {
            LogError::Io { segment: id, source }
        }
    })?;
    // The segment must exist durably before anything refers to it: sync
    // the directory entry now (M2-S16 `dir_fsync_fail`).
    if inf_foundation::fault::fire(crate::fault::DIR_FSYNC_FAIL) {
        return Err(LogError::Fsync(FsyncFailed {
            segment: id,
            source: crate::fault::injected(crate::fault::DIR_FSYNC_FAIL),
        }));
    }
    fs.sync_dir(log_dir).map_err(|source| LogError::Fsync(FsyncFailed { segment: id, source }))?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segment_names_are_canonical() {
        assert_eq!(segment_file_name(SegmentId(17)), "seg-000017.ilog");
        assert_eq!(segment_file_name(SegmentId(1_000_000)), "seg-1000000.ilog");
        assert_eq!(parse_segment_file_name("seg-000017.ilog"), Some(SegmentId(17)));
        assert_eq!(parse_segment_file_name("seg-1000000.ilog"), Some(SegmentId(1_000_000)));
        // Non-canonical padding still *parses* (duplicate detection), but
        // out-of-range, short, or foreign names do not.
        assert_eq!(parse_segment_file_name("seg-0000017.ilog"), Some(SegmentId(17)));
        assert_eq!(parse_segment_file_name("seg-4294967295.ilog"), Some(SegmentId(u32::MAX)));
        assert_eq!(parse_segment_file_name("seg-4294967296.ilog"), None);
        assert_eq!(parse_segment_file_name("seg-00017.ilog"), None);
        assert_eq!(parse_segment_file_name("seg-000017.ilog.tmp"), None);
        assert_eq!(parse_segment_file_name("MANIFEST"), None);
        assert_eq!(parse_segment_file_name("seg-00001x.ilog"), None);
    }
}
