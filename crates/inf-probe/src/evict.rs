//! `inf cache-evict <dir>` (M4.5-S34, the C38b replay clause): drop a
//! data directory's files from the kernel page cache so the next boot
//! reads them from the device — a power-loss-shaped recovery row on a
//! box without `drop_caches` (sudo). Dirty pages are not evictable, so
//! each file is synced first (`fdatasync` on a read-only handle is legal
//! on Linux); a failure on one file is the report's, never a silent
//! partial eviction (L10).
//!
//! The one `unsafe` in this crate: `posix_fadvise(fd, 0, 0, DONTNEED)`
//! — a syscall `std` does not expose, taking a live file descriptor and
//! three integers, no pointers. Inventoried in `SAFETY.md`.

use std::fs::File;
use std::io;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

/// Directory-walk depth bound: a data directory nests `cell-N/…` two
/// levels deep; anything deeper is not ours to touch.
const MAX_DEPTH: usize = 6;
/// File count bound (a data directory holds segments, tier files,
/// extents and checkpoints — thousands, never millions).
const MAX_FILES: u64 = 1 << 20;

/// What one eviction did.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct EvictReport {
    /// Regular files advised.
    pub files: u64,
    /// Their byte lengths summed (the page cache cannot be asked how much
    /// it held; this is the upper bound of what left it).
    pub bytes: u64,
    /// Directories walked.
    pub dirs: u64,
}

/// Syncs and advises `DONTNEED` every regular file under `dir`
/// (bounded depth and count).
///
/// # Errors
/// The first I/O failure (walk, open, sync, advise) — the report is
/// then unknown and the caller must not read the boot as cold.
pub fn evict_page_cache(dir: &Path) -> io::Result<EvictReport> {
    let mut report = EvictReport::default();
    let mut stack: Vec<(PathBuf, usize)> = vec![(dir.to_path_buf(), 0)];
    while let Some((path, depth)) = stack.pop() {
        if depth > MAX_DEPTH {
            return Err(io::Error::other(format!(
                "{}: nested deeper than {MAX_DEPTH}",
                path.display()
            )));
        }
        report.dirs += 1;
        for entry in std::fs::read_dir(&path)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_dir() {
                stack.push((entry.path(), depth + 1));
            } else if kind.is_file() {
                if report.files >= MAX_FILES {
                    return Err(io::Error::other(format!(
                        "{}: more than {MAX_FILES} files",
                        dir.display()
                    )));
                }
                report.bytes += evict_file(&entry.path())?;
                report.files += 1;
            }
        }
    }
    Ok(report)
}

/// Sync then advise one file; returns its length.
fn evict_file(path: &Path) -> io::Result<u64> {
    let file = File::open(path)?;
    let len = file.metadata()?.len();
    file.sync_data()?;
    fadvise_dontneed(&file)?;
    Ok(len)
}

#[cfg(target_os = "linux")]
fn fadvise_dontneed(file: &File) -> io::Result<()> {
    let fd = file.as_raw_fd();
    // SAFETY: `posix_fadvise` takes a file descriptor and three plain
    // integers — no pointers, no memory the kernel writes back to us.
    // `fd` is `file`'s live descriptor for the call's whole duration
    // (`file` is borrowed), and `(0, 0)` names the whole file, which
    // POSIX defines. A nonzero return is an errno value, surfaced typed.
    let rc = unsafe { libc::posix_fadvise(fd, 0, 0, libc::POSIX_FADV_DONTNEED) };
    if rc != 0 {
        return Err(io::Error::from_raw_os_error(rc));
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn fadvise_dontneed(_file: &File) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "cache-evict is Linux-only: posix_fadvise(DONTNEED) has no page-cache-evicting \
         equivalent on this platform",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory tree is walked to its files, each synced and advised;
    /// the report counts files, bytes and directories exactly.
    #[test]
    fn evicts_every_regular_file_under_the_directory() {
        let root = std::env::temp_dir().join(format!("inf-probe-evict-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("cell-0/log")).expect("mkdir");
        std::fs::write(root.join("io-properties.toml"), b"x = 1\n").expect("write");
        std::fs::write(root.join("cell-0/log/000001.seg"), vec![7u8; 8192]).expect("write");
        std::fs::write(root.join("cell-0/ckpt.ick"), vec![9u8; 100]).expect("write");
        let report = evict_page_cache(&root).expect("evict");
        assert_eq!(report.files, 3);
        assert_eq!(report.bytes, 6 + 8192 + 100);
        assert_eq!(report.dirs, 3);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A missing directory is a typed error, not a zero report read as
    /// "nothing to evict".
    #[test]
    fn a_missing_directory_is_an_error() {
        let missing = std::env::temp_dir().join("inf-probe-evict-definitely-missing");
        assert!(evict_page_cache(&missing).is_err());
    }
}
