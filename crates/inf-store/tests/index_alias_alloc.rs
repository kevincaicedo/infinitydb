//! ARCH-W0.2 — the allocation control for ADR-0139 D10's one named
//! deviation. After the pre-half, `inf-store` owns the scratch a
//! never-admitted key's commit-half needs (`index_alias.rs` proves that
//! with every scratch growth refused). What still allocates there is
//! `inf_doc::path::eval`'s match set, through the global allocator —
//! kept, bounded and owned by the document-engine repairs.
//!
//! The counting allocator has no scope tag, so the attribution is by
//! equality: the commit-half's allocation count must equal, exactly, the
//! count of the same document read and `eval` calls made directly. One allocation from
//! `index_maint`, the alias enumeration or the tree would break the
//! equality. Counts are per thread (a process-wide delta captures
//! harness noise it cannot attribute).

#![cfg(feature = "doc")]

use inf_alloc::CountingAllocator;
use inf_doc::path::{EvalLimits, compile, eval};
use inf_doc::{CanonicalDoc, JsonParser};
use inf_foundation::time::Nanos;
use inf_store::{
    COLLISION_KEY_PREFIX, ExpireCond, IndexId, IndexKeyType, IndexSpec, IndexState, JsonSetOptions,
    Keyspace, NsId, StoreConfig,
};

#[global_allocator]
static ALLOC: CountingAllocator = CountingAllocator::new();

const NS: NsId = NsId(0);
const PATHS: [&str; 2] = ["$.v", "$.tags[*]"];
const T0: Nanos = Nanos(1_000_000_000);

fn alias_key(member: u8) -> [u8; 48] {
    let mut out = [0u8; 48];
    out[..16].copy_from_slice(COLLISION_KEY_PREFIX);
    out[32..].copy_from_slice(&[member; 16]);
    out
}

fn fixture() -> Keyspace {
    let mut ks = Keyspace::new(StoreConfig::default());
    for (at, path) in PATHS.iter().enumerate() {
        let id = at as u32 + 1;
        ks.idx_create(IndexSpec {
            id: IndexId(id),
            generation: u64::from(id),
            ns: NS,
            name: format!("idx-{id}").into_bytes(),
            program: compile(path.as_bytes()).expect("valid path").as_bytes().to_vec(),
            key_type: IndexKeyType::I64,
            state: IndexState::Declared,
        })
        .expect("declare");
    }
    ks.db_mut(0);
    ks
}

fn put(ks: &mut Keyspace, key: &[u8], json: &str) {
    let idoc = JsonParser::new().parse(json.as_bytes()).expect("valid doc");
    ks.idx_bracket_begin(NS, &[key], None).expect("pre-half");
    ks.db_mut(0)
        .json_set(
            key,
            &CanonicalDoc::validate(&idoc).expect("canonical fixture"),
            JsonSetOptions::default(),
            T0,
        )
        .expect("json_set");
    ks.idx_bracket_commit(NS, &[key]);
}

/// What re-evaluating `key`'s document may cost: one document-root read
/// plus one direct `eval` per index. The root read is measured, not
/// assumed — it is free in release and costs the debug-only boundary
/// re-validation (`TapeDoc::from_validated_bytes`'s `debug_assert!`) in a
/// test profile.
fn reevaluation_allocations(ks: &mut Keyspace, key: &[u8]) -> u64 {
    let programs: Vec<_> = PATHS.iter().map(|p| compile(p.as_bytes()).expect("path")).collect();
    let limits = EvalLimits::default();
    let before = ALLOC.thread_allocations();
    let read = ks.db_mut(0).json_get(key, T0).expect("read").expect("present");
    let root_read = ALLOC.thread_allocations() - before;
    for program in &programs {
        let matches = eval(program, read.root, &limits).expect("small document");
        std::hint::black_box(matches.iter().count());
    }
    let total = ALLOC.thread_allocations() - before;
    assert!(total > root_read, "the control is live: `eval` allocates its match set today");
    total
}

/// `EXPIRE` reaches no publishing funnel: the commit-half re-evaluates
/// the unchanged document. Everything it allocates is `path::eval`'s.
#[test]
fn a_never_admitted_keys_commit_half_allocates_only_inside_path_eval() {
    let key = b"doc:expire";
    let mut ks = fixture();
    put(&mut ks, key, r#"{"v":7,"tags":[1,2,3,4,5,6,7,8]}"#);
    let allowed = reevaluation_allocations(&mut ks, key);

    ks.idx_bracket_begin(NS, &[key], None).expect("pre-half");
    let deadline = Some(Nanos(900_000_000_000));
    assert!(ks.db_mut(0).expire(key, deadline, ExpireCond::Always, T0));
    let before = ALLOC.thread_allocations();
    ks.idx_bracket_commit(NS, &[key]);
    let spent = ALLOC.thread_allocations() - before;
    assert_eq!(spent, allowed, "every commit-half allocation is one of `path::eval`'s");
}

/// A removal beside an alias adds the enumeration, the member's
/// evaluation and the held marks — and still nothing but `eval`: the view
/// is a stack array and a member's keys stream through the one key
/// buffer. The dying key has no post-image, so the only evaluations are
/// the member's.
#[test]
fn an_alias_evaluation_allocates_only_inside_path_eval() {
    let (dying, holder) = (alias_key(b'a'), alias_key(b'b'));
    let mut ks = fixture();
    put(&mut ks, &dying, r#"{"v":7,"tags":[1,2]}"#);
    put(&mut ks, &holder, r#"{"v":7,"tags":[2,3]}"#);
    let allowed = reevaluation_allocations(&mut ks, &holder);

    ks.idx_bracket_begin(NS, &[&dying], None).expect("pre-half");
    assert!(ks.db_mut(0).del(&dying, T0));
    let before = ALLOC.thread_allocations();
    ks.idx_bracket_commit(NS, &[&dying]);
    let spent = ALLOC.thread_allocations() - before;
    assert_eq!(spent, allowed, "the enumeration and the held marks allocate nothing");
    let counters = ks.idx_counters_total();
    assert_eq!(counters.alias_groups, 1, "the row reached an alias");
    assert_eq!(counters.alias_kept, 2, "`v = 7` and tag `2` stay for the holder");
}
