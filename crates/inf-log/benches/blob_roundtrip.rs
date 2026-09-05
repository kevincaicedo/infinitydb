#![allow(
    clippy::disallowed_methods,
    reason = "bench target: the wall clock is the instrument, not cell code"
)]
//! M4-S17 — the 1 GiB round-trip staging budget (plan §4.1 / AC 2,
//! ADR-0061 D1/D8): one 1 GiB value flows through the chunked extent
//! writer and back through the chunked reader while resident staging
//! stays ≤ 2× the chunk budget — asserted in-process at every chunk, so
//! a staging regression fails the command, not a review. The value is
//! generated and verified streamwise (holding it would defeat the test).
//!
//! Also reports blob write amplification by construction —
//! `device_bytes / value_bytes` (frame CRC 4/4092 + one header block ≈
//! 1.001×) — the "blob WA ≈ 1×" half of the §4.1 row.
//!
//! Run: `INF_BLOB_DIR=<dir-on-nvme> taskset -c 4 cargo bench -p inf-log
//! --bench blob_roundtrip` — refuses tmpfs (a RAM number is a lie).
//! Mode: `Direct` (ADR-0054 default) unless `INF_BLOB_MODE=buffered`.
//! Without `INF_BLOB_DIR`: MemFs at 64 MiB — the staging asserts still
//! bind (they are size-independent); only the throughput line is
//! meaningless and says so. Artifact: 3 replicates under
//! `.artifacts/m4/s17/`.

use std::path::{Path, PathBuf};
use std::time::Instant;

use inf_log::NsId;
use inf_log::blob::{BLOB_CHUNK_BYTES, ExtentId, ExtentWriter, open_extent, unlink_extent_file};
use inf_log::fs::mem::MemFs;
use inf_log::fs::{SegmentFs, StdSegmentFs, TierIoMode};

const NS: NsId = NsId(61);
const GIB: u64 = 1 << 30;
const MEMFS_BYTES: u64 = 64 << 20;
/// The read/verify stream chunk (distinct from the writer's internal
/// batch window so the two budgets are exercised independently).
const STREAM_CHUNK: usize = 1 << 20;

/// Deterministic value stream: chunk `i` of the value, regenerated on
/// the read side — the value is never resident (that is the point).
fn fill_chunk(buf: &mut [u8], chunk_index: u64) {
    let seed = chunk_index.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut x = seed;
    for b in buf.iter_mut() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *b = x as u8;
    }
}

fn run<F: SegmentFs>(fs: &F, shard: &Path, mode: TierIoMode, total: u64, tier: &str) {
    let budget = 2 * BLOB_CHUNK_BYTES;
    let mut chunk = vec![0u8; STREAM_CHUNK];
    let id = ExtentId(1);

    // ---- write leg ----
    let t = Instant::now();
    let mut w = ExtentWriter::create(fs, shard, id, 0, NS, total, mode).expect("create");
    let mut written = 0u64;
    let mut chunk_index = 0u64;
    let mut staging_peak = 0usize;
    while written < total {
        let take = usize::try_from((total - written).min(STREAM_CHUNK as u64)).expect("fits");
        fill_chunk(&mut chunk[..take], chunk_index);
        w.append_chunk(&chunk[..take]).expect("append");
        staging_peak = staging_peak.max(w.staging_bytes());
        assert!(
            w.staging_bytes() <= budget,
            "write staging {} exceeds 2x chunk budget {budget} (AC 2)",
            w.staging_bytes()
        );
        written += take as u64;
        chunk_index += 1;
    }
    let sealed = w.finish().expect("finish");
    let write_secs = t.elapsed().as_secs_f64();
    let device_bytes = sealed.device_bytes();

    // ---- read leg (streamed, verified against the regenerated value) ----
    let t = Instant::now();
    let mut reader = open_extent(fs, shard, id, mode).expect("open");
    let mut expected = vec![0u8; STREAM_CHUNK];
    let mut got: Vec<u8> = Vec::with_capacity(STREAM_CHUNK);
    let mut offset = 0u64;
    let mut chunk_index = 0u64;
    while offset < total {
        let take = usize::try_from((total - offset).min(STREAM_CHUNK as u64)).expect("fits");
        got.clear();
        reader.read(offset, take, &mut got).expect("io").expect("crc");
        fill_chunk(&mut expected[..take], chunk_index);
        assert_eq!(got, &expected[..take], "chunk {chunk_index} reads back byte-exact");
        staging_peak = staging_peak.max(reader.staging_bytes());
        assert!(
            reader.staging_bytes() <= budget,
            "read staging {} exceeds 2x chunk budget {budget} (AC 2)",
            reader.staging_bytes()
        );
        offset += take as u64;
        chunk_index += 1;
    }
    let read_secs = t.elapsed().as_secs_f64();
    unlink_extent_file(fs, shard, id).expect("cleanup");

    // Blob write amplification by construction (ADR-0061 D8): value
    // bytes reach the device once, plus frame CRCs and one header.
    let wa_milli = (u128::from(device_bytes) * 1000).div_ceil(u128::from(total));
    println!(
        "blob_roundtrip[{tier}]: {} MiB · staging peak {} B (budget {} B) · \
         device {} B → blob WA {}.{:03}x · write {:.2} MiB/s · read {:.2} MiB/s",
        total >> 20,
        staging_peak,
        budget,
        device_bytes,
        wa_milli / 1000,
        wa_milli % 1000,
        total as f64 / (1 << 20) as f64 / write_secs,
        total as f64 / (1 << 20) as f64 / read_secs,
    );
    assert!(staging_peak <= budget, "peak staging {staging_peak} within 2x chunk budget (AC 2)");
    assert!(wa_milli < 1010, "blob WA {wa_milli} milli stays ~1x by construction");
}

fn main() {
    match std::env::var("INF_BLOB_DIR") {
        Ok(dir) => {
            let mode = match std::env::var("INF_BLOB_MODE").as_deref() {
                Ok("buffered") => TierIoMode::Buffered,
                _ => TierIoMode::Direct,
            };
            let shard: PathBuf = Path::new(&dir).join("blob-roundtrip-shard");
            let fs = StdSegmentFs;
            fs.create_dir_all(&shard).expect("shard dir");
            run(
                &fs,
                &shard,
                mode,
                GIB,
                if mode == TierIoMode::Direct { "nvme-direct" } else { "nvme-buffered" },
            );
        }
        Err(_) => {
            // MemFs fallback: the staging asserts bind identically (the
            // bound is structural, not size-dependent); throughput on a
            // BTreeMap is not a device number and the tier label says so.
            let fs = MemFs::new();
            fs.create_dir_all(Path::new("/shard")).expect("dir");
            run(&fs, Path::new("/shard"), TierIoMode::Buffered, MEMFS_BYTES, "memfs-no-claim");
        }
    }
}
