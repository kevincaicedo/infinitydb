//! Shadow-slot reconciliation (M4.5-S37, ADR-0093): a plain `SET` whose
//! only exact-hash candidate is cold appends its record and leaves the
//! candidate slotted as a *shadow* — the new record is the key's only
//! logical value (a RAM-resident, key-verified slot outranks every cold
//! candidate in `lookup`'s probe order), and a MAINTAIN read verifies
//! the shadow later: the same read, the same full-key comparison, the
//! same exact death the synchronous path performs today, moved off the
//! command's critical path. Nothing is ever removed on hash evidence.
//!
//! **A ticket is ambiguous until its full-key read** (ADR-0093 A1): the
//! cold slot may be the key's old record or a different key with the
//! same 64 bits, and nobody knows which until the record is read. A
//! ticket is therefore *unverified* or *verified* (`verified_len`), and
//! reconciliation has two halves — **verify** (the read; legal under a
//! pinned checkpoint walk) and **settle** (no read: the exact removal,
//! death and origin chain; never under a walk). Every answer the engine
//! derives from a ticket is exact or waits: `DBSIZE` verifies before it
//! counts (the plane's fenced drain, A3), `SCAN` names the twin like any
//! cold slot (A3), the recovery rebuild reads the slots it cannot pair
//! by construction (A4).
//!
//! The ticket set is a **projection of the index** (L2): a ticket exists
//! exactly for a same-64-bit-hash pair of one RAM slot and one cold
//! slot, so it is rebuilt from the finished index at recovery-complete
//! ([`TieredTable::rebuild_shadow_tickets`]) and lost tickets lose
//! nothing. The rebuild is a **resumable cursor** (A4′, review of
//! 2026-08-28): one 16-slot probe group per step, one settle slot handed
//! to the caller at a time, no list of anything — its memory is the
//! group's scratch plus the ticket maps, which never exceed the cap
//! (`register_shadow` asserts it in release). Every bound is a named
//! constant with a counter (D7 as amended by A6), and every exhaustion
//! turns the eligible write back into the synchronous verify — slower,
//! never less correct.
//!
//! Invariants this module enforces mechanically (ADR-0093 §Invariants):
//! a winner is RAM-resident for the ticket's life (the record pin on
//! the release ceiling — `AddressSpace::set_record_pin`); a cold address
//! carries **one** ticket (`register_shadow` asserts in release, A2); a
//! ticket's cold address is never relocated (compaction and promotion
//! consult [`TieredTable::is_shadow_cold`]); a winner is never deleted
//! while a ticket names it (`TieredTable::delete` asserts); `len()` is
//! the index minus the open tickets, exact once every open ticket is
//! verified.

use std::collections::{BTreeMap, HashMap};
use std::fmt;

use inf_foundation::{LocalCounter, LogicalAddr};

use super::TieredTable;
use crate::address_space::AddrClass;
use crate::index::{GROUP, HomeGroupCursor};
use crate::tiered::TieredLookup;

/// Open tickets per table (ADR-0093 D7): above it new eligible writes
/// verify synchronously, and a recovery rebuild settles the excess pairs
/// at boot instead of ticketing them (A4′). ≈ 100 KiB of map entries at
/// the cap — a bound `register_shadow` asserts in release: every
/// producer checks it first, so the maps never hold more.
pub const SHADOW_TICKETS_CAP: usize = 4096;
/// Reconciliation reads in flight per table (D7).
pub const SHADOW_READS_IN_FLIGHT: usize = 4;
/// The pinned-suffix cap is `MEM-BUDGET / SHADOW_PIN_CAP_DIVISOR` (D7 as
/// amended by A6 — the throttle: above half of it the reconciler reads
/// Foreground, above it no new ticket is admitted; the structural bound
/// is the committed window, where writes park).
pub const SHADOW_PIN_CAP_DIVISOR: u64 = 8;
/// Approximate bytes per open ticket across the two maps (the L5 term
/// `shadow_bytes` reports `pending × this`): a `BTreeMap<(u64, u64), ()>`
/// entry plus a `HashMap<u64, ColdEntry>` entry with their overheads.
const SHADOW_TICKET_BYTES: u64 = 104;

/// The hashtag every forced-collision key starts with (ADR-0094 D3):
/// both keys of a pair route to one cell (the collision must meet inside
/// one table), and under the `collision-oracle` feature the hasher
/// hashes only the first 32 bytes of a 48-byte key with this prefix.
pub use inf_foundation::COLLISION_KEY_PREFIX;

/// Three distinct 48-byte keys — the [`COLLISION_KEY_PREFIX`] hashtag,
/// 16 tag-derived bytes, and a distinct 16-byte suffix each — with one
/// [`TieredTable::hash_key`] **under the `collision-oracle` feature**
/// (ADR-0093 A7 as amended by ADR-0094 D3): the collision oracle for the
/// store suite and the simulators; `tag` selects the triple. Ordinary
/// distinct keys in a shipping build, where no collision is
/// constructible. Never an engine capability.
#[must_use]
pub fn forced_collision_triple(tag: u64) -> [[u8; 48]; 3] {
    let head = tag.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let build = |suffix: u8| -> [u8; 48] {
        let mut out = [0u8; 48];
        out[..16].copy_from_slice(COLLISION_KEY_PREFIX);
        out[16..24].copy_from_slice(&head.to_le_bytes());
        out[24..32].copy_from_slice(&tag.to_le_bytes());
        out[32..].copy_from_slice(&[suffix; 16]);
        out
    };
    [build(b'a'), build(b'b'), build(b'c')]
}

/// The first two keys of [`forced_collision_triple`].
#[must_use]
pub fn forced_collision_pair(tag: u64) -> ([u8; 48], [u8; 48]) {
    let [first, second, _] = forced_collision_triple(tag);
    (first, second)
}

/// One open shadow: the key's hash, the unverified-or-verified cold
/// slot, and the RAM-resident winner.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ShadowTicket {
    pub hash: u64,
    pub cold: LogicalAddr,
    pub winner: LogicalAddr,
    /// `Some(len)` once a full-key read proved the cold record is the
    /// winner's key (its exact encoded length — the settle needs no
    /// read); `None` while the twin's identity is unknown (A1).
    pub verified_len: Option<u32>,
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

/// What the write path found for the key (D2 step 2, as amended by A2).
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
    /// Exactly one cold slot carries the key's 64-bit hash and no ticket
    /// names it: the shadow candidate.
    One(LogicalAddr),
    /// Exactly one cold slot carries the hash and it is already a
    /// ticket's cold address — a second key colliding with a ticketed
    /// slot (A2): the synchronous path's business, whose read tells the
    /// keys apart.
    Ticketed(LogicalAddr),
    /// Two or more cold slots carry the hash (a 64-bit collision on
    /// disk): the synchronous path's business.
    Many,
}

/// Why admission refused a shadow write (D2; each counted).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ShadowRefusal {
    /// The knob is off for this table.
    Off,
    /// A `DBSIZE` drain is fencing admission (A3): the unverified set
    /// may only shrink until the count is answered.
    Fence,
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
    /// The cold record carries the winner's key. From `verify_shadow`:
    /// the ticket is now verified (the settle needs no read). From
    /// `resolve_shadow`: settled — removed exactly, its death
    /// attributed, its address chained as the winner's origin.
    SameKey,
    /// A different key with the same 64-bit hash: both stay, the ticket
    /// ends.
    Collision,
    /// The ticket no longer exists (the key was deleted, or a verified
    /// path removed a slot it named): nothing changed.
    Stale,
    /// Verified same key, but the settle is deferred: a checkpoint walk
    /// is pinned (D5 as amended — a death attributed mid-walk could be
    /// serialized after the twin's ref and attributed twice at recovery)
    /// or the winner's origin list has no room (A5). The ticket stays
    /// verified; the next round without the obstacle settles it without
    /// a read.
    Deferred,
}

/// Why a rebuilt cold slot must be read before the cell serves (A4).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SettleReason {
    /// Two or more RAM-resident records carry the slot's hash: which
    /// one (if any) the twin belongs to is a full-key question.
    Ambiguous,
    /// Exactly one RAM sibling, but the ticket cap is reached: the pair
    /// is real and must not go unprotected, so it is settled now.
    OverCap,
}

/// A rebuilt cold slot the boot reads and settles (A4).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SettleSlot {
    pub hash: u64,
    pub cold: LogicalAddr,
    pub reason: SettleReason,
}

/// The outcome of settling one rebuilt slot by its full key (A4′): the
/// boot never registers a ticket for it — a same-key twin settles now,
/// a distinct key's record stays.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SettleOutcome {
    /// A RAM record carries the twin's key: removed exactly, its death
    /// attributed, its address chained as that record's origin.
    SameKey,
    /// No RAM record carries the twin's key: the slot is a distinct
    /// key's, untouched, no ticket.
    Distinct,
}

/// Why a rebuilt slot could not be settled (A4′) — corrupt-input class,
/// the recovery's fail-stop posture (ADR-0057), never a panic.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SettleError {
    /// The twin's owner already holds `RELOC_ORIGIN_CAP` origins. At
    /// boot the lists are empty (recovery builds a fresh table) and a
    /// checkpoint names at most one cold record per key, so a fourth
    /// same-key twin of one winner is input no engine wrote.
    OriginRoom { winner: LogicalAddr, cold: LogicalAddr },
}

impl fmt::Display for SettleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SettleError::OriginRoom { winner, cold } => write!(
                f,
                "rebuilt twin at {} is a fourth same-key cold record of the winner at {} (its \
                 origin list is full at boot — a checkpoint names one cold record per key)",
                cold.to_raw(),
                winner.to_raw()
            ),
        }
    }
}

impl std::error::Error for SettleError {}

/// Why [`TieredTable::rebuild_shadow_tickets`] stopped before the index
/// was fully walked: the caller's read failed on a slot, or the slot
/// could not be settled. Either is a recovery fail-stop for the server.
#[derive(Debug)]
pub enum ShadowRebuildError<E> {
    Read { slot: SettleSlot, cause: E },
    Settle { slot: SettleSlot, cause: SettleError },
}

impl<E: fmt::Display> fmt::Display for ShadowRebuildError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ShadowRebuildError::Read { slot, cause } => write!(
                f,
                "shadow rebuild: settle slot at {} ({:?}) unreadable: {cause}",
                slot.cold.to_raw(),
                slot.reason
            ),
            ShadowRebuildError::Settle { slot, cause } => write!(
                f,
                "shadow rebuild: settle slot at {} ({:?}): {cause}",
                slot.cold.to_raw(),
                slot.reason
            ),
        }
    }
}

impl<E: fmt::Debug + fmt::Display> std::error::Error for ShadowRebuildError<E> {}

/// The recovery rebuild's cursor (A4′): where the home-group walk stands
/// and the cold slots of the probe group it last stepped, not yet
/// classified. Fixed size — one probe group of scratch; the walk resumes
/// after every settle slot the caller is handed.
#[must_use = "a rebuild must be driven to completion before the cell serves"]
pub struct ShadowRebuild {
    /// The next home group to open once the current chain ends.
    group: usize,
    /// The chain in progress, if one is open.
    cursor: Option<HomeGroupCursor>,
    /// `(hash, cold address)` of the last stepped group's cold slots.
    scratch: [(u64, u64); GROUP],
    scratch_len: usize,
    done: bool,
}

/// Shadow observability (D8) — `INFO tiering` renders these; the A/B
/// and the DST oracles read them. Gauges (`pending`, `verified_pending`,
/// `pinned_bytes`, `pin_cap_bytes`, `bytes`, `enabled`) are filled by
/// [`TieredTable::shadow_counters`].
#[derive(Copy, Clone, Default, Debug, PartialEq, Eq)]
pub struct ShadowCounters {
    pub created: u64,
    /// Same-key settles (the exact removal), read-driven or read-free.
    pub resolved_same_key: u64,
    pub resolved_collision: u64,
    /// Reads whose full-key comparison verified a same-key twin.
    pub verified: u64,
    /// Settles that needed no read (a verified ticket past its deferral).
    pub settled_without_read: u64,
    pub stale: u64,
    pub read_errors: u64,
    pub reads_issued: u64,
    pub reads_foreground: u64,
    pub pending: u64,
    /// Gauge: open tickets already verified same-key (exact in `len()`).
    pub verified_pending: u64,
    pub pending_peak: u64,
    pub pinned_bytes: u64,
    /// Folded on every tail advance while a ticket is open (A6).
    pub pinned_bytes_peak: u64,
    pub pin_cap_bytes: u64,
    pub fallback_off: u64,
    pub fallback_fence: u64,
    pub fallback_multi: u64,
    /// The one exact candidate already carries a ticket (A2).
    pub fallback_ticketed: u64,
    pub fallback_tickets: u64,
    pub fallback_pin: u64,
    pub fallback_origin: u64,
    /// Inserts that skipped the read because no exact-hash cold slot
    /// existed (the fingerprint-only candidate was another key).
    pub exact_miss_inserts: u64,
    pub compaction_deferred: u64,
    pub promote_skip: u64,
    /// `scan_slots` slots that were a ticket's cold address, emitted
    /// like any cold slot (A3).
    pub scan_twins_emitted: u64,
    pub forced_by_delete: u64,
    /// Tickets retargeted to a later winner (the key was overwritten
    /// while its ticket was open).
    pub retargeted: u64,
    /// Tickets ended because a slot they named was removed by another
    /// verified path (a collision key deleted or moved).
    pub dropped_by_removal: u64,
    /// Settles deferred by a pinned checkpoint walk.
    pub deferred_walk: u64,
    /// Settles deferred by a full origin list (A5).
    pub deferred_origin: u64,
    /// `DBSIZE` drains raised (A3) and the twins they read.
    pub dbsize_drains: u64,
    pub dbsize_reads: u64,
    /// The recovery rebuild's boot reads (A4): slots read, settled as
    /// the same key, found distinct, and pairs beyond the cap.
    pub rebuild_reads: u64,
    pub rebuild_settled_same_key: u64,
    pub rebuild_settled_distinct: u64,
    pub rebuild_over_cap: u64,
    pub bytes: u64,
    /// Gauge: 1 per table with the arm on (the CONFIG fan's witness).
    pub enabled: u64,
    /// Gauge: 1 per table whose reconciler is paused (ADR-0093 A8 — the
    /// `tiered-shadow-reconcile no` witness).
    pub reconcile_paused: u64,
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
            verified,
            settled_without_read,
            stale,
            read_errors,
            reads_issued,
            reads_foreground,
            pending,
            verified_pending,
            pending_peak,
            pinned_bytes,
            pinned_bytes_peak,
            pin_cap_bytes,
            fallback_off,
            fallback_fence,
            fallback_multi,
            fallback_ticketed,
            fallback_tickets,
            fallback_pin,
            fallback_origin,
            exact_miss_inserts,
            compaction_deferred,
            promote_skip,
            scan_twins_emitted,
            forced_by_delete,
            retargeted,
            dropped_by_removal,
            deferred_walk,
            deferred_origin,
            dbsize_drains,
            dbsize_reads,
            rebuild_reads,
            rebuild_settled_same_key,
            rebuild_settled_distinct,
            rebuild_over_cap,
            bytes,
            enabled,
            reconcile_paused
        );
    }
}

/// The cold-address side of one ticket.
#[derive(Copy, Clone, Debug)]
struct ColdEntry {
    hash: u64,
    winner: u64,
    verified_len: Option<u32>,
}

/// The open tickets of one table (L1: cell-local, one owner).
pub(super) struct ShadowSet {
    /// `(winner, cold)`, ascending by winner: the front is the oldest
    /// unresolved winner — the record pin — and the read order.
    by_winner: BTreeMap<(u64, u64), ()>,
    /// `cold → (hash, winner, verified)`: the compaction/promotion/
    /// removal probe and the ticket's state.
    by_cold: HashMap<u64, ColdEntry>,
    /// Open tickets whose twin has not been read (A1).
    unverified: usize,
    /// Cold addresses whose read is in flight (bounded by
    /// [`SHADOW_READS_IN_FLIGHT`]).
    in_flight: Vec<u64>,
    /// `DBSIZE` drains in progress (A3): admission refuses while > 0.
    fence: u32,
    enabled: bool,
    /// `false` = paused (A8): `shadow_work` issues no reads and settles
    /// nothing; `DBSIZE` and `DEL` keep their own reads.
    reconcile: bool,
    counters: ShadowCounters,
    /// `scan_slots` runs on `&self` — interior-mutable like the
    /// resolver's `cold_resolves`.
    scan_twins: LocalCounter,
}

impl ShadowSet {
    pub(super) fn new() -> ShadowSet {
        ShadowSet {
            by_winner: BTreeMap::new(),
            by_cold: HashMap::new(),
            unverified: 0,
            in_flight: Vec::with_capacity(SHADOW_READS_IN_FLIGHT),
            fence: 0,
            enabled: false,
            reconcile: true,
            counters: ShadowCounters::default(),
            scan_twins: LocalCounter::new(),
        }
    }

    fn oldest_winner(&self) -> Option<u64> {
        self.by_winner.keys().next().map(|(winner, _)| *winner)
    }

    fn winner_tickets(&self, winner: u64) -> impl Iterator<Item = u64> + '_ {
        self.by_winner.range((winner, 0)..=(winner, u64::MAX)).map(|((_, cold), ())| *cold)
    }

    fn ticket(&self, cold: u64) -> Option<ShadowTicket> {
        self.by_cold.get(&cold).map(|entry| ShadowTicket {
            hash: entry.hash,
            cold: LogicalAddr::from_raw(cold).expect("slot addresses are 48-bit"),
            winner: LogicalAddr::from_raw(entry.winner).expect("slot addresses are 48-bit"),
            verified_len: entry.verified_len,
        })
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

    /// Pauses (`false`) or resumes the reconciler (ADR-0093 A8 — the
    /// `tiered-shadow-reconcile` CONFIG key): paused, `shadow_work` hands
    /// the plane nothing and settles nothing, so open tickets stay
    /// exactly as they are — the DST's lever for the open-ticket rows,
    /// an operator's pause. Bounded by the pin cap (new writes go
    /// synchronous) and the ticket cap; `DBSIZE`'s drain and `DEL`'s
    /// forced resolution keep their own reads.
    pub fn set_shadow_reconcile(&mut self, on: bool) {
        self.shadow.reconcile = on;
    }

    /// Whether the reconciler is paused (A8).
    #[inline]
    #[must_use]
    pub fn shadow_reconcile_paused(&self) -> bool {
        !self.shadow.reconcile
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

    /// Open tickets whose twin has not been read — the ones `len()` is
    /// not yet exact about (A1/A3).
    #[inline]
    #[must_use]
    pub fn shadow_unverified(&self) -> usize {
        self.shadow.unverified
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
        counters.verified_pending = (self.shadow_pending() - self.shadow.unverified) as u64;
        counters.pinned_bytes = self.shadow_pinned_bytes();
        counters.pinned_bytes_peak = counters.pinned_bytes_peak.max(counters.pinned_bytes);
        counters.pin_cap_bytes = self.shadow_pin_cap_bytes();
        counters.bytes = counters.pending * SHADOW_TICKET_BYTES;
        counters.scan_twins_emitted = self.shadow.scan_twins.get();
        counters.enabled = u64::from(self.shadow.enabled);
        counters.reconcile_paused = u64::from(!self.shadow.reconcile);
        counters
    }

    /// The write path's probe (D2 step 2, A2). A `Cold` answer from
    /// `lookup` is a fingerprint match; only the sidecar's 64-bit hash
    /// decides whether a cold slot can be *this* key, and a slot already
    /// under a ticket is never a second ticket's candidate.
    #[must_use]
    pub fn shadow_probe(&self, key: &[u8], hash: u64) -> ShadowProbe {
        debug_assert_eq!(hash, self.hash_key(key));
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
            (1, Some(addr)) if self.is_shadow_cold(addr) => ShadowProbe::Ticketed(addr),
            (1, Some(addr)) => ShadowProbe::One(addr),
            _ => ShadowProbe::Many,
        }
    }

    /// Admission (D2 steps 1, 3, 4, 5; A3's fence) for a shadow write of
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
        if self.shadow.fence > 0 {
            self.shadow.counters.fallback_fence += 1;
            return Err(ShadowRefusal::Fence);
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

    /// Counts a `Ticketed` probe the caller sent down the synchronous
    /// path (A2).
    pub fn note_shadow_ticketed(&mut self) {
        self.shadow.counters.fallback_ticketed += 1;
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

    /// Counts one twin read by a `DBSIZE` drain (A3).
    pub fn note_shadow_dbsize_read(&mut self) {
        self.shadow.counters.dbsize_reads += 1;
    }

    /// Raises or lowers the `DBSIZE` fence (A3): while any drain is in
    /// progress no new ticket is admitted, so the unverified set only
    /// shrinks and the drain terminates.
    ///
    /// # Panics
    /// Debug-panics on a lower without a raise.
    pub fn shadow_fence(&mut self, raise: bool) {
        if raise {
            self.shadow.fence += 1;
            self.shadow.counters.dbsize_drains += 1;
        } else {
            debug_assert!(self.shadow.fence > 0, "fence lowered without a raise");
            self.shadow.fence = self.shadow.fence.saturating_sub(1);
        }
    }

    /// Every open unverified ticket, oldest winner first (the `DBSIZE`
    /// drain's snapshot, A3 — bounded by the ticket cap).
    #[must_use]
    pub fn shadow_unverified_tickets(&self) -> Vec<ShadowTicket> {
        self.shadow_tickets().filter(|t| t.verified_len.is_none()).collect()
    }

    /// Registers the ticket `(hash, cold, winner)` (D2's last step, and
    /// the recovery rebuild's pair formation — D5). The winner is
    /// RAM-resident and the cold address cold; both slots exist exactly;
    /// the cold address carries no ticket (A2).
    ///
    /// # Panics
    /// Panics when a precondition fails — a ticket over a record the
    /// pin cannot keep, a slot that does not exist, or a second ticket
    /// on one cold address is a violated invariant, never an operating
    /// condition (the write path answers `Ticketed` first).
    pub fn register_shadow(&mut self, hash: u64, cold: LogicalAddr, winner: LogicalAddr) {
        let verified_len: Option<u32> = None;
        assert!(self.space.resolve(cold) == AddrClass::Cold, "shadow candidate is not cold");
        assert!(self.space.resolve(winner) != AddrClass::Cold, "shadow winner is not RAM-resident");
        assert!(self.index.contains_pair(hash, cold), "shadow candidate is not slotted");
        assert!(self.index.contains_pair(hash, winner), "shadow winner is not slotted");
        let (c, w) = (cold.to_raw(), winner.to_raw());
        assert!(!self.shadow.by_cold.contains_key(&c), "a cold address carries one ticket");
        // D7 as amended (A4′): the cap is mechanical here, not advisory —
        // admission refuses at it and the rebuild settles past it, so a
        // registration at the cap is a producer that skipped its check.
        assert!(
            self.shadow.by_winner.len() < SHADOW_TICKETS_CAP,
            "a ticket registered at SHADOW_TICKETS_CAP (the producer skipped admission)"
        );
        self.shadow.by_cold.insert(c, ColdEntry { hash, winner: w, verified_len });
        self.shadow.by_winner.insert((w, c), ());
        if verified_len.is_none() {
            self.shadow.unverified += 1;
        }
        self.shadow.counters.created += 1;
        let pending = self.shadow.by_winner.len() as u64;
        self.shadow.counters.pending_peak = self.shadow.counters.pending_peak.max(pending);
        self.sync_shadow_pin();
        self.shadow_note_alloc();
    }

    /// The first ticket naming `winner`, if any (the `DEL` path's probe,
    /// D3).
    #[must_use]
    pub fn shadow_of_winner(&self, winner: LogicalAddr) -> Option<ShadowTicket> {
        let cold = self.shadow.winner_tickets(winner.to_raw()).next()?;
        self.shadow.ticket(cold)
    }

    /// Whether `addr` is a ticket's cold address (compaction, promotion
    /// and the write path consult this — D6, A2).
    #[inline]
    #[must_use]
    pub fn is_shadow_cold(&self, addr: LogicalAddr) -> bool {
        !self.shadow.by_cold.is_empty() && self.shadow.by_cold.contains_key(&addr.to_raw())
    }

    /// Every open ticket, oldest winner first (tests and the DST oracle).
    pub fn shadow_tickets(&self) -> impl Iterator<Item = ShadowTicket> + '_ {
        self.shadow
            .by_winner
            .keys()
            .map(|(_, cold)| self.shadow.ticket(*cold).expect("by_winner and by_cold agree"))
    }

    /// Hands the plane up to `max` reconciliation reads (D4): oldest
    /// winner first, unverified tickets only, never one already in
    /// flight, never more than [`SHADOW_READS_IN_FLIGHT`] in flight.
    /// Verified tickets are **settled** here first without a read (A1)
    /// when no walk is pinned. The reads are marked in flight here;
    /// [`resolve_shadow`](Self::resolve_shadow) or
    /// [`shadow_read_failed`](Self::shadow_read_failed) clears them.
    pub fn shadow_work(&mut self, max: usize) -> Vec<ShadowRead> {
        if self.shadow.by_winner.is_empty() || !self.shadow.reconcile {
            return Vec::new();
        }
        // No settle under a pinned walk (D5 as amended; ADR-0059 D9-1's
        // rule for the same reason): a death attributed mid-walk can
        // land in the live-set section after the walk emitted the
        // twin's ref, and recovery would attribute it again. The reads
        // themselves are legal under a walk (A1): a verified ticket
        // settles the moment the walk ends.
        if self.space.walk_watermark().is_none() {
            self.shadow_settle_verified();
        }
        let free = SHADOW_READS_IN_FLIGHT.saturating_sub(self.shadow.in_flight.len()).min(max);
        if free == 0 || self.shadow.unverified == 0 {
            return Vec::new();
        }
        let foreground = self.shadow_pinned_bytes() >= self.shadow_pin_cap_bytes() / 2;
        let mut out = Vec::with_capacity(free);
        for (_, cold) in self.shadow.by_winner.keys() {
            if self.shadow.in_flight.contains(cold) {
                continue;
            }
            let ticket = self.shadow.ticket(*cold).expect("by_winner and by_cold agree");
            if ticket.verified_len.is_some() {
                continue;
            }
            out.push(ShadowRead { ticket, foreground });
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

    /// Settles every verified ticket whose obstacle is gone (no walk
    /// pinned — the caller checked; origin room re-checked per ticket).
    /// Returns how many settled.
    pub fn shadow_settle_verified(&mut self) -> usize {
        if self.shadow.unverified == self.shadow.by_winner.len() {
            return 0;
        }
        debug_assert!(self.space.walk_watermark().is_none(), "settle under a pinned walk");
        let verified: Vec<u64> = self
            .shadow
            .by_cold
            .iter()
            .filter(|(_, e)| e.verified_len.is_some())
            .map(|(c, _)| *c)
            .collect();
        let mut settled = 0usize;
        for cold in verified {
            if self.settle_ticket(cold) {
                self.shadow.counters.settled_without_read += 1;
                settled += 1;
            }
        }
        settled
    }

    /// A reconciliation read failed (I/O, CRC, a retired-file miss):
    /// the ticket stays, the next round retries (D4.3).
    pub fn shadow_read_failed(&mut self, cold: LogicalAddr) {
        self.shadow.in_flight.retain(|c| *c != cold.to_raw());
        self.shadow.counters.read_errors += 1;
    }

    /// The verdict without the settle (D3's `DEL` path, A3's `DBSIZE`
    /// drain): re-validates the ticket against the index after the
    /// suspension and compares the **full key** of the cold record with
    /// the ticket's *current* winner — the relation is "the twin versus
    /// the key's current RAM record", so a winner that moved under an
    /// overwrite is still the right comparand. `SameKey` marks the ticket
    /// verified (idempotent — an already-verified ticket answers without
    /// comparing); `Collision` ends the ticket (both records are keys);
    /// `Stale` (the ticket is gone) changes nothing.
    ///
    /// # Panics
    /// Panics when `image` is not exactly one record (the caller's
    /// framing is this crate's own vocabulary fed back).
    pub fn verify_shadow(&mut self, hash: u64, cold: LogicalAddr, image: &[u8]) -> ShadowVerdict {
        let c = cold.to_raw();
        self.shadow.in_flight.retain(|x| *x != c);
        let Some(entry) = self.shadow.by_cold.get(&c).copied() else {
            self.shadow.counters.stale += 1;
            return ShadowVerdict::Stale;
        };
        if entry.hash != hash {
            self.shadow.counters.stale += 1;
            return ShadowVerdict::Stale;
        }
        let winner = LogicalAddr::from_raw(entry.winner).expect("48-bit");
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
        if entry.verified_len.is_some() {
            return ShadowVerdict::SameKey;
        }
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
        let len = u32::try_from(image.len()).expect("record lengths fit u32");
        self.shadow.by_cold.get_mut(&c).expect("present").verified_len = Some(len);
        self.shadow.unverified -= 1;
        self.shadow.counters.verified += 1;
        ShadowVerdict::SameKey
    }

    /// Applies one read's verdict (D4): [`verify_shadow`]
    /// (Self::verify_shadow), then — same key, no walk pinned, origin
    /// room — settles: removes exactly the cold pair, attributes the
    /// exact death and chains the address (with its own origins) into
    /// the winner's relocation-origin list — the ADR-0059 D9 repair the
    /// next displacement stages. Under a pinned walk or a full origin
    /// list the verdict is `Deferred` and the ticket stays verified (A1,
    /// A5). `image` is the verbatim cold record.
    ///
    /// # Panics
    /// As [`verify_shadow`](Self::verify_shadow).
    pub fn resolve_shadow(&mut self, hash: u64, cold: LogicalAddr, image: &[u8]) -> ShadowVerdict {
        let verdict = self.verify_shadow(hash, cold, image);
        if verdict != ShadowVerdict::SameKey {
            return verdict;
        }
        if self.space.walk_watermark().is_some() {
            self.shadow.counters.deferred_walk += 1;
            return ShadowVerdict::Deferred;
        }
        if self.settle_ticket(cold.to_raw()) {
            ShadowVerdict::SameKey
        } else {
            ShadowVerdict::Deferred
        }
    }

    /// The settle of one verified ticket (A1): the synchronous path's
    /// removal on the same evidence (§I3 — exact pair, exact length),
    /// the death, the origin chain. `false` when the winner's origin
    /// list has no room (A5: deferred, counted; the ticket stays).
    ///
    /// # Panics
    /// Debug-panics on an unverified ticket or under a pinned walk (the
    /// callers check both).
    fn settle_ticket(&mut self, c: u64) -> bool {
        let entry = self.shadow.by_cold[&c];
        let len = entry.verified_len.expect("settle of an unverified ticket");
        if !self.settle_pair(entry.hash, c, entry.winner, len) {
            self.shadow.counters.deferred_origin += 1;
            return false;
        }
        self.drop_ticket(c);
        self.shadow.counters.resolved_same_key += 1;
        true
    }

    /// The settle itself, ticket or no ticket (the live path's verified
    /// ticket, the boot's rebuilt slot): remove the exact cold pair,
    /// attribute the exact death, chain the address and its own origins
    /// into the winner's list. `false` — nothing changed — when the
    /// winner's list has no room for them (`RELOC_ORIGIN_CAP`).
    fn settle_pair(&mut self, hash: u64, c: u64, w: u64, len: u32) -> bool {
        debug_assert!(self.space.walk_watermark().is_none(), "settle under a pinned walk");
        let cold = LogicalAddr::from_raw(c).expect("48-bit");
        let incoming = self.reloc_origins.get(&(hash, c)).map_or(0, Vec::len) + 1;
        let existing = self.reloc_origins.get(&(hash, w)).map_or(0, Vec::len);
        if existing + incoming > super::RELOC_ORIGIN_CAP {
            return false;
        }
        self.index.remove(hash, cold);
        self.note_death(cold, u64::from(len));
        let mut origins = self.reloc_origins.remove(&(hash, c)).unwrap_or_default();
        origins.push((c, self.live.ckpt_begun()));
        self.reloc_origins.entry((hash, w)).or_default().extend(origins);
        true
    }

    /// A slot at `addr` was removed by a verified path (delete,
    /// displacement replay, a collision key's overwrite): every ticket
    /// naming it as cold or winner ends (D5).
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
    /// — "the key's current RAM record"; a verified ticket stays
    /// verified — the twin's identity did not change); a ticket naming
    /// `old` as its cold address ends (its slot moved into RAM under a
    /// verified key — the collision key was overwritten).
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
            self.shadow.by_winner.remove(&(o, cold)).expect("listed");
            self.shadow.by_winner.insert((n, cold), ());
            let entry = self.shadow.by_cold.get_mut(&cold).expect("listed");
            debug_assert_eq!(entry.hash, hash, "a winner moves under its own key");
            entry.winner = n;
            self.shadow.counters.retargeted += 1;
        }
        self.sync_shadow_pin();
    }

    /// Opens the recovery rebuild of the ticket set (ADR-0093 D5 as
    /// amended by A4′) — the recovery-complete authority: a ticket **is**
    /// a same-64-bit-hash pair of one RAM slot and one cold slot, so after
    /// the checkpoint and the WAL tail have replayed, the tickets are
    /// exactly those pairs. Clears the set, in-flight, the fence and the
    /// pin; the counters are per-life observability and are not reset (a
    /// boot starts them at 0 anyway). Drive it with
    /// [`shadow_rebuild_next`](Self::shadow_rebuild_next) until `None`, or
    /// through [`rebuild_shadow_tickets`](Self::rebuild_shadow_tickets).
    pub fn begin_shadow_rebuild(&mut self) -> ShadowRebuild {
        self.shadow.by_winner.clear();
        self.shadow.by_cold.clear();
        self.shadow.unverified = 0;
        self.shadow.in_flight.clear();
        self.shadow.fence = 0;
        self.space.set_record_pin(None);
        ShadowRebuild {
            group: 0,
            cursor: None,
            scratch: [(0, 0); GROUP],
            scratch_len: 0,
            done: false,
        }
    }

    /// Advances the rebuild to the next slot the boot must **read and
    /// settle** before the cell serves, registering the tickets it can
    /// form on the way; `None` once the index is fully walked. One
    /// home-group chain at a time, one 16-slot probe group per step
    /// (`Index::scan_home_group_step`): for each **cold** slot the
    /// probe chain (`each_exact`) names its RAM siblings — no map of the
    /// index, no vector of cold slots, no settle list (O(1) scratch, the
    /// ticket maps ≤ the cap). Exactly one RAM sibling under the cap ⇒ a
    /// ticket (unverified: the twin may still be a collision key, and the
    /// verdict is the reconciler's ordinary read). Two or more RAM
    /// siblings (two live keys with one hash — which one, if any, the
    /// twin belongs to is a full-key question the review found answered
    /// by a guess), or the cap reached ⇒ the slot is returned for
    /// [`settle_rebuilt_slot`](Self::settle_rebuilt_slot), and the walk
    /// resumes after it (a settle removes a slot of a group already
    /// stepped; the chain's end never moves — `Index::remove` writes
    /// EMPTY only into groups that already hold one).
    ///
    /// Time is O(cold slots × the probe chain's length) — under the keyed
    /// hash (ADR-0094) the chain is the ordinary open-addressing bound at
    /// the table's load, not a value a client can grow.
    pub fn shadow_rebuild_next(&mut self, rebuild: &mut ShadowRebuild) -> Option<SettleSlot> {
        loop {
            // Classify the last stepped group's cold slots first.
            while rebuild.scratch_len > 0 {
                rebuild.scratch_len -= 1;
                let (hash, c) = rebuild.scratch[rebuild.scratch_len];
                let cold = LogicalAddr::from_raw(c).expect("slot addresses are 48-bit");
                let mut siblings = 0usize;
                let mut lowest: Option<LogicalAddr> = None;
                self.index.each_exact(hash, |sib| {
                    if self.space.resolve(sib) != AddrClass::Cold {
                        siblings += 1;
                        lowest = Some(lowest.map_or(sib, |l| l.min(sib)));
                    }
                });
                match (siblings, lowest) {
                    (0, _) => {}
                    (1, Some(winner)) if self.shadow.by_winner.len() < SHADOW_TICKETS_CAP => {
                        self.register_shadow(hash, cold, winner);
                    }
                    (1, Some(_)) => {
                        self.shadow.counters.rebuild_over_cap += 1;
                        return Some(SettleSlot { hash, cold, reason: SettleReason::OverCap });
                    }
                    _ => return Some(SettleSlot { hash, cold, reason: SettleReason::Ambiguous }),
                }
            }
            if rebuild.done {
                return None;
            }
            // Step one probe group of the open chain (or open the next).
            let mut cursor = match rebuild.cursor {
                Some(cursor) => cursor,
                None => {
                    if rebuild.group >= self.index.group_count() {
                        rebuild.done = true;
                        return None;
                    }
                    self.index.home_group_cursor(rebuild.group)
                }
            };
            let (space, scratch, len) =
                (&self.space, &mut rebuild.scratch, &mut rebuild.scratch_len);
            let more = self.index.scan_home_group_step(&mut cursor, |addr, hash| {
                if space.resolve(addr) == AddrClass::Cold {
                    scratch[*len] = (hash, addr.to_raw());
                    *len += 1;
                }
            });
            if more {
                rebuild.cursor = Some(cursor);
            } else {
                rebuild.cursor = None;
                rebuild.group += 1;
            }
        }
    }

    /// Drives a whole rebuild: every slot the walk hands back is read by
    /// `read` and settled by its full key; the first failure ends it.
    /// The server's `finish_tier_replay` and the simulators call this;
    /// its memory is the cursor's — no list of slots exists at any point.
    ///
    /// # Errors
    /// An unreadable slot (`read`'s error) or an unsettleable one — both
    /// the recovery's fail-stop class for the server.
    pub fn rebuild_shadow_tickets<E>(
        &mut self,
        mut read: impl FnMut(&SettleSlot) -> Result<Vec<u8>, E>,
    ) -> Result<(), ShadowRebuildError<E>> {
        let mut rebuild = self.begin_shadow_rebuild();
        while let Some(slot) = self.shadow_rebuild_next(&mut rebuild) {
            let image = read(&slot).map_err(|cause| ShadowRebuildError::Read { slot, cause })?;
            self.settle_rebuilt_slot(slot.hash, slot.cold, &image)
                .map_err(|cause| ShadowRebuildError::Settle { slot, cause })?;
        }
        Ok(())
    }

    /// Settles one slot the rebuild returned (A4′), by the twin's **full
    /// key**, without ever registering a ticket for it: the finished
    /// index is asked for a RAM record with that key — found ⇒ the twin
    /// is its old record, removed now with the exact death and the origin
    /// chain (the synchronous path's removal on the same evidence, §I3);
    /// not found ⇒ the slot is a distinct key's and nothing changes.
    /// `image` is the verbatim cold record. The boot runs with no walk
    /// pinned and empty origin lists, and a checkpoint names at most one
    /// cold record per key, so the origin room a same-key settle needs is
    /// always there — its absence is corrupt input, a typed error.
    ///
    /// # Errors
    /// [`SettleError::OriginRoom`] — the winner's list is full.
    ///
    /// # Panics
    /// Panics when the slot is not a cold exact pair, `image` is not
    /// exactly one record, or a walk is pinned — the caller read the slot
    /// the rebuild named, before serving.
    pub fn settle_rebuilt_slot(
        &mut self,
        hash: u64,
        cold: LogicalAddr,
        image: &[u8],
    ) -> Result<SettleOutcome, SettleError> {
        assert!(self.index.contains_pair(hash, cold), "settle of a slot that is not there");
        assert!(self.space.resolve(cold) == AddrClass::Cold, "settle of a RAM slot");
        assert_eq!(
            crate::record::encoded_len_from_header(image),
            image.len(),
            "settle image is not exactly one record"
        );
        assert!(!self.is_shadow_cold(cold), "settle of a ticketed slot");
        assert!(self.space.walk_watermark().is_none(), "a boot settle under a pinned walk");
        self.shadow.counters.rebuild_reads += 1;
        let key = TieredTable::decode_record(image).key;
        debug_assert_eq!(hash, self.hash_key(key), "a slot carries its record's hash");
        let TieredLookup::Ram(winner) = self.lookup(key, hash, &[]) else {
            self.shadow.counters.rebuild_settled_distinct += 1;
            return Ok(SettleOutcome::Distinct);
        };
        let len = u32::try_from(image.len()).expect("record lengths fit u32");
        if !self.settle_pair(hash, cold.to_raw(), winner.to_raw(), len) {
            return Err(SettleError::OriginRoom { winner, cold });
        }
        self.shadow.counters.rebuild_settled_same_key += 1;
        self.shadow.counters.resolved_same_key += 1;
        Ok(SettleOutcome::SameKey)
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

    /// Counts one `scan_slots` twin emitted (A3; `&self` — interior).
    pub(super) fn note_shadow_scan_twin(&self) {
        self.shadow.scan_twins.incr();
    }

    /// Folds the pinned suffix's peak on a tail advance (A6): one
    /// `is_empty` branch on the allocation path, nothing when no ticket
    /// is open.
    #[inline]
    pub(super) fn shadow_note_alloc(&mut self) {
        if self.shadow.by_winner.is_empty() {
            return;
        }
        let pinned = self.shadow_pinned_bytes();
        self.shadow.counters.pinned_bytes_peak = self.shadow.counters.pinned_bytes_peak.max(pinned);
    }

    fn drop_ticket(&mut self, cold: u64) {
        if let Some(entry) = self.shadow.by_cold.remove(&cold) {
            self.shadow.by_winner.remove(&(entry.winner, cold));
            if entry.verified_len.is_none() {
                self.shadow.unverified -= 1;
            }
        }
        self.shadow.in_flight.retain(|c| *c != cold);
        self.sync_shadow_pin();
    }

    fn sync_shadow_pin(&mut self) {
        let pin = self.shadow.oldest_winner().map(|w| LogicalAddr::from_raw(w).expect("48-bit"));
        self.space.set_record_pin(pin);
    }
}
