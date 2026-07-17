//! M3-S04 equivalence AC (ADR-0038 D2/D3): canonical serialization is
//! byte-identical with interning on and off — `unintern ∘ intern` is the
//! identity, interned documents decode to the same value, freeze through
//! the arena projection yields the canonical plain bytes, and interning
//! never grows a document.
#![cfg(feature = "doc-intern-keys")]

use proptest::collection::vec;
use proptest::prelude::*;

use inf_alloc::arena::{Arena, ArenaConfig};
use inf_doc::intern;
use inf_doc::model::{self, Value};
use inf_doc::{ArenaDoc, TapeDoc};

/// Documents with a small key alphabet so repeated keys — the shape
/// interning exists for — occur constantly; scalars keep full generality.
fn repeated_key_strategy() -> impl Strategy<Value = Value> {
    let key = prop_oneof![
        Just("id".to_string()),
        Just("name_field".to_string()),
        Just("value".to_string()),
        Just("k".to_string()),
        Just("a_much_longer_shared_key_name".to_string()),
        ".{0,12}".prop_map(|s| s),
    ];
    let scalar = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(Value::I64),
        any::<f64>().prop_filter("finite only", |f| f.is_finite()).prop_map(Value::F64),
        ".{0,24}".prop_map(Value::Str),
    ];
    scalar.prop_recursive(4, 128, 12, move |inner| {
        let key = key.clone();
        prop_oneof![
            vec(inner.clone(), 0..12).prop_map(Value::Arr),
            vec((key, inner), 0..12).prop_map(|entries| Value::Obj(entries.into_iter().collect())),
        ]
    })
}

proptest! {
    #[test]
    fn canonical_serialization_is_intern_invariant(v in repeated_key_strategy()) {
        let plain = model::encode(&v).expect("encodes");
        let Some(interned) = intern::intern(&plain) else {
            return Ok(()); // no winning key — the document stays plain
        };
        // Interning never grows a document at rest.
        prop_assert!(interned.len() < plain.len());
        // The transform pair is the identity on the plain side.
        prop_assert_eq!(intern::unintern(&interned), plain.clone());
        // The interned form validates and decodes to the same value.
        let idoc = TapeDoc::from_bytes(&interned).expect("interned form validates");
        prop_assert_eq!(model::from_tape(&idoc), v.clone());
        // Cursor equivalence: the arena projection built FROM the interned
        // tape freezes to the canonical PLAIN bytes — the physical layout
        // never reaches a comparator (the E8 oracle's precondition).
        let mut arena = Arena::new(ArenaConfig::default());
        let adoc = ArenaDoc::from_tape(&idoc, &mut arena).expect("morphs");
        prop_assert_eq!(model::from_cursor(adoc.root_value(&arena)), v);
        let frozen = adoc.freeze(&arena).expect("freezes");
        prop_assert_eq!(frozen, plain);
        adoc.free(&mut arena);
        prop_assert_eq!(arena.report().live_bytes, 0u64);
    }

    /// Point lookups agree between the plain and interned forms for both
    /// tabled and non-tabled keys (the 0xA9 fused-scan arm).
    #[test]
    fn lookups_agree_across_forms(v in repeated_key_strategy(), probe in ".{0,12}") {
        let plain = model::encode(&v).expect("encodes");
        let Some(interned) = intern::intern(&plain) else { return Ok(()) };
        let pdoc = TapeDoc::from_bytes(&plain).expect("validates");
        let idoc = TapeDoc::from_bytes(&interned).expect("validates");
        if let (inf_doc::tape::ValueRef::Obj(po), inf_doc::tape::ValueRef::Obj(io)) =
            (pdoc.root(), idoc.root())
        {
            let plain_hit = po.get(probe.as_bytes()).is_some();
            let interned_hit = io.get(probe.as_bytes()).is_some();
            prop_assert_eq!(plain_hit, interned_hit, "lookup parity for {:?}", probe);
            prop_assert_eq!(po.len(), io.len());
        }
    }
}
