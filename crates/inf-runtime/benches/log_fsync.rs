//! Driver-tier group-commit rehearsal (M2-S06 AC dry-run, dev tier): one
//! `LogWrite` frame + linked fdatasync per iteration against a real
//! preallocated file, at several group sizes. Grouped-write throughput =
//! group × fsync_rate — the §8.2 model this measures directly, with the
//! fsync latency histogram the gate requires attached to every row.
//!
//! This is the *device + driver* rehearsal: the full-stack row (staging →
//! rotor → gate under the reactor) binds at M2-S08/S22. Dev-tier numbers
//! are non-citable (L10); the ≥ 300k w/s gate value binds on the reference
//! NVMe.
//!
//! Run: `cargo bench -p inf-runtime --features uring --bench log_fsync`
//! Env:  `INF_BENCH_FILE` (default `target/log-fsync-bench.seg`),
//!       `INF_BENCH_SECS` per row (default 5).

use std::fs::OpenOptions;
use std::os::fd::IntoRawFd;
use std::time::{Duration, Instant};

use inf_alloc::BufferPool;
use inf_foundation::LogHistogram;
use inf_runtime::{
    BackendDriver, CompletionResult, CompletionToken, IoOp, StableBytes, TokenClass, UringDriver,
    Wait,
};

const FILE_BYTES: u64 = 1 << 30;
const RECORD_BYTES: usize = 70; // ~SET key/value post-image + framing share

fn main() {
    let path = std::env::var_os("INF_BENCH_FILE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("target/log-fsync-bench.seg"));
    let secs: u64 = std::env::var("INF_BENCH_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(5);

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .expect("bench file");
    let fd = file.into_raw_fd();
    // Real block reservation — segment-like conditions, no allocate-on-write
    // noise in the latency distribution.
    // SAFETY: plain fallocate on the fd we just opened.
    let rc = unsafe { libc::fallocate(fd, 0, 0, FILE_BYTES as i64) };
    assert_eq!(rc, 0, "fallocate: {}", std::io::Error::last_os_error());

    let mut driver = UringDriver::new(64).expect("io_uring");
    let mut pool = BufferPool::new(4, 1024);
    println!("# log_fsync — one LogWrite + linked fdatasync per iteration (uring, dev tier)");
    println!(
        "# file: {} ({} GiB fallocated), {} s/row, record = {} B",
        path.display(),
        FILE_BYTES >> 30,
        secs,
        RECORD_BYTES
    );
    println!(
        "{:>6} {:>10} {:>10} {:>12} {:>9} {:>9} {:>9} {:>9}",
        "group", "frames/s", "fsyncs/s", "writes/s", "p50us", "p99us", "p999us", "maxus"
    );

    for group in [1usize, 64, 256, 1024] {
        let frame = vec![0xA5u8; group * RECORD_BYTES];
        let mut hist = LogHistogram::new();
        let mut offset = 0u64;
        let mut frames = 0u64;
        let started = Instant::now();
        let deadline = started + Duration::from_secs(secs);
        let mut out = Vec::new();
        while Instant::now() < deadline {
            if offset + frame.len() as u64 > FILE_BYTES {
                offset = 0; // wrap: the file is one reusable segment
            }
            // SAFETY: `frame` is live and unmodified until both completions
            // below are reaped in this same loop body.
            let data = unsafe { StableBytes::new(&frame) };
            let t0 = Instant::now();
            driver.push(IoOp::LogWrite {
                fd,
                offset,
                data,
                token: CompletionToken::new(TokenClass::LogWrite, 1, 0),
                fsync_token: Some(CompletionToken::new(TokenClass::Fsync, 1, 0)),
            });
            let mut got = 0;
            while got < 2 {
                out.clear();
                let n = driver
                    .submit_and_reap(
                        &mut pool,
                        Wait::Park { timeout: Some(Duration::from_millis(100)) },
                        &mut out,
                    )
                    .expect("submit");
                for c in &out[..n] {
                    match c.result {
                        CompletionResult::LogWritten => got += 1,
                        CompletionResult::Synced => got += 1,
                        CompletionResult::Error { errno, .. } => {
                            panic!("I/O error errno {errno} at offset {offset}")
                        }
                        _ => unreachable!("no other ops in flight"),
                    }
                }
            }
            hist.record(t0.elapsed().as_micros() as u64);
            offset += frame.len() as u64;
            frames += 1;
        }
        let elapsed = started.elapsed().as_secs_f64();
        let fps = frames as f64 / elapsed;
        println!(
            "{:>6} {:>10.0} {:>10.0} {:>12.0} {:>9} {:>9} {:>9} {:>9}",
            group,
            fps,
            fps,
            fps * group as f64,
            hist.percentile(50.0),
            hist.percentile(99.0),
            hist.percentile(99.9),
            hist.max(),
        );
    }
    // SAFETY: fd from into_raw_fd above; closed exactly once.
    unsafe { libc::close(fd) };
    std::fs::remove_file(&path).ok();
}
