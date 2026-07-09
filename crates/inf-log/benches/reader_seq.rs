//! Sequential log read throughput (M2-S04 AC, L4): a full CRC-validating
//! `SegmentReader` pass over a segment written by the staging ring + rotor.
//! Criterion reports bytes/s; the gate value (≥ 2 GB/s sequential read,
//! headroom over the 1 GB/s/cell replay gate) binds on the reference NVMe
//! only — dev-tier runs are page-cache-warm and are trend evidence, not
//! product claims (L10).

use std::hint::black_box;
use std::path::{Path, PathBuf};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use inf_log::fs::StdSegmentFs;
use inf_log::{
    MutationEffect, NsId, ReaderConfig, SegmentConfig, SegmentId, SegmentReader, SegmentRotor,
    StagingConfig, StagingRing, create_cell_dirs, segment_file_name,
};

/// M2-S22 cold-cache row (`INF_BENCH_COOL=1`): evict the segment from the
/// page cache before a pass — sync then fadvise(DONTNEED); the fadvise
/// cost (~ms) is included in the measured pass and disclosed. Pair with
/// `INF_BENCH_DIR` on a real filesystem (the default temp dir is often
/// tmpfs, where "cold" is meaningless).
fn cool_file(path: &Path) {
    if let Ok(file) = std::fs::File::open(path) {
        use std::os::fd::AsRawFd;
        let _ = file.sync_all();
        // SAFETY: fadvise on a live fd; offset 0 + len 0 = whole file.
        unsafe { libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
    }
}

/// Deterministic pseudo-random payload (SplitMix64) — no `rand` dependency.
fn payload(len: usize) -> Vec<u8> {
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        out.extend_from_slice(&z.to_le_bytes());
    }
    out.truncate(len);
    out
}

/// Write one ~`segment_bytes` segment of realistic frames (128 records of
/// 256 B values per frame ≈ 36 KiB frames) and return (log dir, bytes
/// written, records written).
fn write_segment(root: &Path, segment_bytes: u32) -> (PathBuf, u64, u64) {
    let fs = StdSegmentFs;
    let dirs = create_cell_dirs(&fs, &root.join("shard-0")).expect("dirs");
    let cfg = SegmentConfig { segment_bytes, seal_after_ms: None };
    let mut rotor = SegmentRotor::create_fresh(fs, dirs.log.clone(), cfg).expect("rotor");
    let mut ring = StagingRing::new(StagingConfig { capacity_bytes: 1 << 20 });

    let value = payload(256);
    let mut key = *b"user:0000000000000000";
    let mut records = 0u64;
    // Fill most of the segment without triggering rotation.
    while rotor.active_written() + (1 << 20) < segment_bytes {
        for i in 0..128u64 {
            key[5..21].copy_from_slice(&format!("{:016x}", records + i).into_bytes()[..16]);
            ring.stage(&MutationEffect::StringSet { ns: NsId(1), key: &key, value: &value })
                .expect("fits");
        }
        records += 128;
        let lease = ring.flush_into(&mut rotor, 0).expect("flush").expect("frame");
        ring.release(lease);
    }
    (dirs.log, u64::from(rotor.active_written()), records)
}

fn bench_sequential_read(c: &mut Criterion) {
    let root = std::env::var("INF_BENCH_DIR").map_or_else(
        |_| std::env::temp_dir().join(format!("inf-log-reader-bench-{}", std::process::id())),
        |dir| PathBuf::from(dir).join("inf-log-reader-bench"),
    );
    let cold = std::env::var("INF_BENCH_COOL").is_ok_and(|v| v == "1");
    if root.exists() {
        std::fs::remove_dir_all(&root).expect("clear stale bench dir");
    }
    let segment_bytes: u32 = 256 << 20;
    let (log_dir, written, _) = write_segment(&root, segment_bytes);
    let segment_path = log_dir.join(segment_file_name(SegmentId(0)));

    let mut group = c.benchmark_group("reader_seq");
    group.sample_size(20);
    group.throughput(Throughput::Bytes(written));
    // Window sizes bracket the default (1 MiB) to justify it with data.
    for chunk in [256 << 10, 1 << 20, 4 << 20] {
        group.bench_with_input(BenchmarkId::new("full_pass", chunk), &chunk, |b, &chunk| {
            let fs = StdSegmentFs;
            let cfg = ReaderConfig { chunk_bytes: chunk, ..ReaderConfig::default() };
            b.iter(|| {
                if cold {
                    cool_file(&segment_path);
                }
                let mut reader =
                    SegmentReader::open(&fs, &log_dir, SegmentId(0), cfg).expect("open");
                let mut records = 0u64;
                let end = reader
                    .apply_frames(|frame| {
                        for record in frame.records() {
                            let (_, view) = record.expect("valid");
                            black_box(&view);
                            records += 1;
                        }
                        Ok::<(), std::convert::Infallible>(())
                    })
                    .expect("replay");
                black_box((end, records))
            });
        });
    }
    group.finish();

    std::fs::remove_dir_all(&root).expect("cleanup");
}

criterion_group!(benches, bench_sequential_read);
criterion_main!(benches);
