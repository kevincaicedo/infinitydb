//! `JsonDoc` record integration (M3-S03, **ADR-0037**): documents are
//! ordinary records — durable, evictable, version-carrying, tier-able
//! with zero special cases (L2). The record value is self-describing via
//! one form byte:
//!
//! ```text
//! common: form:u8 · deltas_since_full:u16 LE · delta_bytes_since_full:u32 LE ·
//!         lineage:u64 LE
//! form 0 InlineTape: common · canonical idoc bytes (≤ doc_inline_bytes_max)
//! form 1 ArenaTape:  common · addr:u64 LE · len:u32 LE
//! form 2 ArenaTree:  common · root:u64 LE · node_bytes:u32 LE ·
//!                    slack_bytes:u32 LE · frozen_len:u32 LE
//! ```
//!
//! Placement is decided by two thresholds with different owners (ADR-0037
//! D2): the **inline** threshold (default 512 B — measured at S20) picks
//! record-inline vs doc-arena; the **morph** threshold (default 4 KiB —
//! ADR-0036 D8, finalized at S16) picks tape blob vs node tree.
//!
//! Lifecycle law (ADR-0037 D3): every record free or overwrite releases
//! the document payload through [`DocStore::release`]; RENAME transfers
//! the handle; COPY deep-copies through canonical frozen bytes.
//! Accounting is exact at every site — the [`DocDomain`] counters
//! reconcile against the doc arena continuously (the S03 storm AC), and
//! S19 wires them into `INFO memory`.

#[cfg(feature = "doc")]
use inf_alloc::{Arena, ArenaAddr};
#[cfg(feature = "doc")]
use inf_doc::{
    ApplyError, ApplyOp, ArenaDoc, DocError, DocMemReport, DocRef, DocValue, FreezeScratch,
    ScalarPatch, TapeDoc, patch_scalar_in_place,
};
#[cfg(feature = "doc")]
use inf_foundation::time::Nanos;
#[cfg(feature = "doc")]
use inf_log::DocLineage;

#[cfg(feature = "doc")]
use crate::keyspace::ReplayError;
#[cfg(feature = "doc")]
use crate::record::{
    MAX_EXPIRE_MS, MAX_KEY_LEN, MAX_VAL_LEN, RecordKind, RecordSpec, RecordView, TypeTag,
    bump_version_in_place,
};
#[cfg(not(feature = "doc"))]
use crate::record::{RecordView, TypeTag};
use crate::store::StoreConfig;
#[cfg(feature = "doc")]
use crate::store::{CellStore, OpError, SetCond, SetExpire, record_at};

#[cfg(feature = "doc")]
pub(crate) const FORM_INLINE: u8 = 0;
#[cfg(feature = "doc")]
pub(crate) const FORM_TAPE: u8 = 1;
#[cfg(feature = "doc")]
pub(crate) const FORM_TREE: u8 = 2;
#[cfg(feature = "doc")]
const VALUE_PREFIX_LEN: usize = 15;

#[cfg(feature = "doc")]
#[derive(Copy, Clone, Default, Debug)]
pub(crate) struct DocCadence {
    deltas: u16,
    bytes: u32,
}

#[cfg(feature = "doc")]
fn cadence_of(value: &[u8]) -> DocCadence {
    debug_assert!(value.len() >= VALUE_PREFIX_LEN);
    DocCadence {
        deltas: u16::from_le_bytes(value[1..3].try_into().expect("2-byte field")),
        bytes: u32::from_le_bytes(value[3..7].try_into().expect("4-byte field")),
    }
}

#[cfg(feature = "doc")]
fn lineage_of(value: &[u8]) -> DocLineage {
    debug_assert!(value.len() >= VALUE_PREFIX_LEN);
    let raw = u64::from_le_bytes(value[7..15].try_into().expect("8-byte field"));
    DocLineage::new(raw).expect("store-written document lineage is nonzero")
}

#[cfg(feature = "doc")]
pub(crate) fn lineage_of_record(view: RecordView<'_>) -> DocLineage {
    debug_assert_eq!(view.type_tag(), TypeTag::JsonDoc);
    lineage_of(view.value())
}

#[cfg(feature = "doc")]
pub(crate) fn write_lineage(value: &mut [u8], lineage: DocLineage) {
    debug_assert!(value.len() >= VALUE_PREFIX_LEN);
    value[7..15].copy_from_slice(&lineage.get().to_le_bytes());
}

#[cfg(feature = "doc")]
fn write_prefix(out: &mut [u8], form: u8, cadence: DocCadence, lineage: DocLineage) {
    debug_assert!(out.len() >= VALUE_PREFIX_LEN);
    out[0] = form;
    out[1..3].copy_from_slice(&cadence.deltas.to_le_bytes());
    out[3..7].copy_from_slice(&cadence.bytes.to_le_bytes());
    out[7..15].copy_from_slice(&lineage.get().to_le_bytes());
}

/// Document memory-domain counters (ADR-0037 D5). `tape_bytes` +
/// `arena_bytes` partition the doc arena's live bytes exactly;
/// `slack_bytes` ⊆ `arena_bytes` and `intern_bytes`/`inline_bytes` are
/// attribution overlays (inline idoc bytes already live inside
/// `records_live_bytes`). S19 maps these onto the frozen `doc_*` names.
#[derive(Copy, Clone, Default, PartialEq, Eq, Debug)]
pub struct DocDomain {
    /// idoc tape bytes stored as doc-arena blobs (form 1).
    pub tape_bytes: u64,
    /// Tree node/cell bytes in the doc arena, growth slack included (form 2).
    pub arena_bytes: u64,
    /// Unused tree capacity (subset of `arena_bytes`).
    pub slack_bytes: u64,
    /// Interned key-table bytes inside stored tapes (ADR-0038; overlay).
    pub intern_bytes: u64,
    /// idoc bytes stored inline in record values (form 0; overlay).
    pub inline_bytes: u64,
    /// Live inline-form documents.
    pub inline_docs: u64,
    /// Live document records, all forms.
    pub docs_live: u64,
}

/// Move a counter by `after − before` with overflow checks — domain
/// counters may shrink (tree edits free nodes) but never go negative.
#[cfg(feature = "doc")]
fn shift(counter: &mut u64, before: usize, after: usize) {
    *counter = counter
        .checked_add(after as u64)
        .expect("domain counter overflow")
        .checked_sub(before as u64)
        .expect("domain counter underflow");
}

/// Captured description of a record's document payload — taken *before* a
/// record is freed or overwritten, released only after the destructive
/// step succeeds (the OOM-ordering rule, ADR-0037 D3).
#[cfg(feature = "doc")]
#[derive(Copy, Clone, Debug)]
pub(crate) enum DocPayload {
    /// Not a document record.
    None,
    Inline {
        idoc_len: u32,
        dict_len: u32,
    },
    Blob {
        addr: ArenaAddr,
        len: u32,
    },
    Tree {
        root: DocRef,
        mem: DocMemReport,
    },
}

#[cfg(not(feature = "doc"))]
#[derive(Copy, Clone, Debug)]
pub(crate) enum DocPayload {
    None,
}

/// Describe the document payload of the record at `addr` (`None` for
/// non-document records). Free-standing so the destructured `expire_tick`
/// closure can call it.
#[cfg(feature = "doc")]
pub(crate) fn payload_of(arena: &Arena, addr: ArenaAddr, len: usize) -> DocPayload {
    let view = RecordView::new(arena.bytes(addr, len));
    if view.type_tag() != TypeTag::JsonDoc {
        return DocPayload::None;
    }
    let value = view.value();
    match value[0] {
        FORM_INLINE => {
            let idoc = &value[VALUE_PREFIX_LEN..];
            DocPayload::Inline { idoc_len: idoc.len() as u32, dict_len: dict_len_of(idoc) }
        }
        FORM_TAPE => {
            let (addr, len) = decode_tape_handle(value);
            DocPayload::Blob { addr, len }
        }
        FORM_TREE => {
            let (root, mem, _) = decode_tree_handle(value);
            DocPayload::Tree { root, mem }
        }
        form => unreachable!("store-written form byte is 0..=2, got {form}"),
    }
}

#[cfg(not(feature = "doc"))]
pub(crate) fn payload_of(
    arena: &inf_alloc::Arena,
    addr: inf_alloc::ArenaAddr,
    len: usize,
) -> DocPayload {
    debug_assert!(
        RecordView::new(arena.bytes(addr, len)).type_tag() != TypeTag::JsonDoc,
        "JsonDoc records cannot exist without the doc feature"
    );
    DocPayload::None
}

/// The form-agnostic root cursor of the document record at `addr`
/// (`None` for non-document records). Free-standing — the M4.5-S04
/// maintenance hook calls it from destructured death sites and bracket
/// peeks (ADR-0076 D1); `json_get` shares it so the form dispatch has
/// exactly one implementation.
#[cfg(feature = "doc")]
pub(crate) fn doc_root_at<'a>(
    arena: &'a Arena,
    docs: &'a DocStore,
    addr: ArenaAddr,
    len: usize,
) -> Option<DocValue<'a>> {
    let view = RecordView::new(arena.bytes(addr, len));
    if view.type_tag() != TypeTag::JsonDoc {
        return None;
    }
    let value = view.value();
    Some(match value[0] {
        FORM_INLINE => {
            DocValue::from(TapeDoc::from_validated_bytes(&value[VALUE_PREFIX_LEN..]).root())
        }
        FORM_TAPE => {
            let (baddr, blen) = decode_tape_handle(value);
            let bytes = docs.arena.bytes(baddr, blen as usize);
            DocValue::from(TapeDoc::from_validated_bytes(bytes).root())
        }
        FORM_TREE => {
            let (root, mem, _) = decode_tree_handle(value);
            ArenaDoc::from_parts(root, mem).root_value(&docs.arena)
        }
        form => unreachable!("store-written form byte is 0..=2, got {form}"),
    })
}

// ---- handle codecs (store-written bytes only — ADR-0037 D1) ---------------

#[cfg(feature = "doc")]
fn encode_tape_handle(
    addr: ArenaAddr,
    len: u32,
    cadence: DocCadence,
    lineage: DocLineage,
) -> [u8; 27] {
    let mut out = [0u8; 27];
    write_prefix(&mut out, FORM_TAPE, cadence, lineage);
    out[15..23].copy_from_slice(&addr.to_raw().to_le_bytes());
    out[23..27].copy_from_slice(&len.to_le_bytes());
    out
}

#[cfg(feature = "doc")]
fn decode_tape_handle(value: &[u8]) -> (ArenaAddr, u32) {
    debug_assert_eq!(value.len(), 27, "tape handle is exactly 27 bytes");
    let raw = u64::from_le_bytes(value[15..23].try_into().expect("8-byte field"));
    let addr = ArenaAddr::from_raw(raw).expect("store-written handles hold 48-bit addresses");
    let len = u32::from_le_bytes(value[23..27].try_into().expect("4-byte field"));
    (addr, len)
}

#[cfg(feature = "doc")]
fn encode_tree_handle(
    root: DocRef,
    mem: DocMemReport,
    frozen_len: u32,
    cadence: DocCadence,
    lineage: DocLineage,
) -> [u8; 35] {
    debug_assert!(mem.node_bytes <= u32::MAX as usize && mem.slack_bytes <= u32::MAX as usize);
    let mut out = [0u8; 35];
    write_prefix(&mut out, FORM_TREE, cadence, lineage);
    out[15..23].copy_from_slice(&root.to_raw().to_le_bytes());
    out[23..27].copy_from_slice(&(mem.node_bytes as u32).to_le_bytes());
    out[27..31].copy_from_slice(&(mem.slack_bytes as u32).to_le_bytes());
    out[31..35].copy_from_slice(&frozen_len.to_le_bytes());
    out
}

#[cfg(feature = "doc")]
fn decode_tree_handle(value: &[u8]) -> (DocRef, DocMemReport, u32) {
    debug_assert_eq!(value.len(), 35, "tree handle is exactly 35 bytes");
    let raw = u64::from_le_bytes(value[15..23].try_into().expect("8-byte field"));
    let root = DocRef::from_raw(raw).expect("store-written handles hold valid refs");
    let node_bytes = u32::from_le_bytes(value[23..27].try_into().expect("4-byte field")) as usize;
    let slack_bytes = u32::from_le_bytes(value[27..31].try_into().expect("4-byte field")) as usize;
    let frozen_len = u32::from_le_bytes(value[31..35].try_into().expect("4-byte field"));
    (root, DocMemReport { node_bytes, slack_bytes }, frozen_len)
}

/// Interned key-table region length of stored idoc bytes (0 when plain).
#[cfg(all(feature = "doc", feature = "doc-intern-keys"))]
fn dict_len_of(idoc: &[u8]) -> u32 {
    TapeDoc::from_validated_bytes(idoc).dict().len() as u32
}

#[cfg(all(feature = "doc", not(feature = "doc-intern-keys")))]
fn dict_len_of(_idoc: &[u8]) -> u32 {
    0
}

// ---- DocStore --------------------------------------------------------------

/// Per-store document state: the document arena, the domain counters, and
/// a reusable ingest scratch buffer. A fieldless no-op without the `doc`
/// feature (ADR-0037 D7).
pub(crate) struct DocStore {
    #[cfg(feature = "doc")]
    pub(crate) arena: Arena,
    #[cfg(feature = "doc")]
    pub(crate) domain: DocDomain,
    #[cfg(feature = "doc")]
    scratch: Vec<u8>,
    #[cfg(feature = "doc")]
    freeze: FreezeScratch,
    #[cfg(feature = "doc")]
    next_lineage: u64,
}

/// Feature-neutral snapshot consumed by `CellStore::report`. The live
/// partition stays in `domain`; allocator residency and retained scratch
/// are separate RSS-side domains so overlays are never double-counted.
#[derive(Copy, Clone, Default, Debug)]
pub(crate) struct DocStoreReport {
    pub domain: DocDomain,
    pub resident_bytes: u64,
    pub scratch_bytes: u64,
}

#[cfg(feature = "doc")]
impl DocStore {
    pub fn new(cfg: &StoreConfig) -> DocStore {
        DocStore {
            arena: Arena::new(cfg.doc_arena),
            domain: DocDomain::default(),
            scratch: Vec::new(),
            freeze: FreezeScratch::default(),
            next_lineage: DocLineage::FIRST.get(),
        }
    }

    /// FLUSH: drop every document with the arena (O(1), exact).
    pub fn reset(&mut self, cfg: &StoreConfig) {
        *self = DocStore::new(cfg);
    }

    #[inline]
    pub fn report(&self) -> DocStoreReport {
        DocStoreReport {
            domain: self.domain,
            resident_bytes: self.arena.report().resident_bytes,
            scratch_bytes: (self.scratch.capacity() + self.freeze.bytes()) as u64,
        }
    }

    /// Logical document bytes held in the dedicated arena. Kept separate
    /// from allocator residency so reconciliation does not become
    /// tautological with the S19 attribution report.
    #[inline]
    pub fn live_bytes(&self) -> u64 {
        self.arena.report().live_bytes
    }

    /// Phase 3 of the staged lookup pipeline (ADR-0044): the record head
    /// was hinted in the preceding pass, so decode only store-written
    /// document framing and hint the first canonical-tape lines. The exact
    /// key check remains in EXECUTE; a fingerprint collision only wastes
    /// these hints.
    #[inline]
    pub fn prefetch_root(&self, view: RecordView<'_>) {
        if view.type_tag() != TypeTag::JsonDoc {
            return;
        }
        let value = view.value();
        let tape = match value[0] {
            FORM_INLINE => &value[VALUE_PREFIX_LEN..],
            FORM_TAPE => {
                let (addr, len) = decode_tape_handle(value);
                self.arena.bytes(addr, len as usize)
            }
            FORM_TREE => return,
            form => unreachable!("store-written form byte is 0..=2, got {form}"),
        };
        inf_simd::prefetch_read(tape.as_ptr());
        if tape.len() > 64 {
            inf_simd::prefetch_read(tape.as_ptr().wrapping_add(64));
        }
    }

    pub(crate) fn allocate_lineage(&mut self) -> DocLineage {
        let lineage = DocLineage::new(self.next_lineage)
            .expect("document lineage space exhausted after 2^64 incarnations");
        self.next_lineage = self.next_lineage.checked_add(1).unwrap_or(0);
        lineage
    }

    fn observe_lineage(&mut self, lineage: DocLineage) {
        if lineage.get() >= self.next_lineage && self.next_lineage != 0 {
            self.next_lineage = lineage.get().checked_add(1).unwrap_or(0);
        }
    }

    /// Release a captured payload: frees blob bytes / tree subtrees and
    /// keeps the domain exact. The one choke point (ADR-0037 D3).
    pub fn release(&mut self, payload: DocPayload) {
        match payload {
            DocPayload::None => return,
            DocPayload::Inline { idoc_len, dict_len } => {
                shift(&mut self.domain.inline_bytes, idoc_len as usize, 0);
                shift(&mut self.domain.intern_bytes, dict_len as usize, 0);
                self.domain.inline_docs -= 1;
            }
            DocPayload::Blob { addr, len } => {
                let dict_len = dict_len_of(self.arena.bytes(addr, len as usize));
                self.arena.free(addr, len as usize);
                shift(&mut self.domain.tape_bytes, len as usize, 0);
                shift(&mut self.domain.intern_bytes, dict_len as usize, 0);
            }
            DocPayload::Tree { root, mem } => {
                ArenaDoc::from_parts(root, mem).free(&mut self.arena);
                shift(&mut self.domain.arena_bytes, mem.node_bytes, 0);
                shift(&mut self.domain.slack_bytes, mem.slack_bytes, 0);
            }
        }
        self.domain.docs_live -= 1;
    }
}

/// Canonical checkpoint bytes for one live document. Plain tape forms
/// borrow their storage directly; arena trees serialize into the store's
/// recycled freeze buffers (ADR-0043 D7).
#[cfg(feature = "doc")]
pub(crate) fn checkpoint_idoc<'a>(
    docs: &'a mut DocStore,
    view: RecordView<'a>,
) -> Result<&'a [u8], OpError> {
    debug_assert_eq!(view.type_tag(), TypeTag::JsonDoc);
    let stored = match view.value()[0] {
        FORM_INLINE => &view.value()[VALUE_PREFIX_LEN..],
        FORM_TAPE => {
            let (addr, len) = decode_tape_handle(view.value());
            docs.arena.bytes(addr, len as usize)
        }
        FORM_TREE => {
            let (root, mem, _) = decode_tree_handle(view.value());
            return ArenaDoc::from_parts(root, mem)
                .freeze_recycled(&docs.arena, &mut docs.freeze)
                .map_err(op_from_doc);
        }
        form => unreachable!("store-written form byte is 0..=2, got {form}"),
    };
    #[cfg(feature = "doc-intern-keys")]
    if stored[3] & inf_doc::FLAG_INTERNED != 0 {
        inf_doc::intern::unintern_into(stored, &mut docs.scratch);
        return Ok(&docs.scratch);
    }
    Ok(stored)
}

#[cfg(not(feature = "doc"))]
impl DocStore {
    pub fn new(_cfg: &StoreConfig) -> DocStore {
        DocStore {}
    }

    pub fn reset(&mut self, _cfg: &StoreConfig) {}

    #[inline]
    pub fn report(&self) -> DocStoreReport {
        DocStoreReport::default()
    }

    #[inline]
    pub fn prefetch_root(&self, _view: RecordView<'_>) {}

    pub fn release(&mut self, _payload: DocPayload) {}
}

// ---- CellStore document API (feature `doc`) --------------------------------

/// Options for [`CellStore::json_set`] (root document writes). TTL
/// semantics are the caller's (S11 pins them against the oracle).
#[cfg(feature = "doc")]
#[derive(Copy, Clone, Debug, Default)]
pub struct JsonSetOptions {
    pub cond: SetCond,
    pub expire: SetExpire,
}

#[cfg(feature = "doc")]
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum JsonSetOutcome {
    Applied,
    /// NX/XX condition not met.
    Skipped,
}

/// Result of the allocation-free scalar patch probe (ADR-0043 D1).
/// `None` at the API boundary means the key itself is missing; `Missing`
/// here means the document exists but the simple path does not.
#[cfg(feature = "doc")]
pub type JsonScalarPatch = ScalarPatch;

/// Post-command choice consumed immediately by `stage_durable_effects`.
#[cfg(feature = "doc")]
#[derive(Debug)]
pub enum JsonLogDecision {
    Delta { lineage: DocLineage, base_version: u32, post_len: u32 },
    Full { lineage: DocLineage, version: u32, idoc: Vec<u8>, expire_at_ms: Option<u64> },
}

#[cfg(feature = "doc")]
const DOC_FULL_AFTER_DELTAS: u16 = 64;

#[cfg(feature = "doc")]
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) enum DocReplayOutcome {
    Applied,
    SkippedStale,
    SkippedMissing,
}

/// Durable facts recorded alongside a delta so replay never consults
/// mutable boot-time limits or confuses key incarnations.
#[cfg(feature = "doc")]
#[derive(Copy, Clone, Debug)]
pub(crate) struct DocDeltaWitness {
    pub lineage: DocLineage,
    pub base_version: u32,
    pub match_count: u32,
    pub post_len: u32,
}

/// Metadata stored in the document record prefix alongside canonical bytes.
#[cfg(feature = "doc")]
#[derive(Copy, Clone, Debug)]
pub(crate) struct DocWriteMeta {
    pub lineage: DocLineage,
    pub version: u32,
    pub expire_at_ms: Option<u64>,
    pub cadence: DocCadence,
}

/// A resolved document read: the form-agnostic root cursor plus the
/// record version (the WATCH/CAS epoch — M6 consumes it).
#[cfg(feature = "doc")]
#[derive(Debug)]
pub struct JsonRead<'a> {
    pub root: DocValue<'a>,
    pub version: u32,
}

#[cfg(feature = "doc")]
impl CellStore {
    /// `JSON.SET key $ value` (root set). `idoc` MUST be canonical plain
    /// v1 bytes that crossed a trust boundary (builder output or
    /// `TapeDoc::from_bytes` — debug-asserted). Version chains like `SET`;
    /// setting over a non-document key is `WrongType` (ADR-0037 D6).
    pub fn json_set(
        &mut self,
        key: &[u8],
        idoc: &[u8],
        opts: JsonSetOptions,
        now: Nanos,
    ) -> Result<JsonSetOutcome, OpError> {
        debug_assert!(TapeDoc::from_bytes(idoc).is_ok(), "json_set requires validated idoc bytes");
        if key.len() > MAX_KEY_LEN || idoc.len() + VALUE_PREFIX_LEN > MAX_VAL_LEN {
            return Err(OpError::TooLarge);
        }
        let existing = self.resolve(key, now);
        let old_view = existing.map(|(addr, len)| RecordView::new(self.arena.bytes(addr, len)));
        if let Some(v) = old_view
            && v.type_tag() != TypeTag::JsonDoc
        {
            return Err(OpError::WrongType);
        }
        let applies = match opts.cond {
            SetCond::Always => true,
            SetCond::IfAbsent => existing.is_none(),
            SetCond::IfPresent => existing.is_some(),
        };
        if !applies {
            return Ok(JsonSetOutcome::Skipped);
        }
        let version = old_view.map_or(1, |v| v.version().wrapping_add(1));
        let lineage =
            old_view.map_or_else(|| self.docs.allocate_lineage(), |v| lineage_of(v.value()));
        let old_deadline = old_view.and_then(|v| v.expire_at_ms());
        let expire_at_ms = match opts.expire {
            SetExpire::Clear => None,
            SetExpire::Keep => old_deadline,
            SetExpire::At(at) => Some((at.0 / 1_000_000).min(MAX_EXPIRE_MS)),
        };
        self.json_write_value(
            key,
            existing,
            idoc,
            DocWriteMeta { lineage, version, expire_at_ms, cadence: DocCadence::default() },
        )?;
        self.note_ttl(old_deadline.is_some(), expire_at_ms.is_some());
        if let Some(ms) = expire_at_ms
            && old_deadline != Some(ms)
        {
            self.arm_wheel(Self::hash_key(key), ms);
        }
        Ok(JsonSetOutcome::Applied)
    }

    /// Read a document as a form-agnostic cursor (expire-on-read applies,
    /// like every read). `WrongType` for non-document records.
    pub fn json_get(&mut self, key: &[u8], now: Nanos) -> Result<Option<JsonRead<'_>>, OpError> {
        let Some((addr, len)) = self.resolve(key, now) else {
            self.stats.keyspace_misses += 1;
            return Ok(None);
        };
        self.stats.keyspace_hits += 1;
        let view = RecordView::new(self.arena.bytes(addr, len));
        if view.type_tag() != TypeTag::JsonDoc {
            return Err(OpError::WrongType);
        }
        let version = view.version();
        let root = doc_root_at(&self.arena, &self.docs, addr, len)
            .expect("type tag checked: the record is a document");
        Ok(Some(JsonRead { root, version }))
    }

    /// Exact bytes attributable to one document key for
    /// `JSON.DEBUG MEMORY`: record allocation request plus any external
    /// doc-arena allocation request. Shared cell pools and allocator slack
    /// have no honest per-key partition and remain in `INFO memory`; this is
    /// therefore deliberately `partial` relative to RedisJSON.
    pub fn json_memory_usage(&mut self, key: &[u8], now: Nanos) -> Result<Option<u64>, OpError> {
        let Some((addr, len)) = self.resolve(key, now) else { return Ok(None) };
        let view = RecordView::new(self.arena.bytes(addr, len));
        if view.type_tag() != TypeTag::JsonDoc {
            return Err(OpError::WrongType);
        }
        let external = match payload_of(&self.arena, addr, len) {
            DocPayload::Inline { .. } => 0,
            DocPayload::Blob { len, .. } => u64::from(len),
            DocPayload::Tree { mem, .. } => mem.node_bytes as u64,
            DocPayload::None => unreachable!("the record type was checked above"),
        };
        Ok(Some(len as u64 + external))
    }

    /// Try the ADR-0043 allocation-free same-width scalar lane. The
    /// canonical general planner remains the fallback on `Unsupported`.
    /// A committed patch bumps the record version exactly once.
    pub fn json_patch_scalar(
        &mut self,
        key: &[u8],
        program: &inf_doc::PathProgram,
        op: &ApplyOp<'_>,
        now: Nanos,
    ) -> Result<Option<JsonScalarPatch>, OpError> {
        let Some((addr, len)) = self.resolve(key, now) else {
            return Ok(None);
        };
        let view = RecordView::new(self.arena.bytes(addr, len));
        if view.type_tag() != TypeTag::JsonDoc {
            return Err(OpError::WrongType);
        }
        let value_off = view.value_offset();
        let form = view.value()[0];
        let verdict = match form {
            FORM_INLINE => {
                let record = self.arena.bytes_mut(addr, len);
                patch_scalar_in_place(&mut record[value_off + VALUE_PREFIX_LEN..], program, op)
            }
            FORM_TAPE => {
                let (doc_addr, doc_len) = decode_tape_handle(view.value());
                patch_scalar_in_place(
                    self.docs.arena.bytes_mut(doc_addr, doc_len as usize),
                    program,
                    op,
                )
            }
            FORM_TREE => {
                let cadence = cadence_of(view.value());
                let lineage = lineage_of(view.value());
                let (root, before, frozen_len) = decode_tree_handle(view.value());
                let mut doc = ArenaDoc::from_parts(root, before);
                let verdict = match doc.patch_scalar(&mut self.docs.arena, program, op) {
                    Ok(verdict) => verdict,
                    Err(error) => return Err(op_from_apply(error)),
                };
                if matches!(verdict, ScalarPatch::Number(_) | ScalarPatch::Toggled(_)) {
                    let after = doc.report();
                    shift(&mut self.docs.domain.arena_bytes, before.node_bytes, after.node_bytes);
                    shift(&mut self.docs.domain.slack_bytes, before.slack_bytes, after.slack_bytes);
                    let handle =
                        encode_tree_handle(doc.root_ref(), after, frozen_len, cadence, lineage);
                    let record = self.arena.bytes_mut(addr, len);
                    record[value_off..value_off + 35].copy_from_slice(&handle);
                }
                Ok(verdict)
            }
            other => unreachable!("store-written form byte is 0..=2, got {other}"),
        }
        .map_err(op_from_apply)?;
        if matches!(verdict, ScalarPatch::Number(_) | ScalarPatch::Toggled(_)) {
            bump_version_in_place(self.arena.bytes_mut(addr, len));
        }
        Ok(Some(verdict))
    }

    /// Decide and account one durable path mutation after logical commit.
    /// The caller already admitted worst-case full-image bytes, so the
    /// returned choice can be staged without failure (ADR-0043 D5).
    pub fn json_log_delta_decision(
        &mut self,
        key: &[u8],
        delta_record_bytes: usize,
        operand_bytes: usize,
        now: Nanos,
    ) -> Option<JsonLogDecision> {
        let (addr, len) = self.json_log_record(key, now)?;
        let view = RecordView::new(self.arena.bytes(addr, len));
        if view.type_tag() != TypeTag::JsonDoc {
            return None;
        }
        let value_off = view.value_offset();
        let form = view.value()[0];
        let current_bytes = self.json_canonical_len(view) as usize;
        let cadence = cadence_of(view.value());
        let prospective = DocCadence {
            deltas: cadence.deltas.saturating_add(1),
            bytes: cadence
                .bytes
                .saturating_add(u32::try_from(delta_record_bytes).unwrap_or(u32::MAX)),
        };
        let full = prospective.deltas >= DOC_FULL_AFTER_DELTAS
            || prospective.bytes as usize >= current_bytes
            || operand_bytes >= current_bytes;
        let version = view.version();
        let lineage = lineage_of(view.value());
        let expire_at_ms = view.expire_at_ms();
        let idoc = full.then(|| {
            self.frozen_bytes_of(view)
                .expect("store-owned valid documents always freeze within the format bound")
        });
        let next = if full { DocCadence::default() } else { prospective };
        let record = self.arena.bytes_mut(addr, len);
        write_prefix(&mut record[value_off..value_off + VALUE_PREFIX_LEN], form, next, lineage);
        match idoc {
            Some(idoc) => JsonLogDecision::Full { lineage, version, idoc, expire_at_ms },
            None => JsonLogDecision::Delta {
                lineage,
                base_version: version.wrapping_sub(1) & inf_log::DOC_VERSION_MASK,
                post_len: u32::try_from(current_bytes).expect("idoc length is u24"),
            },
        }
        .into()
    }

    /// Root-set/full-image staging. Resets volatile cadence state.
    pub fn json_log_full(&mut self, key: &[u8], now: Nanos) -> Option<JsonLogDecision> {
        let (addr, len) = self.json_log_record(key, now)?;
        self.json_log_full_resolved(addr, len)
    }

    pub(crate) fn json_log_full_resolved(
        &mut self,
        addr: ArenaAddr,
        len: usize,
    ) -> Option<JsonLogDecision> {
        let view = RecordView::new(self.arena.bytes(addr, len));
        if view.type_tag() != TypeTag::JsonDoc {
            return None;
        }
        let value_off = view.value_offset();
        let version = view.version();
        let lineage = lineage_of(view.value());
        let expire_at_ms = view.expire_at_ms();
        let idoc = self
            .frozen_bytes_of(view)
            .expect("store-owned valid documents always freeze within the format bound");
        let form = view.value()[0];
        write_prefix(
            &mut self.arena.bytes_mut(addr, len)[value_off..value_off + VALUE_PREFIX_LEN],
            form,
            DocCadence::default(),
            lineage,
        );
        Some(JsonLogDecision::Full { lineage, version, idoc, expire_at_ms })
    }

    /// Canonical full-image size for durable admission, without access
    /// tracking, expiry reaping, or tree freezing. `None` means the key is
    /// absent, expired, or not a document.
    pub fn json_log_image_bytes(&self, key: &[u8], now: Nanos) -> Option<usize> {
        let (addr, _) = self.json_log_record(key, now)?;
        let view = record_at(&self.arena, addr);
        if view.type_tag() != TypeTag::JsonDoc {
            return None;
        }
        Some(self.json_canonical_len(view) as usize)
    }

    /// Blind idempotent document post-image apply (DocFull/checkpoint).
    pub(crate) fn replay_json_full(
        &mut self,
        key: &[u8],
        lineage: DocLineage,
        version: u32,
        idoc: &[u8],
        now: Nanos,
    ) -> Result<(), ReplayError> {
        TapeDoc::from_bytes(idoc).map_err(ReplayError::InvalidDocument)?;
        if key.len() > MAX_KEY_LEN || idoc.len() + VALUE_PREFIX_LEN > MAX_VAL_LEN {
            return Err(ReplayError::Store(OpError::TooLarge));
        }
        let existing = self.resolve(key, now);
        self.json_write_value(
            key,
            existing,
            idoc,
            DocWriteMeta { lineage, version, expire_at_ms: None, cadence: DocCadence::default() },
        )
        .map_err(ReplayError::Store)?;
        self.docs.observe_lineage(lineage);
        Ok(())
    }

    /// Exactly-once delta apply under ADR-0043's modular u24 rule.
    pub(crate) fn replay_json_delta(
        &mut self,
        key: &[u8],
        witness: DocDeltaWitness,
        program: &inf_doc::PathProgram,
        op: &ApplyOp<'_>,
        now: Nanos,
    ) -> Result<DocReplayOutcome, ReplayError> {
        let Some((addr, len)) = self.resolve(key, now) else {
            return Ok(DocReplayOutcome::SkippedMissing);
        };
        let view = RecordView::new(self.arena.bytes(addr, len));
        if view.type_tag() != TypeTag::JsonDoc {
            return Ok(DocReplayOutcome::SkippedStale);
        }
        let current_lineage = lineage_of(view.value());
        if current_lineage != witness.lineage {
            return if current_lineage > witness.lineage {
                Ok(DocReplayOutcome::SkippedStale)
            } else {
                Err(ReplayError::CorruptDocument("document delta lineage is ahead"))
            };
        }
        let current_len = self.json_canonical_len(view);
        let current = view.version();
        let distance = current.wrapping_sub(witness.base_version) & inf_log::DOC_VERSION_MASK;
        if distance != 0 {
            return if distance < (1 << 23) {
                Ok(DocReplayOutcome::SkippedStale)
            } else {
                Err(ReplayError::CorruptDocument("document delta base version is ahead"))
            };
        }
        if witness.match_count == 1 && current_len == witness.post_len {
            match self
                .json_patch_scalar(key, program, op, now)
                .map_err(ReplayError::Store)?
                .expect("record resolved above")
            {
                ScalarPatch::Number(_) | ScalarPatch::Toggled(_) => {
                    return Ok(DocReplayOutcome::Applied);
                }
                ScalarPatch::Missing | ScalarPatch::Skipped => {
                    return Err(ReplayError::CorruptDocument(
                        "document delta produced no live mutation",
                    ));
                }
                ScalarPatch::Unsupported => {}
            }
        }
        let view = RecordView::new(self.arena.bytes(addr, len));
        let frozen = self.frozen_bytes_of(view).map_err(ReplayError::Store)?;
        let doc = TapeDoc::from_validated_bytes(&frozen);
        let limits = inf_doc::path::EvalLimits { max_matches: witness.match_count };
        let outcome = inf_doc::apply::apply(&doc, program, op, &limits, witness.post_len as usize)
            .map_err(ReplayError::InvalidMutation)?;
        let Some(bytes) = outcome.bytes else {
            return Err(ReplayError::CorruptDocument("document delta produced no canonical edit"));
        };
        if outcome.results.len() != witness.match_count as usize
            || bytes.len() != witness.post_len as usize
        {
            return Err(ReplayError::CorruptDocument(
                "document delta replay output disagrees with recorded bounds",
            ));
        }
        let replaced = self.json_replace(key, &bytes, now).map_err(ReplayError::Store)?;
        debug_assert!(replaced, "record resolved above");
        Ok(DocReplayOutcome::Applied)
    }

    /// Replace a document's content — the path-mutation shape (S16's
    /// subtree-replace fallback): TTL preserved, version bumps exactly
    /// once, placement re-tiered. `Ok(false)` = key missing.
    pub fn json_replace(&mut self, key: &[u8], idoc: &[u8], now: Nanos) -> Result<bool, OpError> {
        debug_assert!(
            TapeDoc::from_bytes(idoc).is_ok(),
            "json_replace requires validated idoc bytes"
        );
        let Some((addr, len)) = self.resolve(key, now) else {
            return Ok(false);
        };
        let view = RecordView::new(self.arena.bytes(addr, len));
        if view.type_tag() != TypeTag::JsonDoc {
            return Err(OpError::WrongType);
        }
        let version = view.version().wrapping_add(1);
        let lineage = lineage_of(view.value());
        let deadline = view.expire_at_ms();
        let cadence = cadence_of(view.value());
        self.json_write_value(
            key,
            Some((addr, len)),
            idoc,
            DocWriteMeta { lineage, version, expire_at_ms: deadline, cadence },
        )?;
        Ok(true)
    }

    /// One-way tape → tree morph (ADR-0036 D8; S16 owns the policy of
    /// *when*). A physical re-representation: the version does NOT bump
    /// (§3.4 R3 — replay reproduces versions and morphs are never logged).
    /// `Ok(false)` = key missing; already-tree documents are `Ok(true)`.
    pub fn json_morph(&mut self, key: &[u8], now: Nanos) -> Result<bool, OpError> {
        let Some((addr, len)) = self.resolve(key, now) else {
            return Ok(false);
        };
        let view = RecordView::new(self.arena.bytes(addr, len));
        if view.type_tag() != TypeTag::JsonDoc {
            return Err(OpError::WrongType);
        }
        if view.value()[0] == FORM_TREE {
            return Ok(true);
        }
        let version = view.version();
        let lineage = lineage_of(view.value());
        let deadline = view.expire_at_ms();
        let cadence = cadence_of(view.value());
        // Copy the tape out: the blob lives in the arena the tree build
        // must borrow mutably; the tree form is never interned (ADR-0038
        // D3), so interned tapes go through their plain form.
        let plain = self.stored_plain_bytes(view);
        let tape = TapeDoc::from_validated_bytes(&plain);
        let adoc = ArenaDoc::from_tape(&tape, &mut self.docs.arena).map_err(op_from_doc)?;
        let mem = adoc.report();
        let handle = encode_tree_handle(adoc.root_ref(), mem, plain.len() as u32, cadence, lineage);
        let spec = RecordSpec {
            key,
            value: &handle,
            version,
            expire_at_ms: deadline,
            kind: RecordKind::JsonDoc,
        };
        match self.write_record_releasing(key, Some((addr, len)), spec) {
            Ok(()) => {
                shift(&mut self.docs.domain.arena_bytes, 0, mem.node_bytes);
                shift(&mut self.docs.domain.slack_bytes, 0, mem.slack_bytes);
                self.docs.domain.docs_live += 1;
                Ok(true)
            }
            Err(e) => {
                adoc.free(&mut self.docs.arena);
                Err(e)
            }
        }
    }

    /// Edit a tree-form document in place. `edit` runs against the
    /// rehydrated [`ArenaDoc`] and the doc arena; on `Ok` the record's
    /// handle fields refresh and the version bumps **exactly once** — the
    /// L6 mutation seam S16's engine composes. On `Err` the version and
    /// record stay untouched (physical accounting still reconciles; the
    /// plan/apply atomicity discipline is S16's — §3.4 R4). `Ok(None)` =
    /// key missing. Non-tree forms are `WrongType`: morph first. This seam
    /// has no production caller after ADR-0043 rejected generic surgery;
    /// it refreshes exact frozen length through recycled scratch in O(doc).
    /// Any future hot structural engine must pass its planner's exact size
    /// through a revised API and an A/B artifact instead of inheriting that
    /// walk silently. Its accounting stays continuously pinned meanwhile:
    /// `tests/doc_storm.rs` drives it inside the reconciliation storm and
    /// `tests/doc_records.rs` pins the `Err`/wrong-form/missing arms — the
    /// visible-debt loop for a caller-less seam (S18/S19 review,
    /// 2026-07-16).
    pub fn json_edit_tree<T>(
        &mut self,
        key: &[u8],
        now: Nanos,
        edit: impl FnOnce(&mut ArenaDoc, &mut Arena) -> Result<T, DocError>,
    ) -> Result<Option<T>, OpError> {
        let Some((addr, len)) = self.resolve(key, now) else {
            return Ok(None);
        };
        let view = RecordView::new(self.arena.bytes(addr, len));
        if view.type_tag() != TypeTag::JsonDoc || view.value()[0] != FORM_TREE {
            return Err(OpError::WrongType);
        }
        let value_off = view.value_offset();
        let cadence = cadence_of(view.value());
        let lineage = lineage_of(view.value());
        let (root, before, frozen_len_before) = decode_tree_handle(view.value());
        let mut adoc = ArenaDoc::from_parts(root, before);
        let outcome = edit(&mut adoc, &mut self.docs.arena);
        let after = adoc.report();
        // Physical accounting reconciles in both outcomes — the tree may
        // have grown before a failure; logical atomicity is S16's plan
        // phase (§3.4 R4), the store's guarantee is version/log untouched.
        shift(&mut self.docs.domain.arena_bytes, before.node_bytes, after.node_bytes);
        shift(&mut self.docs.domain.slack_bytes, before.slack_bytes, after.slack_bytes);
        let frozen_len = if outcome.is_ok() {
            adoc.freeze_recycled(&self.docs.arena, &mut self.docs.freeze)
                .map_err(op_from_doc)?
                .len() as u32
        } else {
            frozen_len_before
        };
        let handle = encode_tree_handle(adoc.root_ref(), after, frozen_len, cadence, lineage);
        let record = self.arena.bytes_mut(addr, len);
        record[value_off..value_off + 35].copy_from_slice(&handle);
        match outcome {
            Ok(value) => {
                bump_version_in_place(record);
                Ok(Some(value))
            }
            Err(e) => Err(op_from_doc(e)),
        }
    }

    /// Canonical plain tape bytes for any form — the ADR-0032 D6 contract
    /// (S17's checkpoint walker, M4 demotion, COPY). `Ok(None)` = missing.
    pub fn json_freeze(&mut self, key: &[u8], now: Nanos) -> Result<Option<Vec<u8>>, OpError> {
        let Some((addr, len)) = self.resolve(key, now) else {
            return Ok(None);
        };
        let view = RecordView::new(self.arena.bytes(addr, len));
        if view.type_tag() != TypeTag::JsonDoc {
            return Err(OpError::WrongType);
        }
        self.frozen_bytes_of(view).map(Some)
    }

    /// Namespace-resolved ingest limits (M3-S11: the command layer
    /// constructs parsers from the target store's config — that IS the
    /// per-namespace resolution ADR-0039 D5 named; construction clamps
    /// to the format ceilings).
    #[inline]
    pub fn doc_parse_limits(&self) -> inf_doc::ParseLimits {
        inf_doc::ParseLimits {
            max_depth: self.cfg.doc_max_depth,
            max_text: self.cfg.doc_max_bytes,
            max_body: self.cfg.doc_max_bytes,
        }
    }

    /// Namespace-resolved path-text cap (ADR-0040 D6; the S10 cache
    /// enforces it before the lookup).
    #[inline]
    pub fn doc_max_path_bytes(&self) -> usize {
        self.cfg.doc_max_path_bytes
    }

    /// Namespace-resolved match-set cap (ADR-0040 D6's declared product
    /// limit — S22 documents it in the matrix).
    #[inline]
    pub fn doc_max_path_matches(&self) -> u32 {
        self.cfg.doc_max_path_matches
    }

    /// Namespace-resolved idoc-byte cap for path mutations (the apply
    /// engine's post-edit bound — same axis the ingest dual bound uses).
    #[inline]
    pub fn doc_max_bytes(&self) -> usize {
        self.cfg.doc_max_bytes
    }

    /// The document memory domain (S19 wires this into reports/`INFO`).
    #[inline]
    pub fn doc_domain(&self) -> DocDomain {
        self.docs.domain
    }

    /// Doc-arena live bytes — the reconciliation counterpart of
    /// [`doc_domain`](Self::doc_domain) (`tape_bytes + arena_bytes`).
    #[inline]
    pub fn doc_live_bytes(&self) -> u64 {
        self.docs.live_bytes()
    }

    // ---- internals ----

    /// Resolve for post-command log staging without LRU/LFU touch or lazy
    /// expiry mutation. An expired physical residue is logically absent.
    fn json_log_record(&self, key: &[u8], now: Nanos) -> Option<(ArenaAddr, usize)> {
        let hash = Self::hash_key(key);
        let arena = &self.arena;
        let addr = self.index.find(hash, |addr| record_at(arena, addr).key() == key)?;
        let view = record_at(arena, addr);
        (!view.is_expired(now)).then_some((addr, view.encoded_len()))
    }

    /// Canonical plain bytes of a resolved document record.
    pub(crate) fn frozen_bytes_of(&self, view: RecordView<'_>) -> Result<Vec<u8>, OpError> {
        debug_assert_eq!(view.type_tag(), TypeTag::JsonDoc);
        let value = view.value();
        match value[0] {
            FORM_INLINE | FORM_TAPE => Ok(self.stored_plain_bytes(view)),
            FORM_TREE => {
                let (root, mem, _) = decode_tree_handle(value);
                ArenaDoc::from_parts(root, mem).freeze(&self.docs.arena).map_err(op_from_doc)
            }
            form => unreachable!("store-written form byte is 0..=2, got {form}"),
        }
    }

    /// Canonical plain idoc length without materializing an uninterned
    /// image or freezing a tree. This is the exact durable-admission and
    /// cadence byte axis (ADR-0043 D2/D8).
    pub(crate) fn json_canonical_len(&self, view: RecordView<'_>) -> u32 {
        debug_assert_eq!(view.type_tag(), TypeTag::JsonDoc);
        let value = view.value();
        let stored = match value[0] {
            FORM_INLINE => &value[VALUE_PREFIX_LEN..],
            FORM_TAPE => {
                let (addr, len) = decode_tape_handle(value);
                self.docs.arena.bytes(addr, len as usize)
            }
            FORM_TREE => return decode_tree_handle(value).2,
            form => unreachable!("store-written form byte is 0..=2, got {form}"),
        };
        #[cfg(feature = "doc-intern-keys")]
        if stored[3] & inf_doc::FLAG_INTERNED != 0 {
            return u32::try_from(inf_doc::intern::uninterned_len(stored))
                .expect("idoc length is u24");
        }
        stored.len() as u32
    }

    /// Owned plain canonical tape bytes of an inline/blob record —
    /// un-interned if the stored form was interned (ADR-0038 D3).
    fn stored_plain_bytes(&self, view: RecordView<'_>) -> Vec<u8> {
        let value = view.value();
        let stored: &[u8] = match value[0] {
            FORM_INLINE => &value[VALUE_PREFIX_LEN..],
            FORM_TAPE => {
                let (baddr, blen) = decode_tape_handle(value);
                self.docs.arena.bytes(baddr, blen as usize)
            }
            form => unreachable!("tape bytes exist only for forms 0/1, got {form}"),
        };
        #[cfg(feature = "doc-intern-keys")]
        if stored[3] & inf_doc::FLAG_INTERNED != 0 {
            return inf_doc::intern::unintern(stored);
        }
        stored.to_vec()
    }

    /// Store `idoc` (canonical plain bytes) at `key`, tiering placement
    /// per ADR-0037 D2 and interning per ADR-0038 when enabled. Releases
    /// any replaced payload only after the write succeeds; frees this
    /// attempt's allocations on failure (leak-free abort).
    pub(crate) fn json_write_value(
        &mut self,
        key: &[u8],
        existing: Option<(ArenaAddr, usize)>,
        idoc: &[u8],
        meta: DocWriteMeta,
    ) -> Result<(), OpError> {
        let DocWriteMeta { lineage, version, expire_at_ms, cadence } = meta;
        if idoc.len() >= self.cfg.doc_morph_bytes_min {
            // Node tree, built from the plain form (trees never intern).
            let tape = TapeDoc::from_validated_bytes(idoc);
            let adoc = ArenaDoc::from_tape(&tape, &mut self.docs.arena).map_err(op_from_doc)?;
            let mem = adoc.report();
            let handle =
                encode_tree_handle(adoc.root_ref(), mem, idoc.len() as u32, cadence, lineage);
            let spec = RecordSpec {
                key,
                value: &handle,
                version,
                expire_at_ms,
                kind: RecordKind::JsonDoc,
            };
            match self.write_record_releasing(key, existing, spec) {
                Ok(()) => {
                    shift(&mut self.docs.domain.arena_bytes, 0, mem.node_bytes);
                    shift(&mut self.docs.domain.slack_bytes, 0, mem.slack_bytes);
                    self.docs.domain.docs_live += 1;
                    Ok(())
                }
                Err(e) => {
                    adoc.free(&mut self.docs.arena);
                    Err(e)
                }
            }
        } else {
            #[cfg(feature = "doc-intern-keys")]
            let interned: Option<Vec<u8>> =
                if self.cfg.doc_intern_keys { inf_doc::intern::intern(idoc) } else { None };
            #[cfg(feature = "doc-intern-keys")]
            let stored: &[u8] = interned.as_deref().unwrap_or(idoc);
            #[cfg(not(feature = "doc-intern-keys"))]
            let stored: &[u8] = idoc;
            let dict_len = dict_len_of(stored);
            if stored.len() <= self.cfg.doc_inline_bytes_max {
                let mut scratch = core::mem::take(&mut self.docs.scratch);
                scratch.clear();
                scratch.resize(VALUE_PREFIX_LEN, 0);
                write_prefix(&mut scratch, FORM_INLINE, cadence, lineage);
                scratch.extend_from_slice(stored);
                let spec = RecordSpec {
                    key,
                    value: &scratch,
                    version,
                    expire_at_ms,
                    kind: RecordKind::JsonDoc,
                };
                let outcome = self.write_record_releasing(key, existing, spec);
                self.docs.scratch = scratch;
                outcome?;
                shift(&mut self.docs.domain.inline_bytes, 0, stored.len());
                shift(&mut self.docs.domain.intern_bytes, 0, dict_len as usize);
                self.docs.domain.inline_docs += 1;
                self.docs.domain.docs_live += 1;
                Ok(())
            } else {
                // A same-size-class overwrite of an existing tape blob
                // rewrites the blob bytes in place instead of paying
                // alloc + copy + release + double accounting per op —
                // the SET path already reuses record slots this way, and
                // the asymmetry was the measured jset-only churn
                // (A/B: .artifacts/m3/jset-server-20260717/). Ordering:
                // the record write commits before blob bytes change, so
                // a refused write leaves the old document fully intact
                // (ADR-0037 D3); the class-local resize is reversible.
                if let Some((record_addr, record_len)) = existing
                    && let DocPayload::Blob { addr: blob_addr, len: blob_len } =
                        payload_of(&self.arena, record_addr, record_len)
                {
                    let old_blob_len = blob_len as usize;
                    let old_dict_len = dict_len_of(self.docs.arena.bytes(blob_addr, old_blob_len));
                    if self.docs.arena.resize_in_place(blob_addr, old_blob_len, stored.len()) {
                        let handle =
                            encode_tape_handle(blob_addr, stored.len() as u32, cadence, lineage);
                        let spec = RecordSpec {
                            key,
                            value: &handle,
                            version,
                            expire_at_ms,
                            kind: RecordKind::JsonDoc,
                        };
                        return match self.write_record_carrying(key, existing, spec) {
                            Ok(()) => {
                                self.docs
                                    .arena
                                    .bytes_mut(blob_addr, stored.len())
                                    .copy_from_slice(stored);
                                shift(&mut self.docs.domain.tape_bytes, old_blob_len, stored.len());
                                shift(
                                    &mut self.docs.domain.intern_bytes,
                                    old_dict_len as usize,
                                    dict_len as usize,
                                );
                                // One live document before and after: docs_live unchanged.
                                Ok(())
                            }
                            Err(e) => {
                                let restored = self.docs.arena.resize_in_place(
                                    blob_addr,
                                    stored.len(),
                                    old_blob_len,
                                );
                                debug_assert!(restored, "same-class blob resize is reversible");
                                Err(e)
                            }
                        };
                    }
                }
                let blob = self.docs.arena.alloc(stored.len()).ok_or(OpError::OutOfMemory)?;
                self.docs.arena.bytes_mut(blob, stored.len()).copy_from_slice(stored);
                let handle = encode_tape_handle(blob, stored.len() as u32, cadence, lineage);
                let spec = RecordSpec {
                    key,
                    value: &handle,
                    version,
                    expire_at_ms,
                    kind: RecordKind::JsonDoc,
                };
                match self.write_record_releasing(key, existing, spec) {
                    Ok(()) => {
                        shift(&mut self.docs.domain.tape_bytes, 0, stored.len());
                        shift(&mut self.docs.domain.intern_bytes, 0, dict_len as usize);
                        self.docs.domain.docs_live += 1;
                        Ok(())
                    }
                    Err(e) => {
                        self.docs.arena.free(blob, stored.len());
                        Err(e)
                    }
                }
            }
        }
    }
}

/// Map format-layer errors onto the store's vocabulary: arena pressure is
/// backpressure, size is the record bound; anything else out of a
/// validated document is an internal invariant.
#[cfg(feature = "doc")]
fn op_from_doc(e: DocError) -> OpError {
    match e {
        DocError::ArenaExhausted => OpError::OutOfMemory,
        DocError::TooLarge { .. } => OpError::TooLarge,
        other => {
            debug_assert!(false, "unexpected DocError from validated document: {other}");
            OpError::TooLarge
        }
    }
}

#[cfg(feature = "doc")]
fn op_from_apply(e: ApplyError) -> OpError {
    match e {
        ApplyError::Overflow => OpError::Overflow,
        ApplyError::NotANumber => OpError::NanOrInf,
        ApplyError::TooLarge => OpError::TooLarge,
        ApplyError::OutOfBounds | ApplyError::RootDelete | ApplyError::Eval(_) => {
            debug_assert!(false, "scalar fast path returned an unsupported error: {e}");
            OpError::TooLarge
        }
    }
}
