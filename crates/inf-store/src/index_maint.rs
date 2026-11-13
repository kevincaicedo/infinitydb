//! At-mutation index maintenance (M4.5-S04; contract ADR-0072 D3/D5/D6/D7,
//! mechanics ADR-0139): the per-store **attach block** — every live index
//! on this store's namespace with its decoded program and its tree — plus
//! the bracket that makes index updates atomic with the mutation from
//! every observer's view (single-threaded cell, L1).
//!
//! The bracket (ADR-0139 step table, ordering load-bearing): the write
//! set and the participating set are noted, the pre-image is evaluated
//! into per-store scratch and the `new` side's scratch is reserved → the
//! mutation applies (and stages) → post-image evaluation → entry diff
//! (deduplicated `(typed key, pk ref)` pairs) → tree ops. A bracket ends
//! only in its commit-half; there is no abort.
//!
//! **Identity (ADR-0139 D2).** The pk ref is the store's keyed hash of
//! the full key, so two keys can share one, and two such documents with
//! an equal indexed value share one tree entry. An entry is therefore a
//! fact about the ref's whole *alias group*: it is removed only after a
//! complete, bounded enumeration of the group (`index_alias`) found no
//! other member that holds the key. The tree's `remove` takes the
//! enumeration's view as a witness.
//!
//! **Coverage (ADR-0139 D4).** Record deaths run the death hook at the
//! death site — the last moment the dying document's values are
//! readable — unless the bracket's diff owns them. That is decided once,
//! from the entry point and the **full key**: an eviction entry point is
//! never covered (and makes the bracket forget what `old` claimed for a
//! write-set key); any other death is covered iff its full key is in the
//! write set and no index is pruned. The hook is `doc`-gated throughout:
//! a slim build compiles it out entirely (it refuses index-bearing
//! catalogs, ADR-0075 D2.5).

#[cfg(feature = "doc")]
use crate::index_key::KeySkip;

/// The maintenance rules this binary keeps its trees under (ADR-0139
/// D11): `1` = group identity and coverage by entry point and full key.
/// A sidecar written under any other value is discarded and rebuilt
/// (ADR-0078 A2). Bump it whenever a rules change alters the set a
/// converged tree holds; the field is three bits (`IdxSidecarRules`).
pub const IDX_MAINT_RULES_VERSION: u8 = 1;
/// [`IDX_MAINT_RULES_VERSION`] as the sidecar carries it — the store
/// hands this to the checkpoint writer and compares it at load.
pub const IDX_MAINT_RULES: inf_log::IdxSidecarRules =
    match inf_log::IdxSidecarRules::new(IDX_MAINT_RULES_VERSION) {
        Some(rules) => rules,
        None => panic!("the maintenance-rules version outgrew the sidecar's three bits"),
    };

/// Hard cap on scratch entries per bracket phase (bounded everything): a
/// mutation whose pre-image would exceed this refuses typed; a post-image
/// that would exceed it degrades the participating indexes (ADR-0139 D12
/// — wrong results are never served either way).
pub const BRACKET_ENTRY_CAP: usize = 65_536;

/// Retained bracket scratch per store (ADR-0139 D10): a
/// pathological wildcard document grows the scratch to its match set —
/// up to [`BRACKET_ENTRY_CAP`] entries of up to `ORDERED_KEY_MAX` bytes
/// per phase — and the store must not keep paying for it in RSS. Past
/// these bounds the buffers shrink back when the bracket (or death
/// hook) closes; the retained scratch is attributed in
/// `doc_scratch_bytes` either way (L5). Encoded-key bytes per buffer.
pub const SCRATCH_RETAIN_BYTES: usize = 64 << 10;
/// Entries per scratch set past which the set shrinks back.
pub const SCRATCH_RETAIN_ENTRIES: usize = 4096;
/// Write sets up to this many keys are scanned linearly per death;
/// wider ones are sorted by hash at bracket open and binary-searched
/// (the crossover measured between 64 and 256 keys).
#[cfg(feature = "doc")]
const WRITE_SET_LINEAR_MAX: usize = 64;

/// Why the bracket pre-half refused the mutation (typed, mapped to a
/// RESP error at the command layer — ADR-0072 D7.1: nothing changed).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum IdxMaintRefusal {
    /// The plan-then-commit reservation found no headroom, or the
    /// bracket's scratch could not grow (`idx_reserve_refuse`,
    /// `idx_scratch_refuse`).
    Reserve,
    /// Pre-image evaluation overflowed the entry, match or key-bytes
    /// caps.
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

/// Maintenance counters (ADR-0139 D8; skip vocabulary ADR-0074 D6).
/// Population: **cell** — one index on one store, or the store itself
/// for the death- and bracket-scoped events; the fold is the node.
/// Nothing skips, prunes, degrades or suppresses silently (L10).
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
    /// Removals suppressed because another member of the ref's alias
    /// group holds the key (ADR-0139 D2 rule 2).
    pub alias_kept: u64,
    /// Enumerations that found at least one alias.
    pub alias_groups: u64,
    /// Scratch entries marked *held* — the alias marking's work. An
    /// equal run is marked once, so this is at most the entries the
    /// removals evaluated, however often a member repeats a value.
    pub alias_held_marks: u64,
    /// Deaths whose hash was in the open bracket's write set while their
    /// full key was not — the cases a hash-only coverage rule got wrong.
    pub cover_alias: u64,
    /// Enumerations that crossed a walk budget (`AliasWalk::Over`).
    pub alias_walk_over: u64,
    /// The longest probe chain an enumeration loaded, in control groups
    /// (a maximum, not a sum).
    pub alias_walk_groups_max: u64,
    /// Write-set keys that died at an eviction entry point inside their
    /// own bracket (ADR-0139 D4's forget).
    pub gate_forget: u64,
    /// Pruned brackets whose key died in the command body.
    pub prune_void: u64,
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
        self.alias_kept += other.alias_kept;
        self.alias_groups += other.alias_groups;
        self.alias_held_marks += other.alias_held_marks;
        self.cover_alias += other.cover_alias;
        self.alias_walk_over += other.alias_walk_over;
        self.alias_walk_groups_max = self.alias_walk_groups_max.max(other.alias_walk_groups_max);
        self.gate_forget += other.gate_forget;
        self.prune_void += other.prune_void;
    }

    #[cfg(feature = "doc")]
    fn note_walk(&mut self, groups: u32) {
        self.alias_walk_groups_max = self.alias_walk_groups_max.max(u64::from(groups));
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
    use inf_foundation::{KeyHasher, fault};

    use super::{
        BRACKET_ENTRY_CAP, IdxCounters, IdxMaintRefusal, MaintMode, SCRATCH_RETAIN_BYTES,
        SCRATCH_RETAIN_ENTRIES, WRITE_SET_LINEAR_MAX,
    };
    use crate::doc;
    use crate::index::Index;
    use crate::index_alias::{AliasView, AliasWalk, Candidate, alias_view};
    use crate::index_key::{IndexKeyBuf, IndexKeyType, IndexScalar, index_key_encode};
    use crate::index_registry::{INDEXES_PER_NODE_MAX, IndexId, IndexMemory, IndexTree};
    use crate::limits::BRACKET_KEY_BYTES_MAX;
    use crate::ordered::PkRef;
    use crate::record::MAX_KEY_LEN;
    use crate::store::{CellStore, record_at};

    /// One attached index: the maintenance-facing cache of the registry
    /// entry (ADR-0139 D1 — recomputed at DDL transitions, never
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

    impl AttachedIndex {
        fn degrade(&mut self) {
            if !self.degraded {
                self.degraded = true;
                self.counters.degraded_trips += 1;
            }
        }
    }

    /// The attach ordinal inside [`ScratchEntry::ord`]; the two bits
    /// above are marks. Every sort, dedup and merge key reads the masked
    /// ordinal, so a mark never changes what the diff compares
    /// (ADR-0139 D9).
    const ORD_MASK: u16 = 0x00FF;
    /// The entry's write-set key died at an eviction entry point and its
    /// hook already removed what this entry claimed (ADR-0139 D4's
    /// "forget"). Compacted out before the commit-half sorts.
    const ORD_DEAD: u16 = 0x4000;
    /// Another member of the ref's alias group holds this key: the
    /// removal is suppressed (ADR-0139 D2 rule 2). Set only after the
    /// sort, on every entry of an equal run.
    const ORD_HELD: u16 = 0x8000;

    const _: () = assert!(INDEXES_PER_NODE_MAX <= ORD_MASK as usize + 1, "ordinal fits its mask");
    // Both phases share one key buffer and `ScratchEntry::off` is `u32`.
    const _: () = assert!(2 * BRACKET_KEY_BYTES_MAX <= u32::MAX as usize, "offsets fit u32");

    /// One scratch entry: an encoded typed key (a range of the phase's
    /// byte buffer) and its pk ref, tagged by attach ordinal.
    #[derive(Copy, Clone, Debug)]
    struct ScratchEntry {
        ord: u16,
        entry_ref: u64,
        off: u32,
        len: u16,
    }

    const _: () = assert!(size_of::<ScratchEntry>() == 16, "a scratch entry is 16 B");

    impl ScratchEntry {
        /// The encoded key this entry spans in `bytes`.
        fn key_in<'a>(&self, bytes: &'a [u8]) -> &'a [u8] {
            &bytes[self.off as usize..self.off as usize + self.len as usize]
        }

        fn ordinal(&self) -> usize {
            usize::from(self.ord & ORD_MASK)
        }

        /// The diff's order: one ref's entries are contiguous, so a
        /// single alias view serves a whole run of removals.
        fn sort_key<'a>(&self, bytes: &'a [u8]) -> (u64, u16, &'a [u8]) {
            (self.entry_ref, self.ord & ORD_MASK, self.key_in(bytes))
        }
    }

    /// One write-set key of the open bracket: its hash (the prefilter),
    /// its bytes (the identity — ADR-0139 D4) and the `old` range its
    /// pre-image evaluated into.
    #[derive(Copy, Clone, Debug)]
    struct WriteKey {
        hash: u64,
        key_off: u32,
        key_len: u16,
        /// False for an argv key longer than `MAX_KEY_LEN`: it cannot
        /// name a record, is not copied, and never tests as a member.
        named: bool,
        old_start: u32,
        old_end: u32,
    }

    const _: () = assert!(size_of::<WriteKey>() == 24, "a write-set key is 24 B");

    /// How a dying record relates to the open bracket's write set.
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    enum WriteSetHit {
        /// The full key is a write-set key.
        Key,
        /// Only the hash matched: an alias of a write-set key.
        HashOnly,
        Miss,
    }

    /// Which indexes a record death must run its hook for (ADR-0139
    /// D4's table, decided once from entry point, full key, prune mask).
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub(crate) enum DeathHook {
        /// A write-set key dying in its own command body with no prune:
        /// the bracket's diff owns the change.
        Covered,
        /// Every attached index.
        All,
        /// Only the indexes in this mask — the bracket's pruned ones,
        /// which have no `old` and will get no `new` (the void prune).
        Masked(u64),
    }

    /// What the alias enumeration and a member's evaluation read: the
    /// record table, the records, the documents and the keyed hasher.
    #[derive(Copy, Clone)]
    pub(crate) struct AliasCtx<'a> {
        pub(crate) arena: &'a inf_alloc::Arena,
        pub(crate) index: &'a Index,
        pub(crate) docs: &'a doc::DocStore,
        pub(crate) hasher: KeyHasher,
    }

    impl AliasCtx<'_> {
        /// One record fetch: is the fragment match at `addr` a write-set
        /// key (`excluded`, by full key — cheaper than the keyed hash,
        /// and in the common one-member group the only match there is),
        /// an alias of `hash`, or a 22-bit neighbour.
        fn inspect(
            &self,
            addr: inf_alloc::ArenaAddr,
            hash: u64,
            excluded: impl FnOnce(&[u8]) -> bool,
        ) -> Candidate {
            let key = record_at(self.arena, addr).key();
            if excluded(key) {
                Candidate::Excluded
            } else if self.hasher.hash(key) == hash {
                Candidate::Alias
            } else {
                Candidate::Neighbour
            }
        }

        fn doc_root(&self, addr: inf_alloc::ArenaAddr) -> Option<DocValue<'_>> {
            let len = record_at(self.arena, addr).encoded_len();
            doc::doc_root_at(self.arena, self.docs, addr, len)
        }
    }

    /// Grows a scratch buffer fallibly (ADR-0139 D10): capacity the
    /// bracket already owns is free; anything more is a `try_reserve`,
    /// and the `idx_scratch_refuse` fault point stands in for its failure.
    fn grow<T>(buf: &mut Vec<T>, additional: usize) -> Result<(), IdxMaintRefusal> {
        if buf.capacity() - buf.len() >= additional {
            return Ok(());
        }
        if fault::fire(crate::fault::IDX_SCRATCH_REFUSE) {
            return Err(IdxMaintRefusal::Reserve);
        }
        buf.try_reserve(additional).map_err(|_| IdxMaintRefusal::Reserve)
    }

    /// Appends one encoded key to a phase's set, under the entry cap and
    /// the key-bytes cap (`EntryFlood`) and fallible growth (`Reserve`).
    fn push_entry(
        bytes: &mut Vec<u8>,
        list: &mut Vec<ScratchEntry>,
        phase_key_bytes: &mut usize,
        encoded: &[u8],
        ord: u16,
        hash: u64,
    ) -> Result<(), IdxMaintRefusal> {
        if list.len() == BRACKET_ENTRY_CAP
            || *phase_key_bytes + encoded.len() > BRACKET_KEY_BYTES_MAX
        {
            return Err(IdxMaintRefusal::EntryFlood);
        }
        grow(bytes, encoded.len())?;
        grow(list, 1)?;
        // In range: both phases together stay under `u32::MAX` (above).
        let off = bytes.len() as u32;
        bytes.extend_from_slice(encoded);
        *phase_key_bytes += encoded.len();
        list.push(ScratchEntry { ord, entry_ref: hash, off, len: encoded.len() as u16 });
        Ok(())
    }

    /// Whether an evaluation's sparse/inexact skips are this index's
    /// maintenance events (counted) or an alias member being read on
    /// another document's behalf (silent).
    #[derive(Copy, Clone, PartialEq, Eq)]
    enum Skips {
        Counted,
        Silent,
    }

    /// Streams one document's encoded keys for `entry` through `sink`.
    /// `TooManyMatches` surfaces as `EntryFlood`: the document cannot
    /// enter or leave the index whole.
    fn eval_keys(
        entry: &mut AttachedIndex,
        root: DocValue<'_>,
        limits: &EvalLimits,
        key_buf: &mut IndexKeyBuf,
        skips: Skips,
        mut sink: impl FnMut(&[u8]) -> Result<(), IdxMaintRefusal>,
    ) -> Result<(), IdxMaintRefusal> {
        let matches =
            eval(&entry.program, root, limits).map_err(|_| IdxMaintRefusal::EntryFlood)?;
        for steps in matches.iter() {
            let Some(value) = resolve(root, steps) else {
                debug_assert!(false, "eval yielded an unresolvable location path");
                continue;
            };
            let Some(scalar) = scalar_of(value) else {
                if skips == Skips::Counted {
                    entry.counters.skipped_sparse += 1;
                }
                continue;
            };
            match index_key_encode(entry.key_type, scalar, key_buf) {
                Ok(()) => sink(key_buf.as_bytes())?,
                Err(skip) if skips == Skips::Counted => entry.counters.note_skip(skip),
                Err(_) => {}
            }
        }
        Ok(())
    }

    /// Marks *held* every entry of `list` equal to `(ord, key)` — the
    /// whole equal run, because the diff's dedup keeps a run's last and
    /// a mark on one duplicate could be the one it drops — and returns
    /// how many it marked. A run is marked whole, so a held first entry
    /// is a held run: a member that repeats the value costs one probe,
    /// never the run again (N × N marks for N repeats on both sides).
    /// `list` is sorted by `(ord, key)` within one ref.
    fn mark_held(list: &mut [ScratchEntry], bytes: &[u8], ord: u16, key: &[u8]) -> u64 {
        let probe = (ord, key);
        let start = list.partition_point(|e| (e.ord & ORD_MASK, e.key_in(bytes)) < probe);
        let mut marked = 0u64;
        for entry in &mut list[start..] {
            if (entry.ord & ORD_MASK, entry.key_in(bytes)) != probe {
                break;
            }
            if entry.ord & ORD_HELD != 0 && !cfg!(inf_canary_held_remark) {
                break;
            }
            entry.ord |= ORD_HELD;
            marked += 1;
            if cfg!(inf_canary_held_on_one) {
                break;
            }
        }
        marked
    }

    /// Per-store bracket scratch (pre/post entry sets, the write-set
    /// table) plus separate death-side buffers — a death hook may run
    /// *inside* an open bracket (the gate's inline eviction) and must
    /// not clobber the bracket's sets.
    #[derive(Default)]
    struct MaintScratch {
        bytes: Vec<u8>,
        old: Vec<ScratchEntry>,
        new: Vec<ScratchEntry>,
        old_key_bytes: usize,
        new_key_bytes: usize,
        write_keys: Vec<WriteKey>,
        write_key_bytes: Vec<u8>,
        /// True once `write_keys` is sorted by hash (wide sets only).
        write_keys_sorted: bool,
        /// Bit `ord` set ⇒ the prune skipped both evaluations for that
        /// index this bracket (ADR-0139 D6).
        prune_mask: u64,
        /// Bit `ord` set ⇒ the bracket can touch the index: the blast
        /// radius of every degrade that names "the participating set".
        /// Complete at open (ADR-0139 D12).
        participating: u64,
        open: bool,
        key_buf: IndexKeyBuf,
        death_bytes: Vec<u8>,
        death_entries: Vec<ScratchEntry>,
    }

    // The prune / participation / touched masks are `u64` bit sets
    // indexed by attach ordinal: the node cap must fit the word (a bump
    // past 64 would wrap the shifts in release).
    const _: () = assert!(INDEXES_PER_NODE_MAX <= 64, "attach-block bitmasks are u64");

    /// The most scratch a store holds between commands (ADR-0139 D10):
    /// every buffer at its retention bound — the two encoded-key
    /// buffers, the write-set table, the three entry sets.
    pub(crate) const SCRATCH_RETAINED_MAX: u64 =
        (3 * SCRATCH_RETAIN_BYTES + 3 * SCRATCH_RETAIN_ENTRIES * size_of::<ScratchEntry>()) as u64;

    /// Each half of the write-set table's one retention bound: past the
    /// whole, the descriptors and the copied keys shrink to a half each.
    const WRITE_SET_RETAIN_KEYS: usize = SCRATCH_RETAIN_BYTES / 2 / size_of::<WriteKey>();
    const WRITE_SET_RETAIN_KEY_BYTES: usize = SCRATCH_RETAIN_BYTES / 2;

    impl MaintScratch {
        /// Capacity the write-set table holds — descriptors **and** the
        /// copied keys: ADR-0139 D10 bounds their sum, not each.
        fn write_set_retained(&self) -> usize {
            self.write_keys.capacity() * size_of::<WriteKey>() + self.write_key_bytes.capacity()
        }

        /// Capacity held by every scratch buffer (the L5 fold).
        fn retained_bytes(&self) -> u64 {
            let entries = |v: &Vec<ScratchEntry>| (v.capacity() * size_of::<ScratchEntry>()) as u64;
            (self.bytes.capacity() + self.death_bytes.capacity() + self.write_key_bytes.capacity())
                as u64
                + entries(&self.old)
                + entries(&self.new)
                + entries(&self.death_entries)
                + (self.write_keys.capacity() * size_of::<WriteKey>()) as u64
        }

        /// Fills the death-side buffers with `root`'s encoded keys for
        /// `entry`, sorted — one ref per document, so dedup is byte
        /// equality on the run. `Err` = the document cannot be evaluated
        /// whole (match cap, key-bytes cap, scratch growth): it cannot
        /// enter or leave the index whole either, and the caller
        /// degrades the index (ADR-0077 D7).
        fn collect_death_keys(
            &mut self,
            entry: &mut AttachedIndex,
            hash: u64,
            root: DocValue<'_>,
            limits: &EvalLimits,
        ) -> Result<(), IdxMaintRefusal> {
            self.death_bytes.clear();
            self.death_entries.clear();
            let MaintScratch { death_bytes, death_entries, key_buf, .. } = self;
            let mut key_bytes = 0usize;
            eval_keys(entry, root, limits, key_buf, Skips::Counted, |encoded| {
                push_entry(death_bytes, death_entries, &mut key_bytes, encoded, 0, hash)
            })?;
            let bytes = &self.death_bytes;
            self.death_entries.sort_unstable_by(|a, b| a.key_in(bytes).cmp(b.key_in(bytes)));
            Ok(())
        }

        /// Notes one write-set key: the hash and — for a key that can
        /// name a record — its bytes (≤ 255 B, one `memcpy`).
        fn note_write_key(&mut self, hash: u64, key: &[u8]) -> Result<usize, IdxMaintRefusal> {
            let named = key.len() <= MAX_KEY_LEN;
            grow(&mut self.write_keys, 1)?;
            // In range: ≤ 1 × the request's own argv bytes.
            let key_off = self.write_key_bytes.len() as u32;
            if named {
                grow(&mut self.write_key_bytes, key.len())?;
                self.write_key_bytes.extend_from_slice(key);
            }
            let at = self.old.len() as u32;
            self.write_keys.push(WriteKey {
                hash,
                key_off,
                key_len: if named { key.len() as u16 } else { 0 },
                named,
                old_start: at,
                old_end: at,
            });
            Ok(self.write_keys.len() - 1)
        }

        fn write_key_bytes_of(&self, wk: &WriteKey) -> &[u8] {
            &self.write_key_bytes[wk.key_off as usize..wk.key_off as usize + wk.key_len as usize]
        }

        /// Where the write-set keys sharing `hash` start: the whole
        /// table up to `WRITE_SET_LINEAR_MAX` keys (the sort costs more
        /// than it saves there), the sorted run beyond it.
        fn hash_run_start(&self, hash: u64) -> usize {
            if self.write_keys_sorted {
                self.write_keys.partition_point(|wk| wk.hash < hash)
            } else {
                0
            }
        }

        /// Write-set membership by **full key**; the hash is only the
        /// prefilter (ADR-0139 D4).
        fn write_set_hit(&self, hash: u64, key: &[u8]) -> WriteSetHit {
            let mut hit = WriteSetHit::Miss;
            for wk in &self.write_keys[self.hash_run_start(hash)..] {
                if wk.hash != hash {
                    if self.write_keys_sorted {
                        break;
                    }
                    continue;
                }
                if cfg!(inf_canary_cover_by_hash) {
                    return WriteSetHit::Key;
                }
                if wk.named && self.write_key_bytes_of(wk) == key {
                    return WriteSetHit::Key;
                }
                hit = WriteSetHit::HashOnly;
            }
            hit
        }

        /// The gate-death forget (ADR-0139 D4): marks dead **every**
        /// `old` range whose key equals the dying key (`MSET k v k v`
        /// has two). O(the hash run + that key's entries).
        fn forget_old_of(&mut self, hash: u64, key: &[u8]) {
            let start = self.hash_run_start(hash);
            let MaintScratch { write_keys, write_key_bytes, write_keys_sorted, old, .. } = self;
            for wk in &write_keys[start..] {
                if wk.hash != hash {
                    if *write_keys_sorted {
                        break;
                    }
                    continue;
                }
                let bytes = &write_key_bytes[wk.key_off as usize..][..wk.key_len as usize];
                let same_key = wk.named && bytes == key;
                if !same_key && !cfg!(inf_canary_cover_by_hash) {
                    continue;
                }
                for entry in &mut old[wk.old_start as usize..wk.old_end as usize] {
                    entry.ord |= ORD_DEAD;
                }
            }
        }

        /// Bracket close: shrinks any bracket buffer a pathological
        /// document grew past the retention bound (ADR-0139 D10) — a
        /// capacity compare per buffer on the common path, a
        /// reallocation only past it. The caller cleared them first: a
        /// buffer never shrinks below its length.
        fn shrink_bracket(&mut self) {
            shrink_past(&mut self.bytes, SCRATCH_RETAIN_BYTES);
            shrink_past(&mut self.old, SCRATCH_RETAIN_ENTRIES);
            shrink_past(&mut self.new, SCRATCH_RETAIN_ENTRIES);
            // The plant: each half capped at the whole bound, as if the
            // table were two buffers.
            if cfg!(inf_canary_write_set_split_cap) {
                shrink_past(&mut self.write_key_bytes, SCRATCH_RETAIN_BYTES);
                shrink_past(&mut self.write_keys, SCRATCH_RETAIN_ENTRIES);
            } else if self.write_set_retained() > SCRATCH_RETAIN_BYTES {
                shrink_past(&mut self.write_keys, WRITE_SET_RETAIN_KEYS);
                shrink_past(&mut self.write_key_bytes, WRITE_SET_RETAIN_KEY_BYTES);
            }
        }

        /// Death-side close — every exit of the death hook and of a
        /// backfill insert: empties the death buffers, then shrinks
        /// them. It touches nothing else: a hook can run inside an open
        /// bracket (the gate's eviction), whose sets are live and whose
        /// `new`-side reservation is capacity the commit-half owns.
        fn release_death(&mut self) {
            // The plant: a buffer never shrinks below its length.
            if !cfg!(inf_canary_death_scratch_kept) {
                self.death_bytes.clear();
                self.death_entries.clear();
            }
            shrink_past(&mut self.death_bytes, SCRATCH_RETAIN_BYTES);
            shrink_past(&mut self.death_entries, SCRATCH_RETAIN_ENTRIES);
        }
    }

    fn shrink_past<T>(buf: &mut Vec<T>, retain: usize) {
        if buf.capacity() > retain {
            buf.shrink_to(retain);
        }
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
        /// Replay-time maintenance dial (ADR-0139 D7): `None` (boot
        /// default) — replay does not maintain; the no-sidecar path
        /// rebuilds via S05. S06's sidecar load arms `CatchUp`.
        replay: Option<MaintMode>,
        scratch: MaintScratch,
        /// Store-scoped counters (population: this cell's store) — the
        /// events that belong to a death or a bracket, not to one index.
        store_counters: IdxCounters,
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

        /// Coverage for a record dying **anywhere but an eviction entry
        /// point** (ADR-0139 D4): decided once, from the full key and the
        /// prune mask. A void prune clears the mask here, and the
        /// unmasked indexes join the participating set in the same step.
        pub(crate) fn death_hook_wanted(&mut self, hash: u64, key: &[u8]) -> DeathHook {
            if !self.active {
                return DeathHook::Covered;
            }
            if !self.scratch.open {
                return DeathHook::All;
            }
            match self.scratch.write_set_hit(hash, key) {
                WriteSetHit::Key if self.scratch.prune_mask == 0 => DeathHook::Covered,
                WriteSetHit::Key if cfg!(inf_canary_prune_survives_death) => DeathHook::Covered,
                WriteSetHit::Key => {
                    self.store_counters.prune_void += 1;
                    DeathHook::Masked(self.clear_prune_mask())
                }
                WriteSetHit::HashOnly => {
                    self.store_counters.cover_alias += 1;
                    DeathHook::All
                }
                WriteSetHit::Miss => {
                    // ADR-0139 D4's premise: no record outside the write
                    // set dies in a command body. A release violation
                    // still takes the safe row — the hook runs.
                    debug_assert!(false, "a record outside the write set died inside a bracket");
                    DeathHook::All
                }
            }
        }

        /// A record dying at an **eviction entry point** is never covered
        /// (ADR-0139 D4): its hook runs. When its full key is in the open
        /// bracket's write set, the bracket forgets what `old` claimed
        /// for it — the hook is about to remove exactly that — and the
        /// prune is void.
        pub(crate) fn note_uncovered_death(&mut self, hash: u64, key: &[u8]) -> DeathHook {
            if !self.active {
                return DeathHook::Covered;
            }
            if !self.scratch.open {
                return DeathHook::All;
            }
            match self.scratch.write_set_hit(hash, key) {
                WriteSetHit::Key if cfg!(inf_canary_gate_covered) => return DeathHook::Covered,
                WriteSetHit::Key => {
                    self.scratch.forget_old_of(hash, key);
                    self.clear_prune_mask();
                    self.store_counters.gate_forget += 1;
                }
                WriteSetHit::HashOnly => self.store_counters.cover_alias += 1,
                WriteSetHit::Miss => {}
            }
            DeathHook::All
        }

        /// Voids the prune: returns the mask that was set; the unmasked
        /// live indexes join `participating` (ADR-0139 D12 — the
        /// commit-half is about to evaluate them).
        fn clear_prune_mask(&mut self) -> u64 {
            let mask = core::mem::take(&mut self.scratch.prune_mask);
            for (ord, entry) in self.entries.iter().enumerate() {
                if mask & (1 << ord) != 0 && !entry.degraded {
                    self.scratch.participating |= 1 << ord;
                }
            }
            mask
        }

        #[inline]
        pub(crate) fn bracket_open(&self) -> bool {
            self.scratch.open
        }

        pub(crate) fn write_set_len(&self) -> usize {
            self.scratch.write_keys.len()
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
                "attach install is once per id (sync points, ADR-0139 D1)"
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

        /// Arms or disarms replay-time maintenance (ADR-0139 D7).
        pub(crate) fn set_replay_maintenance(&mut self, mode: Option<MaintMode>) {
            self.replay = mode;
        }

        pub(crate) fn replay_mode(&self) -> Option<MaintMode> {
            self.replay
        }

        /// Degrades every live index: the replay-refusal backstop
        /// (recovery cannot refuse a record) and the death hook's `Over`
        /// — a hook has no bracket mask, and the dying document may hold
        /// keys in any index (ADR-0139 D9).
        pub(crate) fn degrade_all_live(&mut self) {
            for entry in &mut self.entries {
                entry.degrade();
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

        /// Discharges a veto raised during boot replay (ADR-0078 A1):
        /// the tree empties and the veto clears — the boot-fresh state
        /// the no-sidecar path has (ADR-0075 D4's rebuild without a
        /// generation bump); the S05 walk starts from zero. `true` iff
        /// the veto was set. Never a live-path repair: a serving cell's
        /// veto clears only through the rebuild edge.
        pub(crate) fn discharge_boot_veto(&mut self, id: IndexId) -> bool {
            let Some(entry) = self.entry_mut(id) else { return false };
            if !entry.degraded {
                return false;
            }
            entry.tree = IndexTree::new(entry.key_type);
            entry.degraded = false;
            entry.converged = false;
            true
        }

        pub(crate) fn counters(&self, id: IndexId) -> Option<IdxCounters> {
            self.entries.iter().find(|e| e.id == id).map(|e| e.counters)
        }

        pub(crate) fn counters_fold(&self) -> IdxCounters {
            let mut total = self.store_counters;
            for entry in &self.entries {
                total.absorb(&entry.counters);
            }
            total
        }

        /// L5 fold of every attached tree (the S03 `idx_*` domains — the
        /// source moved here by ADR-0139 D1, values unchanged).
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

        // ---- the bracket (ADR-0139 step table, per-store halves) ----

        fn begin(&mut self) {
            debug_assert!(!self.scratch.open, "brackets never nest (one command per cell)");
            self.clear_scratch();
        }

        fn clear_scratch(&mut self) {
            let scratch = &mut self.scratch;
            scratch.open = false;
            scratch.bytes.clear();
            scratch.old.clear();
            scratch.new.clear();
            scratch.old_key_bytes = 0;
            scratch.new_key_bytes = 0;
            scratch.write_keys.clear();
            scratch.write_key_bytes.clear();
            scratch.write_keys_sorted = false;
            scratch.prune_mask = 0;
            scratch.participating = 0;
            scratch.shrink_bracket();
            // A bracket never closes inside a death hook, so the death
            // side is already released: the whole bound holds here.
            debug_assert!(scratch.retained_bytes() <= SCRATCH_RETAINED_MAX);
            debug_assert!(scratch.write_set_retained() <= SCRATCH_RETAIN_BYTES);
        }

        /// Capacity the write-set table retains (test probe for D10's
        /// own row; `scratch_bytes` is the aggregate).
        #[cfg(test)]
        pub(crate) fn write_set_retained(&self) -> usize {
            self.scratch.write_set_retained()
        }

        /// Retained scratch capacity — folded into the store's
        /// `doc_scratch_bytes` (ADR-0139 D10: bounded and attributed).
        pub(crate) fn scratch_bytes(&self) -> u64 {
            self.scratch.retained_bytes()
        }

        /// Computes and records the prune mask for a path-scoped
        /// mutation on an existing document (ADR-0139 D6): bit set ⇒
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

        /// The participating set, whole, before anything is evaluated
        /// (ADR-0139 D12): every live, non-degraded, non-pruned index —
        /// whatever the pre-image, so a create that floods index *i*
        /// degrades the indexes after *i* too.
        fn set_participating(&mut self) {
            if cfg!(inf_canary_lazy_participation) {
                return;
            }
            let mut mask = 0u64;
            for (ord, entry) in self.entries.iter().enumerate() {
                if !entry.degraded && self.scratch.prune_mask & (1 << ord) == 0 {
                    mask |= 1 << ord;
                }
            }
            self.scratch.participating = mask;
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
            let MaintScratch {
                bytes,
                old,
                new,
                old_key_bytes,
                new_key_bytes,
                key_buf,
                prune_mask,
                participating,
                ..
            } = scratch;
            let (list, phase_key_bytes) = match phase {
                Phase::Old => (old, old_key_bytes),
                Phase::New => (new, new_key_bytes),
            };
            for (ord, entry) in entries.iter_mut().enumerate() {
                if entry.degraded || *prune_mask & (1 << ord) != 0 {
                    continue;
                }
                if cfg!(inf_canary_lazy_participation) {
                    *participating |= 1 << ord;
                }
                eval_keys(entry, root, &limits, key_buf, Skips::Counted, |encoded| {
                    push_entry(bytes, list, phase_key_bytes, encoded, ord as u16, hash)
                })?;
            }
            Ok(())
        }

        /// Owns, before the mutation, the scratch the commit-half needs
        /// for a key that reaches no publishing funnel (`EXPIRE`,
        /// `PERSIST`, an error reply): such a key re-evaluates to `new ⊆
        /// old`, so `|old|` entries and bytes cover it (ADR-0139 D10).
        fn reserve_new_side(&mut self) -> Result<(), IdxMaintRefusal> {
            let scratch = &mut self.scratch;
            grow(&mut scratch.new, scratch.old.len())?;
            grow(&mut scratch.bytes, scratch.old_key_bytes)
        }

        /// The plan-then-commit reservation (ADR-0072 D7.1): arithmetic
        /// headroom per participating tree, plus the
        /// `idx_reserve_refuse` fault point. Failure ⇒ the mutation is
        /// refused before anything changes.
        fn reserve(&self, max_matches: u32) -> Result<(), IdxMaintRefusal> {
            if fault::fire(crate::fault::IDX_RESERVE_REFUSE) {
                return Err(IdxMaintRefusal::Reserve);
            }
            let per_key = self.scratch.write_keys.len().max(1) as u64;
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

        /// Opens the bracket over the noted write set; a wide set is
        /// sorted by hash once so the per-death membership test is a
        /// binary search (a `DEL` of N keys was N² hash compares). The
        /// key bytes are not permuted — each descriptor carries its span.
        fn open(&mut self) {
            if self.scratch.write_keys.len() > WRITE_SET_LINEAR_MAX {
                self.scratch.write_keys.sort_unstable_by_key(|wk| wk.hash);
                self.scratch.write_keys_sorted = true;
            }
            self.scratch.open = true;
        }

        /// Marks every participating index degraded (the blast radius of
        /// a bracket-level trip — ADR-0139 D12). The document mutation
        /// stands; serving refuses until rebuild.
        fn degrade_participating(&mut self) {
            let mask = self.scratch.participating;
            for (ord, entry) in self.entries.iter_mut().enumerate() {
                if mask & (1 << ord) != 0 {
                    entry.degrade();
                }
            }
        }

        /// The commit-half tail: compact the forgotten entries out, sort
        /// both sets, diff, tree ops. This phase cannot refuse — a tree
        /// refusal, an unfinished alias walk or the planted
        /// `idx_apply_trip` lands in the degraded backstop, never in a
        /// wrong result (ADR-0072 D7.2).
        fn apply(&mut self, mode: MaintMode, ctx: &AliasCtx<'_>, max_matches: u32) {
            debug_assert!(self.scratch.open, "apply without an open bracket");
            if fault::fire(crate::fault::IDX_APPLY_TRIP) {
                self.degrade_participating();
                self.clear_scratch();
                return;
            }
            // Before the sort: the dedup keeps a run's last, so a dead
            // twin left in place could shadow a live one (ADR-0139 D4).
            if !cfg!(inf_canary_forget_in_place) {
                self.scratch.old.retain(|e| e.ord & ORD_DEAD == 0);
            }
            let bytes = core::mem::take(&mut self.scratch.bytes);
            self.scratch.old.sort_unstable_by(|a, b| a.sort_key(&bytes).cmp(&b.sort_key(&bytes)));
            self.scratch.new.sort_unstable_by(|a, b| a.sort_key(&bytes).cmp(&b.sort_key(&bytes)));
            let limits = EvalLimits { max_matches };
            let end = BracketDiff { indexes: self, bytes: &bytes, ctx, limits, mode }.run();
            match end {
                DiffEnd::Applied { touched } => {
                    // Cardinality reconciliation (ADR-0072 D5): tree len
                    // and its attribution agree after every bracket.
                    for (ord, entry) in self.entries.iter().enumerate() {
                        if touched & (1 << ord) != 0 {
                            debug_assert_eq!(entry.tree.len(), entry.tree.memory().entries);
                        }
                    }
                }
                DiffEnd::WalkOver => {
                    self.store_counters.alias_walk_over += 1;
                    self.degrade_participating();
                }
            }
            self.scratch.bytes = bytes;
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
        /// (a partial `ready` is unrepresentable). Inserts never
        /// enumerate an alias group: the tree is a set (ADR-0139 D2).
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
            let inserted = backfill_insert_keys(entry, scratch, hash, root, &limits);
            // Every exit: a refused document's keys are scratch too.
            scratch.release_death();
            if inserted.is_err() {
                entry.degrade();
            }
            inserted
        }

        /// The record-death hook (ADR-0072 D6, ADR-0139 D2 rule 2):
        /// evaluate the dying document's entries and remove each one no
        /// other member of its alias group holds. Infallible — what it
        /// cannot decide degrades. `dying` is still slotted, and is
        /// excluded from the group by address. `mask` selects the
        /// indexes ([`DeathHook`]).
        pub(crate) fn remove_doc_entries(
            &mut self,
            ctx: &AliasCtx<'_>,
            hash: u64,
            dying: inf_alloc::ArenaAddr,
            mask: u64,
            max_matches: u32,
        ) {
            let Some(root) = ctx.doc_root(dying) else { return };
            let (walk, tally) =
                alias_view(ctx.index, hash, |a| a == dying, |a| ctx.inspect(a, hash, |_| false));
            self.store_counters.note_walk(tally.groups);
            let AliasWalk::Complete(view) = walk else {
                self.store_counters.alias_walk_over += 1;
                self.degrade_all_live();
                return;
            };
            if !view.is_empty() {
                self.store_counters.alias_groups += 1;
            }
            let limits = EvalLimits { max_matches };
            let CellIndexes { entries, scratch, .. } = self;
            for (ord, entry) in entries.iter_mut().enumerate() {
                if entry.degraded || mask & (1 << ord) == 0 {
                    continue;
                }
                // A document whose evaluation exceeds the caps cannot
                // have been inserted whole; degrade rather than leak.
                if scratch.collect_death_keys(entry, hash, root, &limits).is_err()
                    || mark_held_by_members(ctx, &view, entry, scratch, &limits).is_err()
                {
                    entry.degrade();
                    continue;
                }
                remove_unheld_death_entries(entry, scratch, &view);
            }
            // Every path that filled the death buffers ends here; the
            // two returns above precede the first fill.
            scratch.release_death();
        }
    }

    /// The insert half of [`CellIndexes::backfill_insert_doc`]; `Err` ⇒
    /// the caller degrades the index. A pre-declaration document whose
    /// matches exceed the cap cannot be indexed whole, and a corpus that
    /// outgrew the tree's structural limits (or the planted trip) cannot
    /// be indexed at all: degrade rather than serve a partial projection
    /// (the death-hook rule, ADR-0077 D7).
    fn backfill_insert_keys(
        entry: &mut AttachedIndex,
        scratch: &mut MaintScratch,
        hash: u64,
        root: DocValue<'_>,
        limits: &EvalLimits,
    ) -> Result<u32, ()> {
        let collected = scratch.collect_death_keys(entry, hash, root, limits).is_ok();
        let headroom_wanted = scratch.death_entries.len() as u64;
        if !collected
            || fault::fire(crate::fault::IDX_BACKFILL_TRIP)
            || !entry.tree.insert_headroom(headroom_wanted)
        {
            return Err(());
        }
        let key_of = |e: &ScratchEntry| e.key_in(&scratch.death_bytes);
        let pk_ref = PkRef::from_key_hash(hash);
        let mut fresh = 0u32;
        let mut previous: Option<&ScratchEntry> = None;
        for e in &scratch.death_entries {
            // One ref for the whole document ⇒ dedup is byte equality on
            // the sorted run (the death-hook pattern).
            if previous.is_some_and(|p| key_of(p) == key_of(e)) {
                continue;
            }
            previous = Some(e);
            // Headroom said an `Err` cannot happen — the backstop.
            if entry.tree.insert(key_of(e), pk_ref).map_err(|_| ())? {
                fresh += 1;
            }
        }
        Ok(fresh)
    }

    /// Marks *held* every death entry some alias member also yields for
    /// `entry`'s program. Allocates nothing of its own: a member's keys
    /// stream through the one key buffer (ADR-0139 D9).
    fn mark_held_by_members(
        ctx: &AliasCtx<'_>,
        view: &AliasView,
        entry: &mut AttachedIndex,
        scratch: &mut MaintScratch,
        limits: &EvalLimits,
    ) -> Result<(), IdxMaintRefusal> {
        let MaintScratch { death_bytes, death_entries, key_buf, .. } = scratch;
        for member in view.members() {
            // A string record holds nothing; an expired-but-unreaped
            // document is physically present, so it holds.
            let Some(root) = ctx.doc_root(member) else { continue };
            let mut marks = 0u64;
            let marked = eval_keys(entry, root, limits, key_buf, Skips::Silent, |encoded| {
                marks += mark_held(death_entries, death_bytes, 0, encoded);
                Ok(())
            });
            entry.counters.alias_held_marks += marks;
            marked?;
        }
        Ok(())
    }

    fn remove_unheld_death_entries(
        entry: &mut AttachedIndex,
        scratch: &MaintScratch,
        view: &AliasView,
    ) {
        let key_of = |e: &ScratchEntry| e.key_in(&scratch.death_bytes);
        let entries = &scratch.death_entries;
        for (i, e) in entries.iter().enumerate() {
            // The ref is one hash for the whole document, so dedup is
            // byte equality on the sorted run: keep a run's last, as the
            // bracket's diff does.
            if entries.get(i + 1).is_some_and(|next| key_of(next) == key_of(e)) {
                continue;
            }
            if e.ord & ORD_HELD != 0 {
                entry.counters.alias_kept += 1;
                continue;
            }
            let found = entry.tree.remove(key_of(e), view.pk_ref(), view);
            entry.counters.maint_removes += 1;
            if entry.converged {
                debug_assert!(found, "Strict: a converged death removal finds its entry");
            }
        }
    }

    /// How the bracket's diff ended.
    enum DiffEnd {
        Applied {
            /// Mask of the indexes whose trees changed.
            touched: u64,
        },
        /// An alias enumeration crossed a budget: the removal question
        /// is unanswered, so the participating indexes degrade
        /// (ADR-0139 D9). Tree ops already applied stay — those trees
        /// are the ones that degrade.
        WalkOver,
    }

    /// The bracket's sorted-set diff (the commit-half core): removes for
    /// `old − new`, inserts for `new − old`, multi-match repeats
    /// collapsed (keep a run's last). Both sets are sorted by `(ref, ord,
    /// key)`, so one ref's entries are contiguous and a single alias view
    /// — enumerated at that ref's first removal, never for an
    /// insert-only bracket — serves the whole run.
    struct BracketDiff<'a, 'c> {
        indexes: &'a mut CellIndexes,
        bytes: &'a [u8],
        ctx: &'a AliasCtx<'c>,
        limits: EvalLimits,
        mode: MaintMode,
    }

    impl BracketDiff<'_, '_> {
        fn run(mut self) -> DiffEnd {
            let (mut oi, mut ni) = (0usize, 0usize);
            let mut touched = 0u64;
            let mut view: Option<AliasView> = None;
            loop {
                let scratch = &self.indexes.scratch;
                let key = |e: &ScratchEntry| e.sort_key(self.bytes);
                // Deduplicate multi-match repeats — `(typed key, pk)`
                // pairs collapse per ref: keep a run's last.
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
                    (None, None) => return DiffEnd::Applied { touched },
                };
                match verdict {
                    core::cmp::Ordering::Less => {
                        if !self.remove_at(oi, &mut view) {
                            return DiffEnd::WalkOver;
                        }
                        touched |= 1 << self.indexes.scratch.old[oi].ordinal();
                        oi += 1;
                    }
                    core::cmp::Ordering::Greater => {
                        touched |= self.insert_at(ni);
                        ni += 1;
                    }
                    core::cmp::Ordering::Equal => {
                        oi += 1;
                        ni += 1;
                    }
                }
            }
        }

        /// Removes `old[oi]` unless an alias holds it. `false` ⇔ the
        /// enumeration ended `Over`.
        fn remove_at(&mut self, oi: usize, view: &mut Option<AliasView>) -> bool {
            let candidate = self.indexes.scratch.old[oi];
            if cfg!(inf_canary_forget_in_place) && candidate.ord & ORD_DEAD != 0 {
                return true;
            }
            let pk_ref = PkRef::from_key_hash(candidate.entry_ref);
            let view: &AliasView = match view {
                Some(current) if current.pk_ref() == pk_ref => current,
                stale => {
                    let Some(fresh) = self.enumerate(candidate.entry_ref) else { return false };
                    stale.insert(fresh)
                }
            };
            // Re-read: the enumeration may have marked this entry held.
            let candidate = self.indexes.scratch.old[oi];
            let entry = &mut self.indexes.entries[candidate.ordinal()];
            if candidate.ord & ORD_HELD != 0 {
                entry.counters.alias_kept += 1;
                return true;
            }
            let found = entry.tree.remove(candidate.key_in(self.bytes), pk_ref, view);
            entry.counters.maint_removes += 1;
            if entry.converged && self.mode == MaintMode::Strict {
                debug_assert!(found, "Strict: a converged remove finds its entry");
            }
            true
        }

        /// Enumerates `G(hash)` minus the write set (excluded by full
        /// key — the diff already merged the write set as a group) and
        /// marks *held* every `old` entry of this ref a member yields.
        /// `None` ⇔ `Over`.
        fn enumerate(&mut self, hash: u64) -> Option<AliasView> {
            let ctx = self.ctx;
            let CellIndexes { entries, scratch, store_counters, .. } = &mut *self.indexes;
            let in_write_set = |key: &[u8]| scratch.write_set_hit(hash, key) == WriteSetHit::Key;
            let (walk, tally) =
                alias_view(ctx.index, hash, |_| false, |a| ctx.inspect(a, hash, in_write_set));
            store_counters.note_walk(tally.groups);
            let AliasWalk::Complete(view) = walk else { return None };
            if view.is_empty() {
                return Some(view);
            }
            store_counters.alias_groups += 1;
            let start = scratch.old.partition_point(|e| e.entry_ref < hash);
            let len = scratch.old[start..].partition_point(|e| e.entry_ref == hash);
            let MaintScratch { old, key_buf, participating, .. } = scratch;
            let run = &mut old[start..start + len];
            for member in view.members() {
                let Some(root) = ctx.doc_root(member) else { continue };
                for (ord, entry) in entries.iter_mut().enumerate() {
                    if entry.degraded || *participating & (1 << ord) == 0 {
                        continue;
                    }
                    let mut marks = 0u64;
                    let marked =
                        eval_keys(entry, root, &self.limits, key_buf, Skips::Silent, |encoded| {
                            marks += mark_held(run, self.bytes, ord as u16, encoded);
                            Ok(())
                        });
                    entry.counters.alias_held_marks += marks;
                    // A member that cannot be evaluated whole cannot
                    // have been inserted whole (ADR-0077 D7).
                    if marked.is_err() {
                        entry.degrade();
                    }
                }
            }
            Some(view)
        }

        /// Inserts `new[ni]`; returns its touched bit. A tree refusal
        /// after the reservation is the backstop, not a control path.
        fn insert_at(&mut self, ni: usize) -> u64 {
            let candidate = self.indexes.scratch.new[ni];
            let entry = &mut self.indexes.entries[candidate.ordinal()];
            let key = candidate.key_in(self.bytes);
            match entry.tree.insert(key, PkRef::from_key_hash(candidate.entry_ref)) {
                Ok(fresh) => {
                    entry.counters.maint_inserts += 1;
                    let strict = entry.converged && self.mode == MaintMode::Strict;
                    // Finding the pair present is legal iff an alias
                    // holds the key (ADR-0139 D2 rule 3).
                    #[cfg(debug_assertions)]
                    if strict && !fresh {
                        debug_assert!(self.alias_holds(candidate), "Strict: an insert is fresh");
                    }
                    let _ = (strict, fresh);
                    1 << candidate.ordinal()
                }
                Err(_) => {
                    entry.degrade();
                    0
                }
            }
        }
    }

    #[cfg(debug_assertions)]
    impl BracketDiff<'_, '_> {
        /// Debug-only: does a member of the ref's alias group, outside
        /// the write set, yield `candidate`'s key for its index? An
        /// unfinished walk is not this check's finding.
        fn alias_holds(&mut self, candidate: ScratchEntry) -> bool {
            let ctx = self.ctx;
            let CellIndexes { entries, scratch, .. } = &mut *self.indexes;
            let hash = candidate.entry_ref;
            let in_write_set = |key: &[u8]| scratch.write_set_hit(hash, key) == WriteSetHit::Key;
            let (walk, _) =
                alias_view(ctx.index, hash, |_| false, |a| ctx.inspect(a, hash, in_write_set));
            let AliasWalk::Complete(view) = walk else { return true };
            let wanted = candidate.key_in(self.bytes);
            let entry = &mut entries[candidate.ordinal()];
            let mut holds = false;
            for member in view.members() {
                let Some(root) = ctx.doc_root(member) else { continue };
                let key_buf = &mut scratch.key_buf;
                let _ = eval_keys(entry, root, &self.limits, key_buf, Skips::Silent, |encoded| {
                    holds |= encoded == wanted;
                    Ok(())
                });
            }
            holds
        }
    }

    /// The ADR-0139 D6 disjointness rule, conservative by construction:
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
        /// The bracket pre-half (ADR-0139 step 1): the write set (hash +
        /// key bytes), the participating set — whole, before anything is
        /// evaluated — the pre-image of every write-set key, the `new`
        /// side's scratch, then the reservation. `Err` ⇒ typed refusal,
        /// nothing changed.
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
            self.idx.begin();
            let outcome = self.idx_collect_pre_images(keys, mutation_path);
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

        fn idx_collect_pre_images(
            &mut self,
            keys: &[&[u8]],
            mutation_path: Option<&PathProgram>,
        ) -> Result<(), IdxMaintRefusal> {
            let max_matches = self.cfg.doc_max_path_matches;
            // The prune applies only to a single-key path mutation whose
            // document exists — creation, deletion, and multi-key
            // commands evaluate in full (ADR-0139 D6). Either way the
            // participating set is whole before the first evaluation
            // (D12); the prunable shape has one key, so its one peek
            // decides the mask first.
            let prune_path = match (keys, mutation_path) {
                ([_], Some(path)) if !path.is_root() => Some(path),
                _ => None,
            };
            if prune_path.is_none() {
                self.idx.set_participating();
            }
            for key in keys {
                let hash = self.hash_key(key);
                let CellStore { arena, index, docs, idx, .. } = self;
                let noted = idx.scratch.note_write_key(hash, key)?;
                let root = peek_doc_root(arena, index, docs, key, hash);
                if let Some(path) = prune_path {
                    if root.is_some() {
                        idx.set_prune_mask(path);
                    }
                    idx.set_participating();
                }
                idx.collect(Phase::Old, hash, root, max_matches)?;
                idx.scratch.write_keys[noted].old_end = idx.scratch.old.len() as u32;
            }
            self.idx.reserve_new_side()?;
            self.idx.reserve(max_matches)
        }

        /// The bracket commit-half (ADR-0139 step 6): post-image
        /// evaluation, dedup diff, tree ops. Runs after the mutation
        /// applied (and, on durable namespaces, staged); cannot refuse —
        /// failures land in the degraded backstop. **A bracket ends only
        /// here** (ADR-0139 D3): there is no abort, so a death the
        /// bracket covered can never be dropped with `old`.
        pub(crate) fn idx_bracket_commit(&mut self, keys: &[&[u8]], mode: MaintMode) {
            if !self.idx.bracket_open() {
                return;
            }
            let max_matches = self.cfg.doc_max_path_matches;
            debug_assert_eq!(keys.len(), self.idx.write_set_len());
            self.idx.debug_assert_participation();
            let mut evaluated = Ok(());
            for key in keys {
                let hash = self.hash_key(key);
                let CellStore { arena, index, docs, idx, .. } = self;
                let root = peek_doc_root(arena, index, docs, key, hash);
                debug_assert!(
                    idx.scratch.prune_mask == 0 || root.is_some(),
                    "a death voids the prune, so a pruned document is alive (ADR-0139 D6)"
                );
                evaluated = idx.collect(Phase::New, hash, root, max_matches);
                if evaluated.is_err() {
                    break;
                }
            }
            if evaluated.is_err() {
                // A post-image that floods, crosses the key-bytes cap or
                // outgrows the pre-half's scratch: the mutation stands,
                // the participating indexes degrade (ADR-0139 D12).
                self.idx.degrade_participating();
                self.idx.clear_scratch();
                return;
            }
            let hasher = self.cfg.hasher;
            let CellStore { arena, index, docs, idx, .. } = self;
            idx.apply(mode, &AliasCtx { arena, index, docs, hasher }, max_matches);
        }

        /// Today's abort, kept only as the planted bug that proves the
        /// `COPY` row has teeth: it drops `old` after a covered death.
        #[cfg(inf_canary_copy_abort_after_death)]
        pub(crate) fn idx_bracket_abort_canary(&mut self) {
            if self.idx.bracket_open() {
                self.idx.clear_scratch();
            }
        }

        /// The replay arm's pre-half (ADR-0072 D4 / ADR-0139 D7):
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

    impl CellIndexes {
        /// ADR-0139 D12 at the commit-half: every index the bracket can
        /// touch is in the participating set.
        fn debug_assert_participation(&self) {
            if cfg!(inf_canary_lazy_participation) {
                return;
            }
            for (ord, entry) in self.entries.iter().enumerate() {
                let expected = !entry.degraded && self.scratch.prune_mask & (1 << ord) == 0;
                debug_assert!(
                    !expected || self.scratch.participating & (1 << ord) != 0,
                    "index ordinal {ord} can be touched but is not participating"
                );
            }
        }
    }

    /// Peek a key's document root **without** read side effects: no
    /// lazy-expiry reap, no access-tracking touch, and expired-but-
    /// unreaped records included — the bracket's pre-image must see the
    /// physical record whose entries are in the trees (ADR-0139 D4).
    fn peek_doc_root<'a>(
        arena: &'a inf_alloc::Arena,
        index: &Index,
        docs: &'a doc::DocStore,
        key: &[u8],
        hash: u64,
    ) -> Option<DocValue<'a>> {
        let addr = index.find(hash, |addr| record_at(arena, addr).key() == key)?;
        let len = record_at(arena, addr).encoded_len();
        doc::doc_root_at(arena, docs, addr, len)
    }
}

#[cfg(all(test, feature = "doc"))]
pub(crate) use imp::programs_disjoint;
#[cfg(feature = "doc")]
pub(crate) use imp::{AliasCtx, CellIndexes, DeathHook};

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
    pub(crate) fn scratch_bytes(&self) -> u64 {
        0
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
    use super::imp::SCRATCH_RETAINED_MAX;
    use super::{MaintMode, programs_disjoint};
    use crate::index_key::IndexKeyType;
    use crate::index_registry::IndexId;
    use crate::store::{CellStore, StoreConfig};
    use inf_doc::PathProgram;
    use inf_doc::path::compile;
    use inf_foundation::time::Nanos;

    const NOW: Nanos = Nanos(1_000_000_000);

    fn program(text: &str) -> PathProgram {
        compile(text.as_bytes()).expect("valid path")
    }

    /// Coverage is by **full key** on both membership paths (ADR-0139
    /// D4): a set past `WRITE_SET_LINEAR_MAX` is sorted by hash at open
    /// and binary-searched; a set within it is scanned. Every noted key
    /// is covered; a key whose hash is in the set while its bytes are not
    /// — a real alias, forced by the collision oracle — is hooked and
    /// counted, on both sides of the crossover.
    #[test]
    fn write_set_coverage_is_by_full_key_across_the_membership_crossover() {
        use super::WRITE_SET_LINEAR_MAX;
        use super::imp::DeathHook;
        use crate::index_key::IndexKeyType;
        use crate::index_registry::IndexId;
        use crate::store::{CellStore, StoreConfig};
        use crate::tiered::shadow::forced_collision_pair;
        let mut store = CellStore::new(StoreConfig::default());
        store.idx.install(IndexId(1), 1, IndexKeyType::I64, program("$.n").as_bytes());
        let (member, alias) = forced_collision_pair(7);
        assert_eq!(store.hash_key(&member), store.hash_key(&alias));
        for n in [1usize, WRITE_SET_LINEAR_MAX, WRITE_SET_LINEAR_MAX + 1, 4 * WRITE_SET_LINEAR_MAX]
        {
            let names: Vec<String> = (1..n).map(|i| format!("k{i}")).collect();
            let mut keys: Vec<&[u8]> = names.iter().map(|k| k.as_bytes()).collect();
            keys.push(&member);
            store.idx_bracket_begin(&keys, None).expect("headroom");
            assert!(store.idx.bracket_open());
            for key in &keys {
                let hash = store.hash_key(key);
                let hook = store.idx.death_hook_wanted(hash, key);
                assert_eq!(hook, DeathHook::Covered, "noted key covered (n = {n})");
            }
            let before = store.idx.counters_fold().cover_alias;
            let hook = store.idx.death_hook_wanted(store.hash_key(&alias), &alias);
            assert_eq!(hook, DeathHook::All, "an alias of a noted key is hooked (n = {n})");
            assert_eq!(store.idx.counters_fold().cover_alias, before + 1);
            store.idx_bracket_commit(&keys, super::MaintMode::Strict);
            assert!(!store.idx.bracket_open(), "a bracket ends in its commit-half");
        }
    }

    /// ADR-0139 D10: a pathological wildcard
    /// document grows the bracket scratch to its match set; the store
    /// shrinks it back at bracket close and attributes what it keeps in
    /// `doc_scratch_bytes` — never an unbounded, invisible retention.
    #[test]
    fn bracket_scratch_is_bounded_in_retention_and_attributed() {
        use super::MaintMode;
        use super::imp::SCRATCH_RETAINED_MAX;
        use crate::index_key::IndexKeyType;
        use crate::index_registry::IndexId;
        use crate::store::{CellStore, StoreConfig};
        use inf_foundation::time::Nanos;
        let mut store = CellStore::new(StoreConfig::default());
        store.idx.install(IndexId(1), 1, IndexKeyType::Utf8, program("$.tags[*]").as_bytes());
        let now = Nanos(1_000_000_000);
        let tags: Vec<String> = (0..20_000).map(|i| format!("\"t{i:05}\"")).collect();
        let json = format!(r#"{{"tags":[{}]}}"#, tags.join(","));
        let idoc = inf_doc::JsonParser::new().parse(json.as_bytes()).expect("valid doc");
        let key: &[u8] = b"doc:big";
        store.idx_bracket_begin(&[key], None).expect("headroom");
        store
            .json_set(
                key,
                &inf_doc::CanonicalDoc::validate(&idoc).expect("canonical fixture"),
                Default::default(),
                now,
            )
            .expect("set");
        store.idx_bracket_commit(&[key], MaintMode::Strict);
        assert_eq!(store.idx.tree(IndexId(1)).map(|t| t.len()), Some(20_000));
        let retained = store.idx.scratch_bytes();
        assert!(retained <= SCRATCH_RETAINED_MAX, "retained {retained} B of bracket scratch");
        assert_eq!(
            store.report().doc_scratch_bytes,
            store.docs.report().scratch_bytes + retained,
            "the index scratch is attributed in doc_scratch_bytes (L5)"
        );
    }

    /// ADR-0139 D10's write-set row bounds the table — descriptors plus
    /// the copied keys — at 64 KiB together. 512 absent 255-byte keys
    /// fit under each half's old cap and kept 77,824 B between them.
    #[test]
    fn write_set_table_retention_is_one_combined_bound() {
        use super::SCRATCH_RETAIN_BYTES;
        let mut store = CellStore::new(StoreConfig::default());
        store.idx.install(IndexId(1), 1, IndexKeyType::I64, program("$.n").as_bytes());
        let names: Vec<Vec<u8>> = (0..512u32).map(|i| format!("{i:0>255}").into_bytes()).collect();
        let keys: Vec<&[u8]> = names.iter().map(Vec::as_slice).collect();
        store.idx_bracket_begin(&keys, None).expect("headroom");
        assert!(
            store.idx.write_set_retained() > SCRATCH_RETAIN_BYTES,
            "the bracket reached past it"
        );
        for key in &keys {
            assert!(!store.del(key, NOW), "absent");
        }
        store.idx_bracket_commit(&keys, MaintMode::Strict);
        let kept = store.idx.write_set_retained();
        assert!(kept <= SCRATCH_RETAIN_BYTES, "write-set table retained {kept} B");
        assert_retained_within_bound(&store, "after a wide DEL");
        // A set inside the bound keeps its capacity: no realloc per bracket.
        store.idx_bracket_begin(&keys[..128], None).expect("headroom");
        store.idx_bracket_commit(&keys[..128], MaintMode::Strict);
        let warm = store.idx.write_set_retained();
        store.idx_bracket_begin(&keys[..128], None).expect("headroom");
        store.idx_bracket_commit(&keys[..128], MaintMode::Strict);
        assert_eq!(store.idx.write_set_retained(), warm);
    }

    const TAGS: IndexId = IndexId(1);
    const BIG: &[u8] = b"doc:big";

    /// A store with a `utf8` wildcard index and one 20,000-tag document
    /// — ≈ 0.5 MiB of scratch for whoever evaluates it, eight times the
    /// retention bound. `indexed`: through a bracket (the tree holds it)
    /// or seeded bare (a pre-declaration document, backfill's input).
    fn big_doc_store(indexed: bool) -> CellStore {
        let mut store = CellStore::new(StoreConfig::default());
        store.idx.install(TAGS, 1, IndexKeyType::Utf8, program("$.tags[*]").as_bytes());
        let tags: Vec<String> = (0..20_000).map(|i| format!("\"t{i:05}\"")).collect();
        let json = format!(r#"{{"tags":[{}]}}"#, tags.join(","));
        let idoc = inf_doc::JsonParser::new().parse(json.as_bytes()).expect("valid doc");
        if indexed {
            store.idx_bracket_begin(&[BIG], None).expect("headroom");
        }
        store
            .json_set(
                BIG,
                &inf_doc::CanonicalDoc::validate(&idoc).expect("canonical fixture"),
                Default::default(),
                NOW,
            )
            .expect("set");
        if indexed {
            store.idx_bracket_commit(&[BIG], MaintMode::Strict);
        }
        store
    }

    fn assert_retained_within_bound(store: &CellStore, what: &str) {
        let retained = store.idx.scratch_bytes();
        assert!(retained <= SCRATCH_RETAINED_MAX, "{what}: retained {retained} B of scratch");
    }

    /// ADR-0139 D10 on the death side: the hook and the backfill insert
    /// release their buffers on **every** exit. Shrinking a buffer that
    /// still holds the document's keys is a no-op — a vector never
    /// shrinks below its length — so one large death pinned its whole
    /// scratch until the next one.
    #[test]
    fn death_scratch_is_released_after_a_large_death_and_a_large_backfill() {
        let mut store = big_doc_store(true);
        assert!(store.del(BIG, NOW), "an unbracketed death runs the hook");
        assert_eq!(store.idx.tree(TAGS).map(|t| t.len()), Some(0));
        assert_retained_within_bound(&store, "death hook");

        let mut store = big_doc_store(false);
        store.idx_backfill_slice(TAGS, 0, u32::MAX, u32::MAX, NOW);
        assert_eq!(store.idx.tree(TAGS).map(|t| t.len()), Some(20_000));
        assert_retained_within_bound(&store, "backfill");
    }

    /// The same bound on the exits that refuse: a planted scratch
    /// refusal at the first, the middle and the **last** growth site of
    /// the evaluation — the last fails with nearly all of the document's
    /// keys buffered — and the planted backfill trip, which fails after
    /// every one of them is.
    #[test]
    fn death_scratch_is_released_when_the_evaluation_is_refused() {
        use inf_foundation::fault::{self, FaultSpec};
        let evaluate = |store: &mut CellStore, backfill: bool| {
            if backfill {
                store.idx_backfill_slice(TAGS, 0, u32::MAX, u32::MAX, NOW);
            } else {
                assert!(store.del(BIG, NOW), "an unbracketed death runs the hook");
            }
        };
        for backfill in [false, true] {
            // Count the growth sites with a plant that never fires.
            let mut store = big_doc_store(!backfill);
            fault::arm(crate::fault::IDX_SCRATCH_REFUSE, FaultSpec::Nth(u64::MAX));
            evaluate(&mut store, backfill);
            let sites = fault::occurrences(crate::fault::IDX_SCRATCH_REFUSE);
            fault::disarm_all();
            assert!(sites >= 20, "≈ 0.5 MiB grown by doubling: {sites} sites");
            for nth in [1, sites / 2, sites] {
                let mut store = big_doc_store(!backfill);
                fault::arm(crate::fault::IDX_SCRATCH_REFUSE, FaultSpec::Nth(nth));
                evaluate(&mut store, backfill);
                let fired = fault::fired(crate::fault::IDX_SCRATCH_REFUSE);
                fault::disarm_all();
                assert_eq!(fired, 1, "site {nth} of {sites}");
                assert_eq!(store.idx.is_degraded(TAGS), Some(true), "a death cannot refuse");
                assert_retained_within_bound(&store, "refused evaluation");
            }
        }
        let mut store = big_doc_store(false);
        fault::arm(crate::fault::IDX_BACKFILL_TRIP, FaultSpec::Always);
        evaluate(&mut store, true);
        fault::disarm_all();
        assert_eq!(store.idx.is_degraded(TAGS), Some(true), "the plant is live");
        assert_retained_within_bound(&store, "backfill trip");
    }

    /// A death hook inside an open bracket (the gate's eviction) closes
    /// the death side **only**: the bracket's `new`-side reservation is
    /// capacity the commit-half owns (ADR-0139 D10). Shrinking it there
    /// made a key that reaches no funnel grow scratch after the pre-half
    /// — here refused, so the index would degrade.
    #[test]
    fn a_death_inside_an_open_bracket_leaves_the_brackets_reservation() {
        use inf_foundation::fault::{self, FaultSpec};
        let mut store = big_doc_store(true);
        let victim: &[u8] = b"doc:victim";
        let idoc = inf_doc::JsonParser::new().parse(br#"{"tags":["v"]}"#).expect("valid doc");
        store.idx_bracket_begin(&[victim], None).expect("headroom");
        store
            .json_set(
                victim,
                &inf_doc::CanonicalDoc::validate(&idoc).expect("canonical fixture"),
                Default::default(),
                NOW,
            )
            .expect("set");
        store.idx_bracket_commit(&[victim], MaintMode::Strict);

        store.idx_bracket_begin(&[BIG], None).expect("the pre-half reserves |old| for `new`");
        let hash = store.hash_key(victim);
        let (addr, len) = store.resolve(victim, NOW).expect("the victim is live");
        store.evict_record(hash, addr, len);
        fault::arm(crate::fault::IDX_SCRATCH_REFUSE, FaultSpec::Always);
        store.idx_bracket_commit(&[BIG], MaintMode::Strict);
        let fired = fault::fired(crate::fault::IDX_SCRATCH_REFUSE);
        fault::disarm_all();
        assert_eq!(fired, 0, "the commit-half re-evaluated into scratch it already owned");
        assert_eq!(store.idx.is_degraded(TAGS), Some(false));
        assert_eq!(store.idx.tree(TAGS).map(|t| t.len()), Some(20_000), "the victim's key left");
        assert_retained_within_bound(&store, "bracket close");
    }

    /// The ADR-0139 D6 rule, case by case: only a proven per-step
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
            // Mixed step kinds stay conservative (ADR-0139 D6).
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
