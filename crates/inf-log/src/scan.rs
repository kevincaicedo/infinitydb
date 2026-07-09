//! Boot-time log-directory scan (M2-S02): validate that the per-cell log
//! directory holds a contiguous, duplicate-free segment sequence, with a
//! **named error for every anomaly** — a gap, a duplicate id, or a foreign
//! file is never silently skipped (§8.4 honesty; corrupt topology fails the
//! boot, it does not shrink the log).
//!
//! Also owns first-boot directory creation: `shard-k/log` and
//! `shard-k/ckpt` created with dir-fsync so the cell's storage roots are
//! durable before any segment exists (§4 storage layout).

use core::fmt;
use std::io;
use std::path::{Path, PathBuf};

use crate::fs::SegmentFs;
use crate::lsn::SegmentId;
use crate::segment::parse_segment_file_name;

/// The per-cell storage roots.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CellDirs {
    pub log: PathBuf,
    pub ckpt: PathBuf,
}

/// Create (idempotently) and durably persist the per-cell directories
/// under `shard_dir` (e.g. `<data>/shard-3`).
pub fn create_cell_dirs<F: SegmentFs>(fs: &F, shard_dir: &Path) -> io::Result<CellDirs> {
    let dirs = CellDirs { log: shard_dir.join("log"), ckpt: shard_dir.join("ckpt") };
    fs.create_dir_all(&dirs.log)?;
    fs.create_dir_all(&dirs.ckpt)?;
    // dir-fsync children, the shard dir, and its parent: the new entries
    // must survive power loss before anything is written under them
    // (M2-S16 `dir_fsync_fail` covers this barrier class too).
    if inf_foundation::fault::fire(crate::fault::DIR_FSYNC_FAIL) {
        return Err(crate::fault::injected(crate::fault::DIR_FSYNC_FAIL));
    }
    fs.sync_dir(&dirs.log)?;
    fs.sync_dir(&dirs.ckpt)?;
    fs.sync_dir(shard_dir)?;
    if let Some(parent) = shard_dir.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs.sync_dir(parent)?;
    }
    Ok(dirs)
}

/// Deferred-barrier variant of [`create_cell_dirs`] (M2.5-S01): create the
/// per-cell directories with **no blocking dir-fsyncs** and return open
/// directory handles instead. The caller registers each handle's
/// driver-ridden fdatasync as a boot barrier at the head of the
/// group-commit ledger, so every durable ack is fenced behind the entries
/// becoming durable while boot-ready never waits on the device. A blocking
/// fsync here can stall the reactor for minutes behind foreign journal
/// writeback — the captured cell-2 boot wedge (ADR-0022 D7).
///
/// Handle order: log, ckpt, shard, parent (when it exists).
pub fn create_cell_dirs_deferred<F: SegmentFs>(
    fs: &F,
    shard_dir: &Path,
) -> io::Result<(CellDirs, Vec<F::File>)> {
    let dirs = CellDirs { log: shard_dir.join("log"), ckpt: shard_dir.join("ckpt") };
    fs.create_dir_all(&dirs.log)?;
    fs.create_dir_all(&dirs.ckpt)?;
    let mut handles = Vec::with_capacity(4);
    handles.push(fs.open_dir(&dirs.log)?);
    handles.push(fs.open_dir(&dirs.ckpt)?);
    handles.push(fs.open_dir(shard_dir)?);
    if let Some(parent) = shard_dir.parent().filter(|p| !p.as_os_str().is_empty()) {
        handles.push(fs.open_dir(parent)?);
    }
    Ok((dirs, handles))
}

/// Scan outcome: segment ids in ascending, contiguous order. May start at
/// any id (truncation — M2-S11 — deletes covered prefixes).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SegmentScan {
    segments: Vec<SegmentId>,
}

impl SegmentScan {
    #[must_use]
    pub fn segments(&self) -> &[SegmentId] {
        &self.segments
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.segments.is_empty()
    }

    /// The highest-numbered segment — the active tail candidate.
    #[must_use]
    pub fn tail(&self) -> Option<SegmentId> {
        self.segments.last().copied()
    }
}

/// Named boot-scan failures (M2-S02 AC: documented errors, never a silent
/// skip).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScanError {
    /// Directory could not be read.
    Io { path: PathBuf, kind: io::ErrorKind },
    /// A file in the log directory that is not a well-formed segment name
    /// (foreign file, truncated name, out-of-range id).
    BadName { name: String },
    /// Two files resolve to the same segment id (e.g. non-canonical
    /// zero-padding).
    Duplicate { id: SegmentId, name: String },
    /// The sequence is non-contiguous: `expected` is missing.
    Gap { expected: SegmentId, found: SegmentId },
}

impl fmt::Display for ScanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScanError::Io { path, kind } => {
                write!(f, "log dir scan failed on {}: {kind:?}", path.display())
            }
            ScanError::BadName { name } => {
                write!(f, "foreign or malformed file in log dir: {name:?}")
            }
            ScanError::Duplicate { id, name } => {
                write!(f, "duplicate segment id {id} (second file {name:?})")
            }
            ScanError::Gap { expected, found } => {
                write!(f, "segment sequence gap: expected {expected}, found {found}")
            }
        }
    }
}

impl std::error::Error for ScanError {}

/// Validate the log directory and return the ordered segment set.
pub fn scan_log_dir<F: SegmentFs>(fs: &F, log_dir: &Path) -> Result<SegmentScan, ScanError> {
    let outcome = scan_log_dir_from(fs, log_dir, SegmentId(0))?;
    debug_assert!(outcome.stale.is_empty(), "floor 0 admits no stale segments");
    Ok(outcome.scan)
}

/// A floor-aware scan (M2-S11): live segments (≥ floor) plus the stale
/// prefix a crash mid-truncation may have left behind.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScanOutcome {
    /// The validated live set: contiguous, ascending, all ids ≥ floor.
    pub scan: SegmentScan,
    /// Segments below the floor, ascending. Fully covered by the
    /// manifest-named checkpoint — recovery deletes them (gaps among them
    /// are fine: un-fsynced unlinks may survive a power cut in any subset).
    pub stale: Vec<SegmentId>,
}

/// Validate the log directory against a truncation `floor` (the manifest's
/// `begin_lsn.segment` — M2-S11). Contiguity is enforced only at and above
/// the floor; ids below it are returned as `stale` for deletion. Foreign
/// names and duplicates are errors everywhere — honesty does not stop at
/// the floor.
pub fn scan_log_dir_from<F: SegmentFs>(
    fs: &F,
    log_dir: &Path,
    floor: SegmentId,
) -> Result<ScanOutcome, ScanError> {
    let names = fs
        .list_dir(log_dir)
        .map_err(|err| ScanError::Io { path: log_dir.to_path_buf(), kind: err.kind() })?;

    let mut entries: Vec<(SegmentId, String)> = Vec::with_capacity(names.len());
    for name in names {
        match parse_segment_file_name(&name) {
            Some(id) => entries.push((id, name)),
            None => return Err(ScanError::BadName { name }),
        }
    }
    entries.sort();

    for pair in entries.windows(2) {
        let (prev, _) = &pair[0];
        let (curr, curr_name) = &pair[1];
        if curr == prev {
            return Err(ScanError::Duplicate { id: *curr, name: curr_name.clone() });
        }
        if *curr >= floor && *prev >= floor && curr.0 != prev.0 + 1 {
            return Err(ScanError::Gap { expected: prev.next(), found: *curr });
        }
    }

    let split = entries.partition_point(|(id, _)| *id < floor);
    let stale = entries[..split].iter().map(|(id, _)| *id).collect();
    Ok(ScanOutcome {
        scan: SegmentScan { segments: entries[split..].iter().map(|(id, _)| *id).collect() },
        stale,
    })
}
