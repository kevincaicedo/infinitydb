//! Hierarchical timing wheel v0 (M0 scope: connection/idle deadlines and
//! the loop's park timeout — the TTL expiry wheel is a separate M1 design).
//!
//! 6 levels × 64 slots, 1 ms ticks ⇒ ~2.2 years of range; O(1) insert and
//! cancel, O(slots crossed) advance. Entries cascade down one level when
//! their parent slot is crossed (Kafka/tokio shape). Timers fire strictly
//! at-or-after their deadline; [`TimerWheel::next_deadline`] is conservative
//! (never later than the true next fire, may be earlier across cascade
//! boundaries) — exactly what a park timeout needs.

use inf_foundation::time::Nanos;

const SLOT_BITS: u32 = 6;
const SLOTS: usize = 1 << SLOT_BITS; // 64
const LEVELS: usize = 6;
const TICK_NS: u64 = 1_000_000; // 1 ms

/// Handle for cancellation. Stale ids (already fired/cancelled) are
/// detected by generation and rejected.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct TimerId {
    idx: u32,
    generation: u64,
}

struct TimerEntry {
    deadline_tick: u64,
    key: u64,
    generation: u64,
}

/// `(slab idx, generation)` — wheel slots hold these; the slab is the
/// source of truth, so cancellation is O(1) and slot refs go stale lazily.
type TimerRef = (u32, u64);

pub struct TimerWheel {
    /// `levels[l][slot]` holds entries due within `64^(l+1)` ticks.
    levels: Vec<Vec<Vec<TimerRef>>>,
    slab: Vec<Option<TimerEntry>>,
    free: Vec<u32>,
    now_tick: u64,
    next_generation: u64,
    live: usize,
}

impl Default for TimerWheel {
    fn default() -> Self {
        Self::new()
    }
}

impl TimerWheel {
    pub fn new() -> TimerWheel {
        TimerWheel {
            levels: (0..LEVELS).map(|_| (0..SLOTS).map(|_| Vec::new()).collect()).collect(),
            slab: Vec::new(),
            free: Vec::new(),
            now_tick: 0,
            next_generation: 0,
            live: 0,
        }
    }

    /// Arm a timer. `key` is the caller's routing value (connection slot,
    /// waitlist key…) delivered to the `fire` callback on expiry. Deadlines
    /// at or before "now" fire on the next [`Self::advance`].
    pub fn insert(&mut self, deadline: Nanos, key: u64) -> TimerId {
        let deadline_tick = deadline.0.div_ceil(TICK_NS).max(self.now_tick + 1);
        self.next_generation += 1;
        let generation = self.next_generation;
        let idx = match self.free.pop() {
            Some(i) => i,
            None => {
                let i = u32::try_from(self.slab.len()).expect("timer slab exceeds u32");
                self.slab.push(None);
                i
            }
        };
        self.slab[idx as usize] = Some(TimerEntry { deadline_tick, key, generation });
        self.live += 1;
        self.place((idx, generation), deadline_tick);
        TimerId { idx, generation }
    }

    /// Disarm. Returns `false` for stale ids (already fired or cancelled).
    pub fn cancel(&mut self, id: TimerId) -> bool {
        let Some(slot) = self.slab.get_mut(id.idx as usize) else { return false };
        match slot {
            Some(entry) if entry.generation == id.generation => {
                *slot = None;
                self.free.push(id.idx);
                self.live -= 1;
                // The wheel still holds a stale (idx, generation) ref; it is
                // skipped when its slot is crossed.
                true
            }
            _ => false,
        }
    }

    /// Move time forward to `now`, invoking `fire(key)` for every expired
    /// timer in deadline order per slot.
    pub fn advance(&mut self, now: Nanos, mut fire: impl FnMut(u64)) {
        let target = now.0 / TICK_NS;
        if self.live == 0 {
            // Nothing armed: jump without walking empty slots.
            self.now_tick = self.now_tick.max(target);
            return;
        }
        while self.now_tick < target {
            self.now_tick += 1;
            let t = self.now_tick;
            // Cascade higher levels whose slot boundary we just crossed.
            for level in (1..LEVELS).rev() {
                let span = 1u64 << (SLOT_BITS * level as u32);
                if t.is_multiple_of(span) {
                    let slot = ((t >> (SLOT_BITS * level as u32)) & (SLOTS as u64 - 1)) as usize;
                    let refs = core::mem::take(&mut self.levels[level][slot]);
                    for r in refs {
                        if let Some(entry) = self.live_entry(r) {
                            let deadline_tick = entry.deadline_tick;
                            self.place(r, deadline_tick);
                        }
                    }
                }
            }
            // Fire level 0.
            let slot = (t & (SLOTS as u64 - 1)) as usize;
            let refs = core::mem::take(&mut self.levels[0][slot]);
            for (idx, generation) in refs {
                match &self.slab[idx as usize] {
                    Some(entry) if entry.generation == generation => {
                        debug_assert!(entry.deadline_tick <= t, "level-0 entry not yet due");
                        let key = entry.key;
                        self.slab[idx as usize] = None;
                        self.free.push(idx);
                        self.live -= 1;
                        fire(key);
                    }
                    _ => {} // stale ref (cancelled): skip
                }
            }
        }
    }

    /// Earliest moment a timer could fire — the park timeout. Conservative:
    /// never later than the true next fire; may be earlier when the next
    /// timer sits above level 0 (the park wakes at the cascade boundary and
    /// re-parks). `None` when nothing is armed.
    pub fn next_deadline(&self) -> Option<Nanos> {
        if self.live == 0 {
            return None;
        }
        // Exact within level 0's horizon (the next 64 ticks)…
        let mut earliest: Option<u64> = None;
        for t in (self.now_tick + 1)..=(self.now_tick + SLOTS as u64) {
            let slot = (t & (SLOTS as u64 - 1)) as usize;
            for r in &self.levels[0][slot] {
                if let Some(entry) = self.live_entry(*r)
                    && entry.deadline_tick == t
                {
                    earliest = Some(t);
                    break;
                }
            }
            if earliest.is_some() {
                break;
            }
        }
        // …otherwise the next level-1 cascade boundary bounds it.
        let tick = earliest.unwrap_or_else(|| (self.now_tick / SLOTS as u64 + 1) * SLOTS as u64);
        Some(Nanos(tick * TICK_NS))
    }

    /// Armed timers (tests + leak asserts).
    pub fn armed(&self) -> usize {
        self.live
    }

    fn live_entry(&self, (idx, generation): TimerRef) -> Option<&TimerEntry> {
        self.slab[idx as usize].as_ref().filter(|e| e.generation == generation)
    }

    /// Place a ref in the lowest level whose horizon covers the deadline.
    fn place(&mut self, r: TimerRef, deadline_tick: u64) {
        let delta = deadline_tick - self.now_tick;
        debug_assert!(delta > 0);
        for level in 0..LEVELS {
            let horizon = 1u64 << (SLOT_BITS * (level as u32 + 1));
            if delta < horizon || level == LEVELS - 1 {
                let slot =
                    ((deadline_tick >> (SLOT_BITS * level as u32)) & (SLOTS as u64 - 1)) as usize;
                self.levels[level][slot].push(r);
                return;
            }
        }
        unreachable!("top level catches all deltas");
    }
}

impl core::fmt::Debug for TimerWheel {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "TimerWheel {{ armed: {}, now_tick: {} }}", self.live, self.now_tick)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(v: u64) -> Nanos {
        Nanos::from_millis(v)
    }

    #[test]
    fn fires_in_order_at_deadline() {
        let mut wheel = TimerWheel::new();
        wheel.insert(ms(5), 5);
        wheel.insert(ms(2), 2);
        wheel.insert(ms(9), 9);
        let mut fired = Vec::new();
        wheel.advance(ms(1), |k| fired.push(k));
        assert!(fired.is_empty());
        wheel.advance(ms(6), |k| fired.push(k));
        assert_eq!(fired, vec![2, 5]);
        wheel.advance(ms(20), |k| fired.push(k));
        assert_eq!(fired, vec![2, 5, 9]);
        assert_eq!(wheel.armed(), 0);
    }

    #[test]
    fn cancel_prevents_fire_and_detects_stale() {
        let mut wheel = TimerWheel::new();
        let id = wheel.insert(ms(3), 1);
        assert!(wheel.cancel(id));
        assert!(!wheel.cancel(id), "second cancel is stale");
        let mut fired = Vec::new();
        wheel.advance(ms(10), |k| fired.push(k));
        assert!(fired.is_empty());
        assert_eq!(wheel.armed(), 0);
    }

    #[test]
    fn cascades_across_levels() {
        let mut wheel = TimerWheel::new();
        // Far enough to start at level 2 (≥ 64² ticks = 4096 ms).
        wheel.insert(ms(5000), 42);
        wheel.insert(ms(70), 7); // level 1
        let mut fired = Vec::new();
        wheel.advance(ms(4999), |k| fired.push(k));
        assert_eq!(fired, vec![7]);
        wheel.advance(ms(5001), |k| fired.push(k));
        assert_eq!(fired, vec![7, 42]);
    }

    #[test]
    fn next_deadline_is_conservative() {
        let mut wheel = TimerWheel::new();
        assert_eq!(wheel.next_deadline(), None);
        wheel.insert(ms(3), 1);
        let nd = wheel.next_deadline().expect("armed");
        assert!(nd <= ms(3), "park timeout must never overshoot the deadline");
        wheel.insert(ms(5000), 2);
        wheel.advance(ms(10), |_| {});
        let nd = wheel.next_deadline().expect("level-2 timer still armed");
        assert!(nd <= ms(5000));
    }

    #[test]
    fn idle_jump_does_not_walk_ticks() {
        let mut wheel = TimerWheel::new();
        wheel.advance(Nanos::from_secs(3600), |_| panic!("nothing armed"));
        let id = wheel.insert(Nanos(Nanos::from_secs(3600).0 + ms(2).0), 1);
        let mut fired = Vec::new();
        wheel.advance(Nanos(Nanos::from_secs(3600).0 + ms(5).0), |k| fired.push(k));
        assert_eq!(fired, vec![1]);
        assert!(!wheel.cancel(id), "fired timers are stale");
    }
}
