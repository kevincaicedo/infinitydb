//! M4-S05/S06 AC: mutation storms over the tiered table vs a shadow
//! model. What every op proves:
//!
//! - **Placement (S05):** an exact-fit update of a mutable-region record
//!   stays at its address (in place, version bumped, zero accounting
//!   movement); a size-changing, sealed, or cold update relocates to the
//!   tail with the old bytes attributed dead at the repoint moment.
//! - **Accounting (S06):** `live + dead = allocated` holds after every
//!   single op — dead bytes are exact, never approximate.
//! - **Crash/replay (S06):** replaying the surviving op log (the
//!   harness plays the WAL, exactly as it plays S11's flush) into a fresh
//!   table at a new life origin rebuilds an equivalent state — content
//!   and versions compared, never addresses (§3.1 "addresses are
//!   per-life"; an address-comparing oracle is flaky by design).
//!
//! Campaign row: `cargo test -p inf-store --release --test tiered_mutation`
//! runs the 10⁶-op storm; the proptest wrapper fuzzes seeds at CI scale.

use std::collections::HashMap;

use inf_store::KeyHasher;
use inf_store::{
    AddrClass, AddressSpaceConfig, DemotionConfig, LogicalAddr, OpError, TieredLookup, TieredTable,
};
use proptest::prelude::*;

const RING: u64 = 1 << 20;
const PAGE: u64 = 1 << 12;
/// Record layout arithmetic for a TTL-less string record (§7.2 v0): the
/// expected-placement predicate needs the new encoded length before the
/// write happens. Asserted against the actual record after every write.
const HEADER_LEN: usize = 8;
const MAX_COLD_RETRIES: usize = 4;

/// One logged mutation — the harness-WAL the crash oracle replays.
/// `None` value = delete.
type LoggedOp = (Vec<u8>, Option<Vec<u8>>);

struct OracleEntry {
    value: Vec<u8>,
    addr: u64,
    version: u32,
}

struct Found {
    addr: u64,
    len: usize,
    version: u32,
    value: Vec<u8>,
    was_cold: bool,
}

struct Harness {
    table: TieredTable,
    oracle: HashMap<Vec<u8>, OracleEntry>,
    /// Every allocation this life, in address order: (addr, len).
    log: Vec<(u64, usize)>,
    /// Simulated tier store (S11's flush, played by the test).
    tier: HashMap<u64, Vec<u8>>,
    flush_cursor: usize,
}

impl Harness {
    fn new(life_origin: LogicalAddr) -> Harness {
        Harness {
            table: TieredTable::new(
                AddressSpaceConfig {
                    reserve_bytes: RING as usize,
                    page_bytes: PAGE as usize,
                    life_origin,
                },
                DemotionConfig::for_budget(RING, PAGE),
                64,
                KeyHasher::default(),
            )
            .expect("reservation"),
            oracle: HashMap::new(),
            log: Vec::new(),
            tier: HashMap::new(),
            flush_cursor: 0,
        }
    }

    /// Drives `lookup` through the fetch-verify-retry contract.
    fn find(&self, key: &[u8]) -> Option<Found> {
        let hash = KeyHasher::default().hash(key);
        let mut exclude: Vec<LogicalAddr> = Vec::new();
        loop {
            assert!(exclude.len() <= MAX_COLD_RETRIES, "fingerprint collision storm");
            match self.table.lookup(key, hash, &exclude) {
                TieredLookup::Ram(addr) => {
                    let parts = self.table.record(addr);
                    return Some(Found {
                        addr: addr.to_raw(),
                        len: parts.encoded_len,
                        version: parts.version,
                        value: parts.value.to_vec(),
                        was_cold: false,
                    });
                }
                TieredLookup::Cold(addr) => {
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
                    exclude.push(addr);
                }
                TieredLookup::Miss => return None,
            }
        }
    }

    /// The storm's SET: routes through `update` for present keys and
    /// asserts the S05 placement contract before checking content.
    fn set(&mut self, key: &[u8], value: &[u8], applied: &mut Vec<LoggedOp>) {
        let hash = KeyHasher::default().hash(key);
        let mut existing = self.find(key);
        // Pre-op accounting snapshot: dead bytes may legally grow by the
        // displaced record *and* a ring-seal hole in the same op — the
        // exactness assertion separates the two via the hole counter.
        let dead_before = self.table.space().report().dead_bytes;
        let holes_before = self.table.space().counters().seal_hole_bytes;
        let mut result = match &existing {
            Some(found) => self.table.update(
                key,
                value,
                hash,
                LogicalAddr::from_raw(found.addr).expect("fits"),
                found.len,
                found.version,
            ),
            None => self.table.insert(key, value, hash),
        };
        if matches!(result, Err(OpError::OutOfMemory)) {
            // Ring window full: migrate everything cold (flush-progress
            // backpressure, resolved synchronously) and retry once. The
            // find must re-run — the old copy went cold.
            self.drain_to_cold();
            existing = self.find(key);
            result = match &existing {
                Some(found) => self.table.update(
                    key,
                    value,
                    hash,
                    LogicalAddr::from_raw(found.addr).expect("fits"),
                    found.len,
                    found.version,
                ),
                None => self.table.insert(key, value, hash),
            };
        }
        let placed = result.expect("fits after drain");
        // S05 placement contract + S06 exact dead-byte attribution,
        // computed from the pre-op state.
        let dead_after = self.table.space().report().dead_bytes;
        let hole_delta = self.table.space().counters().seal_hole_bytes - holes_before;
        match &existing {
            Some(found) => {
                let in_place_expected = !found.was_cold
                    && found.addr >= self.table.space().ro_boundary().to_raw()
                    && HEADER_LEN + key.len() + value.len() == found.len;
                if in_place_expected {
                    assert_eq!(placed.to_raw(), found.addr, "exact-fit mutable stays in place");
                    assert_eq!(dead_after, dead_before, "in place moves no accounting");
                } else {
                    assert_ne!(placed.to_raw(), found.addr, "relocation returns a fresh address");
                    assert_eq!(
                        self.table.space().resolve(placed),
                        AddrClass::Mutable,
                        "the copy lands hot at the tail"
                    );
                    assert_eq!(
                        dead_after,
                        dead_before + found.len as u64 + hole_delta,
                        "displaced record attributed dead at the repoint moment, exactly"
                    );
                }
                let version = self.table.record(placed).version;
                assert_eq!(version, found.version.wrapping_add(1) & 0xFF_FFFF, "version bump");
            }
            None => {
                assert_eq!(self.table.record(placed).version, 0, "fresh key starts at 0");
                assert_eq!(dead_after, dead_before + hole_delta, "insert kills no live bytes");
            }
        }
        let parts = self.table.record(placed);
        assert_eq!(parts.value, value, "written value reads back");
        let len = parts.encoded_len;
        if existing.as_ref().is_none_or(|found| found.addr != placed.to_raw()) {
            self.log.push((placed.to_raw(), len));
        }
        self.oracle.insert(
            key.to_vec(),
            OracleEntry { value: value.to_vec(), addr: placed.to_raw(), version: parts.version },
        );
        applied.push((key.to_vec(), Some(value.to_vec())));
        self.check_accounting();
    }

    fn del(&mut self, key: &[u8], applied: &mut Vec<LoggedOp>) {
        match self.find(key) {
            Some(found) => {
                let hash = KeyHasher::default().hash(key);
                self.table.delete(
                    hash,
                    LogicalAddr::from_raw(found.addr).expect("fits"),
                    found.len,
                );
                self.oracle.remove(key).expect("oracle desync on delete");
                applied.push((key.to_vec(), None));
                self.check_accounting();
            }
            None => assert!(!self.oracle.contains_key(key), "miss for a live key"),
        }
    }

    fn get_and_check(&self, key: &[u8]) {
        match (self.find(key), self.oracle.get(key)) {
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

    /// The S06 identity, asserted after every mutation.
    fn check_accounting(&self) {
        let report = self.table.space().report();
        assert_eq!(
            self.table.live_bytes() + report.dead_bytes,
            report.allocated_bytes,
            "live + dead != allocated"
        );
    }
}

/// The S06 crash/replay oracle: a fresh harness at a new life origin
/// replays the op log and must converge to the same content — value and
/// version per key, never addresses. Returns the successor harness (the
/// storm continues on the recovered state, exactly as a rebooted cell
/// would).
fn crash_and_replay(crashed: Harness, applied: &[LoggedOp]) -> Harness {
    let new_origin = crashed.table.space().tail();
    let mut recovered = Harness::new(new_origin);
    let mut replay_log: Vec<LoggedOp> = Vec::new();
    for (key, op) in applied {
        match op {
            Some(value) => recovered.set(key, value, &mut replay_log),
            None => recovered.del(key, &mut replay_log),
        }
    }
    assert_eq!(replay_log.len(), applied.len(), "replay applies every logged op");
    // Equivalence, content-compared: same key set, same value, same
    // version. Addresses belong to the new life (checked as an invariant,
    // not as equivalence).
    assert_eq!(recovered.oracle.len(), crashed.oracle.len(), "replay lost or invented keys");
    for (key, want) in &crashed.oracle {
        let entry = recovered.oracle.get(key).expect("replay lost a key");
        assert_eq!(entry.value, want.value, "replay diverged on content");
        assert_eq!(entry.version, want.version, "replay diverged on version");
        assert!(entry.addr >= new_origin.to_raw(), "recovered address predates the new life");
    }
    recovered
}

fn run_storm(seed: u64, ops: usize) {
    let mut h = Harness::new(LogicalAddr::ZERO);
    let mut applied: Vec<LoggedOp> = Vec::new();
    let mut x = seed | 1;
    let mut rand = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    for _op in 0..ops {
        match rand() % 100 {
            // Same-size updates — the S05 in-place lane (fresh keys insert).
            0..=24 => {
                let key = format!("key:{}", rand() % 1024).into_bytes();
                let len = h
                    .find(&key)
                    .map_or((rand() % 200) as usize, |found| found.len - HEADER_LEN - key.len());
                let value = vec![(rand() % 251) as u8; len];
                h.set(&key, &value, &mut applied);
            }
            // Size-changing updates — the S06 copy-to-tail lane.
            25..=44 => {
                let key = format!("key:{}", rand() % 1024).into_bytes();
                let value = vec![(rand() % 251) as u8; (rand() % 200) as usize];
                h.set(&key, &value, &mut applied);
            }
            // Verified reads (RAM and cold, with the retry contract).
            45..=64 => {
                let key = format!("key:{}", rand() % 1024).into_bytes();
                h.get_and_check(&key);
            }
            65..=74 => {
                let key = format!("key:{}", rand() % 1024).into_bytes();
                h.del(&key, &mut applied);
            }
            // Region migrations at record boundaries (§3.1 order).
            75..=84 => {
                let space = h.table.space();
                let to = h.boundary_in(space.ro_boundary().to_raw(), space.tail().to_raw(), rand());
                h.table.space_mut().advance_ro_boundary(LogicalAddr::from_raw(to).expect("fits"));
            }
            85..=91 => {
                let space = h.table.space();
                let to =
                    h.boundary_in(space.flushed().to_raw(), space.ro_boundary().to_raw(), rand());
                h.flush_to(to);
            }
            92..=95 => {
                let space = h.table.space();
                let to = h.boundary_in(space.head().to_raw(), space.flushed().to_raw(), rand());
                h.table.space_mut().advance_head(LogicalAddr::from_raw(to).expect("fits"));
            }
            // Rare crash: replay the op log into a new life (S06 oracle).
            // Rare because each replay is O(ops so far); the final crash
            // below always runs, so the full state replays at least once.
            _ => {
                if rand() % 4096 == 0 {
                    h = crash_and_replay(h, &applied);
                } else {
                    let key = format!("key:{}", rand() % 1024).into_bytes();
                    h.get_and_check(&key);
                }
            }
        }
    }
    // Final crash: the whole surviving state replays equivalently.
    h = crash_and_replay(h, &applied);
    let keys: Vec<Vec<u8>> = h.oracle.keys().cloned().collect();
    for key in &keys {
        h.get_and_check(key);
    }
}

proptest! {
    /// Seed-fuzzed storms at CI scale; the 10⁶-op named storm below is
    /// the AC row.
    #[test]
    fn tiered_mutation_matches_oracle(seed: u64) {
        run_storm(seed, 2_000);
    }
}

/// The M4-S05/S06 AC storm: 10⁶ ops of in-place and copy-to-tail updates
/// across region migrations and crash/replay lives, one seed,
/// deterministic.
#[test]
fn tiered_mutation_storm_million_ops() {
    let ops = if cfg!(miri) { 2_000 } else { 1_000_000 };
    run_storm(0xDEAD_5EED, ops);
}
