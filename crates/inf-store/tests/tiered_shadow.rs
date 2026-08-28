//! M4.5-S37 — shadow-slot reconciliation at the seam tier (ADR-0093):
//! the eligible write's probe and admission, the winner's supremacy in
//! lookup order, the record pin on the release ceiling, the
//! reconciler's verdicts (same key / collision / stale) with the exact
//! death and the origin chain, the ticket following an overwrite, the
//! removal-site hooks, the recovery appliers re-forming pairs in both
//! orders, compaction and promotion refusing to relocate a ticket's
//! cold address, enumeration skipping it, `len()` arithmetic, and
//! every bound's refusal — each an ADR-0093 invariant pinned by a test,
//! not a comment.
//!
//! The simulated tier store is the `tiered_oracle` shape (record bytes
//! captured as `flushed` passes them); the compaction leg runs the real
//! `TierFlush` over `MemFs` (the `tiered_compaction` shape).

use std::collections::HashMap;
use std::path::Path;

use inf_log::fs::mem::MemFs;
use inf_log::{
    NsId, TIER_FRAME_BYTES, TierFlush, TierFlushConfig, TierIoMode, tier_extract,
    tier_frame_offset, tier_frame_span,
};
use inf_store::{
    AddrClass, AddressSpaceConfig, CompactionConfig, CompactionWork, DemotionConfig, Index,
    LogicalAddr, SHADOW_READS_IN_FLIGHT, SHADOW_TICKETS_CAP, SettleOutcome, SettleReason,
    ShadowProbe, ShadowRefusal, ShadowVerdict, TieredLookup, TieredMode, TieredTable,
    forced_collision_pair, forced_collision_triple,
};

const RING: u64 = 1 << 20;
const PAGE: u64 = 1 << 12;

/// A table with a simulated tier: records captured at flush time, every
/// watermark driven by the test (§3.1 order), accounting asserted after
/// every step.
struct Harness {
    table: TieredTable,
    /// Every allocation this life, in address order: (addr, len).
    log: Vec<(u64, usize)>,
    tier: HashMap<u64, Vec<u8>>,
    flush_cursor: usize,
}

impl Harness {
    fn new() -> Harness {
        Self::with_demote(DemotionConfig::for_budget(RING, PAGE), RING)
    }

    fn with_demote(demote: DemotionConfig, ring: u64) -> Harness {
        let mut table = TieredTable::new(
            AddressSpaceConfig {
                reserve_bytes: ring as usize,
                page_bytes: PAGE as usize,
                life_origin: LogicalAddr::ZERO,
            },
            demote,
            64,
        )
        .expect("reservation");
        // The arm under test; the shipping default is off (ADR-0093 D8).
        table.set_shadow_enabled(true);
        Harness { table, log: Vec::new(), tier: HashMap::new(), flush_cursor: 0 }
    }

    fn note(&mut self, addr: LogicalAddr) {
        let len = self.table.record(addr).encoded_len;
        self.log.push((addr.to_raw(), len));
        self.check_accounting();
    }

    /// A plain insert of an absent key.
    fn insert(&mut self, key: &[u8], value: &[u8]) -> LogicalAddr {
        let hash = TieredTable::hash_key(key);
        let addr = self.table.insert(key, value, hash).expect("fits");
        self.note(addr);
        addr
    }

    /// The synchronous overwrite of a RAM-resident key (the pre-S37
    /// shape for a RAM hit): `update` — in place or copy-to-tail.
    fn update_ram(&mut self, key: &[u8], value: &[u8]) -> LogicalAddr {
        let hash = TieredTable::hash_key(key);
        let TieredLookup::Ram(old) = self.table.lookup(key, hash, &[]) else {
            panic!("update_ram on a key that is not RAM-resident");
        };
        let (len, version) = {
            let parts = self.table.record(old);
            (parts.encoded_len, parts.version)
        };
        let addr = self.table.update(key, value, hash, old, len, version).expect("fits");
        if addr != old {
            self.note(addr);
        }
        self.check_accounting();
        addr
    }

    /// The shadow write (ADR-0093 D2) over the key's one exact cold
    /// candidate: probe → admit → insert → register. Returns the ticket's
    /// `(cold, winner)`.
    fn shadow_set(&mut self, key: &[u8], value: &[u8]) -> (LogicalAddr, LogicalAddr) {
        let hash = TieredTable::hash_key(key);
        let ShadowProbe::One(cold) = self.table.shadow_probe(key, hash) else {
            panic!("shadow_set needs exactly one exact cold candidate");
        };
        let record_len = TieredTable::RECORD_HEADER_LEN + key.len() + value.len();
        self.table.shadow_admit(hash, cold, record_len).expect("admitted");
        let winner = self.table.insert(key, value, hash).expect("fits");
        self.table.register_shadow(hash, cold, winner);
        self.note(winner);
        (cold, winner)
    }

    /// The reconciler's next read, served from the simulated tier, and
    /// its verdict.
    fn reconcile_one(&mut self) -> Option<ShadowVerdict> {
        let read = self.table.shadow_work(1).into_iter().next()?;
        let image = self.tier.get(&read.ticket.cold.to_raw()).expect("cold implies captured");
        let image = image.clone();
        let t = read.ticket;
        Some(self.table.resolve_shadow(t.hash, t.cold, &image))
    }

    fn reconcile_all(&mut self) {
        while self.table.shadow_pending() > 0 {
            self.reconcile_one().expect("work while pending");
        }
    }

    /// Advances `flushed` to `to`, capturing newly-covered record bytes
    /// into the simulated tier first (S11's read-then-advance order).
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

    /// Seals and flushes everything, then releases up to the ceiling —
    /// every record goes cold unless a pin holds it.
    fn drain_to_cold(&mut self) {
        let tail = self.table.space().tail();
        self.table.space_mut().advance_ro_boundary(tail);
        self.flush_to(tail.to_raw());
        let ceiling = self.table.space().release_ceiling();
        self.table.space_mut().advance_head(LogicalAddr::from_raw(ceiling).expect("fits"));
        self.check_accounting();
    }

    fn value_of(&self, key: &[u8]) -> Option<Vec<u8>> {
        let hash = TieredTable::hash_key(key);
        let mut exclude: Vec<LogicalAddr> = Vec::new();
        loop {
            match self.table.lookup(key, hash, &exclude) {
                TieredLookup::Ram(addr) => return Some(self.table.record(addr).value.to_vec()),
                TieredLookup::Cold(addr) => {
                    let bytes = self.tier.get(&addr.to_raw()).expect("cold implies captured");
                    let parts = TieredTable::decode_record(bytes);
                    if parts.key == key {
                        return Some(parts.value.to_vec());
                    }
                    exclude.push(addr);
                }
                TieredLookup::Miss => return None,
            }
        }
    }

    /// The S06 identity, held across every transition (ADR-0093 I4).
    fn check_accounting(&self) {
        let report = self.table.space().report();
        assert_eq!(
            self.table.live_bytes() + report.dead_bytes,
            report.allocated_bytes,
            "live + dead != allocated"
        );
    }
}

/// A cold record for `key` under a fresh table: insert, drain cold.
fn cold_key(h: &mut Harness, key: &[u8], value: &[u8]) -> LogicalAddr {
    let addr = h.insert(key, value);
    h.drain_to_cold();
    assert_eq!(h.table.space().resolve(addr), AddrClass::Cold);
    addr
}

/// ADR-0093 D2/D3/D4: the eligible write appends the winner over the
/// cold twin, the winner serves immediately, `len()` counts the key
/// once, and the reconciler's same-key verdict removes exactly the twin
/// with an exact death and chains it as the winner's origin — the
/// marker the next displacement stages.
#[test]
fn same_key_shadow_serves_the_winner_and_resolves_exactly() {
    let mut h = Harness::new();
    let a = cold_key(&mut h, b"k", b"v1");
    let dead_before = h.table.space().report().dead_bytes;
    let (cold, b) = h.shadow_set(b"k", b"v2");
    assert_eq!(cold, a);
    assert_eq!(h.value_of(b"k").as_deref(), Some(&b"v2"[..]), "the winner serves");
    assert_eq!(h.table.len(), 1, "one key, two slots");
    assert_eq!(h.table.shadow_pending(), 1);
    assert_eq!(h.table.space().record_pin(), Some(b), "the pin is the winner");
    assert!(h.table.is_shadow_cold(a));
    assert_eq!(h.table.space().report().dead_bytes, dead_before, "nothing died yet");
    assert_eq!(h.reconcile_one(), Some(ShadowVerdict::SameKey));
    assert_eq!(h.table.shadow_pending(), 0);
    assert_eq!(h.table.space().record_pin(), None);
    assert!(!h.table.is_shadow_cold(a));
    let hash = TieredTable::hash_key(b"k");
    assert!(!h.table.contains_pair(hash, a), "the twin's slot is gone");
    let a_len = h.tier[&a.to_raw()].len() as u64;
    assert_eq!(h.table.space().report().dead_bytes, dead_before + a_len, "an exact death");
    h.check_accounting();
    assert_eq!(h.value_of(b"k").as_deref(), Some(&b"v2"[..]));
    let origins = h.table.take_displacement_origins(hash, b);
    assert_eq!(origins.len(), 1, "the twin is the winner's origin");
    assert_eq!(origins[0].0, a.to_raw());
    let counters = h.table.shadow_counters();
    assert_eq!((counters.created, counters.resolved_same_key), (1, 1));
}

/// ADR-0093 D4 (the collision verdict) and I3 (no removal on hash
/// evidence): a reconciler handed a record whose key differs from the
/// winner's ends the ticket and removes nothing. A genuine 64-bit
/// collision cannot be built from real keys (`lookup` asserts the hash
/// is the key's own), so the verdict is exercised by feeding the
/// reconciler a foreign record's image — the comparison it makes is
/// the same; the probe order a real collision relies on is pinned at
/// the index level in `ram_verified_slot_outranks_a_cold_twin`.
#[test]
fn a_foreign_image_is_a_collision_and_removes_nothing() {
    let mut h = Harness::new();
    let a = cold_key(&mut h, b"k", b"v1");
    let other = h.insert(b"other", b"o1");
    h.drain_to_cold();
    let (_, b) = h.shadow_set(b"k", b"v2");
    let hash = TieredTable::hash_key(b"k");
    let foreign = h.tier[&other.to_raw()].clone();
    let dead_before = h.table.space().report().dead_bytes;
    assert_eq!(h.table.resolve_shadow(hash, a, &foreign), ShadowVerdict::Collision);
    let _ = b;
    assert_eq!(h.table.shadow_pending(), 0);
    assert!(h.table.contains_pair(hash, a), "both records stay");
    assert!(h.table.contains_pair(hash, b));
    assert_eq!(h.table.space().report().dead_bytes, dead_before, "nothing died");
    assert_eq!(h.table.len(), 3, "a collision is two keys plus `other`");
    assert_eq!(h.table.shadow_counters().resolved_collision, 1);
    assert_eq!(h.value_of(b"k").as_deref(), Some(&b"v2"[..]), "the winner still serves");
}

/// ADR-0093 D3 (I2): a RAM-resident, key-verified slot outranks a cold
/// slot with the same 64-bit hash in either probe order — the property
/// the whole design stands on, pinned at the index with a forced
/// collision (no key hashing involved).
#[test]
fn ram_verified_slot_outranks_a_cold_twin() {
    let hash = 0x9E37_79B9_7F4A_7C15u64;
    let cold = LogicalAddr::from_raw(100).expect("small");
    let ram = LogicalAddr::from_raw(200).expect("small");
    for order in [[cold, ram], [ram, cold]] {
        let mut index: Index<TieredMode> = Index::with_capacity(16);
        for addr in order {
            index.insert(hash, addr);
        }
        // The verify closure is the store's: RAM slots key-verify, cold
        // slots never verify (they are candidates, not answers).
        let hit = index.find(hash, |addr| addr == ram);
        assert_eq!(hit, Some(ram), "order {order:?}");
        let mut exact = Vec::new();
        index.each_exact(hash, |addr| exact.push(addr));
        assert_eq!(exact.len(), 2, "both slots carry the full hash");
        assert!(index.contains_pair(hash, cold) && index.contains_pair(hash, ram));
    }
}

/// ADR-0093 D3: a later overwrite of the key moves the ticket to the
/// new winner (the pin follows, never retreats); a same-length in-place
/// rewrite leaves both alone; the eventual verdict is the same key.
#[test]
fn the_ticket_follows_the_winner_across_overwrites() {
    let mut h = Harness::new();
    let a = cold_key(&mut h, b"k", b"v1");
    let (_, b) = h.shadow_set(b"k", b"v2");
    let c = h.update_ram(b"k", b"a longer value that must copy to the tail");
    assert_ne!(c, b);
    let ticket = h.table.shadow_of_winner(c).expect("the ticket followed");
    assert_eq!((ticket.cold, ticket.winner), (a, c));
    assert!(h.table.shadow_of_winner(b).is_none());
    assert_eq!(h.table.space().record_pin(), Some(c), "the pin followed");
    assert_eq!(h.table.shadow_counters().retargeted, 1);
    // Same length, still mutable: in place.
    let d = h.update_ram(b"k", b"a longer value that must copy to the tail");
    assert_eq!(d, c, "exact fit rewrites in place");
    assert_eq!(h.table.shadow_of_winner(c).map(|t| t.cold), Some(a));
    assert_eq!(h.reconcile_one(), Some(ShadowVerdict::SameKey));
    assert_eq!(
        h.value_of(b"k").as_deref(),
        Some(&b"a longer value that must copy to the tail"[..])
    );
    assert_eq!(h.table.len(), 1);
    h.check_accounting();
}

/// ADR-0093 D4 as amended (A1/A3): a completion whose ticket moved
/// under an overwrite is verified against the ticket's **current**
/// winner — the relation is the twin versus the key's current record,
/// and the image is the twin's immutable bytes — so it resolves; a
/// completion for a ticket that no longer exists is `Stale` and changes
/// nothing.
#[test]
fn a_completion_after_the_winner_moved_resolves_against_the_current_winner() {
    let mut h = Harness::new();
    let a = cold_key(&mut h, b"k", b"v1");
    let (_, b) = h.shadow_set(b"k", b"v2");
    let read = h.table.shadow_work(1).into_iter().next().expect("one ticket");
    assert_eq!(read.ticket.winner, b);
    // The key is overwritten while the read is in flight.
    let c = h.update_ram(b"k", b"a longer value that must copy to the tail");
    let image = h.tier[&a.to_raw()].clone();
    let hash = TieredTable::hash_key(b"k");
    assert_eq!(h.table.resolve_shadow(hash, a, &image), ShadowVerdict::SameKey);
    assert_eq!(h.table.shadow_pending(), 0, "resolved against the moved winner");
    assert_eq!(h.table.shadow_counters().stale, 0);
    let origins = h.table.take_displacement_origins(hash, c);
    assert_eq!(origins.iter().map(|(o, _)| *o).collect::<Vec<_>>(), vec![a.to_raw()]);
    // The same completion again: the ticket is gone — stale, nothing.
    let dead_before = h.table.space().report().dead_bytes;
    assert_eq!(h.table.resolve_shadow(hash, a, &image), ShadowVerdict::Stale);
    assert_eq!(h.table.space().report().dead_bytes, dead_before);
    assert_eq!(h.table.shadow_counters().stale, 1);
    h.check_accounting();
}

/// ADR-0093 D3 (I7): deleting a winner with an open ticket is a
/// violated invariant — the plane resolves first.
#[test]
#[should_panic(expected = "delete of a shadow winner before its ticket resolved")]
fn deleting_a_winner_with_an_open_ticket_panics() {
    let mut h = Harness::new();
    cold_key(&mut h, b"k", b"v1");
    let (_, b) = h.shadow_set(b"k", b"v2");
    let len = h.table.record(b).encoded_len;
    h.table.delete(TieredTable::hash_key(b"k"), b, len);
}

/// ADR-0093 D3: after the forced resolution the delete proceeds, and the
/// twin's address is among the origins the delete's markers cover.
#[test]
fn delete_after_a_forced_resolution_covers_the_twin() {
    let mut h = Harness::new();
    let a = cold_key(&mut h, b"k", b"v1");
    let (_, b) = h.shadow_set(b"k", b"v2");
    let hash = TieredTable::hash_key(b"k");
    let image = h.tier[&a.to_raw()].clone();
    assert_eq!(h.table.resolve_shadow(hash, a, &image), ShadowVerdict::SameKey);
    let len = h.table.record(b).encoded_len;
    let origins = h.table.take_displacement_origins(hash, b);
    assert_eq!(origins.iter().map(|(addr, _)| *addr).collect::<Vec<_>>(), vec![a.to_raw()]);
    h.table.delete(hash, b, len);
    assert_eq!(h.value_of(b"k"), None);
    assert_eq!(h.table.len(), 0);
    h.check_accounting();
}

/// ADR-0093 D3 (I1): the release ceiling clamps at the oldest winner
/// while a ticket is open — sealing and flushing pass it, the head does
/// not — and lifts at resolution.
#[test]
fn the_record_pin_clamps_release_until_resolution() {
    let mut h = Harness::new();
    cold_key(&mut h, b"k", b"v1");
    let (_, b) = h.shadow_set(b"k", b"v2");
    h.insert(b"after", b"x");
    let tail = h.table.space().tail();
    h.table.space_mut().advance_ro_boundary(tail);
    h.flush_to(tail.to_raw());
    assert_eq!(h.table.space().flushed(), tail, "flush passes the winner");
    assert_eq!(h.table.space().release_ceiling(), b.to_raw(), "release does not");
    assert_eq!(h.table.shadow_pinned_bytes(), tail.to_raw() - b.to_raw());
    h.table.space_mut().advance_head(b);
    assert_eq!(h.table.space().resolve(b), AddrClass::ReadOnly, "the winner stays RAM");
    h.reconcile_all();
    assert_eq!(h.table.space().release_ceiling(), tail.to_raw(), "the pin lifted");
    h.table.space_mut().advance_head(tail);
    assert_eq!(h.value_of(b"k").as_deref(), Some(&b"v2"[..]), "served cold now, once");
}

/// ADR-0093 D3 (I1): a winner never demotes while its ticket is open —
/// the drain that would make it cold stops at the pin.
#[test]
fn a_shadowed_winner_never_goes_cold() {
    let mut h = Harness::new();
    cold_key(&mut h, b"k", b"v1");
    let (_, b) = h.shadow_set(b"k", b"v2");
    for i in 0..8u32 {
        h.insert(format!("fill:{i}").as_bytes(), &[0x11; 700]);
    }
    h.drain_to_cold();
    assert_ne!(h.table.space().resolve(b), AddrClass::Cold);
    assert_eq!(h.table.space().head(), b, "the head stopped at the winner");
}

/// ADR-0093 D2 (I9): every admission bound refuses typed and counts —
/// the knob, the pinned-suffix cap, the ticket cap — and a refused
/// write mutates nothing.
#[test]
fn admission_refuses_at_every_bound_and_counts() {
    // Knob off.
    let mut h = Harness::new();
    let a = cold_key(&mut h, b"k", b"v1");
    h.table.set_shadow_enabled(false);
    let hash = TieredTable::hash_key(b"k");
    assert_eq!(h.table.shadow_admit(hash, a, 64), Err(ShadowRefusal::Off));
    assert_eq!(h.table.shadow_counters().fallback_off, 1);
    h.table.set_shadow_enabled(true);
    assert_eq!(h.table.shadow_admit(hash, a, 64), Ok(()));
    // The pinned-suffix cap: a 64 KiB budget caps the suffix at four
    // commit pages (16 KiB); the third 6 KiB winner would exceed it.
    let small =
        DemotionConfig { mem_budget_bytes: 1 << 16, mutable_permille: 500, slice_bytes: PAGE };
    let mut h = Harness::with_demote(small, 1 << 17);
    h.table.set_shadow_enabled(true);
    assert_eq!(h.table.shadow_pin_cap_bytes(), 4 * PAGE);
    for i in 0..3u32 {
        h.insert(format!("p:{i}").as_bytes(), &[0x22; 6000]);
    }
    h.drain_to_cold();
    h.shadow_set(b"p:0", &[0x33; 6000]);
    h.shadow_set(b"p:1", &[0x33; 6000]);
    let hash = TieredTable::hash_key(b"p:2");
    let ShadowProbe::One(cold) = h.table.shadow_probe(b"p:2", hash) else { panic!("one") };
    assert_eq!(h.table.shadow_admit(hash, cold, 6000 + 8 + 3), Err(ShadowRefusal::Pin));
    assert_eq!(h.table.shadow_counters().fallback_pin, 1);
    assert_eq!(h.table.shadow_pending(), 2, "a refusal registers nothing");
    // The ticket cap.
    let mut h = Harness::new();
    h.table.set_shadow_enabled(true);
    let cap = inf_store::SHADOW_TICKETS_CAP;
    for i in 0..=cap {
        h.insert(format!("t:{i}").as_bytes(), b"v");
    }
    h.drain_to_cold();
    for i in 0..cap {
        h.shadow_set(format!("t:{i}").as_bytes(), b"w");
    }
    let key = format!("t:{cap}");
    let hash = TieredTable::hash_key(key.as_bytes());
    let ShadowProbe::One(cold) = h.table.shadow_probe(key.as_bytes(), hash) else { panic!("one") };
    assert_eq!(h.table.shadow_admit(hash, cold, 16), Err(ShadowRefusal::Tickets));
    assert_eq!(h.table.shadow_counters().fallback_tickets, 1);
    assert_eq!(h.table.shadow_pending(), cap);
    assert_eq!(h.table.len(), cap + 1);
    h.reconcile_all();
    assert_eq!(h.table.len(), cap + 1);
    h.check_accounting();
}

/// ADR-0093 D2 step 5: a candidate whose origin list is full refuses —
/// at resolution the candidate itself would join the winner's list and
/// the list may not exceed the ADR-0059 cap. Promotion chains origins
/// like compaction (ADR-0085 D4), so three promotions fill the cap.
#[test]
fn a_candidate_at_the_origin_cap_refuses() {
    let mut h = Harness::new();
    h.table.set_promote_enabled(true);
    let mut addr = cold_key(&mut h, b"k", b"v1");
    let hash = TieredTable::hash_key(b"k");
    for _ in 0..3 {
        let image = h.tier[&addr.to_raw()].clone();
        // Second touch promotes (ADR-0085 D2).
        assert!(!h.table.try_promote(hash, addr, &image));
        assert!(h.table.try_promote(hash, addr, &image), "promoted");
        let TieredLookup::Ram(promoted) = h.table.lookup(b"k", hash, &[]) else { panic!("ram") };
        h.note(promoted);
        h.drain_to_cold();
        addr = promoted;
    }
    assert_eq!(h.table.shadow_admit(hash, addr, 64), Err(ShadowRefusal::Origin));
    assert_eq!(h.table.shadow_counters().fallback_origin, 1);
}

/// ADR-0093 D2 step 2: two exact-hash cold slots are the synchronous
/// path's business (`Many`); zero exact slots behind a fingerprint-only
/// candidate is an insert without a read (`NoCandidate`) — the sidecar
/// is 64-bit evidence.
#[test]
fn the_probe_distinguishes_exact_from_fingerprint_candidates() {
    // NoCandidate / One on real keys.
    let mut h = Harness::new();
    let hash = TieredTable::hash_key(b"k");
    assert_eq!(h.table.shadow_probe(b"k", hash), ShadowProbe::Miss);
    let a = cold_key(&mut h, b"k", b"v1");
    assert_eq!(h.table.shadow_probe(b"k", hash), ShadowProbe::One(a));
    let (_, b) = h.shadow_set(b"k", b"v2");
    assert_eq!(h.table.shadow_probe(b"k", hash), ShadowProbe::RamHit(b));
    // Many: two exact-hash cold slots — forced at the index level (the
    // probe's classifier is what is under test).
    let forced = 0x1234_5678_9ABC_DEF0u64;
    let mut index: Index<TieredMode> = Index::with_capacity(16);
    index.insert(forced, LogicalAddr::from_raw(10).expect("small"));
    index.insert(forced, LogicalAddr::from_raw(20).expect("small"));
    let mut n = 0;
    index.each_exact(forced, |_| n += 1);
    assert_eq!(n, 2);
    // A fingerprint-only neighbour (same tag, fingerprint and home group,
    // a different middle bit) is invisible to `each_exact`.
    index.insert(forced ^ (1 << 20), LogicalAddr::from_raw(30).expect("small"));
    let mut n = 0;
    index.each_exact(forced, |_| n += 1);
    assert_eq!(n, 2, "the fingerprint-only slot is not an exact candidate");
    assert!(index.find(forced, |_| true).is_some(), "but `find` reports it as a candidate");
}

/// ADR-0093 D4/D7: at most `SHADOW_READS_IN_FLIGHT` reads are out at
/// once; a failed read returns its ticket to the work list.
#[test]
fn reads_in_flight_are_bounded_and_failures_retry() {
    let mut h = Harness::new();
    for i in 0..6u32 {
        h.insert(format!("k:{i}").as_bytes(), b"v");
    }
    h.drain_to_cold();
    for i in 0..6u32 {
        h.shadow_set(format!("k:{i}").as_bytes(), b"w");
    }
    let first = h.table.shadow_work(16);
    assert_eq!(first.len(), SHADOW_READS_IN_FLIGHT);
    assert!(h.table.shadow_work(16).is_empty(), "the bound holds");
    let winners: Vec<u64> = first.iter().map(|r| r.ticket.winner.to_raw()).collect();
    assert!(winners.windows(2).all(|w| w[0] < w[1]), "oldest winner first");
    h.table.shadow_read_failed(first[0].ticket.cold);
    let again = h.table.shadow_work(16);
    assert_eq!(again.len(), 1);
    assert_eq!(again[0].ticket, first[0].ticket, "the failed read is re-offered");
    assert_eq!(h.table.shadow_counters().read_errors, 1);
    assert_eq!(h.table.shadow_pending(), 6);
}

/// ADR-0093 D8 (I10): turning the knob off orphans nothing — open
/// tickets keep reconciling.
#[test]
fn the_knob_off_keeps_reconciling_open_tickets() {
    let mut h = Harness::new();
    cold_key(&mut h, b"k", b"v1");
    h.shadow_set(b"k", b"v2");
    h.table.set_shadow_enabled(false);
    assert_eq!(h.table.shadow_work(1).len(), 1);
    assert_eq!(h.reconcile_one(), None, "the read was marked in flight above");
    let read = h.table.shadow_tickets().next().expect("open");
    let image = h.tier[&read.cold.to_raw()].clone();
    assert_eq!(h.table.resolve_shadow(read.hash, read.cold, &image), ShadowVerdict::SameKey);
}

/// ADR-0093 A3: enumeration names a ticket's cold slot like any cold
/// slot — the key twice (legal) or the collision key (required); the
/// twin is counted.
#[test]
fn scan_names_a_tickets_cold_slot() {
    let mut h = Harness::new();
    let a = cold_key(&mut h, b"k", b"v1");
    let (_, b) = h.shadow_set(b"k", b"v2");
    let mut seen = Vec::new();
    let mut cursor = 0;
    loop {
        cursor = h.table.scan_slots(cursor, 64, |_, addr| seen.push(addr));
        if cursor == 0 {
            break;
        }
    }
    seen.sort_unstable();
    let mut want = vec![a, b];
    want.sort_unstable();
    assert_eq!(seen, want, "both slots, the twin included");
    assert_eq!(h.table.shadow_counters().scan_twins_emitted, 1);
}

/// ADR-0093 D6: promotion never relocates a ticket's cold slot.
#[test]
fn promotion_skips_a_tickets_cold_slot() {
    let mut h = Harness::new();
    h.table.set_promote_enabled(true);
    let a = cold_key(&mut h, b"k", b"v1");
    h.shadow_set(b"k", b"v2");
    let hash = TieredTable::hash_key(b"k");
    let image = h.tier[&a.to_raw()].clone();
    assert!(!h.table.try_promote(hash, a, &image), "first touch");
    assert!(!h.table.try_promote(hash, a, &image), "second touch: skipped");
    assert_eq!(h.table.shadow_counters().promote_skip, 1);
    assert!(h.table.is_shadow_cold(a), "still the ticket's cold slot");
    assert_eq!(h.table.shadow_pending(), 1);
}

/// ADR-0093 D5 (I8): the recovery appliers re-form a pair in either
/// order — ref then image, image then ref — and a displacement marker
/// for the cold slot ends the ticket.
#[test]
fn recovery_appliers_reform_pairs_in_both_orders() {
    let origin = LogicalAddr::from_raw(1 << 20).expect("fits");
    let pre_life = LogicalAddr::from_raw(4096).expect("fits");
    let make = || {
        TieredTable::new(
            AddressSpaceConfig {
                reserve_bytes: RING as usize,
                page_bytes: PAGE as usize,
                life_origin: origin,
            },
            DemotionConfig::for_budget(RING, PAGE),
            64,
        )
        .expect("reservation")
    };
    let hash = TieredTable::hash_key(b"k");
    // The recovered catalog: one manifested file covering the pre-life
    // address (refs count into their file — ADR-0058 D4).
    let seed = |t: &mut TieredTable| {
        t.seed_recovered_files(
            &[inf_log::TierFileMeta {
                id: 0,
                base: LogicalAddr::ZERO,
                data_len: 1 << 16,
                reason: inf_log::tier::SealReason::Recovered,
                path: Path::new("shard-0/cold/tier-000000.itier").to_path_buf(),
            }],
            7,
        );
    };
    // The appliers do not track tickets incrementally (a winner's address
    // moves under the ticket as replay overwrites it); the set is rebuilt
    // from the finished index once, at recovery-complete. Ref then image,
    // image then ref, a duplicate ref — every order lands on the same
    // finished index (one RAM winner, one cold twin), so one ticket.
    for order in 0..3 {
        let mut t = make();
        seed(&mut t);
        let b = match order {
            0 => {
                t.apply_ref(hash, pre_life);
                t.apply_image(b"k", b"v2", hash).expect("fits")
            }
            1 => {
                let b = t.apply_image(b"k", b"v2", hash).expect("fits");
                t.apply_ref(hash, pre_life);
                b
            }
            _ => {
                let b = t.apply_image(b"k", b"v2", hash).expect("fits");
                t.apply_ref(hash, pre_life);
                t.apply_ref(hash, pre_life); // the walker's at-least-once
                b
            }
        };
        assert_eq!(t.shadow_pending(), 0, "no ticket before the rebuild");
        assert!(t.rebuild_shadow_tickets().is_empty(), "one RAM sibling: nothing to settle");
        assert_eq!(t.shadow_pending(), 1, "order {order}");
        let ticket = t.shadow_of_winner(b).expect("pair");
        assert_eq!((ticket.cold, ticket.winner), (pre_life, b));
        assert_eq!(t.len(), 1);
        assert_eq!(t.space().record_pin(), Some(b));
    }
    // A ColdDisplace for the twin (a resolution the crashed life made
    // durable through a later displacement) removes the cold slot: the
    // rebuild then finds no pair.
    let mut t = make();
    seed(&mut t);
    let b = t.apply_image(b"k", b"v2", hash).expect("fits");
    t.apply_ref(hash, pre_life);
    assert!(t.apply_displace(hash, pre_life));
    assert!(t.rebuild_shadow_tickets().is_empty());
    assert_eq!(t.shadow_pending(), 0, "the twin's slot is gone");
    assert_eq!(t.space().record_pin(), None);
    let _ = b;
    // Replay's delete of the winner: the key is gone, so the rebuild
    // finds a lone cold twin with no RAM winner — no ticket (a twin the
    // crashed life told apart as a collision key stays slotted, unpaired).
    let mut t = make();
    seed(&mut t);
    let _b = t.apply_image(b"k", b"v2", hash).expect("fits");
    t.apply_ref(hash, pre_life);
    assert!(t.apply_delete(b"k", hash));
    assert!(t.rebuild_shadow_tickets().is_empty());
    assert_eq!(t.shadow_pending(), 0);
    assert!(t.contains_pair(hash, pre_life), "the twin stays slotted, unpaired");
    assert_eq!(t.len(), 1, "one slot, counted as one key (no open ticket)");
}

// ---- forced 64-bit collisions (ADR-0093 A7) ------------------------------

/// A crafted pair: two distinct keys with one `hash_key` (the oracle).
fn collision_pair(tag: u64) -> (Vec<u8>, Vec<u8>) {
    let (k1, k2) = forced_collision_pair(tag);
    assert_ne!(k1, k2);
    assert_eq!(TieredTable::hash_key(&k1), TieredTable::hash_key(&k2));
    (k1.to_vec(), k2.to_vec())
}

/// The plane's synchronous insert of an absent key whose cold candidate
/// is another key: read, compare, exclude, retry, insert.
fn sync_insert_absent(h: &mut Harness, key: &[u8], value: &[u8]) -> LogicalAddr {
    assert_eq!(h.value_of(key), None, "the key is absent (its candidates are other keys)");
    h.insert(key, value)
}

/// ADR-0093 A2: a second key colliding with a ticketed slot is
/// `Ticketed` — synchronous, never a second ticket on one cold address
/// — and the first ticket resolves as the same key it always was.
#[test]
fn a_colliding_write_over_a_ticketed_slot_is_synchronous() {
    let (k1, k2) = collision_pair(1);
    let mut h = Harness::new();
    let a = cold_key(&mut h, &k1, b"one");
    let (cold, b1) = h.shadow_set(&k1, b"one-v2");
    assert_eq!(cold, a);
    let hash = TieredTable::hash_key(&k2);
    assert_eq!(h.table.shadow_probe(&k2, hash), ShadowProbe::Ticketed(a));
    h.table.note_shadow_ticketed();
    assert_eq!(h.table.shadow_counters().fallback_ticketed, 1);
    let b2 = sync_insert_absent(&mut h, &k2, b"two");
    assert_eq!(h.value_of(&k1).as_deref(), Some(&b"one-v2"[..]));
    assert_eq!(h.value_of(&k2).as_deref(), Some(&b"two"[..]));
    assert_eq!(h.table.shadow_pending(), 1, "still the one ticket");
    assert_eq!(h.reconcile_one(), Some(ShadowVerdict::SameKey));
    assert!(!h.table.contains_pair(hash, a));
    assert!(h.table.contains_pair(hash, b1) && h.table.contains_pair(hash, b2));
    assert_eq!(h.table.len(), 2);
    h.check_accounting();
}

/// ADR-0093 A2: the duplicate is unrepresentable — a release assert.
#[test]
#[should_panic(expected = "a cold address carries one ticket")]
fn registering_a_second_ticket_on_one_cold_address_panics() {
    let (k1, k2) = collision_pair(2);
    let mut h = Harness::new();
    let a = cold_key(&mut h, &k1, b"one");
    h.shadow_set(&k1, b"one-v2");
    let b2 = sync_insert_absent(&mut h, &k2, b"two");
    h.table.register_shadow(TieredTable::hash_key(&k2), a, b2);
}

/// ADR-0093 A1/A3: a real collision ticket — the second key's write
/// over the first key's cold record — serves both keys exactly while
/// open, is ambiguous in `len()` until read, and the `DBSIZE` drain
/// (verify without settle) makes the count exact: the verdict is
/// `Collision`, both slots stay, nothing dies.
#[test]
fn a_collision_ticket_keeps_both_keys_and_the_drain_makes_the_count_exact() {
    let (k1, k2) = collision_pair(3);
    let mut h = Harness::new();
    let a = cold_key(&mut h, &k1, b"one");
    let hash = TieredTable::hash_key(&k2);
    assert_eq!(h.table.shadow_probe(&k2, hash), ShadowProbe::One(a), "k1's slot is exact");
    let (cold, b2) = h.shadow_set(&k2, b"two");
    assert_eq!(cold, a);
    assert_eq!(h.value_of(&k1).as_deref(), Some(&b"one"[..]), "k1 serves from its record");
    assert_eq!(h.value_of(&k2).as_deref(), Some(&b"two"[..]), "k2 is the winner");
    assert_eq!(h.table.shadow_unverified(), 1, "ambiguous until read");
    assert_eq!(h.table.len(), 1, "the arithmetic assumes same-key — not yet a fact");
    // The DBSIZE drain: verify every unverified ticket, then count.
    let dead_before = h.table.space().report().dead_bytes;
    let pending = h.table.shadow_unverified_tickets();
    assert_eq!(pending.len(), 1);
    for t in pending {
        let image = h.tier[&t.cold.to_raw()].clone();
        assert_eq!(h.table.verify_shadow(t.hash, t.cold, &image), ShadowVerdict::Collision);
    }
    assert_eq!(h.table.shadow_unverified(), 0);
    assert_eq!(h.table.len(), 2, "exact: two keys");
    assert!(h.table.contains_pair(hash, a) && h.table.contains_pair(hash, b2));
    assert_eq!(h.table.space().report().dead_bytes, dead_before, "nothing died");
    assert_eq!(h.table.shadow_counters().resolved_collision, 1);
    assert_eq!(h.value_of(&k1).as_deref(), Some(&b"one"[..]));
    assert_eq!(h.value_of(&k2).as_deref(), Some(&b"two"[..]));
}

/// ADR-0093 A3: the fence refuses a new ticket while a drain runs (the
/// unverified set only shrinks), and lifts after it.
#[test]
fn the_dbsize_fence_refuses_new_tickets_while_raised() {
    let mut h = Harness::new();
    let a = cold_key(&mut h, b"k", b"v1");
    let hash = TieredTable::hash_key(b"k");
    h.table.shadow_fence(true);
    assert_eq!(h.table.shadow_admit(hash, a, 64), Err(ShadowRefusal::Fence));
    assert_eq!(h.table.shadow_counters().fallback_fence, 1);
    assert_eq!(h.table.shadow_counters().dbsize_drains, 1);
    h.table.shadow_fence(false);
    assert_eq!(h.table.shadow_admit(hash, a, 64), Ok(()));
}

/// ADR-0093 A3: enumeration names a collision key while its slot is a
/// ticket's cold address.
#[test]
fn scan_names_a_collision_key() {
    let (k1, k2) = collision_pair(4);
    let mut h = Harness::new();
    let a = cold_key(&mut h, &k1, b"one");
    let (_, b2) = h.shadow_set(&k2, b"two");
    let mut seen = Vec::new();
    let mut cursor = 0;
    loop {
        cursor = h.table.scan_slots(cursor, 64, |_, addr| seen.push(addr));
        if cursor == 0 {
            break;
        }
    }
    assert!(seen.contains(&a), "k1's slot is named");
    assert!(seen.contains(&b2));
    let named: Vec<Vec<u8>> = seen
        .iter()
        .map(|addr| match h.table.space().resolve(*addr) {
            AddrClass::Cold => TieredTable::decode_record(&h.tier[&addr.to_raw()]).key.to_vec(),
            _ => h.table.record(*addr).key.to_vec(),
        })
        .collect();
    assert!(named.contains(&k1) && named.contains(&k2), "both keys by name");
}

/// ADR-0093 D3/A1: `DEL` of a winner whose twin is a collision key
/// verifies first — the verdict ends the ticket and the other key
/// stays; a verified same-key ticket lets `DEL` skip the read.
#[test]
fn delete_of_a_winner_whose_twin_is_a_collision_key_leaves_the_key() {
    let (k1, k2) = collision_pair(5);
    let mut h = Harness::new();
    let a = cold_key(&mut h, &k1, b"one");
    let (_, b2) = h.shadow_set(&k2, b"two");
    let ticket = h.table.shadow_of_winner(b2).expect("open");
    assert_eq!(ticket.verified_len, None);
    let image = h.tier[&a.to_raw()].clone();
    assert_eq!(h.table.verify_shadow(ticket.hash, ticket.cold, &image), ShadowVerdict::Collision);
    assert!(h.table.shadow_of_winner(b2).is_none(), "the ticket ended");
    let len = h.table.record(b2).encoded_len;
    h.table.delete(ticket.hash, b2, len);
    assert_eq!(h.value_of(&k2), None);
    assert_eq!(h.value_of(&k1).as_deref(), Some(&b"one"[..]), "k1 untouched");
    assert_eq!(h.table.len(), 1);
    h.check_accounting();
    // The same-key shape: verified, then DEL needs no read.
    let (_, b1) = h.shadow_set(&k1, b"one-v2");
    let t = h.table.shadow_of_winner(b1).expect("open");
    assert_eq!(h.table.verify_shadow(t.hash, t.cold, &image), ShadowVerdict::SameKey);
    let t = h.table.shadow_of_winner(b1).expect("still open, verified");
    assert_eq!(t.verified_len, Some(image.len() as u32));
    assert_eq!(h.table.shadow_unverified(), 0);
    assert_eq!(h.table.len(), 1, "exact with the verified ticket open");
}

/// A recovered-life table (pre-life addresses below `origin`) with the
/// manifested file the refs count into.
fn recovered_table() -> TieredTable {
    let origin = LogicalAddr::from_raw(1 << 20).expect("fits");
    let mut t = TieredTable::new(
        AddressSpaceConfig {
            reserve_bytes: RING as usize,
            page_bytes: PAGE as usize,
            life_origin: origin,
        },
        DemotionConfig::for_budget(RING, PAGE),
        64,
    )
    .expect("reservation");
    t.seed_recovered_files(
        &[inf_log::TierFileMeta {
            id: 0,
            base: LogicalAddr::ZERO,
            data_len: 1 << 19,
            reason: inf_log::tier::SealReason::Recovered,
            path: Path::new("shard-0/cold/tier-000000.itier").to_path_buf(),
        }],
        7,
    );
    t
}

/// A record image for `key` (the bytes a tier file would hold), built
/// in a scratch table — the address is irrelevant to the image.
fn record_image(key: &[u8], value: &[u8]) -> Vec<u8> {
    let mut scratch = Harness::new();
    let addr = scratch.insert(key, value);
    let len = scratch.table.record(addr).encoded_len;
    scratch.table.record_bytes(addr, len).to_vec()
}

/// ADR-0093 A4 (the review's mis-pair, reconstructed): two RAM keys
/// with one hash beside a cold twin — the rebuild forms no ticket and
/// returns the slot as `Ambiguous`; the settle reads it and pairs it
/// with its **true** owner by full key, whichever of the two it is; a
/// third key's record is `Distinct` and stays.
#[test]
fn rebuild_settles_an_ambiguous_twin_against_its_true_owner() {
    let (k1, k2) = collision_pair(6);
    let hash = TieredTable::hash_key(&k1);
    let pre_life = LogicalAddr::from_raw(4096).expect("fits");
    for (twin_key, twin_value) in [(&k1, &b"one-old"[..]), (&k2, &b"two-old"[..])] {
        let mut t = recovered_table();
        t.apply_ref(hash, pre_life);
        let b1 = t.apply_image(&k1, b"one", hash).expect("fits");
        let b2 = t.apply_image(&k2, b"two", hash).expect("fits");
        let settle = t.rebuild_shadow_tickets();
        assert_eq!(t.shadow_pending(), 0, "no guess");
        assert_eq!(settle.len(), 1);
        assert_eq!(
            (settle[0].hash, settle[0].cold, settle[0].reason),
            (hash, pre_life, SettleReason::Ambiguous)
        );
        let image = record_image(twin_key, twin_value);
        assert_eq!(t.settle_rebuilt_slot(hash, pre_life, &image), SettleOutcome::SameKey);
        assert!(!t.contains_pair(hash, pre_life), "the twin settled");
        assert_eq!(t.shadow_pending(), 0);
        assert_eq!(t.len(), 2);
        let owner = if twin_key == &k1 { b1 } else { b2 };
        let other = if twin_key == &k1 { b2 } else { b1 };
        assert_eq!(
            t.take_displacement_origins(hash, owner).iter().map(|(o, _)| *o).collect::<Vec<_>>(),
            vec![pre_life.to_raw()],
            "chained into the true owner"
        );
        assert!(t.take_displacement_origins(hash, other).is_empty());
        let c = t.shadow_counters();
        assert_eq!(
            (c.rebuild_reads, c.rebuild_settled_same_key, c.rebuild_settled_distinct),
            (1, 1, 0)
        );
    }
    // A third key's record under the same hash: distinct, untouched.
    let mut t = recovered_table();
    t.apply_ref(hash, pre_life);
    t.apply_image(&k1, b"one", hash).expect("fits");
    t.apply_image(&k2, b"two", hash).expect("fits");
    assert_eq!(t.rebuild_shadow_tickets().len(), 1);
    let [_, _, k3] = forced_collision_triple(6);
    assert_eq!(TieredTable::hash_key(&k3), hash, "a third key with the same hash");
    let image = record_image(&k3, b"three");
    assert_eq!(t.settle_rebuilt_slot(hash, pre_life, &image), SettleOutcome::Distinct);
    assert!(t.contains_pair(hash, pre_life));
    assert_eq!(t.shadow_pending(), 0);
    assert_eq!(t.len(), 3, "three slots, three keys, no ticket");
    assert_eq!(t.shadow_counters().rebuild_settled_distinct, 1);
}

/// ADR-0093 A4/D7: the rebuild forms at most `SHADOW_TICKETS_CAP`
/// tickets; the excess pairs are returned `OverCap` and settle at boot
/// — the maps never exceed the cap, the settle list is the excess.
#[test]
fn rebuild_sends_pairs_beyond_the_cap_to_the_settle_list() {
    let mut t = recovered_table();
    let mut images: HashMap<u64, Vec<u8>> = HashMap::new();
    let mut addr = 64u64;
    for i in 0..=SHADOW_TICKETS_CAP {
        let key = format!("cap:{i}").into_bytes();
        let hash = TieredTable::hash_key(&key);
        let pre = LogicalAddr::from_raw(addr).expect("fits");
        t.apply_ref(hash, pre);
        images.insert(addr, record_image(&key, b"old"));
        t.apply_image(&key, b"new", hash).expect("fits");
        addr += 64;
    }
    let settle = t.rebuild_shadow_tickets();
    assert_eq!(t.shadow_pending(), SHADOW_TICKETS_CAP);
    assert_eq!(settle.len(), 1);
    assert_eq!(settle[0].reason, SettleReason::OverCap);
    assert_eq!(t.shadow_counters().rebuild_over_cap, 1);
    let slot = settle[0];
    let outcome = t.settle_rebuilt_slot(slot.hash, slot.cold, &images[&slot.cold.to_raw()]);
    assert_eq!(outcome, SettleOutcome::SameKey);
    assert!(!t.contains_pair(slot.hash, slot.cold), "the excess pair settled at boot");
    assert_eq!(t.len(), SHADOW_TICKETS_CAP + 1);
}

/// ADR-0093 A1 (verify/settle split): under a pinned walk the read
/// verifies the ticket and the settle is deferred; after the walk the
/// next round settles it **without a read**, and `len()` was exact
/// throughout.
#[test]
fn a_verified_ticket_settles_without_a_read_after_the_walk() {
    let mut h = Harness::new();
    let a = cold_key(&mut h, b"k", b"v1");
    let (_, b) = h.shadow_set(b"k", b"v2");
    h.table.begin_ckpt_walk(1);
    assert_eq!(h.table.shadow_work(4).len(), 1, "the read is legal under a walk (A1)");
    let hash = TieredTable::hash_key(b"k");
    let image = h.tier[&a.to_raw()].clone();
    assert_eq!(h.table.resolve_shadow(hash, a, &image), ShadowVerdict::Deferred);
    let t = h.table.shadow_of_winner(b).expect("open");
    assert_eq!(t.verified_len, Some(image.len() as u32));
    assert_eq!(h.table.shadow_unverified(), 0);
    assert_eq!(h.table.len(), 1, "exact: verified same-key");
    assert!(h.table.contains_pair(hash, a), "not settled under the walk");
    assert_eq!(h.table.shadow_counters().deferred_walk, 1);
    h.table.end_ckpt_walk();
    assert!(h.table.shadow_work(4).is_empty(), "settled, nothing to read");
    assert_eq!(h.table.shadow_pending(), 0);
    assert!(!h.table.contains_pair(hash, a));
    let c = h.table.shadow_counters();
    assert_eq!((c.settled_without_read, c.resolved_same_key, c.verified), (1, 1, 1));
    h.check_accounting();
}

/// ADR-0093 A6: the pinned-suffix peak folds on every tail advance
/// while a ticket is open — an unrelated write after registration moves
/// the peak, which the registration-time sample missed.
#[test]
fn the_pinned_peak_folds_on_unrelated_writes() {
    let mut h = Harness::new();
    cold_key(&mut h, b"k", b"v1");
    let (_, b) = h.shadow_set(b"k", b"v2");
    let at_registration = h.table.shadow_counters().pinned_bytes_peak;
    assert_eq!(at_registration, h.table.space().tail().to_raw() - b.to_raw());
    for i in 0..4u32 {
        h.insert(format!("unrelated:{i}").as_bytes(), &[0x5A; 900]);
    }
    let now = h.table.space().tail().to_raw() - b.to_raw();
    assert!(now > at_registration);
    assert_eq!(h.table.shadow_counters().pinned_bytes_peak, now, "the peak is the real peak");
    assert_eq!(h.table.shadow_pinned_bytes(), now);
}

/// ADR-0093 A5: a settle whose winner has no origin room is deferred
/// (verified, pinned) — never a panic — and settles once the winner's
/// next displacement drains its list.
#[test]
fn a_full_origin_list_defers_the_settle_until_the_winner_moves() {
    let key = b"k".to_vec();
    let hash = TieredTable::hash_key(&key);
    let mut t = recovered_table();
    let twins: Vec<LogicalAddr> =
        (1..=4u64).map(|i| LogicalAddr::from_raw(i * 4096).expect("fits")).collect();
    for twin in &twins {
        t.apply_ref(hash, *twin);
    }
    let winner = t.apply_image(&key, b"new", hash).expect("fits");
    assert!(t.rebuild_shadow_tickets().is_empty(), "one RAM sibling: four tickets");
    assert_eq!(t.shadow_pending(), 4);
    let image = record_image(&key, b"old");
    let mut verdicts = Vec::new();
    for twin in &twins {
        verdicts.push(t.resolve_shadow(hash, *twin, &image));
    }
    assert_eq!(
        verdicts,
        [
            ShadowVerdict::SameKey,
            ShadowVerdict::SameKey,
            ShadowVerdict::SameKey,
            ShadowVerdict::Deferred
        ]
    );
    assert_eq!(t.shadow_counters().deferred_origin, 1);
    assert_eq!(t.shadow_pending(), 1);
    assert_eq!(t.shadow_unverified(), 0);
    assert_eq!(t.len(), 1, "exact throughout");
    // The winner's displacement takes its origins; the settle proceeds.
    assert_eq!(t.take_displacement_origins(hash, winner).len(), 3);
    assert_eq!(t.shadow_settle_verified(), 1);
    assert_eq!(t.shadow_pending(), 0);
    assert!(!t.contains_pair(hash, twins[3]));
    assert_eq!(t.take_displacement_origins(hash, winner).len(), 1);
}

// ---- compaction over the real pipeline (MemFs) --------------------------

const NS: NsId = NsId(37);
const FILE_CAPACITY: u64 = 4 << 10;

struct Rig {
    table: TieredTable,
    fs: MemFs,
    flush: TierFlush<MemFs>,
}

impl Rig {
    fn new() -> Rig {
        // A zero mutable target: every page but the tail's seals, so the
        // first file fills, overflows and seals under a small corpus.
        let demote =
            DemotionConfig { mem_budget_bytes: 1 << 20, mutable_permille: 0, slice_bytes: PAGE };
        let fs = MemFs::new();
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
            fs.clone(),
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
        Rig { table, fs, flush }
    }

    fn maintain(&mut self) {
        loop {
            let sealed = self.table.seal_slice();
            let f = self.table.flush_slice(&mut self.flush).expect("flush slice");
            let released = self.table.release_slice();
            if sealed + released + f.appended_bytes + u64::from(f.gaps_crossed) == 0 {
                break;
            }
        }
    }

    fn read_cold(&self, addr: u64, len: usize) -> Option<Vec<u8>> {
        let contains = |base: u64, flen: u64| addr >= base && addr + len as u64 <= base + flen;
        let (base, path) = self
            .flush
            .sealed()
            .iter()
            .find(|m| contains(m.base.to_raw(), m.data_len))
            .map(|m| (m.base.to_raw(), m.path.clone()))
            .or_else(|| {
                let (_, base, _, durable_len, path) = self.flush.active()?;
                contains(base.to_raw(), durable_len).then(|| (base.to_raw(), path.to_path_buf()))
            })?;
        let image = self.fs.contents(&path)?;
        let (first, count, skip) = tier_frame_span(addr - base, len);
        let from = tier_frame_offset(first) as usize;
        let to = from + count as usize * TIER_FRAME_BYTES;
        let mut out = Vec::new();
        tier_extract(image.get(from..to)?, skip, len, &mut out).ok()?;
        Some(out)
    }

    /// The synchronous overwrite of a cold key (the pre-S37 shape).
    fn sync_overwrite_cold(&mut self, key: &[u8], value: &[u8]) {
        let hash = TieredTable::hash_key(key);
        let TieredLookup::Cold(addr) = self.table.lookup(key, hash, &[]) else { panic!("cold") };
        let head = self.read_cold(addr.to_raw(), TieredTable::RECORD_HEADER_LEN).expect("head");
        let len = TieredTable::record_len_from_header(&head);
        let image = self.read_cold(addr.to_raw(), len).expect("record");
        let parts = TieredTable::decode_record(&image);
        assert_eq!(parts.key, key);
        self.table
            .overwrite(key, value, hash, addr, parts.encoded_len, parts.version)
            .expect("fits");
    }
}

/// ADR-0093 D6 (I6): copy-forward defers a ticket's cold slot — counted,
/// the file not finalized — and relocates it after the ticket ends.
#[test]
fn compaction_defers_a_tickets_cold_slot_until_resolution() {
    let mut rig = Rig::new();
    rig.table.set_shadow_enabled(true);
    rig.table.set_compaction_config(CompactionConfig { dead_ratio_pct: 50, slice_bytes: 1 << 20 });
    for i in 0..8u32 {
        let key = format!("c:{i}");
        rig.table
            .insert(key.as_bytes(), &[0x44; 900], TieredTable::hash_key(key.as_bytes()))
            .expect("fits");
    }
    // Filler past the third commit page: the seal lands on page marks,
    // so the first file (c:0..c:3, 4 × 911 B) fills, overflows and seals.
    for i in 0..6u32 {
        let key = format!("f:{i}");
        rig.table
            .insert(key.as_bytes(), &[0x77; 900], TieredTable::hash_key(key.as_bytes()))
            .expect("fits");
    }
    rig.maintain();
    assert!(!rig.flush.sealed().is_empty(), "the first file sealed");
    assert!(
        matches!(
            rig.table.lookup(b"c:1", TieredTable::hash_key(b"c:1"), &[]),
            TieredLookup::Cold(_)
        ),
        "c:1 went cold"
    );
    let hash = TieredTable::hash_key(b"c:1");
    let ShadowProbe::One(a) = rig.table.shadow_probe(b"c:1", hash) else { panic!("cold") };
    rig.table.shadow_admit(hash, a, 920).expect("admitted");
    let b = rig.table.insert(b"c:1", &[0x55; 900], hash).expect("fits");
    rig.table.register_shadow(hash, a, b);
    // Kill the ticket's file-mates synchronously so the file crosses the
    // dead-ratio trigger with the ticket's slot still live in it.
    for key in [b"c:0".as_slice(), b"c:2", b"c:3"] {
        rig.sync_overwrite_cold(key, &[0x66; 900]);
    }
    rig.maintain();
    let CompactionWork::Read { file_id, addr, len } =
        rig.table.compaction_work(&rig.flush, false, 1 << 20)
    else {
        panic!("a candidate file exists");
    };
    let chunk = rig.read_cold(addr.to_raw(), usize::try_from(len).expect("fits")).expect("chunk");
    let applied = rig.table.compaction_apply(file_id, addr, &chunk);
    assert!(applied.deferred >= 1, "the ticket's slot deferred: {applied:?}");
    assert!(!applied.file_scanned, "a deferred record blocks finalization");
    assert!(rig.table.is_shadow_cold(a), "still slotted, still the ticket's");
    assert_eq!(rig.table.shadow_counters().compaction_deferred, 1);
    let TieredLookup::Ram(w) = rig.table.lookup(b"c:1", hash, &[]) else { panic!("winner") };
    assert_eq!(w, b, "the winner serves throughout");
    // Resolve: the slot is gone, the death exact, the file re-offers.
    let head = rig.read_cold(a.to_raw(), TieredTable::RECORD_HEADER_LEN).expect("head");
    let len = TieredTable::record_len_from_header(&head);
    let image = rig.read_cold(a.to_raw(), len).expect("exact");
    assert_eq!(rig.table.resolve_shadow(hash, a, &image), ShadowVerdict::SameKey);
    assert!(!rig.table.contains_pair(hash, a));
    let report = rig.table.space().report();
    assert_eq!(rig.table.live_bytes() + report.dead_bytes, report.allocated_bytes);
}
