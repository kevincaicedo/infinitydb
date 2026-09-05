#![allow(
    clippy::disallowed_methods,
    reason = "bench target: the wall clock is the instrument, not cell code"
)]
//! M4-S07 foreground-protection storm (§4.1: "foreground p99.9 < 2 ms
//! during sustained demotion storm" — the M1-S05 storm pattern applied
//! to demotion). Foreground ops (GET/SET mix over a tiered table) run
//! while the demotion MAINTAIN loop — seal slices, the S11 flush
//! pipeline, and page-releasing head advances — churns continuously
//! under sustained budget pressure; every foreground op is timed
//! individually and the split histogram is the artifact.
//!
//! **Substrate (M4-S11):** the flush leg is the real `TierFlush`
//! pipeline (rotation, footers, fdatasync barriers, watermark
//! confirmation — ADR-0056). Default substrate is `MemFs` (dev-tier,
//! disclosed); set `INF_STORM_DIR=<dir-on-nvme>` to run the
//! device-loaded leg on the real filesystem in `Direct` mode (ADR-0054
//! default) — the row the S07 §7 sub-gate and the S22 campaign read.
//!
//! Custom harness (the `store`/`resolver`/`mutation` precedent).
//! Run: `taskset -c 4 cargo bench -p inf-store --bench demotion_storm`
//! Artifact: 3–5 replicates under `.artifacts/m4/s07/` (MemFs) /
//! `.artifacts/m4/s11/` (device-loaded).

use std::path::PathBuf;
use std::time::Instant;

use inf_log::flush::{TierFlush, TierFlushConfig};
use inf_log::fs::mem::MemFs;
use inf_log::fs::{SegmentFs, StdSegmentFs};
use inf_log::{NsId, TierIoMode};
use inf_store::KeyHasher;
use inf_store::{
    AddressSpaceConfig, DemotionConfig, Keyspace, LogicalAddr, StoreConfig, TieredLookup,
    TieredTable,
};

const NS: NsId = NsId(71);
const PAGE: u64 = 4 << 10;
const BUDGET: u64 = 32 << 20;
/// Scenario-sized file capacity so the storm exercises capacity
/// rotation (ADR-0056 D2), not only the shutdown seal.
const FILE_CAPACITY: u64 = 4 << 20;
const KEYS: u64 = 4096;
/// Foreground ops per MAINTAIN round (the reactor's EXECUTE:MAINTAIN
/// cadence at storm rate).
const OPS_PER_ROUND: u64 = 64;
const TOTAL_OPS: u64 = 400_000;

fn seeded(x: &mut u64) -> u64 {
    *x ^= *x << 13;
    *x ^= *x >> 7;
    *x ^= *x << 17;
    *x
}

struct Storm<F: SegmentFs> {
    ks: Keyspace,
    flush: TierFlush<F>,
}

impl<F: SegmentFs> Storm<F> {
    fn table(&mut self) -> &mut TieredTable {
        self.ks.tiered_store_mut(NS).expect("materialized")
    }

    /// The MAINTAIN round: seal slice → flush slices until the sealed
    /// backlog drains (each slice is one write batch + one fdatasync
    /// barrier + a watermark confirm — ADR-0056 D3) → release.
    fn maintain(&mut self) {
        self.ks.demote_tick();
        let flush = &mut self.flush;
        let table = self.ks.tiered_store_mut(NS).expect("materialized");
        loop {
            let outcome = table.flush_slice(flush).expect("flush slice");
            if outcome.appended_bytes == 0 && outcome.gaps_crossed == 0 {
                break;
            }
        }
        self.ks.demote_tick();
    }
}

fn run_storm<F: SegmentFs>(fs: F, shard_dir: PathBuf, mode: TierIoMode, label: &str) {
    let demote = DemotionConfig::for_budget(BUDGET, PAGE);
    let ring = demote.ring_reserve_bytes().expect("valid budget");
    let mut ks = Keyspace::new(StoreConfig::default());
    assert!(
        ks.materialize_tiered(
            NS,
            AddressSpaceConfig {
                reserve_bytes: ring,
                page_bytes: PAGE as usize,
                life_origin: LogicalAddr::ZERO,
            },
            demote,
            KEYS as usize * 2,
        )
        .is_ok()
    );
    let flush = TierFlush::new(
        fs,
        TierFlushConfig {
            shard_dir,
            cell: 0,
            ns: NS,
            mode,
            file_capacity: FILE_CAPACITY,
            slice_bytes: PAGE,
        },
        0,
    );
    let mut storm = Storm { ks, flush };

    // Preload the working set; `lens[i]`/`versions[i]` model each key's
    // current record so cold overwrites stay index-only AND exact.
    let mut seed = 0xDEA1_5701u64;
    let mut lens: Vec<usize> = Vec::with_capacity(KEYS as usize);
    let mut versions: Vec<u32> = vec![0; KEYS as usize];
    for i in 0..KEYS {
        let key = format!("k:{i:06}");
        let value = vec![0x41u8; 64 + (seeded(&mut seed) % 192) as usize];
        let hash = KeyHasher::default().hash(key.as_bytes());
        let table = storm.table();
        let addr = table.insert(key.as_bytes(), &value, hash).expect("fits");
        lens.push(table.record(addr).encoded_len);
    }

    // The storm: 30% SET (relocation-heavy — the demotion feed), 70%
    // GET (RAM answers; cold candidates answer index-only here — the
    // suspension path is S08's, not this row's). Every op timed.
    let mut latencies_ns: Vec<u64> = Vec::with_capacity(TOTAL_OPS as usize);
    let mut cold_hits = 0u64;
    let mut ops = 0u64;
    while ops < TOTAL_OPS {
        for _ in 0..OPS_PER_ROUND {
            ops += 1;
            let idx = (seeded(&mut seed) % KEYS) as usize;
            let key = format!("k:{idx:06}");
            let hash = KeyHasher::default().hash(key.as_bytes());
            let is_set = seeded(&mut seed) % 10 < 3;
            let started = Instant::now();
            if is_set {
                let value = vec![0x42u8; 64 + (seeded(&mut seed) % 192) as usize];
                let table = storm.table();
                // Old len/version from the storm's model — cold records
                // stay index-only on the write path (§3.3), exactly.
                let found = match table.lookup(key.as_bytes(), hash, &[]) {
                    TieredLookup::Ram(addr) | TieredLookup::Cold(addr) => Some(addr),
                    TieredLookup::Miss => None,
                };
                let placed = match found {
                    Some(addr) => {
                        table.update(key.as_bytes(), &value, hash, addr, lens[idx], versions[idx])
                    }
                    None => table.insert(key.as_bytes(), &value, hash),
                }
                .expect("paced storm never hits the budget wall");
                versions[idx] = table.record(placed).version;
                lens[idx] = table.record(placed).encoded_len;
            } else {
                let table = storm.table();
                match table.lookup(key.as_bytes(), hash, &[]) {
                    TieredLookup::Ram(addr) => {
                        std::hint::black_box(table.record(addr).value);
                    }
                    TieredLookup::Cold(_) => cold_hits += 1, // suspend in prod (S08)
                    TieredLookup::Miss => {}
                }
            }
            latencies_ns.push(started.elapsed().as_nanos() as u64);
        }
        let started = Instant::now();
        storm.maintain();
        // MAINTAIN slice wall time rides the same tail budget: a slice
        // that stalls the loop shows up exactly like a slow op.
        latencies_ns.push(started.elapsed().as_nanos() as u64);
    }

    latencies_ns.sort_unstable();
    let pick = |p: f64| {
        latencies_ns[((latencies_ns.len() as f64 * p) as usize).min(latencies_ns.len() - 1)]
    };
    let counters = storm.ks.tiering_counters();
    let files_sealed = storm.flush.sealed().len();
    let report = storm.table().space().report();
    println!("--- M4-S07/S11 demotion storm ({label}) ---");
    println!(
        "ops {TOTAL_OPS} (+ maintain slices) | cold candidates {cold_hits} | demote slices {} | sealed {} B | flush slices {} | flushed {} B | files sealed {files_sealed} | stalls {}",
        counters.demote_slices,
        counters.demote_sealed_bytes,
        counters.flush_slices,
        counters.flush_confirmed_bytes,
        counters.tail_alloc_stalls
    );
    println!(
        "committed {} B (budget {} B + slice {} B) | demoted-to-disk head {}",
        report.committed_bytes,
        BUDGET,
        PAGE,
        storm.table().space().head().to_raw()
    );
    println!(
        "foreground+slice latency: p50 {} ns | p99 {} ns | p99.9 {} ns | max {} ns",
        pick(0.50),
        pick(0.99),
        pick(0.999),
        pick(1.0)
    );
    assert!(report.committed_bytes <= BUDGET + PAGE, "the budget window held under the storm");
    assert_eq!(counters.tail_alloc_stalls, 0, "a paced storm never stalls");
}

fn main() {
    match std::env::var_os("INF_STORM_DIR") {
        Some(dir) => {
            let shard = PathBuf::from(dir).join(format!("inf-storm-{}", std::process::id()));
            std::fs::create_dir_all(&shard).expect("storm dir");
            // The device-loaded leg: real filesystem, Direct mode
            // (ADR-0054 default) — fdatasync stalls bill honestly.
            run_storm(StdSegmentFs, shard.clone(), TierIoMode::Direct, "real-device Direct");
            let _ = std::fs::remove_dir_all(&shard);
        }
        None => run_storm(
            MemFs::new(),
            PathBuf::from("bench/shard-0"),
            TierIoMode::Buffered,
            "MemFs substrate, disclosed",
        ),
    }
}
