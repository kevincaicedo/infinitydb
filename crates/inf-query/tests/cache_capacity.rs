//! ADR-0146: resource boundaries and an allocator-owned census of cached statements.

use inf_alloc::CountingAllocator;
use inf_query::limits::{STATEMENT_CACHE_ENTRIES_MAX, StatementCacheCapacity};
use inf_query::partiql::{CatalogView, StatementCache};
use inf_store::{IndexId, IndexKeyType, IndexSpec, IndexState, NsId};

#[global_allocator]
static ALLOC: CountingAllocator = CountingAllocator::new();

struct Catalog(IndexSpec);

impl Catalog {
    fn new() -> Self {
        Self(IndexSpec {
            id: IndexId(1),
            generation: 1,
            ns: NsId(1),
            name: b"idx".to_vec(),
            program: inf_doc::path::compile(b"$.v").unwrap().as_bytes().to_vec(),
            key_type: IndexKeyType::I64,
            state: IndexState::Ready,
        })
    }
}

impl CatalogView for Catalog {
    fn resolve_ns(&self, name: &[u8]) -> Option<NsId> {
        (name == b"ns").then_some(NsId(1))
    }
    fn index_by_name(&self, ns: NsId, name: &[u8]) -> Option<&IndexSpec> {
        (ns == NsId(1) && name == b"idx").then_some(&self.0)
    }
    fn indexes(&self, ns: NsId) -> impl Iterator<Item = &IndexSpec> {
        (ns == NsId(1)).then_some(&self.0).into_iter()
    }
    fn catalog_epoch(&self) -> u64 {
        1
    }
}

#[test]
fn capacity_boundary_refuses_before_any_allocation() {
    for requested in [usize::from(STATEMENT_CACHE_ENTRIES_MAX) + 1, u32::MAX as usize, usize::MAX] {
        let before = ALLOC.thread_allocations();
        let result = StatementCacheCapacity::try_from(requested);
        assert!(result.is_err(), "out-of-budget count must be refused before constructing");
        assert_eq!(ALLOC.thread_allocations(), before);
    }
    for requested in [0, 1024, usize::from(STATEMENT_CACHE_ENTRIES_MAX)] {
        let capacity = StatementCacheCapacity::try_from(requested).unwrap();
        let before = ALLOC.thread_allocations();
        let cache = StatementCache::try_new(capacity).unwrap();
        assert_eq!(ALLOC.thread_allocations() - before, if requested == 0 { 0 } else { 2 });
        assert!(cache.is_empty());
    }
}

#[test]
fn either_metadata_reservation_can_refuse_without_replacing_the_cache() {
    let catalog = Catalog::new();
    let capacity = StatementCacheCapacity::try_from(4).unwrap();
    let mut current = StatementCache::try_new(capacity).unwrap();
    current.get_or_compile(b"SELECT * FROM ns WHERE v = 1", &catalog, 8192).unwrap();
    let before = (current.len(), current.bytes(), current.hits(), current.misses());
    for skip in 0..2 {
        let guard = ALLOC.refuse_after(skip).unwrap();
        let replacement = StatementCache::try_new(capacity);
        let triggered = guard.was_triggered();
        drop(guard);
        assert!(triggered, "reservation refusal was never reached");
        assert!(replacement.is_err(), "an allocation refusal must reach the constructor caller");
        assert_eq!((current.len(), current.bytes(), current.hits(), current.misses()), before);
    }
}

#[test]
fn resident_byte_gauge_counts_decoded_buffers_and_shared_program_once() {
    let catalog = Catalog::new();
    let mut cache = StatementCache::try_new(StatementCacheCapacity::try_from(4).unwrap()).unwrap();
    let statements: &[&[u8]] = &[
        b"SELECT * FROM ns WHERE $key = 'alpha'",
        b"SELECT * FROM ns WHERE v BETWEEN 1 AND 9 AND label = 'open'",
        b"SELECT * FROM ns.SCAN WHERE nested.value = 'ready' AND n IN (1, 2, 3)",
        b"SELECT * FROM ns WHERE v = 4",
        b"SELECT * FROM ns WHERE v = 5",
        b"SELECT * FROM ns WHERE v = 6",
    ];
    for text in statements {
        let reported = cache.bytes();
        let requested = ALLOC.thread_bytes();
        let freed = ALLOC.thread_freed_bytes();
        cache.get_or_compile(std::hint::black_box(text), &catalog, 8192).unwrap();
        let retained = i128::from(ALLOC.thread_bytes() - requested)
            - i128::from(ALLOC.thread_freed_bytes() - freed);
        assert_eq!(cache.bytes() as i128 - reported as i128, retained, "statement: {text:?}");
    }
}
