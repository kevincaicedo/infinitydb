//! M4-S07 AC: fill a tiered namespace to 4× its memory budget under the
//! demotion MAINTAIN loop — RAM residency (committed ring bytes) stays
//! ≤ budget + one slice's slack at **every** observation point, zero
//! foreground stalls occur while demotion keeps pace, and the debt
//! metrics drain once the storm stops (ADR-0053).
//!
//! The harness plays the reactor round: foreground writes bounded per
//! round (a paced storm), then one `Keyspace::demote_tick` (seal +
//! release) and the flush leg the S11 pipeline will own — here an
//! instant simulated device capturing record bytes at flush time (the
//! `tiered_oracle` collaborator pattern), confirming via
//! `advance_flushed`. Content is sample-verified at the end through the
//! fetch-verify contract so demotion is proven lossless, not just
//! bounded.
//!
//! Campaign row: `cargo test -p inf-store --release --test tiered_demotion`.

use std::collections::HashMap;

use inf_store::{
    AddressSpaceConfig, DemotionConfig, EvictionPressure, Keyspace, LogicalAddr, NsId, StoreConfig,
    TieredLookup, TieredTable,
};

const NS: NsId = NsId(31);
const OTHER_NS: NsId = NsId(32);
const BUDGET: u64 = 256 << 10;
const PAGE: u64 = 4 << 10;

/// Deterministic value bytes for key `i`, sized by the seeded stream.
fn value_of(i: u64, seed: u64) -> Vec<u8> {
    let mut x = seed ^ i.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    let len = 16 + (x % 240) as usize;
    vec![(x % 251) as u8; len]
}

struct Harness {
    ks: Keyspace,
    /// Every allocation this life, in address order: (addr, len).
    log: Vec<(u64, usize)>,
    /// Instant simulated tier device: record bytes captured at flush
    /// confirmation (S11's role, played by the test).
    tier: HashMap<u64, Vec<u8>>,
    flush_cursor: usize,
}

impl Harness {
    fn new() -> Harness {
        let demote = DemotionConfig::for_budget(BUDGET, PAGE);
        let ring = demote.ring_reserve_bytes().expect("valid budget");
        assert_eq!(ring as u64, (BUDGET + PAGE).next_power_of_two(), "ADR-0052 D1 derivation");
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
                1024,
            )
            .is_ok()
        );
        Harness { ks, log: Vec::new(), tier: HashMap::new(), flush_cursor: 0 }
    }

    fn table(&mut self) -> &mut TieredTable {
        self.ks.tiered_store_mut(NS).expect("materialized")
    }

    /// The flush leg (S11's slice, instant device): capture every sealed
    /// record at or above the flushed watermark, then confirm.
    fn flush_instant(&mut self) {
        let ro = self.table().space().ro_boundary();
        let flushed = self.table().space().flushed();
        if ro == flushed {
            return;
        }
        while self.flush_cursor < self.log.len() {
            let (addr, len) = self.log[self.flush_cursor];
            if addr + len as u64 > ro.to_raw() {
                break;
            }
            if addr >= flushed.to_raw() {
                let bytes = self
                    .table()
                    .record_bytes(LogicalAddr::from_raw(addr).expect("fits"), len)
                    .to_vec();
                self.tier.insert(addr, bytes);
            }
            self.flush_cursor += 1;
        }
        self.table().space_mut().advance_flushed(ro);
    }

    /// One MAINTAIN round: demote tick + the flush leg between its seal
    /// and release halves (order per ADR-0053: seal → flush-confirm →
    /// release — a second tick releases what this round's flush covered).
    /// Returns the round's total watermark progress in bytes.
    fn maintain_round(&mut self) -> u64 {
        let first = self.ks.demote_tick();
        self.flush_instant();
        let second = self.ks.demote_tick();
        first.sealed_bytes + first.released_bytes + second.sealed_bytes + second.released_bytes
    }

    /// The AC bound, asserted at every observation point.
    fn assert_rss_bound(&mut self, at: &str) {
        let committed = self.table().space().report().committed_bytes;
        assert!(
            committed <= BUDGET + PAGE,
            "{at}: committed {committed} exceeds budget {BUDGET} + slice {PAGE}"
        );
    }

    /// Ground-truth read through the fetch-verify contract (cold reads
    /// resolve against the captured tier bytes — index-only, §3.3).
    fn find(&mut self, key: &[u8]) -> Option<(Vec<u8>, bool)> {
        let hash = TieredTable::hash_key(key);
        let mut exclude: Vec<LogicalAddr> = Vec::new();
        loop {
            assert!(exclude.len() <= 4, "fingerprint collision storm");
            let lookup = {
                let table = self.table();
                table.lookup(key, hash, &exclude)
            };
            match lookup {
                TieredLookup::Ram(addr) => {
                    let parts = self.table().record(addr);
                    return Some((parts.value.to_vec(), false));
                }
                TieredLookup::Cold(addr) => {
                    let bytes = self.tier.get(&addr.to_raw()).expect("cold implies captured");
                    let parts = TieredTable::decode_record(bytes);
                    if parts.key == key {
                        return Some((parts.value.to_vec(), true));
                    }
                    exclude.push(addr);
                }
                TieredLookup::Miss => return None,
            }
        }
    }
}

/// The M4-S07 fill AC, end to end.
#[test]
fn fill_to_4x_budget_holds_rss_within_one_slice() {
    let mut h = Harness::new();
    let seed = 0x51_1CE5;
    let total_target = 4 * BUDGET;
    let mut written = 0u64;
    let mut keys = 0u64;
    // Paced storm: at most one slice of foreground bytes per round (the
    // production reactor's MAINTAIN cadence), then the demotion round.
    while written < total_target {
        let mut round_bytes = 0u64;
        while round_bytes < PAGE && written < total_target {
            let key = format!("fill:{keys:08}");
            let value = value_of(keys, seed);
            let hash = TieredTable::hash_key(key.as_bytes());
            let addr = h
                .table()
                .insert(key.as_bytes(), &value, hash)
                .expect("a paced fill under demotion never sees the budget wall");
            let len = h.table().record(addr).encoded_len;
            h.log.push((addr.to_raw(), len));
            round_bytes += len as u64;
            written += len as u64;
            keys += 1;
        }
        h.maintain_round();
        h.assert_rss_bound("during the fill");
    }
    // Zero foreground stalls: demotion kept pace the whole way.
    assert_eq!(
        h.table().space().counters().tail_alloc_stalls,
        0,
        "the paced fill must never hit backpressure"
    );

    // Debt drains: with the storm stopped, MAINTAIN alone brings the
    // mutable region to its fraction target and releases everything
    // flushed (bounded rounds — the drain is slice-paced, not instant).
    let target = h.table().demotion().mutable_target_bytes();
    let mut rounds = 0;
    loop {
        let progress = h.maintain_round();
        h.assert_rss_bound("during the drain");
        if progress == 0 {
            break; // sealed to the nearest mark, flushed, and released.
        }
        rounds += 1;
        assert!(rounds < 4096, "demotion debt must drain in bounded rounds");
    }
    assert!(h.table().mutable_bytes() <= target + 2 * PAGE, "mutable region at its target");
    let space = h.table().space();
    assert_eq!(space.flushed(), space.ro_boundary(), "everything sealed is flushed");
    assert_eq!(space.head(), space.flushed(), "everything flushed is released");
    let committed = space.report().committed_bytes;
    assert!(
        committed <= target + 2 * PAGE,
        "post-drain residency {committed} is the mutable window, not the budget"
    );

    // Demotion was real: most of the 4× fill now lives below the head.
    assert!(space.head().to_raw() >= 3 * BUDGET, "the bulk of the fill demoted");

    // Lossless: sample keys across the whole fill read back exactly —
    // cold for aged keys, RAM for the recent tail.
    let mut cold_seen = 0u32;
    for i in (0..keys).step_by((keys / 128).max(1) as usize) {
        let key = format!("fill:{i:08}");
        let (value, was_cold) = h.find(key.as_bytes()).expect("no record lost to demotion");
        assert_eq!(value, value_of(i, seed), "byte-exact across the tier boundary");
        cold_seen += u32::from(was_cold);
    }
    assert!(cold_seen > 100, "the sample must be dominated by demoted records");
}

/// EvictionPressure v2 routing (§3.2): tiered namespaces demote, cache
/// namespaces evict, and a cache-only keyspace's demote tick is an empty
/// no-op (the S03 degenerate case at the driver level).
#[test]
fn pressure_response_routes_by_namespace_mode() {
    let h = Harness::new();
    assert_eq!(h.ks.pressure_response(NS), EvictionPressure::Demote);
    assert_eq!(h.ks.pressure_response(OTHER_NS), EvictionPressure::Evict);

    let mut cache_only = Keyspace::new(StoreConfig::default());
    let stats = cache_only.demote_tick();
    assert_eq!(stats.tables_active, 0, "no tiered tables — nothing executes");
    assert_eq!(stats.sealed_bytes + stats.released_bytes, 0);
    assert_eq!(cache_only.tiering_counters().demote_slices, 0);
}

/// The backpressure half (ADR-0053 D4): an unpaced burst hits the budget
/// window, the stall names the exact flushed watermark that unblocks it,
/// and MAINTAIN progress (seal → flush → release past the target) makes
/// the retried write fit — the wait key the WatermarkGate consumer parks
/// on in the DST scenario.
#[test]
fn unpaced_burst_stalls_on_the_budget_and_resumes_after_flush_progress() {
    let mut h = Harness::new();
    // Unpaced: no MAINTAIN between writes — the budget window must
    // refuse before the ring would.
    let mut i = 0u64;
    let stalled_key;
    let stalled_value;
    loop {
        let key = format!("burst:{i:08}");
        let value = value_of(i, 0xB0057);
        let hash = TieredTable::hash_key(key.as_bytes());
        match h.table().insert(key.as_bytes(), &value, hash) {
            Ok(addr) => {
                let len = h.table().record(addr).encoded_len;
                h.log.push((addr.to_raw(), len));
                i += 1;
            }
            Err(_) => {
                stalled_key = key;
                stalled_value = value;
                break;
            }
        }
    }
    let committed = h.table().space().report().committed_bytes;
    assert!(committed <= BUDGET + PAGE, "refusal came at the budget window, not the ring");
    let target = h
        .table()
        .write_stall_target(stalled_key.as_bytes(), &stalled_value)
        .expect("watermark progress unblocks a budget stall");
    assert_eq!(h.table().space().counters().tail_alloc_stalls, 1);

    // MAINTAIN until flushed reaches the stall target (the gate-advance
    // moment), then the exact retry the woken writer performs.
    let mut rounds = 0;
    while h.table().space().flushed() < target || h.table().space().head() < target {
        h.maintain_round();
        rounds += 1;
        assert!(rounds < 4096, "flush progress must reach the stall target");
    }
    let hash = TieredTable::hash_key(stalled_key.as_bytes());
    h.table()
        .insert(stalled_key.as_bytes(), &stalled_value, hash)
        .expect("the stall target is exact — the woken retry fits");
}
