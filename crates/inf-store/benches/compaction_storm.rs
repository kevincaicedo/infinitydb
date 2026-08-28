//! M4-S15 foreground-protection storm (§4.1: "foreground p99.9 < 2 ms
//! with compaction running at full slice budget" — the S07 storm with
//! the copy-forward leg live). Foreground GET/SET ops run while the
//! MAINTAIN round seals, flushes, releases, **and compacts at the full
//! 1 MiB slice budget**, with retirement + unlink cycling every few
//! rounds; every foreground op and every MAINTAIN round is timed into
//! one histogram — a slice that stalls the loop shows up exactly like
//! a slow op.
//!
//! The publication itself (manifest swap fsync) is checkpoint-domain
//! cost and is not billed to this row; the retirement bookkeeping,
//! detach, and unlink are S15 costs and are billed.
//!
//! Default substrate `MemFs` (dev-tier, disclosed); set
//! `INF_STORM_DIR=<dir-on-nvme>` for the device-loaded leg.
//! Run: `taskset -c 4 cargo bench -p inf-store --bench compaction_storm`
//! Artifact: 3–5 replicates under `.artifacts/m4/s15/`.

use std::path::PathBuf;
use std::time::Instant;

use inf_log::flush::{TierFlush, TierFlushConfig, unlink_tier_file};
use inf_log::fs::mem::MemFs;
use inf_log::fs::{SegmentFile, SegmentFs, StdSegmentFs};
use inf_log::{
    NsId, TIER_FRAME_BYTES, TierIoMode, tier_extract, tier_frame_offset, tier_frame_span,
};
use inf_store::KeyHasher;
use inf_store::{
    AddressSpaceConfig, CompactionWork, DemotionConfig, Keyspace, LogicalAddr, StoreConfig,
    TieredLookup, TieredTable,
};

const NS: NsId = NsId(73);
const PAGE: u64 = 4 << 10;
const BUDGET: u64 = 32 << 20;
const FILE_CAPACITY: u64 = 4 << 20;
const KEYS: u64 = 4096;
const OPS_PER_ROUND: u64 = 64;
const TOTAL_OPS: u64 = 400_000;
/// The ADR-0059 D6 default — the "full slice budget" the gate names.
const COMPACT_SLICE: u64 = 1 << 20;
/// Rounds between publication cycles (retirement + unlink cadence).
const ROUNDS_PER_PUBLISH: u64 = 64;

fn seeded(x: &mut u64) -> u64 {
    *x ^= *x << 13;
    *x ^= *x >> 7;
    *x ^= *x << 17;
    *x
}

struct Storm<F: SegmentFs> {
    ks: Keyspace,
    fs: F,
    flush: TierFlush<F>,
    relocated: u64,
    retired: u64,
    unlinked: u64,
}

impl<F: SegmentFs> Storm<F> {
    fn table(&mut self) -> &mut TieredTable {
        self.ks.tiered_store_mut(NS).expect("materialized")
    }

    /// One chunk of a sealed candidate, CRC-verified (the S08 read
    /// modeled synchronously — device latency bills into the round).
    fn read_chunk(&self, file_id: u32, addr: u64, len: u64) -> Vec<u8> {
        let meta = self
            .flush
            .sealed()
            .iter()
            .find(|m| m.id == file_id)
            .expect("candidates are sealed")
            .clone();
        let file = self.fs.open_read(&meta.path).expect("tier file opens");
        let len = usize::try_from(len).expect("fits");
        let (first, count, skip) = tier_frame_span(addr - meta.base.to_raw(), len);
        let from = tier_frame_offset(first);
        let span = count as usize * TIER_FRAME_BYTES;
        let mut window = vec![0u8; span];
        let mut done = 0usize;
        while done < span {
            let n = file.read_at(from + done as u64, &mut window[done..]).expect("read");
            assert!(n > 0, "short tier file");
            done += n;
        }
        let mut out = Vec::new();
        tier_extract(&window, skip, len, &mut out).expect("CRC-clean");
        out
    }

    /// The MAINTAIN round with the S15 leg: seal → flush → release →
    /// one full compaction slice (ADR-0059 D6 ordering — the window
    /// drains before cold bytes copy back in).
    fn maintain(&mut self) {
        self.ks.demote_tick();
        {
            let flush = &mut self.flush;
            let table = self.ks.tiered_store_mut(NS).expect("materialized");
            loop {
                let outcome = table.flush_slice(flush).expect("flush slice");
                if outcome.appended_bytes == 0 && outcome.gaps_crossed == 0 {
                    break;
                }
            }
        }
        self.ks.demote_tick();
        let mut spent = 0u64;
        while spent < COMPACT_SLICE {
            let work = {
                let flush = &self.flush;
                self.ks.tiered_store_mut(NS).expect("materialized").compaction_work(
                    flush,
                    false,
                    COMPACT_SLICE - spent,
                )
            };
            match work {
                CompactionWork::Read { file_id, addr, len } => {
                    let chunk = self.read_chunk(file_id, addr.to_raw(), len);
                    let applied = self.table().compaction_apply(file_id, addr, &chunk);
                    self.relocated += u64::from(applied.relocated);
                    spent += applied.consumed.max(1);
                    if applied.stalled {
                        break; // refusal-aware: the slice ends (D6)
                    }
                }
                CompactionWork::Idle => break,
            }
        }
    }

    /// Retirement cycle (manifest swap cost excluded — checkpoint
    /// domain; the bookkeeping, detach, and unlink are S15's and bill).
    fn publish_cycle(&mut self, ckpt_id: u64) {
        let table = self.ks.tiered_store_mut(NS).expect("materialized");
        table.begin_ckpt_walk(ckpt_id);
        table.end_ckpt_walk();
        table.retire_scan(ckpt_id, &self.flush);
        let ids = table.commit_retirement();
        for id in ids {
            let meta = self.flush.detach_sealed(id).expect("retired files are sealed");
            unlink_tier_file(&self.fs, &meta).expect("unlink");
            self.retired += 1;
            self.unlinked += 1;
        }
    }
}

fn run_storm<F: SegmentFs + Clone>(fs: F, shard_dir: PathBuf, mode: TierIoMode, label: &str) {
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
        fs.clone(),
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
    let mut storm = Storm { ks, fs, flush, relocated: 0, retired: 0, unlinked: 0 };

    let mut seed = 0x515_5701u64;
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

    // 30% SET / 70% GET. Writes are skewed (85% land on the hottest
    // 20% of keys) so cold files hold long-lived live survivors — the
    // dead-ratio trigger then forces real copy-forward, not just
    // is_dead retirement of fully-churned files.
    let mut latencies_ns: Vec<u64> = Vec::with_capacity(TOTAL_OPS as usize);
    let mut cold_hits = 0u64;
    let mut ops = 0u64;
    let mut rounds = 0u64;
    let mut ckpt_id = 0u64;
    while ops < TOTAL_OPS {
        for _ in 0..OPS_PER_ROUND {
            ops += 1;
            let is_set = seeded(&mut seed) % 10 < 3;
            let idx = if is_set && seeded(&mut seed) % 100 < 85 {
                (seeded(&mut seed) % (KEYS / 5)) as usize
            } else {
                (seeded(&mut seed) % KEYS) as usize
            };
            let key = format!("k:{idx:06}");
            let hash = KeyHasher::default().hash(key.as_bytes());
            let started = Instant::now();
            if is_set {
                let value = vec![0x42u8; 64 + (seeded(&mut seed) % 192) as usize];
                let table = storm.table();
                let found = match table.lookup(key.as_bytes(), hash, &[]) {
                    TieredLookup::Ram(addr) | TieredLookup::Cold(addr) => Some(addr),
                    TieredLookup::Miss => None,
                };
                let placed = match found {
                    Some(addr) => {
                        // The D9 origin markers would stage here in prod;
                        // the take models that cost.
                        let _ = table.take_displacement_origins(hash, addr);
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
        rounds += 1;
        let started = Instant::now();
        storm.maintain();
        if rounds.is_multiple_of(ROUNDS_PER_PUBLISH) {
            ckpt_id += 1;
            storm.publish_cycle(ckpt_id);
        }
        latencies_ns.push(started.elapsed().as_nanos() as u64);
    }

    latencies_ns.sort_unstable();
    let pick = |p: f64| {
        latencies_ns[((latencies_ns.len() as f64 * p) as usize).min(latencies_ns.len() - 1)]
    };
    let counters = storm.ks.tiering_counters();
    let report = storm.ks.tiered_store_mut(NS).expect("materialized").space().report();
    let acct = storm.ks.tiering_write_accounting();
    println!("--- M4-S15 compaction storm ({label}) ---");
    println!(
        "ops {TOTAL_OPS} (+ maintain rounds) | cold candidates {cold_hits} | compact slices {} \
         | relocated {} records / {} B | files retired {} | unlinked {} | cold floor {}",
        counters.compact_slices,
        storm.relocated,
        acct.compaction_bytes,
        storm.retired,
        storm.unlinked,
        storm.ks.tiered_store_mut(NS).expect("materialized").cold_floor(),
    );
    println!(
        "committed {} B (budget {} B) | stalls {} | flush slices {} | demote slices {}",
        report.committed_bytes,
        BUDGET,
        counters.tail_alloc_stalls,
        counters.flush_slices,
        counters.demote_slices,
    );
    println!(
        "foreground+slice latency: p50 {} ns | p99 {} ns | p99.9 {} ns | max {} ns",
        pick(0.50),
        pick(0.99),
        pick(0.999),
        pick(1.0)
    );
    assert!(storm.relocated > 0, "the storm compacted for real (coverage, not silence)");
    assert!(storm.unlinked > 0, "the storm reclaimed files for real");
    assert!(report.committed_bytes <= BUDGET + PAGE, "the budget window held under the storm");
    let p999 = pick(0.999);
    assert!(
        p999 < 2_000_000,
        "foreground p99.9 {p999} ns breaches the 2 ms gate at full slice budget"
    );
}

fn main() {
    match std::env::var_os("INF_STORM_DIR") {
        Some(dir) => {
            let shard = PathBuf::from(dir).join(format!("inf-cstorm-{}", std::process::id()));
            std::fs::create_dir_all(&shard).expect("storm dir");
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
