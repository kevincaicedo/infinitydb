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

/// Inventory for the CI coverage check and the S18 sim-disk scheduler.
pub const ALL: &[&str] = &[
    LOG_APPEND_SHORT_WRITE,
    TORN_FRAME,
    FSYNC_ERR,
    MANIFEST_RENAME_FAIL,
    DIR_FSYNC_FAIL,
    POWER_CUT_AFTER_SEAL,
    PREALLOC_NO_SPACE,
];

/// The injected error a firing point surfaces (named, greppable).
pub(crate) fn injected(point: &'static str) -> io::Error {
    io::Error::other(format!("injected fault: {point}"))
}
