//! Hierarchical timing wheel (M1-S04, master plan §7.4): the mechanism of
//! a store's expiry schedule. Four power-of-two tiers of 512 slots each
//! cover the u40-ms deadline range (1 ms · 512 ms · ~4.4 min · ~37 h
//! windows; anything past the ~2.2-year tier-3 horizon parks in an
//! overflow list that re-files on horizon crossings).
//!
//! ## One node per key hash (ADR-0008 A1)
//!
//! A node is `{key_hash, deadline:40 | next:24}` — 16 bytes, no pointer to
//! the record (the index has no stable slot handles). The node files at
//! `deadline + 1`, the first millisecond the record reads as expired. The
//! membership table ([`member`]) maps each scheduled key hash to its one
//! node, so a deadline change moves or keeps that node instead of arming
//! another (placement is an upsert, rules 1 and 3):
//!
//! - `Held`: the node already fires at or before the new deadline + 1; it
//!   fires early and the store re-files it (rule 5).
//! - `Moved`: the node fires too late; it is removed and a node is filed
//!   at the new deadline + 1.
//! - `Refused`: no node within the budget; the store's sweep owns that
//!   record's expiry (rule 6). Never lazy-only.
//!
//! Removal is O(1) without a predecessor link (rule 4): the successor's
//! contents move into the removed node's position and its membership entry
//! follows. A removed node with no successor stays linked as a
//! **tombstone** — a node its hash's entry does not point at — and is freed
//! when its slot drains, without a callback. A list's tombstone stays at
//! its tail, so there are at most `WHEEL_TOMBSTONES_MAX`. Because a
//! successor copy never learns its list's tier, occupancy is kept per slot
//! (a bitmap per tier) by the only code that empties a list — the drain and
//! the cascade.
//!
//! A fire hands the key hash to the store, which answers with the node's
//! next state (re-file, release, or release and owe the sweep). A re-file
//! files at `max(deadline + 1, now + 1)` — strictly after the cursor, so
//! never into the tier-0 slot being drained (I10). Nothing removes a node
//! while `tick` runs: it holds `&mut self`, and its callback cannot reach
//! the wheel (I3).
//!
//! Time is injected (`now` milliseconds on the cell clock — L7); ticking is
//! deterministic and DST-able. The wheel never touches the index or arena.

mod member;

use member::Membership;

use crate::limits::WHEEL_TOMBSTONES_MAX;

/// Slots per tier (power of two).
const SLOTS: usize = 512;
const SLOT_BITS: u32 = 9;
const TIERS: usize = 4;
/// Occupancy bitmap words per tier.
const BITMAP_WORDS: usize = SLOTS / 64;
/// Tier t covers instants within `1 << (SLOT_BITS * (t + 1))` ms of the
/// cursor; tier 3's horizon is 2^36 ms ≈ 2.18 years.
const HORIZON_MS: u64 = 1 << (SLOT_BITS * TIERS as u32);

/// Null link / list terminator (u24 space).
const NIL: u32 = (1 << 24) - 1;

const DEADLINE_BITS: u32 = 40;
const DEADLINE_MASK: u64 = (1 << DEADLINE_BITS) - 1;

const _: () = assert!(crate::record::MAX_EXPIRE_MS == DEADLINE_MASK, "a node holds any deadline");

/// One 16-byte wheel node: key hash + packed `{deadline:40, next:24}`. The
/// deadline is the record's (the node files at `deadline + 1`).
#[derive(Copy, Clone, Debug)]
struct Node {
    hash: u64,
    packed: u64,
}

impl Node {
    #[inline]
    fn new(hash: u64, deadline_ms: u64, next: u32) -> Node {
        debug_assert!(deadline_ms <= DEADLINE_MASK);
        debug_assert!(next <= NIL);
        Node { hash, packed: deadline_ms | (u64::from(next) << DEADLINE_BITS) }
    }

    #[inline]
    fn deadline_ms(self) -> u64 {
        self.packed & DEADLINE_MASK
    }

    #[inline]
    fn next(self) -> u32 {
        (self.packed >> DEADLINE_BITS) as u32
    }

    #[inline]
    fn set_next(&mut self, next: u32) {
        debug_assert!(next <= NIL);
        self.packed = (self.packed & DEADLINE_MASK) | (u64::from(next) << DEADLINE_BITS);
    }

    #[inline]
    fn set_deadline(&mut self, deadline_ms: u64) {
        debug_assert!(deadline_ms <= DEADLINE_MASK);
        self.packed = (self.packed & !DEADLINE_MASK) | deadline_ms;
    }
}

/// How one placement settled (ADR-0008 A1 rule 3).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Placement {
    /// No node for the hash; one was allocated and filed.
    Filed,
    /// A node fires at or before the deadline + 1; nothing moved.
    Held,
    /// A node fired too late; it was removed and a node filed in time.
    Moved,
    /// No node within the budget, or a refused growth: the record is
    /// swept. After a `Moved` removal the hash keeps no node.
    Refused,
}

/// A due node handed to the fire: its key hash and the deadline it holds
/// (the record deadline it was filed for, at `deadline + 1`).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct Due {
    pub hash: u64,
    pub deadline_ms: u64,
}

/// A fired node's next state, as the store's fire decides it (rule 5).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum FireNext {
    /// Members with a deadline remain: re-file at the least one + 1.
    Refile { deadline_ms: u64 },
    /// No member with a deadline remains.
    Release,
    /// The group could not be enumerated (ADR-0139 D9 `Over`): release,
    /// and the sweep owes the members a visit.
    ReleaseOwe,
}

/// Budget for one expiry MAINTAIN slice (M1-S05). Both axes bound the slice:
/// `max_fires` caps reap callbacks (foreground-visible work), `max_steps`
/// caps cursor advancement (bounded even when the wheel is empty but far
/// behind).
#[derive(Copy, Clone, Debug)]
pub struct ExpiryBudget {
    pub max_fires: u32,
    pub max_steps: u32,
    /// Index slots the expiry sweep may walk this slice, shared by every
    /// store the slice serves (ADR-0008 A1 rule 6).
    pub max_sweep_slots: u32,
}

impl ExpiryBudget {
    /// No bound on any axis — drains and oracles only (a slice still
    /// stops at a sweep pass boundary, ADR-0008 A1 rule 6).
    pub const UNBOUNDED: ExpiryBudget =
        ExpiryBudget { max_fires: u32::MAX, max_steps: u32::MAX, max_sweep_slots: u32::MAX };
}

impl Default for ExpiryBudget {
    fn default() -> ExpiryBudget {
        ExpiryBudget {
            max_fires: 64,
            max_steps: 4096,
            max_sweep_slots: crate::limits::EXPIRY_SWEEP_SLOTS_PER_SLICE,
        }
    }
}

/// The node budget one store's wheel honours (ADR-0008 A1 rule 7): at
/// most [`WHEEL_NODES_MAX`](crate::limits::WHEEL_NODES_MAX), checked once
/// here. Lower values serve tests and the simulator's small-cap runs;
/// there is no operator key.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct WheelNodesMax(u32);

/// A node budget above [`WHEEL_NODES_MAX`](crate::limits::WHEEL_NODES_MAX).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct WheelNodesMaxError {
    pub nodes: usize,
}

impl WheelNodesMax {
    /// The width bound itself — the default.
    pub const MAX: WheelNodesMax = WheelNodesMax(crate::limits::WHEEL_NODES_MAX as u32);

    /// # Errors
    /// `nodes` above the u24 link width.
    pub fn new(nodes: usize) -> Result<WheelNodesMax, WheelNodesMaxError> {
        if nodes > crate::limits::WHEEL_NODES_MAX {
            return Err(WheelNodesMaxError { nodes });
        }
        Ok(WheelNodesMax(nodes as u32))
    }

    /// The budget in nodes.
    #[must_use]
    pub fn get(self) -> usize {
        self.0 as usize
    }
}

impl Default for WheelNodesMax {
    fn default() -> WheelNodesMax {
        WheelNodesMax::MAX
    }
}

const _: () = assert!(crate::limits::WHEEL_NODES_MAX < NIL as usize);

/// What one [`TtlWheel::tick`] did (feeds `expiry_debt` + tripwires).
#[derive(Copy, Clone, Default, Debug)]
pub(crate) struct TickStats {
    /// Nodes handed to the fire callback.
    pub fired: u32,
    /// Cursor milliseconds advanced.
    pub steps: u32,
    /// True when the cursor caught up to `now` (no overdue slots remain).
    pub caught_up: bool,
    /// Fires answered with a re-file.
    pub refiled: u32,
    /// Fires answered with `ReleaseOwe`.
    pub owed: u32,
}

/// Whether a tier-0 drain finished its slot or the fire budget cut it.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Drain {
    Done,
    Cut,
}

/// The per-store hierarchical wheel. See module docs.
pub(crate) struct TtlWheel {
    pool: Vec<Node>,
    free: u32,
    /// Allocated nodes: live (in membership) plus tombstones.
    nodes_in_use: u64,
    nodes_max: usize,
    tombstones: u64,
    /// `heads[t][s]` — singly-linked LIFO stack of node indices.
    heads: [[u32; SLOTS]; TIERS],
    /// Bit `s` of tier `t` is set exactly when `heads[t][s] != NIL`.
    occupied: [[u64; BITMAP_WORDS]; TIERS],
    /// Nodes filing past the tier-3 horizon.
    overflow: u32,
    /// The wheel has processed every slot strictly below this millisecond.
    cursor_ms: u64,
    members: Membership,
}

impl TtlWheel {
    pub fn new(start_ms: u64, nodes_max: WheelNodesMax) -> TtlWheel {
        TtlWheel {
            pool: Vec::new(),
            free: NIL,
            nodes_in_use: 0,
            nodes_max: nodes_max.get(),
            tombstones: 0,
            heads: [[NIL; SLOTS]; TIERS],
            occupied: [[0; BITMAP_WORDS]; TIERS],
            overflow: NIL,
            cursor_ms: start_ms,
            members: Membership::new(),
        }
    }

    /// Live nodes: one per scheduled key hash, tombstones excluded.
    #[inline]
    pub fn armed(&self) -> u64 {
        self.nodes_in_use - self.tombstones
    }

    /// Tombstone nodes still linked (rule 4).
    #[inline]
    pub fn tombstones(&self) -> u64 {
        self.tombstones
    }

    /// Every slot strictly below this millisecond has been processed (the
    /// `expiry_debt` lag metric reads `now - cursor`).
    #[inline]
    pub fn cursor_ms(&self) -> u64 {
        self.cursor_ms
    }

    /// Resident bytes (the `wheel_bytes` attribution): pool capacity at
    /// 16 B per node, membership capacity, and the fixed tables.
    pub fn resident_bytes(&self) -> u64 {
        (self.pool.capacity() * size_of::<Node>()) as u64 + self.table_bytes()
    }

    /// Bytes in use (the pressure comparable, `wheel_live_bytes`): nodes
    /// in use at 16 B, membership capacity, and the fixed tables.
    pub fn live_bytes(&self) -> u64 {
        self.nodes_in_use * size_of::<Node>() as u64 + self.table_bytes()
    }

    fn table_bytes(&self) -> u64 {
        let fixed = size_of::<[[u32; SLOTS]; TIERS]>() + size_of::<[[u64; BITMAP_WORDS]; TIERS]>();
        fixed as u64 + self.members.fixed_bytes() + self.members.slot_bytes()
    }

    /// The deadline of the node scheduling `hash`, if any.
    #[inline]
    pub fn node_deadline(&self, hash: u64) -> Option<u64> {
        let at = self.members.lookup(hash, &self.pool)?;
        Some(self.pool[at as usize].deadline_ms())
    }

    /// Schedules `hash` no later than `deadline_ms + 1` (rule 3's upsert).
    pub fn place(&mut self, hash: u64, deadline_ms: u64) -> Placement {
        let deadline_ms = deadline_ms.min(DEADLINE_MASK);
        let Some(at) = self.members.lookup(hash, &self.pool) else {
            return if self.file_new(hash, deadline_ms) {
                Placement::Filed
            } else {
                Placement::Refused
            };
        };
        let filed = self.pool[at as usize].deadline_ms();
        if filed == deadline_ms || (filed < deadline_ms && !cfg!(inf_canary_wheel_arm_per_change)) {
            return Placement::Held;
        }
        if cfg!(inf_canary_wheel_arm_per_change) {
            // The plant: every changed deadline allocates a node, and the
            // old one lingers unpointed until its slot drains (I1's canary).
            let Some(fresh) = self.alloc(hash, deadline_ms) else { return Placement::Refused };
            self.members.retarget(hash, at, fresh, &self.pool);
            self.file(fresh, self.cursor_ms);
            return Placement::Moved;
        }
        self.remove(hash);
        if self.file_new(hash, deadline_ms) { Placement::Moved } else { Placement::Refused }
    }

    /// Unschedules `hash` in O(1) (rule 4): the successor's contents move
    /// into the node's position and its membership entry follows; a node
    /// with no successor stays linked as a tombstone. False when `hash`
    /// has no node.
    pub fn remove(&mut self, hash: u64) -> bool {
        let Some(at) = self.members.lookup(hash, &self.pool) else { return false };
        self.members.remove(hash, at, &self.pool);
        let successor = self.pool[at as usize].next();
        if successor == NIL {
            self.tombstones += 1;
            debug_assert!(self.tombstones <= WHEEL_TOMBSTONES_MAX, "tombstone bound (I6)");
            return true;
        }
        let moved = self.pool[successor as usize];
        self.pool[at as usize] = moved;
        if self.members.lookup(moved.hash, &self.pool) == Some(successor) {
            self.members.retarget(moved.hash, successor, at, &self.pool);
        }
        self.release(successor);
        true
    }

    /// Advances toward `now_ms`, firing due nodes through `fire(due)`
    /// under `budget`; the callback's answer settles the node (rule 5).
    pub fn tick(
        &mut self,
        now_ms: u64,
        budget: ExpiryBudget,
        mut fire: impl FnMut(Due) -> FireNext,
    ) -> TickStats {
        let mut stats = TickStats::default();
        while self.cursor_ms <= now_ms {
            if stats.fired >= budget.max_fires || stats.steps >= budget.max_steps {
                return stats; // budget exhausted — debt stays visible
            }
            if self.drain_slot(now_ms, budget.max_fires, &mut fire, &mut stats) == Drain::Cut {
                return stats;
            }
            // This millisecond is done; cross to the next, cascading any
            // higher-tier slot that window-opens at the new cursor.
            self.cursor_ms += 1;
            stats.steps += 1;
            self.cascade_boundaries();
            self.fast_forward(now_ms, &mut stats);
        }
        stats.caught_up = true;
        stats
    }

    // ---- internals ----

    /// Drains the tier-0 slot for the cursor millisecond. The fire budget
    /// cuts mid-slot (a 1M-same-ms storm must not ride one slot past the
    /// slice — M1-S05); the unprocessed chain splices back and the cursor
    /// stays put. A node its hash's entry does not point at is a tombstone
    /// and is freed without a callback (rule 4).
    fn drain_slot(
        &mut self,
        now_ms: u64,
        max_fires: u32,
        fire: &mut impl FnMut(Due) -> FireNext,
        stats: &mut TickStats,
    ) -> Drain {
        let slot = (self.cursor_ms & (SLOTS as u64 - 1)) as usize;
        let mut at = self.take_list(0, slot);
        let mut keep = NIL;
        while at != NIL {
            if stats.fired >= max_fires {
                let mut rest = at;
                while keep != NIL {
                    let next = self.pool[keep as usize].next();
                    self.pool[keep as usize].set_next(rest);
                    rest = keep;
                    keep = next;
                }
                self.set_list(0, slot, rest);
                return Drain::Cut;
            }
            let node = self.pool[at as usize];
            let next = node.next();
            if node.deadline_ms() >= now_ms {
                // Not yet due (its instant is past `now`): keep it for
                // the window that owns it.
                self.pool[at as usize].set_next(keep);
                keep = at;
            } else if self.members.lookup(node.hash, &self.pool) != Some(at) {
                self.free_tombstone(at);
            } else {
                stats.fired += 1;
                let answer = fire(Due { hash: node.hash, deadline_ms: node.deadline_ms() });
                self.settle(at, answer, now_ms, stats);
            }
            at = next;
        }
        self.set_list(0, slot, keep);
        Drain::Done
    }

    /// Applies a fire's answer to its node.
    fn settle(&mut self, at: u32, answer: FireNext, now_ms: u64, stats: &mut TickStats) {
        match answer {
            FireNext::Release => self.release_member(at),
            FireNext::ReleaseOwe => {
                self.release_member(at);
                stats.owed += 1;
            }
            FireNext::Refile { deadline_ms } => {
                stats.refiled += 1;
                let unclamped = cfg!(inf_canary_wheel_refile_at_or_before_now);
                debug_assert!(unclamped || deadline_ms >= now_ms, "a re-file is later than now");
                self.pool[at as usize].set_deadline(deadline_ms.min(DEADLINE_MASK));
                // I10: strictly after the cursor, so never into the slot
                // being drained — whatever the verdict.
                let floor = if unclamped { 0 } else { now_ms + 1 };
                self.file(at, floor);
            }
        }
    }

    /// On each tier-t window boundary, re-file that tier's newly-current
    /// slot into lower tiers (instants are absolute; `file` re-derives the
    /// right tier from the new cursor).
    fn cascade_boundaries(&mut self) {
        for tier in 1..TIERS {
            let bits = SLOT_BITS * tier as u32;
            if self.cursor_ms & ((1 << bits) - 1) != 0 {
                break; // not a boundary of this tier (nor any higher one)
            }
            let slot = ((self.cursor_ms >> bits) & (SLOTS as u64 - 1)) as usize;
            let mut at = self.take_list(tier, slot);
            while at != NIL {
                let next = self.pool[at as usize].next();
                self.file(at, self.cursor_ms);
                at = next;
            }
        }
        // Tier-3 horizon crossing: pull overflow nodes into range (`file`
        // puts one still past the horizon back on the overflow list).
        if self.cursor_ms & (HORIZON_MS / SLOTS as u64 - 1) == 0 && self.overflow != NIL {
            let mut at = core::mem::replace(&mut self.overflow, NIL);
            while at != NIL {
                let next = self.pool[at as usize].next();
                self.file(at, self.cursor_ms);
                at = next;
            }
        }
    }

    /// Skips empty stretches in O(1) per cascade boundary instead of O(ms):
    /// while tier 0 is empty, jump the cursor straight to the next boundary
    /// of the lowest non-empty tier (the only place new tier-0 work can
    /// appear), or to `now` when nothing is filed anywhere. Each jump
    /// charges one budget step — it is constant work.
    fn fast_forward(&mut self, now_ms: u64, stats: &mut TickStats) {
        loop {
            if self.cursor_ms > now_ms || self.tier_occupied(0) {
                return;
            }
            let align = if self.tier_occupied(1) {
                SLOT_BITS
            } else if self.tier_occupied(2) {
                SLOT_BITS * 2
            } else if self.tier_occupied(3) || self.overflow != NIL {
                SLOT_BITS * 3
            } else {
                // Nothing filed anywhere: snap to now.
                if now_ms > self.cursor_ms {
                    self.cursor_ms = now_ms;
                    stats.steps = stats.steps.saturating_add(1);
                }
                return;
            };
            let boundary = ((self.cursor_ms >> align) + 1) << align;
            let target = boundary.min(now_ms);
            if target <= self.cursor_ms {
                return;
            }
            self.cursor_ms = target;
            stats.steps = stats.steps.saturating_add(1);
            // Skipped boundaries belong to empty tiers only; the landing
            // boundary is the one that can cascade real work.
            self.cascade_boundaries();
        }
    }

    /// Allocates, enters and files a node for `hash`; false when the node
    /// budget or a growth refused (the placement is `Refused`).
    fn file_new(&mut self, hash: u64, deadline_ms: u64) -> bool {
        let Some(at) = self.alloc(hash, deadline_ms) else { return false };
        if self.members.insert(hash, at, &self.pool).is_err() {
            self.release(at);
            return false;
        }
        self.file(at, self.cursor_ms);
        true
    }

    /// A node from the free list or a new pool slot, within `nodes_max`.
    /// Pool growth is fallible: a refused `try_reserve` is a refusal.
    fn alloc(&mut self, hash: u64, deadline_ms: u64) -> Option<u32> {
        if self.free != NIL {
            let at = self.free;
            self.free = self.pool[at as usize].next();
            self.pool[at as usize] = Node::new(hash, deadline_ms, NIL);
            self.nodes_in_use += 1;
            return Some(at);
        }
        if self.pool.len() >= self.nodes_max || self.pool.try_reserve(1).is_err() {
            return None;
        }
        // Exact: `len < nodes_max ≤ WHEEL_NODES_MAX < NIL` (const-asserted).
        let at = self.pool.len() as u32;
        self.pool.push(Node::new(hash, deadline_ms, NIL));
        self.nodes_in_use += 1;
        Some(at)
    }

    /// Returns an unlinked node to the free list.
    fn release(&mut self, at: u32) {
        self.pool[at as usize].set_next(self.free);
        self.free = at;
        self.nodes_in_use -= 1;
    }

    /// Releases a fired node together with its membership entry.
    fn release_member(&mut self, at: u32) {
        let hash = self.pool[at as usize].hash;
        self.members.remove(hash, at, &self.pool);
        self.release(at);
    }

    fn free_tombstone(&mut self, at: u32) {
        debug_assert!(
            self.tombstones > 0 || cfg!(inf_canary_wheel_arm_per_change),
            "an unpointed node is a counted tombstone"
        );
        self.tombstones = self.tombstones.saturating_sub(1);
        self.release(at);
    }

    /// Files node `at` at `max(deadline + 1, floor_ms)`: `floor_ms` is the
    /// cursor for a placement or a cascade, `now + 1` for a re-file.
    fn file(&mut self, at: u32, floor_ms: u64) {
        let instant = (self.pool[at as usize].deadline_ms() + 1).max(floor_ms);
        let delta = instant.saturating_sub(self.cursor_ms);
        if delta >= HORIZON_MS {
            self.pool[at as usize].set_next(self.overflow);
            self.overflow = at;
            return;
        }
        // Smallest tier whose window still contains the instant.
        let mut tier = 0;
        while tier < TIERS - 1 && delta >= (1 << (SLOT_BITS * (tier as u32 + 1))) {
            tier += 1;
        }
        let slot = ((instant >> (SLOT_BITS * tier as u32)) & (SLOTS as u64 - 1)) as usize;
        self.pool[at as usize].set_next(self.heads[tier][slot]);
        self.set_list(tier, slot, at);
    }

    fn take_list(&mut self, tier: usize, slot: usize) -> u32 {
        self.occupied[tier][slot / 64] &= !(1 << (slot % 64));
        core::mem::replace(&mut self.heads[tier][slot], NIL)
    }

    fn set_list(&mut self, tier: usize, slot: usize, head: u32) {
        self.heads[tier][slot] = head;
        let bit = 1 << (slot % 64);
        if head == NIL {
            self.occupied[tier][slot / 64] &= !bit;
        } else {
            self.occupied[tier][slot / 64] |= bit;
        }
    }

    #[inline]
    fn tier_occupied(&self, tier: usize) -> bool {
        self.occupied[tier].iter().any(|word| *word != 0)
    }

    /// Membership entries whose node no slot list reaches, and the linked
    /// nodes no entry points at (test-support audits: O1's orphan check).
    #[cfg(any(test, feature = "test-support"))]
    pub fn audit_links(&self) -> (u64, u64) {
        let mut linked = vec![false; self.pool.len()];
        let lists = self.heads.iter().flatten().copied().chain(core::iter::once(self.overflow));
        for head in lists {
            let mut at = head;
            while at != NIL {
                linked[at as usize] = true;
                at = self.pool[at as usize].next();
            }
        }
        let mut orphans = 0;
        for (_, node) in self.members.entries(&self.pool) {
            orphans += u64::from(!linked[node as usize]);
            linked[node as usize] = false;
        }
        let unpointed = linked.iter().filter(|l| **l).count() as u64;
        (orphans, unpointed)
    }
}

impl core::fmt::Debug for TtlWheel {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TtlWheel")
            .field("armed", &self.armed())
            .field("tombstones", &self.tombstones)
            .field("cursor_ms", &self.cursor_ms)
            .field("pool_len", &self.pool.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    const DRAIN: ExpiryBudget = ExpiryBudget::UNBOUNDED;

    /// Ticks to `now`, releasing every fired node; returns the fired hashes.
    fn drain_all(wheel: &mut TtlWheel, now: u64) -> Vec<u64> {
        let mut fired = Vec::new();
        let stats = wheel.tick(now, DRAIN, |due| {
            fired.push(due.hash);
            FireNext::Release
        });
        assert!(stats.caught_up);
        fired
    }

    fn wheel() -> TtlWheel {
        TtlWheel::new(0, WheelNodesMax::MAX)
    }

    #[test]
    fn node_is_sixteen_bytes() {
        assert_eq!(size_of::<Node>(), 16);
    }

    #[test]
    fn fires_at_the_first_expired_millisecond_never_early() {
        let mut wheel = wheel();
        assert_eq!(wheel.place(0xAA, 99), Placement::Filed);
        assert!(drain_all(&mut wheel, 99).is_empty(), "fired at the deadline itself");
        assert_eq!(drain_all(&mut wheel, 100), vec![0xAA]);
        assert!(drain_all(&mut wheel, 10_000).is_empty(), "double fire");
        assert_eq!(wheel.armed(), 0);
    }

    #[test]
    fn every_tier_and_overflow_deliver() {
        let mut wheel = wheel();
        // One deadline per tier window + one past the horizon.
        let deadlines = [3u64, 700, 300_000, 200_000_000, HORIZON_MS + 5_000];
        for (i, d) in deadlines.iter().enumerate() {
            assert_eq!(wheel.place(i as u64, *d), Placement::Filed);
        }
        let mut fired = drain_all(&mut wheel, HORIZON_MS + 10_000);
        fired.sort_unstable();
        assert_eq!(fired, (0..deadlines.len() as u64).collect::<Vec<_>>());
        assert_eq!(wheel.armed(), 0);
    }

    #[test]
    fn budget_cuts_leave_debt_and_resume() {
        let mut wheel = wheel();
        for i in 0..1000u64 {
            wheel.place(i, 49); // 1000 same-millisecond deadlines (storm shape)
        }
        let mut fired = 0u64;
        let slice = ExpiryBudget { max_fires: 64, max_steps: 4096, max_sweep_slots: 0 };
        let mut release = |_| {
            fired += 1;
            FireNext::Release
        };
        let stats = wheel.tick(60, slice, &mut release);
        assert!(!stats.caught_up, "must cut on fire budget");
        assert_eq!(wheel.armed(), 1000 - 64);
        while !wheel.tick(60, slice, &mut release).caught_up {}
        assert_eq!(fired, 1000);
        assert_eq!(wheel.armed(), 0);
    }

    #[test]
    fn random_deadlines_fire_exactly_once_in_window_oracle() {
        let mut wheel = wheel();
        let mut oracle: BTreeMap<u64, u64> = BTreeMap::new(); // hash → deadline
        let mut x: u64 = 0x9E37_79B9;
        let mut rand = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let n = if cfg!(miri) { 500 } else { 20_000 };
        for i in 0..n {
            // Bias toward small deltas, with a heavy tail across tiers.
            let deadline = match rand() % 4 {
                0 => rand() % 512,
                1 => rand() % 100_000,
                2 => rand() % 10_000_000,
                _ => rand() % (HORIZON_MS * 2),
            };
            wheel.place(i, deadline);
            oracle.insert(i, deadline);
        }
        // Advance in random jumps; every fire must be unique and never
        // before its deadline's first expired millisecond.
        let mut now = 0u64;
        let mut fired: BTreeMap<u64, u64> = BTreeMap::new();
        while now < HORIZON_MS * 2 + 1024 {
            now += 1 + rand() % 50_000_000;
            for h in drain_all(&mut wheel, now) {
                assert!(oracle[&h] < now, "fired before deadline + 1");
                assert!(fired.insert(h, now).is_none(), "double fire for {h}");
            }
        }
        assert_eq!(fired.len(), oracle.len(), "missed fires");
        assert_eq!(wheel.armed(), 0);
    }

    #[test]
    fn pool_reuse_keeps_capacity_bounded() {
        let mut wheel = wheel();
        let mut now = 0;
        for round in 0..50u64 {
            for i in 0..100 {
                wheel.place(i, now + 10 + i);
            }
            now += 2_000;
            assert_eq!(drain_all(&mut wheel, now).len(), 100, "round {round}");
        }
        assert!(wheel.pool.capacity() <= 256, "pool ballooned: {}", wheel.pool.capacity());
    }

    #[test]
    fn placement_is_an_upsert_on_the_key_hash() {
        let mut wheel = wheel();
        assert_eq!(wheel.place(7, 1_000), Placement::Filed);
        assert_eq!(wheel.place(7, 5_000), Placement::Held, "a later deadline keeps the node");
        assert_eq!(wheel.place(7, 1_000), Placement::Held);
        assert_eq!(wheel.place(7, 500), Placement::Moved, "an earlier one moves it");
        assert_eq!(wheel.armed(), 1);
        assert_eq!(wheel.node_deadline(7), Some(500));
    }

    #[test]
    fn removal_copies_the_successor_or_leaves_a_tombstone() {
        let mut wheel = wheel();
        wheel.place(1, 100);
        wheel.place(2, 100); // same slot: the list is 2 → 1
        assert!(wheel.remove(2), "head: the successor's contents move up");
        assert_eq!((wheel.armed(), wheel.tombstones()), (1, 0));
        assert_eq!(wheel.node_deadline(1), Some(100), "1's entry followed its contents");
        assert!(wheel.remove(1), "tail: no successor, a tombstone stays linked");
        assert_eq!((wheel.armed(), wheel.tombstones()), (0, 1));
        assert!(!wheel.remove(1), "a hash without a node");
        assert!(drain_all(&mut wheel, 200).is_empty(), "a tombstone fires no callback");
        assert_eq!(wheel.tombstones(), 0, "freed when its slot drained");
        assert_eq!(wheel.audit_links(), (0, 0));
    }

    #[test]
    fn a_refile_lands_after_now_and_fires_again() {
        let mut wheel = wheel();
        wheel.place(9, 10);
        let mut calls = 0;
        wheel.tick(50, DRAIN, |_| {
            calls += 1;
            FireNext::Refile { deadline_ms: 80 }
        });
        assert_eq!(calls, 1, "a re-file never lands in the slot being drained");
        assert_eq!(wheel.node_deadline(9), Some(80));
        assert_eq!(drain_all(&mut wheel, 81), vec![9]);
    }

    /// The integer maximum: a deadline of `MAX_EXPIRE_MS` files on the
    /// overflow list, which has no occupancy bitmap. A `Moved` out of it
    /// copies its successor or leaves an overflow tombstone, and the drain
    /// past the largest instant frees every node and tombstone.
    #[test]
    fn deadlines_at_the_integer_maximum_move_and_leave_the_overflow_list() {
        let mut wheel = wheel();
        assert_eq!(wheel.place(1, DEADLINE_MASK), Placement::Filed);
        assert_eq!(wheel.place(2, DEADLINE_MASK), Placement::Filed); // overflow: 2 → 1
        assert_ne!(wheel.overflow, NIL);
        assert_eq!(wheel.place(2, 100), Placement::Moved, "head: successor copy");
        assert_eq!((wheel.armed(), wheel.tombstones()), (2, 0));
        assert_eq!(wheel.node_deadline(1), Some(DEADLINE_MASK), "1 followed its contents");
        assert_eq!(wheel.place(1, 200), Placement::Moved, "tail: an overflow tombstone");
        assert_eq!((wheel.armed(), wheel.tombstones()), (2, 1));
        assert_eq!(wheel.place(1, DEADLINE_MASK), Placement::Held, "a later deadline holds");
        assert!(wheel.remove(2), "a tier-0 node removed");
        assert_eq!(wheel.place(3, DEADLINE_MASK), Placement::Filed);
        assert!(wheel.remove(3), "an overflow node removed");
        assert_eq!(wheel.audit_links(), (0, wheel.tombstones()));
        let mut fired = drain_all(&mut wheel, DEADLINE_MASK + 1);
        fired.sort_unstable();
        assert_eq!(fired, vec![1], "only the live node fires");
        assert_eq!((wheel.armed(), wheel.tombstones()), (0, 0));
        assert_eq!(wheel.audit_links(), (0, 0));
        assert_eq!(wheel.overflow, NIL);
    }

    /// A deadline already behind an advanced cursor files at the cursor
    /// (`max(deadline + 1, cursor)`): it fires on the next tick, not a
    /// wheel revolution later and never not at all.
    #[test]
    fn a_past_deadline_behind_the_cursor_fires_on_the_next_tick() {
        let mut wheel = wheel();
        assert!(drain_all(&mut wheel, 5_000).is_empty());
        assert_eq!(wheel.cursor_ms(), 5_001);
        for (hash, deadline) in [(1, 10), (2, 4_999), (3, 5_000), (4, 5_000 - 512)] {
            assert_eq!(wheel.place(hash, deadline), Placement::Filed);
        }
        assert!(drain_all(&mut wheel, 5_000).is_empty(), "the cursor already passed 5 000");
        let mut fired = drain_all(&mut wheel, 5_001);
        fired.sort_unstable();
        assert_eq!(fired, vec![1, 2, 3, 4], "every past deadline fires at the cursor");
        assert_eq!(wheel.armed(), 0);
    }

    #[test]
    fn the_node_budget_refuses_past_its_cap() {
        let mut wheel = TtlWheel::new(0, WheelNodesMax::new(2).expect("small"));
        assert_eq!(wheel.place(1, 10), Placement::Filed);
        assert_eq!(wheel.place(2, 10), Placement::Filed);
        assert_eq!(wheel.place(3, 10), Placement::Refused);
        assert_eq!(wheel.armed(), 2);
        assert!(WheelNodesMax::new(crate::limits::WHEEL_NODES_MAX + 1).is_err());
    }
}
