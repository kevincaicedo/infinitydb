//! S02 round-trip + accounting properties (plan AC; ADR-0036 D6/D8).
//!
//! - encode → validate → decode ≡ identity (tape form)
//! - morph → cursor-decode ≡ identity (arena form)
//! - `freeze(morph(encode(v))) == encode(v)` byte-exact — the canonical-
//!   stability contract checkpoints and M4 demotion stand on
//! - accounting reconciles to zero after free (leak proof)
//!
//! CI runs proptest's default case count; the 10⁶-doc AC run is executed
//! explicitly with `PROPTEST_CASES=1000000 cargo test --release -p inf-doc
//! --test roundtrip` and recorded in the ledger.

use proptest::collection::vec;
use proptest::prelude::*;

use inf_alloc::arena::{Arena, ArenaConfig};
use inf_doc::model::{self, Value};
use inf_doc::{ArenaDoc, TapeDoc};

/// Generated documents: bounded depth/width, all scalar kinds, unicode +
/// NUL-bearing strings, f64 filtered finite (NaN/Inf unrepresentable —
/// ADR-0036 D4), duplicate keys representable (D5).
fn value_strategy() -> impl Strategy<Value = Value> {
    let scalar = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(Value::I64),
        any::<f64>().prop_filter("finite only (D4)", |f| f.is_finite()).prop_map(Value::F64),
        ".{0,40}".prop_map(Value::Str),
        // Length-boundary strings: fixstr/str8/str24 edges.
        prop_oneof![Just(31usize), Just(32), Just(255), Just(256)]
            .prop_map(|n| Value::Str("x".repeat(n))),
    ];
    scalar.prop_recursive(5, 96, 8, |inner| {
        prop_oneof![
            vec(inner.clone(), 0..8).prop_map(Value::Arr),
            vec((".{0,12}", inner), 0..8)
                .prop_map(|entries| Value::Obj(entries.into_iter().collect())),
        ]
    })
}

proptest! {
    #[test]
    fn tape_round_trip_is_identity(v in value_strategy()) {
        let bytes = model::encode(&v).expect("generated values encode");
        let doc = TapeDoc::from_bytes(&bytes).expect("builder output validates");
        prop_assert_eq!(model::from_tape(&doc), v);
    }

    #[test]
    fn arena_round_trip_and_freeze_stability(v in value_strategy()) {
        let bytes = model::encode(&v).expect("generated values encode");
        let doc = TapeDoc::from_bytes(&bytes).expect("validates");
        let mut arena = Arena::new(ArenaConfig::default());
        let baseline = arena.report().live_bytes as usize;
        let adoc = ArenaDoc::from_tape(&doc, &mut arena).expect("morphs");
        // The arena cursors see the same document.
        prop_assert_eq!(model::from_cursor(adoc.root_value(&arena)), v.clone());
        // Canonical stability: freezing the projection reproduces the
        // durable bytes exactly (D8 — the M4 demotion contract).
        let frozen = adoc.freeze(&arena).expect("freezes");
        prop_assert_eq!(&frozen[..], &bytes[..]);
        // Zero slack at morph; accounting matches the arena's own books.
        let report = adoc.report();
        prop_assert_eq!(report.slack_bytes, 0);
        prop_assert_eq!(arena.report().live_bytes as usize - baseline, report.node_bytes);
        // Free reconciles to zero (asserted inside free()) and returns
        // every byte to the arena.
        adoc.free(&mut arena);
        prop_assert_eq!(arena.report().live_bytes as usize, baseline);
    }

    /// Fragments (DocDelta operands, §3.4 R6): bare canonical value bytes
    /// == the full document's body.
    #[test]
    fn fragment_is_the_headerless_body(v in value_strategy()) {
        let full = model::encode(&v).expect("encodes");
        let fragment = model::encode_fragment(&v).expect("encodes");
        prop_assert_eq!(&full[inf_doc::HEADER_LEN..], &fragment[..]);
    }
}

/// Growth slack stays within the D7 bound: pushing N elements one at a
/// time never leaves slack above 25% of the array's slot bytes (+ the +4
/// floor for tiny arrays).
#[test]
fn push_growth_slack_is_bounded() {
    let mut arena = Arena::new(ArenaConfig::default());
    let seed = model::encode(&Value::Arr(vec![])).expect("encodes");
    let doc = TapeDoc::from_bytes(&seed).expect("validates");
    let mut adoc = ArenaDoc::from_tape(&doc, &mut arena).expect("morphs");
    let mut arr = adoc.root_ref();
    for i in 0..10_000i64 {
        let v = adoc.alloc_i64(&mut arena, i).expect("alloc");
        arr = adoc.arr_push(&mut arena, arr, v).expect("push");
        let report = adoc.report();
        // cap grows by max(cap/4, 4): slack ≤ count/4 slots + 4-slot floor.
        let count = (i + 1) as usize;
        let slot = 8;
        assert!(
            report.slack_bytes <= (count / 4 + 4) * slot,
            "slack {} exceeds bound at count {count}",
            report.slack_bytes
        );
    }
    // The projection still freezes to a valid canonical tape.
    let frozen = adoc.freeze(&arena).expect("freezes");
    let redoc = TapeDoc::from_bytes(&frozen).expect("frozen output validates");
    let Value::Arr(items) = model::from_tape(&redoc) else { panic!("array root") };
    assert_eq!(items.len(), 10_000);
    adoc.free(&mut arena);
    assert_eq!(arena.report().live_bytes, 0u64);
}

/// Arena exhaustion mid-morph aborts leak-free (ADR-0036 D8): with a
/// budget too small for the document, morph fails typed and the arena
/// returns to its baseline.
#[test]
fn morph_abort_is_leak_free() {
    // Sized so the morph makes real progress (frames holding refs) before
    // the budget bites — the abort path being tested is the frame cleanup.
    let big = Value::Arr((0..8000).map(|i| Value::Str(format!("string-{i:04}"))).collect());
    let bytes = model::encode(&big).expect("encodes");
    let doc = TapeDoc::from_bytes(&bytes).expect("validates");
    let mut arena = Arena::new(ArenaConfig { chunk_size: 64 << 10, max_resident: Some(64 << 10) });
    let err = ArenaDoc::from_tape(&doc, &mut arena).expect_err("budget too small");
    assert_eq!(err, inf_doc::DocError::ArenaExhausted);
    assert_eq!(arena.report().live_bytes, 0u64, "abort released every allocation");
}
