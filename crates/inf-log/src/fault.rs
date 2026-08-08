//! Named fault points this crate declares (M2-S16, §8.4). The registry —
//! arming, triggers, thread-locality, the compiled-out cost contract —
//! lives in `inf_foundation::fault`; this module owns the *names* and the
//! inventory the CI coverage check (`scripts/check-fault-points.sh`)
//! enforces: every name below must be exercised by at least one test, or
//! the build fails (coverage cannot rot).
//!
//! Each point fires at the code site that owns the mechanism and injects
//! its documented failure:
//!
//! | point | site | documented failure path |
//! |---|---|---|
//! | `log_append_short_write` | `SegmentRotor::commit_frame` | write cut short, typed `LogError::Io` — the frame never fully lands and the caller must treat the append as failed |
//! | `torn_frame` | `SegmentRotor::commit_frame` | prefix lands, call *succeeds* (lying-disk/power-cut physics) — meaningful only as the final write before a crash; recovery truncates the torn tail (M2-S14) |
//! | `fsync_err` | `SegmentRotor::rotate` (seal fsync) | typed `LogError::Fsync` — non-recoverable by contract (§8.4 fsyncgate rule) |
//! | `power_cut_after_seal` | `SegmentRotor::rotate`, after the seal fsync succeeded | typed error standing in for process death: the sealed segment is durable, nothing after it exists |
//! | `manifest_rename_fail` | `meta::write_envelope` step 5 | swap aborts; the committed envelope (old recovery unit) remains authoritative |
//! | `dir_fsync_fail` | `meta::write_envelope` step 6, `segment::create_prealloc`, `scan::create_cell_dirs` | typed error at the barrier that makes a name durable |
//! | `prealloc_no_space` | `segment::create_prealloc` | `LogError::NoSpace` — the S02 ENOSPC discipline: early surfacing, typed refusal, memory namespaces unaffected |
//! | `tier_short_write` | `TierWriter::write_tail_frame` | frame write cut short, typed I/O error — the append fails whole (M4-S11, ADR-0056 D6) |
//! | `tier_torn_frame` | `TierWriter::write_tail_frame` | prefix lands, call *succeeds* — final-write-before-crash physics; recovery truncates or CRC-refuses per ADR-0056 D5 |
//! | `tier_fsync_err` | `TierWriter::sync` / `TierWriter::seal` | typed [`TierWriteFailure::Fsync`](crate::TierWriteFailure) — fatal-by-default, the flushed watermark freezes (§8.4 applies to tier files) |
//! | `tier_footer_torn` | `TierWriter::seal` | crash between data durability and footer durability: the file recovers as *unsealed* at the manifested watermark, the seal is redone by rule |
//! | `tier_unlink_fail` | `flush::unlink_tier_file` | typed I/O error, **non-fatal and counted** (M4-S15, ADR-0059 D3): the durable truth already excludes the file — space is deferred, never durability; the retry and the boot GC both re-drive it |
//! | `blob_short_write` | `blob::device_write` | extent write cut short, typed I/O error — the append fails whole and the extent is abandoned (M4-S17, ADR-0061 D9) |
//! | `blob_fsync_err` | `ExtentWriter::finish` | typed [`ExtentWriteFailure::Fsync`](crate::ExtentWriteFailure) — a **typed abort**, the ADR-0061 D3 narrower behavior: nothing durable references the extent, the file is abandoned (never retried), the id is quarantined |
//! | `blob_unlink_fail` | `blob::unlink_extent_file` | typed I/O error, **non-fatal and counted** — the `tier_unlink_fail` posture: reclaim defers, the sweep re-drives (M4-S17, ADR-0061 D5) |
//! | `tier_write_nospace` | `tier::device_write` | `StorageFull`-kind write refusal, no byte lands (M4-S21, ADR-0063 D4): the flush slice fails typed, the store latches its device leg, MAINTAIN retries — a later success clears the latch (recovery is automatic). `FromNth` arming models "disk stays full" |
//! | `blob_write_nospace` | `blob::device_write` | `StorageFull`-kind write refusal, no byte lands (M4-S21, ADR-0063 D4): the extent is abandoned typed (`DISKFULL` at the caller), never latched — the next attempt is its own recovery probe |
//!
//! The reactor-tier analogs (driver `LogWrite`/`Fdatasync` failures) are
//! injected by the scripted driver today and by the M2-S18 sim disk,
//! which consumes this same registry for power-cut scheduling.

use std::io;

pub const LOG_APPEND_SHORT_WRITE: &str = "log_append_short_write";
pub const TORN_FRAME: &str = "torn_frame";
pub const FSYNC_ERR: &str = "fsync_err";
pub const MANIFEST_RENAME_FAIL: &str = "manifest_rename_fail";
pub const DIR_FSYNC_FAIL: &str = "dir_fsync_fail";
pub const POWER_CUT_AFTER_SEAL: &str = "power_cut_after_seal";
pub const PREALLOC_NO_SPACE: &str = "prealloc_no_space";
pub const TIER_SHORT_WRITE: &str = "tier_short_write";
pub const TIER_TORN_FRAME: &str = "tier_torn_frame";
pub const TIER_FSYNC_ERR: &str = "tier_fsync_err";
pub const TIER_FOOTER_TORN: &str = "tier_footer_torn";
pub const TIER_UNLINK_FAIL: &str = "tier_unlink_fail";
pub const BLOB_SHORT_WRITE: &str = "blob_short_write";
pub const BLOB_FSYNC_ERR: &str = "blob_fsync_err";
pub const BLOB_UNLINK_FAIL: &str = "blob_unlink_fail";
pub const TIER_WRITE_NOSPACE: &str = "tier_write_nospace";
pub const BLOB_WRITE_NOSPACE: &str = "blob_write_nospace";

/// Inventory for the CI coverage check and the S18 sim-disk scheduler.
pub const ALL: &[&str] = &[
    LOG_APPEND_SHORT_WRITE,
    TORN_FRAME,
    FSYNC_ERR,
    MANIFEST_RENAME_FAIL,
    DIR_FSYNC_FAIL,
    POWER_CUT_AFTER_SEAL,
    PREALLOC_NO_SPACE,
    TIER_SHORT_WRITE,
    TIER_TORN_FRAME,
    TIER_FSYNC_ERR,
    TIER_FOOTER_TORN,
    TIER_UNLINK_FAIL,
    BLOB_SHORT_WRITE,
    BLOB_FSYNC_ERR,
    BLOB_UNLINK_FAIL,
    TIER_WRITE_NOSPACE,
    BLOB_WRITE_NOSPACE,
];

/// The injected error a firing point surfaces (named, greppable).
pub(crate) fn injected(point: &'static str) -> io::Error {
    io::Error::other(format!("injected fault: {point}"))
}

/// The injected ENOSPC a `*_nospace` point surfaces — `StorageFull`
/// kind, so the M4-S21 classifiers key on exactly what a real device
/// returns (`fs.rs` injects the same kind).
pub(crate) fn injected_nospace(point: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::StorageFull, format!("injected fault: {point}"))
}
