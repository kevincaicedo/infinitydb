//! Sidecar boot loader (M4.5-S06, ADR-0078 D6): consumes validated tag
//! 0x06 sections during checkpoint load, appends pairs into the attach
//! trees through the ascending fast path, and resolves the per-index
//! rebuild-vs-load decision. Every failure past the file's framing is
//! **body-class**: it discards one projection and rebuilds it through
//! the S05 machine — a sidecar can never refuse a boot (L2).
//!
//! The state machine per `(ns, index id)`: `Accepting` (sections must
//! arrive contiguous, canonical, generation/encoding/scheme-exact) →
//! `Loaded` (a FINAL section closed the stream with a matching total)
//! or `Discarded { reason }` (first violation wins; later sections for
//! the pair are ignored). At checkpoint end, `Accepting` means the
//! writer abandoned the stream mid-emission or an unattributed damaged
//! section swallowed part of it — either way `Incomplete`, discarded
//! (the ADR-0078 D4 resolution of unattributable damage).
//!
//! Ordering across sections needs no bookkeeping: the tree starts
//! empty at boot and replay maintenance is unarmed until
//! [`SidecarLoader::finish_load`], so the tree maximum *is* the last
//! loaded pair — [`IndexTree::append`]'s own refusal enforces the
//! cross-section canon.

#[cfg(feature = "doc")]
use inf_log::IckIdxSidecarSection;
use inf_log::NsId;

#[cfg(feature = "doc")]
use crate::index_key::INDEX_KEY_ENCODING_VERSION;
#[cfg(feature = "doc")]
use crate::index_maint::MaintMode;
use crate::index_registry::{IndexId, SidecarBootDecision};
#[cfg(feature = "doc")]
use crate::index_registry::{IndexState, SidecarRebuildReason};
#[cfg(feature = "doc")]
use crate::keyspace::Keyspace;
#[cfg(feature = "doc")]
use crate::ordered::AppendError;

/// Cell-scope INFO fold of this boot's decisions (ADR-0078 D6;
/// rendered as `idx_sidecar_*` — cumulative per boot like the S04/S05
/// `idx_*` lines).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct SidecarBootInfo {
    /// Indexes whose sidecar loaded whole.
    pub loaded: u32,
    /// Indexes rebuilding from scratch (reasons on the per-index rows).
    pub rebuilt: u32,
    /// Well-framed 0x06 sections whose body failed CRC or canon —
    /// counted here because they are attributable to no index (L10).
    pub damaged_sections: u64,
    /// Pairs appended across every loaded index.
    pub entries_loaded: u64,
}

/// One per-index outcome row — the caller logs these (the decision
/// itself is also recorded on the registry entry for `INF.IDX LIST`).
#[derive(Clone, Debug)]
pub struct SidecarBootRow {
    pub ns: NsId,
    pub id: IndexId,
    /// The ADR-0075 D4 hint: `true` + a rebuild decision is the loud
    /// case — the index was serving before the crash and now is not.
    pub was_ready: bool,
    pub decision: SidecarBootDecision,
}

#[cfg(feature = "doc")]
enum LoadState {
    Accepting { loaded: u64 },
    Loaded { total: u64 },
    Discarded { reason: SidecarRebuildReason },
}

#[cfg(feature = "doc")]
struct LoadRec {
    ns: NsId,
    id: IndexId,
    state: LoadState,
}

/// The per-boot loader — owned by the recovery driver, one per cell.
#[cfg(feature = "doc")]
#[derive(Default)]
pub struct SidecarLoader {
    records: Vec<LoadRec>,
    damaged_sections: u64,
    /// Namespaces armed `CatchUp` at `finish_load` (disarmed at commit).
    armed: Vec<NsId>,
    finished: bool,
}

#[cfg(feature = "doc")]
impl SidecarLoader {
    /// Consumes one validated section. Infallible by design: every
    /// verdict is a state-machine transition, never a boot error.
    pub fn apply_section(&mut self, ks: &mut Keyspace, section: &IckIdxSidecarSection<'_>) {
        assert!(!self.finished, "sections after the checkpoint footer");
        let (ns, id) = (NsId(section.ns), IndexId(section.index_id));
        let at = match self.records.iter().position(|r| r.ns == ns && r.id == id) {
            Some(at) => at,
            None => {
                self.records.push(LoadRec { ns, id, state: LoadState::Accepting { loaded: 0 } });
                self.records.len() - 1
            }
        };
        match self.records[at].state {
            // First violation wins; later sections for the pair are
            // ignored (the tree is already reset).
            LoadState::Discarded { .. } => {}
            // A writer never emits past FINAL — writer bug or damage.
            LoadState::Loaded { .. } => {
                self.discard(ks, at, SidecarRebuildReason::AfterFinal);
            }
            LoadState::Accepting { loaded } => {
                if let Some(reason) = self.section_reason(ks, section, loaded) {
                    self.discard(ks, at, reason);
                    return;
                }
                match Self::append_entries(ks, ns, id, section) {
                    Err(reason) => self.discard(ks, at, reason),
                    Ok(appended) => {
                        let loaded = loaded + appended;
                        if !section.final_section {
                            self.records[at].state = LoadState::Accepting { loaded };
                            return;
                        }
                        if section.total_entries != loaded {
                            self.discard(ks, at, SidecarRebuildReason::TotalMismatch);
                            return;
                        }
                        // Pair assertion against the writer's count:
                        // the tree began empty and every append landed.
                        debug_assert_eq!(
                            ks.idx_tree(ns, id).map(|t| t.len()),
                            Some(loaded),
                            "a loaded stream's cardinality equals its tree's"
                        );
                        self.records[at].state = LoadState::Loaded { total: loaded };
                    }
                }
            }
        }
    }

    /// Binding checks against the seeded registry (ADR-0073 D5, ADR-0078
    /// D6) plus the contiguity canon. `None` = the section may load.
    fn section_reason(
        &self,
        ks: &Keyspace,
        section: &IckIdxSidecarSection<'_>,
        loaded: u64,
    ) -> Option<SidecarRebuildReason> {
        let id = IndexId(section.index_id);
        let Some(spec) = ks.idx_registry().get_by_id(id).filter(|s| s.ns.0 == section.ns) else {
            return Some(SidecarRebuildReason::StaleDeclaration);
        };
        if spec.generation != section.generation {
            return Some(SidecarRebuildReason::GenerationMismatch);
        }
        if section.key_encoding_version != INDEX_KEY_ENCODING_VERSION {
            return Some(SidecarRebuildReason::EncodingVersion);
        }
        if spec.key_type.fixed8() != section.fixed8 {
            return Some(SidecarRebuildReason::SchemeMismatch);
        }
        if section.entries_before != loaded {
            return Some(SidecarRebuildReason::NonContiguous);
        }
        None
    }

    /// Appends a section's pairs through the ascending fast path. The
    /// tree's own refusal enforces the cross-section canon (module
    /// docs); capacity refusals are body-class like everything else.
    fn append_entries(
        ks: &mut Keyspace,
        ns: NsId,
        id: IndexId,
        section: &IckIdxSidecarSection<'_>,
    ) -> Result<u64, SidecarRebuildReason> {
        let Some(tree) = ks.idx_sidecar_tree_mut(ns, id) else {
            debug_assert!(false, "a registered index materializes an attach tree");
            return Err(SidecarRebuildReason::GenerationMismatch);
        };
        let mut appended = 0u64;
        for (key, entry_ref) in section.iter() {
            match tree.append(key, entry_ref) {
                Ok(()) => appended += 1,
                Err(AppendError::OutOfOrder) => return Err(SidecarRebuildReason::OutOfOrder),
                Err(AppendError::Map(_)) => return Err(SidecarRebuildReason::Capacity),
            }
        }
        Ok(appended)
    }

    /// Body-class discard (ADR-0078 D6): reset the tree, freeze the
    /// reason. The declaration itself is untouched — the S05 machine
    /// rebuilds it without knowing sidecars exist.
    fn discard(&mut self, ks: &mut Keyspace, at: usize, reason: SidecarRebuildReason) {
        let (ns, id) = (self.records[at].ns, self.records[at].id);
        ks.idx_sidecar_reset(ns, id);
        self.records[at].state = LoadState::Discarded { reason };
    }

    /// A well-framed section whose body failed CRC or canon — counted,
    /// unattributed (ADR-0078 D4; per-index outcomes resolve through
    /// the completeness rules at [`finish_load`](Self::finish_load)).
    pub fn note_damaged(&mut self) {
        self.damaged_sections += 1;
    }

    /// Checkpoint end (the reader's `Done`): discard every stream still
    /// open, then arm `CatchUp` tail-replay maintenance on every
    /// namespace with a loaded index (ADR-0078 D6) — before the first
    /// tail record applies. Idempotent: a no-checkpoint boot reaches
    /// [`commit_ready`](Self::commit_ready) without an ick phase, and
    /// the commit path finishes an untouched loader itself.
    pub fn finish_load(&mut self, ks: &mut Keyspace) {
        if self.finished {
            return;
        }
        self.finished = true;
        for at in 0..self.records.len() {
            if matches!(self.records[at].state, LoadState::Accepting { .. }) {
                self.discard(ks, at, SidecarRebuildReason::Incomplete);
            }
        }
        for at in 0..self.records.len() {
            let rec = &self.records[at];
            if matches!(rec.state, LoadState::Loaded { .. }) && !self.armed.contains(&rec.ns) {
                self.armed.push(rec.ns);
            }
        }
        for &ns in &self.armed {
            ks.idx_set_replay_maintenance(ns, Some(MaintMode::CatchUp));
        }
    }

    /// End of tail replay: loaded trees are caught up — commit them
    /// (converged + cell `Ready`; the ADR-0077 D4 completion protocol,
    /// entered sideways), disarm replay maintenance, and record every
    /// index's decision (L10). Returns the rows for the caller's boot
    /// log; the fold lands on the keyspace for `INFO stats`.
    pub fn commit_ready(mut self, ks: &mut Keyspace) -> Vec<SidecarBootRow> {
        self.finish_load(ks);
        for ns in self.armed.drain(..) {
            ks.idx_set_replay_maintenance(ns, None);
        }
        let mut info =
            SidecarBootInfo { damaged_sections: self.damaged_sections, ..Default::default() };
        let specs: Vec<(NsId, IndexId)> = ks.idx_registry().iter().map(|s| (s.ns, s.id)).collect();
        let mut rows = Vec::with_capacity(specs.len());
        for (ns, id) in specs {
            let decision = match self.records.iter().find(|r| r.ns == ns && r.id == id) {
                Some(LoadRec { state: LoadState::Loaded { total }, .. }) => {
                    SidecarBootDecision::Loaded { entries: *total }
                }
                Some(LoadRec { state: LoadState::Discarded { reason }, .. }) => {
                    SidecarBootDecision::Rebuilt { reason: *reason }
                }
                Some(LoadRec { state: LoadState::Accepting { .. }, .. }) => {
                    unreachable!("finish_load discarded every open stream")
                }
                None => SidecarBootDecision::Rebuilt { reason: SidecarRebuildReason::NoSidecar },
            };
            if let SidecarBootDecision::Loaded { entries } = decision {
                info.loaded += 1;
                info.entries_loaded += entries;
                ks.idx_set_converged(ns, id, true);
                ks.idx_registry_mut()
                    .set_cell_state(id, IndexState::Ready)
                    .expect("a seeded index is Backfilling; Backfilling → Ready is a D3 edge");
            } else {
                info.rebuilt += 1;
            }
            ks.idx_registry_mut().note_sidecar_boot(id, decision);
            let was_ready = ks.idx_registry().was_ready(id).unwrap_or(false);
            rows.push(SidecarBootRow { ns, id, was_ready, decision });
        }
        ks.note_sidecar_totals(info);
        rows
    }
}
