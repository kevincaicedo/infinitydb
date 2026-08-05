//! M4-S09 tier-file I/O mode A/B (dev tier): `TierIoMode::Direct` vs
//! `Buffered` on the real cold-read machinery — `UringDriver` +
//! registered `AlignedPool` + `ColdReads` (merge off: this row isolates
//! the mode, S10's bench owns shaping) against per-mode tier files
//! written by the real `TierWriter`.
//!
//! Rows (ADR-0054 D5 — the decision axis is the tails on the
//! cold-dominated row joined with the memory-honesty rows; means are
//! informational):
//!   cold-uniform   uniform random record reads over the whole corpus,
//!                  page cache dropped (fadvise DONTNEED) before every
//!                  leg — the loaded cold-read shape the gate measures.
//!   re-reference   uniform reads over a RAM-fitting hot slice — the
//!                  page-cache-friendly case buffered mode is expected
//!                  to win on means, priced by the honesty rows.
//!
//! Memory honesty per leg (ADR-0054 D4 — a buffered row without these
//! is invalid): VmHWM/VmRSS, cgroup-v2 `memory.stat` `file` delta
//! (session scope — shared with the shell, disclosed), and exact
//! per-file page-cache residency via `mincore` over the tier file.
//!
//! Run:  `INF_TIER_AB_DIR=/real/device/dir taskset -c 4 cargo bench -p \
//!        inf-runtime --features uring --bench tier_io_mode`
//! Env:  `INF_TIER_AB_DIR` (required; must NOT be tmpfs — the A/B is
//!       meaningless without a device), `INF_TIER_AB_RECORDS` (default
//!       1<<20 ≈ 1 GiB payload), `INF_TIER_AB_READS` (default 20000),
//!       `INF_TIER_AB_REPS` (default 3, ABBA).

#![cfg(all(target_os = "linux", feature = "uring"))]

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Instant;

use inf_alloc::{AlignedPool, BufferPool};
use inf_log::fs::StdSegmentFs;
use inf_log::{
    TIER_FRAME_BYTES, TierIoMode, TierWriter, tier_extract, tier_frame_offset, tier_frame_span,
};
use inf_runtime::{
    BackendDriver, ColdReadConfig, ColdReads, ColdWait, RawFd, ReadClass, TierFileId, TokenClass,
    UringDriver, Wait,
};
use inf_store::NsId;

const NS: NsId = NsId(90);
const VALUE_BYTES: usize = 1024;
/// Window: a 1 KiB record at any delta spans ≤ 2 frames.
const WINDOW_FRAMES: usize = 2;
const POOL_BUFFERS: usize = 32;
const QD: usize = 16;
/// Hot slice for the re-reference row (records) — 8 MiB of payload.
const HOT_RECORDS: u64 = 8192;

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

// ---- measurement plumbing --------------------------------------------------

fn rusage_cpu_us() -> u64 {
    // SAFETY: rusage is a plain-old-data struct; all-zero is a valid value.
    let mut usage: libc::rusage = unsafe { core::mem::zeroed() };
    // SAFETY: getrusage writes the struct it is handed; SELF is valid.
    let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    assert_eq!(rc, 0, "getrusage");
    let tv = |t: libc::timeval| t.tv_sec as u64 * 1_000_000 + t.tv_usec as u64;
    tv(usage.ru_utime) + tv(usage.ru_stime)
}

fn proc_status_kib(field: &str) -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").expect("proc status");
    status
        .lines()
        .find_map(|line| line.strip_prefix(field))
        .and_then(|rest| rest.trim().trim_end_matches(" kB").trim().parse().ok())
        .unwrap_or(0)
}

/// cgroup-v2 `memory.stat` `file` bytes for our own cgroup (the session
/// scope — shared with the invoking shell; deltas disclosed as such).
fn cgroup_file_bytes() -> Option<u64> {
    let cgroup = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let path = cgroup.lines().find_map(|line| line.strip_prefix("0::"))?.trim();
    let stat = std::fs::read_to_string(format!("/sys/fs/cgroup{path}/memory.stat")).ok()?;
    stat.lines().find_map(|line| line.strip_prefix("file "))?.trim().parse().ok()
}

/// Exact page-cache residency of `path` via mincore — the attribution
/// row that makes the buffered leg's hidden footprint visible.
fn resident_file_bytes(path: &Path) -> u64 {
    let file = std::fs::File::open(path).expect("open for mincore");
    let len = file.metadata().expect("metadata").len() as usize;
    if len == 0 {
        return 0;
    }
    let fd = std::os::fd::AsRawFd::as_raw_fd(&file);
    // SAFETY: mapping a readable file PROT_READ/MAP_SHARED; length is the
    // current file size; the mapping is unmapped below before the fd drops.
    let map =
        unsafe { libc::mmap(core::ptr::null_mut(), len, libc::PROT_READ, libc::MAP_SHARED, fd, 0) };
    assert_ne!(map, libc::MAP_FAILED, "mmap for mincore");
    let pages = len.div_ceil(4096);
    let mut vec = vec![0u8; pages];
    // SAFETY: map/len describe the live mapping; vec holds one byte per page.
    let rc = unsafe { libc::mincore(map, len, vec.as_mut_ptr()) };
    assert_eq!(rc, 0, "mincore");
    // SAFETY: unmapping the mapping created above.
    unsafe { libc::munmap(map, len) };
    let resident_pages = vec.iter().filter(|&&b| b & 1 == 1).count() as u64;
    resident_pages * 4096
}

fn drop_page_cache(path: &Path) {
    let file = std::fs::File::open(path).expect("open for fadvise");
    let fd = std::os::fd::AsRawFd::as_raw_fd(&file);
    // SAFETY: fadvise on a live fd; DONTNEED is advisory.
    let rc = unsafe { libc::posix_fadvise(fd, 0, 0, libc::POSIX_FADV_DONTNEED) };
    assert_eq!(rc, 0, "fadvise DONTNEED");
}

fn governor() -> String {
    std::fs::read_to_string("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "unknown".into())
}

fn fs_of(path: &Path) -> String {
    let cpath = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).expect("path");
    // SAFETY: statfs is a plain-old-data struct; all-zero is a valid value.
    let mut sfs: libc::statfs = unsafe { core::mem::zeroed() };
    // SAFETY: statfs writes the struct it is handed; the path is a live CString.
    let rc = unsafe { libc::statfs(cpath.as_ptr(), &mut sfs) };
    assert_eq!(rc, 0, "statfs");
    match sfs.f_type {
        0x0102_1994 => "tmpfs".into(),
        0xEF53 => "ext4".into(),
        0x9123_683E => "btrfs".into(),
        0x5846_5342 => "xfs".into(),
        other => format!("fstype {other:#x}"),
    }
}

struct SplitMix64(u64);

impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

// ---- corpus ----------------------------------------------------------------

/// Writes `records` fixed-size records (8-byte index marker + filler)
/// through the real writer in `mode`; returns the writer (fd owner).
fn build_corpus(dir: &Path, mode: TierIoMode, records: u64) -> TierWriter<StdSegmentFs> {
    let fs = StdSegmentFs;
    let mut writer = TierWriter::create(&fs, dir, 0, 0, NS, inf_store::LogicalAddr::ZERO, mode)
        .expect("create_tier (Direct refusal here means the fs lacks O_DIRECT — ADR-0054 D3)");
    let mut value = vec![0u8; VALUE_BYTES];
    for index in 0..records {
        value[..8].copy_from_slice(&index.to_le_bytes());
        value[8..16].copy_from_slice(&(!index).to_le_bytes());
        let addr = inf_store::LogicalAddr::from_raw(index * VALUE_BYTES as u64).expect("fits");
        writer.append(addr, &value).expect("append");
    }
    writer.sync().expect("fdatasync — durable before any read");
    writer
}

// ---- one leg ---------------------------------------------------------------

struct LegResult {
    reads: u64,
    wall_us: u64,
    cpu_us: u64,
    p50_us: f64,
    p99_us: f64,
    p999_us: f64,
    vm_hwm_kib: u64,
    cgroup_file_delta: i64,
    resident_after: u64,
}

/// Sustained-QD read leg: keep `QD` reads in flight over `reads`
/// uniform picks from `range` records; verify markers on completion.
fn run_leg(
    fd: RawFd,
    path: &Path,
    reads: u64,
    range: u64,
    seed: u64,
    drop_cache_first: bool,
) -> LegResult {
    if drop_cache_first {
        drop_page_cache(path);
    }
    let cgroup_before = cgroup_file_bytes();
    let mut driver = UringDriver::new(64).expect("io_uring");
    let mut pool = AlignedPool::new(POOL_BUFFERS, WINDOW_FRAMES * TIER_FRAME_BYTES);
    driver.register_tier_pool(&mut pool).expect("registration");
    let cold = ColdReads::with_config(
        pool,
        ColdReadConfig { qd_cap: QD, merge: false, ..ColdReadConfig::default() },
    );
    let file = TierFileId::new(0);
    let mut rng = SplitMix64(seed);
    let mut latencies_ns: Vec<u64> = Vec::with_capacity(reads as usize);
    let mut outstanding: Vec<(ColdWait, Instant, u64, usize)> = Vec::new();
    let mut recv_pool = BufferPool::new(2, 4096);
    let mut out = Vec::new();
    let mut started = 0u64;
    let cpu_before = rusage_cpu_us();
    let wall_before = Instant::now();
    while (latencies_ns.len() as u64) < reads {
        while started < reads && outstanding.len() < QD {
            let index = rng.next() % range;
            let delta = index * VALUE_BYTES as u64;
            let (first, count, skip) = tier_frame_span(delta, VALUE_BYTES);
            assert!(count as usize <= WINDOW_FRAMES);
            let wait = cold
                .enqueue(
                    fd,
                    file,
                    tier_frame_offset(first),
                    count as usize * TIER_FRAME_BYTES,
                    ReadClass::Foreground,
                    0,
                )
                .expect("queue sized for QD");
            outstanding.push((wait, Instant::now(), index, skip));
            started += 1;
        }
        {
            let cold = cold.clone();
            cold.drain(|op| driver.push(op));
        }
        out.clear();
        driver.submit_and_reap(&mut recv_pool, Wait::Poll, &mut out).expect("submit");
        for completion in out.drain(..) {
            assert_eq!(completion.token.class(), TokenClass::TierRead);
            cold.on_completion(completion.token, completion.result, 0);
        }
        // Poll outstanding waits; completed ones record + verify + refill.
        let mut i = 0;
        let mut extracted = Vec::new();
        while i < outstanding.len() {
            if let Some(done) = poll_ready(&mut outstanding[i].0) {
                let (_, at, index, skip) = outstanding.swap_remove(i);
                done.outcome().expect("clean read");
                latencies_ns.push(at.elapsed().as_nanos() as u64);
                done.bytes(|window| {
                    tier_extract(window, skip, 16, &mut extracted).expect("frames verify");
                });
                assert_eq!(&extracted[..8], &index.to_le_bytes(), "marker byte-exact");
                drop(done);
            } else {
                i += 1;
            }
        }
    }
    let wall_us = wall_before.elapsed().as_micros() as u64;
    let cpu_us = rusage_cpu_us() - cpu_before;
    cold.reconcile().expect("custody clean after the leg");
    latencies_ns.sort_unstable();
    let pick = |p: f64| -> f64 {
        let rank = ((p / 100.0) * latencies_ns.len() as f64).ceil().max(1.0) as usize - 1;
        latencies_ns[rank.min(latencies_ns.len() - 1)] as f64 / 1000.0
    };
    let cgroup_after = cgroup_file_bytes();
    LegResult {
        reads,
        wall_us,
        cpu_us,
        p50_us: pick(50.0),
        p99_us: pick(99.0),
        p999_us: pick(99.9),
        vm_hwm_kib: proc_status_kib("VmHWM:"),
        cgroup_file_delta: match (cgroup_before, cgroup_after) {
            (Some(b), Some(a)) => a as i64 - b as i64,
            _ => 0,
        },
        resident_after: resident_file_bytes(path),
    }
}

fn poll_ready(wait: &mut ColdWait) -> Option<inf_runtime::ColdDone> {
    use core::future::Future;
    use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
    fn noop(_: *const ()) {}
    fn clone(p: *const ()) -> RawWaker {
        RawWaker::new(p, &VTABLE)
    }
    static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, noop, noop, noop);
    // SAFETY: the no-op waker never dereferences its pointer.
    let waker = unsafe { Waker::from_raw(RawWaker::new(core::ptr::null(), &VTABLE)) };
    match core::pin::Pin::new(wait).poll(&mut Context::from_waker(&waker)) {
        Poll::Ready(done) => Some(done),
        Poll::Pending => None,
    }
}

// ---- main ------------------------------------------------------------------

fn main() {
    let Some(base) = std::env::var_os("INF_TIER_AB_DIR") else {
        eprintln!(
            "tier_io_mode: set INF_TIER_AB_DIR to a real-device directory (NOT tmpfs); skipping"
        );
        return;
    };
    let base = PathBuf::from(base);
    std::fs::create_dir_all(&base).expect("base dir");
    let fstype = fs_of(&base);
    assert_ne!(fstype, "tmpfs", "a tmpfs A/B measures RAM, not the device — refused");
    let records = env_u64("INF_TIER_AB_RECORDS", 1 << 20);
    let reads = env_u64("INF_TIER_AB_READS", 20_000);
    let reps = env_u64("INF_TIER_AB_REPS", 3);

    let mut report = String::new();
    let push = |report: &mut String, line: &str| {
        println!("{line}");
        report.push_str(line);
        report.push('\n');
    };
    push(&mut report, "# M4-S09 tier I/O mode A/B (dev tier — ADR-0054)");
    push(
        &mut report,
        &format!(
            "env: fs={fstype} governor={} records={records} (~{} MiB payload) reads={reads} \
             QD={QD} reps={reps} (ABBA) merge=off",
            governor(),
            (records * VALUE_BYTES as u64) >> 20,
        ),
    );
    push(
        &mut report,
        "memory-honesty scope: cgroup delta is the session scope (shared shell — disclosed); \
         mincore rows are exact per-file page-cache bytes (ADR-0054 D4).",
    );

    // Per-mode corpora (mode is a per-file property — one file each).
    let modes = [(TierIoMode::Buffered, "buffered"), (TierIoMode::Direct, "direct")];
    let mut writers = Vec::new();
    for (mode, name) in modes {
        let dir = base.join(name);
        // Fresh corpus per run (create-new semantics refuse a stale file).
        if dir.exists() {
            std::fs::remove_dir_all(&dir).expect("clear stale corpus");
        }
        std::fs::create_dir_all(&dir).expect("mode dir");
        let build = Instant::now();
        let writer = build_corpus(&dir, mode, records);
        push(
            &mut report,
            &format!(
                "corpus[{name}]: {} bytes in {:.1}s (build bandwidth is NOT a row — S11 owns \
                 flush; the {name} build cost is disclosed context only)",
                writer.data_len(),
                build.elapsed().as_secs_f64()
            ),
        );
        writers.push((writer, name));
    }

    for (row, range, drop_cache) in
        [("cold-uniform", records, true), ("re-reference", HOT_RECORDS.min(records), false)]
    {
        push(&mut report, &format!("\n## row: {row} (range {range} records)"));
        for rep in 0..reps {
            // ABBA: alternate which mode goes first each replicate.
            let order: [usize; 2] = if rep % 2 == 0 { [0, 1] } else { [1, 0] };
            for &m in &order {
                let (writer, name) = &writers[m];
                let leg = run_leg(
                    writer.raw_fd().expect("real fd"),
                    writer.path(),
                    reads,
                    range,
                    0x5EED_0000 + rep,
                    drop_cache,
                );
                push(
                    &mut report,
                    &format!(
                        "rep{rep} {name:<8} p50 {:>8.1} µs · p99 {:>8.1} µs · p99.9 {:>8.1} µs · \
                         {:>6.0} reads/s · cpu {:>5.1} µs/op · VmHWM {} KiB · cgroup-file Δ \
                         {:+} KiB · file-resident {} KiB",
                        leg.p50_us,
                        leg.p99_us,
                        leg.p999_us,
                        leg.reads as f64 / (leg.wall_us as f64 / 1e6),
                        leg.cpu_us as f64 / leg.reads as f64,
                        leg.vm_hwm_kib,
                        leg.cgroup_file_delta / 1024,
                        leg.resident_after >> 10,
                    ),
                );
            }
        }
    }

    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs();
    let artifacts = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.artifacts/m4/s09");
    std::fs::create_dir_all(&artifacts).expect("artifact dir");
    let out_path = artifacts.join(format!("{stamp}-tier-io-ab.txt"));
    let mut file = std::fs::File::create(&out_path).expect("artifact file");
    file.write_all(report.as_bytes()).expect("artifact write");
    println!("\nartifact: {}", out_path.display());
}
