//! `CellStore` record lifecycle: the store's construction, the record
//! choke points every mutation funnels through, the expiry schedule they
//! drive (ADR-0008 A1), and the eviction policy hooks.
//!
//! The record table and its expiry schedule are one value, a
//! [`RecordIndex`] (I2, I9). Outside this module it derefs to [`Index`]
//! for reads and lends the schedule only as `&ExpirySchedule`; its
//! constructor, both fields and every insert, replace, remove, grow and
//! reset are private here. So a record enters or leaves the table, and a
//! TTL fact moves, only through a function of this module, and no reset
//! replaces the table without its schedule. Each function runs its
//! schedule transition:
//!
//! - [`write_record_carrying`](CellStore::write_record_carrying) — every
//!   record write; after the write succeeds it compares the replaced
//!   record's deadline with the written one;
//! - `release_record` — every record death, behind `free_record` and
//!   `free_record_uncovered`; it reads the dying record's deadline;
//! - three bypasses, each with its own node half: RENAME's source removal
//!   ([`remove_renamed_source`](CellStore::remove_renamed_source), which
//!   runs the death transition), the wheel's reap (its node half is the
//!   fire's answer), and the table reset of `FLUSH*` and `reserve_keys`
//!   ([`reset_records`](CellStore::reset_records), which resets the
//!   schedule with the table).
//!
//! A rewrite that keeps its deadline changes nothing and pays nothing.

use core::ops::Deref;

use inf_alloc::Arena;

use super::*;
use crate::index_alias::{AliasWalk, Candidate, alias_view};
#[cfg(feature = "doc")]
use crate::index_maint::{AliasCtx, DeathHook};
use crate::limits::{EXPIRY_SWEEP_CHUNK_SLOTS, IDX_ALIAS_GROUP_MAX};
use crate::schedule::{ExpirySchedule, Fire, PassEnd, Removal, Survivors, Transition};
use crate::wheel::{FireNext, Placement};

/// The record table behind the choke points and the expiry schedule it
/// drives (ADR-0008 A1 I2, I9). It derefs to [`Index`] for reads and has
/// no `DerefMut`; the schedule is lent out only shared. Its constructor
/// and fields are private to this module, so the rest of the `store`
/// tree can neither mutate a record slot or a TTL fact nor replace one
/// half without the other.
pub(crate) struct RecordIndex {
    table: Index,
    schedule: ExpirySchedule,
}

impl Deref for RecordIndex {
    type Target = Index;

    #[inline]
    fn deref(&self) -> &Index {
        &self.table
    }
}

impl RecordIndex {
    fn new(keys: usize, start_ms: u64, nodes_max: WheelNodesMax) -> RecordIndex {
        RecordIndex {
            table: Index::with_capacity(keys),
            schedule: ExpirySchedule::new(start_ms, nodes_max),
        }
    }

    /// The expiry schedule, read-only: its census, gauges and cursor.
    #[inline]
    pub(super) fn schedule(&self) -> &ExpirySchedule {
        &self.schedule
    }
}

/// What the wheel's reap borrows from the store while the schedule ticks
/// (the schedule itself is the one field it cannot reach — I3).
struct ReapCtx<'a> {
    arena: &'a mut Arena,
    index: &'a mut Index,
    docs: &'a mut DocStore,
    #[cfg(feature = "doc")]
    idx: &'a mut crate::index_maint::CellIndexes,
    hasher: KeyHasher,
    #[cfg(feature = "doc")]
    max_matches: u32,
}

/// One fire's view of its key hash's group.
enum GroupFire {
    /// ADR-0139 D9 `Over`: nothing is decided on an unanswered question.
    Over,
    Complete {
        members: u32,
        reaped: u32,
        next_deadline_ms: Option<u64>,
        least_deadline_ms: Option<u64>,
    },
}

/// A slice's fire tallies, added to `StoreStats` once the tick returns.
#[derive(Default)]
struct FireTally {
    reaped: u64,
    stale: u64,
    over: u64,
}

impl CellStore {
    /// A store with an empty record table and an empty expiry schedule —
    /// built here, because only this module constructs a [`RecordIndex`].
    pub fn new(cfg: StoreConfig) -> CellStore {
        let evict = EvictState { rng: cfg.evict_seed, ..EvictState::default() };
        CellStore {
            arena: Arena::new(cfg.arena),
            // Cursor 0: the first tick fast-forwards to `now` (empty wheel).
            index: RecordIndex::new(cfg.initial_keys.max(64), 0, cfg.wheel_nodes_max),
            stats: StoreStats::default(),
            evict,
            docs: DocStore::new(&cfg),
            idx: crate::index_maint::CellIndexes::new(),
            cfg,
        }
    }

    // ---- active expiry (M1-E2, ADR-0008 A1) ----

    /// One budgeted expiry MAINTAIN slice (M1-S05): advance the wheel
    /// toward `now`, each fire reaping its group's expired members (rule
    /// 5), then walk the sweep while it is owed (rule 6). Bounded by
    /// `budget` on fires, cursor steps and sweep slots, so a 1M-same-second
    /// storm cannot cliff the loop; a slice's sweep stops at a pass
    /// boundary.
    pub fn expire_tick(&mut self, now: Nanos, budget: ExpiryBudget) -> ExpiryStats {
        let now_ms = now.0 / 1_000_000;
        let mut out = ExpiryStats::default();
        let tick = self.wheel_tick(now, budget, &mut out);
        let fires_left = budget.max_fires.saturating_sub(tick.fired);
        self.sweep_slice(now, fires_left, budget.max_sweep_slots, &mut out);
        out.steps = tick.steps;
        out.lag_ms =
            if tick.caught_up { 0 } else { now_ms.saturating_sub(self.index.schedule.cursor_ms()) };
        out.armed = self.index.schedule.armed();
        out.tombstones = self.index.schedule.tombstones();
        out.sweep = self.index.schedule.sweep_state();
        out
    }

    /// The wheel half of a slice: every fire enumerates its hash's group
    /// once (ADR-0139 D9, nothing excluded) and reaps its expired members.
    fn wheel_tick(
        &mut self,
        now: Nanos,
        budget: ExpiryBudget,
        out: &mut ExpiryStats,
    ) -> crate::wheel::TickStats {
        let mut tally = FireTally::default();
        #[cfg(feature = "doc")]
        let max_matches = self.cfg.doc_max_path_matches;
        let hasher = self.cfg.hasher;
        let CellStore { arena, index, docs, idx, .. } = self;
        let RecordIndex { table, schedule } = index;
        #[cfg(not(feature = "doc"))]
        let _ = &idx;
        let mut ctx = ReapCtx {
            arena,
            index: table,
            docs,
            #[cfg(feature = "doc")]
            idx,
            hasher,
            #[cfg(feature = "doc")]
            max_matches,
        };
        let tick = schedule.tick(now.0 / 1_000_000, budget, |hash| {
            fire_answer(fire_group(&mut ctx, hash, now), &mut tally)
        });
        self.stats.expired_active += tally.reaped;
        self.stats.wheel_stale += tally.stale;
        self.stats.expiry_alias_over += tally.over;
        self.stats.wheel_refiled += u64::from(tick.refiled);
        out.reaped = tally.reaped;
        out.stale = tally.stale;
        out.refiled = u64::from(tick.refiled);
        tick
    }

    /// The sweep half of a slice (rule 6): walks index slots in chunks of
    /// `EXPIRY_SWEEP_CHUNK_SLOTS` while a pass is owed, within `slots_max`
    /// slots and `fires_max` reaps (checked between chunks), and stops at
    /// the pass's end.
    fn sweep_slice(&mut self, now: Nanos, fires_max: u32, slots_max: u32, out: &mut ExpiryStats) {
        let (capacity, rebuilds) = (self.index.capacity(), self.index.rebuilds());
        let Some(mut at) = self.index.schedule.sweep_begin(now.0 / 1_000_000, capacity, rebuilds)
        else {
            out.sweep_stop = SweepStop::NotOwed;
            return;
        };
        let mut slots_left = slots_max as usize;
        loop {
            if at.slots_left == 0 {
                let end =
                    self.index.schedule.sweep_end(self.index.capacity(), self.index.rebuilds());
                if end == PassEnd::Voided {
                    self.stats.sweep_passes_voided += 1;
                }
                out.sweep_stop = SweepStop::PassEnd;
                return;
            }
            let span = EXPIRY_SWEEP_CHUNK_SLOTS.min(at.slots_left).min(slots_left);
            if span == 0 || out.swept >= u64::from(fires_max) {
                out.sweep_stop = SweepStop::Budget;
                return;
            }
            let next = self.sweep_chunk(now, at.cursor, span, out);
            slots_left -= span;
            out.sweep_slots += span as u32;
            at = self.index.schedule.sweep_walked(next, span);
        }
    }

    /// One chunk: an expired record is reaped through the eviction-class
    /// death path (never bracket-covered, ADR-0139 D4); a live record with
    /// a deadline is placed. Returns the slot after the chunk.
    fn sweep_chunk(
        &mut self,
        now: Nanos,
        cursor: usize,
        span: usize,
        out: &mut ExpiryStats,
    ) -> usize {
        let mut found = [None; EXPIRY_SWEEP_CHUNK_SLOTS];
        let mut count = 0;
        let next = self.index.live_walk(cursor, span, |addr| {
            found[count] = Some(addr);
            count += 1;
        });
        for addr in found.into_iter().flatten() {
            let record = record_at(&self.arena, addr);
            let Some(deadline_ms) = record.expire_at_ms() else { continue };
            let hash = self.hash_key(record.key());
            if record.is_expired(now) {
                let len = record.encoded_len();
                self.free_record_uncovered(hash, addr, len);
                self.stats.expired_active += 1;
                self.stats.expired_swept += 1;
                out.swept += 1;
            } else if self.index.schedule.sweep_place(hash, deadline_ms) == Placement::Refused {
                self.stats.wheel_fallback += 1;
            }
        }
        next
    }

    /// The drain predicate (ADR-0008 A1 O3): the wheel has caught up to
    /// `now` and the sweep owes nothing a drain frozen at `now` must wait
    /// for. A pure read — the drain ticks until it holds.
    #[must_use]
    pub fn expiry_settled(&self, now: Nanos) -> bool {
        let now_ms = now.0 / 1_000_000;
        self.index.schedule.cursor_ms() > now_ms
            && self.index.schedule.sweep_state().settled_since(now_ms)
    }

    /// The wheel's standing debt in ms at `now` (0 with nothing scheduled
    /// — an idle wheel snaps to `now` on its next tick, so a stale cursor
    /// there is not backlog). Pure read, no tick: the per-store term of
    /// the keyspace-wide `expiry_debt` fold (F-L05-03).
    #[must_use]
    pub fn expiry_lag_ms(&self, now: Nanos) -> u64 {
        if self.index.schedule.armed() == 0 {
            return 0;
        }
        (now.0 / 1_000_000).saturating_sub(self.index.schedule.cursor_ms())
    }

    // ---- test-support reads (ADR-0008 A1's oracles) ----

    /// Live wheel nodes, tombstones excluded (the per-op `armed ≤
    /// ttl_live` check of O1/O2).
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn wheel_armed(&self) -> u64 {
        self.index.schedule.armed()
    }

    /// O(N) schedule audit: the wheel's links walked independently of its
    /// counters (O1's orphans, the tombstones actually linked), and, while
    /// the sweep is idle, every record with a deadline checked against its
    /// hash's node (I7 records with no node; I4 late nodes). Both are
    /// idle-scoped: a walking sweep owes its records a visit.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn expiry_audit(&self) -> ExpiryAudit {
        let schedule = &self.index.schedule;
        let (orphans, tombstones) = schedule.audit_links();
        let sweep_idle = schedule.sweep_state() == SweepState::Idle;
        let mut audit = ExpiryAudit {
            armed: schedule.armed(),
            tombstones,
            ttl_live: schedule.ttl_live(),
            orphans,
            sweep_idle,
            unscheduled: 0,
            late: 0,
        };
        if !sweep_idle {
            return audit;
        }
        let arena = &self.arena;
        self.index.live_walk(0, self.index.capacity(), |addr| {
            let record = record_at(arena, addr);
            let Some(deadline_ms) = record.expire_at_ms() else { return };
            match schedule.node_deadline(self.hash_key(record.key())) {
                None => audit.unscheduled += 1,
                Some(filed) => audit.late += u64::from(filed > deadline_ms),
            }
        });
        audit
    }

    /// The sweep pass under way as `(begin slot, cursor slot)` (the
    /// mid-pass leg of ADR-0008 A1 rule 6).
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn sweep_pass_slots(&self) -> Option<(usize, usize)> {
        self.index.schedule.sweep_pass_slots()
    }

    /// The index slot holding `key`.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn key_index_slot(&self, key: &[u8]) -> Option<usize> {
        let hash = self.hash_key(key);
        let arena = &self.arena;
        let addr = self.index.find(hash, |addr| record_at(arena, addr).key() == key)?;
        self.index.slot_of(hash, addr)
    }

    /// Index slot capacity.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn index_capacity(&self) -> usize {
        self.index.capacity()
    }

    // ---- eviction mechanism (M1-S06; policy logic lives in `evict.rs`) ----

    /// Applies an eviction policy: flips the access-tracking mode and
    /// allocates/frees the CMS (8 KiB only while LFU is selected).
    pub fn set_eviction_policy(&mut self, policy: EvictionPolicy) {
        self.evict.set_policy(policy);
    }

    #[inline]
    pub fn eviction_policy(&self) -> EvictionPolicy {
        self.evict.policy
    }

    /// Logical bytes this store costs (live records + index + wheel + CMS)
    /// — what `maxmemory` pressure compares against (M1-S07), the Redis
    /// `used_memory` shape. Live (not resident) bytes are the comparable:
    /// slab chunks stay mapped and recycle, so resident is monotone while
    /// eviction must be able to bring pressure *down*. The wheel term is
    /// its nodes in use, not its pool capacity (ADR-0008 A1 rule 7). The
    /// RSS story is the slack bound: resident ≤ live-at-peak + class
    /// slack, asserted by the M1-S07 pressure test and gated on the
    /// reference box.
    pub fn used_bytes(&self) -> u64 {
        let r = self.report();
        r.records_live_bytes
            + r.index_bytes
            + r.wheel_live_bytes
            + r.evict_bytes
            + r.doc_tape_bytes
            + r.doc_arena_bytes
    }

    /// Evicts at most one victim under the active policy (bounded candidate
    /// window, `samples` per selection). The pressure driver loops this.
    pub fn evict_step(&mut self, samples: u32, now: Nanos) -> EvictStats {
        evict::evict_one(self, samples, now)
    }

    /// Periodic eviction maintenance: CMS Morris-counter decay on the
    /// injected clock (MAINTAIN slice).
    pub fn evict_maintain(&mut self, now: Nanos) {
        evict::maybe_decay(&mut self.evict, now);
    }

    /// `OBJECT FREQ` under an LFU policy: the CMS estimate (Morris-scaled —
    /// recorded deviation: Redis reports its own log-counter scale).
    pub fn object_freq(&mut self, key: &[u8], now: Nanos) -> Option<u8> {
        self.resolve(key, now)?;
        let hash = self.hash_key(key);
        Some(self.evict.cms.as_ref().map_or(0, |cms| cms.estimate(hash)))
    }

    /// CLOCK aging: drop one reference generation (eviction sweep).
    pub(crate) fn age_record(&mut self, addr: ArenaAddr) {
        let head = self.arena.bytes_mut(addr, 1);
        head[0] = flags_ref_decrement(head[0]);
    }

    /// Reaps a record the eviction sweep found already expired. An
    /// eviction entry point: never bracket-covered (ADR-0139 D4).
    pub(crate) fn reap_expired_at(&mut self, hash: u64, addr: ArenaAddr, len: usize) {
        self.free_record_uncovered(hash, addr, len);
        self.note_reap_lazy();
    }

    /// Removes an eviction victim (counted separately from expirations).
    /// An eviction entry point: never bracket-covered (ADR-0139 D4).
    pub(crate) fn evict_record(&mut self, hash: u64, addr: ArenaAddr, len: usize) {
        self.free_record_uncovered(hash, addr, len);
        self.stats.evicted_keys += 1;
    }

    // ---- the death choke point ----

    /// Free one record completely: index entry, record bytes, and any
    /// document payload behind it (the ADR-0037 D3 choke point). Every
    /// delete and lazy reap funnels here; eviction funnels through
    /// [`free_record_uncovered`](Self::free_record_uncovered).
    ///
    /// Record-death hook (ADR-0072 D6): a dying document's index entries
    /// are removed here — the last moment its values are readable —
    /// unless this death is bracket-covered: a write-set key, by **full
    /// key**, dying inside its own command with no index pruned
    /// (ADR-0139 D4). Zero-index stores pay one cached branch.
    pub(crate) fn free_record(&mut self, hash: u64, addr: ArenaAddr, len: usize) {
        #[cfg(feature = "doc")]
        if self.idx.is_active() {
            let CellStore { arena, idx, .. } = self;
            let hook = idx.death_hook_wanted(hash, record_at(arena, addr).key());
            self.run_death_hook(hook, hash, addr);
        }
        self.release_record(hash, addr, len);
    }

    /// [`free_record`](Self::free_record) for the eviction entry points
    /// (`evict_record`, `reap_expired_at`, the expiry sweep). Eviction is
    /// reachable only from the OOM gate and the MAINTAIN pass — never
    /// from a command body — so a death here precedes the mutation: the
    /// hook always runs, and an open bracket forgets what `old` claimed
    /// for the victim (ADR-0139 D4). There is no coverage question to
    /// ask, so this path never consults the prune mask for it.
    pub(crate) fn free_record_uncovered(&mut self, hash: u64, addr: ArenaAddr, len: usize) {
        #[cfg(feature = "doc")]
        if self.idx.is_active() {
            let CellStore { arena, idx, .. } = self;
            let hook = idx.note_uncovered_death(hash, record_at(arena, addr).key());
            self.run_death_hook(hook, hash, addr);
        }
        self.release_record(hash, addr, len);
    }

    #[cfg(feature = "doc")]
    fn run_death_hook(&mut self, hook: DeathHook, hash: u64, addr: ArenaAddr) {
        let mask = match hook {
            DeathHook::Covered => return,
            DeathHook::All => u64::MAX,
            DeathHook::Masked(mask) => mask,
        };
        let max_matches = self.cfg.doc_max_path_matches;
        let hasher = self.cfg.hasher;
        let CellStore { arena, index, docs, idx, .. } = self;
        let ctx = AliasCtx { arena, index, docs, hasher };
        idx.remove_doc_entries(&ctx, hash, addr, mask, max_matches);
    }

    /// The death transition (ADR-0008 A1 rule 2): the record leaves the
    /// index first, so the group enumeration no longer sees it.
    fn release_record(&mut self, hash: u64, addr: ArenaAddr, len: usize) {
        let payload = doc::payload_of(&self.arena, addr, len);
        self.index.table.remove(hash, addr);
        let deadline = record_at(&self.arena, addr).expire_at_ms();
        self.arena.free(addr, len);
        self.docs.release(payload);
        self.record_died(hash, deadline);
    }

    /// RENAME's source removal: the value's handle moved to the target,
    /// so no payload is released (ADR-0037 D3) — the one `free_record`
    /// bypass besides the wheel's reap. It runs the death transition.
    pub(crate) fn remove_renamed_source(&mut self, hash: u64, addr: ArenaAddr, len: usize) {
        self.index.table.remove(hash, addr);
        let deadline = record_at(&self.arena, addr).expire_at_ms();
        self.arena.free(addr, len);
        self.record_died(hash, deadline);
    }

    fn record_died(&mut self, hash: u64, deadline: Option<u64>) {
        let hasher = self.cfg.hasher;
        let arena: &Arena = &self.arena;
        let RecordIndex { table, schedule } = &mut self.index;
        let table: &Index = table;
        let removal =
            schedule.on_death(hash, deadline, || survivors(arena, table, hasher, hash, None));
        if removal == Removal::Over {
            self.stats.expiry_alias_over += 1;
        }
    }

    /// Replaces the record table with an empty one of `keys` capacity and
    /// resets the schedule with it (`FLUSH*`, and `reserve_keys` on an
    /// empty store): no record remains, so nothing is scheduled or owed.
    pub(super) fn reset_records(&mut self, keys: usize, start_ms: u64) {
        self.index.table = Index::with_capacity(keys);
        self.index.schedule.reset(start_ms);
    }

    /// An expire-on-read reap (the death transition already ran).
    #[inline]
    pub(crate) fn note_reap_lazy(&mut self) {
        self.stats.expired_lazy += 1;
    }

    // ---- reads ----

    /// Index lookup + expire-on-read: returns the live record's address and
    /// encoded length, reaping it if its deadline passed.
    pub(crate) fn resolve(&mut self, key: &[u8], now: Nanos) -> Option<(ArenaAddr, usize)> {
        self.resolve_hashed(key, self.hash_key(key), now)
    }

    #[inline]
    pub(super) fn resolve_hashed(
        &mut self,
        key: &[u8],
        hash: u64,
        now: Nanos,
    ) -> Option<(ArenaAddr, usize)> {
        match self.lookup_hashed(key, hash, now) {
            Lookup::Live(addr, len) => Some((addr, len)),
            Lookup::Reaped(_) | Lookup::Absent => None,
        }
    }

    /// [`resolve_hashed`](Self::resolve_hashed) that also names the address
    /// it freed: a batched caller holding unverified candidate addresses
    /// must invalidate the one that was reaped, not the one it probed to
    /// (F-L05-04).
    pub(super) fn lookup_hashed(&mut self, key: &[u8], hash: u64, now: Nanos) -> Lookup {
        let arena = &self.arena;
        let Some(addr) = self.index.find(hash, |addr| record_at(arena, addr).key() == key) else {
            return Lookup::Absent;
        };
        let view = record_at(arena, addr);
        let len = view.encoded_len();
        if view.is_expired(now) {
            self.free_record(hash, addr, len);
            self.note_reap_lazy();
            return Lookup::Reaped(addr);
        }
        self.touch_access(hash, addr);
        Lookup::Live(addr, len)
    }

    /// Eviction access tracking (M1-S06): one cached branch when no LRU/LFU
    /// policy is active (the M1-S07 hot-path rule). CLOCK saturates the
    /// in-record reference bits (one OR on a line the access already
    /// pulled); LFU Morris-bumps the CMS with one injected-stream roll.
    #[inline]
    pub(super) fn touch_access(&mut self, hash: u64, addr: ArenaAddr) {
        match self.evict.tracking {
            Tracking::None => {}
            Tracking::Clock => {
                let head = self.arena.bytes_mut(addr, 1);
                head[0] = flags_ref_saturate(head[0]);
            }
            Tracking::Lfu => {
                let roll = self.evict.next_roll();
                if let Some(cms) = self.evict.cms.as_mut() {
                    cms.touch(hash, roll);
                }
            }
        }
    }

    // ---- the write choke point ----

    pub(super) fn write_record(
        &mut self,
        key: &[u8],
        existing: Option<(ArenaAddr, usize)>,
        spec: RecordSpec<'_>,
    ) -> Result<(), OpError> {
        self.write_record_releasing(key, existing, spec)
    }

    /// [`write_record_carrying`](Self::write_record_carrying) plus the
    /// ADR-0037 D3 overwrite rule: any document payload behind `existing`
    /// is captured first and released only after the write succeeds — a
    /// failed write leaves the old record and its payload untouched.
    pub(crate) fn write_record_releasing(
        &mut self,
        key: &[u8],
        existing: Option<(ArenaAddr, usize)>,
        spec: RecordSpec<'_>,
    ) -> Result<(), OpError> {
        let old_payload = existing.map(|(addr, len)| doc::payload_of(&self.arena, addr, len));
        self.write_record_carrying(key, existing, spec)?;
        if let Some(payload) = old_payload {
            self.docs.release(payload);
        }
        Ok(())
    }

    /// Writes `spec`, reusing `existing`'s slot when the size class allows,
    /// else alloc-copy-free with an index address swap. **Carries** any
    /// document payload referenced by both old and new value bytes: no
    /// release happens here — TTL rewrites move handle bytes verbatim
    /// (ADR-0037 D3's RENAME/EXPIRE transfer rule; the in-place blob
    /// overwrite in `doc::json_write_value` relies on the same contract).
    ///
    /// The write transition (ADR-0008 A1 rule 2) runs after the write
    /// succeeds and only when the deadline changed: an `Err` leaves the
    /// record, the census and the schedule unchanged.
    pub(crate) fn write_record_carrying(
        &mut self,
        key: &[u8],
        existing: Option<(ArenaAddr, usize)>,
        spec: RecordSpec<'_>,
    ) -> Result<(), OpError> {
        let hash = self.hash_key(key);
        let old = existing
            .and_then(|(addr, len)| RecordView::new(self.arena.bytes(addr, len)).expire_at_ms());
        let new = spec.expire_at_ms;
        let written = self.write_record_at(hash, existing, spec)?;
        if old != new {
            self.deadline_changed(hash, old, new, written);
        }
        Ok(())
    }

    /// The physical write; returns the record's address.
    fn write_record_at(
        &mut self,
        hash: u64,
        existing: Option<(ArenaAddr, usize)>,
        spec: RecordSpec<'_>,
    ) -> Result<ArenaAddr, OpError> {
        let new_len = spec.encoded_len();
        // Writes count as accesses (Redis updates LRU/LFU on write), at
        // write strength: one CLOCK generation / one CMS baseline bump —
        // repeated reads are what saturate recency, so churn cannot
        // impersonate a hot set.
        let written = match existing {
            Some((addr, old_len)) if self.arena.resize_in_place(addr, old_len, new_len) => {
                spec.write(self.arena.bytes_mut(addr, new_len));
                addr
            }
            Some((addr, old_len)) => {
                let new_addr = self.arena.alloc(new_len).ok_or(OpError::OutOfMemory)?;
                spec.write(self.arena.bytes_mut(new_addr, new_len));
                self.index.table.replace(hash, addr, new_addr);
                self.arena.free(addr, old_len);
                new_addr
            }
            None => {
                if self.index.needs_grow() {
                    let hasher = self.cfg.hasher;
                    let arena = &self.arena;
                    self.index.table.grow(|addr, _| hasher.hash(record_at(arena, addr).key()));
                    self.stats.index_grows += 1;
                }
                let new_addr = self.arena.alloc(new_len).ok_or(OpError::OutOfMemory)?;
                spec.write(self.arena.bytes_mut(new_addr, new_len));
                self.index.table.insert(hash, new_addr);
                new_addr
            }
        };
        self.touch_write(hash, written);
        Ok(written)
    }

    /// The write transition. A cleared deadline asks the group without
    /// the rewritten record (it no longer carries a deadline).
    fn deadline_changed(
        &mut self,
        hash: u64,
        old: Option<u64>,
        new: Option<u64>,
        written: ArenaAddr,
    ) {
        let hasher = self.cfg.hasher;
        let arena: &Arena = &self.arena;
        let RecordIndex { table, schedule } = &mut self.index;
        let table: &Index = table;
        let transition = schedule
            .on_write(hash, old, new, || survivors(arena, table, hasher, hash, Some(written)));
        match transition {
            Transition::Placed(Placement::Refused) => self.stats.wheel_fallback += 1,
            Transition::Cleared(Removal::Over) => self.stats.expiry_alias_over += 1,
            Transition::Unchanged
            | Transition::Placed(Placement::Filed | Placement::Held | Placement::Moved)
            | Transition::Cleared(Removal::NoNode | Removal::Kept | Removal::Removed) => {}
        }
    }

    /// Write-strength access mark (see `write_record_at`).
    #[inline]
    fn touch_write(&mut self, hash: u64, addr: ArenaAddr) {
        match self.evict.tracking {
            Tracking::None => {}
            Tracking::Clock => {
                let head = self.arena.bytes_mut(addr, 1);
                head[0] = flags_ref_write(head[0]);
            }
            Tracking::Lfu => {
                let roll = self.evict.next_roll();
                if let Some(cms) = self.evict.cms.as_mut() {
                    cms.touch(hash, roll);
                }
            }
        }
    }
}

/// The removal question (ADR-0008 A1 rule 4): does another record with a
/// deadline share `hash`? ADR-0139 D9's one bounded enumeration, `skip`
/// excluding a rewritten record by address.
fn survivors(
    arena: &Arena,
    index: &Index,
    hasher: KeyHasher,
    hash: u64,
    skip: Option<ArenaAddr>,
) -> Survivors {
    let (walk, _) =
        alias_view(index, hash, |addr| Some(addr) == skip, |addr| alias(arena, hasher, hash, addr));
    match walk {
        AliasWalk::Complete(view)
            if view.members().any(|addr| record_at(arena, addr).expire_at_ms().is_some()) =>
        {
            Survivors::WithDeadline
        }
        AliasWalk::Complete(_) => Survivors::WithoutDeadline,
        AliasWalk::Over(_) => Survivors::Over,
    }
}

/// One fragment match's fetch: an alias when its full keyed hash matches.
fn alias(arena: &Arena, hasher: KeyHasher, hash: u64, addr: ArenaAddr) -> Candidate {
    if hasher.hash(record_at(arena, addr).key()) == hash {
        Candidate::Alias
    } else {
        Candidate::Neighbour
    }
}

/// A fire (rule 5): enumerate the group once and reap **every** expired
/// member. The remaining members all have deadlines ≥ `now` (`is_expired`
/// is `now > deadline`), so the least of them + 1 is later than `now`.
/// `least_deadline_ms` is taken over every member with a deadline, reaped
/// or not — what the schedule's I4 check at fire compares the node with.
fn fire_group(ctx: &mut ReapCtx<'_>, hash: u64, now: Nanos) -> GroupFire {
    let (walk, _) = {
        let arena: &Arena = ctx.arena;
        alias_view(ctx.index, hash, |_| false, |addr| alias(arena, ctx.hasher, hash, addr))
    };
    let AliasWalk::Complete(view) = walk else { return GroupFire::Over };
    let mut expired: [Option<(ArenaAddr, usize)>; IDX_ALIAS_GROUP_MAX] =
        [None; IDX_ALIAS_GROUP_MAX];
    let (mut members, mut reaped) = (0u32, 0u32);
    let (mut next_deadline_ms, mut least_deadline_ms) = (None::<u64>, None::<u64>);
    for addr in view.members() {
        members += 1;
        let record = record_at(ctx.arena, addr);
        let Some(deadline_ms) = record.expire_at_ms() else { continue };
        least_deadline_ms = Some(least_deadline_ms.map_or(deadline_ms, |d| d.min(deadline_ms)));
        // The plant: a fire reaps only its first expired member and
        // re-files at a remaining one's past deadline, which files at or
        // before `now` (the I10 canary).
        let first_only = cfg!(inf_canary_wheel_refile_at_or_before_now) && reaped > 0;
        if record.is_expired(now) && !first_only {
            expired[reaped as usize] = Some((addr, record.encoded_len()));
            reaped += 1;
        } else {
            next_deadline_ms = Some(next_deadline_ms.map_or(deadline_ms, |d| d.min(deadline_ms)));
        }
    }
    for (addr, len) in expired.into_iter().flatten() {
        wheel_reap(ctx, hash, addr, len);
    }
    GroupFire::Complete { members, reaped, next_deadline_ms, least_deadline_ms }
}

/// The wheel's reap: the ADR-0072 D6 death hook and the record's release,
/// without the death transition — the node's fate is the fire's answer.
/// MAINTAIN slices never run inside a command, so no bracket can cover it.
fn wheel_reap(ctx: &mut ReapCtx<'_>, hash: u64, addr: ArenaAddr, len: usize) {
    #[cfg(feature = "doc")]
    if ctx.idx.is_active() {
        debug_assert!(!ctx.idx.bracket_open(), "the wheel never ticks inside a bracket");
        let alias =
            AliasCtx { arena: ctx.arena, index: ctx.index, docs: ctx.docs, hasher: ctx.hasher };
        ctx.idx.remove_doc_entries(&alias, hash, addr, u64::MAX, ctx.max_matches);
    }
    let payload = doc::payload_of(ctx.arena, addr, len);
    ctx.index.remove(hash, addr);
    ctx.arena.free(addr, len);
    ctx.docs.release(payload);
}

/// A group fire as the schedule's answer, tallied for `StoreStats`.
fn fire_answer(group: GroupFire, tally: &mut FireTally) -> Fire {
    match group {
        GroupFire::Over => {
            tally.over += 1;
            Fire { reaped: 0, next: FireNext::ReleaseOwe, least_deadline_ms: None }
        }
        GroupFire::Complete { members, reaped, next_deadline_ms, least_deadline_ms } => {
            tally.reaped += u64::from(reaped);
            tally.stale += u64::from(members == 0);
            let next = match next_deadline_ms {
                Some(deadline_ms) => FireNext::Refile { deadline_ms },
                None => FireNext::Release,
            };
            Fire { reaped, next, least_deadline_ms }
        }
    }
}
