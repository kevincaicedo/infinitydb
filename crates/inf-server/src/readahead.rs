//! Boot-tier filesystem wrapper that gives `SegmentFile::advise_read_ahead`
//! a real implementation: a boot-scoped prefetch thread per recovery-read
//! file (M2.5-S08).
//!
//! `inf-log`'s readers hint the next window after every refill; the
//! prefetcher preads it into the page cache on its own thread, so cold
//! replay's device read overlaps apply instead of composing serially with
//! it. `posix_fadvise(WILLNEED)` was measured first and **rejected**: on
//! this kernel/device class it *doubled* per-window read latency (it
//! defeats the kernel's own sequential-readahead ramp), while the thread
//! fully overlaps — see the S08 A/B artifact.
//!
//! Discipline: the thread is boot-scoped (spawned per `open_read`, joined
//! when the reader drops — recovery replay is the §3.3 sanctioned blocking
//! exception this rides). It communicates by atomics + park/unpark only,
//! touches no cell state, and populates the page cache without changing a
//! single byte the reader sees — L7 determinism and every digest hold; the
//! DST tier never constructs this wrapper.
//!
//! Choosing the fs IS the A/B switch: `ReadAheadFs::new(StdSegmentFs,
//! true)` is the lever-on arm, bare `StdSegmentFs` the lever-off arm.

use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use inf_log::fs::{SegmentFile, SegmentFs, SegmentIoMode, TierIoMode};

/// Prefetch granularity: matches the reader's default window so one
/// advise = one pread.
const PREFETCH_CHUNK: usize = 1 << 20;

/// The prefetch worker: owns a second read handle to the same file and
/// preads `[done, target)` into the page cache, then discards the bytes.
/// Best-effort by construction — any worker error just ends prefetching;
/// the reader's own reads surface real failures.
struct Prefetcher {
    target: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Prefetcher {
    fn spawn(file: std::fs::File) -> io::Result<Prefetcher> {
        let target = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let t = Arc::clone(&target);
        let s = Arc::clone(&stop);
        // L17-02 (review 2026-08-30) records that boot scoping is unenforced past recovery.
        // denylist-allow: boot-scoped prefetch thread (M2.5-S08, the §3.3 recovery exception).
        let worker = std::thread::Builder::new().name("inf-readahead".into()).spawn(move || {
            use std::os::unix::fs::FileExt;
            // The spawning cell thread is core-pinned and this thread
            // inherits that mask — unpin or the prefetch timeshares with
            // the apply loop it exists to overlap.
            inf_runtime::unpin_current_thread();
            let mut buf = vec![0u8; PREFETCH_CHUNK];
            let mut done: u64 = 0;
            let mut ended = false;
            while !s.load(Ordering::Acquire) {
                let want = t.load(Ordering::Acquire);
                if ended || done >= want {
                    // Bounded idle: a missed unpark degrades to this poll,
                    // never a hang; park's token makes the race benign.
                    // denylist-allow: the prefetch thread parks itself, not a cell.
                    std::thread::park_timeout(Duration::from_micros(500));
                    continue;
                }
                let len =
                    usize::try_from(want - done).unwrap_or(PREFETCH_CHUNK).min(PREFETCH_CHUNK);
                match file.read_at(&mut buf[..len], done) {
                    Ok(0) | Err(_) => ended = true,
                    Ok(n) => done += n as u64,
                }
            }
        })?;
        Ok(Prefetcher { target, stop, worker: Some(worker) })
    }

    fn advise(&self, end: u64) {
        let prev = self.target.fetch_max(end, Ordering::AcqRel);
        if end > prev
            && let Some(worker) = &self.worker
        {
            worker.thread().unpark();
        }
    }
}

impl Drop for Prefetcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker.thread().unpark();
            let _ = worker.join();
        }
    }
}

/// [`SegmentFs`] wrapper whose `open_read` files prefetch advised windows
/// on a boot-scoped thread (all other operations delegate 1:1).
///
/// `prefetch` gates the worker per the S08 A/B's regime split: a single
/// recovering cell gains +32–37 % cold replay, but N cells recovering in
/// parallel already saturate the device with N reader streams — N more
/// prefetch streams cost sequential locality (8-cell cold boot measured
/// 7.3 s → 10.0 s). The assembly passes `cells == 1`; re-evaluate the
/// multi-cell arm on the Gen4 box (S18).
#[derive(Copy, Clone, Debug, Default)]
pub struct ReadAheadFs<F> {
    inner: F,
    prefetch: bool,
}

impl<F> ReadAheadFs<F> {
    /// Wrap `inner`; `prefetch = false` is delegation-only (off-arm).
    pub fn new(inner: F, prefetch: bool) -> ReadAheadFs<F> {
        ReadAheadFs { inner, prefetch }
    }
}

/// A file of [`ReadAheadFs`]: read-path opens carry a prefetcher; every
/// operation delegates to the wrapped tier.
#[derive(Debug)]
pub struct ReadAheadFile<File> {
    inner: File,
    prefetcher: Option<Prefetcher>,
}

impl<File> ReadAheadFile<File> {
    fn plain(inner: File) -> ReadAheadFile<File> {
        ReadAheadFile { inner, prefetcher: None }
    }
}

impl std::fmt::Debug for Prefetcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Prefetcher").field("target", &self.target.load(Ordering::Relaxed)).finish()
    }
}

impl<File: SegmentFile> SegmentFile for ReadAheadFile<File> {
    fn write_at(&mut self, offset: u64, data: &[u8]) -> io::Result<()> {
        self.inner.write_at(offset, data)
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read_at(offset, buf)
    }

    fn file_size(&self) -> io::Result<u64> {
        self.inner.file_size()
    }

    fn truncate(&mut self, len: u64) -> io::Result<()> {
        self.inner.truncate(len)
    }

    fn sync_data(&mut self) -> io::Result<()> {
        self.inner.sync_data()
    }

    fn raw_fd(&self) -> Option<std::os::fd::RawFd> {
        self.inner.raw_fd()
    }

    fn fully_allocated(&self) -> io::Result<bool> {
        self.inner.fully_allocated()
    }

    fn advise_read_ahead(&self, offset: u64, len: u64) {
        if let Some(prefetcher) = &self.prefetcher {
            prefetcher.advise(offset.saturating_add(len));
        }
    }
}

impl<F: SegmentFs> SegmentFs for ReadAheadFs<F> {
    type File = ReadAheadFile<F::File>;

    fn create_dir_all(&self, dir: &Path) -> io::Result<()> {
        self.inner.create_dir_all(dir)
    }

    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        self.inner.sync_dir(dir)
    }

    fn list_dir(&self, dir: &Path) -> io::Result<Vec<String>> {
        self.inner.list_dir(dir)
    }

    fn create_segment(&self, path: &Path, prealloc_bytes: u64) -> io::Result<Self::File> {
        self.inner.create_segment(path, prealloc_bytes).map(ReadAheadFile::plain)
    }

    fn create_segment_unsynced(&self, path: &Path, prealloc_bytes: u64) -> io::Result<Self::File> {
        self.inner.create_segment_unsynced(path, prealloc_bytes).map(ReadAheadFile::plain)
    }

    fn create_segment_direct(&self, path: &Path, prealloc_bytes: u64) -> io::Result<Self::File> {
        // Explicit forward (ADR-0086 D4): the trait default would delegate
        // to this *wrapper's* create_segment and silently drop the mode.
        self.inner.create_segment_direct(path, prealloc_bytes).map(ReadAheadFile::plain)
    }

    fn open_segment_append(&self, path: &Path, mode: SegmentIoMode) -> io::Result<Self::File> {
        // Explicit forward (ADR-0086 D4), same reason.
        self.inner.open_segment_append(path, mode).map(ReadAheadFile::plain)
    }

    fn create_tier(&self, path: &Path, mode: TierIoMode) -> io::Result<Self::File> {
        // Explicit forward (ADR-0054 D1): the trait default would delegate
        // to this *wrapper's* create_segment and silently drop the mode.
        self.inner.create_tier(path, mode).map(ReadAheadFile::plain)
    }

    fn open_tier(&self, path: &Path, mode: TierIoMode) -> io::Result<Self::File> {
        // Explicit forward (ADR-0056 D5): the trait default would delegate
        // to this *wrapper's* open_write and silently drop the mode.
        self.inner.open_tier(path, mode).map(ReadAheadFile::plain)
    }

    fn create_meta(&self, path: &Path) -> io::Result<Self::File> {
        self.inner.create_meta(path).map(ReadAheadFile::plain)
    }

    fn create_meta_direct(&self, path: &Path) -> io::Result<Self::File> {
        // Explicit forward (ADR-0088 D3): the mode must not be dropped.
        self.inner.create_meta_direct(path).map(ReadAheadFile::plain)
    }

    fn open_dir(&self, dir: &Path) -> io::Result<Self::File> {
        self.inner.open_dir(dir).map(ReadAheadFile::plain)
    }

    fn open_write(&self, path: &Path) -> io::Result<Self::File> {
        self.inner.open_write(path).map(ReadAheadFile::plain)
    }

    fn open_read(&self, path: &Path) -> io::Result<Self::File> {
        let inner = self.inner.open_read(path)?;
        // Second handle by path, best-effort: absent (e.g. a non-std tier
        // under the wrapper) the file just reads serially.
        let prefetcher = if self.prefetch {
            std::fs::File::open(path).ok().and_then(|file| Prefetcher::spawn(file).ok())
        } else {
            None
        };
        Ok(ReadAheadFile { inner, prefetcher })
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.inner.rename(from, to)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.inner.remove_file(path)
    }
}
