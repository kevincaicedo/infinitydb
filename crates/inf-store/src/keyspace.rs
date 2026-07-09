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

use inf_foundation::hash64;
use inf_foundation::time::Nanos;
use inf_log::{FsyncClass, NsId, RecordView as LogRecordView};

use crate::catalog::NsCatalog;
use crate::evict::{EvictStats, EvictionPolicy};
use crate::ns::{FIRST_NAMED_NS_ID, NsError, NsMode, NsRegistry, NsSpec};
use crate::store::{CellStore, ExpiryStats, MemoryReport, OpError, StoreConfig, StoreStats};
use crate::wall::WallAnchor;
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
    /// Named-namespace stores, materialized on first touch (M2-S08).
    /// Linear scan by id — a node has few named namespaces, and the id is
    /// resolved once per command, not per key.
    named_stores: Vec<(NsId, Box<CellStore>)>,
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
            named_stores: Vec::new(),
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
        for store in self.all_stores() {
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
        for store in self.all_stores() {
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

    /// `CONFIG RESETSTAT` across every db and named store.
    pub fn reset_stats(&mut self) {
        for store in self.dbs.iter_mut().flatten() {
            store.reset_stats();
        }
        for (_, store) in &mut self.named_stores {
            store.reset_stats();
        }
    }

    /// `FLUSHALL` (this cell's slice): every default database plus named
    /// *memory* namespaces. Durable stores are skipped by construction — a
    /// durable flush needs per-key `Delete` records (S10/S11 territory) and
    /// the command layer refuses it first (ADR-0015 deviations); the skip
    /// here is defense in depth, never the primary gate.
    pub fn flush_all(&mut self, now: Nanos) {
        for store in self.dbs.iter_mut().flatten() {
            store.flush(now);
        }
        for (id, store) in &mut self.named_stores {
            if self.named.get_by_id(*id).is_none_or(|s| s.mode == NsMode::Memory) {
                store.flush(now);
            }
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
        let named = self.named_stores.iter_mut().map(|(_, s)| s.as_mut());
        for store in self.dbs.iter_mut().flatten().map(Box::as_mut).chain(named) {
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

    /// Logical used bytes across dbs and named stores (the `maxmemory`
    /// comparable). Named stores count toward pressure but never join the
    /// eviction hand (M2-S08 — see `ns_store_mut`), so sustained pressure
    /// from a named namespace resolves as honest OOM refusals, not silent
    /// eviction of durable data.
    pub fn used_bytes(&self) -> u64 {
        let named: u64 = self.named_stores.iter().map(|(_, s)| s.used_bytes()).sum();
        self.dbs().map(|(_, s)| s.used_bytes()).sum::<u64>() + named
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

    // ---- named namespaces (M1-S08 registry; M2-S08 stores + catalog) ----

    pub fn ns_create(&mut self, spec: NsSpec) -> Result<(), NsError> {
        self.named.create(spec)
    }

    /// Drops the registry entry **and** its store (with all its data). The
    /// id is never reused; log records naming it are skipped on replay.
    pub fn ns_drop(&mut self, name: &[u8]) -> Result<(), NsError> {
        let id = self.named.get(name).map(|s| s.id);
        self.named.drop_ns(name)?;
        if let Some(id) = id {
            self.named_stores.retain(|(nid, _)| *nid != id);
            self.refresh_pressure();
        }
        Ok(())
    }

    pub fn ns_get(&self, name: &[u8]) -> Option<&NsSpec> {
        self.named.get(name)
    }

    pub fn ns_get_by_id(&self, id: NsId) -> Option<&NsSpec> {
        self.named.get_by_id(id)
    }

    pub fn ns_iter(&self) -> impl Iterator<Item = &NsSpec> {
        self.named.iter()
    }

    /// The store behind named namespace `id`, materializing it on first
    /// touch; `None` when the id isn't registered (unknown or dropped).
    ///
    /// Named stores never join the eviction hand regardless of the server
    /// policy: durable namespaces must not evict (ADR-0015 D5 — eviction
    /// without `Delete` records resurrects keys on replay), and per-ns
    /// eviction for named *memory* namespaces is a recorded M2 limitation
    /// (their `EVICTION`/`MAXMEMORY` config is honored as registry state,
    /// enforced post-M2).
    pub fn ns_store_mut(&mut self, id: NsId) -> Option<&mut CellStore> {
        self.named.get_by_id(id)?;
        if let Some(i) = self.named_stores.iter().position(|(nid, _)| *nid == id) {
            return Some(self.named_stores[i].1.as_mut());
        }
        let mut cfg = self.cfg;
        cfg.evict_seed = self.cfg.evict_seed ^ u64::from(id.0).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        let mut store = Box::new(CellStore::new(cfg));
        store.set_eviction_policy(EvictionPolicy::NoEviction);
        self.named_stores.push((id, store));
        Some(self.named_stores.last_mut().expect("pushed above").1.as_mut())
    }

    /// Read-only view of named namespace `id` when materialized.
    pub fn ns_store(&self, id: NsId) -> Option<&CellStore> {
        self.named_stores.iter().find(|(nid, _)| *nid == id).map(|(_, s)| s.as_ref())
    }

    /// The durability class of namespace `id`: `None` for memory (default
    /// dbs and memory-mode named), `Some` for durable — the one branch the
    /// mutation path pays (L2's degenerate case stays free, M2-S09).
    pub fn ns_fsync_class(&self, id: NsId) -> Option<FsyncClass> {
        if id.0 < FIRST_NAMED_NS_ID {
            return None;
        }
        let spec = self.named.get_by_id(id)?;
        if spec.mode == NsMode::Durable { spec.fsync } else { None }
    }

    /// Ascending ids of durable namespaces — the checkpoint walk order
    /// (M2-S10, ADR-0016 D2; memory namespaces have a null log and null
    /// checkpoint coverage).
    #[must_use]
    pub fn durable_ns_ids(&self) -> Vec<u32> {
        let mut ids: Vec<u32> =
            self.ns_iter().filter(|s| s.mode == NsMode::Durable).map(|s| s.id.0).collect();
        ids.sort_unstable();
        ids
    }

    /// Boot-time catalog seed (ADR-0015 D3): replaces the registry with the
    /// persisted entries. Runs before the cell replays or serves; any
    /// existing named stores are dropped with their data.
    ///
    /// # Errors
    /// The first registry-rule violation — a failing catalog is a
    /// fail-stop at boot, never a partial seed.
    pub fn seed_catalog(&mut self, cat: &NsCatalog) -> Result<(), NsError> {
        self.named = NsRegistry::default();
        self.named_stores.clear();
        for spec in &cat.entries {
            self.named.create(spec.clone())?;
        }
        Ok(())
    }

    /// Snapshot the registry as a catalog (the DDL persist path; the caller
    /// owns `next_id` — it lives on the node-level allocator).
    pub fn export_catalog(&self, next_id: u32) -> NsCatalog {
        NsCatalog { next_id, entries: self.ns_iter().cloned().collect() }
    }

    // ---- replay (M2-S08, ADR-0015 D7) ----

    /// Applies one decoded log record — the blind idempotent upsert of
    /// ADR-0011 D4. Records naming an unregistered id (dropped namespace)
    /// or a reserved one are skipped and counted, never an error: the
    /// catalog is authoritative and foreign logs must not wedge recovery.
    ///
    /// `ExpireAt` deadlines convert from record Unix-ms through `anchor`;
    /// a deadline too far in the future for the internal clock clamps to
    /// the maximum representable (never to `now` — clamping forward-lost
    /// deadlines to the past would expire keys that should live).
    ///
    /// # Errors
    /// Arena/bounds failures from the store (recovery fail-stop).
    pub fn apply_record(
        &mut self,
        rec: &LogRecordView<'_>,
        now: Nanos,
        anchor: WallAnchor,
    ) -> Result<ReplayOutcome, OpError> {
        match *rec {
            LogRecordView::StringPostImage { ns, key, value } => {
                let Some(store) = self.replay_store(ns) else {
                    return Ok(ReplayOutcome::SkippedUnknownNs);
                };
                store.replay_set(key, value, now)?;
                Ok(ReplayOutcome::Applied)
            }
            LogRecordView::Delete { ns, key } => {
                let Some(store) = self.replay_store(ns) else {
                    return Ok(ReplayOutcome::SkippedUnknownNs);
                };
                store.replay_del(key, now);
                Ok(ReplayOutcome::Applied)
            }
            LogRecordView::ExpireAt { ns, at_unix_ms, key } => {
                let Some(store) = self.replay_store(ns) else {
                    return Ok(ReplayOutcome::SkippedUnknownNs);
                };
                let at = anchor.internal_from_unix(at_unix_ms).unwrap_or(Nanos(u64::MAX));
                store.replay_expire_at(key, at, now);
                Ok(ReplayOutcome::Applied)
            }
            // Reserved in M2: the catalog is META-owned; no NsOp records
            // are emitted (ADR-0015 D7).
            LogRecordView::NsOp { .. } => Ok(ReplayOutcome::SkippedReserved),
            // Checkpoint marker (M2-S10, ADR-0016 D3): carries no state —
            // S13's recovery orchestration consumes its LSN, not replay.
            LogRecordView::CkptBegin { .. } => Ok(ReplayOutcome::SkippedMarker),
        }
    }

    fn replay_store(&mut self, ns: NsId) -> Option<&mut CellStore> {
        // Defaults never log (memory namespaces have a null log — L2);
        // an id below the named floor in a real log is foreign data.
        if ns.0 < FIRST_NAMED_NS_ID {
            return None;
        }
        self.ns_store_mut(ns)
    }

    /// M2-S13: presize a namespace's store from the `.ick` footer's entry
    /// count before recovery streams the checkpoint (see
    /// [`CellStore::reserve_keys`]). Unknown ids are ignored — a foreign
    /// count must not materialize state the catalog doesn't know.
    pub fn reserve_ns(&mut self, ns: NsId, entries: u64) {
        if ns.0 < FIRST_NAMED_NS_ID {
            return;
        }
        if let Some(store) = self.ns_store_mut(ns) {
            store.reserve_keys(usize::try_from(entries).unwrap_or(usize::MAX));
        }
    }

    // ---- state digest (M2-S13, ADR-0018) ----

    /// Order-independent digest of the cell's live logical state: every
    /// non-expired entry's `(store identity, key, value, expire_at)`
    /// hashed and folded commutatively, so physically different layouts
    /// (arena order, index geometry, materialization order) of the same
    /// logical state digest identically. The recovery determinism oracle:
    /// recovering the same files twice — or recovery vs a reference
    /// full-log replay — must produce equal digests **under the same
    /// injected `now` and wall anchor** (expiry deadlines and cutoffs are
    /// part of the state; L7 injects the clock that interprets them).
    ///
    /// Empty stores contribute nothing: a materialized-but-empty db
    /// digests the same as one never touched.
    pub fn state_digest(&self, now: Nanos) -> StateDigest {
        let mut acc = StateDigest::default();
        for (db, store) in self.dbs() {
            fold_store(&mut acc, store, db as u64, now);
        }
        for (ns, store) in &self.named_stores {
            fold_store(&mut acc, store, NAMED_TAG | u64::from(ns.0), now);
        }
        acc
    }

    /// Every materialized store: default dbs, then named (aggregation
    /// order is stable but unspecified).
    fn all_stores(&self) -> impl Iterator<Item = &CellStore> {
        self.dbs
            .iter()
            .filter_map(|s| s.as_deref())
            .chain(self.named_stores.iter().map(|(_, s)| s.as_ref()))
    }
}

/// A deterministic, layout-independent summary of live logical state
/// (M2-S13). Two keyspaces holding the same entries under the same
/// injected clock compare equal, however they were built.
#[derive(Copy, Clone, Default, PartialEq, Eq, Debug)]
pub struct StateDigest {
    /// Live (non-expired) entries covered.
    pub entries: u64,
    /// Commutative fold of per-entry hashes.
    pub digest: u64,
}

/// Distinguishes named-namespace store tags from default-db indices in
/// the digest (named ids and db indices share small integers).
const NAMED_TAG: u64 = 1 << 32;

const DIGEST_SEED: u64 = 0xD16E_57A7_E5EE_D001;

/// Fold one store's live entries into the digest. Per entry the fields
/// chain (each hash seeds the next, binding key↔value↔expiry↔store), and
/// entries combine by wrapping addition — a multiset hash, insensitive to
/// walk order.
fn fold_store(acc: &mut StateDigest, store: &CellStore, tag: u64, now: Nanos) {
    let mut cursor = 0u64;
    loop {
        cursor = store.digest_post_images(cursor, 1024, now, |key, value, expire_at_ms| {
            let hk = hash64(key, DIGEST_SEED ^ tag);
            let hv = hash64(value, hk);
            let mut exp = [0u8; 9];
            if let Some(ms) = expire_at_ms {
                exp[0] = 1;
                exp[1..].copy_from_slice(&ms.to_le_bytes());
            }
            acc.digest = acc.digest.wrapping_add(hash64(&exp, hv));
            acc.entries += 1;
        });
        if cursor == 0 {
            break;
        }
    }
}

/// What [`Keyspace::apply_record`] did with one record.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum ReplayOutcome {
    Applied,
    /// The record names an id the catalog doesn't know (dropped namespace,
    /// or a reserved default id) — skipped and counted by the caller.
    SkippedUnknownNs,
    /// A record type M2 never emits (`NsOp`) — reserved, skipped.
    SkippedReserved,
    /// A checkpoint-begin marker (M2-S10): expected in every log with
    /// checkpoints, counted separately from foreign skips.
    SkippedMarker,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{SetExpire, SetOptions, StoreConfig};

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

    // ---- M2-S08: named stores, catalog, replay ----

    fn durable_spec(id: u32, name: &[u8]) -> NsSpec {
        NsSpec {
            id: NsId(id),
            name: name.to_vec(),
            mode: NsMode::Durable,
            fsync: Some(FsyncClass::Always),
            policy: None,
            maxmemory: None,
        }
    }

    #[test]
    fn named_stores_are_isolated_and_dropped_with_their_namespace() {
        let mut ks = Keyspace::new(StoreConfig::default());
        let now = Nanos(1);
        ks.ns_create(durable_spec(16, b"ledger")).expect("create");
        ks.db_mut(0).set(b"k", b"db0", SetOptions::default(), now).expect("set db0");
        ks.ns_store_mut(NsId(16))
            .expect("registered")
            .set(b"k", b"ledger", SetOptions::default(), now)
            .expect("set named");
        assert_eq!(ks.db_mut(0).get(b"k", now), Some(&b"db0"[..]));
        assert_eq!(ks.ns_store_mut(NsId(16)).expect("live").get(b"k", now), Some(&b"ledger"[..]));
        assert_eq!(ks.ns_fsync_class(NsId(16)), Some(FsyncClass::Always));
        assert_eq!(ks.ns_fsync_class(NsId(0)), None, "defaults are memory");
        ks.ns_drop(b"ledger").expect("drop");
        assert!(ks.ns_store_mut(NsId(16)).is_none(), "dropped ids resolve to no store");
    }

    #[test]
    fn catalog_seed_and_export_round_trip() {
        let mut ks = Keyspace::new(StoreConfig::default());
        ks.ns_create(durable_spec(16, b"ledger")).expect("create");
        let cat = ks.export_catalog(17);
        let mut fresh = Keyspace::new(StoreConfig::default());
        fresh.seed_catalog(&cat).expect("seed");
        assert_eq!(fresh.export_catalog(17), cat);
        assert!(fresh.ns_store_mut(NsId(16)).is_some());
    }

    #[test]
    fn apply_record_is_a_blind_idempotent_upsert() {
        let mut ks = Keyspace::new(StoreConfig::default());
        let now = Nanos::from_millis(10);
        let anchor = WallAnchor { internal_ms: 0, unix_ms: 1_750_000_000_000 };
        ks.ns_create(durable_spec(16, b"ledger")).expect("create");

        let set = LogRecordView::StringPostImage { ns: NsId(16), key: b"k", value: b"v1" };
        for _ in 0..2 {
            // Double apply: replay from an older checkpoint re-covers records.
            assert_eq!(ks.apply_record(&set, now, anchor).expect("apply"), ReplayOutcome::Applied);
        }
        assert_eq!(ks.ns_store_mut(NsId(16)).expect("live").get(b"k", now), Some(&b"v1"[..]));

        // ExpireAt in the future keeps the key; a past deadline kills it.
        let future_unix = anchor.unix_from_internal(Nanos::from_millis(60_000));
        let exp = LogRecordView::ExpireAt { ns: NsId(16), at_unix_ms: future_unix, key: b"k" };
        assert_eq!(ks.apply_record(&exp, now, anchor).expect("apply"), ReplayOutcome::Applied);
        assert_eq!(ks.ns_store_mut(NsId(16)).expect("live").get(b"k", now), Some(&b"v1"[..]));
        assert_eq!(
            ks.ns_store_mut(NsId(16)).expect("live").get(b"k", Nanos::from_millis(61_000)),
            None,
            "replayed deadline fires"
        );

        let del = LogRecordView::Delete { ns: NsId(16), key: b"gone" };
        assert_eq!(ks.apply_record(&del, now, anchor).expect("apply"), ReplayOutcome::Applied);

        // Unknown / reserved ids skip, never error (dropped-ns tolerance).
        let foreign = LogRecordView::StringPostImage { ns: NsId(99), key: b"x", value: b"y" };
        assert_eq!(
            ks.apply_record(&foreign, now, anchor).expect("apply"),
            ReplayOutcome::SkippedUnknownNs
        );
        let reserved = LogRecordView::StringPostImage { ns: NsId(3), key: b"x", value: b"y" };
        assert_eq!(
            ks.apply_record(&reserved, now, anchor).expect("apply"),
            ReplayOutcome::SkippedUnknownNs
        );
        let nsop = LogRecordView::NsOp { ns: NsId(16), payload: b"reserved" };
        assert_eq!(
            ks.apply_record(&nsop, now, anchor).expect("apply"),
            ReplayOutcome::SkippedReserved
        );
    }

    #[test]
    fn post_image_reads_without_touching_stats() {
        let mut ks = Keyspace::new(StoreConfig::default());
        let now = Nanos(1);
        ks.ns_create(durable_spec(16, b"ledger")).expect("create");
        let store = ks.ns_store_mut(NsId(16)).expect("live");
        store
            .set(
                b"k",
                b"v",
                SetOptions {
                    expire: SetExpire::At(Nanos::from_millis(5_000)),
                    ..SetOptions::default()
                },
                now,
            )
            .expect("set");
        let hits_before = store.stats().keyspace_hits;
        let img = store.post_image(b"k", now).expect("live key");
        assert_eq!(img.value, b"v");
        assert_eq!(img.expire_at_ms, Some(5_000));
        assert_eq!(store.stats().keyspace_hits, hits_before, "no read-stat side effect");
        assert!(
            store.post_image(b"k", Nanos::from_millis(6_000)).is_none(),
            "expired reads absent"
        );
        assert!(store.post_image(b"missing", now).is_none());
    }

    // ---- state digest (M2-S13) ----

    #[test]
    fn digest_is_insertion_order_independent() {
        let now = Nanos(1);
        let mut a = Keyspace::new(StoreConfig::default());
        let mut b = Keyspace::new(StoreConfig::default());
        for i in 0..500 {
            let key = format!("k:{i}");
            let val = format!("v:{i}");
            a.db_mut(0).set(key.as_bytes(), val.as_bytes(), SetOptions::default(), now).unwrap();
            let j = 499 - i;
            let key = format!("k:{j}");
            let val = format!("v:{j}");
            b.db_mut(0).set(key.as_bytes(), val.as_bytes(), SetOptions::default(), now).unwrap();
        }
        assert_eq!(a.state_digest(now), b.state_digest(now));
        assert_eq!(a.state_digest(now).entries, 500);
    }

    #[test]
    fn digest_is_layout_independent() {
        // Same final logical state via different histories: overwrites,
        // deletes, and a different db-materialization order.
        let now = Nanos(1);
        let mut a = Keyspace::new(StoreConfig::default());
        a.db_mut(2).set(b"x", b"seen", SetOptions::default(), now).unwrap();
        a.db_mut(0).set(b"k", b"old", SetOptions::default(), now).unwrap();
        a.db_mut(0).set(b"k", b"final", SetOptions::default(), now).unwrap();
        a.db_mut(0).set(b"dead", b"gone", SetOptions::default(), now).unwrap();
        a.db_mut(0).del(b"dead", now);

        let mut b = Keyspace::new(StoreConfig::default());
        b.db_mut(0).set(b"k", b"final", SetOptions::default(), now).unwrap();
        b.db_mut(2).set(b"x", b"seen", SetOptions::default(), now).unwrap();
        // A materialized-but-empty db must not perturb the digest.
        let _ = b.db_mut(7);

        assert_eq!(a.state_digest(now), b.state_digest(now));
    }

    #[test]
    fn digest_distinguishes_value_ttl_db_and_namespace() {
        let now = Nanos(1);
        let base = |value: &[u8], expire: SetExpire, db: usize| {
            let mut ks = Keyspace::new(StoreConfig::default());
            ks.db_mut(db)
                .set(b"k", value, SetOptions { expire, ..SetOptions::default() }, now)
                .unwrap();
            ks.state_digest(now)
        };
        let reference = base(b"v", SetExpire::Clear, 0);
        assert_ne!(reference, base(b"w", SetExpire::Clear, 0), "value must bind");
        assert_ne!(
            reference,
            base(b"v", SetExpire::At(Nanos::from_millis(5_000)), 0),
            "expiry must bind"
        );
        assert_ne!(reference, base(b"v", SetExpire::Clear, 1), "db identity must bind");

        let mut named = Keyspace::new(StoreConfig::default());
        named.ns_create(durable_spec(16, b"ledger")).expect("create");
        named.ns_store_mut(NsId(16)).unwrap().set(b"k", b"v", SetOptions::default(), now).unwrap();
        assert_ne!(reference, named.state_digest(now), "namespace identity must bind");
        assert_eq!(named.state_digest(now).entries, 1);
    }

    #[test]
    fn digest_excludes_expired_entries_without_reaping() {
        let now = Nanos(1);
        let mut ks = Keyspace::new(StoreConfig::default());
        ks.db_mut(0).set(b"live", b"v", SetOptions::default(), now).unwrap();
        ks.db_mut(0)
            .set(
                b"dying",
                b"v",
                SetOptions { expire: SetExpire::At(Nanos::from_millis(5)), ..Default::default() },
                now,
            )
            .unwrap();

        let mut only_live = Keyspace::new(StoreConfig::default());
        only_live.db_mut(0).set(b"live", b"v", SetOptions::default(), now).unwrap();

        let after = Nanos::from_millis(10);
        assert_eq!(ks.state_digest(after), only_live.state_digest(after));
        // Read-only: the digest walk reaped nothing (the record is still
        // physically resident, just logically dead).
        assert_eq!(ks.db_mut(0).len(), 2, "digest must not reap");
        // Before the deadline both entries count.
        assert_eq!(ks.state_digest(now).entries, 2);
    }
}
