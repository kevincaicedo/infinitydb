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

pub mod sim;

/// Tier-file I/O mode (ADR-0054): `Buffered` rides the kernel page cache;
/// `Direct` bypasses it (`O_DIRECT`) for deterministic memory accounting
/// (L5 — page-cache bytes holding tier contents belong to no allocation
/// domain). A per-*file* decision fixed at creation: the cold-read fd is
/// the writer's fd, and mixing modes on one file is the `open(2)`
/// coherence hazard the per-file shape removes structurally. Durability
/// is mode-independent: `O_DIRECT` bypasses the page cache, not the
/// device write cache — the fdatasync barrier stands either way (D7).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum TierIoMode {
    /// Kernel page cache (the pre-S09 behavior; the config fallback).
    Buffered,
    /// `O_DIRECT`, verified at open — never a silent fallback (D3).
    Direct,
}

/// Log-segment I/O mode (M4.5-S34, ADR-0086 D1) — fixed at segment
/// creation, carried by the segment. `Buffered` is the M2 path byte-for-
/// byte (sparse prealloc, frame format v2, buffered write + linked
/// fdatasync). `Direct` opens the segment `O_DIRECT`, writes 4 KiB-aligned
/// v3 frames, and — once the segment is pre-zeroed — makes `always`
/// frames write-through (FUA-class) instead of FLUSH-class. Durability is
/// mode-independent (ADR-0086 D2); the mode decides the barrier's *cost*.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub enum SegmentIoMode {
    /// Kernel page cache + fdatasync (the M2 default).
    #[default]
    Buffered,
    /// `O_DIRECT`, verified at open — never a silent fallback.
    Direct,
}

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
    /// Sets the file length (`ftruncate` semantics: shrink drops bytes,
    /// grow zero-fills). M4-S11's recovery pre-flush rule (ADR-0056 D5)
    /// is the only caller: un-manifested tier-file bytes are dead-life
    /// garbage, truncated before any new flush. Durable only after a
    /// following [`sync_data`](Self::sync_data) — the sim tier's power
    /// cut may resurrect the old tail until then (real physics).
    fn truncate(&mut self, len: u64) -> io::Result<()>;
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
    /// True when every byte of the file's length is backed by allocated,
    /// written storage — the **read, never remembered** pre-zeroing fact
    /// (ADR-0086 D4): a `Direct` segment writes write-through frames only
    /// while this holds; a sparse tail, a zero-fill whose barrier a crash
    /// lost, or a filesystem that elides zero blocks all read `false` and
    /// run FLUSH-class barriers — correct, slower, visible. The std tier
    /// reads `st_blocks`; in-memory tiers have no sparse concept (`true`);
    /// the sim tier compares its length to the preallocation target.
    fn fully_allocated(&self) -> io::Result<bool>;
    /// Advisory (M2.5-S08 read/apply overlap): the caller will soon read
    /// `[offset, offset + len)` sequentially. Tiers that can prefetch pull
    /// that window toward the page cache in the background, so the device
    /// read overlaps the caller's CPU work on the *current* window. A hint,
    /// never an effect: it changes no bytes the caller reads, so L7
    /// determinism and every digest are untouched — the default (and every
    /// in-memory/sim tier) is a no-op. The real implementation is
    /// `inf-server`'s `ReadAheadFs` boot wrapper (a per-file prefetch
    /// thread; this crate forbids unsafe and never blocks on the hint).
    fn advise_read_ahead(&self, offset: u64, len: u64) {
        let _ = (offset, len);
    }
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
    /// Create a tier file in `mode` (M4-S09, ADR-0054 D1). The default
    /// delegates to [`create_segment`](Self::create_segment) ignoring the
    /// mode — honest **by construction** on the in-memory/sim tiers, whose
    /// byte-visibility model has no page cache to bypass (`Buffered ≡
    /// Direct` there), never a silent fallback on a real filesystem:
    /// [`StdSegmentFs`] implements the real thing and refuses typed when
    /// `Direct` does not take effect (D3). Wrappers must forward this
    /// explicitly (falling into the default would drop the flag).
    fn create_tier(&self, path: &Path, mode: TierIoMode) -> io::Result<Self::File> {
        let _ = mode;
        self.create_segment(path, 0)
    }
    /// Open an **existing** tier file in `mode` (M4-S11, ADR-0056 D5 —
    /// the recovery reopen). Same honesty contract as
    /// [`create_tier`](Self::create_tier): the default delegates to
    /// [`open_write`](Self::open_write) (mode-equivalent on
    /// in-memory/sim tiers), [`StdSegmentFs`] applies and verifies
    /// `O_DIRECT`, and wrappers must forward explicitly.
    fn open_tier(&self, path: &Path, mode: TierIoMode) -> io::Result<Self::File> {
        let _ = mode;
        self.open_write(path)
    }
    /// [`create_segment`](Self::create_segment) with **no durability side
    /// effects** (M2.5-S01 boot barriers): the caller owns making the file
    /// and its directory entry durable — driver-ridden fdatasync barriers
    /// registered ahead of every data fsync in the group-commit ledger —
    /// before any durable ack can reference it. A create-time sync here
    /// blocks the reactor behind foreign journal writeback (the boot-wedge
    /// mechanism). The default falls back to the synced create: correct,
    /// but it pays the barrier at create time.
    fn create_segment_unsynced(&self, path: &Path, prealloc_bytes: u64) -> io::Result<Self::File> {
        self.create_segment(path, prealloc_bytes)
    }
    /// Create a `Direct`-mode segment (ADR-0086 D4): `O_DIRECT` (verified
    /// on the std tier, `Unsupported` off Linux — never a silent
    /// fallback), **sparse** at `prealloc_bytes`, no durability side
    /// effects (the M2.5-S01 deferred shape). The rotor zero-fills it
    /// through the driver before its first frame; until the zero-fill
    /// barrier lands, [`SegmentFile::fully_allocated`] is `false`. The
    /// default delegates to the synced buffered create — honest on tiers
    /// without a page cache to bypass (`Buffered ≡ Direct` there, and a
    /// zero-filled in-memory vector is "fully allocated"); wrappers must
    /// forward explicitly.
    fn create_segment_direct(&self, path: &Path, prealloc_bytes: u64) -> io::Result<Self::File> {
        self.create_segment(path, prealloc_bytes)
    }
    /// Open an **existing** segment for append in `mode` (ADR-0086 D4 —
    /// the recovered tail): `Direct` opens `O_DIRECT` (verified);
    /// `Buffered` is [`open_write`](Self::open_write). Whether the
    /// reopened file is pre-zeroed is the caller's
    /// [`SegmentFile::fully_allocated`] question, not this method's. The
    /// default ignores the mode (in-memory/sim tiers); wrappers forward.
    fn open_segment_append(&self, path: &Path, mode: SegmentIoMode) -> io::Result<Self::File> {
        let _ = mode;
        self.open_write(path)
    }
    /// Create a new *staging* file (create-new semantics) with **no
    /// durability side effects** (M2-S11/S12): META/MANIFEST `.new` and
    /// `.ick.new` staging gets its data durability from an explicit
    /// fdatasync and its name durability from the publication dir-fsync —
    /// a create-time sync here is a wasted device barrier on the loop.
    fn create_meta(&self, path: &Path) -> io::Result<Self::File>;
    /// `create_meta` with `O_DIRECT` (M4.5-S36, ADR-0088 D3): the `.ick`
    /// staging file whose v3 blocks are written as aligned direct writes.
    /// **Required, not defaulted** — a default falling back to
    /// `create_meta` would be a buffered file wearing a direct label
    /// (ADR-0054 D3). Std verifies the flag through fdinfo; the memory
    /// and sim tiers record the mode and assert every write's alignment,
    /// so the simulator catches what tmpfs swallows. Wrappers forward.
    fn create_meta_direct(&self, path: &Path) -> io::Result<Self::File>;
    /// Open a directory as a syncable handle (M2-S11/S12): `sync_data` on
    /// it is the dir-fsync, and `raw_fd` is the address a driver
    /// `Fdatasync` targets so publication barriers never block the loop.
    /// Reads/writes on the handle are meaningless and unused.
    fn open_dir(&self, dir: &Path) -> io::Result<Self::File>;
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

    fn truncate(&mut self, len: u64) -> io::Result<()> {
        self.0.set_len(len)
    }

    fn sync_data(&mut self) -> io::Result<()> {
        self.0.sync_data()
    }

    fn raw_fd(&self) -> Option<std::os::fd::RawFd> {
        Some(std::os::fd::AsRawFd::as_raw_fd(&self.0))
    }

    fn fully_allocated(&self) -> io::Result<bool> {
        let meta = self.0.metadata()?;
        // `st_blocks` is in 512-byte units regardless of the block size.
        // Our writer allocates only as a prefix (sequential zero-fill or
        // sequential frames), so "all blocks present" ⇔ "no sparse tail".
        // `fallocate`d unwritten extents would also count, which is why
        // the writer never uses `fallocate` (ADR-0086 D4).
        let allocated = std::os::unix::fs::MetadataExt::blocks(&meta).saturating_mul(512);
        Ok(allocated >= meta.len())
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

    fn create_segment_unsynced(&self, path: &Path, prealloc_bytes: u64) -> io::Result<Self::File> {
        // No sync_all: an fsync here commits the whole ext4 journal and
        // blocks the reactor behind any entangled foreign writeback for
        // an unbounded time (M2.5-S01). Durability rides the boot-barrier
        // fdatasyncs the caller registers in the group-commit ledger.
        let file =
            std::fs::OpenOptions::new().read(true).write(true).create_new(true).open(path)?;
        file.set_len(prealloc_bytes)?;
        Ok(StdSegmentFile(file))
    }

    fn create_segment_direct(&self, path: &Path, prealloc_bytes: u64) -> io::Result<Self::File> {
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::fs::OpenOptionsExt;
            // Sparse + unsynced like `create_segment_unsynced`: the zero-
            // fill through the driver allocates the extents and its
            // barrier commits them; the dir entry rides the prealloc dir
            // barrier (ADR-0086 D4).
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .custom_flags(libc::O_DIRECT)
                .open(path)?;
            verify_o_direct(&file, path)?;
            file.set_len(prealloc_bytes)?;
            Ok(StdSegmentFile(file))
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (path, prealloc_bytes);
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "O_DIRECT log segments are Linux-only (ADR-0086 D1); configure Buffered",
            ))
        }
    }

    fn open_segment_append(&self, path: &Path, mode: SegmentIoMode) -> io::Result<Self::File> {
        match mode {
            SegmentIoMode::Buffered => self.open_write(path),
            #[cfg(target_os = "linux")]
            SegmentIoMode::Direct => {
                use std::os::unix::fs::OpenOptionsExt;
                let file = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .custom_flags(libc::O_DIRECT)
                    .open(path)?;
                verify_o_direct(&file, path)?;
                Ok(StdSegmentFile(file))
            }
            #[cfg(not(target_os = "linux"))]
            SegmentIoMode::Direct => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "O_DIRECT log segments are Linux-only (ADR-0086 D1); configure Buffered",
            )),
        }
    }

    fn create_tier(&self, path: &Path, mode: TierIoMode) -> io::Result<Self::File> {
        match mode {
            TierIoMode::Buffered => self.create_segment(path, 0),
            #[cfg(target_os = "linux")]
            TierIoMode::Direct => {
                use std::os::unix::fs::OpenOptionsExt;
                let file = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create_new(true)
                    .custom_flags(libc::O_DIRECT)
                    .open(path)?;
                verify_o_direct(&file, path)?;
                file.sync_all()?;
                Ok(StdSegmentFile(file))
            }
            #[cfg(not(target_os = "linux"))]
            TierIoMode::Direct => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "O_DIRECT tier files are Linux-only (ADR-0054 D6); configure Buffered",
            )),
        }
    }

    fn open_tier(&self, path: &Path, mode: TierIoMode) -> io::Result<Self::File> {
        match mode {
            TierIoMode::Buffered => self.open_write(path),
            #[cfg(target_os = "linux")]
            TierIoMode::Direct => {
                use std::os::unix::fs::OpenOptionsExt;
                let file = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .custom_flags(libc::O_DIRECT)
                    .open(path)?;
                verify_o_direct(&file, path)?;
                Ok(StdSegmentFile(file))
            }
            #[cfg(not(target_os = "linux"))]
            TierIoMode::Direct => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "O_DIRECT tier files are Linux-only (ADR-0054 D6); configure Buffered",
            )),
        }
    }

    fn create_meta(&self, path: &Path) -> io::Result<Self::File> {
        let file =
            std::fs::OpenOptions::new().read(true).write(true).create_new(true).open(path)?;
        Ok(StdSegmentFile(file))
    }

    fn create_meta_direct(&self, path: &Path) -> io::Result<Self::File> {
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::fs::OpenOptionsExt;
            // No preallocation: plain `O_DIRECT` writes allocate as they
            // go and the terminal fdatasync commits the metadata once
            // (the FUA-on-unwritten-extent trap is an `O_DSYNC` trap;
            // the checkpoint has no per-write barrier — ADR-0088 D3).
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .custom_flags(libc::O_DIRECT)
                .open(path)?;
            verify_o_direct(&file, path)?;
            Ok(StdSegmentFile(file))
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = path;
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "O_DIRECT checkpoint files are Linux-only (ADR-0088 D3)",
            ))
        }
    }

    fn open_dir(&self, dir: &Path) -> io::Result<Self::File> {
        Ok(StdSegmentFile(std::fs::File::open(dir)?))
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

/// Asserts the `Direct` open took effect (ADR-0054 D3): `open(2)` can
/// accept and then ignore `O_DIRECT` on some filesystems/kernels, and a
/// buffered file wearing a direct label defeats the L5 accounting the
/// mode exists for. Flags are re-read from `/proc/self/fdinfo` — a safe
/// procfs read; this crate forbids unsafe, so no `fcntl` — and absence
/// of the flag is a typed `Unsupported` refusal, never a downgrade.
#[cfg(target_os = "linux")]
fn verify_o_direct(file: &std::fs::File, path: &Path) -> io::Result<()> {
    let fd = std::os::fd::AsRawFd::as_raw_fd(file);
    let info = std::fs::read_to_string(format!("/proc/self/fdinfo/{fd}"))?;
    let flags = info
        .lines()
        .find_map(|line| line.strip_prefix("flags:"))
        .map(str::trim)
        .and_then(|octal| i32::from_str_radix(octal, 8).ok())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "fdinfo flags unreadable for O_DIRECT verify",
            )
        })?;
    if flags & libc::O_DIRECT == 0 {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("O_DIRECT requested but not in effect on {} (ADR-0054 D3)", path.display()),
        ));
    }
    Ok(())
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
        /// Crash-at-step injection (M2-S11): `Some(n)` allows `n` more
        /// mutating operations, then **every** further mutating op fails —
        /// modeling a dead process whose best-effort cleanup also never
        /// runs. Cleared by `clear_op_fault` (the "reboot").
        ops_until_fault: Option<u64>,
    }

    impl State {
        /// Charge one mutating operation against the crash countdown.
        fn tick_op(&mut self) -> io::Result<()> {
            match self.ops_until_fault {
                None => Ok(()),
                Some(0) => Err(io::Error::other("injected fault: op budget exhausted (crash)")),
                Some(ref mut n) => {
                    *n -= 1;
                    Ok(())
                }
            }
        }
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

        /// Crash-at-step injection (M2-S11 swap matrix): allow `n` more
        /// mutating operations (create, write, sync, rename, remove), then
        /// fail every one until [`Self::clear_op_fault`] — the dead
        /// process's cleanup must fail too, or the crash state would be
        /// tidied away before "recovery" observes it.
        pub fn fail_after_ops(&self, n: u64) {
            self.state.borrow_mut().ops_until_fault = Some(n);
        }

        /// Lift the op-countdown fault (the "reboot" before recovery runs).
        pub fn clear_op_fault(&self) {
            self.state.borrow_mut().ops_until_fault = None;
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
            self.fs.borrow_mut().tick_op()?;
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

        fn truncate(&mut self, len: u64) -> io::Result<()> {
            self.fs.borrow_mut().tick_op()?;
            let len = usize::try_from(len).expect("length fits usize");
            self.data.borrow_mut().resize(len, 0);
            Ok(())
        }

        fn sync_data(&mut self) -> io::Result<()> {
            let mut state = self.fs.borrow_mut();
            state.tick_op()?;
            if state.fail_next_sync_data {
                state.fail_next_sync_data = false;
                return Err(io::Error::other("injected fsync failure"));
            }
            Ok(())
        }

        fn raw_fd(&self) -> Option<std::os::fd::RawFd> {
            None
        }

        fn fully_allocated(&self) -> io::Result<bool> {
            // No sparse concept: a vector's bytes all exist.
            Ok(true)
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
            let mut state = self.state.borrow_mut();
            state.tick_op()?;
            if state.dirs.contains(dir) {
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
            state.tick_op()?;
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

        fn create_meta(&self, path: &Path) -> io::Result<Self::File> {
            // Same create-new semantics, zero length, no capacity debit
            // (staging envelopes, not preallocated segments).
            self.create_segment(path, 0)
        }

        fn create_meta_direct(&self, path: &Path) -> io::Result<Self::File> {
            // Direct ≡ buffered in memory; the alignment of every write
            // is asserted by the caller's block layout (v3 sealer) and by
            // the sim tier — this tier records nothing (ADR-0088 D3).
            self.create_segment(path, 0)
        }

        fn open_dir(&self, dir: &Path) -> io::Result<Self::File> {
            let state = self.state.borrow();
            if !state.dirs.contains(dir) {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("no dir {}", dir.display()),
                ));
            }
            // A syncable stand-in: `sync_data` ticks the fault countdown
            // like every other barrier; the buffer is never read/written.
            Ok(MemFile { data: Rc::new(RefCell::new(Vec::new())), fs: Rc::clone(&self.state) })
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
            state.tick_op()?;
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
            state.tick_op()?;
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
