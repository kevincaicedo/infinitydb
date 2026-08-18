//! Per-cell compiled-statement cache (M4.5-S09, ADR-0080 D5) — the
//! M3-S10 `ProgramCache` shape: bounded LRU keyed by raw statement
//! text, fixed-seed FNV-1a (deterministic under DST — ambient hashing
//! randomness is the L7 violation the M3 cache already names), entry
//! cap × byte budget, exact `bytes()`.
//!
//! Compilation reads the catalog, so residency alone cannot prove a
//! hit is still right: every entry records the catalog epoch it
//! compiled under, and an epoch mismatch is an `invalidations`-counted
//! miss that recompiles — resolution stays a pure function of
//! (statement text, catalog) instead of drifting with cache residency
//! (L7). Rejections are not cached (client errors; the M3 rule).

use std::rc::Rc;

use super::{
    CatalogView, CompiledStatement, QlError, STATEMENT_BYTES_CEILING, compile_with_max_bytes,
};

const NIL: u32 = u32::MAX;

/// Nominal per-entry byte share: budget = `capacity ×` this. The
/// statement cap is the honest share — typical statements (tens of
/// bytes) fit thousands of times over; the budget binds adversarial
/// mixes only.
const ENTRY_SHARE_BYTES: usize = STATEMENT_BYTES_CEILING;

/// The per-cell entry-cap default (the M3-S10 value; node assembly
/// re-sizes it).
pub const STATEMENT_CACHE_DEFAULT_ENTRIES: usize = 1024;

struct Entry {
    hash: u64,
    key: Box<[u8]>,
    compiled: Rc<CompiledStatement>,
    /// The catalog epoch this entry compiled under (ADR-0080 D5).
    epoch: u64,
    chain: u32,
    prev: u32,
    next: u32,
}

impl Entry {
    /// Heap bytes this entry pins beyond its slab slot: key text +
    /// serialized program (the dominant owned allocations; the decoded
    /// `Access`/VM views are proportional to it).
    fn heap_bytes(&self) -> usize {
        self.key.len() + self.compiled.program.as_bytes().len()
    }
}

/// Bounded LRU of compiled statements, keyed by raw statement text.
pub struct StatementCache {
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
    invalidations: u64,
}

impl Default for StatementCache {
    fn default() -> StatementCache {
        StatementCache::new(STATEMENT_CACHE_DEFAULT_ENTRIES)
    }
}

impl StatementCache {
    /// `capacity` in entries; 0 disables caching (every lookup
    /// compiles).
    pub fn new(capacity: usize) -> StatementCache {
        let capacity = capacity.min(NIL as usize - 1) as u32;
        let bucket_count = (capacity as usize * 2).next_power_of_two().max(1);
        StatementCache {
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
            invalidations: 0,
        }
    }

    /// Look up `text`, compiling and inserting on miss. A resident
    /// entry whose catalog epoch is stale is invalidated and recompiled
    /// (which may now produce a different program *or a rejection*).
    ///
    /// # Errors
    /// The compiler's documented rejection — never cached.
    pub fn get_or_compile<C: CatalogView>(
        &mut self,
        text: &[u8],
        catalog: &C,
        max_bytes: usize,
    ) -> Result<Rc<CompiledStatement>, QlError> {
        let epoch = catalog.catalog_epoch();
        if self.capacity == 0 {
            self.misses += 1;
            return Ok(Rc::new(compile_with_max_bytes(text, catalog, max_bytes)?));
        }
        let hash = fnv1a(text);
        if let Some(slot) = self.find(hash, text) {
            if self.slab[slot as usize].epoch == epoch {
                self.hits += 1;
                self.touch(slot);
                return Ok(Rc::clone(&self.slab[slot as usize].compiled));
            }
            self.invalidations += 1;
            self.remove(slot);
        }
        self.misses += 1;
        let compiled = Rc::new(compile_with_max_bytes(text, catalog, max_bytes)?);
        let entry_heap = text.len() + compiled.program.as_bytes().len();
        if entry_heap <= self.budget_bytes {
            self.insert(hash, text, Rc::clone(&compiled), epoch, entry_heap);
        }
        Ok(compiled)
    }

    /// Exact resident bytes: slab slots + entry heap + bucket table.
    pub fn bytes(&self) -> usize {
        self.slab.capacity() * size_of::<Entry>()
            + self.entry_bytes
            + self.buckets.len() * size_of::<u32>()
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

    /// Epoch-stale entries dropped on lookup (a DDL happened) — the
    /// S12 `query_*` counter family renders all four.
    pub fn invalidations(&self) -> u64 {
        self.invalidations
    }

    // ---- internals (the M3-S10 slab/LRU mechanics) ----

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

    fn touch(&mut self, slot: u32) {
        if self.lru_head == slot {
            return;
        }
        self.unlink_lru(slot);
        self.link_front(slot);
    }

    /// Drop `slot` entirely (epoch invalidation): swap-remove keeps the
    /// slab dense, so the displaced tail entry's links re-target.
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

    fn unlink_lru(&mut self, slot: u32) {
        let (prev, next) = {
            let e = &self.slab[slot as usize];
            (e.prev, e.next)
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

    fn insert(
        &mut self,
        hash: u64,
        text: &[u8],
        compiled: Rc<CompiledStatement>,
        epoch: u64,
        entry_heap: usize,
    ) {
        debug_assert!(entry_heap <= self.budget_bytes, "oversize entries stay uncached");
        let mut reuse: Option<u32> = None;
        while self.slab.len() as u32 >= self.capacity
            || (self.entry_bytes + entry_heap > self.budget_bytes && !self.slab.is_empty())
        {
            let victim = self.lru_tail;
            debug_assert_ne!(victim, NIL, "eviction requires a resident entry");
            self.unlink_lru(victim);
            self.unlink_chain(victim);
            self.entry_bytes -= self.slab[victim as usize].heap_bytes();
            self.evictions += 1;
            reuse = Some(victim);
            if self.slab.len() as u32 <= self.capacity
                && self.entry_bytes + entry_heap <= self.budget_bytes
            {
                break;
            }
        }
        let entry =
            Entry { hash, key: text.into(), compiled, epoch, chain: NIL, prev: NIL, next: NIL };
        let slot = match reuse {
            Some(slot) => {
                self.slab[slot as usize] = entry;
                slot
            }
            None => {
                self.slab.push(entry);
                (self.slab.len() - 1) as u32
            }
        };
        self.entry_bytes += entry_heap;
        let bucket = self.bucket_of(hash);
        self.slab[slot as usize].chain = self.buckets[bucket];
        self.buckets[bucket] = slot;
        self.link_front(slot);
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

/// Fixed-seed FNV-1a (deterministic — module docs). Statements are
/// tens of bytes; byte-at-a-time sits inside the hit path's budget.
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
    use std::cell::Cell;

    use inf_doc::path;
    use inf_store::{IndexId, IndexKeyType, IndexSpec, IndexState, NsId};

    use super::*;
    use crate::access::AccessStep;
    use crate::partiql::QlErrorKind;

    /// A catalog whose epoch and index liveness the test mutates —
    /// the DDL stand-in.
    struct TestCatalog {
        spec: IndexSpec,
        epoch: Cell<u64>,
        dropped: Cell<bool>,
    }

    impl TestCatalog {
        fn new() -> TestCatalog {
            TestCatalog {
                spec: IndexSpec {
                    id: IndexId(1),
                    generation: 1,
                    ns: NsId(1),
                    name: b"idx".to_vec(),
                    program: path::compile(b"$.v").expect("path").as_bytes().to_vec(),
                    key_type: IndexKeyType::I64,
                    state: IndexState::Ready,
                },
                epoch: Cell::new(1),
                dropped: Cell::new(false),
            }
        }

        fn drop_index(&self) {
            self.dropped.set(true);
            self.epoch.set(self.epoch.get() + 1);
        }
    }

    impl CatalogView for TestCatalog {
        fn resolve_ns(&self, name: &[u8]) -> Option<NsId> {
            (name == b"ns").then_some(NsId(1))
        }

        fn index_by_name(&self, ns: NsId, name: &[u8]) -> Option<&IndexSpec> {
            (!self.dropped.get() && ns == NsId(1) && name == b"idx").then_some(&self.spec)
        }

        fn indexes(&self, ns: NsId) -> impl Iterator<Item = &IndexSpec> {
            (!self.dropped.get() && ns == NsId(1)).then_some(&self.spec).into_iter()
        }

        fn catalog_epoch(&self) -> u64 {
            self.epoch.get()
        }
    }

    const CAP: usize = STATEMENT_BYTES_CEILING;

    #[test]
    fn hit_returns_the_cached_statement_and_counts() {
        let catalog = TestCatalog::new();
        let mut cache = StatementCache::new(8);
        let a = cache.get_or_compile(b"SELECT * FROM ns WHERE v = 1", &catalog, CAP).expect("ok");
        assert_eq!((cache.hits(), cache.misses()), (0, 1));
        let b = cache.get_or_compile(b"SELECT * FROM ns WHERE v = 1", &catalog, CAP).expect("ok");
        assert_eq!((cache.hits(), cache.misses()), (1, 1));
        assert!(Rc::ptr_eq(&a, &b), "a hit serves the resident compilation");
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn spelling_variants_are_distinct_keys() {
        let catalog = TestCatalog::new();
        let mut cache = StatementCache::new(8);
        let a = cache.get_or_compile(b"SELECT * FROM ns WHERE v = 1", &catalog, CAP).expect("ok");
        let b = cache.get_or_compile(b"select * from ns where v = 1", &catalog, CAP).expect("ok");
        assert_eq!(a.program.as_bytes(), b.program.as_bytes(), "same compilation");
        assert_eq!(cache.len(), 2, "text is the key (the M3 posture)");
    }

    /// The ADR-0080 D5 point: a DDL that changes what a statement
    /// compiles to must not be masked by residency — here the resident
    /// program becomes a rejection after the drop.
    #[test]
    fn epoch_invalidation_recompiles_and_can_reject() {
        let catalog = TestCatalog::new();
        let mut cache = StatementCache::new(8);
        let compiled =
            cache.get_or_compile(b"SELECT * FROM ns WHERE v = 1", &catalog, CAP).expect("ok");
        assert!(matches!(compiled.access.step, AccessStep::IndexRange { .. }));
        catalog.drop_index();
        let err = cache
            .get_or_compile(b"SELECT * FROM ns WHERE v = 1", &catalog, CAP)
            .expect_err("the index is gone; resolution now rejects");
        assert_eq!(err.kind, QlErrorKind::NoAccessPath);
        assert_eq!(cache.invalidations(), 1);
        assert_eq!(cache.len(), 0, "the stale entry is gone and the rejection is not cached");
        assert_eq!((cache.hits(), cache.misses()), (0, 2));
    }

    #[test]
    fn rejections_are_never_cached() {
        let catalog = TestCatalog::new();
        let mut cache = StatementCache::new(8);
        for _ in 0..2 {
            cache
                .get_or_compile(b"SELECT * FROM ns ORDER BY v", &catalog, CAP)
                .expect_err("documented rejection");
        }
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.misses(), 2);
    }

    #[test]
    fn lru_evicts_the_coldest_entry() {
        let catalog = TestCatalog::new();
        let mut cache = StatementCache::new(2);
        let s1 = b"SELECT * FROM ns WHERE v = 1".as_slice();
        let s2 = b"SELECT * FROM ns WHERE v = 2".as_slice();
        let s3 = b"SELECT * FROM ns WHERE v = 3".as_slice();
        cache.get_or_compile(s1, &catalog, CAP).expect("ok");
        cache.get_or_compile(s2, &catalog, CAP).expect("ok");
        cache.get_or_compile(s1, &catalog, CAP).expect("hit");
        cache.get_or_compile(s3, &catalog, CAP).expect("ok, evicting s2");
        assert_eq!(cache.evictions(), 1);
        let misses = cache.misses();
        cache.get_or_compile(s2, &catalog, CAP).expect("recompiles");
        assert_eq!(cache.misses(), misses + 1, "s2 was the evicted tail");
    }

    /// Invalidation removes via swap-remove; the displaced entry's
    /// bucket chain and LRU links must survive (the retarget path).
    #[test]
    fn invalidation_keeps_the_table_consistent() {
        let catalog = TestCatalog::new();
        let mut cache = StatementCache::new(8);
        let statements: Vec<Vec<u8>> =
            (0..5).map(|i| format!("SELECT * FROM ns WHERE v = {i}").into_bytes()).collect();
        for s in &statements {
            cache.get_or_compile(s, &catalog, CAP).expect("ok");
        }
        assert_eq!(cache.len(), 5);
        catalog.epoch.set(2); // pure epoch bump — same compilation result
        cache.get_or_compile(&statements[2], &catalog, CAP).expect("recompiles under epoch 2");
        assert_eq!(cache.invalidations(), 1);
        assert_eq!(cache.len(), 5, "removed then re-inserted");
        // Every other entry is still resident and findable (each will
        // invalidate once under the new epoch, then hit).
        for s in &statements {
            cache.get_or_compile(s, &catalog, CAP).expect("ok");
        }
        assert_eq!(cache.invalidations(), 5, "the four stale entries invalidated once each");
        let hits = cache.hits();
        for s in &statements {
            cache.get_or_compile(s, &catalog, CAP).expect("ok");
        }
        assert_eq!(cache.hits(), hits + 5, "all resident under the current epoch");
        // Exact byte accounting after churn.
        let expected: usize = (0..cache.len()).map(|s| cache.slab[s].heap_bytes()).sum::<usize>()
            + cache.slab.capacity() * size_of::<Entry>()
            + cache.buckets.len() * size_of::<u32>();
        assert_eq!(cache.bytes(), expected);
    }

    /// The §4.1 hit-rate row as a property: a hot statement mix an
    /// order of magnitude smaller than the default capacity misses
    /// exactly once per distinct statement — every steady-state lookup
    /// hits, so the rate is bounded below by (n − mix)/n ≥ 99% for any
    /// n ≥ 100 × mix. 100k lookups over 20 statements: 99.98%.
    #[test]
    fn cache_hot_mix_hits_over_99_percent() {
        let catalog = TestCatalog::new();
        let mut cache = StatementCache::default();
        let mix: Vec<Vec<u8>> = (0..20)
            .map(|i| format!("SELECT * FROM ns WHERE v > {i} LIMIT 100").into_bytes())
            .collect();
        for round in 0..5_000usize {
            let statement = &mix[round % mix.len()];
            cache.get_or_compile(statement, &catalog, CAP).expect("ok");
        }
        assert_eq!(cache.misses(), 20, "one cold compile per distinct statement");
        assert_eq!(cache.evictions(), 0, "the hot set fits the default capacity");
        let total = cache.hits() + cache.misses();
        let rate = cache.hits() as f64 / total as f64;
        assert!(rate >= 0.99, "hot-mix hit rate {rate:.4} below the §4.1 gate");
    }

    #[test]
    fn capacity_zero_disables_but_still_serves() {
        let catalog = TestCatalog::new();
        let mut cache = StatementCache::new(0);
        cache.get_or_compile(b"SELECT * FROM ns WHERE v = 1", &catalog, CAP).expect("ok");
        cache.get_or_compile(b"SELECT * FROM ns WHERE v = 1", &catalog, CAP).expect("ok");
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.misses(), 2);
    }
}
