//! Keyspace × secondary indexes (M4.5): declarations, attach-block
//! brackets, backfill custody, sidecar emission, and replay maintenance.

use super::*;

impl Keyspace {
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

    /// The loader's commit gate (ADR-0078 A1): a veto raised during the
    /// tail discharges into the boot-fresh state; `true` iff it was set.
    #[cfg(feature = "doc")]
    pub(crate) fn idx_sidecar_discharge_veto(&mut self, ns: NsId, id: IndexId) -> bool {
        self.existing_store_mut(ns).is_some_and(|s| s.idx.discharge_boot_veto(id))
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
}
