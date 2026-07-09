//! M2-S13 replay-throughput rehearsal (dev tier): time `open_cell_log` —
//! the real boot path: manifest → `.ick` load → tail replay → S14 slack
//! scans → rotor reopen — over a synthetic durable-cell image on the real
//! filesystem, single cell, single thread (recovery is a per-cell local
//! problem, L1).
//!
//! The M2 gate (≥ 1 GB/s/cell) binds on the reference NVMe at S22; this
//! rehearsal isolates the CPU-side replay path (page-cache-warm reads —
//! the S04 reader's dev artifact measured the raw sequential read leg at
//! 3.6 GiB/s on this class of box). Disclose the warm cache in any report.
//!
//! Rows:
//!   tail-only   full-log replay, no checkpoint (the worst-case shape)
//!   ick-tail    checkpoint at ~50% + manifest + truncated prefix (the
//!               steady-state shape: half from `.ick`, half from frames)
//!   slack-floor tiny log in full-size preallocated segments — the fixed
//!               per-boot cost of the S14 tail-region scans over sparse
//!               zeros, isolated
//!
//! Run:  taskset -c 4 cargo bench -p inf-server --bench recovery_replay
//! Env:  INF_BENCH_DIR (default `target/`), INF_BENCH_MIB (default 1024),
//!       INF_BENCH_REPS (default 3)

use std::path::{Path, PathBuf};
use std::time::Instant;

use inf_foundation::time::Nanos;
use inf_log::ckpt::{SyncIckWriter, ick_file_name};
use inf_log::fs::StdSegmentFs;
use inf_log::{
    CkptConfig, Lsn, Manifest, MutationEffect, NsId, RecordView, SegmentConfig, SegmentId,
    SegmentRotor, StagingConfig, StagingRing, create_cell_dirs, scan_log_dir, segment_file_name,
    write_manifest,
};
use inf_server::{DurableConfig, open_cell_log};
use inf_store::{FsyncClass, Keyspace, NsMode, NsSpec, StoreConfig, WallAnchor};

const NS: NsId = NsId(16);
const CELL: u16 = 0;
const VALUE_LEN: usize = 512;
const RECORDS_PER_FRAME: u64 = 64;
/// Encoded record ≈ key(12) + value(512) + framing overhead.
const APPROX_RECORD_BYTES: u64 = 530;

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// M2-S22 cold-cache rows: evict every file under `root` from the page
/// cache — sync (dirty pages don't drop) then fadvise(DONTNEED). No
/// root privileges needed; the artifact discloses the method.
fn cool_tree(root: &Path) {
    let Ok(entries) = std::fs::read_dir(root) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            cool_tree(&path);
        } else if let Ok(file) = std::fs::File::open(&path) {
            use std::os::fd::AsRawFd;
            let _ = file.sync_all();
            // SAFETY: fadvise on a live fd; offset 0 + len 0 = whole file.
            unsafe { libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
        }
    }
}

fn key_of(i: u64) -> [u8; 12] {
    let mut key = *b"k:0000000000";
    let digits = format!("{i:010}");
    key[2..].copy_from_slice(digits.as_bytes());
    key
}

fn value_of(i: u64, buf: &mut [u8; VALUE_LEN]) {
    let mut x = i.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    for chunk in buf.chunks_mut(8) {
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        let bytes = x.wrapping_mul(0x2545_F491_4F6C_DD1D).to_le_bytes();
        chunk.copy_from_slice(&bytes[..chunk.len()]);
    }
}

fn durable_config(root: &Path, segment_bytes: u32) -> DurableConfig {
    DurableConfig {
        data_dir: root.to_path_buf(),
        staging: StagingConfig { capacity_bytes: 128 << 10 },
        segment: SegmentConfig { segment_bytes, seal_after_ms: None },
        ckpt: CkptConfig::default(),
        recover: Default::default(),
        sync_pipeline: 1,
    }
}

fn fresh_keyspace() -> Keyspace {
    let cfg = StoreConfig {
        initial_keys: env_u64("INF_BENCH_PRESIZE", 0) as usize,
        ..Default::default()
    };
    let mut ks = Keyspace::new(cfg);
    ks.ns_create(NsSpec {
        id: NS,
        name: b"bench".to_vec(),
        mode: NsMode::Durable,
        fsync: Some(FsyncClass::Always),
        policy: None,
        maxmemory: None,
    })
    .expect("ns");
    ks
}

struct BuiltImage {
    /// Bytes recovery reads: frames at/above the floor plus the `.ick`.
    replay_bytes: u64,
    records: u64,
}

/// Build one cell image under `root`: `records` unique-key sets, frames of
/// [`RECORDS_PER_FRAME`], and — when `ckpt_at` is set — a begin marker at
/// that record index, an `.ick` holding the state at the marker, a
/// manifest, and the covered prefix segments truncated.
fn build_image(root: &Path, cfg: &DurableConfig, records: u64, ckpt_at: Option<u64>) -> BuiltImage {
    let fs = StdSegmentFs;
    let shard = root.join(format!("shard-{CELL}"));
    let dirs = create_cell_dirs(&fs, &shard).expect("dirs");
    let mut rotor = SegmentRotor::create_fresh(fs, dirs.log.clone(), cfg.segment).expect("rotor");
    let mut ring = StagingRing::new(cfg.staging);
    let mut value = [0u8; VALUE_LEN];
    let mut staged = 0u64;
    let mut begin: Option<Lsn> = None;
    // Data extent per segment — replay reads exactly these bytes.
    let mut extents: std::collections::BTreeMap<u32, u64> = std::collections::BTreeMap::new();
    let flush = |ring: &mut StagingRing,
                 rotor: &mut SegmentRotor<StdSegmentFs>,
                 extents: &mut std::collections::BTreeMap<u32, u64>|
     -> inf_log::FrameLease {
        rotor.maintain(0).expect("maintain");
        let lease = ring.flush_into(rotor, 0).expect("flush").expect("frame");
        extents.insert(rotor.active_segment().0, u64::from(rotor.active_written()));
        lease
    };

    for i in 0..records {
        if ckpt_at == Some(i) {
            // Marker in its own frame; the state at the marker is exactly
            // keys 0..i (unique keys — the instantaneous fuzzy walk).
            if staged > 0 {
                let lease = flush(&mut ring, &mut rotor, &mut extents);
                ring.release(lease);
                staged = 0;
            }
            let at = ring.stage(&MutationEffect::CkptBegin { ckpt_id: 1 }).expect("stage");
            let lease = flush(&mut ring, &mut rotor, &mut extents);
            begin = Some(lease.lsn_of(at));
            ring.release(lease);
        }
        let key = key_of(i);
        value_of(i, &mut value);
        ring.stage(&MutationEffect::StringSet { ns: NS, key: &key, value: &value }).expect("stage");
        staged += 1;
        if staged == RECORDS_PER_FRAME || i + 1 == records {
            let lease = flush(&mut ring, &mut rotor, &mut extents);
            ring.release(lease);
            staged = 0;
        }
    }
    drop(rotor);

    let mut ick_bytes = 0u64;
    let mut floor = SegmentId(0);
    if let Some(half) = ckpt_at {
        let begin = begin.expect("marker staged");
        floor = begin.segment;
        let mut w = SyncIckWriter::create(fs, &dirs.ckpt, &cfg.ckpt, CELL, 1, begin, &[NS.0])
            .expect("ick create");
        for i in 0..half {
            let key = key_of(i);
            value_of(i, &mut value);
            w.append(&RecordView::StringPostImage { ns: NS, key: &key, value: &value })
                .expect("ick record");
        }
        w.finish().expect("ick publish");
        ick_bytes = std::fs::metadata(dirs.ckpt.join(ick_file_name(1))).expect("ick meta").len();
        let scan = scan_log_dir(&fs, &dirs.log).expect("scan");
        let segments: Vec<SegmentId> =
            scan.segments().iter().copied().filter(|id| *id >= floor).collect();
        write_manifest(&fs, &shard, &Manifest { ckpt_id: 1, begin_lsn: begin, segments })
            .expect("manifest");
        // The truncation slice already ran: covered prefix segments gone.
        for &id in scan.segments().iter().filter(|id| **id < floor) {
            std::fs::remove_file(dirs.log.join(segment_file_name(id))).expect("truncate");
        }
    }

    // Replay reads the data extent of every retained (≥ floor) segment
    // plus the `.ick` — sparse prealloc means file sizes lie, so the
    // extents recorded at flush time are the truth.
    let replay_bytes =
        extents.iter().filter(|(id, _)| **id >= floor.0).map(|(_, end)| *end).sum::<u64>()
            + ick_bytes;
    BuiltImage { replay_bytes, records }
}

struct RepResult {
    millis: f64,
    applied: u64,
    ckpt_records: u64,
    digest: u64,
}

fn timed_recover<F: inf_log::fs::SegmentFs>(
    fs: F,
    ks: &mut Keyspace,
    cfg: &DurableConfig,
    anchor: WallAnchor,
    now: Nanos,
) -> (f64, inf_server::RecoverStats) {
    let start = Instant::now();
    let (rotor, stats, _seed) = open_cell_log(fs, ks, CELL, cfg, anchor, now).expect("recover");
    let millis = start.elapsed().as_secs_f64() * 1e3;
    drop(rotor);
    (millis, stats)
}

fn recover_once(cfg: &DurableConfig) -> RepResult {
    let mut ks = fresh_keyspace();
    let anchor = WallAnchor { internal_ms: 0, unix_ms: 1_750_000_000_000 };
    let now = Nanos::from_millis(1);
    // M2.5-S08 A/B: INF_BENCH_READAHEAD=0 is the lever-off arm (bare
    // StdSegmentFs, serial read∘apply); default rides ReadAheadFs like
    // infinityd does.
    let (millis, stats) = if env_u64("INF_BENCH_READAHEAD", 1) != 0 {
        timed_recover(inf_server::ReadAheadFs::new(StdSegmentFs, true), &mut ks, cfg, anchor, now)
    } else {
        timed_recover(StdSegmentFs, &mut ks, cfg, anchor, now)
    };
    let digest = ks.state_digest(now);
    RepResult {
        millis,
        applied: stats.records_applied,
        ckpt_records: stats.ckpt_records,
        digest: digest.digest,
    }
}

fn run_row(name: &str, cfg: &DurableConfig, image: &BuiltImage, reps: u64, cool: Option<&Path>) {
    let mut digests = Vec::new();
    println!("\n== row: {name} ==");
    println!(
        "   image: {} records, {:.1} MiB replayed per boot",
        image.records,
        image.replay_bytes as f64 / (1 << 20) as f64
    );
    for rep in 0..reps {
        if let Some(root) = cool {
            cool_tree(root);
        }
        let r = recover_once(cfg);
        let gib_s = image.replay_bytes as f64 / (1 << 30) as f64 / (r.millis / 1e3);
        let recs_s = (r.applied + r.ckpt_records) as f64 / (r.millis / 1e3);
        println!(
            "   rep {rep}: {:>8.1} ms  {:>6.2} GiB/s  {:>10.0} records/s  (applied {} + ckpt {})",
            r.millis, gib_s, recs_s, r.applied, r.ckpt_records
        );
        digests.push(r.digest);
    }
    assert!(
        digests.windows(2).all(|w| w[0] == w[1]),
        "digest determinism violated across replicates: {digests:x?}"
    );
    println!("   state digest {:#018x} identical across {reps} replicates (L7)", digests[0]);
}

fn read_first_line(path: &str) -> String {
    std::fs::read_to_string(path)
        .map(|s| s.lines().next().unwrap_or("?").to_owned())
        .unwrap_or_else(|_| "?".to_owned())
}

fn main() {
    let base =
        PathBuf::from(std::env::var("INF_BENCH_DIR").unwrap_or_else(|_| "target".to_owned()));
    let mib = env_u64("INF_BENCH_MIB", 1024);
    let reps = env_u64("INF_BENCH_REPS", 3);
    let cold = env_u64("INF_BENCH_COOL", 0) != 0;
    let records = mib * (1 << 20) / APPROX_RECORD_BYTES;

    println!("recovery_replay (M2-S13 dev rehearsal)");
    println!("  kernel   {}", read_first_line("/proc/sys/kernel/osrelease"));
    println!(
        "  governor {}",
        read_first_line("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor")
    );
    println!(
        "  target   {mib} MiB (~{records} records), {reps} replicates, {} page cache",
        if cold { "COLD (fadvise-DONTNEED per rep)" } else { "warm" }
    );
    if !cold {
        println!("  caveat   dev tier: CPU-path rehearsal; the ≥ 1 GB/s gate binds on the");
        println!("           reference NVMe with a cold-cache row at S22 (INF_BENCH_COOL=1)");
    }

    // tail-only: full-log replay.
    let root = base.join("recovery-replay-bench/tail-only");
    let _ = std::fs::remove_dir_all(&root);
    let cfg = durable_config(&root, 256 << 20);
    let image = build_image(&root, &cfg, records, None);
    run_row("tail-only", &cfg, &image, reps, cold.then_some(root.as_path()));
    let _ = std::fs::remove_dir_all(&root);

    // ick-tail: checkpoint at 50%, prefix truncated.
    let root = base.join("recovery-replay-bench/ick-tail");
    let _ = std::fs::remove_dir_all(&root);
    let cfg = durable_config(&root, 64 << 20);
    let image = build_image(&root, &cfg, records, Some(records / 2));
    run_row("ick-tail", &cfg, &image, reps, cold.then_some(root.as_path()));
    let _ = std::fs::remove_dir_all(&root);

    // slack-floor: ~4 MiB of data inside full-size sparse segments — the
    // per-boot fixed cost of the S14 scans, isolated.
    let root = base.join("recovery-replay-bench/slack-floor");
    let _ = std::fs::remove_dir_all(&root);
    let cfg = durable_config(&root, 256 << 20);
    let image = build_image(&root, &cfg, 8_000, None);
    run_row("slack-floor (S14 scan cost)", &cfg, &image, reps, cold.then_some(root.as_path()));
    let _ = std::fs::remove_dir_all(&root);
}
