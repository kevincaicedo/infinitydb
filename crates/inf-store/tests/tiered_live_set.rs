//! M4-S14 — the live-set exactness storm (plan AC 1, ADR-0058): random
//! write/update/delete ops against a shadow model, with the real flush
//! pipeline filing chunks into real (MemFs) tier files, interleaved
//! seal/flush/release slices, ring wraps, capacity rotations, and
//! backpressure. Two oracles:
//!
//! - **every op:** the per-life aggregate identity — the space's dead
//!   bytes decompose exactly into filed dead + pending dead + ring-top
//!   holes, and `live_bytes + dead = allocated` holds as before;
//! - **periodically and at the end:** the per-file identity the AC
//!   names — for every tier file, `live + dead = file bytes` with live
//!   computed from the model (sum of encoded lengths of live records
//!   whose address the file contains), exact, never approximate.
//!
//! Addresses come from the harness's own bookkeeping (the model tracks
//! each key's current address), so the oracle never depends on the
//! machinery under test beyond the counters it checks.

use std::collections::BTreeMap;
use std::path::Path;

use inf_log::fs::mem::MemFs;
use inf_log::{NsId, TierFlush, TierFlushConfig, TierIoMode};
use inf_store::{AddressSpaceConfig, DemotionConfig, LogicalAddr, TieredLookup, TieredTable};
use proptest::prelude::*;

const NS: NsId = NsId(43);
const PAGE: u64 = 4 << 10;
const BUDGET: u64 = 1 << 20;
const FILE_CAPACITY: u64 = 96 << 10;

fn seeded(x: &mut u64) -> u64 {
    *x ^= *x << 13;
    *x ^= *x >> 7;
    *x ^= *x << 17;
    *x
}

struct Storm {
    table: TieredTable,
    flush: TierFlush<MemFs>,
    /// key → (current address, encoded record length, version).
    model: BTreeMap<u64, (u64, usize, u32)>,
}

impl Storm {
    fn new() -> Storm {
        let demote = DemotionConfig::for_budget(BUDGET, PAGE);
        let table = TieredTable::new(
            AddressSpaceConfig {
                reserve_bytes: demote.ring_reserve_bytes().expect("valid budget"),
                page_bytes: PAGE as usize,
                life_origin: LogicalAddr::ZERO,
            },
            demote,
            2048,
        )
        .expect("ring");
        let flush = TierFlush::new(
            MemFs::new(),
            TierFlushConfig {
                shard_dir: Path::new("shard-0").to_path_buf(),
                cell: 0,
                ns: NS,
                mode: TierIoMode::Buffered,
                file_capacity: FILE_CAPACITY,
                slice_bytes: PAGE,
            },
            0,
        );
        Storm { table, flush, model: BTreeMap::new() }
    }

    fn maintain_round(&mut self) {
        loop {
            let sealed = self.table.seal_slice();
            let f = self.table.flush_slice(&mut self.flush).expect("flush slice");
            let released = self.table.release_slice();
            if sealed + released + f.appended_bytes + u64::from(f.gaps_crossed) == 0 {
                break;
            }
        }
    }

    /// SET through the live-path rules; the model authorizes the old
    /// address/length/version (full-hash sidecar makes the index's
    /// candidate the key's own slot at this corpus size).
    fn set(&mut self, id: u64, value: &[u8]) {
        let key = format!("k:{id:05}").into_bytes();
        let hash = TieredTable::hash_key(&key);
        let placed = match self.model.get(&id) {
            Some(&(old, old_len, old_version)) => {
                let old = LogicalAddr::from_raw(old).expect("48-bit");
                match self.table.update(&key, value, hash, old, old_len, old_version) {
                    Ok(addr) => addr,
                    Err(_) => {
                        self.maintain_round();
                        self.table
                            .update(&key, value, hash, old, old_len, old_version)
                            .expect("fits after maintain")
                    }
                }
            }
            None => match self.table.insert(&key, value, hash) {
                Ok(addr) => addr,
                Err(_) => {
                    self.maintain_round();
                    self.table.insert(&key, value, hash).expect("fits after maintain")
                }
            },
        };
        let (len, version) = match self.table.lookup(&key, hash, &[]) {
            TieredLookup::Ram(addr) => {
                assert_eq!(addr, placed);
                let parts = self.table.record(addr);
                (parts.encoded_len, parts.version)
            }
            _ => unreachable!("a fresh write is RAM-resident"),
        };
        self.model.insert(id, (placed.to_raw(), len, version));
    }

    /// DEL: index + accounting only for cold records (§3.3) — the model
    /// supplies the length, exactly like the TTL wheel will.
    fn del(&mut self, id: u64) {
        let Some((addr, len, _)) = self.model.remove(&id) else { return };
        let key = format!("k:{id:05}").into_bytes();
        let hash = TieredTable::hash_key(&key);
        self.table.delete(hash, LogicalAddr::from_raw(addr).expect("48-bit"), len);
    }

    /// The every-op aggregate identity: the space's per-life dead bytes
    /// decompose exactly into {filed dead} + {pending dead} + {ring-top
    /// holes} — one unattributed byte anywhere breaks it immediately.
    fn assert_aggregates(&self) {
        let live_set = self.table.live_set();
        let filed_dead: u64 = live_set.files().iter().map(|f| f.dead_bytes).sum();
        let space = self.table.space();
        assert_eq!(
            space.report().dead_bytes,
            filed_dead + live_set.pending_dead_bytes() + space.counters().seal_hole_bytes,
            "space dead bytes decompose exactly across the live set"
        );
        assert_eq!(
            self.table.live_bytes() + space.report().dead_bytes,
            space.report().allocated_bytes,
            "live + dead = allocated (the S05/S06 identity, preserved)"
        );
    }

    /// The AC's per-file identity: `live + dead = file bytes`, with live
    /// computed from the model — exact for every file, every time.
    fn assert_per_file_exact(&self) {
        let mut model_live: BTreeMap<u32, u64> = BTreeMap::new();
        let files = self.table.live_set().files();
        for &(addr, len, _) in self.model.values() {
            if let Some(file) =
                files.iter().find(|f| addr >= f.base && addr + len as u64 <= f.base + f.data_len)
            {
                *model_live.entry(file.id).or_default() += len as u64;
            }
        }
        for file in files {
            assert!(file.byte_exact && !file.recovered, "single-life files are byte-exact");
            let live = model_live.get(&file.id).copied().unwrap_or(0);
            assert_eq!(
                live + file.dead_bytes,
                file.data_len,
                "file {}: live {} + dead {} must equal file bytes {}",
                file.id,
                live,
                file.dead_bytes,
                file.data_len
            );
            assert_eq!(file.live_bytes(), Some(live), "the exposed live figure agrees");
        }
    }
}

fn run_storm(seed: u64, ops: u64) {
    let mut storm = Storm::new();
    let mut rng = seed | 1;
    for op in 0..ops {
        match seeded(&mut rng) % 100 {
            0..=54 => {
                let id = seeded(&mut rng) % 1024;
                let len = 20 + (seeded(&mut rng) % 160) as usize;
                let value = vec![(seeded(&mut rng) % 251) as u8; len];
                storm.set(id, &value);
            }
            55..=69 => {
                let id = seeded(&mut rng) % 1024;
                storm.del(id);
            }
            70..=79 => {
                storm.table.seal_slice();
            }
            80..=89 => {
                storm.table.flush_slice(&mut storm.flush).expect("flush slice");
            }
            _ => {
                storm.table.release_slice();
            }
        }
        storm.assert_aggregates();
        if op % 4096 == 0 {
            storm.assert_per_file_exact();
        }
    }
    // Drain the pipeline completely, then the full oracle one last time:
    // every pending span filed, every file exact.
    storm.maintain_round();
    storm.table.flush_drain(&mut storm.flush).expect("drain");
    storm.assert_aggregates();
    storm.assert_per_file_exact();
    // Coverage claims only at AC scale — a 2k-op CI storm legally stays
    // inside one file and never wraps the ring.
    if ops >= 1_000_000 {
        assert!(
            storm.table.live_set().files().len() > 3,
            "the storm must rotate through multiple tier files to prove anything"
        );
        assert!(
            storm.table.space().counters().seal_holes > 0,
            "the storm must wrap the ring so hole exclusion is exercised"
        );
    }
}

proptest! {
    /// Seed-fuzzed storms at CI scale; the 10⁶-op named storm below is
    /// the AC row.
    #[test]
    fn live_set_matches_model(seed: u64) {
        run_storm(seed, 2_000);
    }
}

/// The M4-S14 AC storm: 10⁶ random write/update/delete ops — per-file
/// `live + dead = file bytes`, exact, with the aggregate decomposition
/// asserted after every single op.
#[test]
fn live_set_storm_million_ops() {
    let ops = if cfg!(miri) { 2_000 } else { 1_000_000 };
    run_storm(0x14E_5EED, ops);
}
