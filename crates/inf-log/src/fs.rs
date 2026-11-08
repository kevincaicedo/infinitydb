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

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::limits::{LISTING_BATCH_BYTES, LISTING_BATCH_NAMES, LISTING_NAME_BYTES_MAX};

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

impl TierIoMode {
    /// The mode a namespace gets without `TIER-IO-MODE`: `Direct` on Linux
    /// (ADR-0054 D5), `Buffered` on the kqueue/macOS dev tier, which has no
    /// `O_DIRECT` (D6). An explicit `Direct` there still fails at file
    /// creation (D3).
    pub const PLATFORM_DEFAULT: TierIoMode =
        if cfg!(target_os = "linux") { TierIoMode::Direct } else { TierIoMode::Buffered };
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

/// The storage-exhaustion class — ADR-0063's typed, non-fatal admission
/// path (`LogError::NoSpace`, the tier/blob `is_storage_full` legs,
/// `DISKFULL` at the client). `ENOSPC` and, since F-L04-03, `EDQUOT`: a
/// quota'd filesystem (the reference box's `/tmp usrquota`) reports the
/// same operator condition with a different errno, and
/// `ErrorKind::FilesystemQuotaExceeded` is unstable, so the raw errno is
/// the stable test. Every site classifies through here — never inline.
#[must_use]
pub fn is_storage_exhausted(e: &io::Error) -> bool {
    matches!(e.kind(), io::ErrorKind::StorageFull | io::ErrorKind::QuotaExceeded)
        || matches!(e.raw_os_error(), Some(libc::ENOSPC | libc::EDQUOT))
}

/// A read loop's cursor after `read_at` reported `n` bytes into the
/// `cap - done` bytes it was handed. The one refusal, for every reader, of
/// a file that reports more than that slice: a broken fs seam must not
/// pass unwritten buffer bytes off as read.
///
/// # Errors
/// `InvalidData` naming the over-report.
pub(crate) fn advance_read(done: usize, n: usize, cap: usize) -> io::Result<usize> {
    done.checked_add(n).filter(|&next| next <= cap).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "read_at reported {n} bytes into the {} it was handed",
                cap.saturating_sub(done)
            ),
        )
    })
}

/// A caller-owned batch of one directory's entry names (ADR-0202 D1, D2):
/// at most [`LISTING_BATCH_NAMES`] names and [`LISTING_BATCH_BYTES`] name
/// bytes, its storage allocated once by [`NameBatch::new`] at that fixed
/// capacity and reused across calls, never grown. Each name is the entry's
/// own bytes, never converted. A stream's `next_batch` clears it, then
/// fills it; the batch changes only on that call, so an index into it
/// names one name until the caller lists again.
#[derive(Debug)]
pub struct NameBatch {
    /// The names' bytes, back to back.
    bytes: Vec<u8>,
    /// Each name's end offset in `bytes`, in listing order.
    ends: Vec<u16>,
}

impl Default for NameBatch {
    fn default() -> Self {
        NameBatch::new()
    }
}

impl NameBatch {
    /// An empty batch at its fixed capacity.
    #[must_use]
    pub fn new() -> NameBatch {
        NameBatch {
            bytes: Vec::with_capacity(LISTING_BATCH_BYTES),
            ends: Vec::with_capacity(LISTING_BATCH_NAMES),
        }
    }

    /// The names the batch holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.ends.len()
    }

    /// Whether the batch holds no name.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ends.is_empty()
    }

    /// The name at `index`, in listing order; `None` at or past `len()`.
    #[must_use]
    pub fn name(&self, index: usize) -> Option<&OsStr> {
        let end = usize::from(*self.ends.get(index)?);
        let begin = match index.checked_sub(1) {
            Some(before) => usize::from(*self.ends.get(before)?),
            None => 0,
        };
        self.bytes.get(begin..end).map(OsStr::from_bytes)
    }

    /// The names in listing order.
    pub fn iter(&self) -> impl Iterator<Item = &OsStr> + '_ {
        (0..self.len()).filter_map(|index| self.name(index))
    }

    fn clear(&mut self) {
        self.bytes.clear();
        self.ends.clear();
    }

    /// Appends `name` when both bounds take it; `false` when the batch is
    /// full for it. An empty batch takes every name a listing accepts
    /// (`LISTING_NAME_BYTES_MAX ≤ LISTING_BATCH_BYTES`, asserted in
    /// `limits`), so a full batch is never empty.
    fn push(&mut self, name: &[u8]) -> bool {
        if self.ends.len() >= LISTING_BATCH_NAMES {
            return false;
        }
        let Some(end) = self.bytes.len().checked_add(name.len()) else { return false };
        let Ok(end_offset) = u16::try_from(end) else { return false };
        if end > LISTING_BATCH_BYTES {
            return false;
        }
        self.bytes.extend_from_slice(name);
        self.ends.push(end_offset);
        true
    }
}

/// How a `next_batch` call ended (ADR-0202 D1).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[must_use]
pub enum BatchEnd {
    /// The batch holds at least one name and the stream may hold more.
    More,
    /// The batch holds every name left, perhaps none; every later call
    /// answers `End` with an empty batch.
    End,
}

/// What opening a directory's listing found (ADR-0202 D1).
#[derive(Debug)]
#[must_use]
pub enum OpenedDir<S> {
    /// The directory does not exist: its own `ENOENT`.
    Absent,
    /// The directory's listing, open.
    Open(S),
}

/// An open listing of one directory: the typed cursor of a listing in
/// progress (ADR-0202 D1). It owns its directory handle, holds no borrow,
/// is neither `Clone` nor `Copy`, and closes the directory on drop, so an
/// error path releases it. Its position is the filesystem's. Within one
/// open stream a name present in the directory from the open to `End` is
/// returned exactly once; a name created, removed or renamed while it is
/// open may be returned or not, and a name removed after it was returned
/// may be returned again, so a consumer removes a listed name through
/// [`remove_listed`] and takes its `Gone` as that earlier removal (D3).
/// `.` and `..` are never returned. No consumer may depend on the order:
/// the filesystem's on [`StdSegmentFs`], ascending bytes on the in-memory
/// and simulated tiers (deterministic, L7).
pub trait DirStream {
    /// Clears `batch`, then fills it with the next names, at most the
    /// batch's two bounds; a name that does not fit waits whole for the
    /// next call.
    ///
    /// # Errors
    /// I/O failures, and `InvalidData` naming the directory for a name
    /// over [`LISTING_NAME_BYTES_MAX`] bytes: the listing refuses, never
    /// skips. After an `Err` the batch holds no name.
    fn next_batch(&mut self, batch: &mut NameBatch) -> io::Result<BatchEnd>;
}

/// What removing a listed name found (ADR-0202 D3).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[must_use]
pub enum Removal {
    /// The name was removed.
    Removed,
    /// The name was gone: its own earlier removal, which a stream may
    /// return again, never a refusal.
    Gone,
}

/// Removes a name a listing returned (ADR-0202 D3), the one home of the
/// repeat rule: within one open stream a name removed after it was
/// returned may be returned again, so `remove_file`'s `NotFound` is
/// `Gone` — that name's earlier removal — and every other error an `Err`.
/// A boot consumer of a listing removes a listed name through this alone
/// and counts `Removed` only.
///
/// # Errors
/// Every `remove_file` error but `NotFound`.
pub fn remove_listed<F: SegmentFs>(fs: &F, path: &Path) -> io::Result<Removal> {
    match fs.remove_file(path) {
        Ok(()) => Ok(Removal::Removed),
        // The planted canary refuses a listed name's second removal, as a
        // bare `remove_file` would.
        Err(err)
            if err.kind() == io::ErrorKind::NotFound
                && !cfg!(inf_canary_recover_listing_repeat_refused) =>
        {
            Ok(Removal::Gone)
        }
        Err(err) => Err(err),
    }
}

/// The one refusal of a name over the listing's bound, naming the
/// directory (ADR-0202 D2).
fn check_name_len(dir: &Path, name: &[u8]) -> io::Result<()> {
    if name.len() > LISTING_NAME_BYTES_MAX {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{}: a {}-byte name passes the listing's {LISTING_NAME_BYTES_MAX}-byte bound",
                dir.display(),
                name.len()
            ),
        ));
    }
    Ok(())
}

/// Where an in-memory tier's stream resumes (ADR-0202 D5): a key by
/// identity — the greatest name it returned — never an ordinal, so a name
/// removed behind it moves nothing and one created ahead of it is
/// returned.
#[derive(Debug)]
enum ResumeKey {
    /// No batch yet: the directory's first name.
    First,
    /// After this path, the greatest the stream returned.
    After(PathBuf),
    /// `End` was answered.
    Ended,
}

/// Fills `batch` from `names`, an in-memory tier's ascending path map, with
/// the entries of `dir` after `key`, and moves `key` to the greatest name
/// taken (ADR-0202 D5). The range starts at the key, O(log n), and stops at
/// the first path outside `dir` (the paths under one directory sort
/// together); a deeper path is passed. Bound: the names of `dir` after the
/// key, at most one batch taken.
fn fill_after<V>(
    names: &std::collections::BTreeMap<PathBuf, V>,
    dir: &Path,
    key: &mut ResumeKey,
    batch: &mut NameBatch,
) -> io::Result<BatchEnd> {
    use std::ops::Bound;
    let from = match key {
        ResumeKey::First => dir,
        ResumeKey::After(path) => path.as_path(),
        ResumeKey::Ended => return Ok(BatchEnd::End),
    };
    let mut first: Option<&PathBuf> = None;
    let mut last: Option<&PathBuf> = None;
    let mut full = false;
    for (path, _) in names.range::<Path, _>((Bound::Excluded(from), Bound::Unbounded)) {
        if !path.starts_with(dir) {
            break;
        }
        let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else { continue };
        if parent != dir {
            continue;
        }
        check_name_len(dir, name.as_bytes())?;
        if !batch.push(name.as_bytes()) {
            full = true;
            break;
        }
        first.get_or_insert(path);
        last = Some(path);
    }
    // The planted canary resumes at the batch's first name, not its
    // greatest: every later batch returns that batch's names again.
    let resume = if cfg!(inf_canary_listing_resume_rewinds) { first } else { last };
    if let Some(path) = resume {
        *key = ResumeKey::After(path.clone());
    }
    if full {
        return Ok(BatchEnd::More);
    }
    *key = ResumeKey::Ended;
    // The planted canary answers `More` with an empty batch where the walk
    // ends right after a full one.
    if cfg!(inf_canary_listing_more_empty) && batch.is_empty() && first.is_none() {
        return Ok(BatchEnd::More);
    }
    Ok(BatchEnd::End)
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
    /// `inf-server`'s crate-private `ReadAheadFs` — held only by its boot
    /// `Recovery` machine (ADR-0109), a per-file prefetch thread; this
    /// crate forbids unsafe and never blocks on the hint.
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
    /// The tier's open directory listing (ADR-0202 D1). Required, so a
    /// wrapper names its inner tier's stream and forwards
    /// [`open_listing`](Self::open_listing): a defaulted one would drop the
    /// wrapper silently.
    type DirStream: DirStream;

    /// Opens `dir`'s listing: `Absent` is the directory's own `ENOENT`.
    ///
    /// # Errors
    /// Any other errno, `ENOTDIR` included.
    fn open_listing(&self, dir: &Path) -> io::Result<OpenedDir<Self::DirStream>>;

    /// File names (not paths) in `dir`, every batch of its listing
    /// collected, in the listing's order: a whole listing, so a call names
    /// its directory's bound beside it. Non-UTF-8 names are surfaced
    /// lossily so the boot scan can reject them by name — never skipped. A
    /// missing directory is `NotFound`.
    ///
    /// # Errors
    /// `NotFound` for an absent `dir`, and the listing's own errors.
    fn list_dir(&self, dir: &Path) -> io::Result<Vec<String>> {
        let mut stream = match self.open_listing(dir)? {
            OpenedDir::Absent => {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("no dir {}", dir.display()),
                ));
            }
            OpenedDir::Open(stream) => stream,
        };
        let mut batch = NameBatch::new();
        let mut names = Vec::new();
        // Bound: the directory's names; each `More` batch holds one at least.
        loop {
            let end = stream.next_batch(&mut batch)?;
            names.extend(batch.iter().map(|name| name.to_string_lossy().into_owned()));
            match end {
                BatchEnd::More => {}
                BatchEnd::End => return Ok(names),
            }
        }
    }
    /// Create a new segment (create-new semantics: existing file is an
    /// error) preallocated to `prealloc_bytes`. `StorageFull` here is the
    /// ENOSPC discipline seam: it must surface *before* any write needs
    /// the space (M2-S02).
    fn create_segment(&self, path: &Path, prealloc_bytes: u64) -> io::Result<Self::File>;
    /// Create a tier file in `mode` (M4-S09, ADR-0054 D1). The default
    /// delegates to [`create_segment`](Self::create_segment) ignoring the
    /// mode — honest **by construction** on the in-memory tier, whose
    /// byte-visibility model has no page cache to bypass (`Buffered ≡
    /// Direct` there), never a silent fallback on a real filesystem:
    /// [`StdSegmentFs`] implements the real thing and refuses typed when
    /// `Direct` does not take effect (D3); the sim overrides it so a
    /// `Direct` file asserts the `O_DIRECT` alignment contract on every
    /// write (F-L04-14, ADR-0119 A1). Wrappers must forward this
    /// explicitly (falling into the default would drop the flag).
    fn create_tier(&self, path: &Path, mode: TierIoMode) -> io::Result<Self::File> {
        let _ = mode;
        self.create_segment(path, 0)
    }
    /// Open an **existing** tier file in `mode` (M4-S11, ADR-0056 D5 —
    /// the recovery reopen). Same honesty contract as
    /// [`create_tier`](Self::create_tier): the default delegates to
    /// [`open_write`](Self::open_write) (mode-equivalent on the in-memory
    /// tier), [`StdSegmentFs`] applies and verifies `O_DIRECT`, the sim
    /// flags the inode, and wrappers must forward explicitly.
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
    /// but it pays the barrier at create time — and on the sim it would
    /// make the prealloc length durable at creation, which `set_len`
    /// never is (F-L04-05): the sim overrides it, length pending until
    /// the caller's barrier.
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
#[allow(clippy::disallowed_types, reason = "fs-seam: injected SegmentFs real backend")]
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

/// The std tier's open listing (ADR-0202 D4): one directory fd and libc's
/// `DIR` buffer (32 KiB on glibc), held for the listing's life — the same
/// code on macOS, where std's `ReadDir` is `opendir` and `readdir`.
/// `next_batch` reads entries until the batch is full, so it costs a
/// `getdents64` only when libc's buffer is empty, and carries the one name
/// that did not fit to the next call. The handle closes at `End` or on drop.
#[derive(Debug)]
#[allow(clippy::disallowed_types, reason = "fs-seam: injected SegmentFs real backend")]
pub struct StdDirStream {
    /// The open directory; `None` once `End` was answered.
    dir: Option<std::fs::ReadDir>,
    /// The directory's path, for the overlong-name refusal.
    path: PathBuf,
    /// The name the last batch could not take, returned first next.
    carried: Option<OsString>,
}

impl DirStream for StdDirStream {
    fn next_batch(&mut self, batch: &mut NameBatch) -> io::Result<BatchEnd> {
        batch.clear();
        let filled = self.fill(batch);
        if filled.is_err() {
            batch.clear();
        }
        filled
    }
}

impl StdDirStream {
    fn fill(&mut self, batch: &mut NameBatch) -> io::Result<BatchEnd> {
        if let Some(name) = self.carried.take() {
            // Length-checked by the call that met it; an empty batch takes
            // every name the listing accepts.
            let taken = batch.push(name.as_bytes());
            debug_assert!(taken, "an empty batch takes the carried name");
        }
        let Some(dir) = self.dir.as_mut() else { return Ok(BatchEnd::End) };
        // Bound: one batch of names, then the one that did not fit.
        loop {
            let Some(entry) = dir.next() else {
                self.dir = None;
                return Ok(BatchEnd::End);
            };
            let name = entry?.file_name();
            check_name_len(&self.path, name.as_bytes())?;
            if !batch.push(name.as_bytes()) {
                // The planted canary drops the name that did not fit.
                if !cfg!(inf_canary_listing_carry_dropped) {
                    self.carried = Some(name);
                }
                return Ok(BatchEnd::More);
            }
        }
    }
}

impl SegmentFs for StdSegmentFs {
    type File = StdSegmentFile;
    type DirStream = StdDirStream;

    fn open_listing(&self, dir: &Path) -> io::Result<OpenedDir<StdDirStream>> {
        #[allow(clippy::disallowed_methods, reason = "fs-seam: injected SegmentFs real backend")]
        let listing = std::fs::read_dir(dir);
        match listing {
            Ok(read) => Ok(OpenedDir::Open(StdDirStream {
                dir: Some(read),
                path: dir.to_path_buf(),
                carried: None,
            })),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(OpenedDir::Absent),
            Err(err) => Err(err),
        }
    }

    fn create_dir_all(&self, dir: &Path) -> io::Result<()> {
        #[allow(clippy::disallowed_methods, reason = "fs-seam: injected SegmentFs real backend")]
        std::fs::create_dir_all(dir)
    }

    #[allow(clippy::disallowed_types, reason = "fs-seam: injected SegmentFs real backend")]
    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        std::fs::File::open(dir)?.sync_all()
    }

    #[allow(clippy::disallowed_types, reason = "fs-seam: injected SegmentFs real backend")]
    fn create_segment(&self, path: &Path, prealloc_bytes: u64) -> io::Result<Self::File> {
        let file =
            std::fs::OpenOptions::new().read(true).write(true).create_new(true).open(path)?;
        file.set_len(prealloc_bytes)?;
        file.sync_all()?;
        Ok(StdSegmentFile(file))
    }

    #[allow(clippy::disallowed_types, reason = "fs-seam: injected SegmentFs real backend")]
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

    #[allow(clippy::disallowed_types, reason = "fs-seam: injected SegmentFs real backend")]
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

    #[allow(clippy::disallowed_types, reason = "fs-seam: injected SegmentFs real backend")]
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

    #[allow(clippy::disallowed_types, reason = "fs-seam: injected SegmentFs real backend")]
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

    #[allow(clippy::disallowed_types, reason = "fs-seam: injected SegmentFs real backend")]
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

    #[allow(clippy::disallowed_types, reason = "fs-seam: injected SegmentFs real backend")]
    fn create_meta(&self, path: &Path) -> io::Result<Self::File> {
        let file =
            std::fs::OpenOptions::new().read(true).write(true).create_new(true).open(path)?;
        Ok(StdSegmentFile(file))
    }

    #[allow(clippy::disallowed_types, reason = "fs-seam: injected SegmentFs real backend")]
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

    #[allow(clippy::disallowed_types, reason = "fs-seam: injected SegmentFs real backend")]
    fn open_dir(&self, dir: &Path) -> io::Result<Self::File> {
        Ok(StdSegmentFile(std::fs::File::open(dir)?))
    }

    #[allow(clippy::disallowed_types, reason = "fs-seam: injected SegmentFs real backend")]
    fn open_write(&self, path: &Path) -> io::Result<Self::File> {
        Ok(StdSegmentFile(std::fs::OpenOptions::new().read(true).write(true).open(path)?))
    }

    #[allow(clippy::disallowed_types, reason = "fs-seam: injected SegmentFs real backend")]
    fn open_read(&self, path: &Path) -> io::Result<Self::File> {
        Ok(StdSegmentFile(std::fs::File::open(path)?))
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        #[allow(clippy::disallowed_methods, reason = "fs-seam: injected SegmentFs real backend")]
        std::fs::rename(from, to)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        #[allow(clippy::disallowed_methods, reason = "fs-seam: injected SegmentFs real backend")]
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
#[allow(clippy::disallowed_types, reason = "fs-seam: injected SegmentFs real backend")]
fn verify_o_direct(file: &std::fs::File, path: &Path) -> io::Result<()> {
    let fd = std::os::fd::AsRawFd::as_raw_fd(file);
    #[allow(clippy::disallowed_methods, reason = "fs-seam: injected SegmentFs real backend")]
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

    use super::{BatchEnd, DirStream, NameBatch, OpenedDir, ResumeKey, SegmentFile, SegmentFs};

    /// One file: its bytes and the preallocation the budget was debited
    /// for at create (credited back on remove / rename-clobber).
    #[derive(Debug)]
    struct Entry {
        data: Rc<RefCell<Vec<u8>>>,
        debited: u64,
    }

    #[derive(Debug, Default)]
    struct State {
        dirs: std::collections::BTreeSet<PathBuf>,
        files: BTreeMap<PathBuf, Entry>,
        /// Remaining preallocation budget; `None` = unlimited. Debited by
        /// `create_segment` — the ENOSPC injection point — and credited
        /// by `remove_file` / a clobbering `rename` (L04 style row).
        capacity: Option<u64>,
        /// The errno an over-capacity create reports (`None` = the
        /// `StorageFull` kind). `Some(libc::EDQUOT)` models a quota'd
        /// filesystem (F-L04-03).
        exhaustion_errno: Option<i32>,
        /// The errno a `create_dir_all` that would add a directory
        /// reports (`None` = mkdir succeeds): a full or over-quota device
        /// has no block for a new directory, while an existing one is no
        /// change and succeeds.
        mkdir_errno: Option<i32>,
        /// Cap on bytes one `read_at` returns (`None` = unlimited): the
        /// partial-read model every reader must loop over (F-L04-10).
        read_cap: Option<usize>,
        #[cfg(test)]
        overreport_reads: bool,
        fail_next_sync_data: bool,
        /// `read_at` calls across every file — the dependent-read oracle
        /// for the `.ick` loader (review L03, batch 34).
        reads: u64,
        /// `create_meta_direct` answers `Unsupported` — the filesystem /
        /// platform without `O_DIRECT` (ADR-0088 D3 as amended): the
        /// checkpoint's probed buffered fallback is exercised here.
        refuse_direct_meta: bool,
        /// Crash-at-step injection (M2-S11): `Some(n)` allows `n` more
        /// mutating operations, then **every** further mutating op fails —
        /// modeling a dead process whose best-effort cleanup also never
        /// runs. Cleared by `clear_op_fault` (the "reboot").
        ops_until_fault: Option<u64>,
    }

    impl State {
        /// Returns a removed file's preallocation to the budget.
        fn credit(&mut self, bytes: u64) {
            if let Some(capacity) = self.capacity.as_mut() {
                *capacity += bytes;
            }
        }

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
        /// Over-capacity creates fail with this raw errno instead of the
        /// `StorageFull` kind (`libc::EDQUOT` = a quota'd filesystem).
        pub fn set_exhaustion_errno(&self, errno: Option<i32>) {
            self.state.borrow_mut().exhaustion_errno = errno;
        }

        /// Every `create_dir_all` that would add a directory fails with
        /// this raw errno from now on (`libc::ENOSPC`, `libc::EDQUOT`);
        /// one whose directories all exist still succeeds. `None` lifts it.
        pub fn set_mkdir_errno(&self, errno: Option<i32>) {
            self.state.borrow_mut().mkdir_errno = errno;
        }

        /// Every `read_at` returns at most `cap` bytes (partial reads).
        pub fn set_read_cap(&self, cap: Option<usize>) {
            self.state.borrow_mut().read_cap = cap;
        }

        /// Plant a broken file seam: report one byte past the supplied slice.
        #[cfg(test)]
        pub(crate) fn overreport_reads(&self) {
            self.state.borrow_mut().overreport_reads = true;
        }

        pub fn set_capacity(&self, bytes: Option<u64>) {
            self.state.borrow_mut().capacity = bytes;
        }

        /// Make the next `sync_data` on any file fail with `EIO` — the
        /// fsyncgate probe (§8.4).
        pub fn fail_next_sync_data(&self) {
            self.state.borrow_mut().fail_next_sync_data = true;
        }

        /// Model a filesystem without `O_DIRECT`: every
        /// `create_meta_direct` answers `Unsupported` from now on.
        pub fn refuse_direct_meta(&self) {
            self.state.borrow_mut().refuse_direct_meta = true;
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
            self.state.borrow().files.get(path).map(|entry| entry.data.borrow().clone())
        }

        /// `read_at` calls so far across every file (test assertions).
        #[must_use]
        pub fn reads(&self) -> u64 {
            self.state.borrow().reads
        }
    }

    /// The memory tier's open listing (ADR-0202 D5): the tier's own handle
    /// (it borrows the state only inside a call), the directory's path and
    /// the greatest name returned, so each batch takes the names after it
    /// in ascending byte order.
    #[derive(Debug)]
    pub struct MemDirStream {
        state: Rc<RefCell<State>>,
        dir: PathBuf,
        key: ResumeKey,
    }

    impl DirStream for MemDirStream {
        fn next_batch(&mut self, batch: &mut NameBatch) -> io::Result<BatchEnd> {
            batch.clear();
            if inf_foundation::fault::fire(crate::fault::DIR_LISTING_NEXT_FAIL) {
                return Err(crate::fault::injected(crate::fault::DIR_LISTING_NEXT_FAIL));
            }
            let state = self.state.borrow();
            let filled = super::fill_after(&state.files, &self.dir, &mut self.key, batch);
            if filled.is_err() {
                batch.clear();
            }
            filled
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
            let read_cap = {
                let mut fs = self.fs.borrow_mut();
                fs.reads += 1;
                fs.read_cap
            };
            let bytes = self.data.borrow();
            let offset = usize::try_from(offset).expect("offset fits usize");
            if offset >= bytes.len() {
                return Ok(0);
            }
            let n = buf.len().min(bytes.len() - offset).min(read_cap.unwrap_or(usize::MAX));
            buf[..n].copy_from_slice(&bytes[offset..offset + n]);
            #[cfg(test)]
            if self.fs.borrow().overreport_reads {
                return Ok(buf.len() + 1);
            }
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
        type DirStream = MemDirStream;

        fn open_listing(&self, dir: &Path) -> io::Result<OpenedDir<MemDirStream>> {
            if inf_foundation::fault::fire(crate::fault::DIR_LISTING_OPEN_FAIL) {
                return Err(crate::fault::injected(crate::fault::DIR_LISTING_OPEN_FAIL));
            }
            if !self.state.borrow().dirs.contains(dir) {
                return Ok(OpenedDir::Absent);
            }
            Ok(OpenedDir::Open(MemDirStream {
                state: Rc::clone(&self.state),
                dir: dir.to_path_buf(),
                key: ResumeKey::First,
            }))
        }

        fn create_dir_all(&self, dir: &Path) -> io::Result<()> {
            let mut state = self.state.borrow_mut();
            // Charged like the sim tier's: a crash-at-step index means the
            // same op on both (L04 style row).
            state.tick_op()?;
            if let Some(errno) = state.mkdir_errno
                && !state.dirs.contains(dir)
            {
                return Err(io::Error::from_raw_os_error(errno));
            }
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
                    return Err(match state.exhaustion_errno {
                        Some(errno) => io::Error::from_raw_os_error(errno),
                        None => io::Error::new(io::ErrorKind::StorageFull, "injected ENOSPC"),
                    });
                }
                *capacity -= prealloc_bytes;
            }
            let data = Rc::new(RefCell::new(vec![
                0;
                usize::try_from(prealloc_bytes)
                    .expect("prealloc fits usize")
            ]));
            let debited = if state.capacity.is_some() { prealloc_bytes } else { 0 };
            state.files.insert(path.to_path_buf(), Entry { data: Rc::clone(&data), debited });
            Ok(MemFile { data, fs: Rc::clone(&self.state) })
        }

        fn create_meta(&self, path: &Path) -> io::Result<Self::File> {
            // Same create-new semantics, zero length, no capacity debit
            // (staging envelopes, not preallocated segments).
            self.create_segment(path, 0)
        }

        fn create_meta_direct(&self, path: &Path) -> io::Result<Self::File> {
            if self.state.borrow().refuse_direct_meta {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "MemFs: O_DIRECT refused (refuse_direct_meta)",
                ));
            }
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
            let entry = state.files.get(path).ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, format!("no file {}", path.display()))
            })?;
            Ok(MemFile { data: Rc::clone(&entry.data), fs: Rc::clone(&self.state) })
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
            let entry = state.files.remove(from).ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, format!("no file {}", from.display()))
            })?;
            // Replaces an existing destination atomically, like POSIX
            // rename; the clobbered file's bytes return to the budget.
            if let Some(old) = state.files.insert(to.to_path_buf(), entry) {
                state.credit(old.debited);
            }
            Ok(())
        }

        fn remove_file(&self, path: &Path) -> io::Result<()> {
            let mut state = self.state.borrow_mut();
            state.tick_op()?;
            let Some(entry) = state.files.remove(path) else {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("no file {}", path.display()),
                ));
            };
            state.credit(entry.debited);
            Ok(())
        }
    }
}

#[cfg(test)]
mod advance_read_tests {
    use super::advance_read;

    /// The cursor advances up to the slice's end and refuses one past it,
    /// and at the integer maximum — never a wrapped or clamped cursor.
    #[test]
    fn a_read_past_the_slice_is_refused() {
        assert_eq!(advance_read(0, 0, 0).expect("empty"), 0);
        assert_eq!(advance_read(0, 8, 8).expect("exactly the slice"), 8);
        assert_eq!(advance_read(3, 4, 8).expect("inside"), 7);
        assert!(advance_read(0, 9, 8).is_err(), "one past the slice");
        assert!(advance_read(8, 1, 8).is_err(), "a byte into a full buffer");
        assert!(advance_read(usize::MAX, 1, usize::MAX).is_err(), "the sum overflows");
    }
}
