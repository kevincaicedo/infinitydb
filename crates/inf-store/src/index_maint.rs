//! At-mutation index maintenance (M4.5-S04; contract ADR-0072 D3/D5/D6/D7,
//! mechanics ADR-0076): the per-store **attach block** — every live index
//! on this store's namespace with its decoded program and its tree — plus
//! the bracket that makes index updates atomic with the mutation from
//! every observer's view (single-threaded cell, L1).
//!
//! The bracket (ADR-0072 D3, ordering load-bearing): pre-image evaluation
//! into per-store scratch → reservation (plan-then-commit, ADR-0076 D5) →
//! the mutation applies (and stages) → post-image evaluation → entry diff
//! (deduplicated `(typed key, pk)` pairs) → tree ops. Entry ops are
//! globally idempotent (insert-if-absent / remove-if-present on the exact
//! pair — ADR-0072 D5); `Strict` mode's found/fresh checks are
//! `debug_assert`s scoped to **converged** indexes, so backfill
//! interleavings (S05) and sidecar catch-up (S06, `CatchUp`) stay legal
//! by construction.
//!
//! Record deaths outside any command's write set (lazy expiry, the TTL
//! wheel, eviction) run the death hook at the death site — the last
//! moment the dying document's values are readable. The responsibility
//! split (ADR-0076 D4): the death hook no-ops iff this store's bracket is
//! open **and** covers the dying record's key hash; the bracket's diff
//! owns write-set deaths (DEL, GETDEL, overwrites, RENAME's source).
//!
//! The primary-key ref is `hash64(key)` — the store's own key hash
//! (ADR-0076 D2, collision odds disclosed there). The hook is `doc`-gated
//! throughout: a slim build compiles it out entirely (it refuses
//! index-bearing catalogs, ADR-0075 D2.5) — the degenerate-case
//! discipline's strongest form.

#[cfg(feature = "doc")]
use crate::index_key::KeySkip;

/// Hard cap on scratch entries per bracket phase (bounded everything): a
/// mutation whose pre-image would exceed this refuses typed; a post-image
/// that would exceed it degrades the participating indexes (ADR-0076 D5
/// — wrong results are never served either way).
pub const BRACKET_ENTRY_CAP: usize = 65_536;

/// Why the bracket pre-half refused the mutation (typed, mapped to a
/// RESP error at the command layer — ADR-0072 D7.1: nothing changed).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum IdxMaintRefusal {
    /// The plan-then-commit reservation found no headroom (or the
    /// `idx_reserve_refuse` fault point fired).
    Reserve,
    /// Pre-image evaluation overflowed the entry/match caps.
    EntryFlood,
}

impl IdxMaintRefusal {
    /// The RESP error text (one definition — plane and store callers
    /// must not drift).
    pub fn message(self) -> &'static str {
        match self {
            IdxMaintRefusal::Reserve => {
                "ERR index maintenance refused: tree reservation has no headroom"
            }
            IdxMaintRefusal::EntryFlood => {
                "ERR index maintenance refused: entry set exceeds the maintenance cap"
            }
        }
    }
}

/// Assertion strictness for one maintenance pass (ADR-0072 D5: op
/// semantics are identical — only the found/fresh debug checks differ).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum MaintMode {
    /// Live path and rebuild-from-scratch.
    Strict,
    /// S06 sidecar tail-replay: remove-may-miss / insert-may-exist.
    CatchUp,
}

/// Per-index maintenance counters (ADR-0076 D8; skip vocabulary ADR-0074
/// D6). Nothing skips, prunes, or degrades silently (L10).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct IdxCounters {
    /// Sparse-index skips (missing path / type mismatch / null) — the
    /// DynamoDB *feature*, surfaced anyway.
    pub skipped_sparse: u64,
    pub skipped_inexact: u64,
    pub skipped_nan: u64,
    pub skipped_toolong: u64,
    pub maint_inserts: u64,
    pub maint_removes: u64,
    /// Brackets in which the static path-overlap prune skipped both
    /// evaluations for this index (§4.1 arithmetic made observable).
    pub maint_prunes: u64,
    pub degraded_trips: u64,
}

impl IdxCounters {
    pub(crate) fn absorb(&mut self, other: &IdxCounters) {
        self.skipped_sparse += other.skipped_sparse;
        self.skipped_inexact += other.skipped_inexact;
        self.skipped_nan += other.skipped_nan;
        self.skipped_toolong += other.skipped_toolong;
        self.maint_inserts += other.maint_inserts;
        self.maint_removes += other.maint_removes;
        self.maint_prunes += other.maint_prunes;
        self.degraded_trips += other.degraded_trips;
    }

    #[cfg(feature = "doc")]
    fn note_skip(&mut self, skip: KeySkip) {
        match skip {
            KeySkip::Sparse => self.skipped_sparse += 1,
            KeySkip::Inexact => self.skipped_inexact += 1,
            KeySkip::NotANumber => self.skipped_nan += 1,
            KeySkip::TooLong => self.skipped_toolong += 1,
        }
    }
}

#[cfg(feature = "doc")]
mod imp {
    use inf_doc::path::{EvalLimits, eval, resolve};
    use inf_doc::{DocValue, PathProgram, PathStep};
    use inf_foundation::fault;

    use super::{BRACKET_ENTRY_CAP, IdxCounters, IdxMaintRefusal, MaintMode};
    use crate::doc;
    use crate::index_key::{IndexKeyBuf, IndexKeyType, IndexScalar, index_key_encode};
    use crate::index_registry::{INDEXES_PER_NODE_MAX, IndexId, IndexMemory, IndexTree};
    use crate::store::{CellStore, record_at};

    /// One attached index: the maintenance-facing cache of the registry
    /// entry (ADR-0076 D1 — recomputed at DDL transitions, never
    /// consulted for planning) plus this store's tree for it.
    pub(crate) struct AttachedIndex {
        pub(crate) id: IndexId,
        pub(crate) generation: u64,
        key_type: IndexKeyType,
        program: PathProgram,
        /// Precomputed: the program contains a `[*]` step, so its
        /// worst-case entry count per document is the match cap, not 1.
        has_wildcard: bool,
        pub(crate) tree: IndexTree,
        /// The ADR-0072 D7.2 serving veto — cell-local, cleared only by
        /// rebuild. Not a lifecycle state (ADR-0075 D3 is untouched).
        pub(crate) degraded: bool,
        /// This cell reported the index ready (S05 sets it; rebuild and
        /// restart clear it). Scopes the `Strict` found/fresh asserts —
        /// during backfill, misses are legal by design.
        pub(crate) converged: bool,
        pub(crate) counters: IdxCounters,
    }

    /// One scratch entry: an encoded typed key (a range of
    /// `MaintScratch::bytes`) and its pk ref, tagged by attach ordinal.
    #[derive(Copy, Clone, Debug)]
    struct ScratchEntry {
        ord: u16,
        entry_ref: u64,
        off: u32,
        len: u16,
    }

    /// Per-store bracket scratch (pre/post entry sets) plus separate
    /// death-side buffers — a death hook may run *inside* an open
    /// bracket (inline eviction of a non-write-set victim) and must not
    /// clobber the bracket's sets.
    #[derive(Default)]
    struct MaintScratch {
        bytes: Vec<u8>,
        old: Vec<ScratchEntry>,
        new: Vec<ScratchEntry>,
        write_hashes: Vec<u64>,
        /// Bit `ord` set ⇒ the prune skipped both evaluations for that
        /// index this bracket (§4.1; ADR-0076 D6).
        prune_mask: u64,
        /// Bit `ord` set ⇒ the index was evaluated this bracket (the
        /// blast radius of a mid-diff trip).
        participating: u64,
        open: bool,
        key_buf: IndexKeyBuf,
        death_bytes: Vec<u8>,
        death_entries: Vec<ScratchEntry>,
    }

    #[derive(Copy, Clone, PartialEq, Eq)]
    enum Phase {
        Old,
        New,
    }

    /// The attach block: this store's live indexes and their trees, the
    /// bracket scratch, and the one cached branch the zero-index write
    /// path pays (ADR-0072 D2).
    #[derive(Default)]
    pub(crate) struct CellIndexes {
        entries: Vec<AttachedIndex>,
        /// Cached `!entries.is_empty()` — the write path's one branch.
        active: bool,
        /// Replay-time maintenance dial (ADR-0076 D7): `None` (boot
        /// default) — replay does not maintain; the no-sidecar path
        /// rebuilds via S05. S06's sidecar load arms `CatchUp`.
        replay: Option<MaintMode>,
        scratch: MaintScratch,
    }

    impl CellIndexes {
        /// Empty attach block (cell-boot state: no indexes attached).
        pub(crate) fn new() -> CellIndexes {
            CellIndexes::default()
        }

        /// The zero-index fast path: one predictable branch.
        #[inline]
        pub(crate) fn is_active(&self) -> bool {
            self.active
        }

        /// Whether a record death at `hash` must run the death hook
        /// (ADR-0076 D4): active, and not a bracket-covered write-set
        /// key.
        #[inline]
        pub(crate) fn death_hook_wanted(&self, hash: u64) -> bool {
            self.active && !(self.scratch.open && self.scratch.write_hashes.contains(&hash))
        }

        #[inline]
        pub(crate) fn bracket_open(&self) -> bool {
            self.scratch.open
        }

        pub(crate) fn write_set_len(&self) -> usize {
            self.scratch.write_hashes.len()
        }

        /// Installs one index (DDL create, seed, or store
        /// materialization). `program_bytes` passed the ADR-0075 D2.4
        /// gauntlet upstream — failure here is a violated invariant.
        pub(crate) fn install(
            &mut self,
            id: IndexId,
            generation: u64,
            key_type: IndexKeyType,
            program_bytes: &[u8],
        ) {
            debug_assert!(
                self.entries.iter().all(|e| e.id != id),
                "attach install is once per id (sync points, ADR-0076 D1)"
            );
            debug_assert!(self.entries.len() < INDEXES_PER_NODE_MAX);
            let program =
                PathProgram::from_bytes(program_bytes).expect("registry validated the program");
            let has_wildcard = program.steps().any(|s| matches!(s, PathStep::Wild));
            self.entries.push(AttachedIndex {
                id,
                generation,
                key_type,
                program,
                has_wildcard,
                tree: IndexTree::new(key_type),
                degraded: false,
                converged: false,
                counters: IdxCounters::default(),
            });
            self.active = true;
        }

        /// Removes one index and its tree (drop completion).
        pub(crate) fn remove(&mut self, id: IndexId) {
            self.entries.retain(|e| e.id != id);
            self.active = !self.entries.is_empty();
        }

        /// Empties every tree, keeping declarations (`FLUSH*` — the
        /// whole-namespace truncate hook, ADR-0072 D6).
        pub(crate) fn truncate_all(&mut self) {
            for entry in &mut self.entries {
                entry.tree = IndexTree::new(entry.key_type);
            }
        }

        /// Rebuild on this cell (generation already bumped by the
        /// catalog): fresh tree, degradation and convergence cleared.
        pub(crate) fn reset_tree(&mut self, id: IndexId, new_generation: u64) {
            if let Some(entry) = self.entry_mut(id) {
                debug_assert!(new_generation > entry.generation, "rebuild bumps the generation");
                entry.generation = new_generation;
                entry.tree = IndexTree::new(entry.key_type);
                entry.degraded = false;
                entry.converged = false;
            }
        }

        /// S05 flips this when the cell's backfill completes; the
        /// `Strict` found/fresh asserts apply only past it.
        pub(crate) fn set_converged(&mut self, id: IndexId, converged: bool) {
            if let Some(entry) = self.entry_mut(id) {
                entry.converged = converged;
            }
        }

        /// Arms or disarms replay-time maintenance (ADR-0076 D7).
        pub(crate) fn set_replay_maintenance(&mut self, mode: Option<MaintMode>) {
            self.replay = mode;
        }

        pub(crate) fn replay_mode(&self) -> Option<MaintMode> {
            self.replay
        }

        /// Degrades every live index (the replay-refusal backstop:
        /// recovery cannot refuse a record, so an unmaintainable tree
        /// degrades instead — ADR-0076 D7).
        pub(crate) fn degrade_all_live(&mut self) {
            for entry in &mut self.entries {
                if !entry.degraded {
                    entry.degraded = true;
                    entry.counters.degraded_trips += 1;
                }
            }
        }

        pub(crate) fn is_degraded(&self, id: IndexId) -> Option<bool> {
            self.entries.iter().find(|e| e.id == id).map(|e| e.degraded)
        }

        pub(crate) fn tree(&self, id: IndexId) -> Option<&IndexTree> {
            self.entries.iter().find(|e| e.id == id).map(|e| &e.tree)
        }

        pub(crate) fn tree_mut(&mut self, id: IndexId) -> Option<&mut IndexTree> {
            self.entries.iter_mut().find(|e| e.id == id).map(|e| &mut e.tree)
        }

        /// Sidecar-eligible indexes on this store (M4.5-S06, ADR-0078
        /// D1): converged and non-degraded only — a mid-backfill tree
        /// is incomplete in a way no reader can repair, and a degraded
        /// tree's contents are suspect by the veto's own definition.
        /// Rows: `(id, generation, fixed8, entries)`.
        pub(crate) fn sidecar_candidates(&self) -> Vec<(IndexId, u64, bool, u64)> {
            self.entries
                .iter()
                .filter(|e| e.converged && !e.degraded)
                .map(|e| (e.id, e.generation, e.tree.fixed8(), e.tree.len()))
                .collect()
        }

        /// Whether `(id, generation)` is still sidecar-eligible — the
        /// checkpoint driver re-checks between slices and abandons the
        /// stream (no FINAL) on any change (ADR-0078 D1).
        pub(crate) fn sidecar_eligible(&self, id: IndexId, generation: u64) -> bool {
            self.entries
                .iter()
                .any(|e| e.id == id && e.generation == generation && e.converged && !e.degraded)
        }

        /// Empties one tree without touching generation or lifecycle —
        /// the sidecar loader's body-class discard (ADR-0078 D6): the
        /// entries are untrusted, the declaration is not.
        pub(crate) fn reset_tree_contents(&mut self, id: IndexId) {
            if let Some(entry) = self.entry_mut(id) {
                entry.tree = IndexTree::new(entry.key_type);
            }
        }

        pub(crate) fn counters(&self, id: IndexId) -> Option<IdxCounters> {
            self.entries.iter().find(|e| e.id == id).map(|e| e.counters)
        }

        pub(crate) fn counters_fold(&self) -> IdxCounters {
            let mut total = IdxCounters::default();
            for entry in &self.entries {
                total.absorb(&entry.counters);
            }
            total
        }

        /// L5 fold of every attached tree (the S03 `idx_*` domains — the
        /// source moved here by ADR-0076 D1, values unchanged).
        pub(crate) fn memory(&self) -> IndexMemory {
            let mut total = IndexMemory::default();
            for entry in &self.entries {
                total.absorb(entry.tree.memory());
            }
            total
        }

        fn entry_mut(&mut self, id: IndexId) -> Option<&mut AttachedIndex> {
            self.entries.iter_mut().find(|e| e.id == id)
        }

        // ---- the bracket (ADR-0072 D3, per-store halves) ----

        fn begin(&mut self) {
            debug_assert!(!self.scratch.open, "brackets never nest (one command per cell)");
            self.clear_scratch();
        }

        fn clear_scratch(&mut self) {
            self.scratch.open = false;
            self.scratch.bytes.clear();
            self.scratch.old.clear();
            self.scratch.new.clear();
            self.scratch.write_hashes.clear();
            self.scratch.prune_mask = 0;
            self.scratch.participating = 0;
        }

        fn note_write_key(&mut self, hash: u64) {
            self.scratch.write_hashes.push(hash);
        }

        /// Computes and records the prune mask for a path-scoped
        /// mutation on an existing document (ADR-0076 D6): bit set ⇒
        /// provably disjoint ⇒ both evaluations skipped.
        fn set_prune_mask(&mut self, mutation_path: &PathProgram) {
            let mut mask = 0u64;
            for (ord, entry) in self.entries.iter_mut().enumerate() {
                if entry.degraded {
                    continue;
                }
                if programs_disjoint(mutation_path, &entry.program) {
                    mask |= 1 << ord;
                    entry.counters.maint_prunes += 1;
                }
            }
            self.scratch.prune_mask = mask;
        }

        /// Evaluates one document state into the phase's entry set.
        /// `root` is `None` for absent keys and non-document records
        /// (sparse semantics: strings never enter an index).
        fn collect(
            &mut self,
            phase: Phase,
            hash: u64,
            root: Option<DocValue<'_>>,
            max_matches: u32,
        ) -> Result<(), IdxMaintRefusal> {
            let Some(root) = root else { return Ok(()) };
            let limits = EvalLimits { max_matches };
            let CellIndexes { entries, scratch, .. } = self;
            for (ord, entry) in entries.iter_mut().enumerate() {
                if entry.degraded || scratch.prune_mask & (1 << ord) != 0 {
                    continue;
                }
                scratch.participating |= 1 << ord;
                // TooManyMatches surfaces as EntryFlood: the caller
                // refuses pre-apply and degrades post-apply (ADR-0076 D5).
                let Ok(matches) = eval(&entry.program, root, &limits) else {
                    return Err(IdxMaintRefusal::EntryFlood);
                };
                for steps in matches.iter() {
                    let Some(value) = resolve(root, steps) else {
                        debug_assert!(false, "eval yielded an unresolvable location path");
                        continue;
                    };
                    let Some(scalar) = scalar_of(value) else {
                        entry.counters.skipped_sparse += 1;
                        continue;
                    };
                    match index_key_encode(entry.key_type, scalar, &mut scratch.key_buf) {
                        Ok(()) => {
                            let list = match phase {
                                Phase::Old => &mut scratch.old,
                                Phase::New => &mut scratch.new,
                            };
                            if list.len() == BRACKET_ENTRY_CAP {
                                return Err(IdxMaintRefusal::EntryFlood);
                            }
                            let encoded = scratch.key_buf.as_bytes();
                            let off = scratch.bytes.len() as u32;
                            scratch.bytes.extend_from_slice(encoded);
                            list.push(ScratchEntry {
                                ord: ord as u16,
                                entry_ref: hash,
                                off,
                                len: encoded.len() as u16,
                            });
                        }
                        Err(skip) => entry.counters.note_skip(skip),
                    }
                }
            }
            Ok(())
        }

        /// The plan-then-commit reservation (ADR-0072 D7.1 / ADR-0076
        /// D5): arithmetic headroom per participating tree, plus the
        /// `idx_reserve_refuse` fault point. Failure ⇒ the mutation is
        /// refused before anything changes.
        fn reserve(&self, max_matches: u32) -> Result<(), IdxMaintRefusal> {
            if fault::fire(crate::fault::IDX_RESERVE_REFUSE) {
                return Err(IdxMaintRefusal::Reserve);
            }
            let per_key = self.scratch.write_hashes.len().max(1) as u64;
            for (ord, entry) in self.entries.iter().enumerate() {
                if entry.degraded || self.scratch.prune_mask & (1 << ord) != 0 {
                    continue;
                }
                let per_doc = if entry.has_wildcard { u64::from(max_matches) } else { 1 };
                if !entry.tree.insert_headroom(per_doc.saturating_mul(per_key)) {
                    return Err(IdxMaintRefusal::Reserve);
                }
            }
            Ok(())
        }

        fn open(&mut self) {
            self.scratch.open = true;
        }

        /// Marks every participating index degraded (the mid-diff trip's
        /// blast radius is the bracket — ADR-0076 D5). The document
        /// mutation stands; serving refuses until rebuild.
        fn degrade_participating(&mut self) {
            let mask = self.scratch.participating;
            for (ord, entry) in self.entries.iter_mut().enumerate() {
                if mask & (1 << ord) != 0 && !entry.degraded {
                    entry.degraded = true;
                    entry.counters.degraded_trips += 1;
                }
            }
        }

        /// The commit-half tail: sort + dedup both sets, diff, tree ops.
        /// Post-reservation this phase must not fail — a tree refusal or
        /// the planted `idx_apply_trip` lands in the degraded backstop,
        /// never in a wrong result (ADR-0072 D7.2).
        fn apply(&mut self, mode: MaintMode) {
            debug_assert!(self.scratch.open, "apply without an open bracket");
            if fault::fire(crate::fault::IDX_APPLY_TRIP) {
                self.degrade_participating();
                self.clear_scratch();
                return;
            }
            let CellIndexes { entries, scratch, .. } = self;
            let bytes = core::mem::take(&mut scratch.bytes);
            let key = |e: &ScratchEntry| -> (u16, &[u8], u64) {
                (e.ord, &bytes[e.off as usize..e.off as usize + e.len as usize], e.entry_ref)
            };
            scratch.old.sort_unstable_by(|a, b| key(a).cmp(&key(b)));
            scratch.new.sort_unstable_by(|a, b| key(a).cmp(&key(b)));
            let (mut oi, mut ni) = (0usize, 0usize);
            let mut touched = 0u64;
            while oi < scratch.old.len() || ni < scratch.new.len() {
                // Deduplicate multi-match repeats — `(typed key, pk)`
                // pairs collapse per document (§3.3): keep a run's last.
                if oi + 1 < scratch.old.len() && key(&scratch.old[oi]) == key(&scratch.old[oi + 1])
                {
                    oi += 1;
                    continue;
                }
                if ni + 1 < scratch.new.len() && key(&scratch.new[ni]) == key(&scratch.new[ni + 1])
                {
                    ni += 1;
                    continue;
                }
                let verdict = match (scratch.old.get(oi), scratch.new.get(ni)) {
                    (Some(o), Some(n)) => key(o).cmp(&key(n)),
                    (Some(_), None) => core::cmp::Ordering::Less,
                    (None, Some(_)) => core::cmp::Ordering::Greater,
                    (None, None) => unreachable!("loop condition"),
                };
                match verdict {
                    core::cmp::Ordering::Less => {
                        let (ord, k, r) = key(&scratch.old[oi]);
                        let entry = &mut entries[ord as usize];
                        let found = entry.tree.remove(k, r);
                        touched |= 1 << ord;
                        entry.counters.maint_removes += 1;
                        if entry.converged && mode == MaintMode::Strict {
                            debug_assert!(found, "Strict: a converged remove finds its entry");
                        }
                        oi += 1;
                    }
                    core::cmp::Ordering::Greater => {
                        let (ord, k, r) = key(&scratch.new[ni]);
                        let entry = &mut entries[ord as usize];
                        match entry.tree.insert(k, r) {
                            Ok(fresh) => {
                                touched |= 1 << ord;
                                entry.counters.maint_inserts += 1;
                                if entry.converged && mode == MaintMode::Strict {
                                    debug_assert!(fresh, "Strict: a converged insert is fresh");
                                }
                            }
                            Err(_) => {
                                // Reservation said this cannot happen —
                                // the backstop, not a control path.
                                entry.degraded = true;
                                entry.counters.degraded_trips += 1;
                            }
                        }
                        ni += 1;
                    }
                    core::cmp::Ordering::Equal => {
                        oi += 1;
                        ni += 1;
                    }
                }
            }
            // Cardinality reconciliation (ADR-0072 D5): tree len and its
            // attribution agree after every bracket.
            for (ord, entry) in entries.iter().enumerate() {
                if touched & (1 << ord) != 0 {
                    debug_assert_eq!(entry.tree.len(), entry.tree.memory().entries);
                }
            }
            scratch.bytes = bytes;
            self.clear_scratch();
        }

        /// One walked document's inserts for the backfilling index `id`
        /// (M4.5-S05, ADR-0077 D1/D7): evaluate, encode, dedup per
        /// document, insert-if-absent — the walk half of the convergence
        /// argument (the always-on bracket is the other half). Returns
        /// the fresh-insert count; re-emitted documents are no-ops by
        /// idempotence. `Err` means the document cannot enter the index
        /// whole (eval overflow) or the tree has no headroom — the index
        /// is degraded and counted here, and the caller parks the build
        /// (a partial `ready` is unrepresentable).
        ///
        /// Runs only from MAINTAIN slices — never inside a bracket (the
        /// death scratch is shared with the death hook, which is
        /// sequential with the walk on the single-threaded cell).
        ///
        /// # Errors
        /// `Err(())` after degrading the index (ADR-0077 D7).
        pub(crate) fn backfill_insert_doc(
            &mut self,
            id: IndexId,
            hash: u64,
            root: DocValue<'_>,
            max_matches: u32,
        ) -> Result<u32, ()> {
            debug_assert!(!self.scratch.open, "backfill slices never run inside a bracket");
            let limits = EvalLimits { max_matches };
            let CellIndexes { entries, scratch, .. } = self;
            let Some(entry) = entries.iter_mut().find(|e| e.id == id) else {
                debug_assert!(false, "backfill job outlived its attach entry (sync point bug)");
                return Err(());
            };
            debug_assert!(!entry.converged, "a converged index never backfills");
            if entry.degraded {
                return Err(());
            }
            scratch.death_bytes.clear();
            scratch.death_entries.clear();
            let Ok(matches) = eval(&entry.program, root, &limits) else {
                // A pre-declaration document whose matches exceed the cap
                // cannot be indexed whole — degrade rather than serve a
                // partial projection (the death-hook rule, ADR-0077 D7).
                entry.degraded = true;
                entry.counters.degraded_trips += 1;
                return Err(());
            };
            for steps in matches.iter() {
                let Some(value) = resolve(root, steps) else { continue };
                let Some(scalar) = scalar_of(value) else {
                    entry.counters.skipped_sparse += 1;
                    continue;
                };
                match index_key_encode(entry.key_type, scalar, &mut scratch.key_buf) {
                    Ok(()) => {
                        let encoded = scratch.key_buf.as_bytes();
                        let off = scratch.death_bytes.len() as u32;
                        scratch.death_bytes.extend_from_slice(encoded);
                        scratch.death_entries.push(ScratchEntry {
                            ord: 0,
                            entry_ref: hash,
                            off,
                            len: encoded.len() as u16,
                        });
                    }
                    Err(skip) => entry.counters.note_skip(skip),
                }
            }
            let bytes = &scratch.death_bytes;
            let key_of = |e: &ScratchEntry| -> &[u8] {
                &bytes[e.off as usize..e.off as usize + e.len as usize]
            };
            scratch.death_entries.sort_unstable_by(|a, b| key_of(a).cmp(key_of(b)));
            let headroom_wanted = scratch.death_entries.len() as u64;
            if fault::fire(crate::fault::IDX_BACKFILL_TRIP)
                || !entry.tree.insert_headroom(headroom_wanted)
            {
                // The corpus outgrew the tree's structural limits (or the
                // planted trip): honest refusal, never a partial ready.
                entry.degraded = true;
                entry.counters.degraded_trips += 1;
                return Err(());
            }
            let mut fresh = 0u32;
            let mut previous: Option<&ScratchEntry> = None;
            for e in &scratch.death_entries {
                // One hash for the whole document ⇒ dedup is byte
                // equality on the sorted run (the death-hook pattern).
                if previous.is_some_and(|p| key_of(p) == key_of(e)) {
                    continue;
                }
                previous = Some(e);
                match entry.tree.insert(key_of(e), hash) {
                    Ok(true) => fresh += 1,
                    Ok(false) => {}
                    Err(_) => {
                        // Headroom said this cannot happen — the backstop.
                        entry.degraded = true;
                        entry.counters.degraded_trips += 1;
                        return Err(());
                    }
                }
            }
            Ok(fresh)
        }

        /// The record-death hook (ADR-0072 D6): evaluate the dying
        /// document's entries and remove them — idempotent, infallible
        /// (removals never allocate). Runs at `free_record`, the wheel
        /// reap, and every eviction shape; the caller already checked
        /// [`death_hook_wanted`](Self::death_hook_wanted).
        pub(crate) fn remove_doc_entries(
            &mut self,
            hash: u64,
            root: DocValue<'_>,
            max_matches: u32,
        ) {
            let limits = EvalLimits { max_matches };
            let CellIndexes { entries, scratch, .. } = self;
            for entry in entries.iter_mut() {
                if entry.degraded {
                    continue;
                }
                scratch.death_bytes.clear();
                scratch.death_entries.clear();
                let Ok(matches) = eval(&entry.program, root, &limits) else {
                    // A doc whose eval exceeds caps cannot have been
                    // inserted whole; degrade rather than leak.
                    entry.degraded = true;
                    entry.counters.degraded_trips += 1;
                    continue;
                };
                for steps in matches.iter() {
                    let Some(value) = resolve(root, steps) else { continue };
                    let Some(scalar) = scalar_of(value) else {
                        entry.counters.skipped_sparse += 1;
                        continue;
                    };
                    match index_key_encode(entry.key_type, scalar, &mut scratch.key_buf) {
                        Ok(()) => {
                            let encoded = scratch.key_buf.as_bytes();
                            let off = scratch.death_bytes.len() as u32;
                            scratch.death_bytes.extend_from_slice(encoded);
                            scratch.death_entries.push(ScratchEntry {
                                ord: 0,
                                entry_ref: hash,
                                off,
                                len: encoded.len() as u16,
                            });
                        }
                        Err(skip) => entry.counters.note_skip(skip),
                    }
                }
                let bytes = &scratch.death_bytes;
                let key_of = |e: &ScratchEntry| -> &[u8] {
                    &bytes[e.off as usize..e.off as usize + e.len as usize]
                };
                scratch.death_entries.sort_unstable_by(|a, b| key_of(a).cmp(key_of(b)));
                let mut previous: Option<&ScratchEntry> = None;
                for e in &scratch.death_entries {
                    // The ref is one hash for the whole document, so
                    // dedup is byte equality on the sorted run.
                    if previous.is_some_and(|p| key_of(p) == key_of(e)) {
                        continue;
                    }
                    previous = Some(e);
                    let found = entry.tree.remove(key_of(e), hash);
                    entry.counters.maint_removes += 1;
                    if entry.converged {
                        debug_assert!(found, "Strict: a converged death removal finds its entry");
                    }
                }
            }
        }
    }

    /// The ADR-0076 D6 disjointness rule, conservative by construction:
    /// only a proven per-step mismatch prunes; `Wild`, `Other`, mixed
    /// step kinds, and either chain ending all mean "may overlap".
    pub(crate) fn programs_disjoint(mutation: &PathProgram, index: &PathProgram) -> bool {
        let mut m = mutation.steps();
        let mut i = index.steps();
        loop {
            let (Some(ms), Some(is)) = (m.next(), i.next()) else {
                // A chain ended: prefix relationship — overlap.
                return false;
            };
            match (ms, is) {
                (PathStep::Child(a), PathStep::Child(b)) if a != b => return true,
                (PathStep::Index(a), PathStep::Index(b)) if a != b => return true,
                (PathStep::Other, _) | (_, PathStep::Other) => return false,
                // Equal steps, wildcards, and mixed kinds: keep walking.
                _ => {}
            }
        }
    }

    /// Scalar view of a matched value (containers are never indexable —
    /// sparse semantics, plan S04).
    fn scalar_of(value: DocValue<'_>) -> Option<IndexScalar<'_>> {
        Some(match value {
            DocValue::Null => IndexScalar::Null,
            DocValue::Bool(b) => IndexScalar::Bool(b),
            DocValue::I64(v) => IndexScalar::I64(v),
            DocValue::F64(f) => IndexScalar::F64(f),
            DocValue::Str(s) => IndexScalar::Utf8(s.to_str()),
            DocValue::Obj(_) | DocValue::Arr(_) => return None,
        })
    }

    // ---- CellStore-level bracket wrappers (the split borrows live here) ----

    impl CellStore {
        /// The bracket pre-half (ADR-0072 D3 steps 1–2): pre-image
        /// evaluation of every write-set key into per-store scratch,
        /// then the reservation. `Err` ⇒ typed refusal, nothing changed.
        ///
        /// # Errors
        /// [`IdxMaintRefusal`] — the caller maps it to the RESP error.
        pub(crate) fn idx_bracket_begin(
            &mut self,
            keys: &[&[u8]],
            mutation_path: Option<&PathProgram>,
        ) -> Result<(), IdxMaintRefusal> {
            if !self.idx.is_active() {
                return Ok(());
            }
            let max_matches = self.cfg.doc_max_path_matches;
            self.idx.begin();
            // The prune applies only to a single-key path mutation whose
            // document exists — creation, deletion, and multi-key
            // commands evaluate in full (ADR-0076 D6).
            let mut prune_candidate = match (keys.len(), mutation_path) {
                (1, Some(p)) if !p.is_root() => Some(p),
                _ => None,
            };
            let mut outcome = Ok(());
            for key in keys {
                let hash = self.hash_key(key);
                self.idx.note_write_key(hash);
                let CellStore { arena, index, docs, idx, .. } = self;
                let root = peek_doc_root(arena, index, docs, key, hash);
                if root.is_some()
                    && let Some(p) = prune_candidate.take()
                {
                    idx.set_prune_mask(p);
                }
                if let Err(refusal) = idx.collect(Phase::Old, hash, root, max_matches) {
                    outcome = Err(refusal);
                    break;
                }
            }
            if outcome.is_ok() {
                outcome = self.idx.reserve(max_matches);
            }
            match outcome {
                Ok(()) => {
                    self.idx.open();
                    Ok(())
                }
                Err(refusal) => {
                    self.idx.clear_scratch();
                    Err(refusal)
                }
            }
        }

        /// The bracket commit-half (ADR-0072 D3 steps 4–5): post-image
        /// evaluation, dedup diff, tree ops. Runs after the mutation
        /// applied (and, on durable namespaces, staged); infallible —
        /// failures land in the degraded backstop.
        pub(crate) fn idx_bracket_commit(&mut self, keys: &[&[u8]], mode: MaintMode) {
            if !self.idx.bracket_open() {
                return;
            }
            let max_matches = self.cfg.doc_max_path_matches;
            debug_assert_eq!(keys.len(), self.idx.write_set_len());
            let mut flooded = false;
            for key in keys {
                let hash = self.hash_key(key);
                let CellStore { arena, index, docs, idx, .. } = self;
                let root = peek_doc_root(arena, index, docs, key, hash);
                debug_assert!(
                    idx.scratch.prune_mask == 0 || root.is_some(),
                    "a pruned mutation cannot delete its document (ADR-0076 D6)"
                );
                if idx.collect(Phase::New, hash, root, max_matches).is_err() {
                    flooded = true;
                    break;
                }
            }
            if flooded {
                // Post-apply overflow: the mutation stands, the
                // participating indexes degrade (ADR-0076 D5).
                self.idx.degrade_participating();
                self.idx.clear_scratch();
                return;
            }
            self.idx.apply(mode);
        }

        /// Aborts an open bracket without applying (callers that refuse
        /// between the halves).
        pub(crate) fn idx_bracket_abort(&mut self) {
            if self.idx.bracket_open() {
                self.idx.clear_scratch();
            }
        }

        /// The replay arm's pre-half (ADR-0072 D4 / ADR-0076 D7):
        /// `Some(mode)` iff the replay dial is armed and the bracket
        /// opened. Recovery can never refuse a record, so a pre-half
        /// refusal degrades every live index instead (the trees cannot
        /// be maintained truthfully) and replay proceeds unmaintained.
        pub(crate) fn idx_replay_begin(&mut self, key: &[u8]) -> Option<MaintMode> {
            let mode = self.idx.replay_mode()?;
            if !self.idx.is_active() {
                return None;
            }
            match self.idx_bracket_begin(&[key], None) {
                Ok(()) => Some(mode),
                Err(_) => {
                    self.idx.degrade_all_live();
                    None
                }
            }
        }
    }

    /// Peek a key's document root **without** read side effects: no
    /// lazy-expiry reap, no access-tracking touch, and expired-but-
    /// unreaped records included — the bracket's pre-image must see the
    /// physical record whose entries are in the trees (ADR-0076 D4).
    fn peek_doc_root<'a>(
        arena: &'a inf_alloc::Arena,
        index: &crate::index::Index,
        docs: &'a doc::DocStore,
        key: &[u8],
        hash: u64,
    ) -> Option<DocValue<'a>> {
        let addr = index.find(hash, |addr| record_at(arena, addr).key() == key)?;
        let len = record_at(arena, addr).encoded_len();
        doc::doc_root_at(arena, docs, addr, len)
    }
}

#[cfg(feature = "doc")]
pub(crate) use imp::CellIndexes;
#[cfg(all(test, feature = "doc"))]
pub(crate) use imp::programs_disjoint;

/// Slim-build stub: no `doc`, no documents, no maintainable projections
/// (a slim build refuses index-bearing catalogs — ADR-0075 D2.5). The
/// inlined `false` folds every call site away, so slim binaries carry
/// zero added instructions (the S04 degenerate-case AC).
#[cfg(not(feature = "doc"))]
#[derive(Default)]
pub(crate) struct CellIndexes;

#[cfg(not(feature = "doc"))]
impl CellIndexes {
    // Unit-struct value, but constructed like the doc-lane type so the
    // one call site reads identically under both cfgs.
    #[inline]
    pub(crate) fn new() -> CellIndexes {
        CellIndexes
    }

    #[inline]
    pub(crate) fn memory(&self) -> crate::index_registry::IndexMemory {
        crate::index_registry::IndexMemory::default()
    }

    #[inline]
    pub(crate) fn counters_fold(&self) -> IdxCounters {
        IdxCounters::default()
    }

    #[inline]
    pub(crate) fn truncate_all(&mut self) {}
}

#[cfg(all(test, feature = "doc"))]
mod tests {
    use super::programs_disjoint;
    use inf_doc::PathProgram;
    use inf_doc::path::compile;

    fn program(text: &str) -> PathProgram {
        compile(text.as_bytes()).expect("valid path")
    }

    /// The ADR-0076 D6 rule, case by case: only a proven per-step
    /// mismatch prunes; everything ambiguous overlaps. Wrong-side errors
    /// (claiming disjoint when a mutation can touch the indexed path)
    /// are wrong *results*, so the conservative side of every case is
    /// pinned here.
    #[test]
    fn prune_disjointness_is_conservative() {
        let disjoint = [
            ("$.a", "$.b"),
            ("$.a.b", "$.a.c"),
            ("$.a[0]", "$.a[1]"),
            ("$.items[2].price", "$.items[3].price"),
            // Mismatch decided before the chains diverge in length.
            ("$.a.b.c", "$.b"),
        ];
        for (mutation, index) in disjoint {
            assert!(
                programs_disjoint(&program(mutation), &program(index)),
                "{mutation} vs {index} must prune"
            );
        }
        let overlapping = [
            ("$.a", "$.a"),
            // Prefix relationships in both directions.
            ("$.a", "$.a.b"),
            ("$.a.b", "$.a"),
            // Wildcards may match anything at their position.
            ("$.tags[3]", "$.tags[*]"),
            ("$.a[*].x", "$.a[0].x"),
            // Outside-fence mutation selectors are `Other` ⇒ overlap.
            ("$..price", "$.price"),
            ("$.a[1:3]", "$.a[0]"),
            ("$['a','b']", "$.a"),
            // Mixed step kinds stay conservative (ADR-0076 D6).
            ("$.a[0]", "$.a.b"),
            // Root mutation (no steps) is a prefix of everything.
            ("$", "$.price"),
        ];
        for (mutation, index) in overlapping {
            assert!(
                !programs_disjoint(&program(mutation), &program(index)),
                "{mutation} vs {index} must NOT prune"
            );
        }
    }
}
