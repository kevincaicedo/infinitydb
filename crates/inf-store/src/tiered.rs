//! Tiered record table (M4-S02): the §7.3 index over the M4-S01 address
//! space. The index does not change shape — the frozen 8 B slot's 48-bit
//! field is reinterpreted as a [`LogicalAddr`] at table granularity
//! (`Index<TieredMode>`, monomorphized — memory-mode tables are untouched
//! by construction), and record fetch routes through the resolver.
//!
//! This module is **mechanism**; suspension policy stays with the caller
//! (L6): a lookup that lands on a cold candidate returns
//! [`TieredLookup::Cold`] — the command layer (S04's steel thread, S08's
//! hardened path) fetches the bytes through the tier store, verifies the
//! key, and on the ≈2⁻²² fingerprint false positive retries with the
//! address excluded. Nothing here reads a disk byte or holds anything
//! across a suspension: every entry point takes and returns plain
//! addresses, so the resumed command re-resolves by contract (the M0
//! custody rule).
//!
//! Mutation surface (S02 + S05/S06): insert / [`update`] (the routed
//! entry — in-place above the ro-boundary, copy-to-tail below) /
//! overwrite (copy-to-tail mechanism) / delete. TTL, eviction pressure,
//! and WAL wiring arrive with the stories that own them (S07, S11).

pub mod compact;
pub mod promote;
pub mod shadow;

use std::collections::{HashMap, VecDeque};

use inf_foundation::{BuildIntHasher, KeyHasher, LogicalAddr};

use inf_log::flush::{TierFileMeta, TierFlush, TierFlushError};
use inf_log::fs::SegmentFs;
use inf_log::{LiveSetFileEntry, MutationEffect, SealedExtent, StagedAt, StagingFull, StagingRing};

use crate::address_space::{AddrClass, AddressSpace, AddressSpaceConfig, FlushChunk};
use crate::demote::DemotionConfig;
use crate::index::{Index, TieredMode};
use crate::live_set::LiveSet;
use crate::record::{ExtentRef, RecordKind, RecordSpec, RecordView};
use crate::store::{DiskFullCause, OpError};
use crate::write_accounting::WriteAccounting;

mod extent;
mod flush;

// ---- shared data definitions (behaviour lives in the child modules) ----------

/// Answer of a tiered lookup. `Cold` is a *candidate*: the 22-bit
/// fingerprint matched but the key is on disk — the caller fetches,
/// verifies with [`TieredTable::decode_record`], and on mismatch retries
/// via `lookup` with the address added to `exclude`.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum TieredLookup {
    /// RAM-resident, key verified.
    Ram(LogicalAddr),
    /// Cold candidate (addr < head) — fetch + verify, never trust.
    Cold(LogicalAddr),
    /// No live entry for this key.
    Miss,
}

/// Decoded view of one record's parts — the RAM read result and the
/// cold-fetch deserialization result share this shape.
#[derive(Copy, Clone, Debug)]
pub struct RecordParts<'a> {
    pub key: &'a [u8],
    /// Inline value bytes — for a [`TypeTag::StringExtent`]
    /// (crate::TypeTag::StringExtent) record these are the 24-byte
    /// reference ([`extent_ref`](Self::extent_ref) decodes it), never
    /// the value.
    pub value: &'a [u8],
    pub version: u32,
    pub encoded_len: usize,
    /// The record's type — walk drivers and cold-read consumers branch
    /// on it (an extent record's payload is elsewhere; M4-S17).
    pub type_tag: crate::record::TypeTag,
}

impl<'a> RecordParts<'a> {
    fn of(view: RecordView<'a>) -> RecordParts<'a> {
        RecordParts {
            key: view.key(),
            value: view.value(),
            version: view.version(),
            encoded_len: view.encoded_len(),
            type_tag: view.type_tag(),
        }
    }

    /// The out-of-line reference, when this record stores its value in
    /// a blob extent (ADR-0061 D2).
    #[must_use]
    pub fn extent_ref(&self) -> Option<ExtentRef> {
        match self.type_tag {
            crate::record::TypeTag::StringExtent => Some(ExtentRef::decode(self.value)),
            _ => None,
        }
    }
}

/// Sentinel for "no seal mark recorded yet" (M4-S07).
const NO_MARK_PAGE: u64 = u64::MAX;

/// One durable-tiered namespace's record table on one cell (L1).
pub struct TieredTable {
    /// The key hash's secret (ADR-0094) — the node's one value, injected.
    hasher: KeyHasher,
    index: Index<TieredMode>,
    space: AddressSpace,
    /// The id the latest checkpoint walk began under (F-L03-04 witness).
    walk_ckpt_id: Option<u64>,
    live_bytes: u64,
    demote: DemotionConfig,
    /// Record-start addresses the ro-boundary may seal to (ADR-0053 D2):
    /// one mark per commit page — the first record allocated in each page
    /// pushes its address. Address-ordered by construction (bump
    /// allocation); bounded by ring/page entries (stale marks pop as the
    /// boundary passes them).
    seal_marks: VecDeque<LogicalAddr>,
    /// Commit-page index (life-relative) of the newest mark.
    last_mark_page: u64,
    /// Flush-chunk ends appended but not yet confirmed (M4-S11, ADR-0056
    /// D3): ascending (the drive cursor is monotone), pruned below
    /// `flushed` after every confirm, capped by [`FLUSH_ENDS_CAP`]
    /// (overflow drops the smallest — a dominated confirm candidate;
    /// coarser confirmation, never a wrong boundary).
    flush_ends: VecDeque<u64>,
    /// The four write-path byte counters (M4-S13).
    write: WriteAccounting,
    /// Pipeline device bytes already folded into `write.flush_bytes`.
    /// The flush legs read the pipeline's monotone total and charge the
    /// delta once per slice (not per byte), so pairing a table with a
    /// recovered pipeline can never double-count and a slice that wrote
    /// nothing charges nothing.
    flush_device_seen: u64,
    /// Per-tier-file live-set counters (M4-S14, ADR-0058): fed by the
    /// same repoint/delete hooks as the space's dead-byte aggregate,
    /// filed by `flush_slice`, reconciled by the recovery appliers.
    live: LiveSet,
    /// Copy-forward configuration (M4-S15, ADR-0059 D1/D6).
    compact_cfg: compact::CompactionConfig,
    /// The in-flight compaction scan cursor — one candidate at a time
    /// (bounded state; ADR-0059 D2). `None` between candidates.
    compact: Option<compact::CompactCursor>,
    /// Relocation-origin map (M4-S15, ADR-0059 D9): for records
    /// relocated since their covering swap, the `(address, walk-stamp)`
    /// pairs un-superseded checkpoints may still reference, keyed by
    /// `(full hash, current address)`. Displacing mutations take these
    /// and stage one extra `ColdDisplace` per origin — the repair that
    /// keeps ADR-0057 D4's exact replay exact across unlogged
    /// relocations. Bounded: [`RELOC_ORIGIN_CAP`] origins per record
    /// (the scan defers at cap); entries drop at covering swaps;
    /// per-life (boot starts it empty — no live relocation exists).
    /// Hashed by `BuildIntHasher` (ADR-0093 A15): the key is a keyed
    /// SipHash output and an allocator-issued address, and a
    /// cell-resident map carries no per-process `RandomState` (L7).
    reloc_origins: HashMap<(u64, u64), Vec<(u64, u64)>, BuildIntHasher>,
    /// Blob-extent reference map + refcounts + reclaim queue (M4-S17,
    /// ADR-0061 D4/D5): fed by the same `note_death` routing as the
    /// live set; the checkpoint 0x05 section and the replay appliers
    /// rebuild it exactly at recovery.
    extents: crate::extents::ExtentRefs,
    /// Blob routing bounds (ADR-0061 D1; S19 owns the knob).
    blob: crate::extents::BlobConfig,
    /// Cell-local staging epoch: incremented per successful
    /// [`stage_wal`](Self::stage_wal). Extent deaths stamp with it and
    /// reclaim gates on `stamp ≤` the plane-supplied durable epoch —
    /// "the killing record is fsync-covered" in store-visible units
    /// (ADR-0061 D5).
    wal_epoch: u64,
    /// `DISK-BUDGET` (M4-S19, ADR-0062 D5): bounds tier files + extents.
    /// `0` = unbounded; nonzero drives the compaction pressure arm
    /// through [`disk_pressure`](Self::disk_pressure) and, since M4-S21
    /// (ADR-0063), the [`DiskAdmission`] bound enforced at the
    /// `append`/`append_extent` funnels.
    disk_budget_bytes: u64,
    /// Cached disk-admission verdict (M4-S21, ADR-0063 D2): re-derived
    /// from counted terms at the MAINTAIN cadence, debited per
    /// placement in between — the M1-S07 cached-flag pattern hardened
    /// into a countdown so between-round staleness cannot leak the RAM
    /// window past the budget.
    disk_admit: DiskAdmission,
    /// Pressure asked compaction for space and no eligible file had a
    /// dead byte — ADR-0059 D1's `nothing_compactable` verdict, counted
    /// (ADR-0063 D5: the honest "genuinely full of live data" alarm).
    compact_idle_pressure: u64,
    /// Read-driven promotion admission (M4.5-S30, ADR-0085 D6): the
    /// `tiered-promote-on-read` CONFIG key, pushed per cell. Disabled
    /// is fully inert (no filter traffic — the pre-S30 read path).
    promote_enabled: bool,
    /// The second-touch admission filter (ADR-0085 D2): 64 KiB fixed,
    /// direct-mapped, no per-record metadata — the bounded form of the
    /// LRU machinery §9 refuses.
    promote_filter: promote::PromoteFilter,
    /// Promotion observability (ADR-0085 D6) — `INFO tiering` renders
    /// these; the A/B and the DST oracles read them.
    promote_stats: promote::PromotionCounters,
    /// Shadow-slot tickets (M4.5-S37, ADR-0093): the open `(hash, cold,
    /// winner)` pairs, the record pin's source, the reconciler's work
    /// list — a projection of the index, rebuilt at recovery.
    shadow: shadow::ShadowSet,
}

/// Cached disk-admission state (M4-S21, ADR-0063 D2). Two legs: the
/// budget countdown (`headroom`) and the device latch. Refreshed where
/// both halves of `disk_used` are simultaneously fresh — after the
/// flush leg of a MAINTAIN round — and recomputed on budget hot-reload
/// once a usage snapshot exists. Until the first refresh admission is
/// open: recovery's re-appends replay bytes the prior life already
/// admitted, and refusing them would turn a full disk into a boot
/// failure (D5 — boot performs no tier writes).
#[derive(Copy, Clone, Debug, Default)]
struct DiskAdmission {
    /// Foreground bytes admissible before `budget − reserve` under the
    /// projection `disk_used + (tail − flushed)`; `None` = unbounded
    /// (zero budget, or no usage snapshot yet).
    headroom: Option<u64>,
    /// Device leg (ADR-0063 D4): a tier-flush write refused with
    /// ENOSPC; cleared by the next successful flush barrier — MAINTAIN
    /// retries the unflushed backlog, so recovery is automatic.
    device_full: bool,
    /// The tier-file byte half at the last refresh (`None` = never
    /// refreshed). Lets `set_disk_budget` recompute without the plane.
    last_tier_bytes: Option<u64>,
    /// `disk_used` at the last recompute — the refusal payload numbers.
    used: u64,
    /// Typed refusals issued (observable; `INFO tiering`).
    refusals: u64,
}

/// Cap on remembered relocation origins per record (ADR-0059 D9): a
/// record at cap defers further relocation until a covering swap
/// drains its entry — deferral, never growth. Public so the plane's
/// marker budgets derive from it (ADR-0093 A11) instead of restating it.
pub const RELOC_ORIGIN_CAP: usize = 3;

/// Cap on retained unconfirmed flush-chunk ends (~32 KiB worst case).
const FLUSH_ENDS_CAP: usize = 4096;

/// What one [`TieredTable::flush_slice`] round did (observability; the
/// storm/DST oracles assert against these).
#[derive(Copy, Clone, Default, Debug, PartialEq, Eq)]
pub struct FlushSliceOutcome {
    /// Record bytes appended to tier files this slice.
    pub appended_bytes: u64,
    /// ADR-0052 D2 sealed-dead gaps crossed (file sealed, no bytes).
    pub gaps_crossed: u32,
    /// Files sealed this slice (capacity, gap).
    pub files_sealed: u32,
    /// How far `flushed` advanced this slice, in bytes.
    pub confirmed_bytes: u64,
}

impl TieredTable {
    /// `None` when the ring reservation fails (namespace creation surfaces
    /// it typed).
    pub fn new(
        config: AddressSpaceConfig,
        demote: DemotionConfig,
        initial_keys: usize,
        hasher: KeyHasher,
    ) -> Option<TieredTable> {
        let mut space = AddressSpace::new(config)?;
        // The budget admission bound (ADR-0053 D1): alloc refuses — and
        // backpressure engages — at `budget + slice`, not at the (up to
        // 2×) ring wall. Nonsense budgets refuse typed here.
        let window = demote.mem_budget_bytes.checked_add(demote.slice_bytes)?;
        if window < 4 * space.page_bytes() {
            return None;
        }
        space.set_window_limit(window);
        Some(TieredTable {
            hasher,
            index: Index::with_capacity(initial_keys.max(64)),
            space,
            walk_ckpt_id: None,
            live_bytes: 0,
            demote,
            seal_marks: VecDeque::new(),
            last_mark_page: NO_MARK_PAGE,
            flush_ends: VecDeque::new(),
            write: WriteAccounting::default(),
            flush_device_seen: 0,
            live: LiveSet::new(config.life_origin.to_raw()),
            compact_cfg: compact::CompactionConfig::default(),
            compact: None,
            reloc_origins: HashMap::default(),
            extents: crate::extents::ExtentRefs::new(),
            blob: Self::clamp_blob_config(
                config.reserve_bytes as u64,
                crate::extents::BlobConfig::default(),
            ),
            wal_epoch: 0,
            disk_budget_bytes: 0,
            disk_admit: DiskAdmission::default(),
            compact_idle_pressure: 0,
            promote_enabled: true,
            promote_filter: promote::PromoteFilter::new(),
            promote_stats: promote::PromotionCounters::default(),
            shadow: shadow::ShadowSet::new(),
        })
    }

    /// The key hash under this table's secret (ADR-0094) — the same
    /// value as the node's memory-mode stores, so batch pipelines hash
    /// once regardless of table mode. An instance method: a hash is
    /// meaningful only to the table whose hasher computed it.
    #[inline]
    #[must_use]
    pub fn hash_key(&self, key: &[u8]) -> u64 {
        self.hasher.hash(key)
    }

    /// This table's key hasher.
    #[inline]
    #[must_use]
    pub fn hasher(&self) -> KeyHasher {
        self.hasher
    }

    /// Probe groups in the index (the SCAN cursor space; the rebuild's
    /// walk) — tests that craft home-group collisions size by it.
    #[must_use]
    pub fn index_group_count(&self) -> usize {
        self.index.group_count()
    }

    /// Read-only DST digest of every index entry and resident record (ADR-0137).
    /// Cold bytes are covered by the simulator's separate disk-image digest.
    pub fn simulation_digest(&self) -> crate::StateDigest {
        let mut digest = crate::StateDigest::default();
        for group in 0..self.index.group_count() {
            self.index.scan_home_group_ext(group, |addr, hash| {
                let mut entry = inf_foundation::hash64(&addr.to_raw().to_le_bytes(), hash);
                if addr.to_raw() >= self.space.head().to_raw() {
                    let parts = self.record(addr);
                    entry =
                        inf_foundation::hash64(self.record_bytes(addr, parts.encoded_len), entry);
                }
                digest.entries += 1;
                digest.digest = digest.digest.wrapping_add(entry);
            });
        }
        digest
    }

    /// Probe-line prefetch (the batch pipeline's phase 1 — L3).
    #[inline]
    pub fn prefetch(&self, hash: u64) {
        self.index.prefetch(hash);
    }

    /// Unverified-candidate record prefetch (phase 2): prefetches the
    /// candidate's record head lines **only when RAM-resident** — cold
    /// addresses are skipped (they suspend in S08 anyway; prefetching
    /// disk is meaningless).
    #[inline]
    pub fn prefetch_candidate(&self, hash: u64) {
        if let Some(addr) = self.index.find(hash, |_| true)
            && self.space.resolve(addr) != AddrClass::Cold
        {
            let head = self.space.bytes(addr, crate::record::HEADER_LEN).as_ptr();
            inf_simd::prefetch_read(head);
            inf_simd::prefetch_read(head.wrapping_add(64));
        }
    }

    /// Index lookup through the resolver. RAM candidates verify the key
    /// here; cold candidates return for the caller to fetch + verify
    /// (`exclude` carries fetched-and-mismatched addresses on retry — the
    /// false-positive path, ≈2⁻²² per candidate).
    pub fn lookup(&self, key: &[u8], hash: u64, exclude: &[LogicalAddr]) -> TieredLookup {
        debug_assert_eq!(hash, self.hash_key(key));
        let mut cold_candidate = None;
        let ram_hit = self.index.find(hash, |addr| match self.space.resolve(addr) {
            AddrClass::Mutable | AddrClass::ReadOnly => self.record(addr).key == key,
            AddrClass::Cold => {
                if cold_candidate.is_none() && !exclude.contains(&addr) {
                    cold_candidate = Some(addr);
                }
                false // keep probing — a RAM entry may still verify
            }
        });
        match (ram_hit, cold_candidate) {
            (Some(addr), _) => TieredLookup::Ram(addr),
            (None, Some(addr)) => TieredLookup::Cold(addr),
            (None, None) => TieredLookup::Miss,
        }
    }

    /// Whether the exact `(hash, addr)` pair is slotted — the sidecar's
    /// 64-bit hash, never the fingerprint (the oracles' and the DST's
    /// probe; ADR-0093's tickets are defined on this relation).
    #[must_use]
    pub fn contains_pair(&self, hash: u64, addr: LogicalAddr) -> bool {
        self.index.contains_pair(hash, addr)
    }

    /// Reads a RAM-resident record (header first, then the exact slice —
    /// the `record_at` shape over the resolver).
    ///
    /// # Panics
    /// Panics when `addr` is cold — RAM access below the head is a
    /// resolver bypass (the address space enforces it).
    pub fn record(&self, addr: LogicalAddr) -> RecordParts<'_> {
        let head = self.space.bytes(addr, crate::record::HEADER_LEN);
        let full_len = crate::record::encoded_len_from_header(head);
        RecordParts::of(RecordView::new(self.space.bytes(addr, full_len)))
    }

    /// Deserializes a cold-fetched record image (the S04/S08 resume path
    /// and the test oracle's simulated tier store). The bytes come from a
    /// CRC-protected tier page (S11); this trusts them like the arena
    /// trusts its own writes.
    pub fn decode_record(bytes: &[u8]) -> RecordParts<'_> {
        RecordParts::of(RecordView::new(bytes))
    }

    /// Number of bytes the record's fixed header is (the cold-read
    /// first-window size — S04/S08 read at least this much before they
    /// can size the full fetch).
    pub const RECORD_HEADER_LEN: usize = crate::record::HEADER_LEN;

    /// Bytes from a record's start that always hold its key: fixed header
    /// + TTL extension + the longest key (`key_from_prefix`'s bound).
    pub const KEY_PREFIX_LEN: usize =
        crate::record::HEADER_LEN + crate::record::TTL_EXT_LEN + crate::record::MAX_KEY_LEN;

    /// Decodes just the key from a record *prefix* (the SCAN cold-read
    /// window): `Some` when the prefix covers the header, the TTL
    /// extension when present, and the whole key — at most 268 bytes
    /// from the record's start, so one cold window always holds it.
    /// `None` means the prefix stops inside the key (review of
    /// 2026-08-30, C2: naming a key must never require the value's
    /// bytes).
    #[must_use]
    pub fn key_from_prefix(bytes: &[u8]) -> Option<&[u8]> {
        crate::record::key_from_prefix(bytes)
    }

    /// Sizes a full record from its fixed header alone (the cold-read
    /// two-round contract: a first aligned window covers at least the
    /// header; if the record overruns the window, the exact remainder is
    /// fetched in one more read — S08 replaces the second round with
    /// bounded chunked staging).
    ///
    /// # Panics
    /// Debug-panics when `head` is shorter than the fixed header.
    pub fn record_len_from_header(head: &[u8]) -> usize {
        crate::record::encoded_len_from_header(head)
    }

    /// Inserts a record for an absent key (the caller looked up first —
    /// the memory-mode `write_record` precondition, kept).
    pub fn insert(&mut self, key: &[u8], value: &[u8], hash: u64) -> Result<LogicalAddr, OpError> {
        // Absence precondition: no RAM-verified hit. A `Cold` answer is
        // NOT presence — a 2⁻²² fingerprint collision with a cold slot
        // legally reports a candidate for an absent key, so asserting
        // `Miss` here would panic on legal input.
        debug_assert_eq!(hash, self.hash_key(key));
        debug_assert!(
            self.index
                .find(hash, |addr| {
                    addr.to_raw() >= self.space.head().to_raw() && self.record(addr).key == key
                })
                .is_none(),
            "insert of a RAM-verified present key"
        );
        if self.index.needs_grow() {
            // Sidecar-only re-placement: cold-addressed slots re-place
            // without a record read (§3.3 — the closure has no record
            // access, so this is structural, not reviewed-for).
            self.index.grow(|_, ext| ext);
        }
        let addr = self.append(key, value, 0)?;
        self.index.insert(hash, addr);
        Ok(addr)
    }

    /// Routed mutation for a present key (M4-S05): an exact-fit new record
    /// image for an address still in the **mutable region** rewrites in
    /// place — same bytes footprint, version bumped, index and accounting
    /// untouched. Everything else (size change, or the record is sealed/
    /// cold) routes to [`overwrite`](Self::overwrite) — copy-to-tail, the
    /// emergent hot/cold filter (§9).
    ///
    /// The in-place rule is the M1 `Arena::resize_in_place` rule under
    /// exact bump allocation: the region has no size classes, so "same
    /// class" degenerates to "same encoded length". The boundary decision
    /// is the resolver's `Mutable` classification (one compare beyond the
    /// tail check), and [`AddressSpace::bytes_mut`] structurally refuses
    /// addresses below the ro-boundary even if routing here were wrong —
    /// the §3.1 corollary is enforced twice, not reviewed for.
    pub fn update(
        &mut self,
        key: &[u8],
        value: &[u8],
        hash: u64,
        old: LogicalAddr,
        old_len: usize,
        old_version: u32,
    ) -> Result<LogicalAddr, OpError> {
        debug_assert_eq!(hash, self.hash_key(key));
        let spec = RecordSpec {
            key,
            value,
            version: old_version.wrapping_add(1),
            expire_at_ms: None,
            kind: RecordKind::String { raw: false },
        };
        // The in-place branch additionally requires the OLD record to be
        // a plain string (one RAM byte — legal, the branch already
        // proved `Mutable`): a same-length in-place rewrite over a
        // `StringExtent` record would replace the reference without its
        // death ever reaching the refcount hook — the extent-leak shape
        // ADR-0061 D4 closes at exactly this condition.
        if spec.encoded_len() == old_len
            && self.space.resolve(old) == AddrClass::Mutable
            && self.space.bytes(old, 1)[0] >> 4 == crate::record::TypeTag::String as u8
        {
            spec.write(self.space.bytes_mut(old, old_len));
            // An in-place rewrite never reaches `append`, and it is a
            // user write of a full record image — charge it here or the
            // exact-fit workload reports an infinite write amplification
            // (M4-S13).
            self.write.user_bytes += (key.len() + value.len()) as u64;
            return Ok(old);
        }
        self.overwrite(key, value, hash, old, old_len, old_version)
    }

    /// Overwrites the record at `old` (key-verified by the caller's
    /// lookup): copy-to-tail + index repoint + version bump — the S06
    /// shape, address never rewritten in place (§3.1). `old_len` and
    /// `old_version` come from the caller's verified view (RAM read or
    /// cold fetch) — for a cold `old`, reading it here would be a
    /// synchronous disk touch.
    pub fn overwrite(
        &mut self,
        key: &[u8],
        value: &[u8],
        hash: u64,
        old: LogicalAddr,
        old_len: usize,
        old_version: u32,
    ) -> Result<LogicalAddr, OpError> {
        let new_addr = self.append(key, value, old_version.wrapping_add(1))?;
        self.index.replace(hash, old, new_addr);
        self.shadow_note_moved(hash, old, new_addr);
        self.note_death(old, old_len as u64);
        Ok(new_addr)
    }

    /// Deletes the record at `addr` (key-verified by the caller's lookup).
    /// For a cold record this touches the index and accounting only —
    /// never a cold read (§3.3).
    ///
    /// # Panics
    /// Panics when an open shadow ticket names `addr` as its winner
    /// (ADR-0093 D3/I7): the caller resolves the ticket first — a
    /// deleted key must never resurface with its unverified twin's
    /// value, and the plane's obligation is mechanical here.
    pub fn delete(&mut self, hash: u64, addr: LogicalAddr, len: usize) {
        assert!(
            self.shadow_of_winner(addr).is_none(),
            "delete of a shadow winner before its ticket resolved"
        );
        self.index.remove(hash, addr);
        self.shadow_note_removed(addr);
        self.note_death(addr, len as u64);
    }

    /// Live keys: the index minus the open shadow tickets (ADR-0093 D3
    /// — each ticket is exactly one extra slot for one key).
    #[inline]
    pub fn len(&self) -> usize {
        self.index.len() - self.shadow_pending()
    }

    /// True when empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    // ---- demotion slices (M4-S07, ADR-0053) ----

    /// This table's demotion configuration.
    #[inline]
    pub fn demotion(&self) -> DemotionConfig {
        self.demote
    }

    /// Hot-reloads the demotion configuration (M4-S19, ADR-0062 D3):
    /// budget/fraction/slice moves apply to future sealing and admission
    /// only — the ring reservation is fixed at construction, so a budget
    /// whose window would not fit the reserved ring refuses typed
    /// (drop + recreate is the growth path). Shrinks apply immediately;
    /// the pressure machinery engages on the next MAINTAIN round.
    ///
    /// # Errors
    /// A static reason (the command layer wraps it into the reply).
    pub fn set_demotion(&mut self, demote: DemotionConfig) -> Result<(), &'static str> {
        let window = demote
            .mem_budget_bytes
            .checked_add(demote.slice_bytes)
            .ok_or("MEM-BUDGET + MAINTAIN-SLICE overflows")?;
        if window < 4 * self.space.page_bytes() {
            return Err("MEM-BUDGET + MAINTAIN-SLICE is below four commit pages");
        }
        let ring = self.space.report().reserved_bytes;
        if window > ring {
            return Err("MEM-BUDGET exceeds the reserved ring — drop and recreate the \
                        namespace at the larger budget (ADR-0062 D3)");
        }
        self.demote = demote;
        self.space.set_window_limit(window);
        Ok(())
    }

    /// This table's disk budget (`0` = unbounded).
    #[inline]
    pub fn disk_budget(&self) -> u64 {
        self.disk_budget_bytes
    }

    /// Hot-reloads the disk budget (ADR-0062 D5 / ADR-0063 D2): a
    /// pressure threshold and an admission input. Recomputes the cached
    /// admission verdict when a usage snapshot exists; before the first
    /// [`refresh_disk_admission`](Self::refresh_disk_admission) the
    /// verdict stays open (recovery replay must never be refused).
    pub fn set_disk_budget(&mut self, bytes: u64) {
        self.disk_budget_bytes = bytes;
        if let Some(tier_bytes) = self.disk_admit.last_tier_bytes {
            self.recompute_disk_admission(tier_bytes);
        }
    }

    /// Re-derives the disk-admission verdict from counted terms (M4-S21,
    /// ADR-0063 D2). Call at the MAINTAIN point where both usage halves
    /// are fresh — after the flush leg (tier-file bytes just moved) and
    /// the reclaim leg (extent bytes just moved) — the same cadence the
    /// [`disk_pressure`](Self::disk_pressure) poll already rides.
    pub fn refresh_disk_admission(&mut self, tier_file_bytes: u64) {
        self.disk_admit.last_tier_bytes = Some(tier_file_bytes);
        self.recompute_disk_admission(tier_file_bytes);
    }

    fn recompute_disk_admission(&mut self, tier_file_bytes: u64) {
        let used = self.disk_used(tier_file_bytes);
        self.disk_admit.used = used;
        if self.disk_budget_bytes == 0 {
            self.disk_admit.headroom = None;
            return;
        }
        // Foreground stops at `budget − reserve` (ADR-0063 D3): the 5%
        // gap is what compaction copies forward into when the disk is
        // full — held open by asymmetry (`relocate` and flush are never
        // budget-refused), not by a second accounted bucket.
        let admit_limit = self.disk_budget_bytes - self.disk_budget_bytes / 20;
        // The projection: every admitted byte between `flushed` and
        // `tail` reaches a tier file exactly once (files are hole-free,
        // ADR-0056) — admission must provision for it or the cap is
        // fiction once the RAM window drains.
        let unflushed = self.space.tail().to_raw() - self.space.flushed().to_raw();
        self.disk_admit.headroom = Some(admit_limit.saturating_sub(used.saturating_add(unflushed)));
    }

    /// The current disk-admission refusal cause, if admission is closed
    /// (`INFO tiering`; the wiring-time gate consults this before
    /// `stage_wal` and before `ExtentWriter::create` — ADR-0063 D2).
    #[must_use]
    pub fn disk_full(&self) -> Option<DiskFullCause> {
        if self.disk_admit.device_full {
            return Some(DiskFullCause::Device);
        }
        match self.disk_admit.headroom {
            Some(0) => Some(DiskFullCause::Budget {
                used: self.disk_admit.used,
                budget: self.disk_budget_bytes,
            }),
            _ => None,
        }
    }

    /// Typed `DISKFULL` refusals issued by this table (observable).
    #[must_use]
    pub fn diskfull_refusals(&self) -> u64 {
        self.disk_admit.refusals
    }

    /// `disk_used` at the last admission recompute — the enforced
    /// snapshot (`INFO tiering`; 0 until the plane first refreshes).
    #[must_use]
    pub fn disk_admission_used(&self) -> u64 {
        self.disk_admit.used
    }

    /// Times pressure asked compaction for space and nothing was
    /// compactable (ADR-0063 D5 — the operator alarm: the namespace is
    /// genuinely full of live data; only deletes or a budget raise
    /// free space).
    #[must_use]
    pub fn compact_idle_pressure(&self) -> u64 {
        self.compact_idle_pressure
    }

    /// Admission check for `bytes` of new tier-byte placement (M4-S21):
    /// refusal is typed and mutates nothing. Split from
    /// [`disk_admit_debit`](Self::disk_admit_debit) so the debit can sit
    /// *after* the fallible alloc — a debit on a path that then refuses
    /// for memory would leak headroom until the next refresh.
    fn disk_admit_check(&mut self, bytes: u64) -> Result<(), OpError> {
        if self.disk_admit.device_full {
            self.disk_admit.refusals += 1;
            return Err(OpError::DiskFull(DiskFullCause::Device));
        }
        match self.disk_admit.headroom {
            Some(headroom) if headroom < bytes => {
                // The first refusal closes the verdict for the round:
                // a residual sub-record headroom must not read as
                // "open" in `INFO` while every real write refuses.
                self.disk_admit.headroom = Some(0);
                self.disk_admit.refusals += 1;
                Err(OpError::DiskFull(DiskFullCause::Budget {
                    used: self.disk_admit.used,
                    budget: self.disk_budget_bytes,
                }))
            }
            _ => Ok(()),
        }
    }

    /// Debits an admitted placement from the budget countdown.
    /// Approximation (frame/header overhead is not debited) cannot
    /// accumulate: the countdown is re-derived every refresh, and the
    /// D3 reserve absorbs one round's worth.
    fn disk_admit_debit(&mut self, bytes: u64) {
        if let Some(headroom) = self.disk_admit.headroom {
            self.disk_admit.headroom = Some(headroom.saturating_sub(bytes));
        }
    }

    /// This namespace's disk usage (ADR-0062 D5): the plane supplies the
    /// tier-file half (`inf-log` owns files — §3.3) and the store adds
    /// its extent device bytes, live and awaiting reclaim alike.
    #[must_use]
    pub fn disk_used(&self, tier_file_bytes: u64) -> u64 {
        tier_file_bytes.saturating_add(self.extents.stats().disk_bytes)
    }

    /// The compaction pressure signal S19's budget produces and S15's
    /// trigger arm consumes (ADR-0059 D1): engages at 7/8 of the budget
    /// — *before* the cap, so copy-forward frees space while admission
    /// still succeeds. Integer arithmetic; `0` budget never pressures.
    #[must_use]
    pub fn disk_pressure(&self, tier_file_bytes: u64) -> bool {
        if self.disk_budget_bytes == 0 {
            return false;
        }
        let threshold = self.disk_budget_bytes - self.disk_budget_bytes / 8;
        self.disk_used(tier_file_bytes) >= threshold
    }

    /// The composed pressure input for `compaction_work` (M4-S21,
    /// ADR-0063 D3): the ADR-0062 D5 materialized arm (`disk_used ≥
    /// 7/8 · budget`) **or** admission closed. The admission projection
    /// counts unflushed RAM bytes the 7/8 arm cannot see, so with a
    /// sizable window it closes first — without this composition,
    /// refusals could begin before compaction was ever pressured. By
    /// construction, compaction is reclaiming no later than the first
    /// refusal.
    #[must_use]
    pub fn compaction_pressure(&self, tier_file_bytes: u64) -> bool {
        self.disk_pressure(tier_file_bytes) || self.disk_full().is_some()
    }

    /// Mutable-region bytes (`tail − ro_boundary`; includes counted-dead
    /// ring-top holes — documented overcount, ADR-0053 D2).
    #[inline]
    pub fn mutable_bytes(&self) -> u64 {
        self.space.tail().to_raw() - self.space.ro_boundary().to_raw()
    }

    /// Whether a demotion round has work: the mutable region exceeds its
    /// fraction target, or flushed-confirmed bytes await release. A skip
    /// hint, not a progress guarantee — seal marks are page-granular, so
    /// a due table's seal step may legally advance nothing (drivers loop
    /// on [`seal_slice`](Self::seal_slice)/[`release_slice`](Self::release_slice)
    /// returns, never on this).
    pub fn demote_due(&self) -> bool {
        self.mutable_bytes() > self.demote.mutable_target_bytes()
            || self.space.flushed().to_raw() > self.space.head().to_raw()
    }

    // ---- recovery replay appliers (M4-S12, ADR-0057 D4) ----

    /// Applies one checkpoint address reference — idempotent by the
    /// exact `(hash, addr)` pair (the walker's at-least-once re-emission
    /// may duplicate a ref; a duplicated slot would outlive the single
    /// displacement removal and serve stale bytes after the key's next
    /// flush). Live-byte accounting is deliberately untouched: ref
    /// lengths are unknown without a record read — per-file counters
    /// boot *unreconciled* and S14's lazy rebuild owns them.
    ///
    /// # Panics
    /// Debug-panics when `addr` is not below this life's origin — refs
    /// name pre-life (manifested) addresses only; the `.ick` reader
    /// already refused anything at or above its section watermark.
    pub fn apply_ref(&mut self, hash: u64, addr: LogicalAddr) {
        debug_assert!(addr < self.space.life_origin(), "refs name pre-life addresses");
        if self.index.contains_pair(hash, addr) {
            return;
        }
        if self.index.needs_grow() {
            self.index.grow(|_, ext| ext);
        }
        self.index.insert(hash, addr);
        // Count the slot into its file (M4-S14, ADR-0058 D4): the
        // idempotency guard above already collapsed the walker's
        // at-least-once duplicates, so this is exactly once per
        // surviving slot — the count-side reconciliation the plan's
        // lazy walk was for, done for free where the fact is born.
        self.live.note_ref(addr.to_raw());
        // ADR-0093 D5: tickets are rebuilt from the finished index at
        // recovery-complete (`rebuild_shadow_tickets`), never from the
        // walk's home-group order here.
    }

    /// Applies one `ColdDisplace` marker (D4 rule 1): removes exactly
    /// the slot `(hash, old_addr)` if present. Absence is a legal
    /// interleaving (the walk imaged the key after the mutation, so the
    /// old-life slot was never recovered), never a desync.
    pub fn apply_displace(&mut self, hash: u64, old_addr: LogicalAddr) -> bool {
        if !self.index.remove_if_present(hash, old_addr) {
            return false;
        }
        // ADR-0093 D5: a marker that kills a ticket's slot ends the
        // ticket (a resolved shadow's `ColdDisplace(A)` arriving from
        // the tail, or a winner's own displacement).
        self.shadow_note_removed(old_addr);
        if old_addr < self.space.life_origin() {
            // The counted case (M4-S14, ADR-0058 D4): a restored ref
            // slot died to the tail's displacement — uncount it. The
            // blob reference releases here too (M4-S17): the this-life
            // branch routes through `note_death`, but this arm bypasses
            // it, and a restored 0x05 entry must decrement its extent
            // when the tail kills its record.
            self.live.note_displaced(old_addr.to_raw());
            self.extents.note_death(old_addr.to_raw());
        } else {
            // The numeric-collision case: the marker's crashed-life
            // address coincided with a this-life re-append of the same
            // key (both ranges start at the manifested watermark, so
            // collisions are legal, not rare). The removed slot's
            // record is RAM-resident this-life bytes that just became
            // unreachable — attribute the death *now*, with the length
            // read from RAM, or the per-file identity silently leaks
            // exactly this record when its range files. The paired
            // image/delete that follows the marker re-establishes the
            // key (ADR-0057 D4 pairing), so removing the slot here is
            // semantically the displacement it claims to be.
            let head = self.space.bytes(old_addr, crate::record::HEADER_LEN);
            let len = crate::record::encoded_len_from_header(head);
            self.note_death(old_addr, len as u64);
        }
        true
    }

    /// Blind key-verified RAM upsert (D4 rule 2) — checkpoint image and
    /// tail-SET replay. A cold candidate is deliberately **ignored**:
    /// old-life slots die by exact address (rule 1 / checkpoint order),
    /// never by key — verifying one here would be a cold read on the
    /// boot path.
    ///
    /// # Errors
    /// Space refusals from the store (recovery fail-stop at the caller).
    pub fn apply_image(
        &mut self,
        key: &[u8],
        value: &[u8],
        hash: u64,
    ) -> Result<LogicalAddr, OpError> {
        if let TieredLookup::Ram(addr) = self.lookup(key, hash, &[]) {
            let parts = self.record(addr);
            let (len, version) = (parts.encoded_len, parts.version);
            return self.overwrite(key, value, hash, addr, len, version);
        }
        self.insert(key, value, hash)
        // ADR-0093 D5: tickets are rebuilt from the finished index at
        // recovery-complete (`rebuild_shadow_tickets`), not here.
    }

    /// Tail-`DEL` replay (D4 rule 2's delete half): RAM-verified removal
    /// only — the paired displacement marker already killed any old-life
    /// slot by address. Returns whether a RAM entry was removed.
    pub fn apply_delete(&mut self, key: &[u8], hash: u64) -> bool {
        if let TieredLookup::Ram(addr) = self.lookup(key, hash, &[]) {
            let len = self.record(addr).encoded_len;
            // ADR-0093 D5: a replayed `Delete` may find a re-formed pair
            // whose twin the crashed life told apart as a *collision*
            // (its `DEL` staged no marker for it — the twin is another
            // key). The ticket ends and the twin stays slotted, which is
            // exactly the crashed life's verdict; a same-key twin was
            // killed by the `ColdDisplace(A)` the forced resolution's
            // markers carried, before this record. Never the foreground
            // `delete`'s assertion: replay cannot read.
            self.shadow_drop_winner_tickets(addr);
            self.delete(hash, addr, len);
            return true;
        }
        false
    }

    /// This namespace's MANIFEST v2 tier section (ADR-0057 D5): the
    /// flushed watermark plus the catalog's file ranges clamped to it —
    /// sealed files named at their manifested prefix (physical excess is
    /// inert), the active file at its confirmed durable prefix, files
    /// with nothing confirmed not named at all. Files retiring under the
    /// publication this section feeds are excluded (M4-S15, ADR-0059
    /// D3/D5 — the walk that fed it provably emitted no reference into
    /// them, and the resulting range gap is legal).
    pub fn tier_manifest<F: SegmentFs>(
        &self,
        ns: u32,
        flush: &TierFlush<F>,
    ) -> inf_log::manifest::TierNsManifest {
        use inf_log::manifest::{TierFileRange, TierNsManifest};
        let flushed = self.space.flushed().to_raw();
        let mut files: Vec<TierFileRange> = Vec::with_capacity(flush.sealed().len() + 1);
        for meta in flush.sealed() {
            if self.live.is_retiring(meta.id) {
                continue;
            }
            let base = meta.base.to_raw();
            let durable_len = (base + meta.data_len).min(flushed).saturating_sub(base);
            if durable_len > 0 {
                files.push(TierFileRange { id: meta.id, base, durable_len });
            }
        }
        // Files whose seal is staged but not completion-committed
        // (M4.5-S31, ADR-0084 D2) stay manifest-visible as unsealed
        // ranges at their flushed prefix — recovery must never treat a
        // mid-round file as dead-life garbage.
        for pending in flush.pending_seals() {
            let base = pending.base.to_raw();
            let durable_len = (base + pending.data_len).min(flushed).saturating_sub(base);
            if durable_len > 0 {
                files.push(TierFileRange { id: pending.id, base, durable_len });
            }
        }
        if let Some((id, base, _, _, _)) = flush.active() {
            let base = base.to_raw();
            let durable_len = flushed.saturating_sub(base);
            if durable_len > 0 {
                files.push(TierFileRange { id, base, durable_len });
            }
        }
        TierNsManifest { ns, flushed, files }
    }

    /// The flushed-watermark value a refused write of `key`/`value` must
    /// wait for (M4-S07 backpressure, ADR-0053 D4), counting the stall.
    /// `None` after a refused write means no watermark progress can help
    /// — a genuine out-of-space verdict, surfaced typed by the caller.
    pub fn write_stall_target(&mut self, key: &[u8], value: &[u8]) -> Option<LogicalAddr> {
        let spec = RecordSpec {
            key,
            value,
            version: 0, // length-only probe: version never changes encoded_len
            expire_at_ms: None,
            kind: RecordKind::String { raw: false },
        };
        self.stall_target_for(spec.encoded_len())
    }

    /// [`write_stall_target`](Self::write_stall_target) for a refused
    /// **blob** write (ADR-0102 D3, review of 2026-08-30 F-L06-05): the
    /// record that was refused is the 24-byte extent reference, never
    /// the value — sizing the probe from a 1 GiB value asserted above
    /// `ring / 2` and parked on the wrong watermark below it.
    pub fn extent_stall_target(&mut self, key: &[u8]) -> Option<LogicalAddr> {
        let reference = [0u8; crate::record::EXTENT_REF_LEN];
        let spec = RecordSpec {
            key,
            value: &reference,
            version: 0,
            expire_at_ms: None,
            kind: RecordKind::StringExtent,
        };
        self.stall_target_for(spec.encoded_len())
    }

    /// A total stall probe: a record the ring can never hold answers
    /// `None` ("no watermark progress can help") instead of reaching the
    /// space's release assert; a fitting one counts the stall.
    fn stall_target_for(&mut self, len: usize) -> Option<LogicalAddr> {
        if len > self.inline_record_max() {
            return None;
        }
        let target = self.space.stall_target(len)?;
        self.space.note_tail_alloc_stall();
        Some(target)
    }

    /// Counts one write replan (review of 2026-08-30, F-L06-03): the
    /// plane resolved a key, suspended on an extent read, and found the
    /// key's slot moved by the time it was ready to write — the write
    /// re-resolves instead of mutating through a stale address. Always
    /// on; rendered in `INFO tiering`.
    pub fn note_write_replan(&mut self) {
        self.space.note_write_replan();
    }

    /// The underlying address space (watermark advancement, counters,
    /// attribution). Flush/demotion slices (S07/S11) drive it through
    /// here; tests observe it.
    #[inline]
    pub fn space(&self) -> &AddressSpace {
        &self.space
    }

    /// Mutable space access for the lifecycle drivers (seal, flush
    /// confirmation, release — the §3.1 order is enforced inside).
    #[inline]
    pub fn space_mut(&mut self) -> &mut AddressSpace {
        &mut self.space
    }

    /// Raw RAM bytes of an address range — the flush pipeline's page
    /// source (S04/S11) and the test oracle's capture hook.
    #[inline]
    pub fn record_bytes(&self, addr: LogicalAddr, len: usize) -> &[u8] {
        self.space.bytes(addr, len)
    }

    /// Exact index + live-record accounting (L5): index bytes include the
    /// tiered hash sidecar; `live_bytes + space dead = space allocated`.
    pub fn index_bytes(&self) -> u64 {
        self.index.memory_bytes() as u64
    }

    /// Live record bytes (the accounting identity's left half).
    #[inline]
    pub fn live_bytes(&self) -> u64 {
        self.live_bytes
    }

    // ---- per-file live-set counters (M4-S14, ADR-0058) ----

    /// This namespace's per-tier-file live-set counters — S15's trigger
    /// and deletion-precondition input; the checkpoint walk driver
    /// serializes [`LiveSet::files`] into the `.ick` 0x04 section after
    /// this namespace's record/ref emission completes.
    #[inline]
    #[must_use]
    pub fn live_set(&self) -> &LiveSet {
        &self.live
    }

    /// Seeds the recovered files from the manifested catalog (recovery
    /// composition — `recover_tiered_ns` calls this before any replay;
    /// ADR-0058 D4). `boot_ckpt_id` is the manifested checkpoint's id —
    /// the recovered files' initial unref stamp (ADR-0059 D3).
    ///
    /// # Panics
    /// As [`LiveSet::seed_recovered`] — manifest-decode invariants fed
    /// back.
    pub fn seed_recovered_files(&mut self, catalog: &[TierFileMeta], boot_ckpt_id: u64) {
        self.live.seed_recovered(catalog, self.space.life_origin().to_raw(), boot_ckpt_id);
    }

    /// Restores one serialized live-set entry under the ADR-0058 D5
    /// clamp rules (the `.ick` 0x04 applier's per-entry step).
    pub fn restore_live_entry(&mut self, entry: &LiveSetFileEntry) {
        self.live.restore_entry(entry);
    }

    // ---- write-path accounting (M4-S13) ----

    /// This namespace's write-path byte counters for the current boot
    /// life (`INFO tiering`'s per-namespace line; M4-S16's write-amp
    /// input).
    #[inline]
    #[must_use]
    pub fn write_accounting(&self) -> WriteAccounting {
        self.write
    }

    /// Stages one mutation effect for this namespace into the cell's WAL
    /// ring and charges its encoded bytes — **the** WAL-append site for a
    /// tiered namespace, so `wal_bytes` cannot drift from what was
    /// actually staged (a bare `ring.stage()` on a tiered namespace's
    /// effect is the accounting bug this method exists to make
    /// unwritable). A refused staging wrote nothing and charges nothing.
    ///
    /// The caller routed to this table by namespace and owns the
    /// backpressure response to the refusal (M2-S08 admission).
    ///
    /// # Errors
    /// [`StagingFull`] — typed backpressure; the effect is not partially
    /// staged.
    pub fn stage_wal(
        &mut self,
        ring: &mut StagingRing,
        effect: &MutationEffect<'_>,
    ) -> Result<StagedAt, StagingFull> {
        let at = ring.stage(effect)?;
        self.write.wal_bytes += effect.encoded_len() as u64;
        // The staging epoch (M4-S17, ADR-0061 D5): parked extent deaths
        // stamp with the epoch of the next successful staging — exact
        // when the death's own effect stages next; conservative
        // (deferral, never early release) under any other order.
        self.wal_epoch += 1;
        self.extents.stamp(self.wal_epoch);
        Ok(at)
    }

    /// Charges device bytes written by a copy-forward compaction slice.
    /// M4-S15 owns the call site (copy-forward does not exist yet); the
    /// counter reads zero until then, and `INFO tiering` says zero
    /// because it *is* zero — not because nothing counted it.
    ///
    /// **Note for the S15 author:** compaction's re-append must not
    /// charge `user_bytes`. Relocating a live record is not new user
    /// traffic, and charging it would inflate the denominator by exactly
    /// the amount compaction inflates the numerator — hiding compaction's
    /// cost inside the ratio meant to expose it. [`append`](Self::append)
    /// charges, so copy-forward needs a non-charging placement path (or
    /// this counter's sign is wrong).
    #[inline]
    pub fn note_compaction_bytes(&mut self, bytes: u64) {
        self.write.compaction_bytes += bytes;
    }

    fn append(&mut self, key: &[u8], value: &[u8], version: u32) -> Result<LogicalAddr, OpError> {
        if key.len() > crate::record::MAX_KEY_LEN || value.len() > crate::record::MAX_VAL_LEN {
            return Err(OpError::TooLarge);
        }
        // Values at or above the blob threshold must take the extent
        // path (M4-S17, ADR-0061 D1) — the inline refusal is what makes
        // the plane's routing a checked contract, not a convention.
        if value.len() >= self.blob.threshold_bytes as usize {
            return Err(OpError::TooLarge);
        }
        let spec = RecordSpec {
            key,
            value,
            version,
            expire_at_ms: None,
            kind: RecordKind::String { raw: false },
        };
        let len = spec.encoded_len();
        // ADR-0102 D3 (review of 2026-08-30, F-L06-01): a record above
        // half the ring can never be placed — refuse typed before the
        // space's release assert can see it. The threshold clamp above
        // makes this unreachable through the plane's routing; a direct
        // caller (recovery replay of a foreign image, a test) gets the
        // same typed answer.
        if len > self.inline_record_max() {
            return Err(OpError::TooLarge);
        }
        // M4-S21 disk admission (ADR-0063 D1/D2): before the alloc, so
        // refusal mutates nothing; the debit follows the alloc, so a
        // memory refusal never leaks headroom. Recovery re-appends pass
        // unrefused by construction (admission is open until the
        // plane's first post-recovery refresh).
        self.disk_admit_check(len as u64)?;
        let addr = self.space.alloc(len).ok_or(OpError::OutOfMemory)?;
        self.disk_admit_debit(len as u64);
        self.shadow_note_alloc();
        self.note_seal_mark(addr);
        spec.write(self.space.bytes_mut(addr, len));
        self.live_bytes += len as u64;
        // M4-S13 user bytes at the record boundary: what the client asked
        // to store, not what the wire carried and not what the encoding
        // costs. Every record image the namespace admits passes here —
        // client writes, copy-to-tail relocations, and recovery's
        // re-appends alike — which is what makes write amplification a
        // well-defined ratio for the whole boot life. Compaction's
        // copy-forward is the one placement that must NOT charge here
        // (see `note_compaction_bytes`) — it moves bytes the user already
        // paid for.
        self.write.user_bytes += (key.len() + value.len()) as u64;
        Ok(addr)
    }

    /// Seal-mark bookkeeping (ADR-0053 D2): the first record starting
    /// in each commit page is a legal ro-boundary landing point. One
    /// compare per allocation; stale marks (behind the boundary) trim
    /// here so the deque stays bounded even when tests drive the
    /// watermarks directly. Every tail placement — user append and
    /// compaction relocation alike — must pass here, or `seal_slice`
    /// starves of landing points (M4-S15's relocation shares the site
    /// for exactly that reason).
    fn note_seal_mark(&mut self, addr: LogicalAddr) {
        let page = addr.offset_from(self.space.life_origin()) / self.space.page_bytes();
        if page != self.last_mark_page {
            let ro = self.space.ro_boundary();
            while self.seal_marks.front().is_some_and(|&mark| mark <= ro) {
                self.seal_marks.pop_front();
            }
            self.seal_marks.push_back(addr);
            self.last_mark_page = page;
        }
    }

    /// Places a verbatim record image at the tail (M4-S15, ADR-0059 D2)
    /// — copy-forward's placement primitive. Deliberately **not**
    /// [`append`](Self::append): the image copies bit-for-bit (kind,
    /// TTL, version — a re-encode would silently re-type any record
    /// shape `RecordSpec` does not round-trip), no WAL is staged
    /// (relocations are unlogged — §3.1 "replay rebuilds placement"),
    /// and no `user_bytes` are charged (moving bytes the user already
    /// paid for is compaction cost, `note_compaction_bytes`'s domain).
    /// `None` on admission refusal — the same window bound foreground
    /// obeys; the caller ends its slice (ADR-0059 D6, never a
    /// suspension inside the MAINTAIN round).
    fn relocate(&mut self, image: &[u8]) -> Option<LogicalAddr> {
        let len = image.len();
        debug_assert_eq!(
            crate::record::encoded_len_from_header(image),
            len,
            "relocation image is not exactly one record"
        );
        let addr = self.space.alloc(len)?;
        self.shadow_note_alloc();
        self.note_seal_mark(addr);
        self.space.bytes_mut(addr, len).copy_from_slice(image);
        self.live_bytes += len as u64;
        Some(addr)
    }

    /// Folds the paired pipeline's monotone device-byte total into
    /// `flush_bytes` (M4-S13). Called at the end of every flush leg;
    /// charging the delta (rather than a per-write callback) keeps the
    /// flush hot path untouched and makes the fold idempotent.
    fn charge_flush_device<F: SegmentFs>(&mut self, flush: &TierFlush<F>) {
        let total = flush.device_bytes();
        debug_assert!(total >= self.flush_device_seen, "pipeline device bytes are monotone");
        self.write.flush_bytes += total.saturating_sub(self.flush_device_seen);
        self.flush_device_seen = total;
    }
}

impl core::fmt::Debug for TieredTable {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TieredTable")
            .field("live", &self.index.len())
            .field("live_bytes", &self.live_bytes)
            .field("space", &self.space)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simulation_digest_observes_resident_changes_and_cold_index_without_mutation() {
        let mut table = table();
        let hash = table.hash_key(b"key");
        let address = table.insert(b"key", b"one", hash).unwrap();
        let before = table.simulation_digest();
        assert_eq!(before.entries, 1);
        assert_eq!(before, table.simulation_digest());
        assert_eq!(table.walk_ckpt_id(), None);
        let record = table.record(address);
        let (len, version) = (record.encoded_len, record.version);
        table.update(b"key", b"two", hash, address, len, version).unwrap();
        assert_ne!(before, table.simulation_digest());
        assert_eq!(table.walk_ckpt_id(), None);
        let mut cold = cold_table();
        let empty = cold.simulation_digest();
        cold.apply_ref(17, LogicalAddr::ZERO);
        assert_ne!(empty, cold.simulation_digest());
        assert_eq!(cold.simulation_digest().entries, 1);
        assert_eq!(cold.walk_ckpt_id(), None);
    }

    fn cold_table() -> TieredTable {
        let mut cold = TieredTable::new(
            AddressSpaceConfig {
                reserve_bytes: 1 << 16,
                page_bytes: 1 << 12,
                life_origin: LogicalAddr::from_raw(1 << 12).unwrap(),
            },
            DemotionConfig::for_budget(1 << 16, 1 << 12),
            64,
            KeyHasher::default(),
        )
        .unwrap();
        cold.seed_recovered_files(
            &[TierFileMeta {
                id: 0,
                base: LogicalAddr::ZERO,
                data_len: 1 << 12,
                reason: inf_log::tier::SealReason::Capacity,
                path: "tier-0.itier".into(),
            }],
            0,
        );
        cold
    }

    fn assert_insert_does_not_resolve_cold(blob: bool) {
        let mut table = cold_table();
        let hash = table.hash_key(b"absent");
        table.apply_ref(hash, LogicalAddr::ZERO);
        let before = table.space.counters().cold_resolves;
        if blob {
            use inf_log::blob::{ExtentId, ExtentWriter};
            let fs = inf_log::fs::mem::MemFs::new();
            let mut writer = ExtentWriter::create(
                &fs,
                std::path::Path::new("shard"),
                ExtentId(0),
                0,
                inf_log::NsId(1),
                3,
                inf_log::TierIoMode::Buffered,
            )
            .unwrap();
            writer.append_chunk(b"one").unwrap();
            let sealed = writer.finish().unwrap();
            table.insert_extent(b"absent", hash, &sealed).unwrap();
        } else {
            table.insert(b"absent", b"one", hash).unwrap();
        }
        assert_eq!(table.space.counters().cold_resolves, before);
    }

    #[test]
    fn inline_insert_assertion_does_not_count_a_cold_lookup() {
        assert_insert_does_not_resolve_cold(false);
    }

    #[test]
    fn extent_insert_assertion_does_not_count_a_cold_lookup() {
        assert_insert_does_not_resolve_cold(true);
    }

    fn table() -> TieredTable {
        TieredTable::new(
            AddressSpaceConfig {
                reserve_bytes: 1 << 16,
                page_bytes: 1 << 12,
                life_origin: LogicalAddr::ZERO,
            },
            DemotionConfig::for_budget(1 << 16, 1 << 12),
            64,
            KeyHasher::default(),
        )
        .expect("reservation")
    }

    fn find(table: &TieredTable, key: &[u8]) -> (LogicalAddr, usize, u32) {
        let hash = table.hash_key(key);
        match table.lookup(key, hash, &[]) {
            TieredLookup::Ram(addr) => {
                let parts = table.record(addr);
                (addr, parts.encoded_len, parts.version)
            }
            other => panic!("expected RAM hit, got {other:?}"),
        }
    }

    /// M4-S05: an exact-fit update of a mutable-region record rewrites in
    /// place — same address, version bumped, no accounting movement.
    #[test]
    fn update_in_place_when_mutable_and_exact_fit() {
        let mut t = table();
        let hash = t.hash_key(b"k");
        let addr = t.insert(b"k", b"aaaa", hash).expect("fits");
        let (_, len, version) = find(&t, b"k");
        let dead_before = t.space().report().dead_bytes;
        let placed = t.update(b"k", b"bbbb", hash, addr, len, version).expect("fits");
        assert_eq!(placed, addr, "exact fit stays in place");
        let (_, _, new_version) = find(&t, b"k");
        assert_eq!(new_version, version.wrapping_add(1));
        assert_eq!(t.record(addr).value, b"bbbb");
        assert_eq!(t.space().report().dead_bytes, dead_before, "in place moves no accounting");
    }

    /// M4-S05/S06: a size-changing update relocates — copy-to-tail, index
    /// repoint, old bytes attributed dead at the repoint moment.
    #[test]
    fn update_relocates_on_size_change() {
        let mut t = table();
        let hash = t.hash_key(b"k");
        let addr = t.insert(b"k", b"aaaa", hash).expect("fits");
        let (_, len, version) = find(&t, b"k");
        let dead_before = t.space().report().dead_bytes;
        let placed = t.update(b"k", b"longer-value", hash, addr, len, version).expect("fits");
        assert_ne!(placed, addr, "size change copies to tail");
        assert_eq!(t.space().report().dead_bytes, dead_before + len as u64);
        let (found, _, new_version) = find(&t, b"k");
        assert_eq!(found, placed);
        assert_eq!(new_version, version.wrapping_add(1));
        assert_eq!(t.record(placed).value, b"longer-value");
    }

    /// M4-S06: a sealed (read-only) record copies to the tail even on an
    /// exact fit — the §3.1 corollary the routing must never violate.
    #[test]
    fn update_of_sealed_record_copies_to_tail() {
        let mut t = table();
        let hash = t.hash_key(b"k");
        let addr = t.insert(b"k", b"aaaa", hash).expect("fits");
        let (_, len, version) = find(&t, b"k");
        let tail = t.space().tail();
        t.space_mut().advance_ro_boundary(tail);
        let placed = t.update(b"k", b"bbbb", hash, addr, len, version).expect("fits");
        assert_ne!(placed, addr, "sealed record never rewrites in place");
        assert_eq!(t.space().resolve(placed), AddrClass::Mutable, "the copy is hot again");
        assert_eq!(t.record(placed).value, b"bbbb");
        assert_eq!(t.record(placed).version, version.wrapping_add(1));
        assert_eq!(t.space().report().dead_bytes, len as u64);
    }

    /// M4-S07: a seal slice advances the ro-boundary toward the mutable-
    /// fraction target, lands only on record starts, and respects the
    /// per-slice byte bound (ADR-0053 D2/D3).
    #[test]
    fn seal_slice_lands_on_record_boundaries_within_the_slice() {
        // Budget = quarter ring, target = 25% of that, slice = one page.
        let mut t = TieredTable::new(
            AddressSpaceConfig {
                reserve_bytes: 1 << 16,
                page_bytes: 1 << 12,
                life_origin: LogicalAddr::ZERO,
            },
            DemotionConfig::for_budget(1 << 14, 1 << 12),
            64,
            KeyHasher::default(),
        )
        .expect("reservation");
        // ~40 records ≈ 2.5 pages of mutable bytes (record ≈ 260 B).
        let mut starts = Vec::new();
        for i in 0..40u32 {
            let key = format!("k:{i:04}");
            let addr = t.insert(key.as_bytes(), &[0xAB; 240], t.hash_key(key.as_bytes()));
            starts.push(addr.expect("fits").to_raw());
        }
        let target = t.demotion().mutable_target_bytes();
        assert!(t.mutable_bytes() > target, "the fill exceeds the fraction target");
        let sealed = t.seal_slice();
        assert!(sealed > 0, "a due table seals");
        // The step bound: one slice plus the page-granular mark overshoot.
        assert!(sealed <= t.demotion().slice_bytes + 2 * t.space().page_bytes());
        let ro = t.space().ro_boundary().to_raw();
        assert!(starts.contains(&ro), "the boundary is a record start");
        // Drain to the target: repeated slices converge and stop.
        let mut rounds = 0;
        while t.seal_slice() > 0 {
            rounds += 1;
            assert!(rounds < 64, "seal must converge");
        }
        assert!(
            t.mutable_bytes() >= target,
            "sealing never overshoots the mutable target past a record"
        );
        let counters = t.space().counters();
        assert!(counters.demote_slices >= 1, "slices counted");
        assert_eq!(counters.demote_sealed_bytes, t.space().ro_boundary().to_raw());
    }

    /// M4-S07: a record wider than the slice still seals — minimum one
    /// record of progress per step (the ADR-0053 D2 anti-deadlock rule).
    #[test]
    fn seal_slice_makes_progress_past_an_oversized_record() {
        let mut t = TieredTable::new(
            AddressSpaceConfig {
                reserve_bytes: 1 << 16,
                page_bytes: 1 << 12,
                life_origin: LogicalAddr::ZERO,
            },
            // Target 0 mutable bytes: everything is seal debt.
            DemotionConfig { mem_budget_bytes: 1 << 14, mutable_permille: 0, slice_bytes: 256 },
            64,
            KeyHasher::default(),
        )
        .expect("reservation");
        let hash = t.hash_key(b"wide");
        let wide = t.insert(b"wide", &[0x77; 8 << 10], hash).expect("fits");
        let hash2 = t.hash_key(b"next");
        let next = t.insert(b"next", &[0x11; 64], hash2).expect("fits");
        // First slice: the wide record exceeds 256 B but seals whole.
        assert!(t.seal_slice() > 256, "minimum one record of progress");
        assert_eq!(t.space().ro_boundary(), next, "sealed exactly to the next record start");
        assert_eq!(t.space().resolve(wide), AddrClass::ReadOnly);
        // Remaining debt drains to the tail record boundary.
        while t.seal_slice() > 0 {}
        assert_eq!(t.space().ro_boundary(), next, "the tail record has no successor mark yet");
    }

    /// M4-S07 backpressure: a refused write names the flushed watermark
    /// that unblocks it, and after flush + release at that target the
    /// same write fits (ADR-0053 D4 — exact arithmetic, no spurious wake).
    #[test]
    fn stall_target_names_the_unblocking_flushed_watermark() {
        let mut t = table();
        let value = vec![0x5A; (1 << 14) - 32];
        let hash = t.hash_key(b"a");
        t.insert(b"a", &value, hash).expect("fits");
        let hash_b = t.hash_key(b"b");
        t.insert(b"b", &value, hash_b).expect("fits");
        let hash_c = t.hash_key(b"c");
        t.insert(b"c", &value, hash_c).expect("fits");
        let hash_d = t.hash_key(b"d");
        t.insert(b"d", &value, hash_d).expect("fits");
        // The ring (64 KiB) is full: the next write must stall.
        assert!(t.insert(b"e", &value, t.hash_key(b"e")).is_err());
        let target = t.write_stall_target(b"e", &value).expect("watermark progress can help");
        assert_eq!(t.space().counters().tail_alloc_stalls, 1, "the stall is tripwired");
        // MAINTAIN's job, played by the test: seal → flush → release to
        // (at least) the target, then the retried write fits.
        let tail = t.space().tail();
        t.space_mut().advance_ro_boundary(tail);
        t.space_mut().advance_flushed(tail);
        assert!(tail >= target, "the sealed range covers the stall target");
        while t.release_slice() > 0 {}
        t.insert(b"e", &value, t.hash_key(b"e")).expect("fits after release");
    }

    /// M4-S07: release steps are slice-bounded and stop at `flushed`.
    #[test]
    fn release_slice_is_bounded_and_stops_at_flushed() {
        let mut t = table();
        for i in 0..8u32 {
            let key = format!("r:{i}");
            t.insert(key.as_bytes(), &[0x33; 4000], t.hash_key(key.as_bytes())).expect("fits");
        }
        let tail = t.space().tail();
        t.space_mut().advance_ro_boundary(tail);
        t.space_mut().advance_flushed(tail);
        let slice = t.demotion().slice_bytes;
        let mut released_total = 0;
        loop {
            let released = t.release_slice();
            if released == 0 {
                break;
            }
            assert!(released <= slice, "one slice per step");
            released_total += released;
        }
        assert_eq!(released_total, tail.to_raw(), "released exactly the flushed range");
        assert_eq!(t.space().head(), t.space().flushed(), "release stops at flushed");
        assert_eq!(t.release_slice(), 0, "nothing left to release");
    }

    /// M4-S05: the M3 document in-place mutation shape above the boundary
    /// — value-byte surgery plus a version bump through `bytes_mut`, no
    /// record rewrite (ADR-0037 D4 / ADR-0043 over the address space; the
    /// below-boundary refusal is `write_below_ro_boundary_panics` in
    /// `address_space.rs`).
    #[cfg(feature = "doc")]
    #[test]
    fn doc_shape_in_place_patch_above_boundary() {
        let mut t = table();
        let hash = t.hash_key(b"doc");
        let addr = t.insert(b"doc", b"01234567", hash).expect("fits");
        let (_, len, version) = find(&t, b"doc");
        let record = t.space_mut().bytes_mut(addr, len);
        let view = RecordView::new(record);
        let value_at = view.value_offset();
        record[value_at..value_at + 2].copy_from_slice(b"98");
        crate::record::bump_version_in_place(record);
        let parts = t.record(addr);
        assert_eq!(parts.value, b"98234567");
        assert_eq!(parts.version, version.wrapping_add(1));
    }
}
