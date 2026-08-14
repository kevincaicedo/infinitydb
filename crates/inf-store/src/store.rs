//! `CellStore` (M0-S15 substrate, extended by M1-E1/E2): the
//! single-threaded, cell-local string engine — records in the
//! [`Arena`](inf_alloc::Arena), addresses in the
//! [`Index`](crate::index::Index), expiry lazy on read **plus** the M1
//! hierarchical [`TtlWheel`](crate::wheel) driven by budgeted
//! [`expire_tick`](CellStore::expire_tick) MAINTAIN slices.
//!
//! Every operation takes `now: Nanos` from the caller — time is injected
//! (L7), so the store is deterministic and DST-able. Memory accounting is
//! byte-exact by construction (L5): the arena tracks live/slack/resident,
//! the index reports its table bytes, the wheel reports pool + slot bytes,
//! and [`MemoryReport`] exposes the frozen attribution domains.
//!
//! Deviation from the freeze sketch (recorded in the ledger): mutating ops
//! return `Result<_, OpError>` where the sketch had bare values — arena
//! budget exhaustion (`OpError::OutOfMemory`) is a real outcome the command
//! layer must surface as an error reply, and panicking on allocation
//! pressure is forbidden by the engineering rules.

use inf_alloc::{Arena, ArenaAddr, ArenaConfig};
use inf_foundation::hash64;
use inf_foundation::time::Nanos;

use crate::doc::{self, DocStore};
use crate::evict::{self, EvictState, EvictStats, EvictionPolicy, Tracking};
use crate::index::Index;
use crate::record::{
    HEADER_LEN, MAX_EXPIRE_MS, MAX_KEY_LEN, MAX_VAL_LEN, RecordKind, RecordSpec, RecordView,
    TypeTag, flags_ref_decrement, flags_ref_saturate, flags_ref_write,
};
use crate::wheel::{ArmOutcome, TtlWheel};

pub use crate::wheel::ExpiryBudget;

/// Stable hash seed: deterministic across runs and cells (L7; DST oracles
/// rely on reproducible placement).
pub(crate) const HASH_SEED: u64 = 0x1AF1_D8A5_0DB5_EED1;

/// Configuration for [`CellStore::new`].
#[derive(Copy, Clone, Debug)]
pub struct StoreConfig {
    /// Record arena settings (chunk size, resident budget).
    pub arena: ArenaConfig,
    /// Index pre-sizing (entries before the first rehash).
    pub initial_keys: usize,
    /// Seed for the eviction RNG stream (Morris rolls, random-policy slots).
    /// Injected (L7); `Keyspace` derives a per-db stream from the node seed.
    pub evict_seed: u64,
    /// Document arena settings (ADR-0037; unused without the `doc` feature).
    pub doc_arena: ArenaConfig,
    /// Documents at or below this many idoc bytes live inline in the record
    /// value (ADR-0037 D2 — **Proposed** default, measured at S20).
    pub doc_inline_bytes_max: usize,
    /// Eligibility floor (idoc bytes) for tree-form residency. Since
    /// ADR-0046 ingest never auto-morphs (default `usize::MAX`): documents
    /// reside as tape at every size, because the mutation engine is
    /// tape-native and the tree form costs 1.85–4.3× tape RSS. Lowering
    /// this restores the pre-ADR-0046 ingest morph (forced-form A/B arms,
    /// forced-tree tests); `json_morph` remains the explicit seam.
    pub doc_morph_bytes_min: usize,
    /// Repeated-key interning experiment (ADR-0038) — default off, and off
    /// for the M3 release regardless of the A/B (plan §2 cut line).
    pub doc_intern_keys: bool,
    /// Maximum JSON nesting depth accepted at document ingest (M3-S07;
    /// RedisJSON-parity default 128). Configurable downward only — the
    /// format ceiling (`inf_doc::limits::DEPTH_MAX`) clamps it. Per-
    /// namespace override rides the S11 command surface (the parser is
    /// constructed with the namespace's resolved limits there).
    pub doc_max_depth: usize,
    /// Maximum document size at ingest (M3-S07), applied to **both** axes
    /// of the dual bound: input text bytes (reject before the structural
    /// index allocates) and encoded idoc bytes (incremental during stage 2
    /// — small-token documents can encode larger than their text). Default
    /// = the 16 MiB − 1 format ceiling (record `vlen`, u24 skip lengths);
    /// configurable downward only.
    pub doc_max_bytes: usize,
    /// Maximum JSONPath text bytes per command (M3-S11; ADR-0040 D6's
    /// `doc_max_path_bytes`). The S10 program cache enforces it before
    /// the lookup so lower-capped namespaces never hit an over-cap cached
    /// program; the 64 KiB format ceiling clamps upward drift.
    pub doc_max_path_bytes: usize,
    /// Maximum path-evaluation match set per command (M3-S11 wires the
    /// key ADR-0040 D6 named): a declared product limit — the
    /// pathological `$..*` mutation otherwise plans unboundedly.
    pub doc_max_path_matches: u32,
}

impl Default for StoreConfig {
    fn default() -> StoreConfig {
        StoreConfig {
            arena: ArenaConfig::default(),
            initial_keys: 0,
            evict_seed: 0,
            doc_arena: ArenaConfig::default(),
            doc_inline_bytes_max: 512,
            doc_morph_bytes_min: usize::MAX,
            doc_intern_keys: false,
            doc_max_depth: DOC_MAX_DEPTH_DEFAULT,
            doc_max_bytes: DOC_MAX_BYTES_DEFAULT,
            doc_max_path_bytes: DOC_MAX_PATH_BYTES_DEFAULT,
            doc_max_path_matches: DOC_MAX_PATH_MATCHES_DEFAULT,
        }
    }
}

/// M3-S07/S11 limit defaults as literals — slim builds have no `inf_doc`
/// to name the ceilings; the doc-feature assert below makes drift a
/// compile error.
const DOC_MAX_DEPTH_DEFAULT: usize = 128;
const DOC_MAX_BYTES_DEFAULT: usize = 0xFF_FFFF;
const DOC_MAX_PATH_BYTES_DEFAULT: usize = 4096;
const DOC_MAX_PATH_MATCHES_DEFAULT: u32 = 65_536;

#[cfg(feature = "doc")]
const _: () = {
    assert!(DOC_MAX_DEPTH_DEFAULT == inf_doc::limits::DEPTH_MAX);
    assert!(DOC_MAX_BYTES_DEFAULT == inf_doc::limits::DOC_BYTES_MAX);
    assert!(DOC_MAX_PATH_BYTES_DEFAULT == inf_doc::path::PATH_BYTES_DEFAULT);
};

/// Typed operation failure surfaced to the command layer.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum OpError {
    /// Value is not a 64-bit integer string (INCR family).
    NotInt,
    /// Integer overflow/underflow (INCR family).
    Overflow,
    /// Value is not a parseable float (INCRBYFLOAT).
    NotFloat,
    /// Float op produced NaN/Infinity (INCRBYFLOAT).
    NanOrInf,
    /// Arena budget exhausted — backpressure, not a panic.
    OutOfMemory,
    /// Key or value exceeds the v0 record bounds.
    TooLarge,
    /// The record's type does not admit this operation (ADR-0037 D6): a
    /// string mutation on a document, a `json_*` call on a string, or a
    /// document edit in a non-editable physical form. Maps to Redis
    /// `WRONGTYPE` at the command layer.
    WrongType,
    /// Disk admission refused a tiered placement (M4-S21, ADR-0063 D1):
    /// the namespace's disk budget is exhausted or the device is.
    /// Refusal mutates nothing; reads, deletes, expiry, and in-place
    /// updates proceed. Maps to the `DISKFULL` extension error at the
    /// command layer.
    DiskFull(DiskFullCause),
    /// The index-maintenance bracket refused the mutation before any
    /// state changed (M4.5-S04, ADR-0072 D7.1) — plan-then-commit
    /// reservation or the entry-set cap.
    IndexMaintenance(crate::index_maint::IdxMaintRefusal),
}

/// Why disk admission refused (ADR-0063 D1) — the error payload carries
/// the numbers the operator needs, the `TierVaLimitExceeded` structured
/// precedent.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum DiskFullCause {
    /// `DISK-BUDGET` exhausted: the admission projection reached
    /// `budget − reserve`. Snapshot values from the last admission
    /// refresh (ADR-0063 D2 — the cached-verdict cadence).
    Budget {
        /// `disk_used` at the last refresh (tier files + extents).
        used: u64,
        /// The configured `DISK-BUDGET`.
        budget: u64,
    },
    /// The device itself refused a tier write with ENOSPC (the latched
    /// leg — cleared automatically by the next successful flush
    /// barrier).
    Device,
}

/// `SET` condition (NX/XX shapes).
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub enum SetCond {
    #[default]
    Always,
    /// Apply only if absent (`SETNX`, `SET .. NX`).
    IfAbsent,
    /// Apply only if present (`SET .. XX`).
    IfPresent,
}

/// `SET` expiry behavior.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub enum SetExpire {
    /// Drop any existing TTL (plain `SET` semantics).
    #[default]
    Clear,
    /// Keep the existing TTL (`SET .. KEEPTTL`).
    Keep,
    /// Absolute deadline (`SET .. EX/PX/EXAT/PXAT`).
    At(Nanos),
}

/// Options for [`CellStore::set`].
#[derive(Copy, Clone, Debug, Default)]
pub struct SetOptions {
    pub cond: SetCond,
    pub expire: SetExpire,
    /// Return the previous value (`SET .. GET`, `GETSET`).
    pub get_old: bool,
}

/// Result of [`CellStore::set`]. `old` is populated only when
/// `SetOptions::get_old` requested it (it costs a copy).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SetOutcome {
    Applied { old: Option<Vec<u8>> },
    Skipped { old: Option<Vec<u8>> },
}

/// Post-mutation view of one key for durable effect emission (M2-S08):
/// borrowed value bytes plus the store's absolute internal deadline in
/// milliseconds (`None` = no TTL). See [`CellStore::post_image`].
#[derive(Copy, Clone, Debug)]
pub struct PostImage<'a> {
    pub value: &'a [u8],
    pub expire_at_ms: Option<u64>,
}

/// Canonical value emitted by the fuzzy-checkpoint/digest walker. Store
/// handles and cadence bytes never escape this boundary (ADR-0043 D7).
#[derive(Copy, Clone, Debug)]
pub enum CheckpointImage<'a> {
    String(&'a [u8]),
    #[cfg(feature = "doc")]
    JsonDoc {
        lineage: inf_log::DocLineage,
        version: u32,
        idoc: &'a [u8],
    },
}

/// One post-command durable full image, resolved exactly once. The string
/// arm borrows store bytes; the document arm owns canonical idoc because
/// tree freeze/key uninterning crosses the store-local representation seam.
pub enum LogFullImage<'a> {
    String(PostImage<'a>),
    #[cfg(feature = "doc")]
    JsonDoc(crate::doc::JsonLogDecision),
}

/// `EXPIRE` condition flags (NX/XX/GT/LT).
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub enum ExpireCond {
    #[default]
    Always,
    /// `NX`: only when no expiry exists.
    IfNoExpiry,
    /// `XX`: only when an expiry exists.
    IfHasExpiry,
    /// `GT`: only when new > current (no expiry counts as infinite).
    IfGreater,
    /// `LT`: only when new < current.
    IfLess,
}

/// `TTL`/`PTTL` answer.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Ttl {
    Missing,
    NoExpiry,
    Ms(u64),
}

/// TTL side effect of `GETEX` (M1-S01).
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub enum TtlUpdate {
    /// Plain `GETEX` — read only.
    #[default]
    Keep,
    /// `GETEX .. PERSIST`.
    Persist,
    /// `GETEX .. EX/PX/EXAT/PXAT` absolute deadline.
    At(Nanos),
}

/// `OBJECT ENCODING` answer for string records (M1-S02). Derived from the
/// value plus the record's raw flag — matching Redis's `int`/`embstr`/`raw`
/// classification (embstr threshold 44, byte-surgery forces raw).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Encoding {
    Int,
    Embstr,
    Raw,
}

impl Encoding {
    pub fn name(self) -> &'static str {
        match self {
            Encoding::Int => "int",
            Encoding::Embstr => "embstr",
            Encoding::Raw => "raw",
        }
    }
}

/// A record exported for cross-db COPY (M1-S08). Document exports carry
/// canonical frozen tape bytes as `value` (ADR-0037 D3).
#[derive(Clone, Debug)]
pub(crate) struct ExportedRecord {
    pub value: Vec<u8>,
    pub expire_at_ms: Option<u64>,
    pub kind: RecordKind,
}

/// `COPY` outcome (M1-S02).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum CopyResult {
    Copied,
    SourceMissing,
    DestinationExists,
}

/// One [`CellStore::expire_tick`] slice result (M1-S05). `lag_ms` is the
/// `expiry_debt` backlog metric: how far the wheel cursor still trails `now`
/// after the slice (0 = caught up); the MAINTAIN caller escalates budgets on
/// it while foreground latency stays protected.
#[derive(Copy, Clone, Default, Debug)]
pub struct ExpiryStats {
    /// Records actually reaped by this slice.
    pub reaped: u64,
    /// Wheel entries that no longer matched a live expired record.
    pub stale: u64,
    /// Cursor work performed (ms steps + fast-forward jumps).
    pub steps: u32,
    /// Backlog: milliseconds the wheel still trails `now` (0 = caught up).
    pub lag_ms: u64,
    /// Live wheel entries after the slice.
    pub armed: u64,
}

/// Always-on store counters (feeds `INFO stats`/`keyspace` and the M1
/// expiry oracles). Plain fields — the store is single-threaded (L1).
#[derive(Copy, Clone, Default, Debug)]
pub struct StoreStats {
    pub keyspace_hits: u64,
    pub keyspace_misses: u64,
    /// Reaped by expire-on-read.
    pub expired_lazy: u64,
    /// Reaped by wheel slices.
    pub expired_active: u64,
    /// Live records currently carrying a TTL (`INFO keyspace` `expires=`).
    pub ttl_live: u64,
    /// Wheel entries that fired without a matching expired record.
    pub wheel_stale: u64,
    /// TTL writes that could not arm the wheel (pool cap) — lazy-only keys.
    pub wheel_fallback: u64,
    /// Records evicted under memory pressure (M1-S06; `INFO evicted_keys`).
    pub evicted_keys: u64,
}

/// Frozen memory attribution domains (tripwire names, M0 §3.2; document
/// fields join additively at M3-S19). Logical document partitions and
/// overlays are exposed for diagnosis; [`MemoryReport::attributed_bytes`]
/// sums resident partitions only, so intern/tree slack are never counted
/// twice against RSS.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct MemoryReport {
    pub records_live_bytes: u64,
    pub records_slack_bytes: u64,
    pub records_resident_bytes: u64,
    pub index_bytes: u64,
    pub wheel_bytes: u64,
    /// Eviction-engine footprint: the 8 KiB CMS while an LFU policy is
    /// selected, 0 otherwise (M1-S06 L5 domain).
    pub evict_bytes: u64,
    /// Live doc-arena tape blobs (partition with `doc_arena_bytes`).
    pub doc_tape_bytes: u64,
    /// Live doc-arena tree storage, including tree growth slack.
    pub doc_arena_bytes: u64,
    /// Mapped bytes of the document arena (RSS-side partition).
    pub doc_resident_bytes: u64,
    /// Intern dictionaries within tape bytes (diagnostic overlay).
    pub doc_intern_bytes: u64,
    /// Unused tree capacity within `doc_arena_bytes` (diagnostic overlay).
    pub doc_slack_bytes: u64,
    /// Retained parser/ingest/freeze/effect scratch owned by this report's
    /// store or cell. `CellStore::report` contributes store-local scratch;
    /// the command plane adds its one-per-cell buffers.
    pub doc_scratch_bytes: u64,
    /// One bounded path-program cache per cell; zero in a store-only report.
    pub doc_path_cache_bytes: u64,
    /// Index-tree reserved bytes (M4.5-S03 L5 domain, ADR-0075 D6). The
    /// registry is keyspace-owned, so a store-only report holds zero;
    /// [`Keyspace::report`](crate::Keyspace::report) folds it in.
    pub idx_tree_bytes: u64,
    /// Slack inside `idx_tree_bytes` (diagnostic overlay, like
    /// `doc_slack_bytes` — never double-counted against RSS).
    pub idx_slack_bytes: u64,
    pub live_records: u64,
    pub docs_live: u64,
}

impl MemoryReport {
    /// RSS-side sum of disjoint resident domains. Logical partitions and
    /// overlays remain visible fields but deliberately do not join this sum.
    #[must_use]
    pub fn attributed_bytes(&self) -> u64 {
        self.records_resident_bytes
            + self.index_bytes
            + self.wheel_bytes
            + self.evict_bytes
            + self.doc_resident_bytes
            + self.doc_scratch_bytes
            + self.doc_path_cache_bytes
            + self.idx_tree_bytes
    }
}

/// One cell's keyspace slice. Single-threaded by construction (owns a
/// `!Send` arena); all time is injected.
pub struct CellStore {
    pub(crate) arena: Arena,
    pub(crate) index: Index,
    wheel: TtlWheel,
    pub(crate) stats: StoreStats,
    pub(crate) evict: EvictState,
    /// Document arena + domain counters (ADR-0037; no-op without `doc`).
    pub(crate) docs: DocStore,
    /// Index attach block (M4.5-S04, ADR-0076 D1): this namespace's live
    /// indexes and their trees, synced from the registry at DDL
    /// transitions. A zero-index store pays one cached branch; slim
    /// builds get the folded-away stub.
    pub(crate) idx: crate::index_maint::CellIndexes,
    pub(crate) cfg: StoreConfig,
}

impl CellStore {
    pub fn new(cfg: StoreConfig) -> CellStore {
        let evict = EvictState { rng: cfg.evict_seed, ..EvictState::default() };
        CellStore {
            arena: Arena::new(cfg.arena),
            index: Index::with_capacity(cfg.initial_keys.max(64)),
            // Cursor 0: the first tick fast-forwards to `now` (empty wheel).
            wheel: TtlWheel::new(0),
            stats: StoreStats::default(),
            evict,
            docs: DocStore::new(&cfg),
            idx: crate::index_maint::CellIndexes::default(),
            cfg,
        }
    }

    /// Stable key hash — also what the batch pipeline computes up front.
    #[inline]
    pub fn hash_key(key: &[u8]) -> u64 {
        hash64(key, HASH_SEED)
    }

    /// Prefetch the index probe path for a pre-hashed key (PARSE→hash→
    /// prefetch→EXECUTE pipeline, L3/L4).
    #[inline]
    pub fn prefetch(&self, key_hash: u64) {
        self.index.prefetch(key_hash);
    }

    /// Unverified-probe record prefetch — §7.3 phase 2 as a standalone step
    /// (the fabric-apply staged batch, M2.5 Phase H / ADR-0005 shape): find
    /// the first fingerprint candidate and prefetch its record head lines.
    /// The caller executes the exact path afterwards, so a fingerprint
    /// collision (≈2⁻²²) or a reap between prefetch and execute costs
    /// nothing but the wasted lines.
    #[inline]
    pub fn probe_prefetch(&self, key_hash: u64) {
        if let Some(addr) = self.index.find(key_hash, |_| true) {
            let head = self.arena.bytes(addr, HEADER_LEN).as_ptr();
            inf_simd::prefetch_read(head);
            inf_simd::prefetch_read(head.wrapping_add(64));
        }
    }

    /// Document-root prefetch — ADR-0044 phase 3. Revisit the same
    /// fingerprint candidate after its record lines had a full batch pass
    /// to arrive, then hint the first tape lines. This is non-semantic: the
    /// exact execution path still verifies the key and handles expiry.
    #[inline]
    pub fn prefetch_doc_root(&self, key_hash: u64) {
        if let Some(addr) = self.index.find(key_hash, |_| true) {
            self.docs.prefetch_root(record_at(&self.arena, addr));
        }
    }

    /// Live key count (post-expiry keys may still be counted until read or
    /// wheel-reaped) — `DBSIZE`.
    #[inline]
    pub fn len(&self) -> usize {
        self.index.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.index.len() == 0
    }

    /// Always-on counters snapshot.
    #[inline]
    pub fn stats(&self) -> StoreStats {
        self.stats
    }

    /// `CONFIG RESETSTAT`: zero the lifetime counters; the live-state census
    /// (`ttl_live`) is structural and survives.
    pub fn reset_stats(&mut self) {
        let ttl_live = self.stats.ttl_live;
        self.stats = StoreStats { ttl_live, ..StoreStats::default() };
    }

    /// Byte-exact attribution snapshot (L5).
    pub fn report(&self) -> MemoryReport {
        let arena = self.arena.report();
        let docs = self.docs.report();
        let idx = self.idx.memory();
        MemoryReport {
            records_live_bytes: arena.live_bytes,
            records_slack_bytes: arena.slack_bytes,
            records_resident_bytes: arena.resident_bytes,
            index_bytes: self.index.memory_bytes() as u64,
            wheel_bytes: (self.wheel.pool_bytes() + self.wheel.table_bytes()) as u64,
            evict_bytes: self.evict.bytes() as u64,
            doc_tape_bytes: docs.domain.tape_bytes,
            doc_arena_bytes: docs.domain.arena_bytes,
            doc_resident_bytes: docs.resident_bytes,
            doc_intern_bytes: docs.domain.intern_bytes,
            doc_slack_bytes: docs.domain.slack_bytes,
            doc_scratch_bytes: docs.scratch_bytes,
            doc_path_cache_bytes: 0,
            idx_tree_bytes: idx.idx_tree_bytes,
            idx_slack_bytes: idx.idx_slack_bytes,
            live_records: arena.live_allocs,
            docs_live: docs.domain.docs_live,
        }
    }

    /// L5 fold of this store's attached index trees (M4.5-S04, ADR-0076
    /// D1 — the S03 `idx_*` domains, source moved with tree custody).
    pub fn idx_memory(&self) -> crate::index_registry::IndexMemory {
        self.idx.memory()
    }

    // ---- reads (expire-on-read makes them `&mut`) ----

    /// `GET`.
    pub fn get(&mut self, key: &[u8], now: Nanos) -> Option<&[u8]> {
        self.get_with_hash(key, Self::hash_key(key), now)
    }

    /// `GET` with a precomputed hash — the batch pipeline path: EXECUTE
    /// hashes and [`prefetch`](Self::prefetch)es a whole parse batch first,
    /// then executes with the hashes it already has (L3/L4).
    pub fn get_with_hash(&mut self, key: &[u8], hash: u64, now: Nanos) -> Option<&[u8]> {
        debug_assert_eq!(hash, Self::hash_key(key));
        let Some((addr, len)) = self.resolve_hashed(key, hash, now) else {
            self.stats.keyspace_misses += 1;
            return None;
        };
        self.stats.keyspace_hits += 1;
        Some(RecordView::new(self.arena.bytes(addr, len)).value())
    }

    /// `GET` at the command layer (M3-S11 generic-command matrix): the
    /// string read that refuses documents. [`get`](Self::get) stays the
    /// raw store-layer read (gather internals, tests); Redis `GET` on a
    /// document key is `WRONGTYPE`, and the branch costs one compare on
    /// the already-loaded header.
    pub fn get_str(&mut self, key: &[u8], now: Nanos) -> Result<Option<&[u8]>, OpError> {
        let Some((addr, len)) = self.resolve(key, now) else {
            self.stats.keyspace_misses += 1;
            return Ok(None);
        };
        self.stats.keyspace_hits += 1;
        let view = RecordView::new(self.arena.bytes(addr, len));
        if view.type_tag() != TypeTag::String {
            return Err(OpError::WrongType);
        }
        Ok(Some(view.value()))
    }

    /// Diagnostics: index groups visited to terminate a probe for `key`
    /// (probe-length histogram artifact, M0-S14 AC).
    pub fn probe_groups(&self, key: &[u8]) -> usize {
        let arena = &self.arena;
        self.index.probe_groups(Self::hash_key(key), |addr| record_at(arena, addr).key() == key)
    }

    /// Batched `GET` — the full §7.3 pipeline. Per 32-key chunk:
    /// 1. hash every key and prefetch its probe lines (ctrl + slots);
    /// 2. probe to the first 22-bit-fingerprint candidate (no record touch)
    ///    and prefetch the candidate's record lines;
    /// 3. verify keys and read values — by now both the probe and record
    ///    lines are (likely) in cache, so misses overlapped across the
    ///    whole chunk instead of serializing per key.
    ///
    /// Fingerprint collisions (≈2⁻²² per probe) and expired records fall
    /// back to the exact per-key path; results arrive via `out(i, value)`.
    pub fn get_many(
        &mut self,
        keys: &[&[u8]],
        now: Nanos,
        mut out: impl FnMut(usize, Option<&[u8]>),
    ) {
        const CHUNK: usize = 32;
        let mut hashes = [0u64; CHUNK];
        let mut candidates: [Option<ArenaAddr>; CHUNK] = [None; CHUNK];
        for (chunk_at, chunk) in keys.chunks(CHUNK).enumerate() {
            let base = chunk_at * CHUNK;
            for (i, key) in chunk.iter().enumerate() {
                hashes[i] = Self::hash_key(key);
                self.index.prefetch(hashes[i]);
            }
            for (i, _) in chunk.iter().enumerate() {
                // Unverified probe: first fingerprint match, zero record reads.
                candidates[i] = self.index.find(hashes[i], |_| true);
                if let Some(addr) = candidates[i] {
                    let head = self.arena.bytes(addr, HEADER_LEN).as_ptr();
                    inf_simd::prefetch_read(head);
                    inf_simd::prefetch_read(head.wrapping_add(64));
                }
            }
            // A reap (expired record freed mid-chunk) invalidates any later
            // candidate holding the same address (duplicate keys in one
            // batch) — those redo the exact path instead of reading a freed
            // slot.
            let mut redo = [false; CHUNK];
            for i in 0..chunk.len() {
                let key = chunk[i];
                let exact_path = match candidates[i] {
                    None => {
                        self.stats.keyspace_misses += 1;
                        out(base + i, None);
                        continue;
                    }
                    Some(_) if redo[i] => true,
                    Some(addr) => {
                        let view = record_at(&self.arena, addr);
                        if view.key() != key {
                            true // fingerprint collision (≈2⁻²²)
                        } else if view.type_tag() != TypeTag::String {
                            // MGET answers nil for non-string keys (Redis
                            // semantics; the S11 matrix pins documents).
                            self.stats.keyspace_hits += 1;
                            out(base + i, None);
                            continue;
                        } else if view.is_expired(now) {
                            let len = view.encoded_len();
                            self.free_record(hashes[i], addr, len);
                            self.note_reap_lazy();
                            mark_stale(&mut redo, &candidates, i, addr);
                            self.stats.keyspace_misses += 1;
                            out(base + i, None);
                            continue;
                        } else {
                            self.stats.keyspace_hits += 1;
                            out(base + i, Some(view.value()));
                            self.touch_access(hashes[i], addr);
                            continue;
                        }
                    }
                };
                debug_assert!(exact_path);
                match self.resolve_hashed(key, hashes[i], now) {
                    Some((addr, len)) => {
                        let view = RecordView::new(self.arena.bytes(addr, len));
                        self.stats.keyspace_hits += 1;
                        let value = (view.type_tag() == TypeTag::String).then(|| view.value());
                        out(base + i, value);
                    }
                    None => {
                        // The exact path may itself have reaped an expired
                        // record; invalidate matching later candidates.
                        if let Some(addr) = candidates[i] {
                            mark_stale(&mut redo, &candidates, i, addr);
                        }
                        self.stats.keyspace_misses += 1;
                        out(base + i, None);
                    }
                }
            }
        }

        fn mark_stale(
            redo: &mut [bool],
            candidates: &[Option<ArenaAddr>],
            after: usize,
            addr: ArenaAddr,
        ) {
            for j in after + 1..candidates.len() {
                if candidates[j] == Some(addr) {
                    redo[j] = true;
                }
            }
        }
    }

    /// `EXISTS` (single key).
    pub fn exists(&mut self, key: &[u8], now: Nanos) -> bool {
        self.resolve(key, now).is_some()
    }

    /// `STRLEN`. Missing keys are length 0; non-string records are
    /// `WrongType` (M3-S11 generic-command matrix).
    pub fn strlen(&mut self, key: &[u8], now: Nanos) -> Result<u64, OpError> {
        match self.resolve(key, now) {
            Some((addr, len)) => {
                let view = RecordView::new(self.arena.bytes(addr, len));
                if view.type_tag() != TypeTag::String {
                    return Err(OpError::WrongType);
                }
                Ok(view.vlen() as u64)
            }
            None => Ok(0),
        }
    }

    /// `TYPE`.
    pub fn type_of(&mut self, key: &[u8], now: Nanos) -> Option<TypeTag> {
        let (addr, len) = self.resolve(key, now)?;
        Some(RecordView::new(self.arena.bytes(addr, len)).type_tag())
    }

    /// `TTL`/`PTTL` in milliseconds.
    pub fn ttl(&mut self, key: &[u8], now: Nanos) -> Ttl {
        match self.resolve(key, now) {
            None => Ttl::Missing,
            Some((addr, len)) => {
                match RecordView::new(self.arena.bytes(addr, len)).expire_at_ms() {
                    None => Ttl::NoExpiry,
                    Some(at) => Ttl::Ms(at.saturating_sub(now.0 / 1_000_000)),
                }
            }
        }
    }

    /// Absolute expiry deadline in clock milliseconds (`EXPIRETIME`).
    pub fn expire_at(&mut self, key: &[u8], now: Nanos) -> Ttl {
        match self.resolve(key, now) {
            None => Ttl::Missing,
            Some((addr, len)) => {
                match RecordView::new(self.arena.bytes(addr, len)).expire_at_ms() {
                    None => Ttl::NoExpiry,
                    Some(at) => Ttl::Ms(at),
                }
            }
        }
    }

    /// `GETRANGE`/`SUBSTR` — Redis index semantics (negatives count from the
    /// end, ranges clamp, inverted ranges are empty).
    pub fn get_range(
        &mut self,
        key: &[u8],
        start: i64,
        end: i64,
        now: Nanos,
    ) -> Result<&[u8], OpError> {
        let Some((addr, len)) = self.resolve(key, now) else {
            self.stats.keyspace_misses += 1;
            return Ok(b"");
        };
        self.stats.keyspace_hits += 1;
        let view = RecordView::new(self.arena.bytes(addr, len));
        if view.type_tag() != TypeTag::String {
            return Err(OpError::WrongType);
        }
        let value = view.value();
        let n = value.len() as i64;
        let from = if start < 0 { (n + start).max(0) } else { start };
        let to = if end < 0 { (n + end).max(0) } else { end }.min(n - 1);
        if n == 0 || from > to || from >= n {
            return Ok(b"");
        }
        Ok(&value[from as usize..=to as usize])
    }

    /// `OBJECT ENCODING` view: the encoding plus the parsed integer when
    /// int-encoded (drives Redis's shared-integer REFCOUNT answer).
    pub fn object_encoding(&mut self, key: &[u8], now: Nanos) -> Option<(Encoding, Option<i64>)> {
        let (addr, len) = self.resolve(key, now)?;
        let view = RecordView::new(self.arena.bytes(addr, len));
        if !view.is_raw()
            && let Ok(v) = parse_int(view.value())
        {
            return Some((Encoding::Int, Some(v)));
        }
        if view.is_raw() || view.vlen() > 44 {
            Some((Encoding::Raw, None))
        } else {
            Some((Encoding::Embstr, None))
        }
    }

    // ---- writes ----

    /// `SET` family. See [`SetOptions`]/[`SetOutcome`].
    pub fn set(
        &mut self,
        key: &[u8],
        value: &[u8],
        opts: SetOptions,
        now: Nanos,
    ) -> Result<SetOutcome, OpError> {
        check_bounds(key, value)?;
        let existing = self.resolve(key, now);
        let old_view = existing.map(|(addr, len)| RecordView::new(self.arena.bytes(addr, len)));
        // Plain SET is a universal overwrite (ADR-0037 D6), but the GET
        // arm (GETSET / SET…GET) reads the old value *as a string* —
        // Redis errors WRONGTYPE before writing, and so do we.
        if opts.get_old && old_view.is_some_and(|v| v.type_tag() != TypeTag::String) {
            return Err(OpError::WrongType);
        }
        let old_deadline = old_view.and_then(|v| v.expire_at_ms());
        let old_value = if opts.get_old { old_view.map(|v| v.value().to_vec()) } else { None };
        let applies = match opts.cond {
            SetCond::Always => true,
            SetCond::IfAbsent => existing.is_none(),
            SetCond::IfPresent => existing.is_some(),
        };
        if !applies {
            return Ok(SetOutcome::Skipped { old: old_value });
        }
        let version = old_view.map_or(1, |v| v.version().wrapping_add(1));
        let expire_at_ms = match opts.expire {
            SetExpire::Clear => None,
            SetExpire::Keep => old_deadline,
            SetExpire::At(at) => Some((at.0 / 1_000_000).min(MAX_EXPIRE_MS)),
        };
        let spec = RecordSpec {
            key,
            value,
            version,
            expire_at_ms,
            kind: RecordKind::String { raw: false },
        };
        self.write_record(key, existing, spec)?;
        self.note_ttl(old_deadline.is_some(), expire_at_ms.is_some());
        if let Some(ms) = expire_at_ms
            && old_deadline != Some(ms)
        {
            self.arm_wheel(Self::hash_key(key), ms);
        }
        Ok(SetOutcome::Applied { old: old_value })
    }

    /// `DEL` (single key). True if the key existed.
    pub fn del(&mut self, key: &[u8], now: Nanos) -> bool {
        match self.resolve(key, now) {
            Some((addr, len)) => {
                let had_ttl = RecordView::new(self.arena.bytes(addr, len)).expire_at_ms().is_some();
                self.free_record(Self::hash_key(key), addr, len);
                self.note_ttl(had_ttl, false);
                true
            }
            None => false,
        }
    }

    /// `GETDEL`: the value, removing the key. String records only — a
    /// document's value bytes are a physical handle, never a reply
    /// (ADR-0037 D6); the command layer maps the `None`-vs-error split.
    pub fn getdel(&mut self, key: &[u8], now: Nanos) -> Option<Vec<u8>> {
        let (addr, len) = self.resolve(key, now)?;
        let view = RecordView::new(self.arena.bytes(addr, len));
        if view.type_tag() != TypeTag::String {
            return None;
        }
        let value = view.value().to_vec();
        let had_ttl = view.expire_at_ms().is_some();
        self.free_record(Self::hash_key(key), addr, len);
        self.note_ttl(had_ttl, false);
        Some(value)
    }

    // ---- durable-namespace hooks (M2-S08, ADR-0015 D5/D7) ----

    /// Post-mutation snapshot of one key for durable effect emission: the
    /// live value bytes plus the store's absolute internal deadline, read
    /// **without** access-tracking or expire-on-read side effects (staging
    /// a log record must not perturb LRU/LFU the way a client read does).
    /// An expired-but-unreaped key reads as absent — correct for emission:
    /// its deadline already passed, so the post-image is "gone".
    pub fn post_image(&self, key: &[u8], now: Nanos) -> Option<PostImage<'_>> {
        let hash = Self::hash_key(key);
        let arena = &self.arena;
        let addr = self.index.find(hash, |addr| record_at(arena, addr).key() == key)?;
        let view = record_at(arena, addr);
        if view.is_expired(now) {
            return None;
        }
        // Documents emit DocFull through the typed checkpoint/log-image
        // path (ADR-0043 D7) — a handle is a per-boot artifact, never a
        // durable post-image. The tripwire keeps that wiring impossible to
        // bypass through the string effect seam.
        debug_assert_eq!(
            view.type_tag(),
            TypeTag::String,
            "document post-images freeze at the walker (M3-S17)"
        );
        Some(PostImage { value: view.value(), expire_at_ms: view.expire_at_ms() })
    }

    /// Exact canonical value bytes a full durable image would carry,
    /// resolved once and without access tracking. Used only for admission.
    pub fn log_image_bytes(&self, key: &[u8], now: Nanos) -> Option<usize> {
        let hash = Self::hash_key(key);
        let arena = &self.arena;
        let addr = self.index.find(hash, |addr| record_at(arena, addr).key() == key)?;
        let view = record_at(arena, addr);
        if view.is_expired(now) {
            return None;
        }
        match view.type_tag() {
            TypeTag::String => Some(view.value().len()),
            TypeTag::JsonDoc => {
                #[cfg(feature = "doc")]
                {
                    Some(self.json_canonical_len(view) as usize)
                }
                #[cfg(not(feature = "doc"))]
                unreachable!("JsonDoc records cannot exist without the doc feature")
            }
            // Written only by TieredTable (M4-S17, ADR-0061 D2) — one in
            // a memory-mode arena is a record-lifecycle bug.
            TypeTag::StringExtent => {
                unreachable!("StringExtent records exist only in tiered namespaces")
            }
        }
    }

    /// Resolve and materialize the post-command full image once. Document
    /// cadence resets as part of consuming this image for the log.
    pub fn log_full_image(&mut self, key: &[u8], now: Nanos) -> Option<LogFullImage<'_>> {
        let hash = Self::hash_key(key);
        let arena = &self.arena;
        let addr = self.index.find(hash, |addr| record_at(arena, addr).key() == key)?;
        let (_encoded_len, kind, expired) = {
            let view = record_at(&self.arena, addr);
            (view.encoded_len(), view.type_tag(), view.is_expired(now))
        };
        if expired {
            return None;
        }
        match kind {
            TypeTag::String => {
                let view = record_at(&self.arena, addr);
                Some(LogFullImage::String(PostImage {
                    value: view.value(),
                    expire_at_ms: view.expire_at_ms(),
                }))
            }
            TypeTag::StringExtent => {
                unreachable!("StringExtent records exist only in tiered namespaces")
            }
            TypeTag::JsonDoc => {
                #[cfg(feature = "doc")]
                {
                    self.json_log_full_resolved(addr, _encoded_len).map(LogFullImage::JsonDoc)
                }
                #[cfg(not(feature = "doc"))]
                unreachable!("JsonDoc records cannot exist without the doc feature")
            }
        }
    }

    /// Replay upsert (blind idempotent post-image apply — ADR-0011 D4).
    /// Clears any TTL; a following `ExpireAt` record re-arms it, exactly
    /// mirroring emission order. Never consults eviction pressure: the OOM
    /// gate is a Keyspace-level DENYOOM concern and replay must not be
    /// refused by `maxmemory` (recovery degrades loudly on real allocation
    /// failure instead).
    ///
    /// # Errors
    /// Arena/bounds failures only (`OpError::TooLarge`/`OutOfMemory`).
    pub fn replay_set(&mut self, key: &[u8], value: &[u8], now: Nanos) -> Result<(), OpError> {
        self.set(key, value, SetOptions::default(), now).map(|_| ())
    }

    /// Replay delete. Absent keys are a no-op (idempotent re-apply).
    pub fn replay_del(&mut self, key: &[u8], now: Nanos) {
        let _ = self.del(key, now);
    }

    /// Replay TTL arm at an absolute internal deadline. Absent keys are a
    /// no-op (idempotent re-apply of a delete-then-expire suffix).
    pub fn replay_expire_at(&mut self, key: &[u8], at: Nanos, now: Nanos) {
        let _ = self.expire(key, Some(at), ExpireCond::Always, now);
    }

    /// `GETEX`: the value, with an optional TTL side effect (M1-S01). A
    /// past deadline deletes the key after the read (Redis semantics).
    pub fn get_ex(&mut self, key: &[u8], update: TtlUpdate, now: Nanos) -> Option<Vec<u8>> {
        let value = {
            let Some((addr, len)) = self.resolve(key, now) else {
                self.stats.keyspace_misses += 1;
                return None;
            };
            let view = RecordView::new(self.arena.bytes(addr, len));
            if view.type_tag() != TypeTag::String {
                // Documents never answer through the string read API
                // (ADR-0037 D6); the command layer owns WRONGTYPE replies.
                return None;
            }
            self.stats.keyspace_hits += 1;
            view.value().to_vec()
        };
        match update {
            TtlUpdate::Keep => {}
            TtlUpdate::Persist => {
                self.expire(key, None, ExpireCond::Always, now);
            }
            TtlUpdate::At(at) => {
                self.expire(key, Some(at), ExpireCond::Always, now);
            }
        }
        Some(value)
    }

    /// `INCR`/`DECR`/`INCRBY`/`DECRBY` (delta may be negative).
    pub fn incr_by(&mut self, key: &[u8], delta: i64, now: Nanos) -> Result<i64, OpError> {
        let existing = self.resolve(key, now);
        let (current, version, expire_at_ms) = match existing {
            Some((addr, len)) => {
                let view = RecordView::new(self.arena.bytes(addr, len));
                if view.type_tag() != TypeTag::String {
                    return Err(OpError::WrongType);
                }
                (parse_int(view.value())?, view.version().wrapping_add(1), view.expire_at_ms())
            }
            None => (0, 1, None),
        };
        let next = current.checked_add(delta).ok_or(OpError::Overflow)?;
        let mut buf = [0u8; 20];
        let value = fmt_i64(&mut buf, next);
        let spec = RecordSpec {
            key,
            value,
            version,
            expire_at_ms,
            kind: RecordKind::String { raw: false },
        };
        self.write_record(key, existing, spec)?;
        Ok(next)
    }

    /// `INCRBYFLOAT`: the formatted new value (what the record now holds).
    pub fn incr_by_float(
        &mut self,
        key: &[u8],
        delta: f64,
        now: Nanos,
    ) -> Result<Vec<u8>, OpError> {
        let existing = self.resolve(key, now);
        let (current, version, expire_at_ms) = match existing {
            Some((addr, len)) => {
                let view = RecordView::new(self.arena.bytes(addr, len));
                if view.type_tag() != TypeTag::String {
                    return Err(OpError::WrongType);
                }
                (parse_float(view.value())?, view.version().wrapping_add(1), view.expire_at_ms())
            }
            None => (0.0, 1, None),
        };
        let next = current + delta;
        if !next.is_finite() {
            return Err(OpError::NanOrInf);
        }
        // Shortest round-trip formatting; integers print without a decimal
        // point — the Redis `%.17Lg` + zero-strip shape for f64 range.
        // (Recorded deviation: Redis computes in 80-bit long double, so
        // extreme-precision tails can differ — compat-matrix entry.)
        let text = format!("{next}").into_bytes();
        let spec = RecordSpec {
            key,
            value: &text,
            version,
            expire_at_ms,
            kind: RecordKind::String { raw: false },
        };
        self.write_record(key, existing, spec)?;
        Ok(text)
    }

    /// `APPEND`: new value length. Appending to an EXISTING record marks it
    /// raw; a fresh key stays encodable (Redis `tryObjectEncoding` shape,
    /// oracle-pinned).
    pub fn append(&mut self, key: &[u8], tail: &[u8], now: Nanos) -> Result<u64, OpError> {
        let existing = self.resolve(key, now);
        let (mut value, version, expire_at_ms) = match existing {
            Some((addr, len)) => {
                let view = RecordView::new(self.arena.bytes(addr, len));
                if view.type_tag() != TypeTag::String {
                    return Err(OpError::WrongType);
                }
                (view.value().to_vec(), view.version().wrapping_add(1), view.expire_at_ms())
            }
            None => (Vec::new(), 1, None),
        };
        value.extend_from_slice(tail);
        check_bounds(key, &value)?;
        let new_len = value.len() as u64;
        let raw = existing.is_some();
        let spec = RecordSpec {
            key,
            value: &value,
            version,
            expire_at_ms,
            kind: RecordKind::String { raw },
        };
        self.write_record(key, existing, spec)?;
        Ok(new_len)
    }

    /// `SETRANGE`: patch `patch` at `offset`, zero-padding any gap; returns
    /// the new length. Empty patches never create or mutate (Redis).
    pub fn set_range(
        &mut self,
        key: &[u8],
        offset: usize,
        patch: &[u8],
        now: Nanos,
    ) -> Result<u64, OpError> {
        let existing = self.resolve(key, now);
        if patch.is_empty() {
            return Ok(match existing {
                Some((addr, len)) => RecordView::new(self.arena.bytes(addr, len)).vlen() as u64,
                None => 0,
            });
        }
        let end = offset.checked_add(patch.len()).ok_or(OpError::TooLarge)?;
        if key.len() > MAX_KEY_LEN || end > MAX_VAL_LEN {
            return Err(OpError::TooLarge);
        }
        let (mut value, version, expire_at_ms) = match existing {
            Some((addr, len)) => {
                let view = RecordView::new(self.arena.bytes(addr, len));
                if view.type_tag() != TypeTag::String {
                    return Err(OpError::WrongType);
                }
                (view.value().to_vec(), view.version().wrapping_add(1), view.expire_at_ms())
            }
            None => (Vec::new(), 1, None),
        };
        if value.len() < end {
            value.resize(end, 0);
        }
        value[offset..end].copy_from_slice(patch);
        let new_len = value.len() as u64;
        let spec = RecordSpec {
            key,
            value: &value,
            version,
            expire_at_ms,
            kind: RecordKind::String { raw: true },
        };
        self.write_record(key, existing, spec)?;
        Ok(new_len)
    }

    /// `RENAME` (single cell): value, TTL, and encoding move; the source is
    /// removed. `Ok(false)` = source missing. The destination write happens
    /// first so OOM leaves the source intact. Document handles **transfer**:
    /// the value bytes move verbatim and the source record frees without a
    /// payload release (ADR-0037 D3) — the one deliberate `free_record`
    /// bypass in the store.
    pub fn rename(&mut self, src: &[u8], dst: &[u8], now: Nanos) -> Result<bool, OpError> {
        if src == dst {
            return Ok(self.exists(src, now));
        }
        let Some((src_addr, src_len)) = self.resolve(src, now) else {
            return Ok(false);
        };
        let view = RecordView::new(self.arena.bytes(src_addr, src_len));
        let value = view.value().to_vec();
        #[cfg(feature = "doc")]
        let mut value = value;
        let deadline = view.expire_at_ms();
        let kind = view.kind();
        check_bounds(dst, &value)?;
        let dst_existing = self.resolve(dst, now);
        let dst_old = dst_existing.map(|(addr, len)| RecordView::new(self.arena.bytes(addr, len)));
        let version = dst_old.map_or(1, |v| v.version().wrapping_add(1));
        #[cfg(feature = "doc")]
        if matches!(kind, RecordKind::JsonDoc) {
            let lineage = dst_old
                .filter(|view| view.type_tag() == TypeTag::JsonDoc)
                .map_or_else(|| self.docs.allocate_lineage(), doc::lineage_of_record);
            doc::write_lineage(&mut value, lineage);
        }
        let dst_had_ttl = dst_old.and_then(|v| v.expire_at_ms()).is_some();
        let spec = RecordSpec { key: dst, value: &value, version, expire_at_ms: deadline, kind };
        // Releasing: the DESTINATION's old payload dies; the carried source
        // handle inside `value` is untouched by the release.
        self.write_record_releasing(dst, dst_existing, spec)?;
        self.note_ttl(dst_had_ttl, deadline.is_some());
        // Source removal: the dst write never moves the src record, and the
        // payload now belongs to dst — no release.
        let src_had_ttl = deadline.is_some();
        self.index.remove(Self::hash_key(src), src_addr);
        self.arena.free(src_addr, src_len);
        self.note_ttl(src_had_ttl, false);
        if let Some(ms) = deadline {
            self.arm_wheel(Self::hash_key(dst), ms);
        }
        Ok(true)
    }

    /// `COPY` (single cell): duplicates value, TTL, and encoding. Documents
    /// deep-copy through their canonical frozen bytes and re-tier at the
    /// destination (ADR-0037 D3) — handles are never duplicated.
    pub fn copy(
        &mut self,
        src: &[u8],
        dst: &[u8],
        replace: bool,
        now: Nanos,
    ) -> Result<CopyResult, OpError> {
        let Some((src_addr, src_len)) = self.resolve(src, now) else {
            return Ok(CopyResult::SourceMissing);
        };
        // COPY is excluded from the plane brackets (ADR-0076 D3): the
        // destination may live in another database, so the mini-bracket
        // runs here, where the owning store is unambiguous. The peek
        // mutates nothing — `src_addr` stays valid across it.
        #[cfg(feature = "doc")]
        {
            self.idx_bracket_begin(&[dst], None).map_err(OpError::IndexMaintenance)?;
            let result = self.copy_from_resolved(src_addr, src_len, dst, replace, now);
            match &result {
                Ok(CopyResult::Copied) => {
                    self.idx_bracket_commit(&[dst], crate::index_maint::MaintMode::Strict);
                }
                _ => self.idx_bracket_abort(),
            }
            result
        }
        #[cfg(not(feature = "doc"))]
        self.copy_from_resolved(src_addr, src_len, dst, replace, now)
    }

    /// The COPY body past source resolution (split out for the S04
    /// mini-bracket above).
    fn copy_from_resolved(
        &mut self,
        src_addr: ArenaAddr,
        src_len: usize,
        dst: &[u8],
        replace: bool,
        now: Nanos,
    ) -> Result<CopyResult, OpError> {
        let view = RecordView::new(self.arena.bytes(src_addr, src_len));
        #[cfg(feature = "doc")]
        if view.type_tag() == TypeTag::JsonDoc {
            let deadline = view.expire_at_ms();
            let plain = self.frozen_bytes_of(view)?;
            return self.copy_doc_to(dst, &plain, deadline, replace, now);
        }
        let value = view.value().to_vec();
        let deadline = view.expire_at_ms();
        let raw = view.is_raw();
        check_bounds(dst, &value)?;
        let dst_existing = self.resolve(dst, now);
        if dst_existing.is_some() && !replace {
            return Ok(CopyResult::DestinationExists);
        }
        let dst_old = dst_existing.map(|(addr, len)| RecordView::new(self.arena.bytes(addr, len)));
        let version = dst_old.map_or(1, |v| v.version().wrapping_add(1));
        let dst_had_ttl = dst_old.and_then(|v| v.expire_at_ms()).is_some();
        let spec = RecordSpec {
            key: dst,
            value: &value,
            version,
            expire_at_ms: deadline,
            kind: RecordKind::String { raw },
        };
        self.write_record(dst, dst_existing, spec)?;
        self.note_ttl(dst_had_ttl, deadline.is_some());
        if let Some(ms) = deadline {
            self.arm_wheel(Self::hash_key(dst), ms);
        }
        Ok(CopyResult::Copied)
    }

    /// Destination half of a document COPY: `plain` is the source's frozen
    /// canonical tape; placement re-tiers in THIS store's document arena.
    #[cfg(feature = "doc")]
    fn copy_doc_to(
        &mut self,
        dst: &[u8],
        plain: &[u8],
        deadline: Option<u64>,
        replace: bool,
        now: Nanos,
    ) -> Result<CopyResult, OpError> {
        if dst.len() > MAX_KEY_LEN {
            return Err(OpError::TooLarge);
        }
        let dst_existing = self.resolve(dst, now);
        if dst_existing.is_some() && !replace {
            return Ok(CopyResult::DestinationExists);
        }
        let dst_old = dst_existing.map(|(addr, len)| RecordView::new(self.arena.bytes(addr, len)));
        let version = dst_old.map_or(1, |v| v.version().wrapping_add(1));
        let lineage = dst_old
            .filter(|view| view.type_tag() == TypeTag::JsonDoc)
            .map_or_else(|| self.docs.allocate_lineage(), doc::lineage_of_record);
        let dst_had_ttl = dst_old.and_then(|v| v.expire_at_ms()).is_some();
        self.json_write_value(
            dst,
            dst_existing,
            plain,
            doc::DocWriteMeta {
                lineage,
                version,
                expire_at_ms: deadline,
                cadence: doc::DocCadence::default(),
            },
        )?;
        self.note_ttl(dst_had_ttl, deadline.is_some());
        if let Some(ms) = deadline {
            self.arm_wheel(Self::hash_key(dst), ms);
        }
        Ok(CopyResult::Copied)
    }

    /// Cross-db `COPY` export (M1-S08): value, TTL, and encoding state.
    /// Documents export their canonical frozen bytes (never a handle —
    /// handles are meaningless in another store's arena, ADR-0037 D3).
    pub(crate) fn copy_out(&mut self, key: &[u8], now: Nanos) -> Option<ExportedRecord> {
        let (addr, len) = self.resolve(key, now)?;
        let view = RecordView::new(self.arena.bytes(addr, len));
        #[cfg(feature = "doc")]
        if view.type_tag() == TypeTag::JsonDoc {
            return Some(ExportedRecord {
                value: self.frozen_bytes_of(view).ok()?,
                expire_at_ms: view.expire_at_ms(),
                kind: RecordKind::JsonDoc,
            });
        }
        Some(ExportedRecord {
            value: view.value().to_vec(),
            expire_at_ms: view.expire_at_ms(),
            kind: RecordKind::String { raw: view.is_raw() },
        })
    }

    /// Cross-db `COPY` import — the destination half of [`copy`](Self::copy)
    /// with the source already materialized from another db.
    pub(crate) fn copy_in(
        &mut self,
        dst: &[u8],
        rec: &ExportedRecord,
        replace: bool,
        now: Nanos,
    ) -> Result<CopyResult, OpError> {
        check_bounds(dst, &rec.value)?;
        #[cfg(feature = "doc")]
        if rec.kind == RecordKind::JsonDoc {
            // Exported documents carry canonical tape bytes; re-tier here.
            return self.copy_doc_to(dst, &rec.value, rec.expire_at_ms, replace, now);
        }
        let dst_existing = self.resolve(dst, now);
        if dst_existing.is_some() && !replace {
            return Ok(CopyResult::DestinationExists);
        }
        let dst_old = dst_existing.map(|(addr, len)| RecordView::new(self.arena.bytes(addr, len)));
        let version = dst_old.map_or(1, |v| v.version().wrapping_add(1));
        let dst_had_ttl = dst_old.and_then(|v| v.expire_at_ms()).is_some();
        let spec = RecordSpec {
            key: dst,
            value: &rec.value,
            version,
            expire_at_ms: rec.expire_at_ms,
            kind: rec.kind,
        };
        self.write_record(dst, dst_existing, spec)?;
        self.note_ttl(dst_had_ttl, rec.expire_at_ms.is_some());
        if let Some(ms) = rec.expire_at_ms {
            self.arm_wheel(Self::hash_key(dst), ms);
        }
        Ok(CopyResult::Copied)
    }

    /// `EXPIRE`/`PEXPIRE`/`PERSIST` (`at: None` removes the TTL). True if
    /// the deadline was applied/removed.
    pub fn expire(&mut self, key: &[u8], at: Option<Nanos>, cond: ExpireCond, now: Nanos) -> bool {
        let Some((addr, len)) = self.resolve(key, now) else { return false };
        let view = RecordView::new(self.arena.bytes(addr, len));
        let current = view.expire_at_ms();
        let new_ms = at.map(|n| (n.0 / 1_000_000).min(MAX_EXPIRE_MS));
        let applies = match cond {
            ExpireCond::Always => true,
            ExpireCond::IfNoExpiry => current.is_none(),
            ExpireCond::IfHasExpiry => current.is_some(),
            // GT/LT: a missing current TTL counts as infinite (Redis rules):
            // GT never beats infinity; LT always does.
            ExpireCond::IfGreater => match (new_ms, current) {
                (Some(new), Some(cur)) => new > cur,
                (Some(_), None) => false,
                (None, _) => false, // PERSIST with GT/LT is a command error upstream
            },
            ExpireCond::IfLess => match (new_ms, current) {
                (Some(new), Some(cur)) => new < cur,
                (Some(_), None) => true,
                (None, _) => false,
            },
        };
        if !applies || (at.is_none() && current.is_none()) {
            return false;
        }
        // EXPIRE with a deadline at/before `now` deletes the key (Redis
        // semantics) and still reports success.
        if let Some(ms) = new_ms
            && ms <= now.0 / 1_000_000
        {
            self.free_record(Self::hash_key(key), addr, len);
            self.note_ttl(current.is_some(), false);
            return true;
        }
        // Rewrite with the new TTL-extension state. The ±5-byte extension
        // may cross a size class, so the record borrow must end before the
        // write: copy out (TTL changes are rare; a same-class in-place
        // specialization stays reserved).
        let key_owned = view.key().to_vec();
        let value_owned = view.value().to_vec();
        let version = view.version().wrapping_add(1);
        let kind = view.kind();
        let spec = RecordSpec {
            key: &key_owned,
            value: &value_owned,
            version,
            expire_at_ms: new_ms,
            kind,
        };
        // Carrying, not releasing: the value bytes (a document handle
        // included) move verbatim into the rewritten record (ADR-0037 D3).
        if self.write_record_carrying(key, Some((addr, len)), spec).is_err() {
            return false;
        }
        self.note_ttl(current.is_some(), new_ms.is_some());
        if let Some(ms) = new_ms
            && current != new_ms
        {
            self.arm_wheel(Self::hash_key(key), ms);
        }
        true
    }

    // ---- keyspace iteration (M1-S02) ----

    /// `SCAN` over one cell: home-group enumeration in reverse-binary cursor
    /// order. Guarantee: every key present for the whole scan is emitted at
    /// least once, across doubling growth and tombstone-recycling rehashes
    /// (same-capacity rebuilds keep home groups fixed; doublings split a
    /// home group `g` into `{g, g + groups}` — exactly the split the
    /// reverse-binary order tolerates). Keys written or removed mid-scan may
    /// or may not appear (Redis contract). Expired records encountered are
    /// reaped, never emitted. Returns the next cursor (0 = done).
    pub fn scan(
        &mut self,
        cursor: u64,
        count: usize,
        now: Nanos,
        mut emit: impl FnMut(&[u8]),
    ) -> u64 {
        let mask = self.index.group_count() as u64 - 1;
        let mut cursor = cursor & mask;
        let mut emitted = 0usize;
        let mut batch: Vec<ArenaAddr> = Vec::new();
        loop {
            batch.clear();
            {
                let arena = &self.arena;
                self.index.scan_home_group(
                    cursor as usize,
                    |addr| Self::hash_key(record_at(arena, addr).key()),
                    |addr| batch.push(addr),
                );
            }
            for &addr in &batch {
                let view = record_at(&self.arena, addr);
                if view.is_expired(now) {
                    let (hash, len) = (Self::hash_key(view.key()), view.encoded_len());
                    self.free_record(hash, addr, len);
                    self.note_reap_lazy();
                } else {
                    emit(view.key());
                    emitted += 1;
                }
            }
            cursor = next_rev_cursor(cursor, mask);
            if cursor == 0 || emitted >= count {
                return cursor;
            }
        }
    }

    /// The fuzzy-checkpoint walk (M2-S10, ADR-0016 D2): the same
    /// resize-stable home-group enumeration as [`scan`](Self::scan), but
    /// emitting each live entry's post-image `(key, value, expire_at_ms)`
    /// instead of the key — expiry deadline in *internal* ms (the caller
    /// converts through its `WallAnchor` when encoding records). Inherits
    /// the SCAN guarantee: every entry present for the whole walk is
    /// emitted at least once across doublings and tombstone rehashes;
    /// entries written mid-walk may appear zero or more times (harmless —
    /// checkpoint replay is a blind idempotent upsert and the log tail
    /// from `ckpt-begin` re-covers them). Expired records encountered are
    /// reaped, never emitted. No access-tracking side effects on emitted
    /// entries. Returns the next cursor (0 = done).
    pub fn scan_post_images(
        &mut self,
        cursor: u64,
        count: usize,
        now: Nanos,
        mut emit: impl FnMut(&[u8], &[u8], Option<u64>),
    ) -> u64 {
        self.scan_checkpoint_images(cursor, count, now, |key, image, expire_at_ms| match image {
            CheckpointImage::String(value) => emit(key, value, expire_at_ms),
            #[cfg(feature = "doc")]
            CheckpointImage::JsonDoc { .. } => {
                panic!("string-only post-image walker encountered a document")
            }
        })
    }

    /// Type-aware fuzzy-checkpoint walk. It has the same resize/expiry
    /// guarantees as [`scan_post_images`](Self::scan_post_images), but
    /// freezes documents into canonical idoc bytes at the store boundary.
    pub fn scan_checkpoint_images(
        &mut self,
        cursor: u64,
        count: usize,
        now: Nanos,
        mut emit: impl FnMut(&[u8], CheckpointImage<'_>, Option<u64>),
    ) -> u64 {
        let mask = self.index.group_count() as u64 - 1;
        let mut cursor = cursor & mask;
        let mut emitted = 0usize;
        let mut batch: Vec<ArenaAddr> = Vec::new();
        loop {
            batch.clear();
            {
                let arena = &self.arena;
                self.index.scan_home_group(
                    cursor as usize,
                    |addr| Self::hash_key(record_at(arena, addr).key()),
                    |addr| batch.push(addr),
                );
            }
            for &addr in &batch {
                let view = record_at(&self.arena, addr);
                if view.is_expired(now) {
                    let (hash, len) = (Self::hash_key(view.key()), view.encoded_len());
                    self.free_record(hash, addr, len);
                    self.note_reap_lazy();
                    continue;
                }
                match view.type_tag() {
                    TypeTag::String => {
                        emit(
                            view.key(),
                            CheckpointImage::String(view.value()),
                            view.expire_at_ms(),
                        );
                    }
                    TypeTag::StringExtent => {
                        unreachable!("StringExtent records exist only in tiered namespaces")
                    }
                    TypeTag::JsonDoc => {
                        #[cfg(feature = "doc")]
                        {
                            let idoc = doc::checkpoint_idoc(&mut self.docs, view)
                                .expect("store-owned document freezes within its format bound");
                            emit(
                                view.key(),
                                CheckpointImage::JsonDoc {
                                    lineage: doc::lineage_of_record(view),
                                    version: view.version(),
                                    idoc,
                                },
                                view.expire_at_ms(),
                            );
                        }
                        #[cfg(not(feature = "doc"))]
                        unreachable!("JsonDoc records cannot exist without the doc feature");
                    }
                }
                emitted += 1;
            }
            cursor = next_rev_cursor(cursor, mask);
            if cursor == 0 || emitted >= count {
                return cursor;
            }
        }
    }

    /// M2-S13: presize the index for `keys` live entries — recovery's
    /// hint from the `.ick` footer's per-ns counts, applied before the
    /// bulk replay so it avoids the doubling-rehash storm (each doubling
    /// is a stop-and-copy over the whole table). Only effective while the
    /// store is empty; a populated index keeps its geometry (growth on
    /// insert remains correct either way). The hint is clamped defensively
    /// — it may come from a damaged file, and a wrong hint may only cost
    /// memory geometry, never correctness.
    pub fn reserve_keys(&mut self, keys: usize) {
        const MAX_RESERVE: usize = 1 << 28;
        if self.is_empty() && keys > 64 {
            self.index = Index::with_capacity(keys.min(MAX_RESERVE));
        }
    }

    /// M2-S13 (ADR-0018): read-only sibling of
    /// [`scan_post_images`](Self::scan_post_images) for the recovery state
    /// digest — emits each live entry's `(key, value, expire_at_ms)`
    /// **without reaping** expired entries, so the walk performs no
    /// structural mutation and a full cursor sweep emits every live entry
    /// exactly once (the digest oracle needs exactly-once; the mutating
    /// walk guarantees only at-least-once across rehashes). Expired
    /// entries are skipped: they are logically dead at `now` whatever
    /// their physical residue.
    pub fn digest_post_images(
        &self,
        cursor: u64,
        count: usize,
        now: Nanos,
        mut emit: impl FnMut(&[u8], &[u8], Option<u64>),
    ) -> u64 {
        self.digest_checkpoint_images(cursor, count, now, |key, image, expire_at_ms| match image {
            CheckpointImage::String(value) => emit(key, value, expire_at_ms),
            #[cfg(feature = "doc")]
            CheckpointImage::JsonDoc { .. } => {
                panic!("string-only digest walker encountered a document")
            }
        })
    }

    /// Type-aware, read-only state-digest walk. Documents contribute
    /// canonical idoc bytes and their exact logical version; physical form
    /// and volatile cadence state are intentionally absent.
    pub fn digest_checkpoint_images(
        &self,
        cursor: u64,
        count: usize,
        now: Nanos,
        mut emit: impl FnMut(&[u8], CheckpointImage<'_>, Option<u64>),
    ) -> u64 {
        let mask = self.index.group_count() as u64 - 1;
        let mut cursor = cursor & mask;
        let mut emitted = 0usize;
        loop {
            let arena = &self.arena;
            self.index.scan_home_group(
                cursor as usize,
                |addr| Self::hash_key(record_at(arena, addr).key()),
                |addr| {
                    let view = record_at(arena, addr);
                    if view.is_expired(now) {
                        return;
                    }
                    match view.type_tag() {
                        TypeTag::String => emit(
                            view.key(),
                            CheckpointImage::String(view.value()),
                            view.expire_at_ms(),
                        ),
                        TypeTag::StringExtent => {
                            unreachable!("StringExtent records exist only in tiered namespaces")
                        }
                        TypeTag::JsonDoc => {
                            #[cfg(feature = "doc")]
                            {
                                let idoc = self
                                    .frozen_bytes_of(view)
                                    .expect("store-owned document freezes within its format bound");
                                emit(
                                    view.key(),
                                    CheckpointImage::JsonDoc {
                                        lineage: doc::lineage_of_record(view),
                                        version: view.version(),
                                        idoc: &idoc,
                                    },
                                    view.expire_at_ms(),
                                );
                            }
                            #[cfg(not(feature = "doc"))]
                            unreachable!("JsonDoc records cannot exist without the doc feature");
                        }
                    }
                    emitted += 1;
                },
            );
            cursor = next_rev_cursor(cursor, mask);
            if cursor == 0 || emitted >= count {
                return cursor;
            }
        }
    }

    /// `RANDOMKEY` probe: first live key at/after a caller-rolled slot
    /// (randomness is injected — L7). Two-level random (cell, then slot) is
    /// the documented compat deviation.
    pub fn random_key(&mut self, roll: u64, now: Nanos) -> Option<Vec<u8>> {
        loop {
            let addr = self.index.live_from(roll as usize)?;
            let view = record_at(&self.arena, addr);
            if !view.is_expired(now) {
                return Some(view.key().to_vec());
            }
            let (hash, len) = (Self::hash_key(view.key()), view.encoded_len());
            self.free_record(hash, addr, len);
            self.note_reap_lazy();
        }
    }

    /// `FLUSHDB`/`FLUSHALL` (this cell's slice): drop every record, reset
    /// the wheel, keep lifetime counters (Redis flush does not reset stats).
    pub fn flush(&mut self, now: Nanos) {
        self.arena = Arena::new(self.cfg.arena);
        self.index = Index::with_capacity(self.cfg.initial_keys.max(64));
        self.wheel = TtlWheel::new(now.0 / 1_000_000);
        self.docs.reset(&self.cfg);
        // FLUSH* is a removal class (ADR-0072 D6): a bulk replace runs
        // the whole-namespace index truncate, never N removals.
        // Declarations survive; an empty namespace projects empty trees.
        self.idx.truncate_all();
        self.stats.ttl_live = 0;
        self.evict.hand = 0;
    }

    // ---- active expiry (M1-E2) ----

    /// One budgeted expiry MAINTAIN slice (M1-S05): advance the wheel toward
    /// `now`, validating each fired entry against the index and reaping only
    /// records genuinely expired. Stale entries (TTL changed/persisted/key
    /// gone) drop with a counter. Bounded by `budget` on both fires and
    /// cursor steps so a 1M-same-second storm cannot cliff the loop.
    pub fn expire_tick(&mut self, now: Nanos, budget: ExpiryBudget) -> ExpiryStats {
        let now_ms = now.0 / 1_000_000;
        #[cfg(feature = "doc")]
        let max_matches = self.cfg.doc_max_path_matches;
        let CellStore { arena, index, wheel, stats, docs, idx, .. } = self;
        #[cfg(not(feature = "doc"))]
        let _ = &idx;
        let mut out = ExpiryStats::default();
        let tick = wheel.tick(now_ms, budget, |hash, _deadline| {
            // Reap any record on this hash's probe path that is genuinely
            // expired (full-hash check keeps fingerprint collisions out;
            // reaping an expired record is correct regardless of which key
            // armed the entry).
            let found = index.find(hash, |addr| {
                let view = record_at(arena, addr);
                view.is_expired(now) && hash64(view.key(), HASH_SEED) == hash
            });
            match found {
                Some(addr) => {
                    let len = record_at(arena, addr).encoded_len();
                    // The record-death hook (ADR-0072 D6): active expiry
                    // is a removal class like any other; the split-borrow
                    // reap deliberately bypasses `free_record`, so the
                    // hook is wired here too (the D6 structural
                    // exception). MAINTAIN slices never run inside a
                    // command, so no bracket can cover this death.
                    #[cfg(feature = "doc")]
                    if idx.death_hook_wanted(hash)
                        && let Some(root) = doc::doc_root_at(arena, docs, addr, len)
                    {
                        idx.remove_doc_entries(hash, root, max_matches);
                    }
                    let payload = doc::payload_of(arena, addr, len);
                    index.remove(hash, addr);
                    arena.free(addr, len);
                    docs.release(payload);
                    stats.expired_active += 1;
                    stats.ttl_live = stats.ttl_live.saturating_sub(1);
                    out.reaped += 1;
                }
                None => {
                    stats.wheel_stale += 1;
                    out.stale += 1;
                }
            }
        });
        out.steps = tick.steps;
        out.lag_ms = if tick.caught_up { 0 } else { now_ms.saturating_sub(wheel_cursor(wheel)) };
        out.armed = wheel.live();
        out
    }

    // ---- eviction mechanism (M1-S06; policy logic lives in `evict.rs`) ----

    /// Applies an eviction policy: flips the access-tracking mode and
    /// allocates/frees the CMS (8 KiB only while LFU is selected).
    pub fn set_eviction_policy(&mut self, policy: EvictionPolicy) {
        self.evict.set_policy(policy);
    }

    #[inline]
    pub fn eviction_policy(&self) -> EvictionPolicy {
        self.evict.policy
    }

    /// Logical bytes this store costs (live records + index + wheel + CMS)
    /// — what `maxmemory` pressure compares against (M1-S07), the Redis
    /// `used_memory` shape. Live (not resident) bytes are the comparable:
    /// slab chunks stay mapped and recycle, so resident is monotone while
    /// eviction must be able to bring pressure *down*. The RSS story is the
    /// slack bound: resident ≤ live-at-peak + class slack, asserted by the
    /// M1-S07 pressure test and gated on the reference box.
    pub fn used_bytes(&self) -> u64 {
        let r = self.report();
        r.records_live_bytes
            + r.index_bytes
            + r.wheel_bytes
            + r.evict_bytes
            + r.doc_tape_bytes
            + r.doc_arena_bytes
    }

    /// Evicts at most one victim under the active policy (bounded candidate
    /// window, `samples` per selection). The pressure driver loops this.
    pub fn evict_step(&mut self, samples: u32, now: Nanos) -> EvictStats {
        evict::evict_one(self, samples, now)
    }

    /// Periodic eviction maintenance: CMS Morris-counter decay on the
    /// injected clock (MAINTAIN slice).
    pub fn evict_maintain(&mut self, now: Nanos) {
        evict::maybe_decay(&mut self.evict, now);
    }

    /// `OBJECT FREQ` under an LFU policy: the CMS estimate (Morris-scaled —
    /// recorded deviation: Redis reports its own log-counter scale).
    pub fn object_freq(&mut self, key: &[u8], now: Nanos) -> Option<u8> {
        self.resolve(key, now)?;
        let hash = Self::hash_key(key);
        Some(self.evict.cms.as_ref().map_or(0, |cms| cms.estimate(hash)))
    }

    /// CLOCK aging: drop one reference generation (eviction sweep).
    pub(crate) fn age_record(&mut self, addr: ArenaAddr) {
        let head = self.arena.bytes_mut(addr, 1);
        head[0] = flags_ref_decrement(head[0]);
    }

    /// Reaps a record the eviction sweep found already expired.
    pub(crate) fn reap_expired_at(&mut self, hash: u64, addr: ArenaAddr, len: usize) {
        self.free_record(hash, addr, len);
        self.note_reap_lazy();
    }

    /// Removes an eviction victim (counted separately from expirations).
    pub(crate) fn evict_record(&mut self, hash: u64, addr: ArenaAddr, len: usize, had_ttl: bool) {
        self.free_record(hash, addr, len);
        self.note_ttl(had_ttl, false);
        self.stats.evicted_keys += 1;
    }

    // ---- internals ----

    /// Free one record completely: index entry, record bytes, and any
    /// document payload behind it (the ADR-0037 D3 choke point). Every
    /// reap/delete/evict site funnels here; the only deliberate bypass is
    /// RENAME's source removal (the handle transferred to the destination).
    ///
    /// Record-death hook (M4.5-S04, ADR-0072 D6): a dying document's
    /// index entries are removed here — the last moment its values are
    /// readable — unless this death is bracket-covered (a write-set key
    /// dying inside its own command; the bracket's diff owns it,
    /// ADR-0076 D4). Zero-index stores pay one cached branch.
    pub(crate) fn free_record(&mut self, hash: u64, addr: ArenaAddr, len: usize) {
        #[cfg(feature = "doc")]
        if self.idx.death_hook_wanted(hash) {
            let max_matches = self.cfg.doc_max_path_matches;
            let CellStore { arena, docs, idx, .. } = self;
            if let Some(root) = doc::doc_root_at(arena, docs, addr, len) {
                idx.remove_doc_entries(hash, root, max_matches);
            }
        }
        let payload = doc::payload_of(&self.arena, addr, len);
        self.index.remove(hash, addr);
        self.arena.free(addr, len);
        self.docs.release(payload);
    }

    pub(crate) fn arm_wheel(&mut self, hash: u64, deadline_ms: u64) {
        if self.wheel.arm(hash, deadline_ms) == ArmOutcome::PoolFull {
            self.stats.wheel_fallback += 1;
        }
    }

    /// TTL-record census transition (`INFO keyspace` `expires=`).
    #[inline]
    pub(crate) fn note_ttl(&mut self, old: bool, new: bool) {
        match (old, new) {
            (false, true) => self.stats.ttl_live += 1,
            (true, false) => self.stats.ttl_live = self.stats.ttl_live.saturating_sub(1),
            _ => {}
        }
    }

    #[inline]
    fn note_reap_lazy(&mut self) {
        self.stats.expired_lazy += 1;
        self.stats.ttl_live = self.stats.ttl_live.saturating_sub(1);
    }

    /// Index lookup + expire-on-read: returns the live record's address and
    /// encoded length, reaping it if its deadline passed.
    pub(crate) fn resolve(&mut self, key: &[u8], now: Nanos) -> Option<(ArenaAddr, usize)> {
        self.resolve_hashed(key, Self::hash_key(key), now)
    }

    fn resolve_hashed(&mut self, key: &[u8], hash: u64, now: Nanos) -> Option<(ArenaAddr, usize)> {
        let arena = &self.arena;
        let addr = self.index.find(hash, |addr| record_at(arena, addr).key() == key)?;
        let view = record_at(arena, addr);
        let len = view.encoded_len();
        if view.is_expired(now) {
            self.free_record(hash, addr, len);
            self.note_reap_lazy();
            return None;
        }
        self.touch_access(hash, addr);
        Some((addr, len))
    }

    /// Eviction access tracking (M1-S06): one cached branch when no LRU/LFU
    /// policy is active (the M1-S07 hot-path rule). CLOCK saturates the
    /// in-record reference bits (one OR on a line the access already
    /// pulled); LFU Morris-bumps the CMS with one injected-stream roll.
    #[inline]
    fn touch_access(&mut self, hash: u64, addr: ArenaAddr) {
        match self.evict.tracking {
            Tracking::None => {}
            Tracking::Clock => {
                let head = self.arena.bytes_mut(addr, 1);
                head[0] = flags_ref_saturate(head[0]);
            }
            Tracking::Lfu => {
                let roll = self.evict.next_roll();
                if let Some(cms) = self.evict.cms.as_mut() {
                    cms.touch(hash, roll);
                }
            }
        }
    }

    fn write_record(
        &mut self,
        key: &[u8],
        existing: Option<(ArenaAddr, usize)>,
        spec: RecordSpec<'_>,
    ) -> Result<(), OpError> {
        self.write_record_releasing(key, existing, spec)
    }

    /// [`write_record_carrying`](Self::write_record_carrying) plus the
    /// ADR-0037 D3 overwrite rule: any document payload behind `existing`
    /// is captured first and released only after the write succeeds — a
    /// failed write leaves the old record and its payload untouched.
    pub(crate) fn write_record_releasing(
        &mut self,
        key: &[u8],
        existing: Option<(ArenaAddr, usize)>,
        spec: RecordSpec<'_>,
    ) -> Result<(), OpError> {
        let old_payload = existing.map(|(addr, len)| doc::payload_of(&self.arena, addr, len));
        self.write_record_carrying(key, existing, spec)?;
        if let Some(payload) = old_payload {
            self.docs.release(payload);
        }
        Ok(())
    }

    /// Writes `spec`, reusing `existing`'s slot when the size class allows,
    /// else alloc-copy-free with an index address swap. **Carries** any
    /// document payload referenced by both old and new value bytes: no
    /// release happens here — TTL rewrites move handle bytes verbatim
    /// (ADR-0037 D3's RENAME/EXPIRE transfer rule; the in-place blob
    /// overwrite in `doc::json_write_value` relies on the same contract).
    pub(crate) fn write_record_carrying(
        &mut self,
        key: &[u8],
        existing: Option<(ArenaAddr, usize)>,
        spec: RecordSpec<'_>,
    ) -> Result<(), OpError> {
        let new_len = spec.encoded_len();
        let hash = Self::hash_key(key);
        // Writes count as accesses (Redis updates LRU/LFU on write), at
        // write strength: one CLOCK generation / one CMS baseline bump —
        // repeated reads are what saturate recency, so churn cannot
        // impersonate a hot set.
        match existing {
            Some((addr, old_len)) if self.arena.resize_in_place(addr, old_len, new_len) => {
                spec.write(self.arena.bytes_mut(addr, new_len));
                self.touch_write(hash, addr);
                Ok(())
            }
            Some((addr, old_len)) => {
                let new_addr = self.arena.alloc(new_len).ok_or(OpError::OutOfMemory)?;
                spec.write(self.arena.bytes_mut(new_addr, new_len));
                self.index.replace(hash, addr, new_addr);
                self.arena.free(addr, old_len);
                self.touch_write(hash, new_addr);
                Ok(())
            }
            None => {
                if self.index.needs_grow() {
                    let arena = &self.arena;
                    self.index.grow(|addr, _| Self::hash_key(record_at(arena, addr).key()));
                }
                let new_addr = self.arena.alloc(new_len).ok_or(OpError::OutOfMemory)?;
                spec.write(self.arena.bytes_mut(new_addr, new_len));
                self.index.insert(hash, new_addr);
                self.touch_write(hash, new_addr);
                Ok(())
            }
        }
    }

    /// Write-strength access mark (see `write_record_at`).
    #[inline]
    fn touch_write(&mut self, hash: u64, addr: ArenaAddr) {
        match self.evict.tracking {
            Tracking::None => {}
            Tracking::Clock => {
                let head = self.arena.bytes_mut(addr, 1);
                head[0] = flags_ref_write(head[0]);
            }
            Tracking::Lfu => {
                let roll = self.evict.next_roll();
                if let Some(cms) = self.evict.cms.as_mut() {
                    cms.touch(hash, roll);
                }
            }
        }
    }
}

/// The wheel cursor in ms (private peek for the lag metric).
#[inline]
fn wheel_cursor(wheel: &TtlWheel) -> u64 {
    wheel.cursor_ms()
}

/// Reverse-binary cursor increment (the Redis `dictScan` order) over a
/// power-of-two group space: high bits advance first, so groups split by a
/// doubling are visited adjacently and never missed.
#[inline]
pub(crate) fn next_rev_cursor(cursor: u64, mask: u64) -> u64 {
    let mut v = cursor | !mask;
    v = v.reverse_bits();
    v = v.wrapping_add(1);
    v.reverse_bits()
}

/// Reads the record at `addr`: header first (fixed 8 bytes) to learn the
/// full encoded length, then the complete slice.
#[inline]
pub(crate) fn record_at(arena: &Arena, addr: ArenaAddr) -> RecordView<'_> {
    let head = arena.bytes(addr, HEADER_LEN);
    let full_len = crate::record::encoded_len_from_header(head);
    RecordView::new(arena.bytes(addr, full_len))
}

#[inline]
fn check_bounds(key: &[u8], value: &[u8]) -> Result<(), OpError> {
    if key.len() > MAX_KEY_LEN || value.len() > MAX_VAL_LEN {
        return Err(OpError::TooLarge);
    }
    Ok(())
}

/// Strict Redis `string2ll` semantics: optional sign, no leading zeros, no
/// `-0` (oracle-pinned vs Redis 8.0.5 by the compat harness), i64 range
/// (overflow-on-parse is `NotInt`, matching "not an integer or out of
/// range").
fn parse_int(bytes: &[u8]) -> Result<i64, OpError> {
    if bytes.is_empty() || bytes.len() > 21 {
        return Err(OpError::NotInt);
    }
    let (neg, digits) = match bytes[0] {
        b'-' => (true, &bytes[1..]),
        _ => (false, bytes),
    };
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return Err(OpError::NotInt);
    }
    if digits[0] == b'0' && (digits.len() > 1 || neg) {
        return Err(OpError::NotInt);
    }
    let mut acc: i64 = 0;
    for &d in digits {
        acc = acc
            .checked_mul(10)
            .and_then(|a| {
                let v = i64::from(d - b'0');
                if neg { a.checked_sub(v) } else { a.checked_add(v) }
            })
            .ok_or(OpError::NotInt)?;
    }
    Ok(acc)
}

/// Redis `strtold`-shape float parse: full-string consume, no surrounding
/// whitespace, NaN rejected (Infinity parses; the *result* check rejects
/// non-finite outcomes, matching Redis error split).
fn parse_float(bytes: &[u8]) -> Result<f64, OpError> {
    let s = core::str::from_utf8(bytes).map_err(|_| OpError::NotFloat)?;
    if s.is_empty() || s.starts_with(char::is_whitespace) || s.ends_with(char::is_whitespace) {
        return Err(OpError::NotFloat);
    }
    let v: f64 = s.parse().map_err(|_| OpError::NotFloat)?;
    if v.is_nan() {
        return Err(OpError::NotFloat);
    }
    Ok(v)
}

/// Formats an i64 into a stack buffer (no allocation on the INCR path).
fn fmt_i64(buf: &mut [u8; 20], v: i64) -> &[u8] {
    let mut at = buf.len();
    let neg = v < 0;
    // Work in negative space: i64::MIN has no positive counterpart.
    let mut n = if neg { v } else { -v };
    loop {
        at -= 1;
        buf[at] = b'0' + (-(n % 10)) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    if neg {
        at -= 1;
        buf[at] = b'-';
    }
    &buf[at..]
}
