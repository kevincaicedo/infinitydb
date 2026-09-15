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
    /// Refs sitting above level 0 (stale ones included — they leave at
    /// their cascade). Non-zero means a boundary crossing may surface a
    /// timer, so the park must not sleep past it (N17, batch 61).
    refs_above0: usize,
    /// Slot vector recycled through every crossing (no per-tick malloc).
    scratch: Vec<TimerRef>,
    /// Ticks walked one by one (the jump witness).
    #[cfg(test)]
    steps: u64,
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
            refs_above0: 0,
            scratch: Vec::new(),
            #[cfg(test)]
            steps: 0,
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
    /// timer in deadline order per slot. Costs the events crossed, not the
    /// ticks: the walk jumps to the next tick where a level-0 timer is due
    /// or a cascade boundary lies (batch 61 — a DST clock jump of an hour
    /// used to walk 3.6 million ticks).
    pub fn advance(&mut self, now: Nanos, mut fire: impl FnMut(u64)) {
        let target = now.0 / TICK_NS;
        if self.live == 0 {
            // Nothing armed: jump without walking empty slots.
            self.now_tick = self.now_tick.max(target);
            return;
        }
        while self.now_tick < target {
            let next = self.next_event_tick();
            if next > target {
                self.now_tick = target;
                break;
            }
            self.now_tick = next;
            #[cfg(test)]
            {
                self.steps += 1;
            }
            let t = self.now_tick;
            // Cascade higher levels whose slot boundary we just crossed.
            for level in (1..LEVELS).rev() {
                let span = 1u64 << (SLOT_BITS * level as u32);
                if t.is_multiple_of(span) {
                    let slot = ((t >> (SLOT_BITS * level as u32)) & (SLOTS as u64 - 1)) as usize;
                    let refs = self.take_slot(level, slot);
                    self.refs_above0 -= refs.len();
                    for &r in &refs {
                        if let Some(entry) = self.live_entry(r) {
                            let deadline_tick = entry.deadline_tick;
                            self.place(r, deadline_tick);
                        }
                    }
                    self.recycle(refs);
                }
            }
            // Fire level 0.
            let slot = (t & (SLOTS as u64 - 1)) as usize;
            let refs = self.take_slot(0, slot);
            for &(idx, generation) in &refs {
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
            self.recycle(refs);
        }
    }

    /// The next tick at which anything can happen: the earliest level-0
    /// deadline in the horizon (exact — a level-0 entry is due within 64
    /// ticks) or the earliest boundary that crosses a non-empty higher
    /// slot (conservative: a timer never fires before its own cascade
    /// tick, so nothing is skipped). Empty boundaries are not events.
    fn next_event_tick(&self) -> u64 {
        let due_at = |t: u64| {
            let slot = (t & (SLOTS as u64 - 1)) as usize;
            self.levels[0][slot]
                .iter()
                .any(|r| self.live_entry(*r).is_some_and(|e| e.deadline_tick == t))
        };
        let level0 = (self.now_tick + 1..=self.now_tick + SLOTS as u64).find(|&t| due_at(t));
        let cascade = if self.refs_above0 > 0 { self.next_cascade_tick() } else { None };
        match (level0, cascade) {
            (Some(a), Some(b)) => a.min(b),
            (Some(t), None) | (None, Some(t)) => t,
            (None, None) => self.now_tick + SLOTS as u64,
        }
    }

    /// The earliest boundary whose crossing takes a non-empty slot at any
    /// level above 0 — the cascade schedule, `levels × slots` checks.
    fn next_cascade_tick(&self) -> Option<u64> {
        let mut best: Option<u64> = None;
        for level in 1..LEVELS {
            let shift = SLOT_BITS * level as u32;
            let block = self.now_tick >> shift;
            for k in 1..=SLOTS as u64 {
                let slot = ((block + k) & (SLOTS as u64 - 1)) as usize;
                if !self.levels[level][slot].is_empty() {
                    let t = (block + k) << shift;
                    best = Some(best.map_or(t, |b| b.min(t)));
                    break;
                }
            }
        }
        best
    }

    /// Swap a slot's refs out through the scratch vector (capacity kept).
    fn take_slot(&mut self, level: usize, slot: usize) -> Vec<TimerRef> {
        let scratch = core::mem::take(&mut self.scratch);
        core::mem::replace(&mut self.levels[level][slot], scratch)
    }

    fn recycle(&mut self, mut refs: Vec<TimerRef>) {
        refs.clear();
        // Keep the larger of the two so capacity ratchets up, never down.
        if refs.capacity() >= self.scratch.capacity() {
            self.scratch = refs;
        }
    }

    /// Earliest moment a timer could fire — the park timeout. Conservative:
    /// never later than the true next fire; may be earlier when the next
    /// timer sits above level 0 (the park wakes at the cascade boundary and
    /// re-parks). `None` when nothing is armed. Pre-batch-61 a level-0
    /// deadline past the boundary was preferred over the boundary itself,
    /// and a timer cascading there fired up to 63 ms late (N17).
    pub fn next_deadline(&self) -> Option<Nanos> {
        if self.live == 0 {
            return None;
        }
        Some(Nanos(self.next_event_tick() * TICK_NS))
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
        debug_assert!(deadline_tick >= self.now_tick, "placing a timer in the past");
        // `delta == 0` is legal: a cascade landing exactly on the deadline
        // tick re-places the entry into level 0's current slot, which fires
        // later in this same `advance` tick (found by the M2-S05 everysec
        // sweep — deadlines on 64-tick boundaries hit this).
        let delta = deadline_tick - self.now_tick;
        for level in 0..LEVELS {
            let horizon = 1u64 << (SLOT_BITS * (level as u32 + 1));
            if delta < horizon || level == LEVELS - 1 {
                let slot =
                    ((deadline_tick >> (SLOT_BITS * level as u32)) & (SLOTS as u64 - 1)) as usize;
                self.levels[level][slot].push(r);
                if level > 0 {
                    self.refs_above0 += 1;
                }
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

    /// Batch 61 (N17, found under lane L11's `timer.rs:110-143` row): a
    /// timer one level up cascades at the next 64-tick boundary and may be
    /// due *before* a level-0 timer that sits past that boundary. The park
    /// deadline must never be later than the earlier one — pre-fix it
    /// named the level-0 tick and the cascaded timer fired up to 63 ms late.
    #[test]
    fn next_deadline_never_passes_a_cascade_that_fires_earlier() {
        let mut wheel = TimerWheel::new();
        wheel.advance(ms(1), |_| {});
        wheel.insert(ms(65), 65); // delta 64: level 1, cascades at tick 64
        wheel.advance(ms(10), |_| {});
        wheel.insert(ms(70), 70); // delta 60: level 0
        let park = wheel.next_deadline().expect("armed");
        assert!(park <= ms(65), "park deadline {park:?} sleeps past the 65 ms timer");
        let mut fired = Vec::new();
        wheel.advance(park, |k| fired.push(k));
        wheel.advance(ms(65), |k| fired.push(k));
        assert_eq!(fired, vec![65], "the cascaded timer fires at its own tick");
    }

    /// Batch 61 (lane L11 `timer.rs:110-143`): a clock jump costs the
    /// events in it, not the ticks. One timer an hour out; pre-fix the
    /// advance walked 3 600 000 ticks.
    #[test]
    fn advance_jumps_over_ticks_where_nothing_happens() {
        let mut wheel = TimerWheel::new();
        wheel.insert(ms(3_600_000), 1);
        wheel.insert(ms(5), 2);
        let mut fired = Vec::new();
        wheel.advance(ms(3_600_000), |k| fired.push(k));
        assert_eq!(fired, vec![2, 1]);
        assert_eq!(wheel.armed(), 0);
        // One step per event tick and per cascade boundary crossed: far
        // fewer than the ticks in an hour (levels × slots is the bound).
        assert!(wheel.steps <= (LEVELS * SLOTS) as u64, "walked {} ticks", wheel.steps);
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
