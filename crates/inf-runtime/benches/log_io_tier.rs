//! M2-S07 log I/O tier A/B (dev tier): buffered+fdatasync vs O_DIRECT for
//! the grouped `always` write path, measured at the driver+device layer —
//! the same workload shape as `log_fsync.rs` so rows are comparable.
//!
//! Policies:
//!   buffered      buffered write + linked fdatasync (the shipping default)
//!   direct        O_DIRECT write (frames padded to the 4 KiB block) +
//!                 linked fdatasync (O_DIRECT bypasses the page cache, NOT
//!                 the device write cache — the flush is still the barrier)
//!   direct-dsync  O_DIRECT|O_DSYNC write, no separate fdatasync (FUA-class
//!                 barrier-per-write, informational row)
//!
//! Phases:
//!   groups        grouped-write rows per policy (group × fsync-rate model)
//!                 + page-cache footprint delta per policy (indicative)
//!   interference  grouped writer (group 256) racing a sequential reader on
//!                 a second file (fadvise-DONTNEED-cooled) — the
//!                 checkpoint/recovery page-cache interference row
//!
//! Run: `cargo bench -p inf-runtime --features uring --bench log_io_tier`
//! Env:  `INF_BENCH_DIR` (default `target/`), `INF_BENCH_SECS` per row
//!       (default 5), `INF_BENCH_PHASE` (`all` | `groups` | `interference`).

use std::alloc::Layout;
use std::fs::OpenOptions;
use std::os::fd::IntoRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use inf_alloc::BufferPool;
use inf_foundation::LogHistogram;
use inf_runtime::{
    BackendDriver, CompletionResult, CompletionToken, IoOp, RawFd, StableBytes, TokenClass,
    UringDriver, Wait,
};

const FILE_BYTES: u64 = 1 << 30;
const RECORD_BYTES: usize = 70; // ~SET key/value post-image + framing share
const BLOCK: usize = 4096; // O_DIRECT alignment quantum (safe ≥ logical block)
const GROUPS: [usize; 4] = [1, 64, 256, 1024];

#[derive(Copy, Clone, PartialEq)]
enum Policy {
    Buffered,
    Direct,
    DirectDsync,
}

impl Policy {
    fn name(self) -> &'static str {
        match self {
            Policy::Buffered => "buffered",
            Policy::Direct => "direct",
            Policy::DirectDsync => "direct-dsync",
        }
    }

    fn open_flags(self) -> i32 {
        match self {
            Policy::Buffered => 0,
            Policy::Direct => libc::O_DIRECT,
            Policy::DirectDsync => libc::O_DIRECT | libc::O_DSYNC,
        }
    }

    /// The write itself is the durability barrier (no linked fdatasync).
    fn write_is_barrier(self) -> bool {
        self == Policy::DirectDsync
    }

    /// Device-visible length for a payload of `len` bytes.
    fn device_len(self, len: usize) -> usize {
        match self {
            Policy::Buffered => len,
            Policy::Direct | Policy::DirectDsync => len.div_ceil(BLOCK) * BLOCK,
        }
    }
}

/// Block-aligned heap buffer — O_DIRECT requires aligned address + length.
struct AlignedBuf {
    ptr: NonNull<u8>,
    len: usize,
    layout: Layout,
}

impl AlignedBuf {
    fn zeroed(len: usize, align: usize) -> AlignedBuf {
        let layout = Layout::from_size_align(len, align).expect("layout");
        // SAFETY: layout has non-zero size (len ≥ 1 record).
        let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
        let ptr = NonNull::new(ptr).expect("aligned alloc");
        AlignedBuf { ptr, len, layout }
    }

    fn as_slice(&self) -> &[u8] {
        // SAFETY: ptr/len describe the live allocation above.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }

    fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: ptr/len describe the live allocation above; &mut self.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        // SAFETY: allocated with this exact layout in `zeroed`.
        unsafe { std::alloc::dealloc(self.ptr.as_ptr(), self.layout) }
    }
}

fn open_log(path: &Path, policy: Policy) -> RawFd {
    let fd = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .custom_flags(policy.open_flags())
        .open(path)
        .expect("bench file")
        .into_raw_fd();
    // Real block reservation — segment-like conditions for both policies.
    // SAFETY: plain fallocate on the fd we just opened.
    let rc = unsafe { libc::fallocate(fd, 0, 0, FILE_BYTES as i64) };
    assert_eq!(rc, 0, "fallocate: {}", std::io::Error::last_os_error());
    fd
}

fn meminfo_mib(key: &str) -> u64 {
    let text = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    text.lines()
        .find_map(|l| l.strip_prefix(key))
        .and_then(|rest| rest.trim_start_matches(':').split_whitespace().next())
        .and_then(|kb| kb.parse::<u64>().ok())
        .unwrap_or(0)
        / 1024
}

fn cached_plus_dirty_mib() -> u64 {
    meminfo_mib("Cached") + meminfo_mib("Dirty")
}

/// One frame commit: LogWrite (+ linked fdatasync unless the write is the
/// barrier). Returns once every expected completion is reaped.
fn commit_frame(
    driver: &mut UringDriver,
    pool: &mut BufferPool,
    out: &mut Vec<inf_runtime::Completion>,
    fd: RawFd,
    offset: u64,
    frame: &[u8],
    policy: Policy,
) {
    // SAFETY: `frame` is live and unmodified until both completions below
    // are reaped in this same call (one frame in flight — lease discipline).
    let data = unsafe { StableBytes::new(frame) };
    let fsync_token =
        (!policy.write_is_barrier()).then(|| CompletionToken::new(TokenClass::Fsync, 1, 0));
    let expected = if policy.write_is_barrier() { 1 } else { 2 };
    driver.push(IoOp::LogWrite {
        fd,
        offset,
        data,
        token: CompletionToken::new(TokenClass::LogWrite, 1, 0),
        fsync_token,
    });
    let mut got = 0;
    while got < expected {
        out.clear();
        let n = driver
            .submit_and_reap(pool, Wait::Park { timeout: Some(Duration::from_millis(100)) }, out)
            .expect("submit");
        for c in &out[..n] {
            match c.result {
                CompletionResult::LogWritten | CompletionResult::Synced => got += 1,
                CompletionResult::Error { errno, .. } => {
                    panic!("I/O error errno {errno} at offset {offset} ({})", policy.name())
                }
                _ => unreachable!("no other ops in flight"),
            }
        }
    }
}

fn grouped_rows(dir: &Path, policy: Policy, secs: u64) {
    let path = dir.join(format!("log-io-tier-{}.seg", policy.name()));
    let fd = open_log(&path, policy);
    let mut driver = UringDriver::new(64).expect("io_uring");
    let mut pool = BufferPool::new(4, 1024);
    let mut out = Vec::new();
    let cache_before = cached_plus_dirty_mib();
    println!("## policy: {}", policy.name());
    println!(
        "{:>6} {:>10} {:>10} {:>12} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9}",
        "group",
        "frames/s",
        "barrier/s",
        "writes/s",
        "payMiB/s",
        "devMiB/s",
        "p50us",
        "p99us",
        "p999us",
        "maxus"
    );
    for group in GROUPS {
        let payload = group * RECORD_BYTES;
        let device_len = policy.device_len(payload);
        let mut frame = AlignedBuf::zeroed(device_len, BLOCK);
        frame.as_mut_slice()[..payload].fill(0xA5);
        let mut hist = LogHistogram::new();
        let mut offset = 0u64;
        let mut frames = 0u64;
        let started = Instant::now();
        let deadline = started + Duration::from_secs(secs);
        while Instant::now() < deadline {
            if offset + device_len as u64 > FILE_BYTES {
                offset = 0; // wrap: the file is one reusable segment
            }
            let t0 = Instant::now();
            commit_frame(&mut driver, &mut pool, &mut out, fd, offset, frame.as_slice(), policy);
            hist.record(t0.elapsed().as_micros() as u64);
            offset += device_len as u64;
            frames += 1;
        }
        let elapsed = started.elapsed().as_secs_f64();
        let fps = frames as f64 / elapsed;
        println!(
            "{:>6} {:>10.0} {:>10.0} {:>12.0} {:>9.1} {:>9.1} {:>9} {:>9} {:>9} {:>9}",
            group,
            fps,
            fps,
            fps * group as f64,
            fps * payload as f64 / (1 << 20) as f64,
            fps * device_len as f64 / (1 << 20) as f64,
            hist.percentile(50.0),
            hist.percentile(99.0),
            hist.percentile(99.9),
            hist.max(),
        );
    }
    let cache_after = cached_plus_dirty_mib();
    println!(
        "# page-cache footprint (Cached+Dirty delta, indicative): {:+} MiB",
        cache_after as i64 - cache_before as i64
    );
    // SAFETY: fd from into_raw_fd above; closed exactly once.
    unsafe { libc::close(fd) };
    std::fs::remove_file(&path).ok();
}

/// Sequential reader over `path`, cooled with fadvise(DONTNEED) per pass so
/// it stays device-bound — the checkpoint/recovery read stand-in. Runs
/// until `stop` is raised or `deadline` passes (whichever comes first).
fn reader_loop(path: &Path, stop: &AtomicBool, deadline: Option<Instant>) -> (u64, f64) {
    let fd = OpenOptions::new().read(true).open(path).expect("read file").into_raw_fd();
    let mut buf = AlignedBuf::zeroed(1 << 20, BLOCK);
    // SAFETY: fadvise on the fd we just opened; len 0 = whole file.
    unsafe { libc::posix_fadvise(fd, 0, 0, libc::POSIX_FADV_DONTNEED) };
    let started = Instant::now();
    let mut total = 0u64;
    let mut offset = 0i64;
    while !stop.load(Ordering::Relaxed) && deadline.is_none_or(|d| Instant::now() < d) {
        // SAFETY: buf is a live 1 MiB allocation; pread bounds-checked by len.
        let n = unsafe { libc::pread(fd, buf.as_mut_slice().as_mut_ptr().cast(), buf.len, offset) };
        assert!(n >= 0, "pread: {}", std::io::Error::last_os_error());
        if n == 0 || offset as u64 >= FILE_BYTES {
            offset = 0;
            // SAFETY: as above — evict the pass so the next one is cold.
            unsafe { libc::posix_fadvise(fd, 0, 0, libc::POSIX_FADV_DONTNEED) };
            continue;
        }
        total += n as u64;
        offset += n as i64;
    }
    let elapsed = started.elapsed().as_secs_f64();
    // SAFETY: fd from into_raw_fd above; closed exactly once.
    unsafe { libc::close(fd) };
    (total, elapsed)
}

fn build_read_file(path: &Path) {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .expect("read file");
    let fd = file.into_raw_fd();
    // SAFETY: plain fallocate on the fd we just opened.
    let rc = unsafe { libc::fallocate(fd, 0, 0, FILE_BYTES as i64) };
    assert_eq!(rc, 0, "fallocate: {}", std::io::Error::last_os_error());
    // Write real data so reads hit media, then drop it from the cache.
    let chunk = vec![0x5Au8; 1 << 20];
    let mut offset = 0i64;
    while (offset as u64) < FILE_BYTES {
        // SAFETY: chunk is live; pwrite bounds-checked by len.
        let n = unsafe { libc::pwrite(fd, chunk.as_ptr().cast(), chunk.len(), offset) };
        assert!(n > 0, "pwrite: {}", std::io::Error::last_os_error());
        offset += n as i64;
    }
    // SAFETY: fdatasync + fadvise on the fd we own.
    unsafe {
        assert_eq!(libc::fdatasync(fd), 0);
        libc::posix_fadvise(fd, 0, 0, libc::POSIX_FADV_DONTNEED);
        libc::close(fd);
    }
}

fn interference(dir: &Path, secs: u64) {
    const GROUP: usize = 256;
    let read_path = dir.join("log-io-tier-read.dat");
    build_read_file(&read_path);
    println!("## phase: interference — grouped writer (group {GROUP}) vs sequential reader");
    println!(
        "{:>14} {:>12} {:>9} {:>9} {:>9} {:>12}",
        "policy", "writes/s", "p50us", "p99us", "p999us", "readMiB/s"
    );

    // Reader-only baseline.
    {
        let stop = AtomicBool::new(false);
        let deadline = Instant::now() + Duration::from_secs(secs);
        let (bytes, elapsed) = reader_loop(&read_path, &stop, Some(deadline));
        println!(
            "{:>14} {:>12} {:>9} {:>9} {:>9} {:>12.0}",
            "(reader-only)",
            "-",
            "-",
            "-",
            "-",
            bytes as f64 / elapsed / (1 << 20) as f64
        );
    }

    for policy in [Policy::Buffered, Policy::Direct] {
        let path = dir.join(format!("log-io-tier-ix-{}.seg", policy.name()));
        let fd = open_log(&path, policy);
        let mut driver = UringDriver::new(64).expect("io_uring");
        let mut pool = BufferPool::new(4, 1024);
        let mut out = Vec::new();
        let payload = GROUP * RECORD_BYTES;
        let device_len = policy.device_len(payload);
        let mut frame = AlignedBuf::zeroed(device_len, BLOCK);
        frame.as_mut_slice()[..payload].fill(0xA5);

        let stop = AtomicBool::new(false);
        let (hist, frames, elapsed, bytes, relapsed) = std::thread::scope(|s| {
            let h = s.spawn(|| reader_loop(&read_path, &stop, None));
            let mut hist = LogHistogram::new();
            let mut offset = 0u64;
            let mut frames = 0u64;
            let started = Instant::now();
            let deadline = started + Duration::from_secs(secs);
            while Instant::now() < deadline {
                if offset + device_len as u64 > FILE_BYTES {
                    offset = 0;
                }
                let t0 = Instant::now();
                commit_frame(
                    &mut driver,
                    &mut pool,
                    &mut out,
                    fd,
                    offset,
                    frame.as_slice(),
                    policy,
                );
                hist.record(t0.elapsed().as_micros() as u64);
                offset += device_len as u64;
                frames += 1;
            }
            let elapsed = started.elapsed().as_secs_f64();
            stop.store(true, Ordering::Relaxed);
            let (bytes, relapsed) = h.join().expect("reader");
            (hist, frames, elapsed, bytes, relapsed)
        });
        let fps = frames as f64 / elapsed;
        println!(
            "{:>14} {:>12.0} {:>9} {:>9} {:>9} {:>12.0}",
            policy.name(),
            fps * GROUP as f64,
            hist.percentile(50.0),
            hist.percentile(99.0),
            hist.percentile(99.9),
            bytes as f64 / relapsed / (1 << 20) as f64
        );
        // SAFETY: fd from into_raw_fd above; closed exactly once.
        unsafe { libc::close(fd) };
        std::fs::remove_file(&path).ok();
    }
    std::fs::remove_file(&read_path).ok();
}

fn main() {
    let dir = std::env::var_os("INF_BENCH_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target"));
    let secs: u64 = std::env::var("INF_BENCH_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(5);
    let phase = std::env::var("INF_BENCH_PHASE").unwrap_or_else(|_| "all".into());

    println!("# log_io_tier — M2-S07 buffered vs O_DIRECT A/B (uring, dev tier)");
    println!(
        "# dir: {} ({} GiB fallocated files), {} s/row, record = {} B, block = {} B",
        dir.display(),
        FILE_BYTES >> 30,
        secs,
        RECORD_BYTES,
        BLOCK
    );

    if phase == "all" || phase == "groups" {
        for policy in [Policy::Buffered, Policy::Direct, Policy::DirectDsync] {
            grouped_rows(&dir, policy, secs);
        }
    }
    if phase == "all" || phase == "interference" {
        interference(&dir, secs);
    }
}
