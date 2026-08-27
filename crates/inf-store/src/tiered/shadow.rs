//! Shadow-slot reconciliation (M4.5-S37, ADR-0093): a plain `SET` whose
//! only exact-hash candidate is cold appends its record and leaves the
//! candidate slotted as a *shadow* — the new record is the key's only
//! logical value (a RAM-resident, key-verified slot outranks every cold
//! candidate in `lookup`'s probe order), and a MAINTAIN read verifies
//! the shadow later: the same read, the same full-key comparison, the
//! same exact death the synchronous path performs today, moved off the
//! command's critical path. Nothing is ever removed on hash evidence.
//!
//! The ticket set is a **projection of the index** (L2): a ticket exists
//! exactly for a same-64-bit-hash pair of one RAM slot and one cold
//! slot, so it is rebuilt wherever recovery forms such a pair (the
//! replay appliers) and lost tickets lose nothing. Every bound is a
//! named constant with a counter (D7), and every exhaustion turns the
//! eligible write back into the synchronous verify — slower, never
//! less correct.
//!
//! Invariants this module enforces mechanically (ADR-0093 §Invariants):
//! a winner is RAM-resident for the ticket's life (the record pin on
//! the release ceiling — `AddressSpace::set_record_pin`); a ticket's
//! cold address is never relocated (compaction and promotion consult
//! [`TieredTable::is_shadow_cold`]); a winner is never deleted while a
//! ticket names it (`TieredTable::delete` asserts); `len()` is the
//! index minus the open tickets.

use std::collections::{BTreeMap, HashMap};

use inf_foundation::{LocalCounter, LogicalAddr};

use super::TieredTable;
use crate::address_space::AddrClass;
use crate::tiered::TieredLookup;

/// Open tickets per table (ADR-0093 D7): above it new eligible writes
/// verify synchronously. ≈ 100 KiB of map entries at the cap.
pub const SHADOW_TICKETS_CAP: usize = 4096;
/// Reconciliation reads in flight per table (D7).
pub const SHADOW_READS_IN_FLIGHT: usize = 4;
/// The pinned-suffix cap is `MEM-BUDGET / SHADOW_PIN_CAP_DIVISOR` (D7):
/// above it new eligible writes verify synchronously; above half of it
/// the reconciler reads Foreground.
pub const SHADOW_PIN_CAP_DIVISOR: u64 = 8;
/// Approximate bytes per open ticket across the two maps (the L5 term
/// `shadow_bytes` reports `pending × this`): a `BTreeMap<(u64, u64), u64>`
/// entry plus a `HashMap<u64, (u64, u64)>` entry with their overheads.
const SHADOW_TICKET_BYTES: u64 = 96;

/// One open shadow: the key's hash, the unverified cold slot, and the
/// RAM-resident winner.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ShadowTicket {
    pub hash: u64,
    pub cold: LogicalAddr,
    pub winner: LogicalAddr,
}

/// One reconciliation read the plane issues (D4): the ticket, and the
/// read class the pinned suffix asks for.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ShadowRead {
    pub ticket: ShadowTicket,
    /// `true` = the pinned suffix crossed half its cap: read Foreground
    /// (the oldest ticket is what releases RAM).
    pub foreground: bool,
}

/// What the write path found for the key (D2 step 2).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ShadowProbe {
    /// A RAM-resident, key-verified record: an ordinary overwrite.
    RamHit(LogicalAddr),
    /// No candidate of any kind: the ordinary insert path (untouched).
    Miss,
    /// `lookup` reported a cold candidate but no slot carries the key's
    /// 64-bit hash below the head: a fingerprint-only match, i.e.
    /// another key — an insert is correct without a read.
    NoCandidate,
    /// Exactly one cold slot carries the key's 64-bit hash: the shadow
    /// candidate.
    One(LogicalAddr),
    /// Two or more cold slots carry the hash (a 64-bit collision on
    /// disk): the synchronous path's business.
    Many,
}

/// Why admission refused a shadow write (D2; each counted).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ShadowRefusal {
    /// The knob is off for this table.
    Off,
    /// The ticket cap.
    Tickets,
    /// The pinned-suffix cap.
    Pin,
    /// The candidate's origin list has no room for one more entry.
    Origin,
}

/// The reconciler's verdict on one read (D4).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ShadowVerdict {
    /// The cold record carried the winner's key: removed exactly, its
    /// death attributed, its address chained as the winner's origin.
    SameKey,
    /// A different key with the same 64-bit hash: both stay.
    Collision,
    /// The ticket no longer describes the index (the key was deleted or
    /// its winner moved past the read): nothing changed.
    Stale,
    /// A checkpoint walk is pinned: the death is not attributed under a
    /// walk (D5 as amended — the walk may already have emitted the
    /// twin's ref and would serialize its death in the live-set section
    /// after it; recovery would then re-form the pair and attribute the
    /// death twice). The ticket stays; the next round after the walk
    /// resolves it.
    Deferred,
}

/// Shadow observability (D8) — `INFO tiering` renders these; the A/B
/// and the DST oracles read them. Gauges (`pending`, `pinned_bytes`,
/// `pin_cap_bytes`, `bytes`) are filled by [`TieredTable::shadow_counters`].
#[derive(Copy, Clone, Default, Debug, PartialEq, Eq)]
pub struct ShadowCounters {
    pub created: u64,
    pub resolved_same_key: u64,
    pub resolved_collision: u64,
    pub stale: u64,
    pub read_errors: u64,
    pub reads_issued: u64,
    pub reads_foreground: u64,
    pub pending: u64,
    pub pending_peak: u64,
    pub pinned_bytes: u64,
    pub pinned_bytes_peak: u64,
    pub pin_cap_bytes: u64,
    pub fallback_off: u64,
    pub fallback_multi: u64,
    pub fallback_tickets: u64,
    pub fallback_pin: u64,
    pub fallback_origin: u64,
    /// Inserts that skipped the read because no exact-hash cold slot
    /// existed (the fingerprint-only candidate was another key).
    pub exact_miss_inserts: u64,
    pub compaction_deferred: u64,
    pub promote_skip: u64,
    pub scan_skipped: u64,
    pub forced_by_delete: u64,
    /// Tickets retargeted to a later winner (the key was overwritten
    /// while its ticket was open).
    pub retargeted: u64,
    /// Tickets ended because a slot they named was removed by another
    /// verified path (a collision key deleted or moved).
    pub dropped_by_removal: u64,
    /// Reads whose verdict was deferred by a pinned checkpoint walk.
    pub deferred_walk: u64,
    pub bytes: u64,
    /// Gauge: 1 per table with the arm on (the CONFIG fan's witness).
    pub enabled: u64,
}

impl ShadowCounters {
    /// Field-wise fold for the cell aggregate (`INFO tiering`).
    /// Saturating: a scrape must never panic a serving cell.
    pub fn add(&mut self, ns: ShadowCounters) {
        macro_rules! fold {
            ($($field:ident),*) => { $( self.$field = self.$field.saturating_add(ns.$field); )* };
        }
        fold!(
            created,
            resolved_same_key,
            resolved_collision,
            stale,
            read_errors,
            reads_issued,
            reads_foreground,
            pending,
            pending_peak,
            pinned_bytes,
            pinned_bytes_peak,
            pin_cap_bytes,
            fallback_off,
            fallback_multi,
            fallback_tickets,
            fallback_pin,
            fallback_origin,
            exact_miss_inserts,
            compaction_deferred,
            promote_skip,
            scan_skipped,
            forced_by_delete,
            retargeted,
            dropped_by_removal,
            deferred_walk,
            bytes,
            enabled
        );
    }
}

/// The open tickets of one table (L1: cell-local, one owner).
pub(super) struct ShadowSet {
    /// `(winner, cold) → hash`, ascending by winner: the front is the
    /// oldest unresolved winner — the record pin — and the read order.
    by_winner: BTreeMap<(u64, u64), u64>,
    /// `cold → (hash, winner)`: the compaction/promotion/removal probe.
    by_cold: HashMap<u64, (u64, u64)>,
    /// Cold addresses whose read is in flight (bounded by
    /// [`SHADOW_READS_IN_FLIGHT`]).
    in_flight: Vec<u64>,
    enabled: bool,
    counters: ShadowCounters,
    /// `scan_slots` runs on `&self` — interior-mutable like the
    /// resolver's `cold_resolves`.
    scan_skipped: LocalCounter,
}

impl ShadowSet {
    pub(super) fn new() -> ShadowSet {
        ShadowSet {
            by_winner: BTreeMap::new(),
            by_cold: HashMap::new(),
            in_flight: Vec::with_capacity(SHADOW_READS_IN_FLIGHT),
            enabled: false,
            counters: ShadowCounters::default(),
            scan_skipped: LocalCounter::new(),
        }
    }

    fn oldest_winner(&self) -> Option<u64> {
        self.by_winner.keys().next().map(|(winner, _)| *winner)
    }

    fn winner_tickets(&self, winner: u64) -> impl Iterator<Item = u64> + '_ {
        self.by_winner.range((winner, 0)..=(winner, u64::MAX)).map(|((_, cold), _)| *cold)
    }
}

impl TieredTable {
    /// Whether eligible writes may take the shadow path (ADR-0093 D8:
    /// the `tiered-shadow-overwrite` CONFIG key, pushed per cell).
    #[inline]
    #[must_use]
    pub fn shadow_enabled(&self) -> bool {
        self.shadow.enabled
    }

    /// Flips shadow admission (hot — the `push_pressure` fan). Off is
    /// inert for new writes; open tickets keep reconciling (D8).
    pub fn set_shadow_enabled(&mut self, on: bool) {
        self.shadow.enabled = on;
    }

    /// The pinned-suffix cap (D7): `MEM-BUDGET / 8`, never below four
    /// commit pages.
    #[must_use]
    pub fn shadow_pin_cap_bytes(&self) -> u64 {
        (self.demote.mem_budget_bytes / SHADOW_PIN_CAP_DIVISOR).max(4 * self.space.page_bytes())
    }

    /// Open tickets.
    #[inline]
    #[must_use]
    pub fn shadow_pending(&self) -> usize {
        self.shadow.by_winner.len()
    }

    /// The pinned suffix: `tail − oldest unresolved winner` (0 when no
    /// ticket is open) — the RAM the release watermark may not pass.
    #[must_use]
    pub fn shadow_pinned_bytes(&self) -> u64 {
        self.shadow.oldest_winner().map_or(0, |w| self.space.tail().to_raw() - w)
    }

    /// This table's shadow counters with the gauges filled.
    #[must_use]
    pub fn shadow_counters(&self) -> ShadowCounters {
        let mut counters = self.shadow.counters;
        counters.pending = self.shadow_pending() as u64;
        counters.pinned_bytes = self.shadow_pinned_bytes();
        counters.pin_cap_bytes = self.shadow_pin_cap_bytes();
        counters.bytes = counters.pending * SHADOW_TICKET_BYTES;
        counters.scan_skipped = self.shadow.scan_skipped.get();
        counters.enabled = u64::from(self.shadow.enabled);
        counters
    }

    /// The write path's probe (D2 step 2). A `Cold` answer from `lookup`
    /// is a fingerprint match; only the sidecar's 64-bit hash decides
    /// whether a cold slot can be *this* key.
    #[must_use]
    pub fn shadow_probe(&self, key: &[u8], hash: u64) -> ShadowProbe {
        debug_assert_eq!(hash, Self::hash_key(key));
        match self.lookup(key, hash, &[]) {
            TieredLookup::Ram(addr) => return ShadowProbe::RamHit(addr),
            TieredLookup::Miss => return ShadowProbe::Miss,
            TieredLookup::Cold(_) => {}
        }
        let mut exact: Option<LogicalAddr> = None;
        let mut count = 0usize;
        self.index.each_exact(hash, |addr| {
            if self.space.resolve(addr) == AddrClass::Cold {
                count += 1;
                exact.get_or_insert(addr);
            }
        });
        match (count, exact) {
            (0, _) => ShadowProbe::NoCandidate,
            (1, Some(addr)) => ShadowProbe::One(addr),
            _ => ShadowProbe::Many,
        }
    }

    /// Admission (D2 steps 1, 3, 4, 5) for a shadow write of
    /// `encoded_len` bytes over the cold candidate `cold`. Pure apart
    /// from the refusal counters; the caller then inserts and registers
    /// inside the same borrow.
    ///
    /// # Errors
    /// The first refusal in order — the caller runs the synchronous
    /// verify instead.
    pub fn shadow_admit(
        &mut self,
        hash: u64,
        cold: LogicalAddr,
        encoded_len: usize,
    ) -> Result<(), ShadowRefusal> {
        if !self.shadow.enabled {
            self.shadow.counters.fallback_off += 1;
            return Err(ShadowRefusal::Off);
        }
        if self.shadow.by_winner.len() >= SHADOW_TICKETS_CAP {
            self.shadow.counters.fallback_tickets += 1;
            return Err(ShadowRefusal::Tickets);
        }
        if self.shadow_pinned_bytes() + encoded_len as u64 > self.shadow_pin_cap_bytes() {
            self.shadow.counters.fallback_pin += 1;
            return Err(ShadowRefusal::Pin);
        }
        let origins = self.reloc_origins.get(&(hash, cold.to_raw())).map_or(0, Vec::len);
        if origins + 1 > super::RELOC_ORIGIN_CAP {
            self.shadow.counters.fallback_origin += 1;
            return Err(ShadowRefusal::Origin);
        }
        Ok(())
    }

    /// Counts a `Many` probe the caller sent down the synchronous path.
    pub fn note_shadow_multi(&mut self) {
        self.shadow.counters.fallback_multi += 1;
    }

    /// Counts an insert that skipped the read on exact-hash evidence
    /// ([`ShadowProbe::NoCandidate`] after a fingerprint-only `Cold`).
    pub fn note_shadow_exact_miss_insert(&mut self) {
        self.shadow.counters.exact_miss_inserts += 1;
    }

    /// Counts a `DEL`/`GETDEL` that resolved a ticket synchronously (D3).
    pub fn note_shadow_forced_delete(&mut self) {
        self.shadow.counters.forced_by_delete += 1;
    }

    /// Registers the ticket `(hash, cold, winner)` (D2's last step, and
    /// the recovery appliers' pair formation — D5). The winner is
    /// RAM-resident and the cold address cold; both slots exist exactly.
    ///
    /// # Panics
    /// Panics when either precondition fails — a ticket over a record
    /// the pin cannot keep or a slot that does not exist is a violated
    /// invariant, never an operating condition.
    pub fn register_shadow(&mut self, hash: u64, cold: LogicalAddr, winner: LogicalAddr) {
        assert!(self.space.resolve(cold) == AddrClass::Cold, "shadow candidate is not cold");
        assert!(self.space.resolve(winner) != AddrClass::Cold, "shadow winner is not RAM-resident");
        assert!(self.index.contains_pair(hash, cold), "shadow candidate is not slotted");
        assert!(self.index.contains_pair(hash, winner), "shadow winner is not slotted");
        let (c, w) = (cold.to_raw(), winner.to_raw());
        if self.shadow.by_cold.insert(c, (hash, w)).is_some() {
            debug_assert!(false, "a cold address carries one ticket");
        }
        self.shadow.by_winner.insert((w, c), hash);
        self.shadow.counters.created += 1;
        let pending = self.shadow.by_winner.len() as u64;
        self.shadow.counters.pending_peak = self.shadow.counters.pending_peak.max(pending);
        self.sync_shadow_pin();
        let pinned = self.shadow_pinned_bytes();
        self.shadow.counters.pinned_bytes_peak = self.shadow.counters.pinned_bytes_peak.max(pinned);
    }

    /// The first ticket naming `winner`, if any (the `DEL` path's probe,
    /// D3).
    #[must_use]
    pub fn shadow_of_winner(&self, winner: LogicalAddr) -> Option<ShadowTicket> {
        let w = winner.to_raw();
        self.shadow.winner_tickets(w).next().map(|cold| ShadowTicket {
            hash: self.shadow.by_cold[&cold].0,
            cold: LogicalAddr::from_raw(cold).expect("slot addresses are 48-bit"),
            winner,
        })
    }

    /// Whether `addr` is a ticket's cold address (compaction, promotion
    /// and enumeration consult this — D6).
    #[inline]
    #[must_use]
    pub fn is_shadow_cold(&self, addr: LogicalAddr) -> bool {
        !self.shadow.by_cold.is_empty() && self.shadow.by_cold.contains_key(&addr.to_raw())
    }

    /// Every open ticket, oldest winner first (tests and the DST oracle).
    pub fn shadow_tickets(&self) -> impl Iterator<Item = ShadowTicket> + '_ {
        self.shadow.by_winner.iter().map(|((winner, cold), hash)| ShadowTicket {
            hash: *hash,
            cold: LogicalAddr::from_raw(*cold).expect("48-bit"),
            winner: LogicalAddr::from_raw(*winner).expect("48-bit"),
        })
    }

    /// Hands the plane up to `max` reconciliation reads (D4): oldest
    /// winner first, never one already in flight, never more than
    /// [`SHADOW_READS_IN_FLIGHT`] in flight. The reads are marked in
    /// flight here; [`resolve_shadow`](Self::resolve_shadow) or
    /// [`shadow_read_failed`](Self::shadow_read_failed) clears them.
    pub fn shadow_work(&mut self, max: usize) -> Vec<ShadowRead> {
        let free = SHADOW_READS_IN_FLIGHT.saturating_sub(self.shadow.in_flight.len()).min(max);
        if free == 0 || self.shadow.by_winner.is_empty() {
            return Vec::new();
        }
        // No resolution under a pinned walk (D5 as amended; ADR-0059
        // D9-1's rule for the same reason): a death attributed mid-walk
        // can land in the live-set section after the walk emitted the
        // twin's ref, and recovery would attribute it again.
        if self.space.walk_watermark().is_some() {
            return Vec::new();
        }
        let foreground = self.shadow_pinned_bytes() >= self.shadow_pin_cap_bytes() / 2;
        let mut out = Vec::with_capacity(free);
        for ((winner, cold), hash) in &self.shadow.by_winner {
            if self.shadow.in_flight.contains(cold) {
                continue;
            }
            out.push(ShadowRead {
                ticket: ShadowTicket {
                    hash: *hash,
                    cold: LogicalAddr::from_raw(*cold).expect("48-bit"),
                    winner: LogicalAddr::from_raw(*winner).expect("48-bit"),
                },
                foreground,
            });
            if out.len() == free {
                break;
            }
        }
        for read in &out {
            self.shadow.in_flight.push(read.ticket.cold.to_raw());
        }
        self.shadow.counters.reads_issued += out.len() as u64;
        if foreground {
            self.shadow.counters.reads_foreground += out.len() as u64;
        }
        out
    }

    /// A reconciliation read failed (I/O, CRC, a retired-file miss):
    /// the ticket stays, the next round retries (D4.3).
    pub fn shadow_read_failed(&mut self, cold: LogicalAddr) {
        self.shadow.in_flight.retain(|c| *c != cold.to_raw());
        self.shadow.counters.read_errors += 1;
    }

    /// The verdict without the resolution (D3's `DEL` path): re-validates
    /// the ticket against the index after the suspension and compares
    /// the **full key**. `Collision` ends the ticket (both records are
    /// keys); `SameKey` changes nothing — the caller removes the twin
    /// through its own marker path (`delete`), which attributes the death
    /// consistently with a pinned walk; `Stale` changes nothing.
    ///
    /// # Panics
    /// Panics when `image` is not exactly one record (the caller's
    /// framing is this crate's own vocabulary fed back).
    pub fn verify_shadow(
        &mut self,
        hash: u64,
        cold: LogicalAddr,
        winner: LogicalAddr,
        image: &[u8],
    ) -> ShadowVerdict {
        let (c, w) = (cold.to_raw(), winner.to_raw());
        self.shadow.in_flight.retain(|x| *x != c);
        let current = self.shadow.by_cold.get(&c).copied();
        if current != Some((hash, w)) {
            self.shadow.counters.stale += 1;
            return ShadowVerdict::Stale;
        }
        if !self.index.contains_pair(hash, cold) || !self.index.contains_pair(hash, winner) {
            // The pair no longer exists (a removal path that missed the
            // hook would be a bug — the debug assert names it; the
            // release path drops the dead ticket and moves on).
            debug_assert!(false, "an open ticket names a slot that is gone");
            self.drop_ticket(c);
            self.shadow.counters.stale += 1;
            return ShadowVerdict::Stale;
        }
        assert!(self.space.resolve(winner) != AddrClass::Cold, "a pinned winner went cold");
        assert_eq!(
            crate::record::encoded_len_from_header(image),
            image.len(),
            "shadow image is not exactly one record"
        );
        let same_key = {
            let cold_key = TieredTable::decode_record(image).key;
            self.record(winner).key == cold_key
        };
        if !same_key {
            self.drop_ticket(c);
            self.shadow.counters.resolved_collision += 1;
            return ShadowVerdict::Collision;
        }
        ShadowVerdict::SameKey
    }

    /// Applies one read's verdict (D4): [`verify_shadow`]
    /// (Self::verify_shadow), then — same key, no walk pinned — removes
    /// exactly the cold pair, attributes the exact death and chains the
    /// address (with its own origins) into the winner's relocation-origin
    /// list — the ADR-0059 D9 repair the next displacement stages.
    /// Under a pinned walk the verdict is `Deferred` and the ticket
    /// stays (D5 as amended). `image` is the verbatim cold record.
    ///
    /// # Panics
    /// As [`verify_shadow`](Self::verify_shadow).
    pub fn resolve_shadow(
        &mut self,
        hash: u64,
        cold: LogicalAddr,
        winner: LogicalAddr,
        image: &[u8],
    ) -> ShadowVerdict {
        let verdict = self.verify_shadow(hash, cold, winner, image);
        if verdict != ShadowVerdict::SameKey {
            return verdict;
        }
        if self.space.walk_watermark().is_some() {
            self.shadow.counters.deferred_walk += 1;
            return ShadowVerdict::Deferred;
        }
        let (c, w) = (cold.to_raw(), winner.to_raw());
        // The synchronous path's removal, performed on the same
        // evidence (ADR-0093 §I3): exact pair, exact length.
        self.index.remove(hash, cold);
        self.note_death(cold, image.len() as u64);
        let mut origins = self.reloc_origins.remove(&(hash, c)).unwrap_or_default();
        origins.push((c, self.live.ckpt_begun()));
        let chain = self.reloc_origins.entry((hash, w)).or_default();
        chain.extend(origins);
        assert!(chain.len() <= super::RELOC_ORIGIN_CAP, "shadow resolution exceeds the origin cap");
        self.drop_ticket(c);
        self.shadow.counters.resolved_same_key += 1;
        ShadowVerdict::SameKey
    }

    /// A slot at `addr` was removed by a verified path (delete,
    /// displacement replay, a collision key's overwrite): every ticket
    /// naming it as cold or winner ends (D5). The pair-formation hooks
    /// re-register a pair the paired mutation re-forms.
    pub(super) fn shadow_note_removed(&mut self, addr: LogicalAddr) {
        if self.shadow.by_winner.is_empty() {
            return;
        }
        let a = addr.to_raw();
        let mut dropped = 0u64;
        if self.shadow.by_cold.contains_key(&a) {
            self.drop_ticket(a);
            dropped += 1;
        }
        let colds: Vec<u64> = self.shadow.winner_tickets(a).collect();
        for cold in colds {
            self.drop_ticket(cold);
            dropped += 1;
        }
        self.shadow.counters.dropped_by_removal += dropped;
    }

    /// Ends every ticket naming `winner` without touching any slot (the
    /// replayed `Delete` of a winner whose twin the crashed life told
    /// apart — D5; the twin is another key and stays).
    pub(super) fn shadow_drop_winner_tickets(&mut self, winner: LogicalAddr) {
        if self.shadow.by_winner.is_empty() {
            return;
        }
        let colds: Vec<u64> = self.shadow.winner_tickets(winner.to_raw()).collect();
        for cold in colds {
            self.drop_ticket(cold);
            self.shadow.counters.dropped_by_removal += 1;
        }
    }

    /// The slot `(hash, old)` was repointed to `new` (an overwrite or a
    /// relocation): tickets naming `old` as their winner follow it (D3
    /// — "the key's current RAM record"); a ticket naming `old` as its
    /// cold address ends (its slot moved into RAM under a verified key —
    /// the collision key was overwritten).
    pub(super) fn shadow_note_moved(&mut self, hash: u64, old: LogicalAddr, new: LogicalAddr) {
        if self.shadow.by_winner.is_empty() {
            return;
        }
        let (o, n) = (old.to_raw(), new.to_raw());
        if self.shadow.by_cold.contains_key(&o) {
            self.drop_ticket(o);
            self.shadow.counters.dropped_by_removal += 1;
        }
        let colds: Vec<u64> = self.shadow.winner_tickets(o).collect();
        for cold in colds {
            let ticket_hash = self.shadow.by_winner.remove(&(o, cold)).expect("listed");
            debug_assert_eq!(ticket_hash, hash, "a winner moves under its own key");
            self.shadow.by_winner.insert((n, cold), ticket_hash);
            self.shadow.by_cold.insert(cold, (ticket_hash, n));
            self.shadow.counters.retargeted += 1;
        }
        self.sync_shadow_pin();
    }

    /// Rebuilds the entire ticket set from the index (ADR-0093 D5) — the
    /// recovery-complete authority: a ticket **is** a same-64-bit-hash
    /// pair of one RAM slot and one cold slot, so after the checkpoint
    /// and the WAL tail have replayed into the final index, the tickets
    /// are exactly those pairs. Rebuilding from the finished index —
    /// rather than tracking incrementally through replay's overwrites,
    /// where a winner's address moves under the ticket — is the design's
    /// "projection of the index" made literal, and it cannot leave an
    /// orphaned slot. Clears in-flight and every counter-invisible bit of
    /// per-ticket state; the counters (created, verdicts) are per-life
    /// observability and are not reset (a boot starts them at 0 anyway).
    /// Control-plane cost, O(index): the checkpoint walk's cadence.
    ///
    /// One RAM slot with several exact-hash cold siblings forms one
    /// ticket per cold slot, all against that winner; the pin follows the
    /// oldest winner as always.
    pub fn rebuild_shadow_tickets(&mut self) {
        self.shadow.by_winner.clear();
        self.shadow.by_cold.clear();
        self.shadow.in_flight.clear();
        // Home-group scan: every live slot, emitted once (the checkpoint
        // walker's primitive — no record read for the classification, the
        // sidecar hash and the resolver decide).
        let groups = self.index.group_count();
        let mut ram: std::collections::HashMap<u64, LogicalAddr> = std::collections::HashMap::new();
        let mut colds: Vec<(u64, LogicalAddr)> = Vec::new();
        for g in 0..groups {
            self.index.scan_home_group_ext(g, |addr, hash| {
                if self.space.resolve(addr) == AddrClass::Cold {
                    colds.push((hash, addr));
                } else {
                    // One RAM slot per key (the index invariant); a
                    // duplicate hash here would be a different key with
                    // the same 64 bits — vanishingly rare, and either RAM
                    // winner is a live record of *a* key, so the pair is
                    // still "cold twin vs a RAM record of this hash".
                    ram.insert(hash, addr);
                }
            });
        }
        for (hash, cold) in colds {
            if let Some(&winner) = ram.get(&hash) {
                self.register_shadow(hash, cold, winner);
            }
        }
    }

    /// Counts one compaction record deferred for being a ticket's cold
    /// address (D6).
    pub(super) fn note_shadow_compaction_deferred(&mut self) {
        self.shadow.counters.compaction_deferred += 1;
    }

    /// Counts one promotion skipped for the same reason (D6).
    pub(super) fn note_shadow_promote_skip(&mut self) {
        self.shadow.counters.promote_skip += 1;
    }

    /// Counts one `scan_slots` slot skipped (D3; `&self` — interior).
    pub(super) fn note_shadow_scan_skipped(&self) {
        self.shadow.scan_skipped.incr();
    }

    fn drop_ticket(&mut self, cold: u64) {
        if let Some((_, winner)) = self.shadow.by_cold.remove(&cold) {
            self.shadow.by_winner.remove(&(winner, cold));
        }
        self.shadow.in_flight.retain(|c| *c != cold);
        self.sync_shadow_pin();
    }

    fn sync_shadow_pin(&mut self) {
        let pin = self.shadow.oldest_winner().map(|w| LogicalAddr::from_raw(w).expect("48-bit"));
        self.space.set_record_pin(pin);
    }
}
