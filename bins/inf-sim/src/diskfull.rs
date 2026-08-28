//! `m4-diskfull` (M4-S21, ADR-0063): the disk-budget admission scenario —
//! fail writes, never corrupt, never silently drop, and recover
//! automatically when space frees.
//!
//! A seeded churn (fresh inserts interleaved with overwrites — ADR-0062
//! D9: survivors stay interleaved or the compaction leg is vacuous) runs
//! against a tiered table with a small `DISK-BUDGET` over the simulated
//! disk, driving the real MAINTAIN round shape: demote legs → flush →
//! release → **admission refresh** → compaction under the composed
//! pressure signal → retirement → unlink. Three phases per seed:
//!
//! 1. **Fill** — churn until the first typed `DISKFULL` refusal. Oracle:
//!    the composed pressure input is active at that moment (ADR-0063 D3).
//! 2. **At-cap churn** — writes keep arriving: refused ones must mutate
//!    **nothing** (tail, live bytes, user-byte accounting — snapshot
//!    compared per refusal); deletes still apply; compaction relocates
//!    into the D3 reserve and retirement returns space, so admission
//!    oscillates and some writes land.
//! 3. **Relief** — a seeded mass delete, then MAINTAIN rounds only:
//!    admission must reopen within a bounded number of rounds (recovery
//!    is automatic — no operator step is even representable here).
//!
//! Standing oracles: `disk_used ≤ budget` at every round's observation
//! point, and the final audit reads **every** surviving key back
//! byte-exact — RAM-resident from the table, cold from the simulated
//! device's actual CRC-checked bytes. Every event folds into
//! `trace_hash`; `--verify-determinism` requires two-run identity (L7).
//! (Power-cut interplay stays with `m4-recovery` — this scenario owns
//! admission behavior; the device-ENOSPC latch legs live in the
//! crash-matrix rows, where the fault points are.)

use std::collections::BTreeMap;
use std::path::PathBuf;

use inf_foundation::hash64;
use inf_foundation::rng::{Entropy, SplitMix64};
use inf_log::flush::unlink_tier_file;
use inf_log::fs::sim::SimDisk;
use inf_log::{
    TIER_FRAME_BYTES, TierFlush, TierFlushConfig, TierIoMode, tier_extract, tier_frame_offset,
    tier_frame_span,
};
use inf_store::{
    AddressSpaceConfig, CompactionConfig, CompactionWork, DemotionConfig, DiskFullCause, KeyHasher,
    Keyspace, LogicalAddr, NsId, OpError, StoreConfig, TieredLookup, TieredTable,
};

const NS: NsId = NsId(63);
const PAGE: u64 = 4 << 10;
const MEM_BUDGET: u64 = 256 << 10;
const DISK_BUDGET: u64 = 2 << 20;
/// Fresh-key weight in the churn (1-in-4; the rest overwrite — D9).
const FRESH_ONE_IN: u64 = 4;
const AT_CAP_OPS: u64 = 1_500;
const RELIEF_ROUND_LIMIT: u64 = 512;
const FILL_OP_LIMIT: u64 = 100_000;

/// Scenario knobs (the DSL v0 shape).
#[derive(Debug)]
pub struct DiskfullScenario {
    pub seed: u64,
}

impl DiskfullScenario {
    #[must_use]
    pub fn m4_diskfull(seed: u64) -> DiskfullScenario {
        DiskfullScenario { seed }
    }
}

/// (value bytes, encoded record len) — one modeled record.
type ModelEntry = (Vec<u8>, usize);

#[derive(Debug, Default)]
pub struct DiskfullReport {
    pub violations: Vec<String>,
    pub refusals: u64,
    pub reopens: u64,
    pub relocated_bytes: u64,
    pub retired_files: u64,
    pub peak_disk_used: u64,
    pub keys_verified: u64,
    pub trace_hash: u64,
}

impl DiskfullReport {
    #[must_use]
    pub fn ok(&self) -> bool {
        self.violations.is_empty()
    }
}

struct World {
    ks: Keyspace,
    flush: TierFlush<SimDisk>,
    disk: SimDisk,
    /// key → (value bytes, encoded record len) — the content model.
    model: BTreeMap<Vec<u8>, ModelEntry>,
    rng: SplitMix64,
    ckpt_id: u64,
    report: DiskfullReport,
}

impl World {
    fn new(seed: u64) -> World {
        let mut ks =
            Keyspace::new(StoreConfig { hasher: KeyHasher::from_seed(seed), ..Default::default() });
        let demote = DemotionConfig {
            mem_budget_bytes: MEM_BUDGET,
            mutable_permille: 250,
            slice_bytes: PAGE,
        };
        ks.materialize_tiered(
            NS,
            AddressSpaceConfig {
                reserve_bytes: demote.ring_reserve_bytes().expect("valid budget"),
                page_bytes: PAGE as usize,
                life_origin: LogicalAddr::ZERO,
            },
            demote,
            1024,
        )
        .expect("materialize");
        let table = ks.tiered_store_mut(NS).expect("materialized");
        table.set_compaction_config(CompactionConfig { dead_ratio_pct: 50, slice_bytes: PAGE });
        table.set_disk_budget(DISK_BUDGET);
        let disk = SimDisk::new();
        let flush = TierFlush::new(
            disk.clone(),
            TierFlushConfig {
                shard_dir: PathBuf::from("node/shard-0"),
                cell: 0,
                ns: NS,
                mode: TierIoMode::Buffered,
                file_capacity: 16 * PAGE,
                slice_bytes: PAGE,
            },
            0,
        );
        World {
            ks,
            flush,
            disk,
            model: BTreeMap::new(),
            rng: SplitMix64::new(seed ^ 0xD15C_F011),
            ckpt_id: 0,
            report: DiskfullReport::default(),
        }
    }

    fn table(&mut self) -> &mut TieredTable {
        self.ks.tiered_store_mut(NS).expect("materialized")
    }

    fn fold(&mut self, tag: u64, value: u64) {
        self.report.trace_hash = hash64(&value.to_le_bytes(), self.report.trace_hash ^ tag);
    }

    fn violation(&mut self, text: String) {
        if self.report.violations.len() < 16 {
            self.report.violations.push(text);
        }
    }

    fn value_for(key_id: u64, generation: u64) -> Vec<u8> {
        let len = 1024 + ((key_id.wrapping_mul(31) ^ generation) % 512) as usize;
        (0..len).map(|i| (i as u64 ^ key_id.wrapping_mul(7) ^ generation) as u8).collect()
    }

    /// One MAINTAIN round in the ADR-0063 D2 shape: demote legs run to
    /// their due-target, then the admission refresh where both usage
    /// halves are fresh, then the cap oracle.
    fn maintain_round(&mut self) -> u64 {
        let mut total = 0u64;
        for _ in 0..64 {
            let table = self.ks.tiered_store_mut(NS).expect("materialized");
            let sealed = table.seal_slice();
            let outcome = {
                let flush = &mut self.flush;
                table.flush_slice(flush).expect("sim flush slice")
            };
            let released = table.release_slice();
            let step = sealed + released + outcome.appended_bytes + u64::from(outcome.gaps_crossed);
            total += step;
            if step == 0 || !table.demote_due() {
                break;
            }
        }
        let tier_bytes = self.flush.disk_bytes();
        let table = self.ks.tiered_store_mut(NS).expect("materialized");
        table.refresh_disk_admission(tier_bytes);
        let used = table.disk_used(tier_bytes);
        self.report.peak_disk_used = self.report.peak_disk_used.max(used);
        if used > DISK_BUDGET {
            self.violation(format!("cap oracle: disk_used {used} > budget {DISK_BUDGET}"));
        }
        total
    }

    /// One compaction slice under the composed pressure input, then the
    /// retirement pipeline (walk stamp → retire scan → manifest
    /// exclusion → commit + detach + unlink).
    fn compact_and_retire(&mut self) {
        let pressure = {
            let tier_bytes = self.flush.disk_bytes();
            self.table().compaction_pressure(tier_bytes)
        };
        let mut spent = 0u64;
        while spent < PAGE {
            let work = {
                let flush = &self.flush;
                self.ks.tiered_store_mut(NS).expect("materialized").compaction_work(
                    flush,
                    pressure,
                    PAGE - spent,
                )
            };
            let CompactionWork::Read { file_id, addr, len } = work else { break };
            let Some(bytes) = self.read_span(file_id, addr.to_raw(), len as usize) else { break };
            let applied = self.table().compaction_apply(file_id, addr, &bytes);
            spent += applied.consumed.max(applied.need).max(1);
            self.report.relocated_bytes += applied.relocated_bytes;
            if applied.stalled {
                break;
            }
        }
        self.ckpt_id += 1;
        let ckpt = self.ckpt_id;
        let table = self.ks.tiered_store_mut(NS).expect("materialized");
        table.begin_ckpt_walk(ckpt);
        table.end_ckpt_walk();
        {
            let flush = &self.flush;
            table.retire_scan(ckpt, flush);
            let _section = table.tier_manifest(NS.0, flush);
        }
        let ids = table.commit_retirement();
        for &id in &ids {
            let meta = self.flush.detach_sealed(id).expect("retired files are sealed");
            unlink_tier_file(&self.disk, &meta).expect("sim unlink");
            self.report.retired_files += 1;
        }
    }

    /// One churn op. `Ok(true)` = landed, `Ok(false)` = typed `DISKFULL`
    /// (purity-checked), `Err` = the memory window needs a round.
    fn churn_op(&mut self, next_fresh: &mut u64) -> Result<bool, ()> {
        let fresh = *next_fresh < 8 || self.rng.next_below(FRESH_ONE_IN) == 0;
        let key_id = if fresh { *next_fresh } else { self.rng.next_below(*next_fresh) };
        let generation = self.rng.next_below(1 << 20) + 1;
        let key = format!("k:{key_id:06}").into_bytes();
        let value = Self::value_for(key_id, generation);
        let hash = self.table().hash_key(&key);
        let found = match self.table().lookup(&key, hash, &[]) {
            TieredLookup::Ram(addr) => {
                let parts = self.table().record(addr);
                Some((addr, parts.encoded_len, parts.version))
            }
            TieredLookup::Cold(addr) => {
                let len = self.model.get(&key).expect("cold key is modeled").1;
                let _ = self.table().take_displacement_origins(hash, addr);
                Some((addr, len, 0))
            }
            TieredLookup::Miss => None,
        };
        // The refusal-purity snapshot (ADR-0063 D1: refusal mutates
        // nothing).
        let (tail0, live0, user0) = {
            let table = self.table();
            (table.space().tail().to_raw(), table.live_bytes(), table.write_accounting().user_bytes)
        };
        let result = match found {
            Some((addr, len, version)) => {
                self.table().update(&key, &value, hash, addr, len, version)
            }
            None => self.table().insert(&key, &value, hash),
        };
        match result {
            Ok(placed) => {
                let encoded = self.table().record(placed).encoded_len;
                self.model.insert(key, (value, encoded));
                if fresh {
                    *next_fresh += 1;
                }
                self.fold(0x0B, placed.to_raw());
                Ok(true)
            }
            Err(OpError::DiskFull(cause)) => {
                let table = self.table();
                if table.space().tail().to_raw() != tail0
                    || table.live_bytes() != live0
                    || table.write_accounting().user_bytes != user0
                {
                    self.violation("refusal purity: a DISKFULL refusal mutated state".into());
                }
                self.report.refusals += 1;
                self.fold(0xDF, matches!(cause, DiskFullCause::Device) as u64);
                Ok(false)
            }
            Err(OpError::OutOfMemory) => Err(()),
            Err(e) => {
                self.violation(format!("unexpected refusal {e:?}"));
                Ok(false)
            }
        }
    }

    fn delete_key(&mut self, key: Vec<u8>) {
        let Some((_, len)) = self.model.remove(&key) else { return };
        let hash = self.table().hash_key(&key);
        let addr = match self.table().lookup(&key, hash, &[]) {
            TieredLookup::Ram(addr) | TieredLookup::Cold(addr) => addr,
            TieredLookup::Miss => {
                self.violation("model key missing from the table".into());
                return;
            }
        };
        let _ = self.table().take_displacement_origins(hash, addr);
        self.table().delete(hash, addr, len);
        self.fold(0xDE, addr.to_raw());
    }

    /// Reads `len` bytes at `addr` out of the simulated device's actual
    /// bytes (CRC-verified) — the compaction feed and the final audit's
    /// cold path.
    fn read_span(&self, file_id: u32, addr: u64, len: usize) -> Option<Vec<u8>> {
        let meta = self.flush.sealed().iter().find(|m| m.id == file_id)?;
        let image = self.disk.contents(&meta.path)?;
        let (first, count, skip) = tier_frame_span(addr - meta.base.to_raw(), len);
        let from = tier_frame_offset(first) as usize;
        let to = from + count as usize * TIER_FRAME_BYTES;
        let mut out = Vec::new();
        tier_extract(image.get(from..to)?, skip, len, &mut out).ok()?;
        Some(out)
    }

    fn read_cold_by_containment(&self, addr: u64, len: usize) -> Option<Vec<u8>> {
        let meta = self.flush.sealed().iter().find(|m| {
            addr >= m.base.to_raw() && addr + len as u64 <= m.base.to_raw() + m.data_len
        })?;
        self.read_span(meta.id, addr, len)
    }

    /// The final audit: every surviving key reads back byte-exact.
    fn verify_content(&mut self) {
        {
            let flush = &mut self.flush;
            let table = self.ks.tiered_store_mut(NS).expect("materialized");
            table.flush_drain(flush).expect("final drain");
        }
        let model: Vec<(Vec<u8>, ModelEntry)> =
            self.model.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        for (key, (value, len)) in model {
            let hash = self.table().hash_key(&key);
            let ok = match self.table().lookup(&key, hash, &[]) {
                TieredLookup::Ram(addr) => {
                    let parts = self.table().record(addr);
                    parts.key == &key[..] && parts.value == &value[..]
                }
                TieredLookup::Cold(addr) => {
                    match self.read_cold_by_containment(addr.to_raw(), len) {
                        Some(bytes) => {
                            let parts = TieredTable::decode_record(&bytes);
                            parts.key == &key[..] && parts.value == &value[..]
                        }
                        None => false,
                    }
                }
                TieredLookup::Miss => false,
            };
            if ok {
                self.report.keys_verified += 1;
                self.fold(0xC0, hash);
            } else {
                self.violation(format!(
                    "content oracle: key {:?} lost or corrupt",
                    String::from_utf8_lossy(&key)
                ));
            }
        }
    }
}

/// Runs the scenario to its report (deterministic per seed — L7).
#[must_use]
pub fn run_diskfull_scenario(scenario: &DiskfullScenario) -> DiskfullReport {
    let mut w = World::new(scenario.seed);
    let mut next_fresh = 0u64;

    // Phase 1 — fill to the first refusal.
    let mut ops = 0u64;
    loop {
        match w.churn_op(&mut next_fresh) {
            Ok(true) => {}
            Ok(false) => break,
            Err(()) => {
                if w.maintain_round() == 0 {
                    w.violation("fill wedged: the memory window never drained".into());
                    break;
                }
            }
        }
        ops += 1;
        if ops > FILL_OP_LIMIT {
            w.violation("fill never reached the budget".into());
            break;
        }
    }
    let tier_bytes = w.flush.disk_bytes();
    if !w.table().compaction_pressure(tier_bytes) {
        w.violation("composed pressure inactive at the first refusal (ADR-0063 D3)".into());
    }

    // Phase 2 — churn at the cap: refusals stay pure, deletes apply,
    // compaction + retirement oscillate admission.
    let mut was_closed = w.table().disk_full().is_some();
    for i in 0..AT_CAP_OPS {
        match w.churn_op(&mut next_fresh) {
            Ok(_) => {}
            Err(()) => {
                let _ = w.maintain_round();
            }
        }
        if i.is_multiple_of(8) {
            let keys: Vec<Vec<u8>> = w.model.keys().cloned().collect();
            if !keys.is_empty() {
                let pick = w.rng.next_below(keys.len() as u64) as usize;
                w.delete_key(keys[pick].clone());
            }
        }
        if i.is_multiple_of(4) {
            w.compact_and_retire();
            let _ = w.maintain_round();
        }
        let closed = w.table().disk_full().is_some();
        if was_closed && !closed {
            w.report.reopens += 1;
            w.fold(0x40, i);
        }
        was_closed = closed;
    }

    // Phase 3 — relief: a seeded mass delete, then MAINTAIN only;
    // admission must reopen with no operator step.
    let keys: Vec<Vec<u8>> = w.model.keys().cloned().collect();
    for key in keys {
        if w.rng.next_below(2) == 0 {
            w.delete_key(key);
        }
    }
    let mut reopened = false;
    for _ in 0..RELIEF_ROUND_LIMIT {
        w.compact_and_retire();
        let _ = w.maintain_round();
        if w.table().disk_full().is_none() {
            reopened = true;
            break;
        }
    }
    if !reopened {
        w.violation(format!("relief: admission never reopened within {RELIEF_ROUND_LIMIT} rounds"));
    } else {
        w.report.reopens += 1;
    }

    // Final audit.
    w.verify_content();
    let refusals = w.report.refusals;
    if refusals == 0 {
        w.violation("vacuous run: the budget never refused".into());
    }
    w.fold(0xF1, refusals);
    let keys = w.report.keys_verified;
    w.fold(0xF2, keys);
    w.report
}
