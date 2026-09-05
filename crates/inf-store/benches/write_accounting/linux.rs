#![allow(
    clippy::disallowed_methods,
    reason = "bench target: the wall clock is the instrument, not cell code"
)]
//! The Linux body of the `write_accounting` bench — see the target root
//! (`../write_accounting.rs`) for why it lives in its own module: the
//! whole measurement is `/proc/diskstats` against the database's own
//! counters, which exists on Linux only (ADR-0065 D4).
//!
//! M4-S13 accounting-vs-block-layer validation: does the sum of the
//! per-namespace write counters match what the device was actually asked
//! to write? The AC is ≤ 10% divergence on a controlled run, and the
//! whole point of the exercise is that the answer is *measured* against
//! an instrument the database does not control (`/proc/diskstats`, the
//! same counter `iostat` reads) rather than asserted from the code that
//! produced the numbers.
//!
//! **The run is controlled, deliberately:** one tiered namespace, one
//! cell, a real WAL (`SegmentRotor` + `StagingRing`, frames written to
//! real segment files) and the real `TierFlush` pipeline in `Direct`
//! mode on a real filesystem, with **checkpointing quiesced** — a
//! checkpoint would write bytes that belong to a different domain
//! (`INFO persistence` reports those) and would show up here as an
//! unexplained device-byte surplus. Everything else on the box is noise
//! the methodology names: the harness reports the idle write rate it
//! measured before the workload so a reader can judge whether the box
//! was quiet enough for the number to mean anything.
//!
//! Run:
//! `INF_ACCT_DIR=<dir-on-nvme> taskset -c 4 cargo bench -p inf-store
//! --bench write_accounting`
//! (refuses tmpfs — a RAM "device byte" is not a device byte).
//! Optional: `INF_ACCT_MIB=<user MiB>` (default 256).
//! Artifact: `.artifacts/m4/s13/`.

use std::path::{Path, PathBuf};
use std::time::Instant;

use inf_alloc::REGION_PAGE_BYTES;
use inf_log::flush::unlink_tier_file;
use inf_log::fs::{SegmentFile, SegmentFs, StdSegmentFs};
use inf_log::{
    MutationEffect, NsId, SegmentConfig, SegmentRotor, StagingConfig, StagingRing,
    TIER_FRAME_BYTES, TierFlush, TierFlushConfig, TierIoMode, create_cell_dirs, tier_extract,
    tier_frame_offset, tier_frame_span,
};
use inf_store::KeyHasher;
use inf_store::{
    AddressSpaceConfig, CompactionWork, DemotionConfig, Keyspace, LogicalAddr, StoreConfig,
    TieredLookup, WriteAccounting,
};

const NS: NsId = NsId(31);
/// RAM budget for the namespace: the workload is many times this, so the
/// tier does real work and the residue left in the mutable region is a
/// rounding error rather than the measurement.
const BUDGET: u64 = 64 << 20;
/// One commit page (ADR-0052 D4 default) — the MAINTAIN slice quantum
/// for both the seal step and the flush barrier.
const SLICE: u64 = REGION_PAGE_BYTES as u64;
const FILE_CAPACITY: u64 = 64 << 20;
/// Tier-file capacity for the churn leg. Smaller on purpose: with 64 MiB
/// files a live set of the same size is reclaimed as one fully-dead file
/// at a time, relocating almost nothing — and relocation is exactly what
/// this leg exists to put on the device. Sixteen files spanning the live
/// set give dead ratios that walk up gradually, which is the shape the
/// dead-ratio trigger was designed for.
const CHURN_FILE_CAPACITY: u64 = 4 << 20;
const SEGMENT_BYTES: u32 = 64 << 20;
const VALUE_BYTES: usize = 512;
/// Records staged into one WAL frame before it is sealed and written.
/// Large frames keep the per-frame envelope (and the block-granularity
/// rounding of the final writeback) far below the measurement noise.
const RECORDS_PER_FRAME: u32 = 1024;
/// Seconds of doing nothing, to read the box's background write rate.
const IDLE_PROBE_SECS: u64 = 3;
/// Copy-forward budget per MAINTAIN round on the churn leg (ADR-0059 D6
/// default).
const COMPACT_SLICE: u64 = 1 << 20;
/// MAINTAIN rounds between retirement cycles on the churn leg.
const ROUNDS_PER_PUBLISH: u64 = 16;

// ---- block-layer instrument (procfs; no dependency, no root) ----------------

/// `(major, minor)` of the device backing `dir`, decoded from `st_dev`
/// the way the kernel encodes it (12+20 bit split).
fn device_numbers(dir: &Path) -> std::io::Result<(u64, u64)> {
    use std::os::linux::fs::MetadataExt;
    let dev = std::fs::metadata(dir)?.st_dev();
    let major = ((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff);
    let minor = (dev & 0xff) | ((dev >> 12) & !0xff);
    Ok((major, minor))
}

/// Sectors written by `(major, minor)` since boot — `/proc/diskstats`
/// field 10, the number `iostat` turns into kB_wrtn. Sectors are 512 B
/// by kernel convention in this file regardless of physical block size.
fn sectors_written(major: u64, minor: u64) -> Option<u64> {
    let stats = std::fs::read_to_string("/proc/diskstats").ok()?;
    for line in stats.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 10 {
            continue;
        }
        if f[0].parse::<u64>().ok()? == major && f[1].parse::<u64>().ok()? == minor {
            return f[9].parse::<u64>().ok();
        }
    }
    None
}

fn refuse_tmpfs(dir: &Path) {
    // A tmpfs directory has no diskstats row at all, so the run would
    // fail loudly anyway — this says why, at the top, in one line.
    let (major, _) = device_numbers(dir).expect("stat the bench dir");
    assert_ne!(
        major,
        0,
        "{} is a memory filesystem — point INF_ACCT_DIR at the NVMe",
        dir.display()
    );
}

// ---- the controlled workload -------------------------------------------------

/// Which leg of the validation this run is (M4-S16 added the second).
#[derive(Copy, Clone, PartialEq, Eq)]
enum Leg {
    /// Unique keys, no dead bytes, no compaction — the S13 leg that
    /// established the write-twice baseline.
    Insert,
    /// A bounded key set overwritten until the same user-byte total, with
    /// copy-forward compaction running in every MAINTAIN round. The leg
    /// that settles ADR-0060 D2: with relocations in the picture, is the
    /// reported numerator `wal + flush` (the accepted one) or
    /// `wal + flush + compaction` (the rejected one)? The block layer,
    /// which counts each device write exactly once, decides.
    Churn,
}

struct Run {
    accounting: WriteAccounting,
    /// Frame bytes the WAL actually wrote (header + records + trailer).
    /// `wal_bytes` is the record half of this; the difference is the
    /// shared envelope the counters deliberately do not pro-rate.
    wal_frame_bytes: u64,
    /// The namespace's `INFO tiering` per-namespace fields at the end of
    /// the run, in `INFO`'s own order — printed so the operator guide's
    /// worked example carries measured values, never invented ones.
    ns_line: String,
    records: u64,
    seconds: f64,
}

/// Writes `user_mib` MiB of user bytes through the full path — WAL frame
/// then tier flush — and returns what the counters saw.
fn workload(dir: &Path, user_mib: u64, leg: Leg) -> Run {
    let log_dir = dir.join("log");
    let shard_dir = dir.join("shard-0");
    let dirs = create_cell_dirs(&StdSegmentFs, dir).expect("cell dirs");
    std::fs::create_dir_all(&shard_dir).expect("shard dir");

    let demote = DemotionConfig { slice_bytes: SLICE, ..DemotionConfig::for_budget(BUDGET, SLICE) };
    let mut ks = Keyspace::new(StoreConfig::default());
    assert!(
        ks.materialize_tiered(
            NS,
            AddressSpaceConfig {
                reserve_bytes: demote.ring_reserve_bytes().expect("valid budget"),
                page_bytes: REGION_PAGE_BYTES,
                life_origin: LogicalAddr::ZERO,
            },
            demote,
            1 << 16,
        )
        .is_ok()
    );
    let mut flush = TierFlush::new(
        StdSegmentFs,
        TierFlushConfig {
            shard_dir: shard_dir.clone(),
            cell: 0,
            ns: NS,
            mode: TierIoMode::Direct,
            file_capacity: match leg {
                Leg::Insert => FILE_CAPACITY,
                Leg::Churn => CHURN_FILE_CAPACITY,
            },
            slice_bytes: SLICE,
        },
        0,
    );
    let mut rotor = SegmentRotor::create_fresh(
        StdSegmentFs,
        dirs.log.clone(),
        SegmentConfig { segment_bytes: SEGMENT_BYTES, ..Default::default() },
    )
    .expect("fresh log");
    let mut ring = StagingRing::new(StagingConfig::default());

    let value = vec![0x77u8; VALUE_BYTES];
    let records = (user_mib << 20) / (8 + VALUE_BYTES as u64);
    // The churn leg cycles a bounded key set so overwrites kill cold
    // copies and compaction has something to reclaim. The set is sized so
    // the live data is many tier files but a small multiple of the memory
    // budget — dead ratios then walk up gradually instead of jumping.
    let keys = match leg {
        Leg::Insert => records,
        Leg::Churn => (records / 8).max(1),
    };
    let mut lens: Vec<usize> = vec![0; usize::try_from(keys).expect("fits")];
    let mut versions: Vec<u32> = vec![0; usize::try_from(keys).expect("fits")];
    let started = Instant::now();
    let mut wal_frame_bytes = 0u64;
    let mut seed = 0x5165_0016u64;
    let mut ckpt_id = 0u64;
    let mut rounds = 0u64;
    for i in 0..records {
        // Churn overwrites are skewed (85% into the hottest fifth), which
        // is what leaves a tier file mostly-dead-with-survivors rather
        // than uniformly dead: a file that dies whole is reclaimed for
        // free, and free reclaim is not what this leg is measuring.
        let idx = if i < keys {
            usize::try_from(i).expect("fits")
        } else if seeded(&mut seed) % 100 < 85 {
            usize::try_from(seeded(&mut seed) % (keys / 5).max(1)).expect("fits")
        } else {
            usize::try_from(seeded(&mut seed) % keys).expect("fits")
        };
        let key = format!("k:{idx:06}");
        let effect = MutationEffect::StringSet { ns: NS, key: key.as_bytes(), value: &value };
        let table = ks.tiered_store_mut(NS).expect("materialized");
        table.stage_wal(&mut ring, &effect).expect("frame has room");
        let hash = KeyHasher::default().hash(key.as_bytes());
        let placed = if i < keys {
            table
                .insert(key.as_bytes(), &value, hash)
                .expect("the drain below keeps the budget window open")
        } else {
            // Overwrite: the displaced copy is cold by now, so this is the
            // copy-to-tail path whose dead bytes drive compaction. Lengths
            // and versions are tracked here rather than read back — a cold
            // read would add device *reads* to a write-accounting run.
            let addr = match table.lookup(key.as_bytes(), hash, &[]) {
                TieredLookup::Ram(a) | TieredLookup::Cold(a) => a,
                TieredLookup::Miss => unreachable!("every key was inserted above"),
            };
            let _ = table.take_displacement_origins(hash, addr);
            table
                .update(key.as_bytes(), &value, hash, addr, lens[idx], versions[idx])
                .expect("the drain below keeps the budget window open")
        };
        lens[idx] = table.record(placed).encoded_len;
        versions[idx] = table.record(placed).version;
        if i % u64::from(RECORDS_PER_FRAME) == u64::from(RECORDS_PER_FRAME) - 1 {
            wal_frame_bytes += write_frame(&mut ring, &mut rotor);
            maintain(&mut ks, &mut flush);
            if leg == Leg::Churn {
                compact(&mut ks, &mut flush);
                maintain(&mut ks, &mut flush);
                rounds += 1;
                // Retirement cycles keep the leg in steady state: without
                // them emptied files are never unlinked, dead space only
                // grows, and compaction runs out of candidates it is
                // allowed to reclaim (ADR-0059 D3).
                if rounds.is_multiple_of(ROUNDS_PER_PUBLISH) {
                    ckpt_id += 1;
                    publish(&mut ks, &mut flush, ckpt_id);
                }
            }
        }
    }
    wal_frame_bytes += write_frame(&mut ring, &mut rotor);
    maintain(&mut ks, &mut flush);
    if leg == Leg::Churn {
        compact(&mut ks, &mut flush);
        maintain(&mut ks, &mut flush);
        publish(&mut ks, &mut flush, ckpt_id + 1);
    }
    let table = ks.tiered_store_mut(NS).expect("materialized");
    table.flush_drain(&mut flush).expect("final drain");
    let seconds = started.elapsed().as_secs_f64();

    // Force the buffered WAL bytes out of the page cache: until they are
    // on the device, the instrument cannot see them (the tier files are
    // O_DIRECT + fdatasync'd, so they already are).
    sync_dir_files(&log_dir);

    let ns_line = ns_line(&ks);
    // One namespace, so its counters *are* the cell's — and the ratio is
    // per namespace by construction (the cell aggregate type has none).
    let accounting = ks.tiered_namespaces().next().expect("materialized").1.write_accounting();
    Run { accounting, wal_frame_bytes, ns_line, records, seconds }
}

/// The `INFO tiering` per-namespace fields, in `INFO`'s order. The line
/// itself is assembled by `inf-server` (which this crate cannot see);
/// the shape is pinned there by
/// `info_tiering_renders_per_namespace_watermarks_and_write_counters`.
fn ns_line(ks: &Keyspace) -> String {
    let mut out = String::new();
    for (ns, table) in ks.tiered_namespaces() {
        let space = table.space();
        let report = space.report();
        let write = table.write_accounting();
        out.push_str(&format!(
            "tiering_ns{}:head={},flushed={},ro_boundary={},tail={},committed_bytes={},\
             budget_bytes={},live_bytes={},dead_bytes={},user_bytes={},wal_bytes={},\
             flush_bytes={},compaction_bytes={},write_amp_milli={}",
            ns.0,
            space.head().to_raw(),
            space.flushed().to_raw(),
            space.ro_boundary().to_raw(),
            space.tail().to_raw(),
            report.committed_bytes,
            table.demotion().mem_budget_bytes,
            table.live_bytes(),
            report.dead_bytes,
            write.user_bytes,
            write.wal_bytes,
            write.flush_bytes,
            write.compaction_bytes,
            write.write_amplification(),
        ));
    }
    out
}

/// Seals the staged records into a frame and writes it to the active
/// segment (the synchronous LOG-step shape). Returns the frame bytes.
fn write_frame(ring: &mut StagingRing, rotor: &mut SegmentRotor<StdSegmentFs>) -> u64 {
    let Some(lease) = ring.flush_into(rotor, 0).expect("log append") else {
        return 0;
    };
    let bytes = u64::from(lease.frame_len());
    ring.release(lease);
    bytes
}

/// One MAINTAIN cadence driven to quiescence: seal slice → flush slice →
/// release, exactly as `demote_tick` + `flush_slice` do in the reactor.
fn maintain(ks: &mut Keyspace, flush: &mut TierFlush<StdSegmentFs>) {
    for _ in 0..1_000_000u32 {
        let demoted = ks.demote_tick();
        let table = ks.tiered_store_mut(NS).expect("materialized");
        let flushed = table.flush_slice(flush).expect("flush slice");
        if demoted.sealed_bytes == 0
            && demoted.released_bytes == 0
            && flushed.appended_bytes == 0
            && flushed.confirmed_bytes == 0
            && flushed.gaps_crossed == 0
        {
            return;
        }
    }
    panic!("demotion + flush must quiesce");
}

/// One full copy-forward slice at the ADR-0059 D6 budget: the scan reads
/// come off the device (real `read_at` at frame granularity, CRC-verified
/// by `tier_extract`), the relocations land in the RAM tail, and the
/// caller's next `maintain` flushes them — which is the whole point of the
/// churn leg. Device *reads* do not enter `/proc/diskstats` field 10, so
/// the scan cannot inflate the write measurement.
fn compact(ks: &mut Keyspace, flush: &mut TierFlush<StdSegmentFs>) {
    let mut spent = 0u64;
    while spent < COMPACT_SLICE {
        let work = ks.tiered_store_mut(NS).expect("materialized").compaction_work(
            flush,
            false,
            COMPACT_SLICE - spent,
        );
        let CompactionWork::Read { file_id, addr, len } = work else { return };
        let meta = flush.sealed().iter().find(|m| m.id == file_id).expect("candidates are sealed");
        let len = usize::try_from(len).expect("fits");
        let (first, count, skip) = tier_frame_span(addr.to_raw() - meta.base.to_raw(), len);
        let mut window = vec![0u8; count as usize * TIER_FRAME_BYTES];
        let file = StdSegmentFs.open_read(&meta.path).expect("tier file opens");
        let mut done = 0usize;
        while done < window.len() {
            let n = file
                .read_at(tier_frame_offset(first) + done as u64, &mut window[done..])
                .expect("read");
            assert!(n > 0, "short tier file");
            done += n;
        }
        let mut bytes = Vec::new();
        tier_extract(&window, skip, len, &mut bytes).expect("CRC-clean");
        let applied =
            ks.tiered_store_mut(NS).expect("materialized").compaction_apply(file_id, addr, &bytes);
        spent += applied.consumed.max(applied.need).max(1);
        if applied.stalled {
            return;
        }
    }
}

/// One retirement cycle: a checkpoint walk stamps the emptied files, the
/// manifest excludes them, and the unlink follows the swap (ADR-0059 D3).
/// The MANIFEST/checkpoint writes themselves are deliberately *not*
/// performed — those are checkpoint-domain device bytes that
/// `INFO persistence` owns, and letting them onto this device would show
/// up here as unexplained surplus (the S13 attribution trap, still armed).
fn publish(ks: &mut Keyspace, flush: &mut TierFlush<StdSegmentFs>, ckpt_id: u64) {
    let table = ks.tiered_store_mut(NS).expect("materialized");
    table.begin_ckpt_walk(ckpt_id);
    table.end_ckpt_walk();
    table.retire_scan(ckpt_id, flush);
    let _section = table.tier_manifest(NS.0, flush);
    for id in table.commit_retirement() {
        let meta = flush.detach_sealed(id).expect("retired files are sealed");
        unlink_tier_file(&StdSegmentFs, &meta).expect("unlink");
    }
}

fn seeded(x: &mut u64) -> u64 {
    *x ^= *x << 13;
    *x ^= *x >> 7;
    *x ^= *x << 17;
    *x
}

fn sync_dir_files(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        if let Ok(file) = std::fs::File::open(entry.path()) {
            let _ = file.sync_all();
        }
    }
    if let Ok(handle) = std::fs::File::open(dir) {
        let _ = handle.sync_all();
    }
}

pub fn run() {
    let dir = std::env::var_os("INF_ACCT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("inf-write-accounting"));
    let user_mib: u64 =
        std::env::var("INF_ACCT_MIB").ok().and_then(|v| v.parse().ok()).unwrap_or(256);
    let root = dir.join(format!("acct-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("bench dir");
    refuse_tmpfs(&root);
    let (major, minor) = device_numbers(&root).expect("stat");
    let read_sectors = || {
        sectors_written(major, minor)
            .unwrap_or_else(|| panic!("no /proc/diskstats row for {major}:{minor}"))
    };

    println!("--- M4-S13 write accounting vs the block layer ---");
    println!("dir {} · device {major}:{minor} · {user_mib} MiB user bytes", root.display());

    // Noise floor: what the rest of the box writes while this process
    // writes nothing. Reported, never assumed — a run whose noise is a
    // meaningful share of the measurement is an invalid run (§19), and
    // the reader needs the number to make that call.
    let idle_before = read_sectors();
    let idle_started = Instant::now();
    // Spin rather than sleep: `thread::sleep` is deny-listed workspace-wide
    // (cells park through the driver), and a pinned bench core has nothing
    // better to do than hold still for three seconds.
    while idle_started.elapsed().as_secs() < IDLE_PROBE_SECS {
        std::hint::spin_loop();
    }
    let idle_bytes = (read_sectors() - idle_before) * 512;
    let idle_rate = idle_bytes as f64 / idle_started.elapsed().as_secs_f64();
    println!("idle noise floor: {idle_bytes} B in {IDLE_PROBE_SECS} s ({idle_rate:.0} B/s)");

    // Warm-up: creates the directories and files the measured run reuses
    // shapes of, so filesystem first-touch costs are not billed to it.
    let warmup = workload(&root.join("warmup"), 8, Leg::Insert);
    println!(
        "warm-up: {} records, accounted {} B",
        warmup.records,
        warmup.accounting.written_bytes()
    );

    let mut worst_divergence = 0.0f64;
    for (leg, label, dir) in [
        (Leg::Insert, "insert-only (S13 leg: no dead bytes, no compaction)", "main"),
        (Leg::Churn, "overwrite churn (S16 leg: copy-forward running)", "churn"),
    ] {
        let before = read_sectors();
        let run = workload(&root.join(dir), user_mib, leg);
        let device_bytes = (read_sectors() - before) * 512;
        let divergence = report_leg(label, &run, device_bytes);
        if divergence.abs() > worst_divergence.abs() {
            worst_divergence = divergence;
        }
    }

    let _ = std::fs::remove_dir_all(&root);
    println!("\nPASS: both legs within the ±10% AC (worst {worst_divergence:+.2}%).");
}

/// Reports one leg and returns its counters-vs-device divergence in
/// percent. Asserts the ±10% AC itself, so a failing run fails the
/// command — an artifact is not a place to notice a broken window later.
fn report_leg(label: &str, run: &Run, device_bytes: u64) -> f64 {
    let acct = run.accounting;
    let accounted = acct.written_bytes();
    let envelope = run.wal_frame_bytes.saturating_sub(acct.wal_bytes);
    let divergence = (accounted as f64 - device_bytes as f64) / device_bytes as f64 * 100.0;
    let with_envelope = accounted + envelope;
    let divergence_full =
        (with_envelope as f64 - device_bytes as f64) / device_bytes as f64 * 100.0;
    // The rejected numerator (ADR-0060 D2): `+ compaction_bytes`, i.e.
    // every relocated byte counted once as a relocation and once as the
    // flush that actually wrote it.
    let rejected = accounted + acct.compaction_bytes;
    let divergence_rejected = (rejected as f64 - device_bytes as f64) / device_bytes as f64 * 100.0;

    println!("\n=== {label} ===");
    println!("records {} in {:.1} s", run.records, run.seconds);
    println!("user_bytes        {:>14}", acct.user_bytes);
    println!("wal_bytes         {:>14}  (records only)", acct.wal_bytes);
    println!(
        "flush_bytes       {:>14}  (tier device bytes, relocations included)",
        acct.flush_bytes
    );
    println!(
        "compaction_bytes  {:>14}  (relocation volume — not a numerator leg)",
        acct.compaction_bytes
    );
    println!("written_bytes     {:>14}  = the write-amp numerator (wal + flush)", accounted);
    println!(
        "write amplification {}.{:03}×  (INFO: write_amp_milli)",
        acct.write_amplification().milli().expect("user bytes were admitted") / 1_000,
        acct.write_amplification().milli().expect("user bytes were admitted") % 1_000
    );
    println!("\nWAL frame envelope {:>13}  (frame header/trailer, not pro-rated)", envelope);
    println!("device (diskstats) {:>13}", device_bytes);
    println!("divergence           {divergence:>+10.2}%  (counters alone; gate ±10%)");
    println!("divergence           {divergence_full:>+10.2}%  (counters + named envelope term)");
    println!(
        "divergence           {divergence_rejected:>+10.2}%  (REJECTED numerator, \
         + compaction_bytes)"
    );
    println!("\nINFO tiering, per-namespace fields:\n{}", run.ns_line);

    assert!(
        divergence.abs() <= 10.0,
        "accounting diverges from the block layer by {divergence:.2}% — outside the S13 AC"
    );
    divergence
}
