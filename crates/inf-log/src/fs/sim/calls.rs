//! The simulated disk's call log: the calls an oracle times and matches,
//! each logged by the call that performs it, so the code an oracle judges
//! neither writes an entry nor can skip one. A failure the disk did not
//! log was not injected.
//!
//! Two calls are logged: a checkpoint's staging create (`create_meta` or
//! `create_meta_direct` on a `ckpt-NNNNNN.ick.new` name), with its result
//! and whether the name exists after it, and a rename onto a shard's
//! `MANIFEST`, the recovery unit's commit point. Each entry carries the
//! harness's last [`DiskMark`]: the phase that ran and its injected time,
//! so an oracle spaces two creates by the time the harness gave, never by
//! a clock the cell under test keeps.

use std::io;
use std::path::{Path, PathBuf};

use inf_foundation::CellId;
use inf_foundation::time::Nanos;

use crate::limits::SIM_DISK_CALLS_LOGGED_MAX;

/// What the harness was running when the disk logged a call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiskPhase {
    /// One iteration of this cell's loop.
    Cell(CellId),
}

/// The harness's mark: the phase it runs and the injected time it gave
/// that phase. Every call logged after [`SimDisk::mark`](super::SimDisk::mark)
/// carries it, until the next mark.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiskMark {
    pub phase: DiskPhase,
    pub at: Nanos,
}

/// A logged call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiskCall {
    /// A checkpoint's staging file created. `left_file`: the name exists
    /// after the call, whatever its result.
    CheckpointCreate { left_file: bool },
    /// A rename onto a shard's `MANIFEST` (the path logged is the target).
    ManifestRename,
}

/// How a logged call ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallResult {
    Ok,
    /// A fault point fired inside the call; its name.
    Injected(&'static str),
    /// The call failed otherwise; the error's kind.
    Failed(io::ErrorKind),
}

impl CallResult {
    /// The result of a call that ran, no point having fired in it.
    #[must_use]
    pub fn of<T>(result: &io::Result<T>) -> CallResult {
        match result {
            Ok(_) => CallResult::Ok,
            Err(err) => CallResult::Failed(err.kind()),
        }
    }
}

/// One entry of the call log. `mark` is `None` for a call made before the
/// harness's first mark.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoggedCall {
    pub call: DiskCall,
    pub path: PathBuf,
    pub result: CallResult,
    pub mark: Option<DiskMark>,
}

/// The log, its current mark and the calls it could not hold. A power cut
/// keeps it: it is the harness's record of what the disk was asked, not
/// disk state.
#[derive(Debug, Default)]
pub(super) struct CallLog {
    mark: Option<DiskMark>,
    entries: Vec<LoggedCall>,
    unlogged: u64,
}

impl CallLog {
    pub(super) fn mark(&mut self, mark: DiskMark) {
        self.mark = Some(mark);
    }

    /// Appends one call under the current mark; past
    /// [`SIM_DISK_CALLS_LOGGED_MAX`] the call is counted, not kept.
    pub(super) fn record(&mut self, call: DiskCall, path: &Path, result: CallResult) {
        if self.entries.len() >= SIM_DISK_CALLS_LOGGED_MAX {
            self.unlogged += 1;
            return;
        }
        let mark = self.mark;
        self.entries.push(LoggedCall { call, path: path.to_path_buf(), result, mark });
    }

    /// The entries from `first` on, in call order; none past the end.
    pub(super) fn entries_from(&self, first: usize) -> Vec<LoggedCall> {
        self.entries.get(first..).map_or_else(Vec::new, <[LoggedCall]>::to_vec)
    }

    pub(super) fn unlogged(&self) -> u64 {
        self.unlogged
    }
}

/// A checkpoint's staging name, `ckpt-NNNNNN.ick.new`.
pub(super) fn is_checkpoint_staging(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .and_then(crate::ckpt::parse_ick_staging_file_name)
        .is_some()
}

/// A shard's committed `MANIFEST` name.
pub(super) fn is_manifest(path: &Path) -> bool {
    path.file_name().is_some_and(|name| name == crate::manifest::MANIFEST_FILE)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::fs::SegmentFs;
    use crate::fs::sim::SimDisk;

    fn entry(
        call: DiskCall,
        path: &Path,
        result: CallResult,
        mark: Option<DiskMark>,
    ) -> LoggedCall {
        LoggedCall { call, path: path.to_path_buf(), result, mark }
    }

    /// Each checkpoint staging create and each rename onto `MANIFEST` is
    /// logged by the call itself, under the mark set before it, with its
    /// result and, for a create, whether the name exists after it; no
    /// other create or rename is. A power cut keeps the log.
    #[test]
    fn the_disk_logs_staging_creates_and_manifest_renames_under_the_mark() {
        let disk = SimDisk::new();
        let (shard, ckpt) = (Path::new("shard-0"), Path::new("shard-0/ckpt"));
        disk.create_dir_all(ckpt).expect("dirs");
        let (first, second, third) = (
            ckpt.join("ckpt-000001.ick.new"),
            ckpt.join("ckpt-000002.ick.new"),
            ckpt.join("ckpt-000003.ick.new"),
        );
        let (staged, manifest) = (shard.join("MANIFEST.new"), shard.join("MANIFEST"));
        let _unmarked = disk.create_meta(&first).expect("first create");
        let mark = DiskMark { phase: DiskPhase::Cell(CellId(1)), at: Nanos(5_000) };
        disk.mark(mark.phase, mark.at);
        let _direct = disk.create_meta_direct(&second).expect("direct create");
        let _probe = disk.create_meta_direct(&ckpt.join(".direct-probe")).expect("probe");
        let _manifest = disk.create_meta(&staged).expect("staged MANIFEST");
        assert!(disk.create_meta(&first).is_err(), "the name exists");
        disk.refuse_direct_meta();
        assert!(disk.create_meta_direct(&third).is_err(), "O_DIRECT refused");
        disk.rename(&second, &ckpt.join("ckpt-000002.ick")).expect("publish");
        disk.rename(&staged, &manifest).expect("commit");
        assert!(disk.rename(&staged, &manifest).is_err(), "nothing left to rename");
        disk.power_cut(7);
        let made = DiskCall::CheckpointCreate { left_file: true };
        let none = DiskCall::CheckpointCreate { left_file: false };
        let failed = CallResult::Failed;
        let expected = vec![
            entry(made, &first, CallResult::Ok, None),
            entry(made, &second, CallResult::Ok, Some(mark)),
            entry(made, &first, failed(io::ErrorKind::AlreadyExists), Some(mark)),
            entry(none, &third, failed(io::ErrorKind::Unsupported), Some(mark)),
            entry(DiskCall::ManifestRename, &manifest, CallResult::Ok, Some(mark)),
            entry(DiskCall::ManifestRename, &manifest, failed(io::ErrorKind::NotFound), Some(mark)),
        ];
        assert_eq!(disk.logged_calls(0), expected, "a cut keeps the log");
        assert_eq!(disk.logged_calls(4), expected[4..], "a reader resumes at its index");
        assert!(disk.logged_calls(expected.len() + 1).is_empty(), "past the end: none");
        assert_eq!(disk.calls_unlogged(), 0);
    }

    /// `ckpt_create_fail` refuses a checkpoint's staging create on both
    /// create calls before the file exists: the error names the point, no
    /// file is left, and the log records each refusal under the mark, then
    /// the create that follows once the point is disarmed. A create of
    /// another name never meets it.
    #[test]
    fn a_refused_staging_create_leaves_no_file_and_is_logged() {
        use inf_foundation::fault::{self, FaultSpec};
        let disk = SimDisk::new();
        let ckpt = Path::new("shard-0/ckpt");
        disk.create_dir_all(ckpt).expect("dirs");
        let (staging, other) =
            (ckpt.join("ckpt-000004.ick.new"), Path::new("shard-0/MANIFEST.new"));
        let mark = DiskMark { phase: DiskPhase::Cell(CellId(0)), at: Nanos(9) };
        disk.mark(mark.phase, mark.at);
        fault::arm(crate::fault::CKPT_CREATE_FAIL, FaultSpec::Always);
        let refused = [disk.create_meta(&staging).err(), disk.create_meta_direct(&staging).err()];
        let other_created = disk.create_meta(other).map(drop);
        let fired = fault::fired(crate::fault::CKPT_CREATE_FAIL);
        fault::disarm_all();
        for err in refused {
            let err = err.expect("the staging create is refused");
            assert!(err.to_string().contains("ckpt_create_fail"), "typed and named: {err}");
        }
        assert_eq!(fired, 2, "engagement: both staging creates met the point");
        assert!(other_created.is_ok(), "another name never meets the point");
        assert!(disk.open_read(&staging).is_err(), "a refused create leaves no file");
        let _created = disk.create_meta_direct(&staging).expect("disarmed, the create runs");
        let point = CallResult::Injected(crate::fault::CKPT_CREATE_FAIL);
        let none = DiskCall::CheckpointCreate { left_file: false };
        let made = DiskCall::CheckpointCreate { left_file: true };
        let expected = vec![
            entry(none, &staging, point, Some(mark)),
            entry(none, &staging, point, Some(mark)),
            entry(made, &staging, CallResult::Ok, Some(mark)),
        ];
        assert_eq!(disk.logged_calls(0), expected, "each attempt is a logged create");
    }

    /// At the cap a call is counted, not kept: the log a reader sees is
    /// whole or says it is not.
    #[test]
    fn a_call_past_the_cap_is_counted_not_kept() {
        let mut log = CallLog::default();
        let path = Path::new("shard-0/MANIFEST");
        for _ in 0..=SIM_DISK_CALLS_LOGGED_MAX {
            log.record(DiskCall::ManifestRename, path, CallResult::Ok);
        }
        assert_eq!(log.entries_from(0).len(), SIM_DISK_CALLS_LOGGED_MAX);
        assert_eq!(log.unlogged(), 1, "the one call past the cap");
    }
}
