//! M3-S04 store integration (ADR-0038 D3/D4): with the knob on, stored
//! tape bytes intern (attributed via `intern_bytes`), reads resolve
//! through the dict-aware cursors, and every canonical emission —
//! `json_freeze`, COPY — is the plain form, byte-identical to a store
//! with interning off.
#![cfg(all(feature = "doc", feature = "doc-intern-keys"))]

use inf_doc::model::{self, Value};
use inf_foundation::time::Nanos;
use inf_store::{CellStore, CopyResult, DocDomain, JsonLogDecision, JsonSetOptions, StoreConfig};

fn now() -> Nanos {
    Nanos::from_millis(1)
}

/// Blob-tier document with heavily repeated keys (the interning shape).
fn wide_doc() -> Vec<u8> {
    let element = |i: i64| {
        Value::Obj(vec![
            ("identifier".into(), Value::I64(i)),
            ("display_name".into(), Value::Str(format!("row{i}"))),
        ])
    };
    model::encode(&Value::Arr((0..40).map(element).collect())).expect("encodes")
}

#[test]
fn interning_on_and_off_are_observationally_identical() {
    let doc = wide_doc();
    let mut off = CellStore::new(StoreConfig::default());
    let mut on = CellStore::new(StoreConfig { doc_intern_keys: true, ..StoreConfig::default() });
    for store in [&mut off, &mut on] {
        store.json_set(b"k", &doc, JsonSetOptions::default(), now()).expect("set");
    }
    // The knob changes stored bytes (attributed), never observable state.
    assert_eq!(off.doc_domain().intern_bytes, 0);
    assert!(on.doc_domain().intern_bytes > 0, "table bytes attributed");
    assert!(
        on.doc_domain().tape_bytes < off.doc_domain().tape_bytes,
        "interned blob is strictly smaller"
    );
    // Cursor reads agree.
    for store in [&mut off, &mut on] {
        let read = store.json_get(b"k", now()).expect("doc").expect("present");
        let inf_doc::DocValue::Arr(arr) = read.root else { panic!("array root") };
        assert_eq!(arr.len(), 40);
        let Some(inf_doc::DocValue::Obj(first)) = arr.index(0) else { panic!("object") };
        assert!(matches!(first.get(b"identifier"), Some(inf_doc::DocValue::I64(0))));
    }
    // Canonical emissions are byte-identical (the E8 precondition).
    let frozen_off = off.json_freeze(b"k", now()).expect("doc").expect("present");
    let frozen_on = on.json_freeze(b"k", now()).expect("doc").expect("present");
    assert_eq!(frozen_off, doc);
    assert_eq!(frozen_on, doc, "freeze un-interns to the canonical plain form");
    assert_eq!(
        on.json_log_image_bytes(b"k", now()),
        Some(doc.len()),
        "durable admission reserves the plain emitted image, not interned storage"
    );
    let Some(JsonLogDecision::Full { idoc, .. }) = on.json_log_full(b"k", now()) else {
        panic!("document stages a full image")
    };
    assert_eq!(idoc.len(), doc.len());
    // COPY out of an interning store carries plain canonical bytes.
    assert_eq!(on.copy(b"k", b"c", false, now()).expect("copy"), CopyResult::Copied);
    assert_eq!(on.json_freeze(b"c", now()).expect("doc").expect("present"), doc);
    // Teardown drains both domains to zero — intern accounting included.
    for store in [&mut off, &mut on] {
        store.flush(now());
        assert_eq!(store.doc_domain(), DocDomain::default());
    }
}

#[test]
fn documents_without_winning_keys_stay_plain_under_the_knob() {
    let mut s = CellStore::new(StoreConfig { doc_intern_keys: true, ..StoreConfig::default() });
    let doc = model::encode(&Value::Obj(vec![(
        "solo".into(),
        Value::Str("x".repeat(700)), // blob tier, single-use key
    )]))
    .expect("encodes");
    s.json_set(b"k", &doc, JsonSetOptions::default(), now()).expect("set");
    let d = s.doc_domain();
    assert_eq!(d.intern_bytes, 0, "no winner ⇒ stored plain");
    assert_eq!(d.tape_bytes, doc.len() as u64);
    assert_eq!(s.json_freeze(b"k", now()).expect("doc").expect("present"), doc);
}
