//! Tier-file retirement on the control thread (ADR-0156 D3): the final
//! close of a retired tier file's handle.
//!
//! A retiring file's one handle is closed once, by its last owner, and the
//! close's error is reported, never retried by a second close: the number
//! is released on every return (Linux releases it even when `close`
//! reports `EINTR` or `EIO`), so a second close would close whatever file
//! or socket reused it. [`FinalClose`] is that close for each file type a
//! cell's tier pipeline holds: the real file's one `close(2)`, the
//! simulated disk's close (where `tier_close_fail` fires), and the
//! in-memory file's drop.

use std::io;

use inf_log::fs::StdSegmentFile;
use inf_log::fs::mem::MemFile;
use inf_log::fs::sim::SimFile;

/// A tier file's handle closed once, by its final owner (ADR-0156 D3): the
/// handle is consumed on every outcome, so nothing can close it again.
pub trait FinalClose {
    /// Closes the handle and reports the close's own error.
    ///
    /// # Errors
    /// The close's error: an errno `close(2)` reported, or the simulated
    /// disk's `EIO` from `tier_close_fail`. The handle is gone either way.
    fn final_close(self) -> io::Result<()>;
}

impl FinalClose for StdSegmentFile {
    fn final_close(self) -> io::Result<()> {
        inf_runtime::net::close_reporting(self.into_owned_fd())
    }
}

impl FinalClose for SimFile {
    fn final_close(self) -> io::Result<()> {
        self.close_reporting()
    }
}

impl FinalClose for MemFile {
    fn final_close(self) -> io::Result<()> {
        drop(self);
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test-only: real filesystem fixtures and /proc")]
mod tests {
    use std::path::Path;

    use inf_log::fs::sim::SimDisk;
    use inf_log::fs::{SegmentFile as _, SegmentFs as _, StdSegmentFs, TierIoMode};

    use super::*;

    /// The real file's final close is its one `close(2)`: on Linux its
    /// descriptor's entry is gone, or names another file once a later open
    /// reused the number.
    #[test]
    fn the_real_files_final_close_closes_its_descriptor() {
        let dir = std::env::temp_dir().join(format!("inf-final-close-{}", std::process::id()));
        let fs = StdSegmentFs;
        fs.create_dir_all(&dir).expect("dir");
        let path = dir.join("tier-000000.itier");
        let file = fs.create_tier(&path, TierIoMode::Buffered).expect("create");
        let entry = format!("/proc/self/fd/{}", file.raw_fd().expect("a real descriptor"));
        let before = std::fs::read_link(&entry).ok();
        assert_eq!(file.final_close().map_err(|e| e.kind()), Ok(()));
        if cfg!(target_os = "linux") {
            assert_eq!(before.as_deref(), Some(path.as_path()), "the entry named the file");
            assert_ne!(std::fs::read_link(&entry).ok(), before, "its descriptor is closed");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The simulated file's final close is the disk's: its fd leaves the
    /// open table and the close is logged; the in-memory file's is a drop.
    #[test]
    fn the_simulated_and_in_memory_final_closes_close() {
        let disk = SimDisk::new();
        let cold = Path::new("shard-0/cold");
        disk.create_dir_all(cold).expect("dirs");
        let file = disk.create_tier(&cold.join("tier-000000.itier"), TierIoMode::Buffered);
        let tier = |path: &Path| path.extension().is_some_and(|ext| ext == "itier");
        let file = file.expect("create");
        assert_eq!(disk.open_files_named(tier), 1);
        assert_eq!(file.final_close().map_err(|e| e.kind()), Ok(()));
        assert_eq!(disk.open_files_named(tier), 0, "its fd left the disk's open table");
        assert_eq!(disk.logged_calls(0).len(), 1, "the disk logged the close");

        let fs = inf_log::fs::mem::MemFs::new();
        fs.create_dir_all(cold).expect("dirs");
        let file = fs.create_tier(&cold.join("tier-000000.itier"), TierIoMode::Buffered);
        assert_eq!(file.expect("create").final_close().map_err(|e| e.kind()), Ok(()));
    }
}
