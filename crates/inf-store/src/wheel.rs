//! Hierarchical timing wheel **v1** (M1-S04, master plan §7.4): the per-cell
//! active-expiry engine. Four power-of-two tiers of 512 slots each cover the
//! full u40-ms deadline range (1 ms · 512 ms · ~4.4 min · ~37 h windows;
//! anything past the ~2.2-year tier-3 horizon parks in an overflow list that
//! re-files on horizon crossings).
//!
//! ## Stale-tolerant entries (recorded design decision, M1-S04)
//!
//! The milestone task sketched wheel slots holding `{slot_handle,
//! generation}`. The M0 index deliberately has no stable slot handles
//! (records move across size classes; the table rehashes), so v1 stores
//! `{key_hash, deadline_ms}` — exactly 16 bytes per entry — and validates at
//! fire time instead: probe the index by hash and reap **only** records that
//! are actually expired at `now`. A record whose TTL was changed, persisted,
//! deleted, or overwritten simply makes the old entry *stale*; firing a
//! stale entry is a no-op (counted, `wheel_stale`). Reaping any genuinely
//! expired record is always correct, so hash collisions cannot misfire.
//! Same intent as the sketch — no key copies, no dangling pointers — with
//! zero changes to the frozen index seam.
//!
//! ## Bounds (L5, bounded-everything)
//!
//! Node pool: `{hash: u64, deadline:40 | next:24}` = 16 B exactly; the u24
//! next-link caps the pool at 2^24 − 1 entries per cell (~16.7M TTL'd keys
//! per cell, ~268 MB at the cap). On pool exhaustion `arm` reports
//! [`ArmOutcome::PoolFull`] and the key falls back to lazy expire-on-read —
//! a counted tripwire, never an error. Fixed overhead: 4 × 512 slot heads +
//! lengths = 16 KiB per cell.
//!
//! Time is injected (`now` milliseconds on the cell clock — L7); ticking is
//! deterministic and DST-able. The wheel never touches the index or arena —
//! the store owns the fire-time validation (`CellStore::expire_tick`).

/// Slots per tier (power of two).
const SLOTS: usize = 512;
const SLOT_BITS: u32 = 9;
const TIERS: usize = 4;
/// Tier t covers deadlines within `1 << (SLOT_BITS * (t + 1))` ms of the
/// cursor; tier 3's horizon is 2^36 ms ≈ 2.18 years.
const HORIZON_MS: u64 = 1 << (SLOT_BITS * TIERS as u32);

/// Null link / list terminator (u24 space).
const NIL: u32 = (1 << 24) - 1;
/// Maximum pool size: u24 links, NIL reserved.
const POOL_CAP: usize = NIL as usize;

const DEADLINE_BITS: u32 = 40;
const DEADLINE_MASK: u64 = (1 << DEADLINE_BITS) - 1;

/// One 16-byte wheel entry: key hash + packed `{deadline:40, next:24}`.
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
}

/// Result of [`TtlWheel::arm`].
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum ArmOutcome {
    Armed,
    /// Node pool exhausted — the key stays lazy-expired only (tripwire).
    PoolFull,
}

/// Budget for one expiry MAINTAIN slice (M1-S05). Both axes bound the slice:
/// `max_fires` caps reap callbacks (foreground-visible work), `max_steps`
/// caps cursor advancement (bounded even when the wheel is empty but far
/// behind).
#[derive(Copy, Clone, Debug)]
pub struct ExpiryBudget {
    pub max_fires: u32,
    pub max_steps: u32,
}

impl Default for ExpiryBudget {
    fn default() -> ExpiryBudget {
        ExpiryBudget { max_fires: 64, max_steps: 4096 }
    }
}

/// What one [`TtlWheel::tick`] did (feeds `expiry_debt` + tripwires).
#[derive(Copy, Clone, Default, Debug)]
pub struct TickStats {
    /// Entries handed to the fire callback.
    pub fired: u32,
    /// Cursor milliseconds advanced.
    pub steps: u32,
    /// True when the cursor caught up to `now` (no overdue slots remain).
    pub caught_up: bool,
}

/// The per-cell hierarchical wheel. See module docs.
pub(crate) struct TtlWheel {
    pool: Vec<Node>,
    free: u32,
    /// `heads[t][s]` — singly-linked LIFO stack of node indices.
    heads: [[u32; SLOTS]; TIERS],
    /// Live entries per tier (O(1) empty-tier fast-forward).
    tier_live: [u64; TIERS],
    /// Entries with deadlines past the tier-3 horizon.
    overflow: u32,
    overflow_live: u64,
    /// The wheel has processed every slot strictly below this millisecond.
    cursor_ms: u64,
    /// Total live entries (pool occupancy).
    live: u64,
}

impl TtlWheel {
    pub fn new(start_ms: u64) -> TtlWheel {
        TtlWheel {
            pool: Vec::new(),
            free: NIL,
            heads: [[NIL; SLOTS]; TIERS],
            tier_live: [0; TIERS],
            overflow: NIL,
            overflow_live: 0,
            cursor_ms: start_ms,
            live: 0,
        }
    }

    /// Live entries (≥ live TTL'd keys; stale entries inflate it until they
    /// fire).
    #[inline]
    pub fn live(&self) -> u64 {
        self.live
    }

    /// Every slot strictly below this millisecond has been processed (the
    /// `expiry_debt` lag metric reads `now - cursor`).
    #[inline]
    pub fn cursor_ms(&self) -> u64 {
        self.cursor_ms
    }

    /// Exact pool footprint in bytes (16 B/entry by construction — the
    /// M1-S04 attribution AC).
    #[inline]
    pub fn pool_bytes(&self) -> usize {
        self.pool.capacity() * size_of::<Node>()
    }

    /// Fixed slot-head footprint (heads + per-tier counters).
    #[inline]
    pub fn table_bytes(&self) -> usize {
        size_of::<[[u32; SLOTS]; TIERS]>() + size_of::<[u64; TIERS]>()
    }

    /// Files `{hash, deadline_ms}`. Deadlines at or before the cursor file
    /// into the imminent slot and fire on the next tick.
    pub fn arm(&mut self, hash: u64, deadline_ms: u64) -> ArmOutcome {
        let deadline_ms = deadline_ms.min(DEADLINE_MASK);
        let Some(node) = self.alloc(hash, deadline_ms) else {
            return ArmOutcome::PoolFull;
        };
        self.file(node);
        ArmOutcome::Armed
    }

    /// Advances toward `now_ms`, firing due entries through `fire(hash,
    /// deadline_ms)` under `budget`. The callback validates against the
    /// index and reaps (or drops a stale entry); the wheel only schedules.
    pub fn tick(
        &mut self,
        now_ms: u64,
        budget: ExpiryBudget,
        mut fire: impl FnMut(u64, u64),
    ) -> TickStats {
        let mut stats = TickStats::default();
        while self.cursor_ms <= now_ms {
            if stats.fired >= budget.max_fires || stats.steps >= budget.max_steps {
                return stats; // budget exhausted — debt stays visible
            }
            // Drain the tier-0 slot for the cursor millisecond. The fire
            // budget cuts MID-SLOT (a 1M-same-ms storm must not ride one
            // slot past the slice — M1-S05); the unprocessed chain splices
            // back and the cursor stays put for the next slice.
            let slot = (self.cursor_ms & (SLOTS as u64 - 1)) as usize;
            let mut at = self.heads[0][slot];
            let mut keep = NIL;
            let mut cut = false;
            while at != NIL {
                if stats.fired >= budget.max_fires {
                    cut = true;
                    break;
                }
                let node = self.pool[at as usize];
                let next = node.next();
                if node.deadline_ms() <= now_ms {
                    fire(node.hash, node.deadline_ms());
                    stats.fired += 1;
                    self.release(at, 0);
                } else {
                    // A future-window deadline sharing this slot index —
                    // keep it for the wrap that owns it.
                    self.pool[at as usize].set_next(keep);
                    keep = at;
                }
                at = next;
            }
            if cut {
                // Splice kept nodes onto the unprocessed remainder.
                let mut k = keep;
                while k != NIL {
                    let next = self.pool[k as usize].next();
                    self.pool[k as usize].set_next(at);
                    at = k;
                    k = next;
                }
                self.heads[0][slot] = at;
                return stats;
            }
            self.heads[0][slot] = keep;
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

    /// On each tier-t window boundary, re-file that tier's newly-current
    /// slot into lower tiers (deadlines are absolute; `file` re-derives the
    /// right tier from the new cursor).
    fn cascade_boundaries(&mut self) {
        for tier in 1..TIERS {
            let bits = SLOT_BITS * tier as u32;
            if self.cursor_ms & ((1 << bits) - 1) != 0 {
                break; // not a boundary of this tier (nor any higher one)
            }
            let slot = ((self.cursor_ms >> bits) & (SLOTS as u64 - 1)) as usize;
            let mut at = core::mem::replace(&mut self.heads[tier][slot], NIL);
            while at != NIL {
                let node = self.pool[at as usize];
                let next = node.next();
                self.tier_live[tier] -= 1;
                self.live -= 1; // file() re-adds
                let refiled = at;
                at = next;
                self.refile(refiled, node);
            }
        }
        // Tier-3 horizon crossing: pull overflow entries into range.
        if self.cursor_ms & (HORIZON_MS / SLOTS as u64 - 1) == 0 && self.overflow != NIL {
            let mut at = core::mem::replace(&mut self.overflow, NIL);
            let mut still_over = NIL;
            while at != NIL {
                let node = self.pool[at as usize];
                let next = node.next();
                if node.deadline_ms() >= self.cursor_ms + HORIZON_MS {
                    self.pool[at as usize].set_next(still_over);
                    still_over = at;
                } else {
                    self.overflow_live -= 1;
                    self.live -= 1;
                    self.refile(at, node);
                }
                at = next;
            }
            self.overflow = still_over;
        }
    }

    /// Skips empty stretches in O(1) per cascade boundary instead of O(ms):
    /// while tier 0 is empty, jump the cursor straight to the next boundary
    /// of the lowest non-empty tier (the only place new tier-0 work can
    /// appear), or to `now` when nothing is armed anywhere. Each jump
    /// charges one budget step — it is constant work.
    fn fast_forward(&mut self, now_ms: u64, stats: &mut TickStats) {
        loop {
            if self.cursor_ms > now_ms || self.tier_live[0] > 0 {
                return;
            }
            let align = if self.tier_live[1] > 0 {
                SLOT_BITS
            } else if self.tier_live[2] > 0 {
                SLOT_BITS * 2
            } else if self.tier_live[3] > 0 || self.overflow_live > 0 {
                SLOT_BITS * 3
            } else {
                // Nothing armed anywhere: snap to now.
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

    fn alloc(&mut self, hash: u64, deadline_ms: u64) -> Option<u32> {
        if self.free != NIL {
            let at = self.free;
            self.free = self.pool[at as usize].next();
            self.pool[at as usize] = Node::new(hash, deadline_ms, NIL);
            return Some(at);
        }
        if self.pool.len() >= POOL_CAP {
            return None;
        }
        self.pool.push(Node::new(hash, deadline_ms, NIL));
        Some((self.pool.len() - 1) as u32)
    }

    /// Returns a fired node to the free list (`tier` already decremented by
    /// the caller's list surgery — tier 0 drain path).
    fn release(&mut self, at: u32, tier: usize) {
        self.pool[at as usize].set_next(self.free);
        self.free = at;
        self.tier_live[tier] -= 1;
        self.live -= 1;
    }

    /// Files node `at` (fresh or cascading) by absolute deadline.
    fn file(&mut self, at: u32) {
        let node = self.pool[at as usize];
        self.refile(at, node);
    }

    fn refile(&mut self, at: u32, node: Node) {
        let deadline = node.deadline_ms();
        let delta = deadline.saturating_sub(self.cursor_ms);
        if delta >= HORIZON_MS {
            self.pool[at as usize].set_next(self.overflow);
            self.overflow = at;
            self.overflow_live += 1;
            self.live += 1;
            return;
        }
        // Smallest tier whose window still contains the deadline.
        let mut tier = 0;
        while tier < TIERS - 1 && delta >= (1 << (SLOT_BITS * (tier as u32 + 1))) {
            tier += 1;
        }
        let slot = ((deadline >> (SLOT_BITS * tier as u32)) & (SLOTS as u64 - 1)) as usize;
        self.pool[at as usize].set_next(self.heads[tier][slot]);
        self.heads[tier][slot] = at;
        self.tier_live[tier] += 1;
        self.live += 1;
    }
}

impl core::fmt::Debug for TtlWheel {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TtlWheel")
            .field("live", &self.live)
            .field("tier_live", &self.tier_live)
            .field("overflow_live", &self.overflow_live)
            .field("cursor_ms", &self.cursor_ms)
            .field("pool_len", &self.pool.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn drain_all(wheel: &mut TtlWheel, now: u64) -> Vec<(u64, u64)> {
        let mut fired = Vec::new();
        loop {
            let stats = wheel.tick(
                now,
                ExpiryBudget { max_fires: u32::MAX, max_steps: u32::MAX },
                |h, d| fired.push((h, d)),
            );
            if stats.caught_up {
                return fired;
            }
        }
    }

    #[test]
    fn node_is_sixteen_bytes() {
        assert_eq!(size_of::<Node>(), 16);
    }

    #[test]
    fn fires_exactly_at_deadline_never_early() {
        let mut wheel = TtlWheel::new(0);
        wheel.arm(0xAA, 100);
        assert!(drain_all(&mut wheel, 99).is_empty(), "fired early");
        assert_eq!(drain_all(&mut wheel, 100), vec![(0xAA, 100)]);
        assert!(drain_all(&mut wheel, 10_000).is_empty(), "double fire");
        assert_eq!(wheel.live(), 0);
    }

    #[test]
    fn every_tier_and_overflow_deliver() {
        let mut wheel = TtlWheel::new(0);
        // One deadline per tier window + one past the horizon.
        let deadlines = [3u64, 700, 300_000, 200_000_000, HORIZON_MS + 5_000];
        for (i, d) in deadlines.iter().enumerate() {
            assert_eq!(wheel.arm(i as u64, *d), ArmOutcome::Armed);
        }
        let mut fired = drain_all(&mut wheel, HORIZON_MS + 10_000);
        fired.sort_unstable();
        let want: Vec<(u64, u64)> =
            deadlines.iter().enumerate().map(|(i, d)| (i as u64, *d)).collect();
        assert_eq!(fired, want);
        assert_eq!(wheel.live(), 0);
    }

    #[test]
    fn budget_cuts_leave_debt_and_resume() {
        let mut wheel = TtlWheel::new(0);
        for i in 0..1000u64 {
            wheel.arm(i, 50); // 1000 same-millisecond deadlines (storm shape)
        }
        let mut fired = 0u64;
        let stats =
            wheel.tick(60, ExpiryBudget { max_fires: 64, max_steps: 4096 }, |_, _| fired += 1);
        assert!(!stats.caught_up, "must cut on fire budget");
        assert_eq!(fired, 64);
        assert_eq!(wheel.live(), 1000 - 64);
        // Resumes across ticks until drained.
        while !wheel
            .tick(60, ExpiryBudget { max_fires: 64, max_steps: 4096 }, |_, _| fired += 1)
            .caught_up
        {}
        assert_eq!(fired, 1000);
        assert_eq!(wheel.live(), 0);
    }

    #[test]
    fn random_deadlines_fire_exactly_once_in_window_oracle() {
        let mut wheel = TtlWheel::new(0);
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
            wheel.arm(i, deadline);
            oracle.insert(i, deadline);
        }
        // Advance in random jumps; every fire must match the oracle and be
        // unique; nothing may fire before its deadline.
        let mut now = 0u64;
        let mut fired: BTreeMap<u64, u64> = BTreeMap::new();
        while now < HORIZON_MS * 2 + 1024 {
            now += 1 + rand() % 50_000_000;
            for (h, d) in drain_all(&mut wheel, now) {
                assert!(d <= now, "fired before deadline");
                assert_eq!(oracle.get(&h), Some(&d), "wrong deadline for {h}");
                assert!(fired.insert(h, d).is_none(), "double fire for {h}");
            }
        }
        assert_eq!(fired.len(), oracle.len(), "missed fires");
        assert_eq!(wheel.live(), 0);
        // Pool memory is exactly 16 B per peak entry (attribution AC shape).
        assert_eq!(wheel.pool_bytes(), wheel.pool.capacity() * 16);
    }

    #[test]
    fn pool_reuse_keeps_capacity_bounded() {
        let mut wheel = TtlWheel::new(0);
        let mut now = 0;
        for round in 0..50u64 {
            for i in 0..100 {
                wheel.arm(i, now + 10 + i);
            }
            now += 2_000;
            let fired = drain_all(&mut wheel, now);
            assert_eq!(fired.len(), 100, "round {round}");
        }
        assert!(wheel.pool.capacity() <= 256, "pool ballooned: {}", wheel.pool.capacity());
    }
}
