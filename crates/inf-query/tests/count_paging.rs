//! The S09 COUNT(*) paging AC (plan AC 2): page counts sum to
//! scan-derived truth under mutation load, per-page work is bounded by
//! the scan budget (counters asserted), and resume is exact — mid-key
//! included. The pager under test is the production `RangePager`
//! driving a real `IndexTree`; the residual is the production VM; only
//! doc custody (the pk-ref table) is a test fixture, standing where
//! S11's store resolution will.

use std::collections::{HashMap, HashSet};

use inf_doc::{DocValue, JsonParser, TapeDoc, path};
use inf_query::access::AccessStep;
use inf_query::page::{PageOutcome, PageResume, RangePager};
use inf_query::partiql::{CatalogView, CompiledStatement, compile};
use inf_store::{
    Fixed8, IndexId, IndexKeyBuf, IndexKeyType, IndexScalar, IndexSpec, IndexState, IndexTree,
    NsId, OrderedMap, index_key_encode,
};

struct OneIndex {
    spec: IndexSpec,
}

impl CatalogView for OneIndex {
    fn resolve_ns(&self, name: &[u8]) -> Option<NsId> {
        (name == b"ns").then_some(NsId(1))
    }

    fn index_by_name(&self, ns: NsId, name: &[u8]) -> Option<&IndexSpec> {
        (ns == NsId(1) && name == self.spec.name.as_slice()).then_some(&self.spec)
    }

    fn indexes(&self, ns: NsId) -> impl Iterator<Item = &IndexSpec> {
        (ns == NsId(1)).then_some(&self.spec).into_iter()
    }

    fn catalog_epoch(&self) -> u64 {
        1
    }
}

fn catalog(path_text: &str, key_type: IndexKeyType) -> OneIndex {
    OneIndex {
        spec: IndexSpec {
            id: IndexId(1),
            generation: 1,
            ns: NsId(1),
            name: b"idx".to_vec(),
            program: path::compile(path_text.as_bytes()).expect("path").as_bytes().to_vec(),
            key_type,
            state: IndexState::Ready,
        },
    }
}

fn i64_key(v: i64) -> Vec<u8> {
    let mut buf = IndexKeyBuf::new();
    index_key_encode(IndexKeyType::I64, IndexScalar::I64(v), &mut buf).expect("encodes");
    buf.as_bytes().to_vec()
}

/// A fixture document store: pk ref → (idoc bytes, live). The S04
/// maintenance contract keeps tree entries and docs in step; here the
/// test plays both roles.
struct Docs {
    docs: HashMap<u64, Vec<u8>>,
}

impl Docs {
    fn insert(&mut self, tree: &mut IndexTree, pk: u64, price: i64, flag: bool) {
        let json = format!("{{\"price\": {price}, \"flag\": {flag}}}");
        let bytes = JsonParser::new().parse(json.as_bytes()).expect("doc parses");
        self.docs.insert(pk, bytes);
        assert!(tree.insert(&i64_key(price), pk).expect("tree capacity"));
    }

    fn remove(&mut self, tree: &mut IndexTree, pk: u64, price: i64) {
        assert!(self.docs.remove(&pk).is_some());
        assert!(tree.remove(&i64_key(price), pk));
    }
}

/// Drive one page: candidates from the pager, residual through the
/// production VM (the S11 shape), matches reported back.
fn run_page(
    tree: &IndexTree,
    docs: &Docs,
    compiled: &CompiledStatement,
    resume: Option<&PageResume>,
    scan_budget: u32,
    limit_remaining: Option<u32>,
    emitted: &mut Vec<u64>,
) -> PageOutcome {
    let AccessStep::IndexRange { lo, hi, .. } = &compiled.access.step else {
        panic!("count statements over a range compile to index ranges");
    };
    let mut pager = RangePager::new(lo, hi, resume, scan_budget, limit_remaining);
    while let Some((_, pk)) = pager.next(tree) {
        let doc = docs.docs.get(&pk).expect("fixture keeps docs and entries in step");
        let matched = match &compiled.vm {
            None => true,
            Some(vm) => {
                let tape = TapeDoc::from_bytes(doc).expect("valid idoc");
                vm.eval(DocValue::from(tape.root()), 10_000).expect("fuel suffices").verdict
            }
        };
        if matched {
            emitted.push(pk);
            pager.count_match();
        }
    }
    let outcome = pager.finish();
    assert!(outcome.scanned <= scan_budget, "a page never scans past its budget");
    outcome
}

/// COUNT over a multi-page range with a residual, mutating between
/// pages: stable documents count exactly once; scanned stays bounded;
/// the page sum equals the emitted set.
#[test]
fn count_pages_sum_to_truth_under_mutation() {
    let catalog = catalog("$.price", IndexKeyType::I64);
    let mut tree = IndexTree::Fixed8(OrderedMap::<Fixed8>::new());
    let mut docs = Docs { docs: HashMap::new() };
    // Stable docs: prices 0..100, flag alternates — the residual keeps
    // the even pks. In-range stable matches: price in [10, 60) & flag.
    for pk in 0..100u64 {
        docs.insert(&mut tree, pk, pk as i64, pk % 2 == 0);
    }
    let stable_expected: HashSet<u64> =
        (0..100u64).filter(|pk| (10..60).contains(&(*pk as i64)) && pk % 2 == 0).collect();
    let compiled = compile(
        b"SELECT COUNT(*) FROM ns WHERE price >= 10 AND price < 60 AND flag = TRUE",
        &catalog,
    )
    .expect("compiles");
    assert!(compiled.vm.is_some(), "the flag conjunct rides the residual");

    let mut emitted: Vec<u64> = Vec::new();
    let mut total: u64 = 0;
    let mut resume: Option<PageResume> = None;
    let mut pages = 0;
    let mut churn_pk = 1000u64;
    loop {
        let outcome = run_page(&tree, &docs, &compiled, resume.as_ref(), 7, None, &mut emitted);
        total += u64::from(outcome.matched);
        pages += 1;
        assert!(pages <= 40, "paging terminates");
        if !outcome.more {
            break;
        }
        resume = outcome.resume;
        assert!(resume.is_some(), "a consumed page resumes after its last pair");
        // Mutation load between pages: churn docs behind, inside, and
        // past the range. Churned docs are exempt from exactness (per-
        // cell read-committed honesty); stable ones are not.
        docs.insert(&mut tree, churn_pk, 5, true); // behind the range
        docs.insert(&mut tree, churn_pk + 1, 55, true); // inside
        docs.insert(&mut tree, churn_pk + 2, 90, true); // past
        if churn_pk > 1000 {
            // Take back the previous round's in-range churn doc.
            docs.remove(&mut tree, churn_pk - 3 + 1, 55);
        }
        churn_pk += 3;
    }
    let emitted_set: HashSet<u64> = emitted.iter().copied().collect();
    assert_eq!(emitted_set.len(), emitted.len(), "no document is counted twice");
    assert_eq!(total as usize, emitted.len(), "page counts sum to the emitted documents");
    for pk in &stable_expected {
        assert!(emitted_set.contains(pk), "stable in-range doc {pk} was counted");
    }
    for pk in &emitted_set {
        if *pk < 100 {
            assert!(stable_expected.contains(pk), "stable out-of-range doc {pk} appeared");
        }
    }
}

/// Mid-key resume: a multi-valued equality range holds many refs under
/// one key; page boundaries inside the key must not skip or duplicate
/// (the `OrderedCursor::resume_after` contract).
#[test]
fn resume_is_exact_mid_key() {
    let catalog = catalog("$.tags[*]", IndexKeyType::I64);
    let mut tree = IndexTree::Fixed8(OrderedMap::<Fixed8>::new());
    let mut docs = Docs { docs: HashMap::new() };
    // Ten docs share the indexed value 7 (plus neighbors on both sides
    // that the equality range must exclude).
    for pk in 0..10u64 {
        let json = format!("{{\"tags\": [7], \"pk\": {pk}}}");
        let bytes = JsonParser::new().parse(json.as_bytes()).expect("doc parses");
        docs.docs.insert(pk, bytes);
        assert!(tree.insert(&i64_key(7), pk).expect("capacity"));
    }
    assert!(tree.insert(&i64_key(6), 96).expect("capacity"));
    assert!(tree.insert(&i64_key(8), 98).expect("capacity"));
    docs.docs.insert(96, JsonParser::new().parse(b"{\"tags\": [6]}").expect("doc"));
    docs.docs.insert(98, JsonParser::new().parse(b"{\"tags\": [8]}").expect("doc"));

    let compiled = compile(b"SELECT * FROM ns WHERE tags[*] = 7", &catalog).expect("compiles");
    let mut emitted: Vec<u64> = Vec::new();
    let mut resume: Option<PageResume> = None;
    let mut pages = 0;
    loop {
        let outcome = run_page(&tree, &docs, &compiled, resume.as_ref(), 3, None, &mut emitted);
        pages += 1;
        if !outcome.more {
            break;
        }
        resume = outcome.resume;
    }
    // 3+3+3+1: the fourth page consumes the last ref and discovers the
    // upper bound in the same page (the bound probe charges no scan).
    assert_eq!(pages, 4, "10 refs at one key over budget-3 pages");
    assert_eq!(emitted, (0..10).collect::<Vec<u64>>(), "every ref once, in ref order");
}

/// Statement LIMIT is a total cap across pages; reaching it completes
/// the statement even though the range has more entries.
#[test]
fn limit_caps_the_total_across_pages() {
    let catalog = catalog("$.price", IndexKeyType::I64);
    let mut tree = IndexTree::Fixed8(OrderedMap::<Fixed8>::new());
    let mut docs = Docs { docs: HashMap::new() };
    for pk in 0..20u64 {
        docs.insert(&mut tree, pk, pk as i64, true);
    }
    let compiled =
        compile(b"SELECT * FROM ns WHERE price >= 0 LIMIT 5", &catalog).expect("compiles");
    assert_eq!(compiled.access.limit, Some(5));
    let mut emitted: Vec<u64> = Vec::new();
    let mut limit_remaining = compiled.access.limit;
    let mut resume: Option<PageResume> = None;
    let mut total = 0u32;
    loop {
        let outcome =
            run_page(&tree, &docs, &compiled, resume.as_ref(), 3, limit_remaining, &mut emitted);
        total += outcome.matched;
        if !outcome.more {
            break;
        }
        resume = outcome.resume;
        limit_remaining = limit_remaining.map(|l| l - outcome.matched);
    }
    assert_eq!(total, 5, "LIMIT bounds the statement, not the page");
    assert_eq!(emitted, vec![0, 1, 2, 3, 4]);
}

/// Early suspension (the S11 byte-budget shape): stop driving the pager
/// mid-page, resume from the outcome, lose nothing.
#[test]
fn suspension_resumes_without_loss() {
    let catalog = catalog("$.price", IndexKeyType::I64);
    let mut tree = IndexTree::Fixed8(OrderedMap::<Fixed8>::new());
    let mut docs = Docs { docs: HashMap::new() };
    for pk in 0..10u64 {
        docs.insert(&mut tree, pk, pk as i64, true);
    }
    let compiled = compile(b"SELECT * FROM ns WHERE price >= 0", &catalog).expect("compiles");
    let AccessStep::IndexRange { lo, hi, .. } = &compiled.access.step else { unreachable!() };
    // Take two candidates, then suspend mid-page.
    let mut pager = RangePager::new(lo, hi, None, 100, None);
    let mut emitted: Vec<u64> = Vec::new();
    for _ in 0..2 {
        let (_, pk) = pager.next(&tree).expect("candidates remain");
        emitted.push(pk);
        pager.count_match();
    }
    let suspended = pager.finish();
    assert!(suspended.more);
    assert_eq!(suspended.scanned, 2);
    let resume = suspended.resume.expect("a consumed page resumes");
    let outcome = run_page(&tree, &docs, &compiled, Some(&resume), 100, None, &mut emitted);
    assert!(!outcome.more);
    assert_eq!(emitted, (0..10).collect::<Vec<u64>>());
}
