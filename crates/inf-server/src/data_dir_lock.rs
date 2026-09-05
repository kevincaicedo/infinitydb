//! The data directory's owner lock (ADR-0094 D7): `<data_dir>/LOCK`, held
//! **exclusive** for the process's life and taken before anything else
//! touches the directory — the key-hash secret, its binding scan, the
//! device profile, the catalog, every shard. Two first boots can no
//! longer both observe "no secret" and each write one; two nodes can no
//! longer serve one directory. The kernel releases the lock on any exit,
//! SIGKILL included, so the crash matrix's kill-and-restart needs no
//! cleanup and no stale-lock heuristic exists to get wrong. Contention is
//! a typed refusal, never a wait: a second owner of a directory is an
//! operator error, not a queue. Boot code: blocking file I/O is fine
//! here; no cell runs yet. The simulators hold no lock (one process by
//! construction; their seam is the injected filesystem).

use std::fmt;
use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};

/// The lock file's name under the data directory. Not data for the
/// ADR-0094 D2 predates rule (that looks at `META` and `shard-*`).
pub const LOCK_FILE: &str = "LOCK";

/// Why the directory could not be owned — each a boot refusal.
#[derive(Debug)]
pub enum DataDirLockError {
    /// Another process holds the lock.
    Held { path: PathBuf },
    /// The lock file could not be created, opened, or locked.
    Io { path: PathBuf, err: io::Error },
}

impl fmt::Display for DataDirLockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DataDirLockError::Held { path } => write!(
                f,
                "{}: held by another process — one process owns a data directory (ADR-0094 D7); \
                 stop it, or point this boot at another directory (fail-stop)",
                path.display()
            ),
            DataDirLockError::Io { path, err } => write!(f, "{}: {err}", path.display()),
        }
    }
}

impl std::error::Error for DataDirLockError {}

/// The held lock. Dropping it (or the process ending) releases the
/// directory; a node keeps it for its whole life.
#[derive(Debug)]
pub struct DataDirLock {
    /// The open, locked file — the lock lives exactly as long as it.
    _file: File,
    path: PathBuf,
}

impl DataDirLock {
    /// Own `data_dir` (created if absent) or refuse.
    ///
    /// # Errors
    /// [`DataDirLockError::Held`] when another process owns it;
    /// [`DataDirLockError::Io`] for any other failure.
    pub fn acquire(data_dir: &Path) -> Result<DataDirLock, DataDirLockError> {
        let path = data_dir.join(LOCK_FILE);
        let io_err = |err: io::Error| DataDirLockError::Io { path: path.clone(), err };
        std::fs::create_dir_all(data_dir).map_err(io_err)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(io_err)?;
        match file.try_lock() {
            Ok(()) => Ok(DataDirLock { _file: file, path }),
            Err(TryLockError::WouldBlock) => Err(DataDirLockError::Held { path }),
            Err(TryLockError::Error(err)) => Err(io_err(err)),
        }
    }

    /// The lock file's path (the boot line names it).
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(
        clippy::disallowed_methods,
        reason = "test-only: a wall-clock stamp names the scratch dir"
    )]
    fn fresh_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "inf-data-dir-lock-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// ADR-0094 D7 within one process: a second `acquire` on a held
    /// directory is the typed refusal (the lock is per open file, so a
    /// second handle contends exactly as a second process would); the
    /// release on drop lets the next owner in.
    #[test]
    fn a_held_directory_refuses_a_second_owner_until_released() {
        let dir = fresh_dir("held");
        let first = DataDirLock::acquire(&dir).expect("first owner");
        assert_eq!(first.path(), dir.join(LOCK_FILE));
        let err = DataDirLock::acquire(&dir).expect_err("second owner refused");
        assert!(matches!(err, DataDirLockError::Held { .. }), "{err}");
        assert!(err.to_string().contains("held by another process"), "{err}");
        drop(first);
        let _next = DataDirLock::acquire(&dir).expect("released on drop");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
