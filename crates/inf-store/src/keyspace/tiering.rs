//! Keyspace × tiered namespaces: table materialization, tiering knobs,
//! demotion ticks, and the tiering/write-accounting/extent reports.

use super::*;

impl Keyspace {
    /// Number of durable-tiered tables on this cell (0 until the M4-S04
    /// steel thread materializes one).
    pub fn tiered_tables(&self) -> usize {
        self.tiered_stores.len()
    }

    /// The aggregate reserved-VA admission bound (ADR-0062 D4) for one
    /// more `requested_bytes` ring: checked arithmetic, refused before
    /// any Region exists — the reservation is the VA truth this bound
    /// counts, so the check and the mmap can never disagree. Pure, so
    /// the DDL program can run it before the catalog persist (ADR-0103
    /// D3).
    ///
    /// # Errors
    /// `VaLimitExceeded` with the three quantities named.
    pub fn tiered_admit_check(&self, requested_bytes: u64) -> Result<(), TieredCreateError> {
        let admitted_bytes = self.tiering_usage().reserved_bytes;
        let limit_bytes = self.tiered_va_limit_bytes;
        if admitted_bytes.checked_add(requested_bytes).is_none_or(|total| total > limit_bytes) {
            return Err(TieredCreateError::VaLimitExceeded {
                requested_bytes,
                admitted_bytes,
                limit_bytes,
            });
        }
        Ok(())
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
        self.tiered_admit_check(config.reserve_bytes as u64)?;
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
            total.cold_read_errors += counters.cold_read_errors;
            total.tail_alloc_stalls += counters.tail_alloc_stalls;
            total.demote_slices += counters.demote_slices;
            total.demote_sealed_bytes += counters.demote_sealed_bytes;
            total.flush_slices += counters.flush_slices;
            total.flush_confirmed_bytes += counters.flush_confirmed_bytes;
            total.compact_slices += counters.compact_slices;
            total.write_replans += counters.write_replans;
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
            total.quarantined += stats.quarantined;
            total.quarantine_revived += stats.quarantine_revived;
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
}
