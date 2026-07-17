//! M3-S16 error-atomicity AC: an operation engineered to fail at the
//! k-th match leaves canonical bytes, record version, and document-domain
//! accounting unchanged across inline, tape-blob, and arena-tree forms.
//!
//! Release campaign:
//! `PROPTEST_CASES=100000 cargo test --release -p inf-store --test mutation_error_atomicity`
#![cfg(feature = "doc")]

use inf_doc::TapeDoc;
use inf_doc::apply::{ApplyError, ApplyOp, apply};
use inf_doc::model::{self, Value};
use inf_doc::path::{EvalLimits, compile};
use inf_foundation::time::Nanos;
use inf_store::{CellStore, JsonSetOptions, StoreConfig};
use proptest::prelude::*;

const NOW: Nanos = Nanos::from_millis(1);

proptest! {
    #[test]
    fn kth_match_failure_preserves_logical_and_physical_state(
        valid_before in 0usize..8,
        valid_after in 0usize..8,
        index in 1usize..12,
        form in 0u8..3,
    ) {
        let entries = (0..valid_before + valid_after + 1)
            .map(|position| {
                let len = if position == valid_before { index - 1 } else { index };
                let values = (0..len).map(|value| Value::I64(value as i64)).collect();
                (format!("k{position}"), Value::Arr(values))
            })
            .collect();
        let idoc = model::encode(&Value::Obj(entries)).expect("fixture");
        let cfg = match form {
            0 => StoreConfig {
                doc_inline_bytes_max: usize::MAX,
                doc_morph_bytes_min: usize::MAX,
                ..StoreConfig::default()
            },
            1 => StoreConfig {
                doc_inline_bytes_max: 0,
                doc_morph_bytes_min: usize::MAX,
                ..StoreConfig::default()
            },
            2 => StoreConfig {
                doc_inline_bytes_max: 0,
                doc_morph_bytes_min: 0,
                ..StoreConfig::default()
            },
            _ => unreachable!("strategy emits 0..3"),
        };
        let mut store = CellStore::new(cfg);
        store.json_set(b"doc", &idoc, JsonSetOptions::default(), NOW).expect("set");
        let before_bytes = store.json_freeze(b"doc", NOW).unwrap().unwrap();
        let before_version = store.json_get(b"doc", NOW).unwrap().unwrap().version;
        let before_domain = store.doc_domain();
        let before_live = store.doc_live_bytes();

        let program = compile(b"$.*").expect("path");
        let elements = model::encode_fragment(&Value::Arr(vec![Value::I64(9)]))
            .expect("operand");
        let doc = TapeDoc::from_validated_bytes(&before_bytes);
        let error = apply(
            &doc,
            &program,
            &ApplyOp::ArrInsert { index: index as i64, elements: &elements },
            &EvalLimits::default(),
            store.doc_max_bytes(),
        )
        .expect_err("the k-th array is too short");
        prop_assert_eq!(error, ApplyError::OutOfBounds);

        prop_assert_eq!(store.json_freeze(b"doc", NOW).unwrap().unwrap(), before_bytes);
        prop_assert_eq!(store.json_get(b"doc", NOW).unwrap().unwrap().version, before_version);
        prop_assert_eq!(store.doc_domain(), before_domain);
        prop_assert_eq!(store.doc_live_bytes(), before_live);
    }
}
