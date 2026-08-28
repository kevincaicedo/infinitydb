//! `Keyspace` — one cell's slice of every namespace (M1-E3/E4): the 16
//! default Redis databases (`SELECT 0..15`) as lazily-materialized
//! [`CellStore`]s, the named-namespace registry, and the memory-pressure
//! driver that turns `maxmemory`/`maxmemory-policy` into bounded eviction.
//!
//! ## Pressure model (M1-S07; per-namespace legs M4-S27, ADR-0068)
//!
//! `maxmemory` is the node-wide budget (Redis semantics); the server layer
//! hands each cell `maxmemory / cells` — cells are symmetric by contiguous
//! slot ranges, so per-cell division preserves the global bound without any
//! shared state (L1). The default databases share the budget exactly like
//! Redis databases share the instance budget. Named **memory** namespaces
//! are enforced per ADR-0068: a store without its own `MAXMEMORY` inherits
//! the node policy and joins the global eviction hand; a store with its own
//! budget carries a cached per-store `over_limit` flag and reclaims through
//! its own step-limited pass — never through the global hand, so a
//! namespace at its budget cannot disturb the numbered dbs (structural
//! isolation, not tuning). Durable named namespaces stay hard-`NoEviction`
//! (ADR-0015 D5 — eviction without `Delete` records resurrects keys on
//! replay); tiered namespaces refuse the knobs outright (ADR-0062 owns
//! their budget — one authority per namespace, never two).
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
use inf_foundation::{KeyHasher, hash64};
use inf_log::{FsyncClass, NsId, RecordView as LogRecordView};

use inf_foundation::LogicalAddr;

use crate::address_space::{AddressSpaceConfig, TieringCounters};
use crate::catalog::{IndexCatalog, NsCatalog};
use crate::demote::{DemoteStats, DemotionConfig, EvictionPressure};
use crate::evict::{EvictStats, EvictionPolicy};
use crate::index_backfill::{BackfillInfo, BackfillJob, BackfillTickStats};
use crate::index_registry::{IndexError, IndexId, IndexRegistry, IndexSpec, IndexState};
use crate::ns::{FIRST_NAMED_NS_ID, NsError, NsMode, NsRegistry, NsSpec};
use crate::record::ExtentRef;
use crate::store::{
    CellStore, CheckpointImage, ExpiryStats, MemoryReport, OpError, StoreConfig, StoreStats,
};
use crate::tiered::TieredTable;
use crate::tiered::promote::PromotionCounters;
use crate::tiered::shadow::ShadowCounters;
use crate::wall::WallAnchor;
use crate::wheel::ExpiryBudget;
use crate::write_accounting::{WriteAccountingTotals, WriteAmpSummary};

/// Redis default database count (`SELECT 0..15`; CONFIG `databases`).
pub const DEFAULT_DBS: usize = 16;

/// Victims one blocked write may evict inline before the OOM verdict
/// (eviction-vs-write races resolve by escalation, bounded — M1-S07).
/// Steady-state pressure needs ~1 victim per write; the headroom covers
/// bursts. A budget shrink larger than this answers OOM transiently until
/// the MAINTAIN slice drains to the watermark — the bounded-everything
/// trade (Redis evicts unboundedly inline; recorded deviation).
const INLINE_MAX_EVICTIONS: u32 = 512;
/// Zero-yield eviction steps tolerated per rotation member before the
/// global sweep concludes nothing is evictable (each step examines ≤ 256
/// slots). The effective limit scales with the rotation set — ADR-0068 D2:
/// `2 × (DEFAULT_DBS + eligible named stores)`.
const DRY_STEPS_PER_MEMBER: u32 = 2;
/// Zero-yield steps tolerated by one namespace's own budget pass before it
/// concludes nothing qualifies this slice (8 × 256 = 2 Ki slots examined).
/// The store's hand persists across passes, so successive MAINTAIN slices
/// cover the whole table even when each pass gives up early.
const NS_DRY_STEP_LIMIT: u32 = 8;
/// Replay displacement-register bound (ADR-0059 D9): one displacing
/// mutation stages at most `RELOC_ORIGIN_CAP + 1` markers, so a longer
/// run inside one pairing is corrupt input, not load.
const DISPLACE_REGISTER_CAP: usize = 4;

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

/// Per-cell default for the aggregate reserved-Region-VA admission bound
/// (M4-S19, ADR-0062 D4 — the ADR-0051 accepted debt, retired): an
/// explicit bounded default, never an inferred host maximum. The node
/// key `tiered-reserved-va-limit` divides by cell count exactly like
/// `maxmemory` (cells are symmetric), and the CONFIG sweep pushes the
/// share via [`Keyspace::set_tiered_va_limit`].
pub const TIERED_VA_LIMIT_DEFAULT: u64 = 256 << 30;

/// Why a tiered-table materialization was refused (M4-S19, ADR-0062 D4).
/// Refusal mutates nothing: no `Region` is constructed, no entry is left
/// behind — the S01 refusal contract at DDL scale.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TieredCreateError {
    /// A table for this namespace already exists on the cell.
    Exists,
    /// The ring reservation or budget window is unrepresentable
    /// (`TieredTable::new` refused — nonsense configuration).
    Unrepresentable,
    /// The aggregate reserved-VA admission bound would be exceeded —
    /// checked **before** any mmap.
    VaLimitExceeded { requested_bytes: u64, admitted_bytes: u64, limit_bytes: u64 },
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

/// One materialized named-namespace store with its per-namespace pressure
/// state (M4-S27, ADR-0068 D2 budget leg).
struct NamedStore {
    id: NsId,
    store: Box<CellStore>,
    /// This cell's share of the spec's `MAXMEMORY` (`maxmemory /
    /// budget_shares`, floored at 1 so a sub-cell-count budget stays a
    /// budget); 0 = no per-namespace budget (the inheritance leg).
    budget_share: u64,
    /// Cached `used > budget_share` — the same one-branch write-path
    /// pattern as the global `over_limit`, recomputed at the same touch
    /// points ([`Keyspace::refresh_pressure`]).
    over_limit: bool,
}

/// One cell's keyspace: default dbs + named-namespace registry + pressure.
pub struct Keyspace {
    dbs: [Option<Box<CellStore>>; DEFAULT_DBS],
    cfg: StoreConfig,
    named: NsRegistry,
    /// Named-namespace stores, materialized on first touch (M2-S08).
    /// Linear scan by id — a node has few named namespaces, and the id is
    /// resolved once per command, not per key.
    named_stores: Vec<NamedStore>,
    /// Durable-tiered record tables (M4-S02 landing site; the S04 steel
    /// thread materializes the first entry). Empty on memory/durable-arena
    /// nodes — which is exactly what the S03 degenerate-case zero-counter
    /// assertion observes through [`tiering_counters`](Self::tiering_counters).
    tiered_stores: Vec<(NsId, Box<TieredTable>)>,
    pressure: PressureConfig,
    /// Divisor for per-namespace `MAXMEMORY` shares (the node's cell
    /// count, pushed with the CONFIG sweep — same symmetric-cells
    /// argument as `maxmemory / cells`; 1 on planeless/embedded tiers).
    budget_shares: u64,
    /// Cached `used > limit` (the M1-S07 one-branch write-path flag).
    over_limit: bool,
    /// Eviction rotation cursor across the global hand's rotation set:
    /// `0..DEFAULT_DBS` are the numbered dbs, `DEFAULT_DBS..` index into
    /// `named_stores` (ADR-0068 D2 inheritance leg).
    hand_db: usize,
    /// This cell's share of the node's reserved-VA admission bound
    /// (M4-S19, ADR-0062 D4). Admission-only: lowering it under standing
    /// reservations refuses new creations, never evicts.
    tiered_va_limit_bytes: u64,
    /// Read-driven promotion admission (M4.5-S30, ADR-0085 D6): the
    /// `tiered-promote-on-read` CONFIG key's cell-local value, applied
    /// to every standing and future tiered table.
    tier_promote: bool,
    /// Shadow-slot admission (M4.5-S37, ADR-0093 D8): the
    /// `tiered-shadow-overwrite` CONFIG key's cell-local value — the
    /// A/B arm, default off.
    tier_shadow: bool,
    /// `tiered-shadow-reconcile` (ADR-0093 A8): `false` pauses every
    /// table's reconciler; tickets stay open, bounded by their caps.
    tier_shadow_reconcile: bool,
    /// Replay displacement register (ADR-0057 D4, widened to a bounded
    /// list by ADR-0059 D9): `ColdDisplace` markers park here until the
    /// paired mutation record — the very next record, same namespace —
    /// drains them. Non-empty at end-of-log is a decode error the
    /// recovery driver checks via [`displace_register_len`](Self::displace_register_len).
    pending_displace: Vec<(NsId, u64)>,
    /// Per-cell index registry (M4.5-S03, ADR-0075/ADR-0072 D2):
    /// declarations replicated by the DDL fan, this cell's trees and
    /// machine states beside them. Consulted at DDL/MAINTAIN rate only —
    /// the mutation path reads a cached flag (S04).
    indexes: IndexRegistry,
    /// Per-cell backfill jobs (M4.5-S05, ADR-0077): volatile by design —
    /// nothing here is durable or recovery-load-bearing; boot re-derives
    /// jobs from the seeded registry (D2: crash ⇒ restart the walk).
    pub(crate) backfill: Vec<BackfillJob>,
    /// Cumulative walk totals for INFO (per boot, like the S04 `idx_*`
    /// counter lines).
    backfill_docs_total: u64,
    backfill_inserted_total: u64,
    /// This boot's sidecar load fold (M4.5-S06, ADR-0078 D6) — written
    /// once by the loader's commit, rendered by `INFO stats`.
    sidecar_info: crate::index_sidecar::SidecarBootInfo,
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
            tiered_stores: Vec::new(),
            pressure: PressureConfig::default(),
            budget_shares: 1,
            over_limit: false,
            hand_db: 0,
            tiered_va_limit_bytes: TIERED_VA_LIMIT_DEFAULT,
            tier_promote: true,
            tier_shadow: false,
            tier_shadow_reconcile: true,
            pending_displace: Vec::new(),
            indexes: IndexRegistry::default(),
            backfill: Vec::new(),
            backfill_docs_total: 0,
            backfill_inserted_total: 0,
            sidecar_info: Default::default(),
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
            // Attach-block sync point (ADR-0076 D1): declarations that
            // predate materialization install their trees now.
            #[cfg(feature = "doc")]
            install_ns_attaches(&self.indexes, NsId(db as u32), &mut store);
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
            doc_tape_bytes: 0,
            doc_arena_bytes: 0,
            doc_resident_bytes: 0,
            doc_intern_bytes: 0,
            doc_slack_bytes: 0,
            doc_scratch_bytes: 0,
            doc_path_cache_bytes: 0,
            idx_tree_bytes: 0,
            idx_slack_bytes: 0,
            live_records: 0,
            docs_live: 0,
        };
        for store in self.all_stores() {
            let r = store.report();
            total.records_live_bytes += r.records_live_bytes;
            total.records_slack_bytes += r.records_slack_bytes;
            total.records_resident_bytes += r.records_resident_bytes;
            total.index_bytes += r.index_bytes;
            total.wheel_bytes += r.wheel_bytes;
            total.evict_bytes += r.evict_bytes;
            total.doc_tape_bytes += r.doc_tape_bytes;
            total.doc_arena_bytes += r.doc_arena_bytes;
            total.doc_resident_bytes += r.doc_resident_bytes;
            total.doc_intern_bytes += r.doc_intern_bytes;
            total.doc_slack_bytes += r.doc_slack_bytes;
            total.doc_scratch_bytes += r.doc_scratch_bytes;
            total.doc_path_cache_bytes += r.doc_path_cache_bytes;
            // Index trees live in the owning store's attach block since
            // ADR-0076 D1 — the fold rides the per-store reports.
            total.idx_tree_bytes += r.idx_tree_bytes;
            total.idx_slack_bytes += r.idx_slack_bytes;
            total.live_records += r.live_records;
            total.docs_live += r.docs_live;
        }
        total
    }

    /// Number of durable-tiered tables on this cell (0 until the M4-S04
    /// steel thread materializes one).
    pub fn tiered_tables(&self) -> usize {
        self.tiered_stores.len()
    }

    /// Materializes the tiered record table for namespace `ns` (M4-S04 —
    /// the first `tiered_stores` entry; M4-S07 adds the demotion
    /// configuration; M4-S19 adds the aggregate reserved-VA admission
    /// bound, checked **before** any mmap — ADR-0062 D4). Command
    /// routing to tiered tables remains the standing wiring obligation;
    /// the flush/demotion drivers and the harnesses reach the table
    /// through [`tiered_store_mut`](Self::tiered_store_mut).
    ///
    /// # Errors
    /// [`TieredCreateError`] — refusal mutates nothing.
    pub fn materialize_tiered(
        &mut self,
        ns: NsId,
        config: AddressSpaceConfig,
        demote: DemotionConfig,
        initial_keys: usize,
    ) -> Result<(), TieredCreateError> {
        if self.tiered_stores.iter().any(|(nid, _)| *nid == ns) {
            return Err(TieredCreateError::Exists);
        }
        let requested_bytes = config.reserve_bytes as u64;
        let admitted_bytes = self.tiering_usage().reserved_bytes;
        let limit_bytes = self.tiered_va_limit_bytes;
        // Checked arithmetic, refused before the Region exists: the
        // reservation is the VA truth this bound counts, so the check
        // and the mmap can never disagree.
        if admitted_bytes.checked_add(requested_bytes).is_none_or(|total| total > limit_bytes) {
            return Err(TieredCreateError::VaLimitExceeded {
                requested_bytes,
                admitted_bytes,
                limit_bytes,
            });
        }
        let mut table = TieredTable::new(config, demote, initial_keys, self.cfg.hasher)
            .ok_or(TieredCreateError::Unrepresentable)?;
        table.set_promote_enabled(self.tier_promote);
        table.set_shadow_enabled(self.tier_shadow);
        table.set_shadow_reconcile(self.tier_shadow_reconcile);
        self.tiered_stores.push((ns, Box::new(table)));
        Ok(())
    }

    /// Materializes a tiered table from a registered spec's tier block
    /// (M4-S19): derives the ring from the spec's budget + slice, applies
    /// every derived config, and enforces the D4 admission bound. Fresh
    /// life at origin zero — recovery-time re-materialization supplies
    /// its own origin through the recovery path.
    ///
    /// # Errors
    /// [`TieredCreateError`] — refusal mutates nothing.
    pub fn materialize_tiered_spec(
        &mut self,
        ns: NsId,
        tier: &crate::ns::TierSpec,
    ) -> Result<(), TieredCreateError> {
        let demote = tier.demotion_config();
        let reserve_bytes =
            demote.ring_reserve_bytes().ok_or(TieredCreateError::Unrepresentable)?;
        let config = AddressSpaceConfig {
            reserve_bytes,
            page_bytes: inf_alloc::REGION_PAGE_BYTES,
            life_origin: LogicalAddr::ZERO,
        };
        // Index presize: tables grow; a fixed hint keeps creation O(1).
        self.materialize_tiered(ns, config, demote, 1024)?;
        let table = self.tiered_store_mut(ns).expect("materialized above");
        table.set_compaction_config(tier.compaction_config());
        table.set_blob_config(tier.blob_config());
        table.set_disk_budget(tier.disk_budget_bytes);
        Ok(())
    }

    /// Replaces a namespace's fresh-at-origin-zero table with the
    /// recovered one (M4-S26; ADR-0057 D6 step 2 — `seed_catalog`
    /// materializes fresh, boot recovery swaps the recovered life in
    /// before any checkpoint entry or tail record applies). The spec's
    /// derived knobs re-apply on the recovered table.
    ///
    /// # Panics
    /// Panics when the namespace is not a registered tiered namespace —
    /// the recovery driver only recovers manifested tiered sections.
    pub fn install_recovered_tiered(&mut self, ns: NsId, table: TieredTable) {
        let tier = self
            .ns_get_by_id(ns)
            .and_then(|spec| spec.tier)
            .expect("recovered namespace carries a tier block");
        let entry = self
            .tiered_stores
            .iter_mut()
            .find(|(id, _)| *id == ns)
            .expect("seed_catalog materialized the namespace");
        *entry.1 = table;
        entry.1.set_compaction_config(tier.compaction_config());
        entry.1.set_blob_config(tier.blob_config());
        entry.1.set_disk_budget(tier.disk_budget_bytes);
        entry.1.set_promote_enabled(self.tier_promote);
        entry.1.set_shadow_enabled(self.tier_shadow);
        entry.1.set_shadow_reconcile(self.tier_shadow_reconcile);
    }

    /// This cell's share of the node reserved-VA limit (ADR-0062 D4).
    #[must_use]
    pub fn tiered_va_limit(&self) -> u64 {
        self.tiered_va_limit_bytes
    }

    /// Pushes the cell's VA-limit share (the CONFIG sweep — Hot class,
    /// admission-only: standing reservations are never evicted).
    pub fn set_tiered_va_limit(&mut self, bytes: u64) {
        self.tiered_va_limit_bytes = bytes;
    }

    /// Pushes the read-driven-promotion admission flag (M4.5-S30,
    /// ADR-0085 D6 — the `tiered-promote-on-read` CONFIG sweep) to every
    /// standing tiered table; future tables inherit it at materialize/
    /// install time.
    pub fn set_tier_promote(&mut self, on: bool) {
        self.tier_promote = on;
        for (_, table) in &mut self.tiered_stores {
            table.set_promote_enabled(on);
        }
    }

    /// Pushes the shadow-slot admission flag (M4.5-S37, ADR-0093 D8 —
    /// the `tiered-shadow-overwrite` CONFIG sweep) to every standing
    /// tiered table; future tables inherit it. Off orphans nothing:
    /// open tickets keep reconciling.
    pub fn set_tier_shadow(&mut self, on: bool) {
        self.tier_shadow = on;
        for (_, table) in &mut self.tiered_stores {
            table.set_shadow_enabled(on);
        }
    }

    /// Pushes the reconciler pause (M4.5-S37, ADR-0093 A8 — the
    /// `tiered-shadow-reconcile` CONFIG sweep; `false` = paused) to every
    /// standing tiered table; future tables inherit it.
    pub fn set_tier_shadow_reconcile(&mut self, on: bool) {
        self.tier_shadow_reconcile = on;
        for (_, table) in &mut self.tiered_stores {
            table.set_shadow_reconcile(on);
        }
    }

    /// Aggregated shadow-slot counters across every tiered table on
    /// this cell (M4.5-S37, ADR-0093 D8) — identically zero on a
    /// memory-mode node (the §3.3 zero contract).
    pub fn tiering_shadow(&self) -> ShadowCounters {
        let mut total = ShadowCounters::default();
        for (_, table) in &self.tiered_stores {
            total.add(table.shadow_counters());
        }
        total
    }

    /// Aggregated read-promotion counters across every tiered table on
    /// this cell (M4.5-S30, ADR-0085 D6) — identically zero on a
    /// memory-mode node (the §3.3 zero contract).
    pub fn tiering_promotion(&self) -> PromotionCounters {
        let mut total = PromotionCounters::default();
        for (_, table) in &self.tiered_stores {
            total.add(table.promotion_counters());
        }
        total
    }

    /// EvictionPressure v2 (M4-S07, §3.2): how namespace `ns` answers
    /// memory pressure. Table-granular, never per-op — cache namespaces
    /// keep the M1 eviction path instruction-identical (ADR-0053 D5).
    pub fn pressure_response(&self, ns: NsId) -> EvictionPressure {
        if self.tiered_stores.iter().any(|(nid, _)| *nid == ns) {
            EvictionPressure::Demote
        } else {
            EvictionPressure::Evict
        }
    }

    /// One demotion MAINTAIN round (M4-S07, ADR-0053): per tiered table,
    /// one seal step toward the mutable-fraction target and one release
    /// step below the flushed watermark — each bounded by the table's
    /// `slice_bytes`. The flush leg between them (`flushed` advancement
    /// after fdatasync) is the S11 pipeline's; its confirmation call
    /// sites are `advance_flushed` on each table's space (ADR-0053 D6).
    /// On a node with no tiered namespaces this iterates an empty Vec —
    /// the degenerate case executes nothing and counts nothing (S03).
    pub fn demote_tick(&mut self) -> DemoteStats {
        let mut stats = DemoteStats::default();
        for (_, table) in &mut self.tiered_stores {
            let sealed = table.seal_slice();
            let released = table.release_slice();
            if sealed > 0 || released > 0 {
                stats.sealed_bytes += sealed;
                stats.released_bytes += released;
                stats.tables_active += 1;
            }
        }
        stats
    }

    /// Aggregated tiered-table memory attribution (L5): reserved and
    /// committed ring bytes, live/dead record bytes, and index bytes
    /// across every tiered table on this cell. All-zero on memory-mode
    /// nodes (no table exists — the S03 degenerate case).
    pub fn tiering_usage(&self) -> TieredUsage {
        let mut usage = TieredUsage::default();
        for (_, table) in &self.tiered_stores {
            let report = table.space().report();
            usage.reserved_bytes += report.reserved_bytes;
            usage.committed_bytes += report.committed_bytes;
            usage.allocated_bytes += report.allocated_bytes;
            usage.dead_bytes += report.dead_bytes;
            usage.live_bytes += table.live_bytes();
            usage.index_bytes += table.index_bytes();
        }
        usage
    }

    /// Aggregated write-path byte counters across this cell's tiered
    /// namespaces (M4-S13): the `INFO tiering` totals. Exactly the
    /// field-wise sum of the per-namespace lines rendered beside it, so
    /// the two can never disagree — and identically zero on a
    /// memory-mode node, where no `TieredTable` exists to hold them.
    ///
    /// Node-wide write amplification is deliberately **not** derivable
    /// from this value: blending namespaces hides a runaway tiered
    /// namespace behind a quiet one, so the return type carries totals
    /// only and [`tiering_write_amp`](Self::tiering_write_amp) reports the
    /// worst namespace instead (M4-S16, ADR-0060 D4).
    pub fn tiering_write_accounting(&self) -> WriteAccountingTotals {
        let mut totals = WriteAccountingTotals::default();
        for (_, table) in &self.tiered_stores {
            totals.add(table.write_accounting());
        }
        totals
    }

    /// This cell's write-amplification summary (M4-S16): the worst
    /// per-namespace ratio and how many namespaces have no denominator.
    /// Zero on a memory-mode node for the same structural reason the
    /// counters are — there is no tiered namespace to ask.
    pub fn tiering_write_amp(&self) -> WriteAmpSummary {
        let mut summary = WriteAmpSummary::default();
        for (_, table) in &self.tiered_stores {
            summary.add(table.write_accounting().write_amplification());
        }
        summary
    }

    /// This cell's blob write-amplification summary (M4-S18, ADR-0061
    /// D8): the worst per-namespace `blob_bytes / blob_user_bytes` ratio
    /// and how many namespaces wrote extent bytes without a blob
    /// denominator. The same worst-not-blend rule as
    /// [`tiering_write_amp`](Self::tiering_write_amp), on the disjoint
    /// device leg — and the same structural zero on memory-mode nodes.
    pub fn tiering_blob_write_amp(&self) -> WriteAmpSummary {
        let mut summary = WriteAmpSummary::default();
        for (_, table) in &self.tiered_stores {
            summary.add(table.write_accounting().blob_write_amplification());
        }
        summary
    }

    /// This cell's tiered namespaces in materialization order — the
    /// per-namespace `INFO tiering` lines (watermarks + write counters)
    /// and any future per-namespace reporting walk this.
    pub fn tiered_namespaces(&self) -> impl Iterator<Item = (NsId, &TieredTable)> {
        self.tiered_stores.iter().map(|(ns, table)| (*ns, table.as_ref()))
    }

    /// The tiered table for namespace `ns`, if materialized.
    pub fn tiered_store_mut(&mut self, ns: NsId) -> Option<&mut TieredTable> {
        let i = self.tiered_stores.iter().position(|(nid, _)| *nid == ns)?;
        Some(self.tiered_stores[i].1.as_mut())
    }

    /// Aggregated tiering code-path counters across every tiered table on
    /// this cell (M4-S03): identically zero unless tiering code executed —
    /// the §3.3 "provably unexecuted" rule as a scrapeable fact, asserted
    /// by the degenerate-case A/B report and cache-profile CI runs.
    pub fn tiering_counters(&self) -> TieringCounters {
        let mut total = TieringCounters::default();
        for (_, table) in &self.tiered_stores {
            let counters = table.space().counters();
            total.tail_allocs += counters.tail_allocs;
            total.seal_holes += counters.seal_holes;
            total.seal_hole_bytes += counters.seal_hole_bytes;
            total.region_commit_pages += counters.region_commit_pages;
            total.region_decommit_pages += counters.region_decommit_pages;
            total.cold_resolves += counters.cold_resolves;
            total.tail_alloc_stalls += counters.tail_alloc_stalls;
            total.demote_slices += counters.demote_slices;
            total.demote_sealed_bytes += counters.demote_sealed_bytes;
            total.flush_slices += counters.flush_slices;
            total.flush_confirmed_bytes += counters.flush_confirmed_bytes;
            total.compact_slices += counters.compact_slices;
        }
        total
    }

    /// Aggregated blob-extent observables across every tiered table on
    /// this cell (M4-S17, ADR-0061 D8) — identically zero on a
    /// memory-mode node (no table, no extents; the §3.3 zero contract).
    pub fn tiering_extent_stats(&self) -> crate::extents::ExtentStats {
        let mut total = crate::extents::ExtentStats::default();
        for (_, table) in &self.tiered_stores {
            let stats = table.extent_stats();
            total.live += stats.live;
            total.live_bytes += stats.live_bytes;
            total.created += stats.created;
            total.reclaimed += stats.reclaimed;
            total.reclaimable += stats.reclaimable;
            total.reclaim_slices += stats.reclaim_slices;
            total.reclaim_deferred += stats.reclaim_deferred;
            total.rmw_ops += stats.rmw_ops;
            total.disk_bytes += stats.disk_bytes;
        }
        total
    }

    /// Aggregated disk-admission observables across every tiered table
    /// on this cell (M4-S21, ADR-0063 D5) — identically zero on a
    /// memory-mode node (the §3.3 zero contract).
    pub fn tiering_disk_admission(&self) -> DiskAdmissionTotals {
        let mut total = DiskAdmissionTotals::default();
        for (_, table) in &self.tiered_stores {
            if table.disk_full().is_some() {
                total.full_namespaces += 1;
            }
            total.refusals += table.diskfull_refusals();
            total.compact_idle_pressure += table.compact_idle_pressure();
            total.used_bytes += table.disk_admission_used();
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
            total.index_grows += s.index_grows;
        }
        total
    }

    /// `CONFIG RESETSTAT` across every db and named store.
    pub fn reset_stats(&mut self) {
        for store in self.dbs.iter_mut().flatten() {
            store.reset_stats();
        }
        for entry in &mut self.named_stores {
            entry.store.reset_stats();
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
        for entry in &mut self.named_stores {
            if self.named.get_by_id(entry.id).is_none_or(|s| s.mode == NsMode::Memory) {
                entry.store.flush(now);
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
        let named = self.named_stores.iter_mut().map(|e| e.store.as_mut());
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
    /// every materialized db (tracking mode + CMS lifecycle) and into every
    /// **inheriting** named memory store — one with its own `EVICTION`
    /// keeps it: explicit beats inherited (ADR-0068 D3).
    pub fn set_pressure(&mut self, pressure: PressureConfig) {
        self.pressure = pressure;
        for store in self.dbs.iter_mut().flatten() {
            store.set_eviction_policy(pressure.policy);
        }
        for i in 0..self.named_stores.len() {
            let spec = self.named.get_by_id(self.named_stores[i].id);
            if spec.is_some_and(|s| s.mode == NsMode::Memory && s.policy.is_none()) {
                self.named_stores[i].store.set_eviction_policy(pressure.policy);
            }
        }
        self.refresh_pressure();
    }

    /// Pushes the per-namespace budget divisor (the node's cell count —
    /// the CONFIG sweep calls this beside [`set_pressure`](Self::set_pressure)).
    /// Every materialized store's cached share and flag recompute here, so
    /// a cell-count change can never leave a stale share behind.
    pub fn set_budget_shares(&mut self, shares: u64) {
        self.budget_shares = shares.max(1);
        for i in 0..self.named_stores.len() {
            let spec = self.named.get_by_id(self.named_stores[i].id);
            self.named_stores[i].budget_share = match spec {
                Some(s) if s.mode == NsMode::Memory => {
                    ns_budget_share(s.maxmemory, self.budget_shares)
                }
                _ => 0,
            };
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

    /// Logical used bytes across dbs, named stores, and index trees (the
    /// `maxmemory` comparable). Every named store counts toward the
    /// global flag — L5 attribution truth (ADR-0068 D5) — but reclaim
    /// authority differs: budget-less memory stores join the global hand,
    /// budgeted ones own their per-namespace pass, and durable/tiered
    /// stores never evict, so their sustained pressure resolves as honest
    /// OOM refusals. Index trees count too (ADR-0075 D6: an unattributed
    /// byte is a lie) — eviction shrinks them back through the S04
    /// removal hook, never directly.
    pub fn used_bytes(&self) -> u64 {
        let named: u64 = self.named_stores.iter().map(|e| e.store.used_bytes()).sum();
        let idx: u64 = self.all_stores().map(|s| s.idx_memory().idx_tree_bytes).sum();
        self.dbs().map(|(_, s)| s.used_bytes()).sum::<u64>() + named + idx
    }

    /// Recomputes the cached pressure flags — global and per-namespace
    /// (ADR-0068 D5: the per-ns flags cache at the same touch points).
    /// Called after mutations (cheap: a few loads per materialized store;
    /// the global half short-circuits when no limit is set). Per-ns
    /// comparisons include the namespace's index-tree bytes (ADR-0075
    /// D6 — index growth tightens the document budget).
    #[inline]
    pub fn refresh_pressure(&mut self) {
        self.over_limit =
            self.pressure.limit_bytes != 0 && self.used_bytes() > self.pressure.limit_bytes;
        for i in 0..self.named_stores.len() {
            let entry = &self.named_stores[i];
            if entry.budget_share == 0 {
                self.named_stores[i].over_limit = false;
                continue;
            }
            let idx = entry.store.idx_memory().idx_tree_bytes;
            let used = self.named_stores[i].store.used_bytes() + idx;
            self.named_stores[i].over_limit = used > self.named_stores[i].budget_share;
        }
    }

    /// The write-path OOM gate (M1-S07): callers reach this only for
    /// DENYOOM commands when `over_limit` is already set. Unfreeable
    /// pressure answers OOM immediately (nothing in the rotation set
    /// evicts); otherwise the escalation is inline and bounded — free
    /// victims now, re-check, and only then issue the honest OOM.
    pub fn free_for_write(&mut self, now: Nanos) -> Result<(), OpError> {
        if !self.over_limit {
            return Ok(());
        }
        self.evict_toward(self.pressure.limit_bytes, INLINE_MAX_EVICTIONS, now);
        self.refresh_pressure();
        if self.over_limit { Err(OpError::OutOfMemory) } else { Ok(()) }
    }

    /// The namespace-scoped write-path OOM gate (M4-S27, ADR-0068 D4):
    /// `None` when `ns` is not a named memory namespace with its own
    /// `MAXMEMORY` — the caller falls back to the global gate (numbered
    /// dbs, inheriting memory stores, durable, tiered). Otherwise the
    /// namespace's own verdict: one branch on its cached flag, then the
    /// bounded inline pass over **its own keys only**, then honest OOM.
    pub fn ns_free_for_write(&mut self, ns: NsId, now: Nanos) -> Option<Result<(), OpError>> {
        let spec = self.named.get_by_id(ns)?;
        if spec.mode != NsMode::Memory || spec.maxmemory.is_none() {
            return None;
        }
        // Materialize so the first write to a budgeted namespace is gated
        // by its own budget, never the global flag (D4 scope).
        self.ns_store_mut(ns)?;
        let i = self.named_stores.iter().position(|e| e.id == ns).expect("materialized above");
        if !self.named_stores[i].over_limit {
            return Some(Ok(()));
        }
        // The store-side target leaves room for the namespace's index
        // trees (ADR-0075 D6: the flag compares store + idx vs share).
        let idx = self.named_stores[i].store.idx_memory().idx_tree_bytes;
        let target = self.named_stores[i].budget_share.saturating_sub(idx);
        self.evict_ns_toward(i, target, INLINE_MAX_EVICTIONS, now);
        self.refresh_pressure();
        Some(if self.named_stores[i].over_limit { Err(OpError::OutOfMemory) } else { Ok(()) })
    }

    /// The cached per-namespace budget flag (test/introspection surface;
    /// the write path goes through [`ns_free_for_write`](Self::ns_free_for_write)).
    #[must_use]
    pub fn ns_over_limit(&self, ns: NsId) -> bool {
        self.named_stores.iter().any(|e| e.id == ns && e.over_limit)
    }

    /// One eviction MAINTAIN slice: CMS decay everywhere it is armed, the
    /// global leg toward the node low watermark (`limit − limit/16`), then
    /// each budgeted named store toward its own low watermark under what
    /// remains of the slice budget (shared like the expiry slice — later
    /// stores see what earlier ones left, so one namespace cannot multiply
    /// the slice). Proactive — a config change shows observable effect
    /// within one MAINTAIN round even with no writes arriving (M1-S03 AC).
    pub fn evict_tick(&mut self, now: Nanos, budget: EvictBudget) -> EvictStats {
        for store in self.dbs.iter_mut().flatten() {
            store.evict_maintain(now);
        }
        for entry in &mut self.named_stores {
            entry.store.evict_maintain(now);
        }
        let mut stats = EvictStats::default();
        let mut left = budget.max_evictions;
        let limit = self.pressure.limit_bytes;
        if limit != 0 {
            let step = self.evict_toward(limit - limit / 16, left, now);
            left -= (step.evicted as u32).min(left);
            stats.absorb(step);
        }
        let mut i = 0;
        while i < self.named_stores.len() && left > 0 {
            let share = self.named_stores[i].budget_share;
            if share != 0 {
                // Store-side watermark: the share minus the namespace's
                // index-tree bytes (ADR-0075 D6 accounting).
                let idx = self.named_stores[i].store.idx_memory().idx_tree_bytes;
                let target = (share - share / 16).saturating_sub(idx);
                let step = self.evict_ns_toward(i, target, left, now);
                left -= (step.evicted as u32).min(left);
                stats.absorb(step);
            }
            i += 1;
        }
        self.refresh_pressure();
        stats
    }

    /// Bounded eviction loop over the global rotation set (ADR-0068 D2):
    /// the materialized dbs (under the node policy) plus every named
    /// memory store **without** its own budget whose effective policy
    /// evicts. One victim per step until usage reaches `target`, the
    /// budget is spent, or a dry rotation proves nothing qualifies —
    /// [`DRY_STEPS_PER_MEMBER`] chances per rotation member.
    fn evict_toward(&mut self, target: u64, max_evictions: u32, now: Nanos) -> EvictStats {
        let mut stats = EvictStats::default();
        let samples = self.pressure.samples;
        let node_evicts = self.pressure.policy != EvictionPolicy::NoEviction;
        let eligible_named = self.named_stores.iter().filter(|e| named_in_hand(e)).count() as u32;
        if !node_evicts && eligible_named == 0 {
            // Nothing in the rotation set may evict — the honest OOM path.
            return stats;
        }
        let dry_limit = DRY_STEPS_PER_MEMBER * (DEFAULT_DBS as u32 + eligible_named);
        let rotation = DEFAULT_DBS + self.named_stores.len();
        let mut evicted = 0u32;
        let mut dry_steps = 0u32;
        while self.used_bytes() > target && evicted < max_evictions && dry_steps < dry_limit {
            // Rotate to the next hand member without spending dry budget
            // on holes and out-of-hand stores (at least one member is
            // eligible — checked above — so this terminates); only real
            // sweep attempts may conclude "nothing evictable".
            self.hand_db %= rotation;
            while !self.hand_member(node_evicts, self.hand_db) {
                self.hand_db = (self.hand_db + 1) % rotation;
            }
            let at = self.hand_db;
            let step = if at < DEFAULT_DBS {
                match self.dbs[at].as_mut() {
                    Some(store) if !store.is_empty() => store.evict_step(samples, now),
                    _ => EvictStats::default(),
                }
            } else {
                let store = &mut self.named_stores[at - DEFAULT_DBS].store;
                if store.is_empty() {
                    EvictStats::default()
                } else {
                    store.evict_step(samples, now)
                }
            };
            if step.evicted == 0 && step.freed_bytes == 0 {
                dry_steps += 1;
                self.hand_db = (self.hand_db + 1) % rotation;
            } else {
                dry_steps = 0;
                evicted += step.evicted as u32;
            }
            stats.absorb(step);
        }
        stats
    }

    /// Whether rotation position `at` is in the global hand right now.
    fn hand_member(&self, node_evicts: bool, at: usize) -> bool {
        if at < DEFAULT_DBS {
            node_evicts && self.dbs[at].is_some()
        } else {
            named_in_hand(&self.named_stores[at - DEFAULT_DBS])
        }
    }

    /// One namespace's own budget pass (ADR-0068 D2): evict from **this
    /// store only** toward `target`, bounded by `max_evictions` and
    /// [`NS_DRY_STEP_LIMIT`]. Isolation is structural — no other store's
    /// keys and no global dry-step accounting are touched.
    fn evict_ns_toward(
        &mut self,
        i: usize,
        target: u64,
        max_evictions: u32,
        now: Nanos,
    ) -> EvictStats {
        let samples = self.pressure.samples;
        let entry = &mut self.named_stores[i];
        let mut stats = EvictStats::default();
        if entry.store.eviction_policy() == EvictionPolicy::NoEviction {
            return stats;
        }
        let mut evicted = 0u32;
        let mut dry_steps = 0u32;
        while entry.store.used_bytes() > target
            && evicted < max_evictions
            && dry_steps < NS_DRY_STEP_LIMIT
        {
            let step = entry.store.evict_step(samples, now);
            if step.evicted == 0 && step.freed_bytes == 0 {
                dry_steps += 1;
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
        // Cross-db COPY is excluded from the plane brackets (ADR-0076
        // D3): the destination store is only unambiguous here.
        let dst_store = self.db_mut(dst_db);
        #[cfg(feature = "doc")]
        {
            dst_store.idx_bracket_begin(&[dst], None).map_err(OpError::IndexMaintenance)?;
            let result = dst_store.copy_in(dst, &rec, replace, now);
            match &result {
                Ok(crate::store::CopyResult::Copied) => {
                    dst_store.idx_bracket_commit(&[dst], crate::index_maint::MaintMode::Strict);
                }
                _ => dst_store.idx_bracket_abort(),
            }
            result
        }
        #[cfg(not(feature = "doc"))]
        dst_store.copy_in(dst, &rec, replace, now)
    }

    // ---- named namespaces (M1-S08 registry; M2-S08 stores + catalog) ----

    /// Registers `spec` and, for a tiered spec (M4-S19), materializes the
    /// cell's `TieredTable` under the D4 admission bound. Registration
    /// and materialization succeed or fail together — an admission
    /// refusal rolls the registry entry back, leaving nothing behind.
    pub fn ns_create(&mut self, spec: NsSpec) -> Result<(), NsError> {
        let tier = spec.tier;
        let (id, name) = (spec.id, spec.name.clone());
        self.named.create(spec)?;
        if let Some(tier) = tier
            && let Err(e) = self.materialize_tiered_spec(id, &tier)
        {
            self.named.drop_ns(&name).expect("just created");
            return Err(match e {
                TieredCreateError::Exists => NsError::Exists,
                TieredCreateError::Unrepresentable => NsError::InvalidTierConfig(
                    "MEM-BUDGET + MAINTAIN-SLICE has no representable ring reservation",
                ),
                TieredCreateError::VaLimitExceeded {
                    requested_bytes,
                    admitted_bytes,
                    limit_bytes,
                } => NsError::TierVaLimitExceeded { requested_bytes, admitted_bytes, limit_bytes },
            });
        }
        Ok(())
    }

    /// Hot-reloads a memory namespace's pressure knobs (M4-S27, ADR-0068
    /// D3): the registry entry, the materialized store's policy, and the
    /// cached budget share update together. `policy = None` returns the
    /// store to inheriting the node policy; `maxmemory = None` removes the
    /// per-namespace budget (the store rejoins the global hand).
    ///
    /// # Errors
    /// Typed refusals for unknown, durable, and tiered namespaces (D1:
    /// durable never evicts; tiered budgets belong to ADR-0062).
    pub fn ns_set_memory(
        &mut self,
        name: &[u8],
        policy: Option<EvictionPolicy>,
        maxmemory: Option<u64>,
    ) -> Result<(), NsError> {
        self.named.set_memory_pressure(name, policy, maxmemory)?;
        let spec = self.named.get(name).expect("just updated");
        let (id, effective) = (spec.id, spec.policy.unwrap_or(self.pressure.policy));
        let share = ns_budget_share(spec.maxmemory, self.budget_shares);
        if let Some(entry) = self.named_stores.iter_mut().find(|e| e.id == id) {
            entry.store.set_eviction_policy(effective);
            entry.budget_share = share;
        }
        self.refresh_pressure();
        Ok(())
    }

    /// Drops the registry entry **and** its store (with all its data). The
    /// id is never reused; log records naming it are skipped on replay.
    /// A tiered namespace's table drops with it (M4-S19, ADR-0062 D7):
    /// the `Region` unmaps on drop, returning exactly its ring to the D4
    /// admitted sum — structurally, not by bookkeeping. The plane owns
    /// the file half of the teardown (tier + blob unlinks after pin
    /// drain — `inf-store` never does I/O, §3.3).
    pub fn ns_drop(&mut self, name: &[u8]) -> Result<(), NsError> {
        let id = self.named.get(name).map(|s| s.id);
        self.named.drop_ns(name)?;
        if let Some(id) = id {
            self.named_stores.retain(|e| e.id != id);
            self.tiered_stores.retain(|(nid, _)| *nid != id);
            // A namespace drop takes its index declarations and trees
            // with it (M4.5-S03) — their ids stay retired.
            self.indexes.remove_ns(id);
            self.refresh_pressure();
        }
        Ok(())
    }

    /// Hot-reloads a tiered namespace's spec (M4-S19, ADR-0062 D3): the
    /// registry entry and the materialized table update together, or
    /// neither does. CreateOnly keys are the command layer's refusal;
    /// this applies what it is given after re-running the range gauntlet
    /// and the ring bound.
    pub fn ns_set_tier(&mut self, name: &[u8], tier: crate::ns::TierSpec) -> Result<(), NsError> {
        tier.validate().map_err(NsError::InvalidTierConfig)?;
        let spec = self.named.get(name).ok_or(NsError::Unknown)?;
        if spec.tier.is_none() {
            return Err(NsError::NotTiered);
        }
        let id = spec.id;
        // Apply to the table first (it can refuse on the ring bound);
        // the registry write follows only on success.
        if let Some(table) = self.tiered_store_mut(id) {
            table.set_demotion(tier.demotion_config()).map_err(NsError::InvalidTierConfig)?;
            table.set_compaction_config(tier.compaction_config());
            table.set_blob_config(tier.blob_config());
            table.set_disk_budget(tier.disk_budget_bytes);
        }
        self.named.set_tier(name, tier)
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
    /// Materialization applies the spec's pressure semantics (M4-S27,
    /// ADR-0068): a **memory** store carries its own `EVICTION` (or
    /// inherits the node policy) and its `MAXMEMORY` share; durable stores
    /// — including tiered shells — stay pinned `NoEviction` (ADR-0015 D5:
    /// eviction without `Delete` records resurrects keys on replay). Only
    /// memory-mode stores can ever carry an evicting policy — the global
    /// hand's eligibility test stands on that invariant.
    pub fn ns_store_mut(&mut self, id: NsId) -> Option<&mut CellStore> {
        let spec = self.named.get_by_id(id)?;
        if let Some(i) = self.named_stores.iter().position(|e| e.id == id) {
            return Some(self.named_stores[i].store.as_mut());
        }
        let (policy, budget_share) = match spec.mode {
            NsMode::Memory => (
                spec.policy.unwrap_or(self.pressure.policy),
                ns_budget_share(spec.maxmemory, self.budget_shares),
            ),
            _ => (EvictionPolicy::NoEviction, 0),
        };
        let mut cfg = self.cfg;
        cfg.evict_seed = self.cfg.evict_seed ^ u64::from(id.0).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        let mut store = Box::new(CellStore::new(cfg));
        store.set_eviction_policy(policy);
        // Attach-block sync point (ADR-0076 D1): declarations that
        // predate materialization install their trees now.
        #[cfg(feature = "doc")]
        install_ns_attaches(&self.indexes, id, &mut store);
        self.named_stores.push(NamedStore { id, store, budget_share, over_limit: false });
        Some(self.named_stores.last_mut().expect("pushed above").store.as_mut())
    }

    /// Read-only view of named namespace `id` when materialized.
    pub fn ns_store(&self, id: NsId) -> Option<&CellStore> {
        self.named_stores.iter().find(|e| e.id == id).map(|e| e.store.as_ref())
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

    /// True when any durable namespace runs `FSYNC always` — the only
    /// consumer of write-through frames, and therefore of segment
    /// pre-zeroing (M4.5-S36, ADR-0088 D5 amended: an `everysec`-only
    /// cell never pays the zero-fill's second write).
    #[must_use]
    pub fn has_always_namespace(&self) -> bool {
        self.ns_iter().any(|s| s.mode == NsMode::Durable && s.fsync == Some(FsyncClass::Always))
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
    /// existing named stores are dropped with their data. Tiered entries
    /// (M4-S19) re-materialize their tables — fresh life at origin zero;
    /// the tiered recovery path (tier files, checkpoint restore) is the
    /// standing wiring obligation, and no live-node write can predate it
    /// (the D8 `USE` refusal keeps the data plane unreachable).
    ///
    /// Index declarations seed per ADR-0075 D4: `dropping` entries resume
    /// their drop (not seeded — removal persists with the next swap);
    /// every other entry survives with id/generation/program/type intact
    /// and regresses to `backfilling` (contents are projections and must
    /// rebuild before planning resumes), with the pre-crash-`ready` hint
    /// retained for S06's sidecar path. Generations never bump at boot
    /// (a bump would permanently stale every sidecar — ADR-0073 D5.1).
    ///
    /// # Errors
    /// The first registry-rule or admission violation — a failing catalog
    /// is a fail-stop at boot, never a partial seed.
    pub fn seed_catalog(&mut self, cat: &NsCatalog) -> Result<(), NsError> {
        self.named = NsRegistry::default();
        self.named_stores.clear();
        self.tiered_stores.clear();
        self.indexes = IndexRegistry::default();
        // Boot restarts every build (ADR-0077 D2): jobs re-derive from
        // the seeded registry at the first MAINTAIN tick.
        self.backfill.clear();
        for spec in &cat.entries {
            // `ns_create` materializes tiered entries under the D4 bound
            // — seeding and DDL share one path, so they cannot drift.
            self.ns_create(spec.clone())?;
        }
        for spec in &cat.index.entries {
            if spec.state == IndexState::Dropping {
                continue;
            }
            let was_ready = spec.state == IndexState::Ready;
            let seeded = IndexSpec { state: IndexState::Backfilling, ..spec.clone() };
            // Decode already ran every record rule (ADR-0075 D2.4), so a
            // refusal here is a violated invariant, not an operating error.
            self.indexes.create(seeded, was_ready).expect("catalog decode validated the record");
        }
        // Attach-block resync (ADR-0076 D1): default dbs survive a
        // re-seed materialized, so any stale attach is rebuilt from the
        // fresh registry (named stores were just cleared and rebuild
        // their attaches at materialization).
        #[cfg(feature = "doc")]
        {
            let Keyspace { dbs, indexes, .. } = self;
            for (db, store) in dbs.iter_mut().enumerate() {
                if let Some(store) = store.as_deref_mut() {
                    store.idx = crate::index_maint::CellIndexes::default();
                    install_ns_attaches(indexes, NsId(db as u32), store);
                }
            }
        }
        self.refresh_pressure();
        Ok(())
    }

    /// Snapshot the registry as a catalog (the DDL persist path; the
    /// caller owns all three counters — they live on the node-level
    /// allocator and never regress).
    pub fn export_catalog(
        &self,
        next_id: u32,
        next_index_id: u32,
        next_index_generation: u64,
    ) -> NsCatalog {
        NsCatalog {
            next_id,
            entries: self.ns_iter().cloned().collect(),
            index: IndexCatalog {
                next_id: next_index_id,
                next_generation: next_index_generation,
                entries: self.indexes.export(),
            },
        }
    }

    // ---- index declarations (M4.5-S03, ADR-0075) ----

    /// Registers an index declaration on this cell (the DDL fan's apply
    /// leg and the S10 origin leg share this path). The namespace-mode
    /// gate runs here — the registry itself is namespace-agnostic.
    ///
    /// # Errors
    /// `UnknownNamespace` for an unregistered named target;
    /// `TierRefusesIndexes` (ADR-0072 D8a); `InvalidProgram` from the
    /// gauntlet; the registry's own typed refusals.
    pub fn idx_create(&mut self, spec: IndexSpec) -> Result<(), IndexError> {
        if spec.ns.0 >= FIRST_NAMED_NS_ID {
            let ns_spec =
                self.named.get_by_id(spec.ns).ok_or(IndexError::UnknownNamespace(spec.ns.0))?;
            if ns_spec.tier.is_some() {
                return Err(IndexError::TierRefusesIndexes);
            }
        }
        validate_program_gate(&spec.program)?;
        #[cfg(feature = "doc")]
        let attach = (spec.id, spec.generation, spec.key_type, spec.ns);
        #[cfg(feature = "doc")]
        let program = spec.program.clone();
        self.indexes.create(spec, false)?;
        // Attach-block sync point (ADR-0076 D1): a materialized store
        // gets its tree now; a lazy store installs at materialization.
        #[cfg(feature = "doc")]
        if let Some(store) = self.existing_store_mut(attach.3) {
            store.idx.install(attach.0, attach.1, attach.2, &program);
        }
        self.refresh_pressure();
        Ok(())
    }

    /// Completes a drop (teardown finished on this cell): the entry and
    /// its tree go; the id stays retired forever.
    ///
    /// # Errors
    /// `Unknown` for an unregistered id.
    pub fn idx_drop_finish(&mut self, id: IndexId) -> Result<(), IndexError> {
        let spec = self.indexes.remove(id)?;
        #[cfg(feature = "doc")]
        if let Some(store) = self.existing_store_mut(spec.ns) {
            store.idx.remove(id);
        }
        #[cfg(not(feature = "doc"))]
        let _ = spec;
        self.refresh_pressure();
        Ok(())
    }

    /// Rebuild on this cell: the catalog transition (generation bump,
    /// ADR-0075 D3) and the attach tree reset (ADR-0076 D1) together.
    ///
    /// # Errors
    /// `Unknown` / `InvalidTransition` (only `Ready` rebuilds).
    pub fn idx_rebuild(&mut self, id: IndexId, new_generation: u64) -> Result<(), IndexError> {
        self.indexes.rebuild(id, new_generation)?;
        #[cfg(feature = "doc")]
        {
            let ns = self.indexes.get_by_id(id).expect("just rebuilt").ns;
            if let Some(store) = self.existing_store_mut(ns) {
                store.idx.reset_tree(id, new_generation);
            }
        }
        self.refresh_pressure();
        Ok(())
    }

    /// The per-cell index registry (lifecycle transitions, trees,
    /// binding validation — DDL/MAINTAIN-rate access only).
    pub fn idx_registry(&self) -> &IndexRegistry {
        &self.indexes
    }

    /// Split borrow for the backfill sync's retain pass (M4.5-S05).
    pub(crate) fn backfill_and_registry_mut(&mut self) -> (&mut Vec<BackfillJob>, &IndexRegistry) {
        (&mut self.backfill, &self.indexes)
    }

    /// Folds one tick into the cumulative INFO totals (M4.5-S05).
    pub(crate) fn idx_backfill_note_totals(&mut self, stats: &BackfillTickStats) {
        self.backfill_docs_total += stats.docs_scanned;
        self.backfill_inserted_total += stats.entries_inserted;
    }

    /// The cumulative walk totals half of [`BackfillInfo`] (M4.5-S05).
    pub(crate) fn backfill_totals(&self) -> BackfillInfo {
        BackfillInfo {
            docs_scanned_total: self.backfill_docs_total,
            entries_inserted_total: self.backfill_inserted_total,
            ..BackfillInfo::default()
        }
    }

    pub fn idx_registry_mut(&mut self) -> &mut IndexRegistry {
        &mut self.indexes
    }

    /// Whether `ns` carries any live index declaration — the recompute
    /// source for the S04 store-side cached flag (ADR-0072 D2), never
    /// the per-mutation consultation itself.
    #[must_use]
    pub fn ns_has_indexes(&self, ns: NsId) -> bool {
        self.indexes.has_indexes(ns)
    }

    /// The plane's bracket guard (M4.5-S04, ADR-0076 D3): one cheap test
    /// per write command — with zero indexes anywhere it is a load of an
    /// empty list's length.
    #[must_use]
    pub fn ns_indexed(&self, ns: NsId) -> bool {
        self.indexes.has_indexes(ns)
    }

    /// The store owning `ns` **iff already materialized** (attach sync
    /// points must never force materialization on the DDL path).
    #[cfg(feature = "doc")]
    pub(crate) fn existing_store_mut(&mut self, ns: NsId) -> Option<&mut CellStore> {
        if ns.0 < FIRST_NAMED_NS_ID {
            self.dbs[ns.0 as usize].as_deref_mut()
        } else {
            self.named_stores.iter_mut().find(|e| e.id == ns).map(|e| e.store.as_mut())
        }
    }

    /// Read-only resolution of `ns` to its store (defaults + named).
    #[cfg(feature = "doc")]
    pub(crate) fn existing_store(&self, ns: NsId) -> Option<&CellStore> {
        if ns.0 < FIRST_NAMED_NS_ID {
            self.dbs.get(ns.0 as usize).and_then(|s| s.as_deref())
        } else {
            self.named_stores.iter().find(|e| e.id == ns).map(|e| e.store.as_ref())
        }
    }

    /// The bracket pre-half for one command on `ns` (ADR-0072 D3 rows —
    /// the plane calls this after admission, before execution).
    /// Materializes the store: an indexed namespace's first write must
    /// still run its bracket.
    ///
    /// # Errors
    /// [`IdxMaintRefusal`] — the caller writes the typed refusal and the
    /// command never executes (nothing changed).
    #[cfg(feature = "doc")]
    pub fn idx_bracket_begin(
        &mut self,
        ns: NsId,
        keys: &[&[u8]],
        mutation_path: Option<&inf_doc::PathProgram>,
    ) -> Result<(), crate::index_maint::IdxMaintRefusal> {
        let store = if ns.0 < FIRST_NAMED_NS_ID {
            self.db_mut(ns.0 as usize)
        } else {
            let Some(store) = self.ns_store_mut(ns) else { return Ok(()) };
            store
        };
        store.idx_bracket_begin(keys, mutation_path)
    }

    /// The bracket commit-half (after the mutation applied and, on
    /// durable namespaces, staged). Infallible — failures land in the
    /// degraded backstop (ADR-0072 D7.2).
    #[cfg(feature = "doc")]
    pub fn idx_bracket_commit(&mut self, ns: NsId, keys: &[&[u8]]) {
        if let Some(store) = self.existing_store_mut(ns) {
            store.idx_bracket_commit(keys, crate::index_maint::MaintMode::Strict);
        }
    }

    /// Aborts an open bracket without applying (the plane's refusal
    /// paths between the halves).
    #[cfg(feature = "doc")]
    pub fn idx_bracket_abort(&mut self, ns: NsId) {
        if let Some(store) = self.existing_store_mut(ns) {
            store.idx_bracket_abort();
        }
    }

    /// Mutable tree access for `(ns, id)` — S05's backfill walk inserts
    /// through this; tests grow trees without a document corpus.
    #[cfg(feature = "doc")]
    pub fn idx_tree_mut(
        &mut self,
        ns: NsId,
        id: IndexId,
    ) -> Option<&mut crate::index_registry::IndexTree> {
        self.existing_store_mut(ns).and_then(|s| s.idx.tree_mut(id))
    }

    /// This cell's tree for `(ns, id)` (tests, S05's walk, S11's range
    /// reads) — `None` until the store materializes or when undeclared.
    pub fn idx_tree(&self, ns: NsId, id: IndexId) -> Option<&crate::index_registry::IndexTree> {
        #[cfg(feature = "doc")]
        {
            self.existing_store(ns).and_then(|s| s.idx.tree(id))
        }
        #[cfg(not(feature = "doc"))]
        {
            let _ = (ns, id);
            None
        }
    }

    /// The cell-local serving veto (ADR-0072 D7.2): `Some(true)` means
    /// queries must refuse with the rebuild-path error (S11 consults it
    /// beside the registry's binding gate).
    pub fn idx_degraded(&self, ns: NsId, id: IndexId) -> Option<bool> {
        #[cfg(feature = "doc")]
        {
            self.existing_store(ns).and_then(|s| s.idx.is_degraded(id))
        }
        #[cfg(not(feature = "doc"))]
        {
            let _ = (ns, id);
            None
        }
    }

    /// Per-index maintenance counters (S10's `INF.IDX LIST` renders
    /// these; tests assert them).
    pub fn idx_counters(&self, ns: NsId, id: IndexId) -> Option<crate::index_maint::IdxCounters> {
        #[cfg(feature = "doc")]
        {
            self.existing_store(ns).and_then(|s| s.idx.counters(id))
        }
        #[cfg(not(feature = "doc"))]
        {
            let _ = (ns, id);
            None
        }
    }

    /// Node-fold of the maintenance counters (the INFO stats lines).
    pub fn idx_counters_total(&self) -> crate::index_maint::IdxCounters {
        let mut total = crate::index_maint::IdxCounters::default();
        for store in self.all_stores() {
            total.absorb(&store.idx.counters_fold());
        }
        total
    }

    /// Marks this cell's copy of `(ns, id)` converged (S05 flips it when
    /// the backfill walk completes; the `Strict` found/fresh asserts
    /// apply only past it).
    #[cfg(feature = "doc")]
    pub fn idx_set_converged(&mut self, ns: NsId, id: IndexId, converged: bool) {
        if let Some(store) = self.existing_store_mut(ns) {
            store.idx.set_converged(id, converged);
        }
    }

    // ---- checkpoint sidecar (M4.5-S06, ADR-0078) ----

    /// Sidecar-eligible indexes on `ns` (ADR-0078 D1: converged and
    /// non-degraded). Rows: `(id, generation, fixed8, entries)` — the
    /// checkpoint driver captures its emission plan from these.
    #[must_use]
    pub fn idx_sidecar_candidates(&self, ns: NsId) -> Vec<(IndexId, u64, bool, u64)> {
        #[cfg(feature = "doc")]
        {
            self.existing_store(ns).map(|s| s.idx.sidecar_candidates()).unwrap_or_default()
        }
        #[cfg(not(feature = "doc"))]
        {
            let _ = ns;
            Vec::new()
        }
    }

    /// Whether `(ns, id, generation)` is still sidecar-eligible — the
    /// driver re-checks between slices and abandons the stream (no
    /// FINAL) on any change (ADR-0078 D1).
    #[must_use]
    pub fn idx_sidecar_eligible(&self, ns: NsId, id: IndexId, generation: u64) -> bool {
        #[cfg(feature = "doc")]
        {
            self.existing_store(ns).is_some_and(|s| s.idx.sidecar_eligible(id, generation))
        }
        #[cfg(not(feature = "doc"))]
        {
            let _ = (ns, id, generation);
            false
        }
    }

    /// Emits up to `max_entries` pairs of `(ns, id)`'s tree from
    /// `cursor` in ascending order (the re-seek cursor is the walk's
    /// resume state — never pinned across slices). Returns the emitted
    /// count; fewer than `max_entries` means the tree is exhausted.
    pub fn idx_sidecar_emit(
        &self,
        ns: NsId,
        id: IndexId,
        cursor: &mut crate::ordered::OrderedCursor,
        max_entries: u32,
        mut emit: impl FnMut(&[u8], u64),
    ) -> u32 {
        let Some(tree) = self.idx_tree(ns, id) else { return 0 };
        let mut emitted = 0u32;
        while emitted < max_entries {
            let Some((key, entry_ref)) = tree.cursor_next(cursor) else { break };
            emit(key, entry_ref);
            emitted += 1;
        }
        emitted
    }

    /// Whether any declaration targets a durable namespace — the `.ick`
    /// v2 selection predicate's index half (ADR-0073 D2 as refined by
    /// ADR-0078 D7: registration, not convergence, drives the version).
    #[must_use]
    pub fn idx_declared_on_durable(&self) -> bool {
        self.indexes
            .iter()
            .any(|spec| self.named.get_by_id(spec.ns).is_some_and(|ns| ns.mode == NsMode::Durable))
    }

    /// Mutable tree access for the sidecar loader, materializing the
    /// owning store (an index on an unwritten namespace still loads its
    /// empty-FINAL sidecar — the backfill-tick materialization
    /// precedent).
    #[cfg(feature = "doc")]
    pub(crate) fn idx_sidecar_tree_mut(
        &mut self,
        ns: NsId,
        id: IndexId,
    ) -> Option<&mut crate::index_registry::IndexTree> {
        let store = if ns.0 < FIRST_NAMED_NS_ID {
            self.db_mut(ns.0 as usize)
        } else {
            self.ns_store_mut(ns)?
        };
        store.idx.tree_mut(id)
    }

    /// The loader's body-class discard: empty the tree, touch nothing
    /// else (ADR-0078 D6).
    #[cfg(feature = "doc")]
    pub(crate) fn idx_sidecar_reset(&mut self, ns: NsId, id: IndexId) {
        if let Some(store) = self.existing_store_mut(ns) {
            store.idx.reset_tree_contents(id);
        }
    }

    /// This boot's sidecar fold (`INFO stats` renders `idx_sidecar_*`).
    #[must_use]
    pub fn idx_sidecar_info(&self) -> crate::index_sidecar::SidecarBootInfo {
        self.sidecar_info
    }

    /// Written once by the loader's commit (ADR-0078 D6).
    #[cfg(feature = "doc")]
    pub(crate) fn note_sidecar_totals(&mut self, info: crate::index_sidecar::SidecarBootInfo) {
        self.sidecar_info = info;
    }

    /// Arms replay-time maintenance on `ns` (ADR-0076 D7): `None` (the
    /// boot default) means replay does not maintain — the no-sidecar
    /// path rebuilds via S05. S06's sidecar load arms `CatchUp`;
    /// rebuild-through-replay tests arm `Strict`.
    #[cfg(feature = "doc")]
    pub fn idx_set_replay_maintenance(
        &mut self,
        ns: NsId,
        mode: Option<crate::index_maint::MaintMode>,
    ) {
        let store = if ns.0 < FIRST_NAMED_NS_ID {
            self.db_mut(ns.0 as usize)
        } else {
            let Some(store) = self.ns_store_mut(ns) else { return };
            store
        };
        store.idx.set_replay_maintenance(mode);
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
    ) -> Result<ReplayOutcome, ReplayError> {
        // Tiered namespaces own their records' replay (ADR-0057 D4,
        // routed by M4-S26) — intercepted before the CellStore arms
        // because a tiered namespace also materializes a named CellStore
        // shell, and applying a tiered record there would build state the
        // tiered index never serves (invisible until the first restart).
        if let Some(outcome) = self.apply_record_tiered(rec)? {
            return Ok(outcome);
        }
        match *rec {
            LogRecordView::StringPostImage { ns, key, value } => {
                let Some(store) = self.replay_store(ns) else {
                    return Ok(ReplayOutcome::SkippedUnknownNs);
                };
                // Replay maintenance (ADR-0072 D4): a string image over a
                // document key is an overwrite death — the same bracket,
                // the same code path, dialed by the store's replay mode.
                #[cfg(feature = "doc")]
                let maint = store.idx_replay_begin(key);
                let outcome = store.replay_set(key, value, now);
                #[cfg(feature = "doc")]
                if let Some(mode) = maint {
                    store.idx_bracket_commit(&[key], mode);
                }
                outcome.map_err(ReplayError::Store)?;
                Ok(ReplayOutcome::Applied)
            }
            LogRecordView::Delete { ns, key } => {
                let Some(store) = self.replay_store(ns) else {
                    return Ok(ReplayOutcome::SkippedUnknownNs);
                };
                #[cfg(feature = "doc")]
                let maint = store.idx_replay_begin(key);
                store.replay_del(key, now);
                #[cfg(feature = "doc")]
                if let Some(mode) = maint {
                    store.idx_bracket_commit(&[key], mode);
                }
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
            // Every ColdDisplace record resolves in the tiered
            // pre-dispatch above; this arm exists for exhaustiveness.
            LogRecordView::ColdDisplace { .. } => {
                debug_assert!(false, "ColdDisplace is intercepted by apply_record_tiered");
                Ok(ReplayOutcome::SkippedReserved)
            }
            // Out-of-line reference (M4-S17, ADR-0061 D2) naming a
            // namespace that is not tiered here (dropped, or a foreign
            // log) — memory-mode namespaces have no extents, so nothing
            // in a CellStore can apply it.
            LogRecordView::StringExtentRef { .. } => Ok(ReplayOutcome::SkippedReserved),
            // Checkpoint marker (M2-S10, ADR-0016 D3): carries no state —
            // S13's recovery orchestration consumes its LSN, not replay.
            LogRecordView::CkptBegin { .. } => Ok(ReplayOutcome::SkippedMarker),
            LogRecordView::DocFull { ns, key, lineage, version, idoc } => {
                let Some(store) = self.replay_store(ns) else {
                    return Ok(ReplayOutcome::SkippedUnknownNs);
                };
                #[cfg(feature = "doc")]
                {
                    let maint = store.idx_replay_begin(key);
                    let outcome = store.replay_json_full(key, lineage, version, idoc, now);
                    if let Some(mode) = maint {
                        store.idx_bracket_commit(&[key], mode);
                    }
                    outcome?;
                    Ok(ReplayOutcome::Applied)
                }
                #[cfg(not(feature = "doc"))]
                {
                    let _ = (store, key, lineage, version, idoc, now);
                    Err(ReplayError::DocumentUnsupported)
                }
            }
            LogRecordView::DocDelta {
                ns,
                key,
                lineage,
                base_version,
                match_count,
                post_len,
                opcode,
                program,
                operand,
            } => {
                let Some(store) = self.replay_store(ns) else {
                    return Ok(ReplayOutcome::SkippedUnknownNs);
                };
                #[cfg(feature = "doc")]
                {
                    let program = inf_doc::PathProgram::from_bytes(program)
                        .map_err(ReplayError::InvalidPathProgram)?;
                    let op = inf_doc::decode_apply_op(opcode, operand)
                        .map_err(ReplayError::InvalidDelta)?;
                    let maint = store.idx_replay_begin(key);
                    let outcome = store.replay_json_delta(
                        key,
                        crate::doc::DocDeltaWitness {
                            lineage,
                            base_version,
                            match_count,
                            post_len,
                        },
                        &program,
                        &op,
                        now,
                    );
                    if let Some(mode) = maint {
                        store.idx_bracket_commit(&[key], mode);
                    }
                    match outcome? {
                        crate::doc::DocReplayOutcome::Applied => Ok(ReplayOutcome::Applied),
                        crate::doc::DocReplayOutcome::SkippedStale => {
                            Ok(ReplayOutcome::SkippedDocDeltaStale)
                        }
                        crate::doc::DocReplayOutcome::SkippedMissing => {
                            Ok(ReplayOutcome::SkippedDocDeltaMissing)
                        }
                    }
                }
                #[cfg(not(feature = "doc"))]
                {
                    let _ = (
                        store,
                        key,
                        lineage,
                        base_version,
                        match_count,
                        post_len,
                        opcode,
                        program,
                        operand,
                        now,
                    );
                    Err(ReplayError::DocumentUnsupported)
                }
            }
        }
    }

    /// The tiered replay arms (ADR-0057 D4 rules 1–3; register bound by
    /// ADR-0059 D9). Returns `None` when the record is not tiered-routed
    /// — the caller's CellStore arms own it.
    ///
    /// # Errors
    /// Register overflow, marker adjacency violations, and store space
    /// refusals — all recovery fail-stop at the caller.
    fn apply_record_tiered(
        &mut self,
        rec: &LogRecordView<'_>,
    ) -> Result<Option<ReplayOutcome>, ReplayError> {
        // Strict marker adjacency (ADR-0057 D4: markers stage in the same
        // frame immediately before their mutation): while the register is
        // armed, only further markers or the paired mutation — all in the
        // marker's namespace — are legal.
        if let Some(&(pending_ns, _)) = self.pending_displace.first() {
            let legal = match *rec {
                LogRecordView::ColdDisplace { ns, .. }
                | LogRecordView::StringPostImage { ns, .. }
                | LogRecordView::Delete { ns, .. }
                | LogRecordView::StringExtentRef { ns, .. } => ns == pending_ns,
                _ => false,
            };
            if !legal {
                return Err(ReplayError::Displacement(
                    "displacement marker not followed by its paired mutation",
                ));
            }
        }
        match *rec {
            LogRecordView::ColdDisplace { ns, old_addr } => {
                if !self.is_tiered(ns) {
                    // Dropped namespace or a foreign log: the paired
                    // mutation skips the same way — no register entry.
                    return Ok(Some(ReplayOutcome::SkippedUnknownNs));
                }
                if self.pending_displace.len() >= DISPLACE_REGISTER_CAP {
                    return Err(ReplayError::Displacement(
                        "displacement register overflow (ADR-0059 D9 bounds markers at 4)",
                    ));
                }
                self.pending_displace.push((ns, old_addr));
                Ok(Some(ReplayOutcome::Applied))
            }
            LogRecordView::StringPostImage { ns, key, value } if self.is_tiered(ns) => {
                let hash = self.cfg.hasher.hash(key);
                self.drain_displace(ns, hash)?;
                let table = self.tiered_store_mut(ns).expect("is_tiered checked");
                table.apply_image(key, value, hash).map_err(ReplayError::Store)?;
                Ok(Some(ReplayOutcome::Applied))
            }
            LogRecordView::Delete { ns, key } if self.is_tiered(ns) => {
                let hash = self.cfg.hasher.hash(key);
                self.drain_displace(ns, hash)?;
                let table = self.tiered_store_mut(ns).expect("is_tiered checked");
                table.apply_delete(key, hash);
                Ok(Some(ReplayOutcome::Applied))
            }
            LogRecordView::StringExtentRef { ns, key, extent_id, offset, len }
                if self.is_tiered(ns) =>
            {
                let hash = self.cfg.hasher.hash(key);
                self.drain_displace(ns, hash)?;
                let table = self.tiered_store_mut(ns).expect("is_tiered checked");
                table
                    .apply_extent_image(key, hash, ExtentRef { extent_id, offset, len })
                    .map_err(ReplayError::Store)?;
                Ok(Some(ReplayOutcome::Applied))
            }
            // Tiered namespaces carry no expiry and no documents in M4
            // (commands refuse; only a foreign log can contain these) —
            // skipped so they never build CellStore-shell state.
            LogRecordView::ExpireAt { ns, .. }
            | LogRecordView::DocFull { ns, .. }
            | LogRecordView::DocDelta { ns, .. }
                if self.is_tiered(ns) =>
            {
                Ok(Some(ReplayOutcome::SkippedReserved))
            }
            _ => Ok(None),
        }
    }

    /// Applies every parked marker with the paired mutation's key hash
    /// (D4 rule 1): exact `(hash, old_addr)` removal, zero disk reads.
    fn drain_displace(&mut self, ns: NsId, hash: u64) -> Result<(), ReplayError> {
        if self.pending_displace.is_empty() {
            return Ok(());
        }
        let pending = core::mem::take(&mut self.pending_displace);
        let table = self.tiered_store_mut(ns).expect("caller checked tiered");
        for &(marker_ns, old_addr) in &pending {
            debug_assert_eq!(marker_ns, ns, "adjacency check pinned the namespace");
            let Some(addr) = LogicalAddr::from_raw(old_addr) else {
                return Err(ReplayError::Displacement(
                    "displacement address exceeds the 48-bit logical space",
                ));
            };
            table.apply_displace(hash, addr);
        }
        Ok(())
    }

    /// Parked displacement markers awaiting their paired mutation — the
    /// end-of-log check (ADR-0057 D4): recovery fail-stops when this is
    /// non-zero after the last replayed record.
    #[must_use]
    pub fn displace_register_len(&self) -> usize {
        self.pending_displace.len()
    }

    /// Whether `ns` names a materialized tiered table on this cell.
    #[must_use]
    pub fn is_tiered(&self, ns: NsId) -> bool {
        self.tiered_stores.iter().any(|(id, _)| *id == ns)
    }

    /// The node's key hasher (ADR-0094): every store of this keyspace
    /// hashes with it, and the plane copies it for its stage-time
    /// prefetch so the hash it computes is the one the store probes.
    #[inline]
    #[must_use]
    pub fn hasher(&self) -> KeyHasher {
        self.cfg.hasher
    }

    /// Shared view of a tiered table (the command layer's lookup path —
    /// M4-S26; mutation goes through [`tiered_store_mut`](Self::tiered_store_mut)).
    #[must_use]
    pub fn tiered_store(&self, ns: NsId) -> Option<&TieredTable> {
        self.tiered_stores.iter().find(|(id, _)| *id == ns).map(|(_, t)| t.as_ref())
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
        // A tiered namespace's entries live in its table, not the named
        // CellStore shell — presizing the shell would allocate a dataset-
        // sized index nothing ever touches (M4-S26).
        if self.is_tiered(ns) {
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
        for entry in &self.named_stores {
            fold_store(&mut acc, &entry.store, NAMED_TAG | u64::from(entry.id.0), now);
        }
        acc
    }

    /// Every materialized store: default dbs, then named (aggregation
    /// order is stable but unspecified).
    fn all_stores(&self) -> impl Iterator<Item = &CellStore> {
        self.dbs
            .iter()
            .filter_map(|s| s.as_deref())
            .chain(self.named_stores.iter().map(|e| e.store.as_ref()))
    }
}

/// This cell's share of a per-namespace `MAXMEMORY` (ADR-0068 D2): the
/// node-wide budget divides by the symmetric cell count exactly like the
/// node `maxmemory`. `0` means "no budget" (the inheritance leg), so a
/// budget smaller than the cell count floors at 1 byte per cell — a
/// nonsense-tiny budget stays a budget, never accidentally unlimited.
/// Install every declaration of `ns` into a store's attach block — the
/// ADR-0076 D1 sync helper shared by materialization, DDL, and the seed
/// resync. The registry validated each program at its trust boundary.
#[cfg(feature = "doc")]
fn install_ns_attaches(indexes: &IndexRegistry, ns: NsId, store: &mut CellStore) {
    for spec in indexes.iter().filter(|s| s.ns == ns) {
        store.idx.install(spec.id, spec.generation, spec.key_type, &spec.program);
    }
}

/// The DDL-side program gauntlet (ADR-0075 D2.4) — byte-valid M3
/// bytecode inside the indexable-path fence. A doc-less build refuses:
/// it could not maintain the projection it would be declaring (L8).
#[cfg(feature = "doc")]
fn validate_program_gate(bytes: &[u8]) -> Result<(), IndexError> {
    crate::index_registry::validate_index_program(bytes).map_err(IndexError::InvalidProgram)
}

#[cfg(not(feature = "doc"))]
fn validate_program_gate(_bytes: &[u8]) -> Result<(), IndexError> {
    Err(IndexError::InvalidProgram("indexes require the document engine (doc feature)"))
}

fn ns_budget_share(maxmemory: Option<u64>, shares: u64) -> u64 {
    maxmemory.map_or(0, |bytes| (bytes / shares.max(1)).max(1))
}

/// Whether a named store belongs to the global eviction hand (ADR-0068 D2
/// inheritance leg): no budget of its own and an effective policy that
/// evicts. Durable and tiered-shell stores are excluded by construction —
/// their policy is pinned `NoEviction` at materialization.
fn named_in_hand(entry: &NamedStore) -> bool {
    entry.budget_share == 0 && entry.store.eviction_policy() != EvictionPolicy::NoEviction
}

/// Aggregated tiered-table memory attribution (M4-S07, L5). Rendered in
/// `INFO tiering`; every field is identically zero on memory-mode nodes
/// and joins the S03 zero-assert set.
#[derive(Copy, Clone, Default, Debug, PartialEq, Eq)]
pub struct TieredUsage {
    /// Reserved ring virtual bytes (sum of `R` across tables).
    pub reserved_bytes: u64,
    /// Committed ring bytes — the RAM-residency figure `MEM-BUDGET`
    /// bounds (ADR-0053 D1).
    pub committed_bytes: u64,
    /// Bytes allocated this life (`tail − life_origin`, summed).
    pub allocated_bytes: u64,
    /// Dead bytes (relocations, deletes, seal holes — S14's input).
    pub dead_bytes: u64,
    /// Live record bytes.
    pub live_bytes: u64,
    /// Index bytes (incl. the tiered hash sidecar).
    pub index_bytes: u64,
}

/// Aggregated disk-admission observables (M4-S21, ADR-0063 D5).
/// Rendered in `INFO tiering`; identically zero on memory-mode nodes
/// (no table exists) and joins the S03 zero-assert set.
#[derive(Copy, Clone, Default, Debug, PartialEq, Eq)]
pub struct DiskAdmissionTotals {
    /// Namespaces currently refusing (budget or device leg).
    pub full_namespaces: u64,
    /// Typed `DISKFULL` refusals issued (sum).
    pub refusals: u64,
    /// `nothing_compactable`-under-pressure rounds (sum) — the "full of
    /// live data" operator alarm.
    pub compact_idle_pressure: u64,
    /// `disk_used` at each table's last admission recompute (sum) — the
    /// enforced snapshots, not a live `statvfs`.
    pub used_bytes: u64,
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
        cursor = store.digest_checkpoint_images(cursor, 1024, now, |key, image, expire_at_ms| {
            let hk = hash64(key, DIGEST_SEED ^ tag);
            let hv = match image {
                CheckpointImage::String(value) => hash64(value, hk),
                #[cfg(feature = "doc")]
                CheckpointImage::JsonDoc { lineage, version, idoc } => {
                    let version = version.to_le_bytes();
                    let lineage = lineage.get().to_le_bytes();
                    hash64(&version, hash64(&lineage, hash64(idoc, hk ^ 0xD0C0_F011_D0C0_F011)))
                }
            };
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
    /// A fuzzy checkpoint/DocFull already includes this delta (R1).
    SkippedDocDeltaStale,
    /// The fuzzy walker captured the later delete/expiry (R2).
    SkippedDocDeltaMissing,
}

/// Replay failures are durability-input failures, never client command
/// errors. Recovery maps every variant to fail-stop `InvalidData`.
#[derive(Debug)]
pub enum ReplayError {
    Store(OpError),
    /// Displacement-marker stream violation (ADR-0057 D4 pairing /
    /// ADR-0059 D9 bound) — corrupt or truncated tiered replay input.
    Displacement(&'static str),
    DocumentUnsupported,
    #[cfg(feature = "doc")]
    InvalidDocument(inf_doc::DocError),
    #[cfg(feature = "doc")]
    InvalidPathProgram(inf_doc::PathError),
    #[cfg(feature = "doc")]
    InvalidDelta(inf_doc::DeltaDecodeError),
    #[cfg(feature = "doc")]
    InvalidMutation(inf_doc::ApplyError),
    CorruptDocument(&'static str),
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
            tier: None,
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
        let cat = ks.export_catalog(17, 1, 1);
        let mut fresh = Keyspace::new(StoreConfig::default());
        fresh.seed_catalog(&cat).expect("seed");
        assert_eq!(fresh.export_catalog(17, 1, 1), cat);
        assert!(fresh.ns_store_mut(NsId(16)).is_some());
    }

    // ---- M4.5-S03 (ADR-0075): index registry + catalog + accounting ----

    #[cfg(feature = "doc")]
    fn idx_spec(id: u32, generation: u64, ns: u32, name: &[u8], state: IndexState) -> IndexSpec {
        IndexSpec {
            id: IndexId(id),
            generation,
            ns: NsId(ns),
            name: name.to_vec(),
            program: inf_doc::path::compile(b"$.price").expect("valid path").as_bytes().to_vec(),
            key_type: crate::IndexKeyType::F64,
            state,
        }
    }

    /// The DDL gates: tiered namespaces refuse (ADR-0072 D8a), unknown
    /// named targets refuse, default dbs and plain namespaces accept,
    /// and a fenced-out path refuses through the shared gauntlet.
    #[cfg(feature = "doc")]
    #[test]
    fn idx_create_gates_by_namespace_mode() {
        let mut ks = Keyspace::new(StoreConfig::default());
        ks.ns_create(durable_spec(16, b"ledger")).expect("create");
        ks.ns_create(NsSpec {
            fsync: Some(FsyncClass::Everysec),
            tier: Some(crate::ns::TierSpec::for_budget(64 << 20)),
            ..durable_spec(17, b"cold")
        })
        .expect("tiered create");
        ks.idx_create(idx_spec(1, 1, 0, b"on-db0", IndexState::Declared)).expect("default db");
        ks.idx_create(idx_spec(2, 2, 16, b"on-ledger", IndexState::Declared)).expect("named ns");
        assert_eq!(
            ks.idx_create(idx_spec(3, 3, 17, b"on-cold", IndexState::Declared)),
            Err(IndexError::TierRefusesIndexes)
        );
        assert_eq!(
            ks.idx_create(idx_spec(3, 3, 99, b"nowhere", IndexState::Declared)),
            Err(IndexError::UnknownNamespace(99))
        );
        let fenced = IndexSpec {
            program: inf_doc::path::compile(b"$..a").expect("valid path").as_bytes().to_vec(),
            ..idx_spec(3, 3, 16, b"fenced", IndexState::Declared)
        };
        assert!(matches!(ks.idx_create(fenced), Err(IndexError::InvalidProgram(_))));
        assert!(ks.ns_has_indexes(NsId(0)));
        assert!(ks.ns_has_indexes(NsId(16)));
        assert!(!ks.ns_has_indexes(NsId(17)));
    }

    /// The ADR-0075 D4 restart semantics: `dropping` resumes its drop
    /// (gone after seed, gone from the next export); every other state
    /// survives with id/generation intact and regresses to
    /// `backfilling`, with the pre-crash-`ready` hint retained for S06.
    /// Driving the rebuild to completion returns the pre-crash-ready
    /// index to `ready` — the AC's end state.
    #[cfg(feature = "doc")]
    #[test]
    fn catalog_restart_maps_index_states_per_d4() {
        let mut ks = Keyspace::new(StoreConfig::default());
        ks.ns_create(durable_spec(16, b"ledger")).expect("create");
        ks.idx_create(idx_spec(1, 10, 16, b"was-ready", IndexState::Declared)).expect("create");
        ks.idx_create(idx_spec(2, 11, 16, b"mid-backfill", IndexState::Declared)).expect("create");
        ks.idx_create(idx_spec(3, 12, 16, b"mid-drop", IndexState::Declared)).expect("create");
        let reg = ks.idx_registry_mut();
        reg.set_catalog_state(IndexId(1), IndexState::Backfilling).expect("edge");
        reg.set_catalog_state(IndexId(1), IndexState::Ready).expect("edge");
        reg.set_catalog_state(IndexId(2), IndexState::Backfilling).expect("edge");
        reg.set_catalog_state(IndexId(3), IndexState::Dropping).expect("edge");
        let cat = ks.export_catalog(17, 4, 13);

        let mut fresh = Keyspace::new(StoreConfig::default());
        fresh.seed_catalog(&cat).expect("seed");
        let reg = fresh.idx_registry();
        // The dropping entry resumed its drop: not seeded.
        assert!(reg.get_by_id(IndexId(3)).is_none());
        // Survivors regress to backfilling, generations un-bumped
        // (ADR-0073 D5.1 — a boot bump would stale every sidecar).
        let ready = reg.get_by_id(IndexId(1)).expect("survives");
        assert_eq!(ready.state, IndexState::Backfilling);
        assert_eq!(ready.generation, 10);
        assert_eq!(reg.was_ready(IndexId(1)), Some(true), "the S06 sidecar hint");
        let mid = reg.get_by_id(IndexId(2)).expect("survives");
        assert_eq!(mid.state, IndexState::Backfilling);
        assert_eq!(reg.was_ready(IndexId(2)), Some(false));
        // Planning refuses both until rebuild completes...
        assert!(fresh.idx_registry().validate_binding(NsId(16), IndexId(1), 10).is_err());
        // ...and the next export no longer carries the dropped entry.
        assert!(!fresh.export_catalog(17, 4, 13).index.entries.iter().any(|e| e.id == IndexId(3)));
        // Rebuild completion returns the pre-crash-ready index to ready.
        fresh.idx_registry_mut().set_catalog_state(IndexId(1), IndexState::Ready).expect("edge");
        fresh.idx_registry().validate_binding(NsId(16), IndexId(1), 10).expect("ready again");
    }

    /// ADR-0075 D6 (the ADR-0072 D8c decision): `idx_tree_bytes` counts
    /// toward the namespace's `MAXMEMORY` used bytes — index growth
    /// tightens the document budget, and the OOM verdict is honest when
    /// nothing evictable remains below `share − idx`.
    #[cfg(feature = "doc")]
    #[test]
    fn idx_tree_bytes_count_toward_the_namespace_budget() {
        let mut ks = Keyspace::new(StoreConfig::default());
        ks.ns_create(NsSpec {
            id: NsId(16),
            name: b"cache".to_vec(),
            mode: NsMode::Memory,
            fsync: None,
            policy: Some(EvictionPolicy::AllKeysRandom),
            maxmemory: None,
            tier: None,
        })
        .expect("create");
        ks.idx_create(idx_spec(1, 1, 16, b"by-price", IndexState::Declared)).expect("create");
        // Fill documents-side bytes, then set the budget just above the
        // store's own usage — without index bytes the flag stays clear.
        for i in 0..64u64 {
            let key = format!("k{i}");
            ks.ns_store_mut(NsId(16))
                .expect("store")
                .set(key.as_bytes(), &[0u8; 128], SetOptions::default(), now())
                .expect("set");
        }
        let store_used = ks.ns_store(NsId(16)).expect("store").used_bytes();
        ks.ns_set_memory(b"cache", Some(EvictionPolicy::AllKeysRandom), Some(store_used + 4096))
            .expect("budget");
        assert!(!ks.ns_over_limit(NsId(16)), "no index bytes yet — under budget");
        // Grow the tree past the headroom; the flag must flip on the
        // combined comparison and the report must attribute the bytes.
        let tree = ks.idx_tree_mut(NsId(16), IndexId(1)).expect("tree");
        for i in 0..4096u64 {
            tree.insert(&i.to_be_bytes(), i).expect("insert");
        }
        ks.refresh_pressure();
        assert!(ks.ns_over_limit(NsId(16)), "index growth tightens the namespace budget");
        let report = ks.report();
        assert!(report.idx_tree_bytes > 4096, "trees attribute (L5)");
        assert!(report.idx_slack_bytes < report.idx_tree_bytes);
        assert!(ks.used_bytes() >= store_used + report.idx_tree_bytes);
        // Dropping the index returns the namespace under budget.
        ks.idx_drop_finish(IndexId(1)).expect("drop");
        assert!(!ks.ns_over_limit(NsId(16)), "drop returns the budget");
        assert_eq!(ks.report().idx_tree_bytes, 0);
    }

    /// A namespace drop takes its declarations with it; the ids stay
    /// retired and the flags recompute.
    #[cfg(feature = "doc")]
    #[test]
    fn ns_drop_removes_its_index_declarations() {
        let mut ks = Keyspace::new(StoreConfig::default());
        ks.ns_create(durable_spec(16, b"ledger")).expect("create");
        ks.idx_create(idx_spec(1, 1, 16, b"by-price", IndexState::Declared)).expect("create");
        assert!(ks.ns_has_indexes(NsId(16)));
        ks.ns_drop(b"ledger").expect("drop");
        assert!(!ks.ns_has_indexes(NsId(16)));
        assert!(ks.idx_registry().get_by_id(IndexId(1)).is_none());
    }

    /// M4-S27 (ADR-0068): a memory namespace's persisted pressure knobs
    /// survive the catalog round trip **as enforcement**, not display —
    /// the reseeded store materializes with the explicit policy and its
    /// own budget gate armed.
    #[test]
    fn catalog_round_trip_preserves_memory_pressure_enforcement() {
        let mut ks = Keyspace::new(StoreConfig::default());
        let spec = NsSpec {
            id: NsId(16),
            name: b"cache".to_vec(),
            mode: NsMode::Memory,
            fsync: None,
            policy: Some(crate::EvictionPolicy::AllKeysRandom),
            maxmemory: Some(1 << 20),
            tier: None,
        };
        ks.ns_create(spec).expect("create");
        let cat = ks.export_catalog(17, 1, 1);
        let mut fresh = Keyspace::new(StoreConfig::default());
        fresh.seed_catalog(&cat).expect("seed");
        let store = fresh.ns_store_mut(NsId(16)).expect("live");
        assert_eq!(store.eviction_policy(), crate::EvictionPolicy::AllKeysRandom);
        assert!(
            fresh.ns_free_for_write(NsId(16), now()).is_some(),
            "the per-namespace budget gate must be armed from the catalog"
        );
        // An inheriting store without a budget answers through the global
        // gate instead.
        let mut ks = Keyspace::new(StoreConfig::default());
        ks.ns_create(NsSpec {
            id: NsId(16),
            name: b"plain".to_vec(),
            mode: NsMode::Memory,
            fsync: None,
            policy: None,
            maxmemory: None,
            tier: None,
        })
        .expect("create");
        assert!(ks.ns_free_for_write(NsId(16), now()).is_none());
    }

    /// ADR-0068 D1: `MAXMEMORY` on a tiered spec refuses typed — one
    /// budget authority per namespace (`MEM-BUDGET` owns tiered memory).
    #[test]
    fn tiered_spec_with_maxmemory_is_refused() {
        let mut ks = Keyspace::new(StoreConfig::default());
        let spec = NsSpec {
            maxmemory: Some(1 << 20),
            tier: Some(crate::TierSpec::for_budget(64 << 20)),
            ..durable_spec(16, b"hot")
        };
        assert_eq!(ks.ns_create(spec), Err(NsError::MaxmemoryNotAllowedTiered));
        assert_eq!(ks.ns_iter().count(), 0, "refusal mutates nothing");
    }

    /// ADR-0068 D3 scope pins: the memory hot-reload path refuses durable
    /// and tiered namespaces typed and applies to memory namespaces —
    /// including the already-materialized store's policy and budget.
    #[test]
    fn ns_set_memory_scopes_and_applies_hot() {
        let mut ks = Keyspace::new(StoreConfig::default());
        ks.ns_create(durable_spec(16, b"ledger")).expect("create");
        ks.ns_create(NsSpec {
            id: NsId(17),
            name: b"cache".to_vec(),
            mode: NsMode::Memory,
            fsync: None,
            policy: None,
            maxmemory: None,
            tier: None,
        })
        .expect("create");
        assert_eq!(
            ks.ns_set_memory(b"ledger", None, Some(1 << 20)),
            Err(NsError::PressureKeysNotHotDurable)
        );
        assert_eq!(ks.ns_set_memory(b"missing", None, None), Err(NsError::Unknown));
        // Materialize first, then hot-reload: the live store follows.
        let _ = ks.ns_store_mut(NsId(17)).expect("live");
        ks.ns_set_memory(b"cache", Some(crate::EvictionPolicy::AllKeysLru), Some(1 << 20))
            .expect("hot");
        assert_eq!(
            ks.ns_store(NsId(17)).expect("live").eviction_policy(),
            crate::EvictionPolicy::AllKeysLru
        );
        assert!(ks.ns_free_for_write(NsId(17), now()).is_some(), "budget gate armed");
        ks.ns_set_memory(b"cache", None, None).expect("hot");
        assert!(ks.ns_free_for_write(NsId(17), now()).is_none(), "budget gate disarmed");
        assert_eq!(
            ks.ns_store(NsId(17)).expect("live").eviction_policy(),
            EvictionPolicy::NoEviction,
            "policy returns to inheriting the (default noeviction) node policy"
        );
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
