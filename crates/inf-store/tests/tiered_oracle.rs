//! M4-S02 AC: tiered store/lookup/delete vs a HashMap oracle across
//! simulated region migrations — zero misses, zero stales. Records
//! migrate mutable → read-only → cold underneath live traffic (watermark
//! advances at record boundaries; a simulated tier store captures record
//! bytes at flush time, playing S11's role), and every lookup answer is
//! checked for location class *and* content. Cold candidates exercise the
//! fetch-verify-retry contract, including fingerprint false positives.
//!
//! Campaign row: `cargo test -p inf-store --release --test tiered_oracle`
//! runs the 10⁶-op storm; the proptest wrapper fuzzes seeds at CI scale.

use std::collections::HashMap;

use inf_store::{
    AddressSpaceConfig, DemotionConfig, LogicalAddr, OpError, TieredLookup, TieredTable,
};
use proptest::prelude::*;

const RING: u64 = 1 << 20;
const PAGE: u64 = 1 << 12;
/// Fetch-verify-retry bound: 22-bit fingerprints make even one retry
/// ≈2⁻²² per probe; four nested collisions would be a hash bug.
const MAX_COLD_RETRIES: usize = 4;

struct OracleEntry {
    value: Vec<u8>,
    addr: u64,
    version: u32,
}

struct Harness {
    table: TieredTable,
    oracle: HashMap<Vec<u8>, OracleEntry>,
    /// Every allocation this life, in address order: (addr, len).
    log: Vec<(u64, usize)>,
    /// Simulated tier store: record bytes captured when `flushed` passed
    /// them (S11's flush, played by the test).
    tier: HashMap<u64, Vec<u8>>,
    /// First `log` entry not yet captured into `tier`.
    flush_cursor: usize,
}

/// The verified outcome of driving a lookup to ground truth: the caller's
/// side of the L6 contract, resolved synchronously by the test.
struct Found {
    addr: u64,
    len: usize,
    version: u32,
    value: Vec<u8>,
    was_cold: bool,
}

impl Harness {
    fn new() -> Harness {
        Harness {
            table: TieredTable::new(
                AddressSpaceConfig {
                    reserve_bytes: RING as usize,
                    page_bytes: PAGE as usize,
                    life_origin: LogicalAddr::ZERO,
                },
                DemotionConfig::for_budget(RING, PAGE),
                64,
            )
            .expect("reservation"),
            oracle: HashMap::new(),
            log: Vec::new(),
            tier: HashMap::new(),
            flush_cursor: 0,
        }
    }

    /// Drives `lookup` through the fetch-verify-retry contract until it
    /// grounds out (verified hit or miss).
    fn find(&self, key: &[u8]) -> Option<Found> {
        let hash = TieredTable::hash_key(key);
        self.table.prefetch(hash);
        self.table.prefetch_candidate(hash); // skips cold candidates
        let mut exclude: Vec<LogicalAddr> = Vec::new();
        loop {
            assert!(exclude.len() <= MAX_COLD_RETRIES, "fingerprint collision storm");
            match self.table.lookup(key, hash, &exclude) {
                TieredLookup::Ram(addr) => {
                    let parts = self.table.record(addr);
                    assert_eq!(parts.key, key, "Ram answers are pre-verified");
                    return Some(Found {
                        addr: addr.to_raw(),
                        len: parts.encoded_len,
                        version: parts.version,
                        value: parts.value.to_vec(),
                        was_cold: false,
                    });
                }
                TieredLookup::Cold(addr) => {
                    // §3.1: cold ⇒ below head ⇒ below flushed ⇒ captured.
                    let bytes = self.tier.get(&addr.to_raw()).expect("cold implies flushed");
                    let parts = TieredTable::decode_record(bytes);
                    if parts.key == key {
                        return Some(Found {
                            addr: addr.to_raw(),
                            len: parts.encoded_len,
                            version: parts.version,
                            value: parts.value.to_vec(),
                            was_cold: true,
                        });
                    }
                    exclude.push(addr); // false positive: retry excluding it
                }
                TieredLookup::Miss => return None,
            }
        }
    }

    fn set(&mut self, key: &[u8], value: &[u8]) {
        let hash = TieredTable::hash_key(key);
        let existing = self.find(key);
        let result = match &existing {
            Some(found) => self.table.overwrite(
                key,
                value,
                hash,
                LogicalAddr::from_raw(found.addr).expect("fits"),
                found.len,
                found.version,
            ),
            None => self.table.insert(key, value, hash),
        };
        let addr = match result {
            Ok(addr) => addr,
            Err(OpError::OutOfMemory) => {
                // Ring window full: migrate everything cold (flush-progress
                // backpressure, resolved synchronously by the test) and
                // retry once. The find must re-run — the old copy went cold.
                self.drain_to_cold();
                let found = self.find(key);
                match found {
                    Some(found) => self
                        .table
                        .overwrite(
                            key,
                            value,
                            hash,
                            LogicalAddr::from_raw(found.addr).expect("fits"),
                            found.len,
                            found.version,
                        )
                        .expect("fits after drain"),
                    None => self.table.insert(key, value, hash).expect("fits after drain"),
                }
            }
            Err(err) => panic!("unexpected op error: {err:?}"),
        };
        let parts = self.table.record(addr);
        let version = parts.version;
        self.log.push((addr.to_raw(), parts.encoded_len));
        self.oracle.insert(
            key.to_vec(),
            OracleEntry { value: value.to_vec(), addr: addr.to_raw(), version },
        );
        self.check_accounting();
    }

    fn del(&mut self, key: &[u8]) -> bool {
        let hash = TieredTable::hash_key(key);
        match self.find(key) {
            Some(found) => {
                self.table.delete(
                    hash,
                    LogicalAddr::from_raw(found.addr).expect("fits"),
                    found.len,
                );
                let entry = self.oracle.remove(key).expect("oracle desync on delete");
                assert_eq!(entry.addr, found.addr, "deleted a stale address");
                self.check_accounting();
                true
            }
            None => {
                assert!(!self.oracle.contains_key(key), "miss for a live key");
                false
            }
        }
    }

    fn get_and_check(&self, key: &[u8]) {
        let found = self.find(key);
        match (found, self.oracle.get(key)) {
            (Some(found), Some(want)) => {
                assert_eq!(found.addr, want.addr, "stale address served");
                assert_eq!(found.value, want.value, "stale value served");
                assert_eq!(found.version, want.version, "stale version served");
                let head = self.table.space().head().to_raw();
                assert_eq!(found.was_cold, want.addr < head, "residency class wrong");
            }
            (None, None) => {}
            (found, want) => panic!(
                "lookup/oracle disagree: found={:?} oracle={:?}",
                found.map(|f| f.addr),
                want.map(|w| w.addr)
            ),
        }
    }

    /// Advances `flushed` to `to`, capturing newly-covered record bytes
    /// into the simulated tier store first (they must be captured while
    /// still RAM-resident — exactly S11's read-then-advance order).
    fn flush_to(&mut self, to: u64) {
        while self.flush_cursor < self.log.len() && self.log[self.flush_cursor].0 < to {
            let (addr, len) = self.log[self.flush_cursor];
            let bytes =
                self.table.record_bytes(LogicalAddr::from_raw(addr).expect("fits"), len).to_vec();
            self.tier.insert(addr, bytes);
            self.flush_cursor += 1;
        }
        self.table.space_mut().advance_flushed(LogicalAddr::from_raw(to).expect("fits"));
    }

    fn drain_to_cold(&mut self) {
        let tail = self.table.space().tail();
        self.table.space_mut().advance_ro_boundary(tail);
        self.flush_to(tail.to_raw());
        self.table.space_mut().advance_head(tail);
    }

    /// Advancement candidates: record starts in `[from, to]`, plus `to`.
    fn boundary_in(&self, from: u64, to: u64, roll: u64) -> u64 {
        let lo = self.log.partition_point(|&(a, _)| a < from);
        let hi = self.log.partition_point(|&(a, _)| a <= to);
        let choices = (hi - lo) + 1;
        let pick = (roll % choices as u64) as usize;
        if pick == choices - 1 { to } else { self.log[lo + pick].0 }
    }

    /// The S06 identity, held from day one: live + dead = allocated.
    fn check_accounting(&self) {
        let report = self.table.space().report();
        assert_eq!(
            self.table.live_bytes() + report.dead_bytes,
            report.allocated_bytes,
            "live + dead != allocated"
        );
    }
}

fn run_storm(seed: u64, ops: usize) {
    let mut h = Harness::new();
    let mut x = seed | 1;
    let mut rand = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    for _op in 0..ops {
        match rand() % 100 {
            // Writes (insert or copy-to-tail overwrite).
            0..=39 => {
                let key = format!("key:{}", rand() % 2048).into_bytes();
                let len = (rand() % 240) as usize;
                let value = vec![(rand() % 251) as u8; len];
                h.set(&key, &value);
            }
            // Verified reads (RAM and cold, with retry contract).
            40..=69 => {
                let key = format!("key:{}", rand() % 2048).into_bytes();
                h.get_and_check(&key);
            }
            // Deletes — index + accounting only for cold records.
            70..=81 => {
                let key = format!("key:{}", rand() % 2048).into_bytes();
                h.del(&key);
            }
            // Region migrations at record boundaries (§3.1 order).
            82..=89 => {
                let space = h.table.space();
                let to = h.boundary_in(space.ro_boundary().to_raw(), space.tail().to_raw(), rand());
                h.table.space_mut().advance_ro_boundary(LogicalAddr::from_raw(to).expect("fits"));
            }
            90..=94 => {
                let space = h.table.space();
                let to =
                    h.boundary_in(space.flushed().to_raw(), space.ro_boundary().to_raw(), rand());
                h.flush_to(to);
            }
            _ => {
                let space = h.table.space();
                let to = h.boundary_in(space.head().to_raw(), space.flushed().to_raw(), rand());
                h.table.space_mut().advance_head(LogicalAddr::from_raw(to).expect("fits"));
            }
        }
    }
    // Final sweep: every oracle key still resolves, correctly classified,
    // byte-exact — zero misses, zero stales.
    let keys: Vec<Vec<u8>> = h.oracle.keys().cloned().collect();
    for key in &keys {
        h.get_and_check(key);
    }
}

proptest! {
    /// Seed-fuzzed storms at CI scale; the 10⁶-op named storm below is
    /// the AC row.
    #[test]
    fn tiered_table_matches_oracle(seed: u64) {
        run_storm(seed, 2_000);
    }
}

/// The M4-S02 AC storm: 10⁶ store/lookup/delete ops across simulated
/// region migrations vs the oracle, one seed, deterministic.
#[test]
fn tiered_storm_million_ops() {
    let ops = if cfg!(miri) { 2_000 } else { 1_000_000 };
    run_storm(0xBEEF_CAFE, ops);
}
