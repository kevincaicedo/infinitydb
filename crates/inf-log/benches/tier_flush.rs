//! M4-S11 flush-bandwidth bench (§4.1: "flush ≥ 0.8× device
//! sequential-write bandwidth"; ADR-0056 D3). Two legs, ABBA, same
//! device, same total bytes, same fdatasync cadence — the ceiling is
//! measured **in the same run** (L10):
//!
//! - `raw`: sequential 1 MiB `write_at`s through the fs seam with a
//!   barrier every `SYNC_EVERY` — the device's sequential-write ceiling
//!   under the pipeline's own durability cadence.
//! - `pipeline`: `TierFlush` fed record-aligned ~1 MiB ranges (the
//!   seal-slice shape) with `slice_bytes = SYNC_EVERY` and 1 GiB file
//!   capacity — staging copy, frame CRCs, rotation, footers, and
//!   barriers all included. Data bytes/s (frame overhead 4/4096 bills
//!   against the pipeline, disclosed by construction).
//!
//! Run: `INF_BENCH_DIR=<dir-on-nvme> taskset -c 4 cargo bench -p
//! inf-log --bench tier_flush` — refuses tmpfs (a RAM ratio is a lie).
//! Mode: `Direct` (ADR-0054 default) unless `INF_TIER_MODE=buffered`.
//! Artifact: 3 replicates under `.artifacts/m4/s11/`.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Instant;

use inf_foundation::LogicalAddr;
use inf_log::flush::{TierFlush, TierFlushConfig};
use inf_log::fs::{SegmentFile, SegmentFs, StdSegmentFs, TierIoMode};
use inf_log::{NsId, TIER_FILE_CAPACITY_DEFAULT};

const NS: NsId = NsId(31);
/// Total data bytes per leg (bounded: the dev NVMe is DRAM-less and its
/// sustained-write throughput collapses across multi-GiB campaigns —
/// shorter legs + ABBA order keep the ratio honest; both legs pay
/// identically).
const TOTAL: u64 = 512 << 20;
/// Write granularity (the seal-slice range shape).
const CHUNK: usize = 1 << 20;
/// fdatasync cadence for both legs (the bulk-flush slice quantum —
/// ADR-0056 D3's knob, disclosed).
const SYNC_EVERY: u64 = 64 << 20;
const REPLICATES: usize = 3;

#[cfg(target_os = "linux")]
fn refuse_tmpfs(dir: &Path) {
    let cpath = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()).expect("path");
    // SAFETY: statfs is a plain-old-data out-param struct; all-zero is a
    // valid initial state for it.
    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: statfs writes into the zeroed out-param on success; the
    // path pointer lives across the call (bench-local plumbing, not
    // library code — the crate root keeps forbid(unsafe_code)).
    let rc = unsafe { libc::statfs(cpath.as_ptr(), &raw mut stat) };
    assert_eq!(rc, 0, "statfs({})", dir.display());
    const TMPFS_MAGIC: i64 = 0x0102_1994;
    assert_ne!(
        stat.f_type,
        TMPFS_MAGIC,
        "{} is tmpfs — a RAM bandwidth ratio is a lie; point INF_BENCH_DIR at the NVMe",
        dir.display()
    );
}

#[cfg(not(target_os = "linux"))]
fn refuse_tmpfs(_dir: &Path) {}

/// The ceiling leg: sequential aligned writes + the same barrier cadence.
fn raw_leg(dir: &Path, rep: usize) -> f64 {
    let fs = StdSegmentFs;
    let path = dir.join(format!("raw-{rep}.bin"));
    let mut file = fs.create_segment(&path, 0).expect("create");
    // One aligned 1 MiB source block (over-allocate + align, the
    // FrameStaging pattern — O_DIRECT-legal writes if the leg is ever
    // switched; buffered writes don't care).
    let raw = vec![0x5Au8; CHUNK * 2];
    let at = raw.as_ptr().align_offset(4096);
    let block = &raw[at..at + CHUNK];
    let started = Instant::now();
    let mut written = 0u64;
    let mut since_sync = 0u64;
    while written < TOTAL {
        file.write_at(written, block).expect("write");
        written += CHUNK as u64;
        since_sync += CHUNK as u64;
        if since_sync >= SYNC_EVERY {
            file.sync_data().expect("fdatasync");
            since_sync = 0;
        }
    }
    file.sync_data().expect("final fdatasync");
    let secs = started.elapsed().as_secs_f64();
    drop(file);
    let _ = std::fs::remove_file(&path);
    written as f64 / secs / (1 << 20) as f64
}

/// The pipeline leg: TierFlush end to end, data bytes per second.
fn pipeline_leg(dir: &Path, rep: usize, mode: TierIoMode) -> f64 {
    let fs = StdSegmentFs;
    let shard = dir.join(format!("flush-shard-{rep}"));
    std::fs::create_dir_all(&shard).expect("shard dir");
    let mut flush = TierFlush::new(
        fs,
        TierFlushConfig {
            shard_dir: shard.clone(),
            cell: 0,
            ns: NS,
            mode,
            file_capacity: TIER_FILE_CAPACITY_DEFAULT,
            slice_bytes: SYNC_EVERY,
        },
        0,
    );
    let payload = vec![0xC3u8; CHUNK];
    let started = Instant::now();
    let mut appended = 0u64;
    let mut since_sync = 0u64;
    while appended < TOTAL {
        let addr = LogicalAddr::from_raw(appended).expect("fits");
        flush.append_range(addr, &payload).expect("append range");
        appended += CHUNK as u64;
        since_sync += CHUNK as u64;
        if since_sync >= SYNC_EVERY {
            flush.sync().expect("slice barrier");
            since_sync = 0;
        }
    }
    flush.seal_shutdown().expect("final seal");
    let secs = started.elapsed().as_secs_f64();
    let files = flush.sealed().len();
    drop(flush);
    let _ = std::fs::remove_dir_all(&shard);
    let mibs = appended as f64 / secs / (1 << 20) as f64;
    println!("    pipeline rep {rep}: {mibs:.0} MiB/s data ({files} files sealed)");
    mibs
}

fn main() {
    let dir = std::env::var_os("INF_BENCH_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("inf-tier-flush"));
    std::fs::create_dir_all(&dir).expect("bench dir");
    refuse_tmpfs(&dir);
    let mode = match std::env::var("INF_TIER_MODE").as_deref() {
        Ok("buffered") => TierIoMode::Buffered,
        _ => TierIoMode::Direct,
    };
    println!(
        "--- M4-S11 tier-flush bandwidth (dir {}, mode {mode:?}, {} MiB/leg, sync every {} MiB) ---",
        dir.display(),
        TOTAL >> 20,
        SYNC_EVERY >> 20
    );
    let mut ratios = Vec::new();
    for rep in 0..REPLICATES {
        // ABBA: alternate which leg runs first per replicate.
        let (raw, pipe) = if rep % 2 == 0 {
            let raw = raw_leg(&dir, rep);
            let pipe = pipeline_leg(&dir, rep, mode);
            (raw, pipe)
        } else {
            let pipe = pipeline_leg(&dir, rep, mode);
            let raw = raw_leg(&dir, rep);
            (raw, pipe)
        };
        let ratio = pipe / raw;
        println!("  rep {rep}: raw {raw:.0} MiB/s | pipeline {pipe:.0} MiB/s | ratio {ratio:.3}");
        ratios.push(ratio);
        std::io::stdout().flush().ok();
    }
    ratios.sort_by(f64::total_cmp);
    let median = ratios[ratios.len() / 2];
    println!("median pipeline/raw ratio: {median:.3} (gate: >= 0.8x — §4.1 S11)");
    let _ = std::fs::remove_dir_all(&dir);
}
