//! M3-S03 storm ACs (ADR-0037 D4/D5): randomized create/mutate/delete/
//! expire/morph storms across all three placement tiers, with the
//! reconciliation invariant asserted after **every** operation —
//! `doc_live_bytes == tape_bytes + arena_bytes` — versions bumping exactly
//! once per logical mutation, and the domain draining to exactly zero at
//! teardown (zero drift).
//!
//! CI runs proptest's default case count; the 10⁶-op AC run is executed
//! explicitly with `PROPTEST_CASES=5000 cargo test --release -p inf-store
//! --test doc_storm` (5000 cases × 200 ops) and recorded in the ledger.
#![cfg(feature = "doc")]

use proptest::collection::vec;
use proptest::prelude::*;

use inf_doc::model::{self, Value};
use inf_foundation::time::Nanos;
use inf_store::{
    CellStore, ExpireCond, ExpiryBudget, JsonSetOptions, JsonSetOutcome, OpError, StoreConfig,
};

const KEYS: usize = 24;
const OPS_PER_CASE: usize = 200;

#[derive(Copy, Clone, Debug)]
enum SizeClass {
    Inline,
    Blob,
    Tree,
}

#[derive(Copy, Clone, Debug)]
enum Op {
    Set {
        key: usize,
        size: SizeClass,
    },
    Replace {
        key: usize,
        size: SizeClass,
    },
    /// Morph to tree, then push `pushes` elements in one edit command.
    MorphAndEdit {
        key: usize,
        pushes: u8,
    },
    Del {
        key: usize,
    },
    /// Arm a short TTL; a later `WheelTick`/read reaps it.
    ExpireSoon {
        key: usize,
    },
    WheelTick,
    Flush,
}

fn op_strategy() -> impl Strategy<Value = Op> {
    let key = 0..KEYS;
    let size = prop_oneof![Just(SizeClass::Inline), Just(SizeClass::Blob), Just(SizeClass::Tree)];
    prop_oneof![
        8 => (key.clone(), size.clone()).prop_map(|(key, size)| Op::Set { key, size }),
        4 => (key.clone(), size).prop_map(|(key, size)| Op::Replace { key, size }),
        4 => (key.clone(), 1u8..12).prop_map(|(key, pushes)| Op::MorphAndEdit { key, pushes }),
        3 => key.clone().prop_map(|key| Op::Del { key }),
        2 => key.prop_map(|key| Op::ExpireSoon { key }),
        2 => Just(Op::WheelTick),
        1 => Just(Op::Flush),
    ]
}

/// A document in the requested tier, rooted in an array so morph/edit work.
fn doc_for(size: SizeClass, salt: i64) -> Vec<u8> {
    let elements = match size {
        SizeClass::Inline => 8,
        SizeClass::Blob => 300,
        SizeClass::Tree => 1400, // ≥ 4096 idoc bytes with 3-byte varints
    };
    let items = (0..elements).map(|i| Value::I64(salt.wrapping_mul(1_000) + 200 + i)).collect();
    model::encode(&Value::Arr(items)).expect("encodes")
}

/// What the model expects of one key.
#[derive(Copy, Clone, Default)]
struct Expect {
    /// Expected version after every counted logical mutation, or `None`
    /// when the key must be absent.
    version: Option<u32>,
    /// Absolute deadline in ms, when armed.
    deadline_ms: Option<u64>,
}

fn reconcile(store: &CellStore) {
    let d = store.doc_domain();
    assert_eq!(
        store.doc_live_bytes(),
        d.tape_bytes + d.arena_bytes,
        "domain must partition the doc arena exactly"
    );
    assert!(d.slack_bytes <= d.arena_bytes, "slack is a subset of tree bytes");
}

proptest! {
    #[test]
    fn storms_reconcile_to_zero_drift(ops in vec(op_strategy(), OPS_PER_CASE)) {
        let mut store = CellStore::new(StoreConfig::default());
        let mut expect = [Expect::default(); KEYS];
        let mut now_ms: u64 = 1;
        let key_name = |k: usize| format!("key:{k:02}");

        for op in ops {
            now_ms += 2;
            let now = Nanos::from_millis(now_ms);
            // Fold lazily-expired keys into the model before acting.
            for e in &mut expect {
                if e.deadline_ms.is_some_and(|at| now_ms >= at) {
                    *e = Expect::default();
                }
            }
            match op {
                Op::Set { key, size } => {
                    let doc = doc_for(size, key as i64);
                    let name = key_name(key);
                    let outcome = store
                        .json_set(name.as_bytes(), &doc, JsonSetOptions::default(), now)
                        .expect("set");
                    prop_assert_eq!(outcome, JsonSetOutcome::Applied);
                    let e = &mut expect[key];
                    // Plain set clears the TTL and chains the version.
                    e.version = Some(e.version.map_or(1, |v| (v + 1) & 0xFF_FFFF));
                    e.deadline_ms = None;
                }
                Op::Replace { key, size } => {
                    let doc = doc_for(size, -(key as i64) - 1);
                    let name = key_name(key);
                    let replaced = store.json_replace(name.as_bytes(), &doc, now).expect("ok");
                    let e = &mut expect[key];
                    prop_assert_eq!(replaced, e.version.is_some());
                    if let Some(v) = e.version {
                        e.version = Some((v + 1) & 0xFF_FFFF);
                    }
                }
                Op::MorphAndEdit { key, pushes } => {
                    let name = key_name(key);
                    let morphed = store.json_morph(name.as_bytes(), now).expect("morph");
                    let e = &mut expect[key];
                    prop_assert_eq!(morphed, e.version.is_some(), "morph hits live keys only");
                    if morphed {
                        // Morph never bumps; the edit bumps exactly once.
                        let edited = store
                            .json_edit_tree(name.as_bytes(), now, |doc, arena| {
                                let mut root = doc.root_ref();
                                for i in 0..pushes {
                                    let v = doc.alloc_i64(arena, i64::from(i))?;
                                    root = doc.arr_push(arena, root, v)?;
                                }
                                Ok(())
                            })
                            .expect("edit");
                        prop_assert_eq!(edited, Some(()));
                        let v = e.version.expect("live");
                        e.version = Some((v + 1) & 0xFF_FFFF);
                    }
                }
                Op::Del { key } => {
                    let name = key_name(key);
                    let existed = store.del(name.as_bytes(), now);
                    prop_assert_eq!(existed, expect[key].version.is_some());
                    expect[key] = Expect::default();
                }
                Op::ExpireSoon { key } => {
                    let name = key_name(key);
                    let deadline = Nanos::from_millis(now_ms + 3);
                    let applied =
                        store.expire(name.as_bytes(), Some(deadline), ExpireCond::Always, now);
                    let e = &mut expect[key];
                    prop_assert_eq!(applied, e.version.is_some());
                    if let Some(v) = e.version {
                        // The TTL rewrite bumps like any key mutation.
                        e.version = Some((v + 1) & 0xFF_FFFF);
                        e.deadline_ms = Some(now_ms + 3);
                    }
                }
                Op::WheelTick => {
                    store.expire_tick(now, ExpiryBudget::default());
                }
                Op::Flush => {
                    store.flush(now);
                    expect = [Expect::default(); KEYS];
                    prop_assert_eq!(store.doc_domain(), inf_store::DocDomain::default());
                }
            }
            reconcile(&store);
        }

        // Versions: every live key answers exactly its counted version.
        let final_now = Nanos::from_millis(now_ms + 100);
        for (k, e) in expect.iter().enumerate() {
            let name = key_name(k);
            let alive_expected =
                e.version.is_some() && e.deadline_ms.is_none_or(|at| now_ms + 100 < at);
            match store.json_get(name.as_bytes(), final_now) {
                Ok(Some(read)) => {
                    prop_assert!(alive_expected, "key {name} should be gone");
                    prop_assert_eq!(read.version, e.version.expect("live"), "version drift");
                }
                Ok(None) => prop_assert!(!alive_expected, "key {name} should be alive"),
                Err(e) => return Err(TestCaseError::fail(format!("json_get: {e:?}"))),
            }
        }

        // Teardown: zero drift, exactly (the S03 AC).
        for k in 0..KEYS {
            let name = key_name(k);
            let _ = store.del(name.as_bytes(), final_now);
        }
        prop_assert_eq!(store.doc_domain(), inf_store::DocDomain::default());
        prop_assert_eq!(store.doc_live_bytes(), 0);
    }

    /// Mixed-type interleaving: strings and documents share the keyspace
    /// without cross-contamination — a plain SET over a document releases
    /// its payload, WrongType refusals mutate nothing, and the domain
    /// still drains to zero.
    #[test]
    fn mixed_string_document_keyspace_reconciles(
        ops in vec((0..KEYS, 0u8..4), OPS_PER_CASE / 2)
    ) {
        let mut store = CellStore::new(StoreConfig::default());
        let now = Nanos::from_millis(1);
        for (key, kind) in ops {
            let name = format!("key:{key:02}");
            match kind {
                0 => {
                    store.set(name.as_bytes(), b"plain-string", Default::default(), now)
                        .expect("set");
                }
                1 => {
                    let doc = doc_for(SizeClass::Blob, key as i64);
                    // WrongType when a string sits there; overwrite via DEL.
                    match store.json_set(name.as_bytes(), &doc, JsonSetOptions::default(), now) {
                        Ok(_) => {}
                        Err(OpError::WrongType) => {
                            store.del(name.as_bytes(), now);
                        }
                        Err(e) => return Err(TestCaseError::fail(format!("json_set: {e:?}"))),
                    }
                }
                2 => {
                    let _ = store.incr_by(name.as_bytes(), 1, now); // WrongType on docs — inert
                }
                _ => {
                    store.del(name.as_bytes(), now);
                }
            }
            reconcile(&store);
        }
        store.flush(now);
        prop_assert_eq!(store.doc_domain(), inf_store::DocDomain::default());
        prop_assert_eq!(store.doc_live_bytes(), 0);
    }
}
