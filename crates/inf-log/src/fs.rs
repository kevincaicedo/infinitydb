//! Injected filesystem seam (milestone M2 §3.3, L7): every durable-path
//! file operation inf-log performs directly — segment creation and
//! preallocation (MAINTAIN slices), seal fsync, boot-time directory scan,
//! the META/MANIFEST atomic swap ([`crate::meta`], M2-S08/S11) — flows
//! through [`SegmentFs`] so the deterministic-simulation tier can fault
//! each one (the M2-S18 sim disk implements this trait; M2-S16 names the
//! fault points).
//!
//! The *hot* path — the per-iteration frame `writev` + linked fdatasync —
//! is not here: it rides `BackendDriver` file ops wired in M2-S05, where
//! the reactor owns submission batching (L3). This trait is the
//! control-path seam.
//!
//! [`StdSegmentFs`] is the boot/dev tier (`std::fs` is permitted here by
//! §3.3, and only here). Its preallocation uses `set_len` — ftruncate
//! semantics, which on Linux extends without reserving blocks. Real
//! `fallocate` reservation arrives with the io_uring file ops (M2-S05,
//! `IORING_OP_FALLOCATE`); until then StdSegmentFs ENOSPC surfacing is
//! best-effort and the discipline is proven against [`mem::MemFs`] fault
//! injection. Recorded limitation (see the M2 ledger).

use std::io;
use std::path::Path;

/// One open segment file. Offsets are absolute; the caller (the rotor)
/// owns position bookkeeping.
pub trait SegmentFile {
    /// Write all of `data` at `offset` (short writes are completed or fail).
    fn write_at(&mut self, offset: u64, data: &[u8]) -> io::Result<()>;
    /// Read up to `buf.len()` bytes at `offset`; returns bytes read
    /// (0 = EOF). Partial reads are legal — readers loop.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize>;
    /// Current file length in bytes.
    fn file_size(&self) -> io::Result<u64>;
    /// Durably flush file *data* (fdatasync class). A failure here is
    /// fatal to the cell (§8.4 fsyncgate rule) — callers map it to
    /// [`FsyncFailed`](crate::FsyncFailed) and never retry.
    fn sync_data(&mut self) -> io::Result<()>;
    /// The platform fd, when this tier has one (M2-S05, ADR-0013 D4): the
    /// address the reactor's `BackendDriver` file ops target. `None` on
    /// in-memory tiers — the reactor path requires a real-file tier, the
    /// sim tier implements the driver ops themselves. `inf-log` never
    /// performs a syscall on it.
    fn raw_fd(&self) -> Option<std::os::fd::RawFd>;
}

/// Filesystem operations for the per-cell log directory.
pub trait SegmentFs {
    type File: SegmentFile;

    fn create_dir_all(&self, dir: &Path) -> io::Result<()>;
    /// Durably persist a directory's entries (dir-fsync). Required after
    /// segment create and before a MANIFEST rename counts (M2-S11).
    fn sync_dir(&self, dir: &Path) -> io::Result<()>;
    /// File names (not paths) in `dir`. Non-UTF-8 names are surfaced
    /// lossily so the boot scan can reject them by name — never skipped.
    fn list_dir(&self, dir: &Path) -> io::Result<Vec<String>>;
    /// Create a new segment (create-new semantics: existing file is an
    /// error) preallocated to `prealloc_bytes`. `StorageFull` here is the
    /// ENOSPC discipline seam: it must surface *before* any write needs
    /// the space (M2-S02).
    fn create_segment(&self, path: &Path, prealloc_bytes: u64) -> io::Result<Self::File>;
    /// Open an existing segment for reading and appending (the recovered
    /// tail).
    fn open_write(&self, path: &Path) -> io::Result<Self::File>;
    /// Open an existing segment read-only (sealed segments, recovery).
    fn open_read(&self, path: &Path) -> io::Result<Self::File>;
    /// Atomically rename `from` onto `to`, replacing an existing `to`
    /// (POSIX rename semantics) — the commit point of the META/MANIFEST
    /// swap protocol (M2-S08/S11). A missing `from` is `NotFound`. Durable
    /// only after the following `sync_dir`.
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
    /// Remove a file; `NotFound` when it does not exist — callers clearing
    /// staging debris (`META.new`) ignore that case explicitly.
    fn remove_file(&self, path: &Path) -> io::Result<()>;
}

/// `std::fs`-backed tier for boot scan, dev, and integration tests.
#[derive(Copy, Clone, Debug, Default)]
pub struct StdSegmentFs;

/// A real file. Uses positional I/O (`pread`/`pwrite`) — no seek state.
#[derive(Debug)]
pub struct StdSegmentFile(std::fs::File);

impl SegmentFile for StdSegmentFile {
    fn write_at(&mut self, offset: u64, data: &[u8]) -> io::Result<()> {
        std::os::unix::fs::FileExt::write_all_at(&self.0, data, offset)
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        std::os::unix::fs::FileExt::read_at(&self.0, buf, offset)
    }

    fn file_size(&self) -> io::Result<u64> {
        Ok(self.0.metadata()?.len())
    }

    fn sync_data(&mut self) -> io::Result<()> {
        self.0.sync_data()
    }

    fn raw_fd(&self) -> Option<std::os::fd::RawFd> {
        Some(std::os::fd::AsRawFd::as_raw_fd(&self.0))
    }
}

impl SegmentFs for StdSegmentFs {
    type File = StdSegmentFile;

    fn create_dir_all(&self, dir: &Path) -> io::Result<()> {
        std::fs::create_dir_all(dir)
    }

    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        std::fs::File::open(dir)?.sync_all()
    }

    fn list_dir(&self, dir: &Path) -> io::Result<Vec<String>> {
        let mut names = Vec::new();
        for entry in std::fs::read_dir(dir)? {
            names.push(entry?.file_name().to_string_lossy().into_owned());
        }
        Ok(names)
    }

    fn create_segment(&self, path: &Path, prealloc_bytes: u64) -> io::Result<Self::File> {
        let file =
            std::fs::OpenOptions::new().read(true).write(true).create_new(true).open(path)?;
        file.set_len(prealloc_bytes)?;
        file.sync_all()?;
        Ok(StdSegmentFile(file))
    }

    fn open_write(&self, path: &Path) -> io::Result<Self::File> {
        Ok(StdSegmentFile(std::fs::OpenOptions::new().read(true).write(true).open(path)?))
    }

    fn open_read(&self, path: &Path) -> io::Result<Self::File> {
        Ok(StdSegmentFile(std::fs::File::open(path)?))
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }
}

pub mod mem {
    //! Deterministic in-memory [`SegmentFs`] with targeted fault injection —
    //! the unit/property-test tier for the segment lifecycle. The M2-S18
    //! sim disk (lose/tear/reorder of un-fsynced writes) supersedes this
    //! for DST; this model keeps only what S02's ACs need: ENOSPC on
    //! preallocation and fsync failure.
    //!
    //! Single-threaded by design (cells are single-threaded — L1):
    //! `Rc<RefCell<…>>`, deterministic `BTreeMap` iteration order.

    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::rc::Rc;

    use super::{SegmentFile, SegmentFs};

    #[derive(Debug, Default)]
    struct State {
        dirs: std::collections::BTreeSet<PathBuf>,
        files: BTreeMap<PathBuf, Rc<RefCell<Vec<u8>>>>,
        /// Remaining preallocation budget; `None` = unlimited. Debited by
        /// `create_segment` — the ENOSPC injection point.
        capacity: Option<u64>,
        fail_next_sync_data: bool,
    }

    /// Shared-handle in-memory filesystem.
    #[derive(Clone, Default)]
    pub struct MemFs {
        state: Rc<RefCell<State>>,
    }

    impl std::fmt::Debug for MemFs {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            let state = self.state.borrow();
            f.debug_struct("MemFs")
                .field("files", &state.files.keys().collect::<Vec<_>>())
                .field("capacity", &state.capacity)
                .finish()
        }
    }

    impl MemFs {
        #[must_use]
        pub fn new() -> MemFs {
            MemFs::default()
        }

        /// Cap the total bytes `create_segment` may preallocate from now
        /// on. `None` lifts the cap.
        pub fn set_capacity(&self, bytes: Option<u64>) {
            self.state.borrow_mut().capacity = bytes;
        }

        /// Make the next `sync_data` on any file fail with `EIO` — the
        /// fsyncgate probe (§8.4).
        pub fn fail_next_sync_data(&self) {
            self.state.borrow_mut().fail_next_sync_data = true;
        }

        /// Raw contents of a file (test assertions).
        #[must_use]
        pub fn contents(&self, path: &Path) -> Option<Vec<u8>> {
            self.state.borrow().files.get(path).map(|data| data.borrow().clone())
        }
    }

    /// Handle onto one in-memory file.
    #[derive(Debug)]
    pub struct MemFile {
        data: Rc<RefCell<Vec<u8>>>,
        fs: Rc<RefCell<State>>,
    }

    impl SegmentFile for MemFile {
        fn write_at(&mut self, offset: u64, data: &[u8]) -> io::Result<()> {
            let mut bytes = self.data.borrow_mut();
            let offset = usize::try_from(offset).expect("offset fits usize");
            let end = offset + data.len();
            if end > bytes.len() {
                bytes.resize(end, 0);
            }
            bytes[offset..end].copy_from_slice(data);
            Ok(())
        }

        fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
            let bytes = self.data.borrow();
            let offset = usize::try_from(offset).expect("offset fits usize");
            if offset >= bytes.len() {
                return Ok(0);
            }
            let n = buf.len().min(bytes.len() - offset);
            buf[..n].copy_from_slice(&bytes[offset..offset + n]);
            Ok(n)
        }

        fn file_size(&self) -> io::Result<u64> {
            Ok(self.data.borrow().len() as u64)
        }

        fn sync_data(&mut self) -> io::Result<()> {
            let mut state = self.fs.borrow_mut();
            if state.fail_next_sync_data {
                state.fail_next_sync_data = false;
                return Err(io::Error::other("injected fsync failure"));
            }
            Ok(())
        }

        fn raw_fd(&self) -> Option<std::os::fd::RawFd> {
            None
        }
    }

    impl SegmentFs for MemFs {
        type File = MemFile;

        fn create_dir_all(&self, dir: &Path) -> io::Result<()> {
            let mut state = self.state.borrow_mut();
            let mut current = PathBuf::new();
            for part in dir.components() {
                current.push(part);
                state.dirs.insert(current.clone());
            }
            Ok(())
        }

        fn sync_dir(&self, dir: &Path) -> io::Result<()> {
            if self.state.borrow().dirs.contains(dir) {
                Ok(())
            } else {
                Err(io::Error::new(io::ErrorKind::NotFound, format!("no dir {}", dir.display())))
            }
        }

        fn list_dir(&self, dir: &Path) -> io::Result<Vec<String>> {
            let state = self.state.borrow();
            if !state.dirs.contains(dir) {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("no dir {}", dir.display()),
                ));
            }
            Ok(state
                .files
                .keys()
                .filter(|path| path.parent() == Some(dir))
                .filter_map(|path| path.file_name())
                .map(|name| name.to_string_lossy().into_owned())
                .collect())
        }

        fn create_segment(&self, path: &Path, prealloc_bytes: u64) -> io::Result<Self::File> {
            let mut state = self.state.borrow_mut();
            let parent = path.parent().unwrap_or_else(|| Path::new(""));
            if !state.dirs.contains(parent) {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("no dir {}", parent.display()),
                ));
            }
            if state.files.contains_key(path) {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("{} exists", path.display()),
                ));
            }
            if let Some(capacity) = state.capacity.as_mut() {
                if prealloc_bytes > *capacity {
                    return Err(io::Error::new(io::ErrorKind::StorageFull, "injected ENOSPC"));
                }
                *capacity -= prealloc_bytes;
            }
            let data = Rc::new(RefCell::new(vec![
                0;
                usize::try_from(prealloc_bytes)
                    .expect("prealloc fits usize")
            ]));
            state.files.insert(path.to_path_buf(), Rc::clone(&data));
            Ok(MemFile { data, fs: Rc::clone(&self.state) })
        }

        fn open_write(&self, path: &Path) -> io::Result<Self::File> {
            let state = self.state.borrow();
            let data = state.files.get(path).ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, format!("no file {}", path.display()))
            })?;
            Ok(MemFile { data: Rc::clone(data), fs: Rc::clone(&self.state) })
        }

        fn open_read(&self, path: &Path) -> io::Result<Self::File> {
            self.open_write(path)
        }

        fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
            let mut state = self.state.borrow_mut();
            let parent = to.parent().unwrap_or_else(|| Path::new(""));
            if !state.dirs.contains(parent) {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("no dir {}", parent.display()),
                ));
            }
            let data = state.files.remove(from).ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, format!("no file {}", from.display()))
            })?;
            // Replaces an existing destination atomically, like POSIX rename.
            state.files.insert(to.to_path_buf(), data);
            Ok(())
        }

        fn remove_file(&self, path: &Path) -> io::Result<()> {
            let mut state = self.state.borrow_mut();
            if state.files.remove(path).is_none() {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("no file {}", path.display()),
                ));
            }
            Ok(())
        }
    }
}
