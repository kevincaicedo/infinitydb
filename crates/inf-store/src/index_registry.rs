//! Per-cell index registry (M4.5-S03, ADR-0075; registration surface
//! ADR-0072 D2): the §3.2-frozen entry `{index id, generation, ns, path
//! program bytes, key type, state}`, one [`OrderedMap`] tree per entry
//! (scheme chosen by [`IndexKeyType::fixed8`]), the lifecycle state
//! machine whose transitions are the only way planning sees an index,
//! and the `idx_tree_bytes`/`idx_slack_bytes` memory domains (L5).
//!
//! Two state scopes share one vocabulary ([`IndexState`]): the
//! **catalog** state (replicated to every cell by the DDL fan; planning
//! reads it — never per-cell state, or the partially-ready-fleet query
//! §3.1 forbids comes back) and this cell's **machine** state (its own
//! backfill progress, published to the control-plane `IndexBoard`).
//! The registry is never probed on the data plane: S04's mutation path
//! reads a cached per-namespace flag recomputed here at DDL transitions
//! (ADR-0072 D2), and everything else is DDL/MAINTAIN-rate work.

use inf_log::NsId;

use crate::index_key::{INDEX_KEY_ENCODING_VERSION, IndexKeyType};
use crate::ns::valid_ns_name;
use crate::ordered::{
    AppendError, Fixed8, OrderedCursor, OrderedMap, OrderedMapError, OrderedMapMemory, VarKey,
};

/// First allocatable index id; `IndexId(0)` is reserved (the board's
/// "no report" null — ADR-0075 D1).
pub const FIRST_INDEX_ID: u32 = 1;

/// First generation; 0 is reserved like the id null.
pub const FIRST_INDEX_GENERATION: u64 = 1;

/// Live index declarations per node (ADR-0075 D1): a typed refusal at
/// create, the readiness board's fixed size, and a documented limit —
/// 3× the DynamoDB per-table GSI cap.
pub const INDEXES_PER_NODE_MAX: usize = 64;

/// Cap on stored path-program bytes (decode strictness; real programs
/// are far smaller — the M3 parser caps text well below this).
pub const INDEX_PROGRAM_MAX: usize = 4096;

/// Node-unique index id, allocated once from the catalog counter and
/// never reused (the ADR-0015 D2 namespace-id rule, applied verbatim).
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct IndexId(pub u32);

/// Lifecycle state (§3.2 freeze; transitions per ADR-0075 D3). Used at
/// both scopes — see the module docs.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum IndexState {
    /// Registered; distribution/backfill not yet started.
    Declared,
    /// Contents building (initial backfill or post-restart rebuild).
    Backfilling,
    /// Complete and servable — the only state planning accepts.
    Ready,
    /// Teardown in progress; no new statement plans against it.
    Dropping,
}

impl IndexState {
    pub fn name(self) -> &'static str {
        match self {
            IndexState::Declared => "declared",
            IndexState::Backfilling => "backfilling",
            IndexState::Ready => "ready",
            IndexState::Dropping => "dropping",
        }
    }

    /// The D3 edge set. `Ready → Backfilling` is legal only as a rebuild
    /// (the caller bumps the generation first — asserted in
    /// [`IndexRegistry::set_catalog_state`]).
    fn can_transition(self, to: IndexState) -> bool {
        use IndexState::{Backfilling, Declared, Dropping, Ready};
        matches!(
            (self, to),
            (Declared, Backfilling)
                | (Backfilling, Ready)
                | (Ready, Backfilling)
                | (Declared | Backfilling | Ready, Dropping)
        )
    }
}

/// One index registry entry — the §3.2 freeze row as data. `state` is
/// the **catalog** state; the per-cell machine state lives beside it in
/// the registry, never in this replicated record.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct IndexSpec {
    pub id: IndexId,
    /// Bumps on create and rebuild under the same name (never at boot —
    /// ADR-0075 D4; sidecars bind it exactly, ADR-0073 D5.1).
    pub generation: u64,
    /// The indexed namespace; ids `0..16` are the default dbs.
    pub ns: NsId,
    /// Unique per `(ns, name)`; the namespace-name charset.
    pub name: Vec<u8>,
    /// M3 path-program bytecode v1, opaque at rest; validated at the
    /// trust boundaries (catalog decode + DDL) by
    /// [`validate_index_program`].
    pub program: Vec<u8>,
    pub key_type: IndexKeyType,
    pub state: IndexState,
}

/// Typed registry failures (the command layer maps them to replies).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum IndexError {
    /// Duplicate `(ns, name)` — or a duplicate id, which is a caller bug
    /// (ids are allocated once) and additionally trips a debug assert.
    Exists,
    Unknown,
    /// The target namespace is not registered (named ids only — the
    /// default dbs `0..16` are implicit; checked by the keyspace layer).
    UnknownNamespace(u32),
    InvalidName,
    /// The [`INDEXES_PER_NODE_MAX`] cap (documented limit, ADR-0075 D1).
    TooManyIndexes,
    /// A D3-illegal lifecycle edge — explicit, never a silent no-op.
    InvalidTransition {
        from: IndexState,
        to: IndexState,
    },
    /// Path-program bytes failed the gauntlet; the reason names it.
    InvalidProgram(&'static str),
    /// `INF.IDX` on a tiered namespace (ADR-0072 D8a: string-only —
    /// the doc-tiering deferral; checked by the keyspace layer).
    TierRefusesIndexes,
}

/// Why a cursor/compile binding failed (ADR-0075 D7) — S09's compile
/// check and S11's cursor decode consult this; a wrong page is
/// unrepresentable through the seam.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum IndexBindError {
    /// No such `(ns, id)` — dropped, or never existed.
    UnknownIndex,
    /// The index was rebuilt or re-created since the cursor was issued.
    StaleGeneration { bound: u64, current: u64 },
    /// The index exists but is not servable.
    NotReady(IndexState),
}

/// Why a sidecar load ended in a rebuild (M4.5-S06, ADR-0078 D6) —
/// recorded per index per boot (L10); per-index rendering rides S10's
/// `INF.IDX LIST` beside the other per-index rows.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SidecarRebuildReason {
    /// The checkpoint carried no sections for this index (never
    /// converged pre-crash, or no checkpoint at all).
    NoSidecar,
    /// Sections named a declaration the seeded catalog does not hold
    /// (dropped before the crash). Never surfaces on a row — the id no
    /// longer exists to carry one — but the state machine still
    /// swallows the stream's remaining sections.
    StaleDeclaration,
    /// Sections bound a generation the seeded catalog does not hold
    /// (rebuilt or re-created since the checkpoint — ADR-0073 D5.1).
    GenerationMismatch,
    /// Sections bound a key-encoding version this binary does not
    /// produce (ADR-0073 D5.2).
    EncodingVersion,
    /// The section's key scheme disagrees with the declared key type.
    SchemeMismatch,
    /// A section's `entries_before` broke the ordinal chain (writer
    /// abandonment or unattributed damage resolving here — ADR-0078
    /// D4/D6).
    NonContiguous,
    /// A pair failed the cross-section strictly-ascending canon.
    OutOfOrder,
    /// The tree refused an append (structural capacity).
    Capacity,
    /// The stream ended without a FINAL section.
    Incomplete,
    /// A section arrived after the FINAL marker.
    AfterFinal,
    /// The FINAL total disagreed with what loaded.
    TotalMismatch,
}

impl SidecarRebuildReason {
    pub fn name(self) -> &'static str {
        match self {
            SidecarRebuildReason::NoSidecar => "no-sidecar",
            SidecarRebuildReason::StaleDeclaration => "stale-declaration",
            SidecarRebuildReason::GenerationMismatch => "generation-mismatch",
            SidecarRebuildReason::EncodingVersion => "encoding-version",
            SidecarRebuildReason::SchemeMismatch => "scheme-mismatch",
            SidecarRebuildReason::NonContiguous => "non-contiguous",
            SidecarRebuildReason::OutOfOrder => "out-of-order",
            SidecarRebuildReason::Capacity => "capacity",
            SidecarRebuildReason::Incomplete => "incomplete",
            SidecarRebuildReason::AfterFinal => "after-final",
            SidecarRebuildReason::TotalMismatch => "total-mismatch",
        }
    }
}

/// The per-boot rebuild-vs-load decision (ADR-0078 D6 — the plan's S06
/// "recorded per index per boot" requirement as data).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SidecarBootDecision {
    /// The sidecar loaded whole and tail catch-up will finish the job.
    Loaded { entries: u64 },
    /// The projection rebuilds from scratch (the S05 machine).
    Rebuilt { reason: SidecarRebuildReason },
}

/// One index's tree: the projection contents, scheme-monomorphized per
/// the declared key type (ADR-0074 D1 — i64/f64/bool keys are exactly
/// 8 bytes and ride `Fixed8`; utf8 rides `VarKey`). Custody: trees live
/// in the owning store's attach block (`index_maint`, ADR-0076 D1 —
/// amending the D1 wording here); the registry stays the catalog
/// authority.
pub enum IndexTree {
    Fixed8(OrderedMap<Fixed8>),
    Var(OrderedMap<VarKey>),
}

impl IndexTree {
    #[cfg(feature = "doc")]
    pub(crate) fn new(key_type: IndexKeyType) -> IndexTree {
        if key_type.fixed8() {
            IndexTree::Fixed8(OrderedMap::new())
        } else {
            IndexTree::Var(OrderedMap::new())
        }
    }

    pub fn len(&self) -> u64 {
        match self {
            IndexTree::Fixed8(map) => map.len(),
            IndexTree::Var(map) => map.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// L5 snapshot (O(1) — the tree maintains its own attribution).
    pub fn memory(&self) -> OrderedMapMemory {
        match self {
            IndexTree::Fixed8(map) => map.memory(),
            IndexTree::Var(map) => map.memory(),
        }
    }

    /// Insert-if-absent on the `(typed key, pk)` pair (ADR-0072 D5
    /// idempotent entry ops — one semantics everywhere).
    ///
    /// # Errors
    /// Capacity refusals ([`OrderedMapError`]) — the tree is unchanged;
    /// S04's plan-then-commit reservation turns them into typed refusals.
    pub fn insert(&mut self, key: &[u8], entry_ref: u64) -> Result<bool, OrderedMapError> {
        match self {
            IndexTree::Fixed8(map) => map.insert(key, entry_ref),
            IndexTree::Var(map) => map.insert(key, entry_ref),
        }
    }

    /// Whether `additional` inserts are guaranteed inside the tree's
    /// structural limits — the S04 reservation's arithmetic headroom
    /// check (ADR-0076 D5): no allocation, conservative.
    pub fn insert_headroom(&self, additional: u64) -> bool {
        match self {
            IndexTree::Fixed8(map) => map.insert_headroom(additional),
            IndexTree::Var(map) => map.insert_headroom(additional),
        }
    }

    /// Append a strictly-ascending pair — the S06 sidecar load path
    /// (ADR-0078 D5). Same semantics as `insert` on ascending input;
    /// out-of-order input refuses typed (the loader's bytes come from
    /// disk — a body-class discard, never a panic).
    ///
    /// # Errors
    /// [`AppendError`] — the tree is unchanged.
    pub fn append(&mut self, key: &[u8], entry_ref: u64) -> Result<(), AppendError> {
        match self {
            IndexTree::Fixed8(map) => map.append(key, entry_ref),
            IndexTree::Var(map) => map.append(key, entry_ref),
        }
    }

    /// True when the tree keys ride the `Fixed8` scheme (the sidecar
    /// meta's `key_scheme` byte — ADR-0078 D2).
    pub fn fixed8(&self) -> bool {
        matches!(self, IndexTree::Fixed8(_))
    }

    /// Remove-if-present on the exact pair; `true` when it was present.
    pub fn remove(&mut self, key: &[u8], entry_ref: u64) -> bool {
        match self {
            IndexTree::Fixed8(map) => map.remove(key, entry_ref),
            IndexTree::Var(map) => map.remove(key, entry_ref),
        }
    }

    pub fn contains(&self, key: &[u8], entry_ref: u64) -> bool {
        match self {
            IndexTree::Fixed8(map) => map.contains(key, entry_ref),
            IndexTree::Var(map) => map.contains(key, entry_ref),
        }
    }

    /// One cursor step (re-seek semantics — the S01 freeze); `None`
    /// past the end.
    pub fn cursor_next<'c>(&self, cursor: &'c mut OrderedCursor) -> Option<(&'c [u8], u64)> {
        match self {
            IndexTree::Fixed8(map) => cursor.next(map),
            IndexTree::Var(map) => cursor.next(map),
        }
    }
}

/// The per-namespace `idx_*` byte fold (L5 domains, plan-named).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct IndexMemory {
    /// Reserved tree bytes (pools + heap, capacity — the RSS side).
    pub idx_tree_bytes: u64,
    /// Slack inside `idx_tree_bytes` (diagnostic overlay, like
    /// `doc_slack_bytes`).
    pub idx_slack_bytes: u64,
    /// Live entries across the folded trees.
    pub entries: u64,
}

impl IndexMemory {
    #[cfg(feature = "doc")]
    pub(crate) fn absorb(&mut self, m: OrderedMapMemory) {
        self.idx_tree_bytes += m.total_bytes();
        self.idx_slack_bytes += m.slack_bytes;
        self.entries += m.entries;
    }
}

struct RegEntry {
    spec: IndexSpec,
    /// This cell's machine state (backfill progress) — never replicated,
    /// never consulted by planning.
    cell_state: IndexState,
    /// ADR-0075 D4 rebuild-class hint: the pre-crash persisted state was
    /// `ready`. S06 reads it for *severity*, not outcome (ADR-0078 D6):
    /// a was-ready index that ends rebuilt was serving before the crash
    /// and now is not — that downgrade is named loudly in the boot log.
    was_ready: bool,
    /// This boot's rebuild-vs-load decision (M4.5-S06, ADR-0078 D6).
    /// `None` until recovery decides (and forever on a fresh cell).
    sidecar_boot: Option<SidecarBootDecision>,
}

/// The per-cell registry (ADR-0072 D2): every declaration replicated,
/// this cell's trees and machine states beside them. Linear scans
/// everywhere — the entry count is ≤ [`INDEXES_PER_NODE_MAX`] and no
/// lookup is on the data plane.
#[derive(Default)]
pub struct IndexRegistry {
    entries: Vec<RegEntry>,
    /// Namespaces with ≥ 1 live entry, recomputed at DDL transitions —
    /// the source the store-side cached flags (S04) refresh from.
    ns_flagged: Vec<NsId>,
    /// Monotone catalog epoch (ADR-0080 D5): bumps on every mutation
    /// that can change what a statement compiles to — create, remove,
    /// catalog-state flip, rebuild. Cell-machine state changes do not
    /// bump it (planning reads catalog state only, ADR-0075 D3). The
    /// S09 statement cache guards entries with it.
    epoch: u64,
}

impl IndexRegistry {
    /// Registers a declaration and creates its empty tree. `was_ready`
    /// marks a boot-seeded entry whose pre-crash state was `ready`
    /// (ADR-0075 D4); live DDL passes `false`.
    ///
    /// # Errors
    /// `InvalidName` / `InvalidProgram` / `TooManyIndexes` / `Exists` —
    /// nothing registers on any refusal.
    pub fn create(&mut self, spec: IndexSpec, was_ready: bool) -> Result<(), IndexError> {
        if !valid_ns_name(&spec.name) {
            return Err(IndexError::InvalidName);
        }
        if spec.program.is_empty() || spec.program.len() > INDEX_PROGRAM_MAX {
            return Err(IndexError::InvalidProgram("path program length out of range"));
        }
        if self.entries.len() >= INDEXES_PER_NODE_MAX {
            return Err(IndexError::TooManyIndexes);
        }
        if self.get(spec.ns, &spec.name).is_some() {
            return Err(IndexError::Exists);
        }
        if self.get_by_id(spec.id).is_some() {
            debug_assert!(false, "index ids are allocated once and never reused");
            return Err(IndexError::Exists);
        }
        debug_assert!(spec.id.0 >= FIRST_INDEX_ID, "id 0 is reserved");
        debug_assert!(spec.generation >= FIRST_INDEX_GENERATION, "generation 0 is reserved");
        let cell_state = spec.state;
        self.entries.push(RegEntry { spec, cell_state, was_ready, sidecar_boot: None });
        self.recompute_flags();
        self.epoch += 1;
        Ok(())
    }

    /// Removes the entry and its tree (drop completion / `dropping`
    /// resumed at boot). The id stays retired forever.
    pub fn remove(&mut self, id: IndexId) -> Result<IndexSpec, IndexError> {
        let at = self.entries.iter().position(|e| e.spec.id == id).ok_or(IndexError::Unknown)?;
        let entry = self.entries.remove(at);
        self.recompute_flags();
        self.epoch += 1;
        Ok(entry.spec)
    }

    /// Drops every entry of `ns` (namespace drop takes its indexes and
    /// their trees with it); returns how many were removed.
    pub fn remove_ns(&mut self, ns: NsId) -> usize {
        let before = self.entries.len();
        self.entries.retain(|e| e.spec.ns != ns);
        let removed = before - self.entries.len();
        if removed > 0 {
            self.recompute_flags();
            self.epoch += 1;
        }
        removed
    }

    /// One catalog-state transition, D3-checked. A rebuild
    /// (`Ready → Backfilling`) must arrive with its generation already
    /// bumped by the caller — asserted, because a same-generation rebuild
    /// would let stale cursors and sidecars bind the new contents.
    ///
    /// # Errors
    /// `Unknown` / the explicit `InvalidTransition`.
    pub fn set_catalog_state(&mut self, id: IndexId, to: IndexState) -> Result<(), IndexError> {
        let entry = self.entry_mut(id).ok_or(IndexError::Unknown)?;
        let from = entry.spec.state;
        if !from.can_transition(to) {
            return Err(IndexError::InvalidTransition { from, to });
        }
        entry.spec.state = to;
        self.epoch += 1;
        Ok(())
    }

    /// Rebuild (S05/S10 consume; the surface is D3's): `Ready →
    /// Backfilling` with the fresh generation from the catalog allocator.
    /// The keyspace resets the owning store's attach tree in the same
    /// transition (ADR-0076 D1 — custody lives with the store).
    ///
    /// # Errors
    /// `Unknown` / `InvalidTransition` (only `Ready` rebuilds).
    pub fn rebuild(&mut self, id: IndexId, new_generation: u64) -> Result<(), IndexError> {
        let entry = self.entry_mut(id).ok_or(IndexError::Unknown)?;
        let from = entry.spec.state;
        if from != IndexState::Ready {
            return Err(IndexError::InvalidTransition { from, to: IndexState::Backfilling });
        }
        debug_assert!(new_generation > entry.spec.generation, "rebuild bumps the generation");
        entry.spec.generation = new_generation;
        entry.spec.state = IndexState::Backfilling;
        entry.cell_state = IndexState::Backfilling;
        self.epoch += 1;
        Ok(())
    }

    /// One cell-machine transition (this cell's backfill progress),
    /// same D3 edge set.
    ///
    /// # Errors
    /// `Unknown` / the explicit `InvalidTransition`.
    pub fn set_cell_state(&mut self, id: IndexId, to: IndexState) -> Result<(), IndexError> {
        let entry = self.entry_mut(id).ok_or(IndexError::Unknown)?;
        let from = entry.cell_state;
        if !from.can_transition(to) {
            return Err(IndexError::InvalidTransition { from, to });
        }
        entry.cell_state = to;
        Ok(())
    }

    /// The ADR-0075 D7 binding gate — the one function compile checks
    /// and cursor decodes consult. Catalog state only (§3.1).
    ///
    /// # Errors
    /// Typed [`IndexBindError`] — a wrong page is unrepresentable here.
    pub fn validate_binding(
        &self,
        ns: NsId,
        id: IndexId,
        generation: u64,
    ) -> Result<(), IndexBindError> {
        let entry = self
            .entries
            .iter()
            .find(|e| e.spec.id == id && e.spec.ns == ns)
            .ok_or(IndexBindError::UnknownIndex)?;
        if entry.spec.generation != generation {
            return Err(IndexBindError::StaleGeneration {
                bound: generation,
                current: entry.spec.generation,
            });
        }
        if entry.spec.state != IndexState::Ready {
            return Err(IndexBindError::NotReady(entry.spec.state));
        }
        Ok(())
    }

    pub fn get(&self, ns: NsId, name: &[u8]) -> Option<&IndexSpec> {
        self.entries.iter().find(|e| e.spec.ns == ns && e.spec.name == name).map(|e| &e.spec)
    }

    pub fn get_by_id(&self, id: IndexId) -> Option<&IndexSpec> {
        self.entries.iter().find(|e| e.spec.id == id).map(|e| &e.spec)
    }

    /// This cell's machine state for `id`.
    pub fn cell_state(&self, id: IndexId) -> Option<IndexState> {
        self.entries.iter().find(|e| e.spec.id == id).map(|e| e.cell_state)
    }

    /// The D4 sidecar-eligibility hint (S06 reads it at boot).
    pub fn was_ready(&self, id: IndexId) -> Option<bool> {
        self.entries.iter().find(|e| e.spec.id == id).map(|e| e.was_ready)
    }

    /// Records this boot's rebuild-vs-load decision for `id` (M4.5-S06,
    /// ADR-0078 D6). Unknown ids are ignored — a stale sidecar's
    /// declaration no longer exists to carry a record.
    pub fn note_sidecar_boot(&mut self, id: IndexId, decision: SidecarBootDecision) {
        if let Some(entry) = self.entry_mut(id) {
            entry.sidecar_boot = Some(decision);
        }
    }

    /// This boot's rebuild-vs-load decision for `id` (`None` until
    /// recovery decides — and forever on a fresh cell).
    pub fn sidecar_boot(&self, id: IndexId) -> Option<SidecarBootDecision> {
        self.entries.iter().find(|e| e.spec.id == id).and_then(|e| e.sidecar_boot)
    }

    /// Monotone catalog epoch (ADR-0080 D5) — changes iff a statement
    /// could now compile differently. The S09 statement cache compares
    /// it on every hit; server-side catalog views fold namespace DDL in.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn iter(&self) -> impl Iterator<Item = &IndexSpec> {
        self.entries.iter().map(|e| &e.spec)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Catalog snapshot for the persist path (the `export_catalog` leg).
    pub fn export(&self) -> Vec<IndexSpec> {
        self.entries.iter().map(|e| e.spec.clone()).collect()
    }

    /// Whether `ns` has any live index — the recomputed source behind
    /// the store-side one-branch cached flag (ADR-0072 D2). DDL-rate
    /// callers only; the mutation path reads the store's cache.
    pub fn has_indexes(&self, ns: NsId) -> bool {
        self.ns_flagged.contains(&ns)
    }

    fn entry_mut(&mut self, id: IndexId) -> Option<&mut RegEntry> {
        self.entries.iter_mut().find(|e| e.spec.id == id)
    }

    fn recompute_flags(&mut self) {
        self.ns_flagged.clear();
        for entry in &self.entries {
            if !self.ns_flagged.contains(&entry.spec.ns) {
                self.ns_flagged.push(entry.spec.ns);
            }
        }
    }
}

/// The one program gauntlet (ADR-0075 D2.4), shared by catalog decode
/// and the DDL path: byte-valid M3 bytecode v1, inside the §3.1
/// indexable-path fence. Doc-build only — a slim build refuses
/// index-bearing catalogs before ever reaching this.
///
/// # Errors
/// A static reason naming the violated rule.
#[cfg(feature = "doc")]
pub fn validate_index_program(bytes: &[u8]) -> Result<(), &'static str> {
    if bytes.is_empty() || bytes.len() > INDEX_PROGRAM_MAX {
        return Err("path program length out of range");
    }
    let program = inf_doc::PathProgram::from_bytes(bytes)
        .map_err(|_| "invalid path-program bytes (M3 bytecode v1)")?;
    if !program.within_index_fence() {
        return Err("path outside the indexable fence (child steps, [*], array index only)");
    }
    Ok(())
}

/// Compile-time guard: the encoding version this registry records is the
/// one the S02 module exports (a drifted constant would bind wrong
/// versions into catalogs and sidecars).
const _: () = assert!(INDEX_KEY_ENCODING_VERSION == 1);

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(id: u32, ns: u32, name: &[u8], state: IndexState) -> IndexSpec {
        IndexSpec {
            id: IndexId(id),
            generation: u64::from(id),
            ns: NsId(ns),
            name: name.to_vec(),
            program: program_bytes("$.price"),
            key_type: IndexKeyType::F64,
            state,
        }
    }

    #[cfg(feature = "doc")]
    fn program_bytes(text: &str) -> Vec<u8> {
        inf_doc::path::compile(text.as_bytes()).expect("valid path").as_bytes().to_vec()
    }

    #[cfg(not(feature = "doc"))]
    fn program_bytes(_text: &str) -> Vec<u8> {
        vec![1, 0, 1] // version, flags, Root — opaque without `doc`.
    }

    /// Every D3-legal edge transitions; every illegal edge is the
    /// The catalog epoch (ADR-0080 D5) moves on exactly the mutations
    /// that can change what a statement compiles to — and not on
    /// cell-machine progress, which planning never reads (ADR-0075 D3).
    #[test]
    fn epoch_moves_on_catalog_mutations_only() {
        let mut reg = IndexRegistry::default();
        assert_eq!(reg.epoch(), 0);
        reg.create(spec(1, 16, b"by-price", IndexState::Declared), false).expect("create");
        assert_eq!(reg.epoch(), 1);
        reg.set_catalog_state(IndexId(1), IndexState::Backfilling).expect("flip");
        assert_eq!(reg.epoch(), 2);
        reg.set_cell_state(IndexId(1), IndexState::Backfilling).expect("cell flip");
        assert_eq!(reg.epoch(), 2, "cell-machine state never influences planning");
        reg.set_catalog_state(IndexId(1), IndexState::Ready).expect("ready");
        assert_eq!(reg.epoch(), 3);
        reg.rebuild(IndexId(1), 2).expect("rebuild");
        assert_eq!(reg.epoch(), 4);
        assert!(reg.set_catalog_state(IndexId(1), IndexState::Declared).is_err());
        assert_eq!(reg.epoch(), 4, "refused transitions do not move the epoch");
        reg.remove(IndexId(1)).expect("remove");
        assert_eq!(reg.epoch(), 5);
        reg.create(spec(2, 16, b"by-score", IndexState::Declared), false).expect("create");
        assert_eq!(reg.remove_ns(NsId(16)), 1);
        assert_eq!(reg.epoch(), 7);
    }

    /// explicit typed rejection — at both state scopes.
    #[test]
    fn lifecycle_edges_are_exactly_the_d3_set() {
        use IndexState::{Backfilling, Declared, Dropping, Ready};
        let all = [Declared, Backfilling, Ready, Dropping];
        let legal = [
            (Declared, Backfilling),
            (Backfilling, Ready),
            (Ready, Backfilling),
            (Declared, Dropping),
            (Backfilling, Dropping),
            (Ready, Dropping),
        ];
        for from in all {
            for to in all {
                let mut reg = IndexRegistry::default();
                reg.create(spec(1, 16, b"by-price", from), false).expect("create");
                let expect_ok = legal.contains(&(from, to));
                let catalog = reg.set_catalog_state(IndexId(1), to);
                let cell = reg.set_cell_state(IndexId(1), to);
                if expect_ok {
                    catalog.expect("legal edge");
                    cell.expect("legal edge");
                } else {
                    assert_eq!(catalog, Err(IndexError::InvalidTransition { from, to }));
                    assert_eq!(cell, Err(IndexError::InvalidTransition { from, to }));
                }
            }
        }
    }

    #[test]
    fn create_validations_refuse_typed() {
        let mut reg = IndexRegistry::default();
        reg.create(spec(1, 16, b"by-price", IndexState::Declared), false).expect("create");
        // Duplicate (ns, name); the same name on another ns is fine.
        assert_eq!(
            reg.create(spec(2, 16, b"by-price", IndexState::Declared), false),
            Err(IndexError::Exists)
        );
        reg.create(spec(2, 17, b"by-price", IndexState::Declared), false).expect("other ns");
        // Bad name; bad program length.
        assert_eq!(
            reg.create(spec(3, 16, b"has space", IndexState::Declared), false),
            Err(IndexError::InvalidName)
        );
        let empty = IndexSpec { program: Vec::new(), ..spec(3, 16, b"p", IndexState::Declared) };
        assert_eq!(
            reg.create(empty, false),
            Err(IndexError::InvalidProgram("path program length out of range"))
        );
        assert_eq!(reg.len(), 2);
    }

    #[test]
    fn the_node_cap_is_a_typed_refusal() {
        let mut reg = IndexRegistry::default();
        for i in 0..INDEXES_PER_NODE_MAX as u32 {
            let name = format!("idx-{i}");
            reg.create(spec(i + 1, 16, name.as_bytes(), IndexState::Declared), false)
                .expect("under the cap");
        }
        assert_eq!(
            reg.create(spec(1000, 16, b"one-more", IndexState::Declared), false),
            Err(IndexError::TooManyIndexes)
        );
    }

    /// The D7 binding gate: unknown, stale-generation, and non-`ready`
    /// bindings each fail with their own typed error — the
    /// drop-during-query and rebuild-during-query verdicts at the seam.
    #[test]
    fn binding_gate_answers_typed() {
        let mut reg = IndexRegistry::default();
        reg.create(spec(1, 16, b"by-price", IndexState::Declared), false).expect("create");
        assert_eq!(
            reg.validate_binding(NsId(16), IndexId(1), 1),
            Err(IndexBindError::NotReady(IndexState::Declared))
        );
        reg.set_catalog_state(IndexId(1), IndexState::Backfilling).expect("edge");
        reg.set_catalog_state(IndexId(1), IndexState::Ready).expect("edge");
        reg.validate_binding(NsId(16), IndexId(1), 1).expect("ready + exact generation binds");
        assert_eq!(
            reg.validate_binding(NsId(16), IndexId(1), 7),
            Err(IndexBindError::StaleGeneration { bound: 7, current: 1 })
        );
        // Wrong namespace and dropped index are both UnknownIndex.
        assert_eq!(
            reg.validate_binding(NsId(17), IndexId(1), 1),
            Err(IndexBindError::UnknownIndex)
        );
        reg.set_catalog_state(IndexId(1), IndexState::Dropping).expect("edge");
        assert_eq!(
            reg.validate_binding(NsId(16), IndexId(1), 1),
            Err(IndexBindError::NotReady(IndexState::Dropping))
        );
        reg.remove(IndexId(1)).expect("remove");
        assert_eq!(
            reg.validate_binding(NsId(16), IndexId(1), 1),
            Err(IndexBindError::UnknownIndex)
        );
    }

    /// Rebuild bumps the generation and regresses the states: a cursor
    /// bound to the old generation fails typed. (Tree contents live in
    /// the owning store's attach block since ADR-0076 D1 — the reset leg
    /// is covered by the keyspace rebuild test.)
    #[test]
    fn rebuild_bumps_generation_and_state() {
        let mut reg = IndexRegistry::default();
        reg.create(spec(1, 16, b"by-price", IndexState::Ready), false).expect("create");
        reg.rebuild(IndexId(1), 9).expect("rebuild from ready");
        let spec_after = reg.get_by_id(IndexId(1)).expect("entry");
        assert_eq!(spec_after.generation, 9);
        assert_eq!(spec_after.state, IndexState::Backfilling);
        assert_eq!(reg.cell_state(IndexId(1)), Some(IndexState::Backfilling));
        assert_eq!(
            reg.validate_binding(NsId(16), IndexId(1), 1),
            Err(IndexBindError::StaleGeneration { bound: 1, current: 9 })
        );
        // Only Ready rebuilds.
        assert!(matches!(reg.rebuild(IndexId(1), 10), Err(IndexError::InvalidTransition { .. })));
    }

    #[test]
    fn ns_flags_track_ddl_transitions() {
        let mut reg = IndexRegistry::default();
        assert!(!reg.has_indexes(NsId(16)));
        reg.create(spec(1, 16, b"a", IndexState::Declared), false).expect("create");
        reg.create(spec(2, 16, b"b", IndexState::Declared), false).expect("create");
        reg.create(spec(3, 0, b"c", IndexState::Declared), false).expect("default db");
        assert!(reg.has_indexes(NsId(16)));
        assert!(reg.has_indexes(NsId(0)));
        assert!(!reg.has_indexes(NsId(17)));
        reg.remove(IndexId(1)).expect("remove");
        assert!(reg.has_indexes(NsId(16)), "one entry remains");
        assert_eq!(reg.remove_ns(NsId(16)), 1);
        assert!(!reg.has_indexes(NsId(16)));
        assert!(reg.has_indexes(NsId(0)), "other namespaces untouched");
    }

    // The L5 memory-fold reconciliation moved with tree custody
    // (ADR-0076 D1): per-store attach folds are covered beside the
    // maintenance hook and the keyspace accounting tests.

    /// The fence gauntlet splits accepted and rejected programs (the
    /// catalog-decode trust boundary shares this exact function).
    #[cfg(feature = "doc")]
    #[test]
    fn program_gauntlet_enforces_the_fence() {
        for text in ["$.a", "$.items[2].price", "$.tags[*]"] {
            validate_index_program(&program_bytes(text)).expect("inside the fence");
        }
        for text in ["$..a", "$[1:3]", "$['a','b']"] {
            assert!(validate_index_program(&program_bytes(text)).is_err(), "{text} refused");
        }
        assert!(validate_index_program(&[]).is_err(), "empty refused");
        assert!(validate_index_program(&[0xFF; 8]).is_err(), "garbage refused");
    }
}
