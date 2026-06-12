//! `Keyspace` — one cell's slice of every namespace (M1-E3/E4): the 16
//! default Redis databases (`SELECT 0..15`) as lazily-materialized
//! [`CellStore`]s, the named-namespace registry, and the memory-pressure
//! driver that turns `maxmemory`/`maxmemory-policy` into bounded eviction.
//!
//! ## Pressure model (M1-S07)
//!
//! `maxmemory` is the node-wide budget (Redis semantics); the server layer
//! hands each cell `maxmemory / cells` — cells are symmetric by contiguous
//! slot ranges, so per-cell division preserves the global bound without any
//! shared state (L1). The default databases share the budget exactly like
//! Redis databases share the instance budget; named namespaces carry their
//! own (dormant until M2 makes them addressable — see `ns.rs`).
//!
//! The write path pays **one branch on a cached flag** (`over_limit`): the
//! flag is recomputed after mutations and after eviction slices, never
//! probed by summation on the per-command fast path when no limit is set.
//! Pressure work is bounded everywhere: inline (write-blocking) eviction
//! frees at most [`INLINE_MAX_EVICTIONS`] victims before issuing the honest
//! OOM verdict; the MAINTAIN slice drives usage down to the low watermark
//! (`limit − limit/16`, hysteresis) under its own budget so a storm of
//! writes cannot monopolize the loop (the bounded-everything rule).

use inf_foundation::time::Nanos;

use crate::evict::{EvictStats, EvictionPolicy};
use crate::ns::{NsError, NsRegistry, NsSpec};
use crate::store::{CellStore, ExpiryStats, MemoryReport, OpError, StoreConfig, StoreStats};
use crate::wheel::ExpiryBudget;

/// Redis default database count (`SELECT 0..15`; CONFIG `databases`).
pub const DEFAULT_DBS: usize = 16;

/// Victims one blocked write may evict inline before the OOM verdict
/// (eviction-vs-write races resolve by escalation, bounded — M1-S07).
/// Steady-state pressure needs ~1 victim per write; the headroom covers
/// bursts. A budget shrink larger than this answers OOM transiently until
/// the MAINTAIN slice drains to the watermark — the bounded-everything
/// trade (Redis evicts unboundedly inline; recorded deviation).
const INLINE_MAX_EVICTIONS: u32 = 512;
/// Zero-yield eviction steps tolerated across the db rotation before the
/// sweep concludes nothing is evictable (each step examines ≤ 256 slots).
const DRY_STEP_LIMIT: u32 = 2 * DEFAULT_DBS as u32;

/// Per-cell pressure configuration (pushed from the typed CONFIG store
/// within one MAINTAIN round — the M1-S03 `hot-per-cell` class).
#[derive(Copy, Clone, Debug, Default)]
pub struct PressureConfig {
    /// This cell's budget share in bytes; 0 = unlimited.
    pub limit_bytes: u64,
    pub policy: EvictionPolicy,
    /// Candidates examined per victim selection (`maxmemory-samples`).
    pub samples: u32,
}

/// Budget for one eviction MAINTAIN slice.
#[derive(Copy, Clone, Debug)]
pub struct EvictBudget {
    pub max_evictions: u32,
}

impl Default for EvictBudget {
    fn default() -> EvictBudget {
        EvictBudget { max_evictions: 64 }
    }
}

/// One cell's keyspace: default dbs + named-namespace registry + pressure.
pub struct Keyspace {
    dbs: [Option<Box<CellStore>>; DEFAULT_DBS],
    cfg: StoreConfig,
    named: NsRegistry,
    pressure: PressureConfig,
    /// Cached `used > limit` (the M1-S07 one-branch write-path flag).
    over_limit: bool,
    /// Eviction rotation cursor across populated dbs.
    hand_db: usize,
}

impl Keyspace {
    /// `cfg.evict_seed` seeds the per-db eviction streams (vary it per cell
    /// — L7: all randomness is injected).
    pub fn new(cfg: StoreConfig) -> Keyspace {
        let mut ks = Keyspace {
            dbs: Default::default(),
            cfg,
            named: NsRegistry::default(),
            pressure: PressureConfig::default(),
            over_limit: false,
            hand_db: 0,
        };
        // db0 is eager: it serves every connection that never SELECTs.
        let _ = ks.db_mut(0);
        ks
    }

    /// The store behind database `db`, materializing it on first touch.
    ///
    /// # Panics
    /// Panics if `db >= 16` — SELECT validates the range upstream.
    pub fn db_mut(&mut self, db: usize) -> &mut CellStore {
        assert!(db < DEFAULT_DBS, "db index validated at the command layer");
        if self.dbs[db].is_none() {
            let mut cfg = self.cfg;
            // Distinct per-db streams from one injected seed.
            cfg.evict_seed = self.cfg.evict_seed ^ (db as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
            let mut store = Box::new(CellStore::new(cfg));
            store.set_eviction_policy(self.pressure.policy);
            self.dbs[db] = Some(store);
        }
        self.dbs[db].as_mut().expect("materialized above")
    }

    /// Read-only view of `db` when it has been materialized.
    pub fn db(&self, db: usize) -> Option<&CellStore> {
        self.dbs.get(db).and_then(|s| s.as_deref())
    }

    /// Materialized databases, in index order.
    pub fn dbs(&self) -> impl Iterator<Item = (usize, &CellStore)> {
        self.dbs.iter().enumerate().filter_map(|(i, s)| s.as_deref().map(|s| (i, s)))
    }

    // ---- aggregation (M1-S09: per-ns numbers must reconcile with totals) ----

    /// Aggregated memory attribution: the exact field-wise sum of every
    /// materialized db's report (the reconciliation AC checks this).
    pub fn report(&self) -> MemoryReport {
        let mut total = MemoryReport {
            records_live_bytes: 0,
            records_slack_bytes: 0,
            records_resident_bytes: 0,
            index_bytes: 0,
            wheel_bytes: 0,
            evict_bytes: 0,
            live_records: 0,
        };
        for (_, store) in self.dbs() {
            let r = store.report();
            total.records_live_bytes += r.records_live_bytes;
            total.records_slack_bytes += r.records_slack_bytes;
            total.records_resident_bytes += r.records_resident_bytes;
            total.index_bytes += r.index_bytes;
            total.wheel_bytes += r.wheel_bytes;
            total.evict_bytes += r.evict_bytes;
            total.live_records += r.live_records;
        }
        total
    }

    /// Aggregated lifetime counters across dbs.
    pub fn stats(&self) -> StoreStats {
        let mut total = StoreStats::default();
        for (_, store) in self.dbs() {
            let s = store.stats();
            total.keyspace_hits += s.keyspace_hits;
            total.keyspace_misses += s.keyspace_misses;
            total.expired_lazy += s.expired_lazy;
            total.expired_active += s.expired_active;
            total.ttl_live += s.ttl_live;
            total.wheel_stale += s.wheel_stale;
            total.wheel_fallback += s.wheel_fallback;
            total.evicted_keys += s.evicted_keys;
        }
        total
    }

    /// `CONFIG RESETSTAT` across every db.
    pub fn reset_stats(&mut self) {
        for store in self.dbs.iter_mut().flatten() {
            store.reset_stats();
        }
    }

    /// `FLUSHALL` (this cell's slice): every database.
    pub fn flush_all(&mut self, now: Nanos) {
        for store in self.dbs.iter_mut().flatten() {
            store.flush(now);
        }
        self.refresh_pressure();
    }

    /// One budgeted expiry MAINTAIN slice across every materialized db
    /// (M1-S05 over M1-S08): the fire/step budget is shared — later dbs see
    /// what earlier dbs left, so a storm in one db cannot multiply the
    /// slice by the db count. `lag_ms` reports the worst db (it drives the
    /// plane's debt escalation).
    pub fn expire_tick(&mut self, now: Nanos, budget: ExpiryBudget) -> ExpiryStats {
        let mut total = ExpiryStats::default();
        let mut left = budget;
        for store in self.dbs.iter_mut().flatten() {
            if left.max_fires == 0 || left.max_steps == 0 {
                break;
            }
            let s = store.expire_tick(now, left);
            let consumed = (s.reaped + s.stale).min(u64::from(u32::MAX)) as u32;
            left.max_fires = left.max_fires.saturating_sub(consumed);
            left.max_steps = left.max_steps.saturating_sub(s.steps);
            total.reaped += s.reaped;
            total.stale += s.stale;
            total.steps += s.steps;
            total.lag_ms = total.lag_ms.max(s.lag_ms);
            total.armed += s.armed;
        }
        if total.reaped > 0 {
            self.refresh_pressure();
        }
        total
    }

    // ---- pressure (M1-S07) ----

    /// Applies pressure config (per-cell share) and pushes the policy into
    /// every materialized db (tracking mode + CMS lifecycle).
    pub fn set_pressure(&mut self, pressure: PressureConfig) {
        self.pressure = pressure;
        for store in self.dbs.iter_mut().flatten() {
            store.set_eviction_policy(pressure.policy);
        }
        self.refresh_pressure();
    }

    #[inline]
    pub fn pressure(&self) -> PressureConfig {
        self.pressure
    }

    /// The cached write-path flag: one branch, no summation (M1-S07).
    #[inline]
    pub fn over_limit(&self) -> bool {
        self.over_limit
    }

    /// Logical used bytes across dbs (the `maxmemory` comparable).
    pub fn used_bytes(&self) -> u64 {
        self.dbs().map(|(_, s)| s.used_bytes()).sum()
    }

    /// Recomputes the cached pressure flag. Called after mutations (cheap:
    /// a few loads per materialized db; short-circuits when no limit).
    #[inline]
    pub fn refresh_pressure(&mut self) {
        self.over_limit =
            self.pressure.limit_bytes != 0 && self.used_bytes() > self.pressure.limit_bytes;
    }

    /// The write-path OOM gate (M1-S07): callers reach this only for
    /// DENYOOM commands when `over_limit` is already set. `noeviction`
    /// answers OOM immediately; eviction policies escalate inline — free
    /// bounded victims now, re-check, and only then issue the honest OOM.
    pub fn free_for_write(&mut self, now: Nanos) -> Result<(), OpError> {
        if !self.over_limit {
            return Ok(());
        }
        if self.pressure.policy == EvictionPolicy::NoEviction {
            return Err(OpError::OutOfMemory);
        }
        self.evict_toward(self.pressure.limit_bytes, INLINE_MAX_EVICTIONS, now);
        self.refresh_pressure();
        if self.over_limit { Err(OpError::OutOfMemory) } else { Ok(()) }
    }

    /// One eviction MAINTAIN slice: drive usage to the low watermark
    /// (`limit − limit/16`) under `budget`, plus periodic CMS decay.
    /// Proactive — CONFIG SET maxmemory shows observable effect within one
    /// MAINTAIN round even with no writes arriving (M1-S03 AC).
    pub fn evict_tick(&mut self, now: Nanos, budget: EvictBudget) -> EvictStats {
        for store in self.dbs.iter_mut().flatten() {
            store.evict_maintain(now);
        }
        let limit = self.pressure.limit_bytes;
        if limit == 0 || self.pressure.policy == EvictionPolicy::NoEviction {
            self.refresh_pressure();
            return EvictStats::default();
        }
        let low_watermark = limit - limit / 16;
        let stats = self.evict_toward(low_watermark, budget.max_evictions, now);
        self.refresh_pressure();
        stats
    }

    /// Bounded eviction loop: rotate the hand across materialized dbs,
    /// evicting one victim per step, until usage reaches `target`, the
    /// eviction budget is spent, or a full dry rotation proves nothing
    /// qualifies (sparse windows get [`DRY_STEP_LIMIT`] chances).
    fn evict_toward(&mut self, target: u64, max_evictions: u32, now: Nanos) -> EvictStats {
        let mut stats = EvictStats::default();
        let (samples, policy) = (self.pressure.samples, self.pressure.policy);
        if policy == EvictionPolicy::NoEviction {
            return stats;
        }
        let mut evicted = 0u32;
        let mut dry_steps = 0u32;
        while self.used_bytes() > target && evicted < max_evictions && dry_steps < DRY_STEP_LIMIT {
            // Rotate to the next materialized db without spending dry
            // budget on the holes (db0 always exists, so this terminates) —
            // only real sweep attempts may conclude "nothing evictable".
            while self.dbs[self.hand_db].is_none() {
                self.hand_db = (self.hand_db + 1) % DEFAULT_DBS;
            }
            let db = self.hand_db;
            let step = match self.dbs[db].as_mut() {
                Some(store) if !store.is_empty() => store.evict_step(samples, now),
                _ => EvictStats::default(),
            };
            if step.evicted == 0 && step.freed_bytes == 0 {
                dry_steps += 1;
                self.hand_db = (self.hand_db + 1) % DEFAULT_DBS;
            } else {
                dry_steps = 0;
                evicted += step.evicted as u32;
            }
            stats.absorb(step);
        }
        stats
    }

    // ---- cross-db ops (M1-S08) ----

    /// `COPY src dst DB n` across databases: value, TTL, and encoding move
    /// exactly like the single-db copy. Same-db calls delegate.
    pub fn copy_between(
        &mut self,
        src_db: usize,
        src: &[u8],
        dst_db: usize,
        dst: &[u8],
        replace: bool,
        now: Nanos,
    ) -> Result<crate::store::CopyResult, OpError> {
        if src_db == dst_db {
            return self.db_mut(src_db).copy(src, dst, replace, now);
        }
        let Some(rec) = self.db_mut(src_db).copy_out(src, now) else {
            return Ok(crate::store::CopyResult::SourceMissing);
        };
        self.db_mut(dst_db).copy_in(dst, &rec, replace, now)
    }

    // ---- named namespaces (M1-S08) ----

    pub fn ns_create(&mut self, spec: NsSpec) -> Result<(), NsError> {
        self.named.create(spec)
    }

    pub fn ns_drop(&mut self, name: &[u8]) -> Result<(), NsError> {
        self.named.drop_ns(name)
    }

    pub fn ns_get(&self, name: &[u8]) -> Option<&NsSpec> {
        self.named.get(name)
    }

    pub fn ns_iter(&self) -> impl Iterator<Item = &NsSpec> {
        self.named.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{SetOptions, StoreConfig};

    fn now() -> Nanos {
        Nanos(1_000_000)
    }

    #[test]
    fn dbs_materialize_lazily_and_isolate() {
        let mut ks = Keyspace::new(StoreConfig::default());
        assert_eq!(ks.dbs().count(), 1, "db0 eager, others lazy");
        ks.db_mut(3).set(b"k", b"three", SetOptions::default(), now()).expect("set");
        ks.db_mut(0).set(b"k", b"zero", SetOptions::default(), now()).expect("set");
        assert_eq!(ks.db_mut(3).get(b"k", now()), Some(&b"three"[..]));
        assert_eq!(ks.db_mut(0).get(b"k", now()), Some(&b"zero"[..]));
        assert_eq!(ks.db_mut(5).get(b"k", now()), None, "fresh db is empty");
        assert_eq!(ks.dbs().count(), 3);
    }

    #[test]
    fn flush_scopes_per_db_and_flush_all_clears() {
        let mut ks = Keyspace::new(StoreConfig::default());
        ks.db_mut(0).set(b"a", b"1", SetOptions::default(), now()).expect("set");
        ks.db_mut(1).set(b"a", b"2", SetOptions::default(), now()).expect("set");
        ks.db_mut(1).flush(now());
        assert_eq!(ks.db_mut(0).len(), 1, "FLUSHDB on db1 must not touch db0");
        assert_eq!(ks.db_mut(1).len(), 0);
        ks.flush_all(now());
        assert_eq!(ks.db_mut(0).len(), 0);
    }

    #[test]
    fn report_reconciles_with_per_db_sums() {
        let mut ks = Keyspace::new(StoreConfig::default());
        for db in [0usize, 2, 7] {
            for i in 0..50 {
                let key = format!("k:{i}");
                ks.db_mut(db).set(key.as_bytes(), b"v", SetOptions::default(), now()).expect("set");
            }
        }
        let total = ks.report();
        let by_hand: u64 = ks.dbs().map(|(_, s)| s.report().records_live_bytes).sum();
        assert_eq!(total.records_live_bytes, by_hand);
        assert_eq!(total.live_records, 150);
        let used_by_hand: u64 = ks.dbs().map(|(_, s)| s.used_bytes()).sum();
        assert_eq!(ks.used_bytes(), used_by_hand);
    }

    #[test]
    fn noeviction_returns_oom_and_del_recovers() {
        let mut ks = Keyspace::new(StoreConfig::default());
        for i in 0..100 {
            let key = format!("fill:{i}");
            ks.db_mut(0)
                .set(key.as_bytes(), &[0u8; 256], SetOptions::default(), now())
                .expect("set");
        }
        let used = ks.used_bytes();
        ks.set_pressure(PressureConfig {
            limit_bytes: used - 1,
            policy: EvictionPolicy::NoEviction,
            samples: 5,
        });
        assert!(ks.over_limit());
        assert_eq!(ks.free_for_write(now()), Err(OpError::OutOfMemory));
        // Freeing memory clears pressure without eviction.
        for i in 0..100 {
            let key = format!("fill:{i}");
            ks.db_mut(0).del(key.as_bytes(), now());
        }
        ks.refresh_pressure();
        assert!(!ks.over_limit());
        assert_eq!(ks.free_for_write(now()), Ok(()));
    }

    #[test]
    fn allkeys_eviction_frees_below_limit() {
        let mut ks = Keyspace::new(StoreConfig::default());
        for i in 0..500 {
            let key = format!("fill:{i}");
            ks.db_mut(0)
                .set(key.as_bytes(), &[0u8; 200], SetOptions::default(), now())
                .expect("set");
        }
        let limit = ks.used_bytes() * 3 / 4;
        ks.set_pressure(PressureConfig {
            limit_bytes: limit,
            policy: EvictionPolicy::AllKeysRandom,
            samples: 5,
        });
        assert!(ks.over_limit());
        assert_eq!(ks.free_for_write(now()), Ok(()), "eviction must clear pressure");
        assert!(ks.used_bytes() <= limit, "used {} > limit {limit}", ks.used_bytes());
        assert!(ks.stats().evicted_keys > 0);
    }

    #[test]
    fn volatile_policy_with_no_ttl_keys_is_an_honest_oom() {
        let mut ks = Keyspace::new(StoreConfig::default());
        for i in 0..200 {
            let key = format!("fill:{i}");
            ks.db_mut(0)
                .set(key.as_bytes(), &[0u8; 200], SetOptions::default(), now())
                .expect("set");
        }
        ks.set_pressure(PressureConfig {
            limit_bytes: ks.used_bytes() / 2,
            policy: EvictionPolicy::VolatileLru,
            samples: 5,
        });
        assert_eq!(
            ks.free_for_write(now()),
            Err(OpError::OutOfMemory),
            "nothing volatile ⇒ OOM, never an allkeys fallback"
        );
        assert_eq!(ks.stats().evicted_keys, 0, "non-volatile records must survive");
    }

    #[test]
    fn maintain_tick_drives_to_low_watermark_after_config_shrink() {
        let mut ks = Keyspace::new(StoreConfig::default());
        for i in 0..500 {
            let key = format!("fill:{i}");
            ks.db_mut(0)
                .set(key.as_bytes(), &[0u8; 200], SetOptions::default(), now())
                .expect("set");
        }
        let limit = ks.used_bytes() / 2;
        ks.set_pressure(PressureConfig {
            limit_bytes: limit,
            policy: EvictionPolicy::AllKeysLru,
            samples: 5,
        });
        // No writes arrive; MAINTAIN alone must surface the new budget.
        let mut slices = 0;
        while ks.over_limit() && slices < 1_000 {
            ks.evict_tick(now(), EvictBudget::default());
            slices += 1;
        }
        assert!(ks.used_bytes() <= limit, "maintain must reach the budget");
        assert!(
            ks.used_bytes() <= limit - limit / 16 + 256,
            "and settle near the low watermark (hysteresis)"
        );
    }
}
