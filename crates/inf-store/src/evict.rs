//! Eviction engine **v1** (M1-S06/S07, master plan §7.4): per-cell
//! clock-sweep over index slot regions + an 8 KiB Count-Min Sketch with
//! Morris-style probabilistic counters for the LFU tiers. All 8 Redis
//! policies. Policy logic lives in this module only — `CellStore` exposes
//! mechanism (candidate iteration via [`Index::live_walk`], reference bits
//! in record flags, reap) and [`Keyspace`](crate::Keyspace) drives pressure;
//! that is the frozen §3.2 eviction-hook seam (M7 tiering reuses it).
//!
//! ## Recency: CLOCK, not timestamped LRU (recorded design decision)
//!
//! Redis approximates LRU with a 24-bit per-object clock; record format v0
//! has no spare bytes (the 8-byte header is load-bearing for the L5 gate).
//! The two spare *bits* in the flags nibble hold a saturating 2-bit
//! reference counter instead: every access sets it to 3 (one OR on a cache
//! line the access already owns), the sweep hand decrements, and a record at
//! 0 is a victim — classic CLOCK with 4 generations. Zero bytes per record;
//! the hit-rate parity gate (M1-S06 AC, zipfian vs Redis) is the artifact
//! that judges the approximation.
//!
//! ## Frequency: CMS + Morris (8 KiB exactly, L5-bounded)
//!
//! 4 rows × 2048 one-byte counters = 8192 B per store, allocated only while
//! an LFU policy is active. Increment is conservative-update (only the
//! current minimum cells bump) and Morris-style probabilistic — bump chance
//! 1/(min·FACTOR + 1), the Redis `lfu-log-factor` shape — so 8-bit counters
//! cover huge frequency ranges. Aging halves every counter on a fixed
//! injected-time period (`DECAY_PERIOD_MS`); an 8 KiB sweep is microseconds
//! and stays off the hot path (MAINTAIN slice). All randomness comes from a
//! seeded SplitMix64 stream owned by the store — deterministic under DST
//! (L7).

use inf_foundation::time::Nanos;

use crate::record::RecordView;
use crate::store::CellStore;

/// The eight Redis `maxmemory-policy` values (M1-S06).
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub enum EvictionPolicy {
    #[default]
    NoEviction,
    AllKeysLru,
    VolatileLru,
    AllKeysRandom,
    VolatileRandom,
    VolatileTtl,
    AllKeysLfu,
    VolatileLfu,
}

impl EvictionPolicy {
    /// Parses the Redis config token (`CONFIG SET maxmemory-policy`).
    pub fn parse(text: &str) -> Option<EvictionPolicy> {
        Some(match text {
            "noeviction" => EvictionPolicy::NoEviction,
            "allkeys-lru" => EvictionPolicy::AllKeysLru,
            "volatile-lru" => EvictionPolicy::VolatileLru,
            "allkeys-random" => EvictionPolicy::AllKeysRandom,
            "volatile-random" => EvictionPolicy::VolatileRandom,
            "volatile-ttl" => EvictionPolicy::VolatileTtl,
            "allkeys-lfu" => EvictionPolicy::AllKeysLfu,
            "volatile-lfu" => EvictionPolicy::VolatileLfu,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            EvictionPolicy::NoEviction => "noeviction",
            EvictionPolicy::AllKeysLru => "allkeys-lru",
            EvictionPolicy::VolatileLru => "volatile-lru",
            EvictionPolicy::AllKeysRandom => "allkeys-random",
            EvictionPolicy::VolatileRandom => "volatile-random",
            EvictionPolicy::VolatileTtl => "volatile-ttl",
            EvictionPolicy::AllKeysLfu => "allkeys-lfu",
            EvictionPolicy::VolatileLfu => "volatile-lfu",
        }
    }

    /// Only TTL-bearing records qualify as victims.
    #[inline]
    pub fn volatile_only(self) -> bool {
        matches!(
            self,
            EvictionPolicy::VolatileLru
                | EvictionPolicy::VolatileRandom
                | EvictionPolicy::VolatileTtl
                | EvictionPolicy::VolatileLfu
        )
    }

    /// Access tracking this policy needs on the read/write path.
    #[inline]
    pub(crate) fn tracking(self) -> Tracking {
        match self {
            EvictionPolicy::AllKeysLru | EvictionPolicy::VolatileLru => Tracking::Clock,
            EvictionPolicy::AllKeysLfu | EvictionPolicy::VolatileLfu => Tracking::Lfu,
            _ => Tracking::None,
        }
    }
}

/// Hot-path access-tracking mode (one cached branch — M1-S07).
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub(crate) enum Tracking {
    #[default]
    None,
    /// Saturate the 2-bit flag counter on access.
    Clock,
    /// Morris-bump the CMS on access.
    Lfu,
}

// ---- Count-Min Sketch ----------------------------------------------------------

const CMS_ROWS: usize = 4;
const CMS_COLS: usize = 2048; // 4 × 2048 × 1 B = 8 KiB exactly (§7.4)
const CMS_COL_BITS: u32 = 11;
/// Morris growth factor (the Redis `lfu-log-factor` default shape).
const MORRIS_FACTOR: u64 = 10;
/// Halve every counter this often on the injected clock (Redis
/// `lfu-decay-time` ≈ 1 minute).
pub(crate) const DECAY_PERIOD_MS: u64 = 60_000;

/// 8 KiB Count-Min Sketch with Morris counters. Boxed and live only while an
/// LFU policy is selected (slim builds and non-LFU namespaces pay nothing).
pub(crate) struct Cms {
    rows: Box<[u8; CMS_ROWS * CMS_COLS]>,
}

impl Cms {
    pub fn new() -> Cms {
        Cms { rows: Box::new([0; CMS_ROWS * CMS_COLS]) }
    }

    /// Exact footprint (L5 attribution: `cms_bytes` domain).
    pub fn bytes(&self) -> usize {
        CMS_ROWS * CMS_COLS
    }

    /// The four row cells of `hash` — 11 disjoint bits of the key hash per
    /// row (44 of 64 bits consumed; the index uses different fragments).
    #[inline]
    fn cells(hash: u64) -> [usize; CMS_ROWS] {
        let mut at = [0usize; CMS_ROWS];
        let mut i = 0;
        while i < CMS_ROWS {
            let col = ((hash >> (i as u32 * CMS_COL_BITS)) & (CMS_COLS as u64 - 1)) as usize;
            at[i] = i * CMS_COLS + col;
            i += 1;
        }
        at
    }

    /// Frequency estimate: min over the four rows.
    #[inline]
    pub fn estimate(&self, hash: u64) -> u8 {
        Self::cells(hash).into_iter().map(|c| self.rows[c]).min().unwrap_or(0)
    }

    /// Morris + conservative update: with probability `1/(min·F + 1)` bump
    /// only the cells currently at the minimum. `roll` is one draw from the
    /// store's seeded stream (L7: injected randomness only).
    #[inline]
    pub fn touch(&mut self, hash: u64, roll: u64) {
        let cells = Self::cells(hash);
        let min = cells.iter().map(|&c| self.rows[c]).min().unwrap_or(0);
        if min == u8::MAX {
            return;
        }
        // P(bump) = 1/(min·F + 1) without floats: roll < 2^64 / (min·F + 1).
        let gate = u64::MAX / (u64::from(min) * MORRIS_FACTOR + 1);
        if roll > gate {
            return;
        }
        for c in cells {
            if self.rows[c] == min {
                self.rows[c] += 1;
            }
        }
    }

    /// Aging: halve every counter (MAINTAIN slice, every `DECAY_PERIOD_MS`).
    pub fn decay(&mut self) {
        for cell in self.rows.iter_mut() {
            *cell >>= 1;
        }
    }
}

// ---- per-store eviction state ----------------------------------------------------

/// Per-store eviction mechanism state (owned by `CellStore`; driven by
/// `Keyspace` pressure).
#[derive(Default)]
pub(crate) struct EvictState {
    pub policy: EvictionPolicy,
    pub tracking: Tracking,
    /// Clock hand: next index slot the sweep visits.
    pub hand: usize,
    /// CMS, live only under an LFU policy.
    pub cms: Option<Cms>,
    /// SplitMix64 state (Morris rolls, random-policy slot rolls). Seeded by
    /// `Keyspace` from the injected node seed (L7).
    pub rng: u64,
    /// Last CMS decay instant (injected clock ms).
    pub last_decay_ms: u64,
}

impl EvictState {
    /// Applies a policy change: tracking mode flips and the CMS is
    /// allocated/dropped to match (8 KiB only while LFU is selected).
    pub fn set_policy(&mut self, policy: EvictionPolicy) {
        self.policy = policy;
        self.tracking = policy.tracking();
        match (self.tracking, self.cms.is_some()) {
            (Tracking::Lfu, false) => self.cms = Some(Cms::new()),
            (Tracking::Lfu, true) => {}
            (_, _) => self.cms = None,
        }
    }

    #[inline]
    pub fn next_roll(&mut self) -> u64 {
        self.rng = self.rng.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.rng;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    pub fn bytes(&self) -> usize {
        self.cms.as_ref().map_or(0, Cms::bytes)
    }
}

/// One eviction step result.
#[derive(Copy, Clone, Default, Debug)]
pub struct EvictStats {
    /// Records evicted (not counting expired records reaped en route).
    pub evicted: u64,
    /// Encoded bytes returned to the arena by evictions.
    pub freed_bytes: u64,
    /// Index slots the hand examined.
    pub scanned_slots: u64,
}

impl EvictStats {
    pub fn absorb(&mut self, other: EvictStats) {
        self.evicted += other.evicted;
        self.freed_bytes += other.freed_bytes;
        self.scanned_slots += other.scanned_slots;
    }
}

/// Slot window one selection step may examine before settling for the best
/// candidate seen (bounded everything — a sparse table cannot stall a slice).
const MAX_SLOTS_PER_STEP: usize = 256;

/// Evicts (at most) one victim from `store` under `policy`. Returns the
/// stats of the step; `evicted == 0` means no qualifying candidate exists in
/// the examined window (the caller decides whether that is OOM).
///
/// Expired records met during the sweep are reaped as *expirations* (cheaper
/// than evicting live data and already-correct memory to reclaim first).
pub(crate) fn evict_one(store: &mut CellStore, samples: u32, now: Nanos) -> EvictStats {
    let mut stats = EvictStats::default();
    let policy = store.evict.policy;
    if policy == EvictionPolicy::NoEviction || store.index.is_empty() {
        return stats;
    }
    let volatile = policy.volatile_only();
    let random = matches!(policy, EvictionPolicy::AllKeysRandom | EvictionPolicy::VolatileRandom);
    // Random policies roll a fresh slot per step; clock policies resume the
    // hand (aging persists across steps — that IS the clock).
    let mut hand = if random {
        (store.evict.next_roll() as usize) & (store.index.capacity() - 1)
    } else {
        store.evict.hand
    };

    let clock = matches!(policy, EvictionPolicy::AllKeysLru | EvictionPolicy::VolatileLru);
    // CLOCK is a sweep, not a sample: the hand decrements every record it
    // passes and stops at the first generation-0 victim (the slot window
    // bounds the work). Sampled policies (LFU/TTL/random) examine
    // `samples` candidates per Redis `maxmemory-samples`.
    let sample_cap = if clock { usize::MAX } else { samples.max(1) as usize };
    // Best victim so far: (score, addr, key_hash, encoded_len, had_ttl).
    let mut best: Option<(u64, inf_alloc::ArenaAddr, u64, usize, bool)> = None;
    let mut seen = 0usize;
    let mut found_zero = false;
    let mut expired: Vec<(u64, inf_alloc::ArenaAddr, usize)> = Vec::new();
    let mut aged: Vec<inf_alloc::ArenaAddr> = Vec::new();

    let mut slots_left = MAX_SLOTS_PER_STEP.min(store.index.capacity());
    while slots_left > 0 && seen < sample_cap && !found_zero {
        let span = slots_left.min(64);
        {
            let arena = &store.arena;
            let index = &store.index;
            let cms = store.evict.cms.as_ref();
            hand = index.live_walk(hand, span, |addr| {
                if seen >= sample_cap || found_zero {
                    return;
                }
                let view = crate::store::record_at(arena, addr);
                let hash = CellStore::hash_key(view.key());
                if view.is_expired(now) {
                    expired.push((hash, addr, view.encoded_len()));
                    return;
                }
                let deadline = view.expire_at_ms();
                if volatile && deadline.is_none() {
                    return;
                }
                seen += 1;
                let score = score_of(policy, &view, hash, cms);
                // CLOCK aging: a scanned non-victim loses one generation;
                // the first generation-0 record ends the sweep (the hand
                // stops at its victim).
                if clock {
                    if score == 0 {
                        found_zero = true;
                    } else {
                        aged.push(addr);
                    }
                }
                let len = view.encoded_len();
                let had_ttl = deadline.is_some();
                if best.is_none_or(|(s, ..)| score < s) {
                    best = Some((score, addr, hash, len, had_ttl));
                }
            });
        }
        stats.scanned_slots += span as u64;
        slots_left -= span;
        if random && seen > 0 {
            break; // random policies take the first qualifying sample
        }
    }
    if !random {
        store.evict.hand = hand;
    }
    // Decrement happens after the walk (the walk borrows the arena shared).
    for addr in aged {
        store.age_record(addr);
    }
    for (hash, addr, len) in expired {
        store.reap_expired_at(hash, addr, len);
        stats.freed_bytes += len as u64;
    }
    if let Some((_, addr, hash, len, had_ttl)) = best {
        store.evict_record(hash, addr, len, had_ttl);
        stats.evicted = 1;
        stats.freed_bytes += len as u64;
    }
    stats
}

/// Lower score = better victim.
#[inline]
fn score_of(policy: EvictionPolicy, view: &RecordView<'_>, hash: u64, cms: Option<&Cms>) -> u64 {
    match policy {
        // CLOCK: generation 0 evicts first; ties resolved by walk order.
        EvictionPolicy::AllKeysLru | EvictionPolicy::VolatileLru => u64::from(view.ref_level()),
        // LFU: CMS estimate (Morris-scaled).
        EvictionPolicy::AllKeysLfu | EvictionPolicy::VolatileLfu => {
            u64::from(cms.map_or(0, |c| c.estimate(hash)))
        }
        // volatile-ttl: nearest deadline first.
        EvictionPolicy::VolatileTtl => view.expire_at_ms().unwrap_or(u64::MAX),
        // Random: every sample scores equally; the first wins.
        _ => 0,
    }
}

/// Runs the CMS decay schedule (called from the store's MAINTAIN seam).
pub(crate) fn maybe_decay(state: &mut EvictState, now: Nanos) {
    let now_ms = now.0 / 1_000_000;
    if let Some(cms) = state.cms.as_mut() {
        if state.last_decay_ms == 0 {
            state.last_decay_ms = now_ms;
        } else if now_ms.saturating_sub(state.last_decay_ms) >= DECAY_PERIOD_MS {
            cms.decay();
            state.last_decay_ms = now_ms;
        }
    } else {
        state.last_decay_ms = now_ms;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cms_is_exactly_8_kib_and_estimates_monotonically() {
        let mut cms = Cms::new();
        assert_eq!(cms.bytes(), 8192);
        let hot = 0xDEAD_BEEF_F00D_u64;
        // Deterministic rolls: always bump (roll = 0 passes every gate).
        for _ in 0..8 {
            cms.touch(hot, 0);
        }
        let hot_est = cms.estimate(hot);
        assert!(hot_est >= 8, "conservative update with certain rolls counts exactly: {hot_est}");
        assert_eq!(cms.estimate(0x1234_5678), 0, "untouched key estimates 0");
        cms.decay();
        assert_eq!(cms.estimate(hot), hot_est / 2, "decay halves");
    }

    #[test]
    fn morris_gate_saturates_instead_of_overflowing() {
        let mut cms = Cms::new();
        for _ in 0..100_000 {
            cms.touch(42, 0); // certain bumps
        }
        assert_eq!(cms.estimate(42), u8::MAX, "saturates at 255");
    }

    #[test]
    fn policy_parse_roundtrips_all_eight() {
        for name in [
            "noeviction",
            "allkeys-lru",
            "volatile-lru",
            "allkeys-random",
            "volatile-random",
            "volatile-ttl",
            "allkeys-lfu",
            "volatile-lfu",
        ] {
            let policy = EvictionPolicy::parse(name).expect(name);
            assert_eq!(policy.name(), name);
        }
        assert_eq!(EvictionPolicy::parse("lru"), None);
    }

    #[test]
    fn set_policy_allocates_cms_only_for_lfu() {
        let mut state = EvictState::default();
        assert_eq!(state.bytes(), 0);
        state.set_policy(EvictionPolicy::AllKeysLfu);
        assert_eq!(state.bytes(), 8192);
        state.set_policy(EvictionPolicy::AllKeysLru);
        assert_eq!(state.bytes(), 0, "CMS freed when LFU deselected");
    }
}
