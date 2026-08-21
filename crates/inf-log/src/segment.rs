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
//! `slot.first_record_lsn()` under `slot.layout()`, then
//! [`SegmentRotor::commit_frame`] writes the bytes at the reserved base.
//!
//! **I/O mode and barrier class** (M4.5-S34, ADR-0086): every segment
//! carries the [`SegmentIoMode`] it was created with. `Direct` segments
//! take 4 KiB-aligned v3 frames and, once **pre-zeroed**, write-through
//! (FUA-class) barriers. Pre-zeroing is the rotor's job and runs through
//! the driver (never a blocking write on the cell): the plane pulls zero
//! slices from [`SegmentRotor::next_zero_slice`], reports each
//! completion, issues the zero-fill barrier the rotor asks for, and the
//! next segment becomes *ready*. Pre-zeroed is **read, never
//! remembered** — `SegmentFile::fully_allocated` at every create/open —
//! so a sparse tail or a lost barrier degrades loudly to FLUSH-class
//! barriers instead of silently entering the unwritten-extent trap.

use core::fmt;
use std::io;
use std::path::{Path, PathBuf};

use crate::frame::{FRAME_ALIGN, FRAME_HEADER_LEN, FrameLayout};
use crate::fs::{SegmentFile, SegmentFs, SegmentIoMode};
use crate::lsn::{Lsn, SegmentId};
use crate::scan::SegmentScan;

pub const DEFAULT_SEGMENT_BYTES: u32 = 256 << 20;

/// Default largest padded frame written write-through on a `Direct`
/// segment (ADR-0086 D1/D7): the reference device's probed crossover sits
/// between 256 KiB (FUA still 4× cheaper than FLUSH) and 1 MiB (FUA
/// loses); `io-properties.toml` overrides it per device.
pub const DEFAULT_FUA_MAX_FRAME_BYTES: u32 = 256 << 10;

/// Bytes per driver-ridden zero-fill write (ADR-0086 D4): large enough
/// to run near the device's sequential rate, small enough that a
/// write-through frame queued behind one slice waits ~0.25 ms and a
/// not-ready rotation waits at most one slice completion. The first
/// dev-tier A/B ran 1 MiB slices unpaced and paid for it in `always` p99
/// (8 → 19–23 ms at 4 cells): the zero-fill burst is background I/O on
/// the same device as the barrier.
pub const ZERO_FILL_SLICE_BYTES: u32 = 256 << 10;

/// Zero-fill head start (ADR-0086 D4 pacing): while the active segment
/// is pre-zeroed, the next segment's fill cursor may run ahead of
/// `2 × active.written + ZERO_FILL_HEAD_START` and no further — the fill
/// finishes at half the active segment's life with 2× headroom over the
/// log's own rate, spread across it instead of a burst. Derived from
/// bytes, not tuned from a clock (L7-neutral). An active segment that is
/// *not* pre-zeroed (a fresh cell, a reopened sparse tail, a not-ready
/// rotation) fills unpaced: the FLUSH class it is running costs more
/// than the burst.
pub const ZERO_FILL_HEAD_START: u32 = 16 << 20;

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
    /// I/O mode of every segment created from here on (ADR-0086 D1).
    /// `Buffered` is the M2 path byte-for-byte and the default until the
    /// reference-box A/B; `Direct` requires `segment_bytes` to be a
    /// multiple of [`FRAME_ALIGN`].
    pub io_mode: SegmentIoMode,
    /// Largest padded frame written write-through on a pre-zeroed
    /// `Direct` segment; larger frames keep the linked fdatasync (the
    /// device-probed crossover, ADR-0086 D7).
    pub fua_max_frame_bytes: u32,
}

impl Default for SegmentConfig {
    fn default() -> Self {
        SegmentConfig {
            segment_bytes: DEFAULT_SEGMENT_BYTES,
            seal_after_ms: None,
            io_mode: SegmentIoMode::Buffered,
            fua_max_frame_bytes: DEFAULT_FUA_MAX_FRAME_BYTES,
        }
    }
}

impl SegmentConfig {
    /// Boot-configuration invariant (ADR-0086 D3): aligned frames need an
    /// aligned segment end, or the last frame's padding would run past
    /// the preallocation.
    ///
    /// # Panics
    /// If `Direct` is configured with a `segment_bytes` that is not a
    /// multiple of [`FRAME_ALIGN`].
    pub fn assert_valid(&self) {
        if self.io_mode == SegmentIoMode::Direct {
            assert!(
                self.segment_bytes.is_multiple_of(FRAME_ALIGN),
                "Direct segments need segment_bytes to be a multiple of {FRAME_ALIGN}"
            );
        }
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
    /// Rotation is due but the next segment has a zero-fill slice or
    /// barrier in flight (ADR-0086 D4): the frame waits one completion.
    /// Retryable — the LOG step returns and re-tries next iteration.
    NextNotReady {
        segment: SegmentId,
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
            LogError::NextNotReady { segment } => {
                write!(f, "next segment {segment} has a zero-fill op in flight: frame waits")
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
/// commit. `len` is the **on-device** length (padding included under
/// [`FrameLayout::Aligned`]); `barrier` is the class this frame may use.
#[derive(Debug)]
#[must_use = "a reserved frame slot must be committed"]
pub struct FrameSlot {
    base: Lsn,
    len: u32,
    layout: FrameLayout,
    write_through_ok: bool,
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

    /// On-device frame length (padding included) — the append-cursor
    /// advance and the exclusive end the frame's barrier covers.
    #[must_use]
    #[allow(clippy::len_without_is_empty)] // a reservation is never empty
    pub fn len(&self) -> u32 {
        self.len
    }

    /// The layout the frame must be sealed under (the active segment's).
    #[must_use]
    pub fn layout(&self) -> FrameLayout {
        self.layout
    }

    /// True when this frame may be written write-through (ADR-0086 D1):
    /// the active segment is `Direct` **and** pre-zeroed, and the padded
    /// frame is inside `fua_max_frame_bytes`. Otherwise a due sync rides
    /// the linked fdatasync.
    #[must_use]
    pub fn write_through_ok(&self) -> bool {
        self.write_through_ok
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
    /// Zero bytes written to pre-zero `Direct` segments (ADR-0086 D4) —
    /// the second write of every direct log byte, disclosed.
    pub zero_fill_bytes: u64,
    /// Rotations onto a `Direct` segment whose zero-fill had not finished
    /// (the active filled first): the segment runs FLUSH-class barriers.
    pub rotations_unzeroed: u64,
    /// Class-upgrade rotations (a pre-zeroed segment was ready while the
    /// active one was not) — how a fresh or recovered cell converges.
    pub rotations_upgrade: u64,
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
    io_mode: SegmentIoMode,
    /// Every byte backed by written storage (ADR-0086 D4) — read at
    /// create/open, never assumed. Decides write-through eligibility.
    prezeroed: bool,
}

impl<File> ActiveSegment<File> {
    fn layout(&self) -> FrameLayout {
        match self.io_mode {
            SegmentIoMode::Buffered => FrameLayout::Packed,
            SegmentIoMode::Direct => FrameLayout::Aligned,
        }
    }
}

/// Where a preallocated next segment stands on its way to ready
/// (ADR-0086 D4). `Buffered` segments are born `Ready { prezeroed: false }`.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum NextState {
    /// Zero slices are being written through the driver; `in_flight` is
    /// the length of the slice awaiting its `LogWritten` (0 = none).
    Filling {
        cursor: u32,
        in_flight: u32,
    },
    /// Every zero byte landed; the barrier is owed (the plane registers
    /// and issues it).
    AwaitBarrier,
    /// The barrier is in flight.
    BarrierInFlight,
    Ready {
        prezeroed: bool,
    },
}

struct NextSegment<File> {
    id: SegmentId,
    file: File,
    io_mode: SegmentIoMode,
    state: NextState,
}

/// One zero-fill write for the plane to issue (ADR-0086 D4): `fd` of the
/// next segment, absolute `offset`, `len` bytes of zeros from the cell's
/// aligned zero window.
#[derive(Copy, Clone, Debug)]
pub struct ZeroSlice {
    pub fd: std::os::fd::RawFd,
    pub offset: u64,
    pub len: u32,
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
    next: Option<NextSegment<F::File>>,
    sealed: Vec<SegmentId>,
    space_exhausted: bool,
    /// The log life frames appended through this rotor belong to
    /// (ADR-0031 D5): 1 on fresh logs; recovery derives max-observed + 1
    /// and stamps it via [`set_resume_epoch`](Self::set_resume_epoch).
    /// Carried here (the log writer's segment state) so cell assembly can
    /// wire `StagingRing::set_frame_epoch` without recovery signature
    /// churn.
    resume_epoch: u32,
    stats: RotorStats,
}

impl<F: SegmentFs> fmt::Debug for SegmentRotor<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SegmentRotor")
            .field("log_dir", &self.log_dir)
            .field("active", &self.active.id)
            .field("written", &self.active.written)
            .field("io_mode", &self.active.io_mode)
            .field("prezeroed", &self.active.prezeroed)
            .field("next", &self.next.as_ref().map(|next| (next.id, next.state)))
            .field("sealed", &self.sealed)
            .field("stats", &self.stats)
            .finish()
    }
}

impl<F: SegmentFs> SegmentRotor<F> {
    /// First boot of a cell: create `seg-000000.ilog` preallocated in an
    /// existing (already dir-fsynced — see `create_cell_dirs`) log dir.
    pub fn create_fresh(fs: F, log_dir: PathBuf, cfg: SegmentConfig) -> Result<Self, LogError> {
        cfg.assert_valid();
        let id = SegmentId(0);
        let file = create_prealloc(&fs, &log_dir, id, &cfg)?;
        let active = activate(id, file, 0, cfg.io_mode)?;
        Ok(SegmentRotor {
            fs,
            log_dir,
            cfg,
            active,
            next: None,
            sealed: Vec::new(),
            space_exhausted: false,
            resume_epoch: 1,
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
    /// Under `Direct` (ADR-0086 D4) segment 0 is created sparse — zero
    /// boot cost, no boot-path blocking — and runs FLUSH-class barriers
    /// until the class-upgrade rotation onto the first pre-zeroed next
    /// segment MAINTAIN produces.
    pub fn create_fresh_deferred(
        fs: F,
        log_dir: PathBuf,
        cfg: SegmentConfig,
    ) -> Result<Self, LogError> {
        cfg.assert_valid();
        let id = SegmentId(0);
        let file = create_prealloc_deferred(&fs, &log_dir, id, &cfg)?;
        let active = activate(id, file, 0, cfg.io_mode)?;
        Ok(SegmentRotor {
            fs,
            log_dir,
            cfg,
            active,
            next: None,
            sealed: Vec::new(),
            space_exhausted: false,
            resume_epoch: 1,
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

    /// Recovery hands the derived log life here (max epoch observed across
    /// the valid prefix and every validating beyond-frame, + 1 — ADR-0031
    /// D5) so cell assembly can wire `StagingRing::set_frame_epoch`.
    pub fn set_resume_epoch(&mut self, epoch: u32) {
        assert!(epoch > 0, "frame epoch 0 is reserved (ADR-0031 D1)");
        self.resume_epoch = epoch;
    }

    /// The log life frames appended through this rotor must stamp
    /// (1 unless recovery derived a later one).
    #[must_use]
    pub fn resume_epoch(&self) -> u32 {
        self.resume_epoch
    }

    /// Reopen after a boot scan: the highest-numbered segment is the tail;
    /// recovery (M2-S13/S14) computes `tail_offset` — the byte after the
    /// last valid frame (the aligned successor when that frame was v3) —
    /// and hands it here. The tail reopens in the configured mode
    /// (ADR-0086 D4): `Direct` needs an aligned `tail_offset` (a v2 tail
    /// resumed under `Direct` rounds up — the skipped bytes are zeros or
    /// un-covered residue, both legal slack) and is pre-zeroed only if the
    /// file says so.
    pub fn open_existing(
        fs: F,
        log_dir: PathBuf,
        cfg: SegmentConfig,
        scan: &SegmentScan,
        tail_offset: u32,
    ) -> Result<Self, LogError> {
        cfg.assert_valid();
        let Some(tail) = scan.tail() else {
            return Self::create_fresh(fs, log_dir, cfg);
        };
        let path = log_dir.join(segment_file_name(tail));
        let file = fs
            .open_segment_append(&path, cfg.io_mode)
            .map_err(|source| LogError::Io { segment: tail, source })?;
        let written = match cfg.io_mode {
            SegmentIoMode::Buffered => tail_offset,
            SegmentIoMode::Direct => crate::frame::align_up_frame(tail_offset),
        };
        let active = activate(tail, file, written, cfg.io_mode)?;
        let sealed =
            scan.segments().split_last().map(|(_, rest)| rest.to_vec()).unwrap_or_default();
        Ok(SegmentRotor {
            fs,
            log_dir,
            cfg,
            active,
            next: None,
            sealed,
            space_exhausted: false,
            resume_epoch: 1,
            stats: RotorStats::default(),
        })
    }

    /// The active segment's I/O mode.
    #[must_use]
    pub fn active_io_mode(&self) -> SegmentIoMode {
        self.active.io_mode
    }

    /// True while the active segment writes write-through frames for
    /// due syncs (`Direct` and pre-zeroed — ADR-0086 D4): the
    /// `barrier_class` observable.
    #[must_use]
    pub fn active_write_through(&self) -> bool {
        self.active.io_mode == SegmentIoMode::Direct && self.active.prezeroed
    }

    /// True when `next_zero_slice` would hand out a slice now — a pure
    /// peek (M4.5-S36, ADR-0088 D5): the device budget is consulted
    /// *before* a slice is marked in flight, because a slice taken and
    /// never issued is a phantom the rotation would wait on forever (the
    /// `m2-device-budget` sweep found exactly that shape: an acked
    /// `everysec` record behind a rotation that never came).
    #[must_use]
    pub fn zero_fill_pending(&self) -> bool {
        let Some(next) = self.next.as_ref() else { return false };
        let NextState::Filling { cursor, in_flight: 0 } = next.state else { return false };
        if self.active.prezeroed {
            let allowed =
                self.active.written.saturating_mul(2).saturating_add(ZERO_FILL_HEAD_START);
            if cursor >= allowed {
                return false;
            }
        }
        next.file.raw_fd().is_some()
    }

    /// The next zero-fill write to issue, when the next segment is
    /// filling and no slice is in flight (ADR-0086 D4). Marks the slice
    /// in flight; the plane reports it back through
    /// [`note_zero_slice_written`](Self::note_zero_slice_written).
    /// `max_len` is the zero window's size.
    pub fn next_zero_slice(&mut self, max_len: u32) -> Option<ZeroSlice> {
        debug_assert!(max_len.is_multiple_of(FRAME_ALIGN), "zero window is aligned");
        let paced = self.active.prezeroed;
        let active_written = self.active.written;
        let next = self.next.as_mut()?;
        let NextState::Filling { cursor, in_flight: 0 } = next.state else { return None };
        if paced {
            let allowed = active_written.saturating_mul(2).saturating_add(ZERO_FILL_HEAD_START);
            if cursor >= allowed {
                return None;
            }
        }
        let fd = next.file.raw_fd()?;
        let len = max_len.min(self.cfg.segment_bytes - cursor);
        debug_assert!(len > 0, "a filling segment has bytes left");
        next.state = NextState::Filling { cursor, in_flight: len };
        Some(ZeroSlice { fd, offset: u64::from(cursor), len })
    }

    /// The in-flight zero slice's `LogWritten` arrived.
    ///
    /// # Panics
    /// If no slice was in flight — a completion-routing bug.
    pub fn note_zero_slice_written(&mut self) {
        let next = self.next.as_mut().expect("zero slice written with no next segment");
        let NextState::Filling { cursor, in_flight } = next.state else {
            panic!("zero slice written while not filling")
        };
        assert!(in_flight > 0, "zero slice written with none in flight");
        let cursor = cursor + in_flight;
        self.stats.zero_fill_bytes += u64::from(in_flight);
        next.state = if cursor == self.cfg.segment_bytes {
            NextState::AwaitBarrier
        } else {
            NextState::Filling { cursor, in_flight: 0 }
        };
    }

    /// The next segment's fd when its zero-fill barrier is owed (every
    /// zero byte landed, barrier not yet issued). Marks it in flight; the
    /// plane registers the ledger entry and issues the fdatasync.
    #[must_use]
    pub fn take_zero_fill_barrier(&mut self) -> Option<std::os::fd::RawFd> {
        let next = self.next.as_mut()?;
        if next.state != NextState::AwaitBarrier {
            return None;
        }
        let fd = next.file.raw_fd()?;
        next.state = NextState::BarrierInFlight;
        Some(fd)
    }

    /// The zero-fill barrier's `Synced` arrived: the next segment is
    /// pre-zeroed and ready.
    ///
    /// # Panics
    /// If no barrier was in flight.
    pub fn note_zero_fill_synced(&mut self) {
        let next = self.next.as_mut().expect("zero-fill synced with no next segment");
        assert_eq!(next.state, NextState::BarrierInFlight, "zero-fill synced out of order");
        next.state = NextState::Ready { prezeroed: true };
    }

    /// A class-upgrade rotation is due (ADR-0086 D4): the active segment
    /// cannot write write-through frames and a pre-zeroed next segment is
    /// ready. Checked at `begin_frame_deferred` — rotation happens only
    /// while no frame is in flight (ADR-0087 D4).
    fn upgrade_due(&self) -> bool {
        self.cfg.io_mode == SegmentIoMode::Direct
            && !self.active.prezeroed
            && self
                .next
                .as_ref()
                .is_some_and(|next| next.state == NextState::Ready { prezeroed: true })
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
                    let io_mode = self.cfg.io_mode;
                    let state = next_state(&file, id, io_mode)?;
                    self.next = Some(NextSegment { id, file, io_mode, state });
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

    /// Reserve space for one frame of `frame_len` (unpadded) bytes,
    /// rotating first if its padded length does not fit the active
    /// segment. Hot path: the fit check is one compare; rotation itself is
    /// a pointer swap onto the preallocated next segment.
    pub fn begin_frame(&mut self, frame_len: u32, now_ms: u64) -> Result<FrameSlot, LogError> {
        if self.padded_bound(frame_len) > self.cfg.segment_bytes {
            return Err(LogError::FrameTooLarge { len: frame_len, max: self.cfg.segment_bytes });
        }
        if !self.fits(frame_len) {
            self.rotate()?;
        }
        Ok(self.reserve(frame_len, now_ms))
    }

    /// `begin_frame` for the reactor tier (M2-S05): rotation, when due, is
    /// **deferred** — the pointer swap happens now, but the seal fdatasync
    /// rides `BackendDriver` via the returned [`SealHandoff`] instead of
    /// blocking the append path (ADR-0013 D4). Only the size seal rotates
    /// here; the reactor tier ships with `seal_after_ms = None` (the M2
    /// cut line — time seals remain a synchronous-tier feature until their
    /// config lands). A class-upgrade rotation (ADR-0086 D4) also lands
    /// here — callers establish no frame is in flight first
    /// (`rotation_due` + drain, ADR-0087 D4).
    pub fn begin_frame_deferred(
        &mut self,
        frame_len: u32,
        now_ms: u64,
    ) -> Result<DeferredBegin<F::File>, LogError> {
        if self.padded_bound(frame_len) > self.cfg.segment_bytes {
            return Err(LogError::FrameTooLarge { len: frame_len, max: self.cfg.segment_bytes });
        }
        let handoff = if !self.fits(frame_len) || self.upgrade_due() {
            Some(self.rotate_deferred()?)
        } else {
            None
        };
        Ok((self.reserve(frame_len, now_ms), handoff))
    }

    /// Would `begin_frame_deferred` rotate for a frame of `frame_len`
    /// (unpadded) bytes — it does not fit the active segment, or a
    /// class-upgrade rotation is due (ADR-0086 D4)? The LOG step asks
    /// before reserving: rotation is a pipeline drain point (ADR-0087
    /// D4), so a frame that needs it waits until no frame is in flight.
    /// Pure: performs nothing.
    #[must_use]
    pub fn rotation_due(&self, frame_len: u32) -> bool {
        !self.fits(frame_len) || self.upgrade_due()
    }

    /// Would a frame of `frame_len` (unpadded) bytes be write-through
    /// eligible on the segment it will land in (ADR-0086 D1: `Direct` ∧
    /// pre-zeroed ∧ padded length ≤ `fua_max_frame_bytes`)? Asked before
    /// the seal so the barrier plan (ADR-0087 D3) is decided before any
    /// state moves. When a rotation is due the answer is for the *next*
    /// segment, which the reservation will also report in
    /// `FrameSlot::write_through_ok` — the two agree by construction
    /// (both read the same segment state; asserted at the plane).
    #[must_use]
    pub fn next_frame_write_through_ok(&self, frame_len: u32) -> bool {
        if self.rotation_due(frame_len) {
            let Some(next) = self.next.as_ref() else { return false };
            let prezeroed = next.state == NextState::Ready { prezeroed: true };
            let padded = FrameLayout::Aligned.padded_len(frame_len);
            return next.io_mode == SegmentIoMode::Direct
                && prezeroed
                && padded <= self.cfg.fua_max_frame_bytes;
        }
        let padded = self.active.layout().padded_len(frame_len);
        self.active_write_through() && padded <= self.cfg.fua_max_frame_bytes
    }

    /// The largest on-device length a frame of `frame_len` bytes can take
    /// in this rotor — the active layout's, or the aligned one when the
    /// configured mode could rotate it onto a `Direct` segment. Bounds the
    /// `FrameTooLarge` refusal so a rotation never discovers a frame that
    /// fit the old segment but not the new.
    fn padded_bound(&self, frame_len: u32) -> u32 {
        let here = self.active.layout().padded_len(frame_len);
        match self.cfg.io_mode {
            SegmentIoMode::Direct => here.max(FrameLayout::Aligned.padded_len(frame_len)),
            SegmentIoMode::Buffered => here,
        }
    }

    /// Does a frame of `frame_len` (unpadded) bytes fit the active
    /// segment under its layout?
    fn fits(&self, frame_len: u32) -> bool {
        let padded = self.active.layout().padded_len(frame_len);
        self.active.written.saturating_add(padded) <= self.cfg.segment_bytes
    }

    /// The reservation itself: padded length under the active layout,
    /// write-through eligibility from the active segment's state.
    fn reserve(&mut self, frame_len: u32, now_ms: u64) -> FrameSlot {
        if self.active.first_append_at_ms.is_none() {
            self.active.first_append_at_ms = Some(now_ms);
        }
        let layout = self.active.layout();
        let len = layout.padded_len(frame_len);
        debug_assert!(self.active.written.saturating_add(len) <= self.cfg.segment_bytes);
        if layout == FrameLayout::Aligned {
            debug_assert!(self.active.written.is_multiple_of(FRAME_ALIGN), "aligned cursor");
        }
        let write_through_ok = self.active_write_through() && len <= self.cfg.fua_max_frame_bytes;
        FrameSlot {
            base: Lsn::new(self.active.id, self.active.written),
            len,
            layout,
            write_through_ok,
        }
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
    /// LOG step rotates exclusively while no write is in flight (rotation
    /// is a pipeline drain point, ADR-0087 D4: the plane checks
    /// `rotation_due` and waits for `StagingRing::drained`), so the
    /// handed-off segment is complete.
    ///
    /// A next segment still zero-filling is taken anyway once no zero
    /// slice is in flight (a slice could otherwise land over the first
    /// frame) — it then runs FLUSH-class barriers (`rotations_unzeroed`,
    /// ADR-0086 D4); while a slice is in flight the frame waits
    /// (`NextNotReady`).
    fn rotate_deferred(&mut self) -> Result<SealHandoff<F::File>, LogError> {
        let next = match self.next.take() {
            Some(next) => next,
            None => self.inline_prealloc()?,
        };
        let next = match next.state {
            NextState::Filling { in_flight, .. } if in_flight > 0 => {
                let id = next.id;
                self.next = Some(next);
                return Err(LogError::NextNotReady { segment: id });
            }
            NextState::BarrierInFlight => {
                // The fd has a sync in flight; taking it is harmless (the
                // barrier covers zeros only) but the ledger entry would
                // then name the *active* fd — keep the state machine
                // honest and wait one completion instead.
                let id = next.id;
                self.next = Some(next);
                return Err(LogError::NextNotReady { segment: id });
            }
            NextState::Filling { .. } | NextState::AwaitBarrier => {
                self.stats.rotations_unzeroed += 1;
                next
            }
            NextState::Ready { .. } => next,
        };
        if self.upgrade_due_for(&next) {
            self.stats.rotations_upgrade += 1;
        }
        let prezeroed = matches!(next.state, NextState::Ready { prezeroed: true });
        let old = core::mem::replace(
            &mut self.active,
            ActiveSegment {
                id: next.id,
                file: next.file,
                written: 0,
                first_append_at_ms: None,
                io_mode: next.io_mode,
                prezeroed,
            },
        );
        self.sealed.push(old.id);
        self.stats.rotations += 1;
        Ok(SealHandoff { segment: old.id, file: old.file, end_offset: old.written })
    }

    /// The slow path: rotation found no preallocated next segment (a
    /// MAINTAIN cadence miss, counted). Under `Direct` the fresh file is
    /// sparse, so it starts `Filling` — and is taken un-zeroed right away
    /// by the caller (FLUSH class until the next upgrade).
    fn inline_prealloc(&mut self) -> Result<NextSegment<F::File>, LogError> {
        let id = self.active.id.next();
        let file = create_prealloc(&self.fs, &self.log_dir, id, &self.cfg)?;
        self.stats.inline_preallocs += 1;
        let io_mode = self.cfg.io_mode;
        let state = next_state(&file, id, io_mode)?;
        Ok(NextSegment { id, file, io_mode, state })
    }

    /// Was this rotation a class upgrade (active not pre-zeroed, next is)?
    fn upgrade_due_for(&self, next: &NextSegment<F::File>) -> bool {
        !self.active.prezeroed && next.state == NextState::Ready { prezeroed: true }
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
        let next = match self.next.take() {
            Some(next) => next,
            None => self.inline_prealloc()?,
        };
        // The synchronous tier never drives a zero-fill (no driver): a
        // `Direct` next segment is ready iff its tier is born allocated.
        let prezeroed = matches!(next.state, NextState::Ready { prezeroed: true });
        self.sealed.push(self.active.id);
        self.active = ActiveSegment {
            id: next.id,
            file: next.file,
            written: 0,
            first_append_at_ms: None,
            io_mode: next.io_mode,
            prezeroed,
        };
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

    /// The preallocated next segment, if any (ready or still filling).
    #[must_use]
    pub fn next_ready(&self) -> Option<SegmentId> {
        self.next.as_ref().map(|next| next.id)
    }

    /// True while the next segment is still being pre-zeroed (filling or
    /// awaiting its barrier) — the zero-fill observable.
    #[must_use]
    pub fn next_zero_filling(&self) -> bool {
        self.next.as_ref().is_some_and(|next| !matches!(next.state, NextState::Ready { .. }))
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
    let created = match cfg.io_mode {
        SegmentIoMode::Buffered => fs.create_segment_unsynced(&path, u64::from(cfg.segment_bytes)),
        SegmentIoMode::Direct => fs.create_segment_direct(&path, u64::from(cfg.segment_bytes)),
    };
    created.map_err(|source| {
        if source.kind() == io::ErrorKind::StorageFull || source.raw_os_error() == Some(28) {
            LogError::NoSpace { segment: id }
        } else {
            LogError::Io { segment: id, source }
        }
    })
}

/// Where a freshly created next segment starts (ADR-0086 D4): `Buffered`
/// is ready (and never write-through); `Direct` is ready only if the tier
/// says every byte is allocated — read, never assumed (an in-memory tier
/// is born allocated; a real sparse file needs the driver fill) — and
/// fd-less tiers cannot be filled at all.
fn next_state<File: SegmentFile>(
    file: &File,
    id: SegmentId,
    io_mode: SegmentIoMode,
) -> Result<NextState, LogError> {
    match io_mode {
        SegmentIoMode::Buffered => Ok(NextState::Ready { prezeroed: false }),
        SegmentIoMode::Direct => {
            let allocated =
                file.fully_allocated().map_err(|source| LogError::Io { segment: id, source })?;
            if allocated || file.raw_fd().is_none() {
                Ok(NextState::Ready { prezeroed: allocated })
            } else {
                Ok(NextState::Filling { cursor: 0, in_flight: 0 })
            }
        }
    }
}

/// Build the active-segment record, reading the pre-zeroed fact from the
/// file (ADR-0086 D4 — never assumed).
fn activate<File: SegmentFile>(
    id: SegmentId,
    file: File,
    written: u32,
    io_mode: SegmentIoMode,
) -> Result<ActiveSegment<File>, LogError> {
    let prezeroed = match io_mode {
        SegmentIoMode::Buffered => false,
        SegmentIoMode::Direct => {
            file.fully_allocated().map_err(|source| LogError::Io { segment: id, source })?
        }
    };
    Ok(ActiveSegment { id, file, written, first_append_at_ms: None, io_mode, prezeroed })
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
    let created = match cfg.io_mode {
        SegmentIoMode::Buffered => fs.create_segment(&path, u64::from(cfg.segment_bytes)),
        SegmentIoMode::Direct => fs.create_segment_direct(&path, u64::from(cfg.segment_bytes)),
    };
    let file = created.map_err(|source| {
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
