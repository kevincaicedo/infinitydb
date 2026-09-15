//! Per-cell compiled-program cache (M3-S10; ADR-0041 D1/D2).
//!
//! One instance per cell, shared by every connection and namespace the
//! cell serves: a compiled program is a pure function of the path text
//! (ADR-0040 D2 — mode is the first byte, escapes normalize away), so
//! the raw text IS the mode-qualified key. Compilation cost leaves the
//! hot path here: the MRU entry gets one length/memcmp; other hits pay one
//! fixed-seed hash + chain probe before moving to the front.
//!
//! Bounded and counted (L5): at most `capacity` entries AND a byte
//! budget of `capacity × 4 KiB` (the nominal per-entry share — path
//! caps are config-raisable up to the 64 KiB ceiling, so entry count
//! alone would leave the pool ~128 MiB adversarial). Eviction is LRU;
//! an entry larger than the whole budget is served compiled-but-uncached.
//! `bytes()` is exact (keys + programs + slab + buckets) and surfaces as
//! `doc_path_cache_bytes` (S19 wires the domain rollup).
//!
//! Deterministic by construction (L7): the hash is fixed-seed FNV-1a —
//! `std` `RandomState` is ambient randomness, which both violates the
//! cell denylist's spirit and leaks nondeterminism into eviction order
//! and metrics under DST. Behavior is a pure function of the lookup
//! sequence.

use super::{PATH_BYTES_CEILING, PathError, PathErrorKind, PathProgram, compile_with_max_bytes};

const NIL: u32 = u32::MAX;

/// Nominal per-entry byte share: budget = `capacity ×` this. 4 KiB is
/// the default path-text cap — typical entries (a few dozen bytes) fit
/// thousands of times over; the budget only binds adversarial mixes.
const ENTRY_SHARE_BYTES: usize = 4096;

#[derive(Debug)]
struct Entry {
    hash: u64,
    key: Box<[u8]>,
    program: PathProgram,
    /// Bucket chain link.
    chain: u32,
    /// LRU list links (`prev` toward the head = most recent).
    prev: u32,
    next: u32,
}

impl Entry {
    /// Exact heap bytes this entry pins beyond its slab slot.
    fn heap_bytes(&self) -> usize {
        self.key.len() + self.program.as_bytes().len()
    }
}

/// Bounded LRU of compiled path programs, keyed by raw path text.
///
/// `Default` is the plan's per-cell default: 1024 entries (M3 §5 S10;
/// `doc-path-cache-size` re-sizes it at node assembly).
#[derive(Debug)]
pub struct ProgramCache {
    buckets: Box<[u32]>,
    slab: Vec<Entry>,
    lru_head: u32,
    lru_tail: u32,
    capacity: u32,
    budget_bytes: usize,
    entry_bytes: usize,
    hits: u64,
    misses: u64,
    evictions: u64,
    /// Holds the compiled program when caching is off (capacity 0) or
    /// the entry exceeds the whole budget — the borrow the caller gets
    /// must live somewhere.
    uncached: Option<PathProgram>,
}

/// The plan's per-cell entry-cap default (M3 §5 S10).
pub const PROGRAM_CACHE_DEFAULT_ENTRIES: usize = 1024;

impl Default for ProgramCache {
    fn default() -> ProgramCache {
        ProgramCache::new(PROGRAM_CACHE_DEFAULT_ENTRIES)
    }
}

impl ProgramCache {
    /// `capacity` in entries; 0 disables caching (every lookup compiles).
    pub fn new(capacity: usize) -> ProgramCache {
        let capacity = capacity.min(NIL as usize - 1) as u32;
        let bucket_count = (capacity as usize * 2).next_power_of_two().max(1);
        ProgramCache {
            buckets: vec![NIL; if capacity == 0 { 1 } else { bucket_count }].into_boxed_slice(),
            slab: Vec::new(),
            lru_head: NIL,
            lru_tail: NIL,
            capacity,
            budget_bytes: capacity as usize * ENTRY_SHARE_BYTES,
            entry_bytes: 0,
            hits: 0,
            misses: 0,
            evictions: 0,
            uncached: None,
        }
    }

    /// Look up `text`, compiling and inserting on miss. `max_bytes` is
    /// the namespace's resolved path-text cap, enforced **before** the
    /// lookup so a lower-capped namespace can never be served an
    /// over-cap cached program (ADR-0041 D1).
    pub fn get_or_compile(
        &mut self,
        text: &[u8],
        max_bytes: usize,
    ) -> Result<&PathProgram, PathError> {
        if text.len() > max_bytes.min(PATH_BYTES_CEILING) {
            return Err(PathError { offset: 0, kind: PathErrorKind::PathTooLong });
        }
        if self.capacity == 0 {
            self.misses += 1;
            let program = compile_with_max_bytes(text, max_bytes)?;
            return Ok(self.uncached.insert(program));
        }
        let hot = self.lru_head;
        if hot != NIL && *self.slab[hot as usize].key == *text {
            self.hits += 1;
            return Ok(&self.slab[hot as usize].program);
        }
        let hash = fnv1a(text);
        if let Some(slot) = self.find(hash, text) {
            self.hits += 1;
            self.touch(slot);
            return Ok(&self.slab[slot as usize].program);
        }
        self.misses += 1;
        let program = compile_with_max_bytes(text, max_bytes)?;
        let entry_heap = text.len() + program.as_bytes().len();
        if entry_heap > self.budget_bytes {
            return Ok(self.uncached.insert(program));
        }
        let slot = self.insert(hash, text, program, entry_heap);
        Ok(&self.slab[slot as usize].program)
    }

    /// Exact resident bytes: slab slots + entry heap + bucket table
    /// (`doc_path_cache_bytes`).
    pub fn bytes(&self) -> usize {
        self.slab.capacity() * size_of::<Entry>()
            + self.entry_bytes
            + self.buckets.len() * size_of::<u32>()
            + self.uncached.as_ref().map_or(0, |program| program.as_bytes().len())
    }

    pub fn len(&self) -> usize {
        self.slab.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slab.is_empty()
    }

    pub fn hits(&self) -> u64 {
        self.hits
    }

    pub fn misses(&self) -> u64 {
        self.misses
    }

    pub fn evictions(&self) -> u64 {
        self.evictions
    }

    // ---- internals ----

    fn bucket_of(&self, hash: u64) -> usize {
        (hash & (self.buckets.len() as u64 - 1)) as usize
    }

    fn find(&self, hash: u64, text: &[u8]) -> Option<u32> {
        let mut slot = self.buckets[self.bucket_of(hash)];
        while slot != NIL {
            let entry = &self.slab[slot as usize];
            if entry.hash == hash && *entry.key == *text {
                return Some(slot);
            }
            slot = entry.chain;
        }
        None
    }

    /// Move `slot` to the LRU head (most recent).
    fn touch(&mut self, slot: u32) {
        if self.lru_head == slot {
            return;
        }
        self.unlink_lru(slot);
        self.link_front(slot);
    }

    fn unlink_lru(&mut self, slot: u32) {
        let (prev, next) = {
            let e = &mut self.slab[slot as usize];
            let links = (e.prev, e.next);
            // An unlinked slot carries no links a later splice could read.
            (e.prev, e.next) = (NIL, NIL);
            links
        };
        if prev != NIL {
            self.slab[prev as usize].next = next;
        } else {
            self.lru_head = next;
        }
        if next != NIL {
            self.slab[next as usize].prev = prev;
        } else {
            self.lru_tail = prev;
        }
    }

    fn link_front(&mut self, slot: u32) {
        let old_head = self.lru_head;
        {
            let e = &mut self.slab[slot as usize];
            e.prev = NIL;
            e.next = old_head;
        }
        if old_head != NIL {
            self.slab[old_head as usize].prev = slot;
        }
        self.lru_head = slot;
        if self.lru_tail == NIL {
            self.lru_tail = slot;
        }
    }

    /// Insert a fresh entry, evicting from the LRU tail until both the
    /// entry cap and the byte budget hold. Returns the slab slot.
    fn insert(&mut self, hash: u64, text: &[u8], program: PathProgram, entry_heap: usize) -> u32 {
        debug_assert!(entry_heap <= self.budget_bytes, "oversize entries stay uncached");
        // Every victim leaves the slab (F-L09-01's class): unlinking alone
        // kept the slot resident — unreachable, uncounted, with stale links.
        while self.slab.len() as u32 >= self.capacity
            || (self.entry_bytes + entry_heap > self.budget_bytes && !self.slab.is_empty())
        {
            let victim = self.lru_tail;
            debug_assert_ne!(victim, NIL, "eviction requires a resident entry");
            self.remove(victim);
            self.evictions += 1;
        }
        self.slab.push(Entry { hash, key: text.into(), program, chain: NIL, prev: NIL, next: NIL });
        let slot = (self.slab.len() - 1) as u32;
        self.entry_bytes += entry_heap;
        let bucket = self.bucket_of(hash);
        self.slab[slot as usize].chain = self.buckets[bucket];
        self.buckets[bucket] = slot;
        self.link_front(slot);
        slot
    }

    /// Drop `slot` entirely: swap-remove keeps the slab dense, so the
    /// displaced tail entry's links re-target.
    fn remove(&mut self, slot: u32) {
        self.unlink_lru(slot);
        self.unlink_chain(slot);
        self.entry_bytes -= self.slab[slot as usize].heap_bytes();
        let last = (self.slab.len() - 1) as u32;
        self.slab.swap_remove(slot as usize);
        if slot != last {
            self.retarget(last, slot);
        }
    }

    /// Every link that pointed at `from` now points at `to` (the
    /// swap-remove fixup).
    fn retarget(&mut self, from: u32, to: u32) {
        let (hash, prev, next) = {
            let e = &self.slab[to as usize];
            (e.hash, e.prev, e.next)
        };
        if prev != NIL {
            self.slab[prev as usize].next = to;
        } else if self.lru_head == from {
            self.lru_head = to;
        }
        if next != NIL {
            self.slab[next as usize].prev = to;
        } else if self.lru_tail == from {
            self.lru_tail = to;
        }
        let bucket = self.bucket_of(hash);
        if self.buckets[bucket] == from {
            self.buckets[bucket] = to;
        } else {
            let mut cursor = self.buckets[bucket];
            while cursor != NIL {
                if self.slab[cursor as usize].chain == from {
                    self.slab[cursor as usize].chain = to;
                    break;
                }
                cursor = self.slab[cursor as usize].chain;
            }
        }
    }

    fn unlink_chain(&mut self, slot: u32) {
        let (hash, next) = {
            let e = &self.slab[slot as usize];
            (e.hash, e.chain)
        };
        let bucket = self.bucket_of(hash);
        let mut cursor = self.buckets[bucket];
        if cursor == slot {
            self.buckets[bucket] = next;
            return;
        }
        while cursor != NIL {
            let cursor_next = self.slab[cursor as usize].chain;
            if cursor_next == slot {
                self.slab[cursor as usize].chain = next;
                return;
            }
            cursor = cursor_next;
        }
        unreachable!("resident entries are chained");
    }
}

/// Fixed-seed FNV-1a (deterministic — module docs). Paths are tens of
/// bytes; byte-at-a-time is inside the hit path's budget.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hit_returns_the_cached_program_and_counts() {
        let mut cache = ProgramCache::new(8);
        let a = cache.get_or_compile(b"$.a.b", 4096).expect("compiles").clone();
        assert_eq!((cache.hits(), cache.misses()), (0, 1));
        let b = cache.get_or_compile(b"$.a.b", 4096).expect("hits").clone();
        assert_eq!((cache.hits(), cache.misses()), (1, 1));
        assert_eq!(a, b);
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn distinct_texts_are_distinct_keys_even_when_programs_agree() {
        // `['a']` and `.a` compile to identical programs (escape
        // insensitivity, ADR-0040 D2) but cache under their own texts.
        let mut cache = ProgramCache::new(8);
        let bracket = cache.get_or_compile(b"$['a']", 4096).expect("compiles").clone();
        let dot = cache.get_or_compile(b"$.a", 4096).expect("compiles").clone();
        assert_eq!(bracket, dot);
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn legacy_and_dollar_mode_share_the_key_space() {
        let mut cache = ProgramCache::new(8);
        let legacy = cache.get_or_compile(b".a", 4096).expect("compiles").clone();
        let dollar = cache.get_or_compile(b"$.a", 4096).expect("compiles").clone();
        assert!(legacy.is_legacy());
        assert!(!dollar.is_legacy());
        assert_eq!(cache.len(), 2, "mode rides the text bytes — no collision");
    }

    #[test]
    fn lru_evicts_the_coldest_entry() {
        let mut cache = ProgramCache::new(2);
        cache.get_or_compile(b"$.a", 4096).expect("compiles");
        cache.get_or_compile(b"$.b", 4096).expect("compiles");
        // Touch $.a so $.b is the tail.
        cache.get_or_compile(b"$.a", 4096).expect("hits");
        cache.get_or_compile(b"$.c", 4096).expect("compiles, evicting the tail ($.b)");
        assert_eq!(cache.evictions(), 1);
        assert_eq!(cache.len(), 2);
        let miss_before = cache.misses();
        cache.get_or_compile(b"$.b", 4096).expect("recompiles");
        assert_eq!(cache.misses(), miss_before + 1, "$.b was the evicted tail");
        // Recency now reads {$.b, $.c}: re-inserting $.b evicted $.a
        // (touched at step 3, but colder than the step-4 $.c insert).
        let hits_before = cache.hits();
        cache.get_or_compile(b"$.c", 4096).expect("still cached");
        assert_eq!(cache.hits(), hits_before + 1);
        let miss_before = cache.misses();
        cache.get_or_compile(b"$.a", 4096).expect("recompiles");
        assert_eq!(cache.misses(), miss_before + 1, "$.a fell out on the $.b re-insert");
    }

    #[test]
    fn compile_failures_are_not_cached() {
        let mut cache = ProgramCache::new(8);
        assert!(cache.get_or_compile(b"$..", 4096).is_err());
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.misses(), 1);
    }

    #[test]
    fn namespace_cap_binds_before_the_lookup() {
        let mut cache = ProgramCache::new(8);
        cache.get_or_compile(b"$.abcdef", 4096).expect("compiles under the wide cap");
        let err = cache.get_or_compile(b"$.abcdef", 4).expect_err("lower cap refuses");
        assert_eq!(err.kind, PathErrorKind::PathTooLong);
        assert_eq!(cache.hits(), 0, "the capped lookup never reached the table");
    }

    #[test]
    fn capacity_zero_disables_but_still_serves() {
        let mut cache = ProgramCache::new(0);
        let p = cache.get_or_compile(b"$.a", 4096).expect("compiles").clone();
        assert!(!p.is_legacy());
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.misses(), 1);
        cache.get_or_compile(b"$.a", 4096).expect("compiles again");
        assert_eq!(cache.misses(), 2, "no residency without capacity");
    }

    #[test]
    fn byte_accounting_is_exact_across_churn() {
        let mut cache = ProgramCache::new(4);
        for i in 0..64u32 {
            let text = format!("$.key{i}");
            cache.get_or_compile(text.as_bytes(), 4096).expect("compiles");
            let expected: usize =
                (0..cache.len()).map(|s| cache.slab[s].heap_bytes()).sum::<usize>()
                    + cache.slab.capacity() * size_of::<Entry>()
                    + cache.buckets.len() * size_of::<u32>();
            assert_eq!(cache.bytes(), expected);
        }
        assert_eq!(cache.len(), 4);
        assert_eq!(cache.evictions(), 60);
    }

    /// The structural invariants the slab/LRU mechanics promise: every
    /// slab slot is chained from its bucket, the LRU list threads exactly
    /// the slab, the byte ledger is the sum of the resident entries.
    fn check_invariants(cache: &ProgramCache) {
        let n = cache.slab.len();
        assert!(n <= cache.capacity as usize, "slab {n} over capacity {}", cache.capacity);
        let mut heap = 0usize;
        for (s, e) in cache.slab.iter().enumerate() {
            assert_eq!(
                cache.find(e.hash, &e.key),
                Some(s as u32),
                "slot {s} is orphaned from its bucket chain"
            );
            heap += e.heap_bytes();
        }
        assert_eq!(cache.entry_bytes, heap, "entry_bytes drifted from the resident entries");
        assert!(cache.entry_bytes <= cache.budget_bytes, "budget exceeded");
        let (mut seen, mut prev, mut cur) = (0usize, NIL, cache.lru_head);
        while cur != NIL {
            let e = &cache.slab[cur as usize];
            assert_eq!(e.prev, prev, "LRU prev link at slot {cur}");
            prev = cur;
            cur = e.next;
            seen += 1;
            assert!(seen <= n, "LRU list cycles");
        }
        assert_eq!(prev, cache.lru_tail, "LRU tail");
        assert_eq!(seen, n, "the LRU list threads {seen} of {n} slots");
        let expected = heap
            + cache.slab.capacity() * size_of::<Entry>()
            + cache.buckets.len() * size_of::<u32>()
            + cache.uncached.as_ref().map_or(0, |program| program.as_bytes().len());
        assert_eq!(cache.bytes(), expected, "bytes() is not exact");
    }

    /// A path whose last member name pads the text to `len` bytes.
    fn padded(i: usize, len: usize) -> Vec<u8> {
        let mut s = format!("$.k{i}").into_bytes();
        if len > s.len() + 1 {
            s.push(b'.');
            s.resize(len, b'a');
        }
        s
    }

    const NS_CAP: usize = 4096;

    /// F-L09-01's class in the M3 cache (the S09 statement cache copied
    /// this loop): an insert whose byte budget needs a second victim
    /// unlinked both but reused one slot — the other stayed in the slab,
    /// unreachable, uncounted, with stale LRU links.
    #[test]
    fn multi_victim_eviction_does_not_leak_slots() {
        let mut cache = ProgramCache::new(4); // budget = 4 × 4096
        cache.get_or_compile(&padded(0, 0), NS_CAP).expect("ok");
        // A 2 560 B path compiles to a program of about its own size:
        // three such entries and the tiny one fit, a fourth does not.
        let big: Vec<Vec<u8>> = (1..=4).map(|i| padded(i, NS_CAP * 5 / 8)).collect();
        for p in &big[..3] {
            cache.get_or_compile(p, NS_CAP).expect("ok");
        }
        assert_eq!((cache.len(), cache.evictions()), (4, 0));
        // Evicting the tiny tail frees ~20 B: the fourth big entry takes
        // a second victim.
        cache.get_or_compile(&big[3], NS_CAP).expect("ok");
        assert_eq!(cache.evictions(), 2);
        assert_eq!(cache.len(), 3, "two victims out, one entry in");
        check_invariants(&cache);
    }

    /// The same shape driven by a generator: mixed sizes and hits —
    /// every step keeps the structure.
    #[test]
    fn eviction_keeps_the_structure() {
        use std::cell::Cell;

        use proptest::prelude::*;
        let pad = prop_oneof![Just(0usize), (NS_CAP - 128)..=NS_CAP];
        let op = (0usize..12, pad);
        let multi_victim = Cell::new(0u64);
        proptest!(
            ProptestConfig::with_cases(512),
            |(capacity in 1usize..6, ops in proptest::collection::vec(op, 1..64))| {
            let mut cache = ProgramCache::new(capacity);
            for (idx, pad) in ops {
                let before = cache.evictions();
                cache.get_or_compile(&padded(idx, pad), NS_CAP).expect("ok");
                if cache.evictions() >= before + 2 {
                    multi_victim.set(multi_victim.get() + 1);
                }
                check_invariants(&cache);
            }
        });
        assert!(multi_victim.get() > 0, "the generator never took two victims in one insert");
    }
}
