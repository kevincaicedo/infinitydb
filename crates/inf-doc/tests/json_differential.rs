//! M3-S05 differential AC: our parser against `serde_json` as a value
//! oracle (plan AC). The oracle is configured to share our models where
//! they can be shared — `preserve_order` (IndexMap: insertion order +
//! first-position/last-value duplicates, the ADR-0036 D5 rule) and
//! `float_roundtrip` (exact f64) — and the residual model differences are
//! the documented **exclusion list**, encoded in `model_of`:
//!
//! - integers outside i64 range: serde keeps u64 exactly; our model is
//!   i64/f64 (ADR-0036 D4) — the conversion applies OUR rule, so the
//!   comparison verifies the fallback arithmetic, not serde's u64.
//! - depth: both cap at 128; generated documents stay far below.
//!
//! f64 comparison is bit-exact (`to_bits`), so `-0.0` and denormals bind.
//! CI runs proptest defaults; the 10⁶-doc AC run is
//! `PROPTEST_CASES=250000 cargo test --release -p inf-doc --test
//! json_differential` (4 properties × 250k = 10⁶ parsed documents),
//! recorded in the ledger.

use proptest::collection::vec;
use proptest::prelude::*;

use inf_doc::model::{self, Value};
use inf_doc::{JsonParser, TapeDoc};

/// Bit-exact structural equality (PartialEq would equate -0.0 and 0.0).
fn strict_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::F64(x), Value::F64(y)) => x.to_bits() == y.to_bits(),
        (Value::Arr(x), Value::Arr(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(a, b)| strict_eq(a, b))
        }
        (Value::Obj(x), Value::Obj(y)) => {
            x.len() == y.len()
                && x.iter().zip(y).all(|((ka, va), (kb, vb))| ka == kb && strict_eq(va, vb))
        }
        _ => a == b,
    }
}

/// serde_json::Value → our model, applying the ADR-0036 D4 number rule
/// (the exclusion list lives exactly here).
fn model_of(v: &serde_json::Value) -> Value {
    match v {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::I64(i)
            } else if let Some(u) = n.as_u64() {
                Value::F64(u as f64) // outside i64 → the D4 fallback
            } else {
                Value::F64(n.as_f64().expect("finite by construction"))
            }
        }
        serde_json::Value::String(s) => Value::Str(s.clone()),
        serde_json::Value::Array(items) => Value::Arr(items.iter().map(model_of).collect()),
        serde_json::Value::Object(entries) => {
            Value::Obj(entries.iter().map(|(k, v)| (k.clone(), model_of(v))).collect())
        }
    }
}

/// Generated serde documents: all scalar kinds, unicode-heavy strings and
/// keys (escapes exercise the unescape path when serde prints them), a
/// small duplicate-prone key pool, and huge-int/edge numbers.
fn serde_value_strategy() -> impl Strategy<Value = serde_json::Value> {
    let key = prop_oneof![
        Just("id".to_string()),
        Just("k".to_string()),
        Just("контроль".to_string()),
        ".{0,12}",
    ];
    let scalar = prop_oneof![
        Just(serde_json::Value::Null),
        any::<bool>().prop_map(serde_json::Value::from),
        any::<i64>().prop_map(serde_json::Value::from),
        any::<u64>().prop_map(serde_json::Value::from), // u64-range integers
        any::<f64>().prop_filter("finite", |f| f.is_finite()).prop_map(serde_json::Value::from),
        // Strings with escapes, control chars, unicode, NULs.
        ".{0,24}".prop_map(serde_json::Value::from),
        Just(serde_json::Value::from("line\nbreak\t\"quoted\" \\slash\u{0}\u{1F30D}")),
    ];
    scalar.prop_recursive(5, 96, 8, move |inner| {
        let key = key.clone();
        prop_oneof![
            vec(inner.clone(), 0..8).prop_map(serde_json::Value::Array),
            vec((key, inner), 0..8)
                .prop_map(|entries| { serde_json::Value::Object(entries.into_iter().collect()) }),
        ]
    })
}

fn parse_ours(text: &[u8]) -> Value {
    let mut p = JsonParser::new();
    let bytes = p.parse(text).unwrap_or_else(|e| panic!("parse failed: {e}"));
    let doc = TapeDoc::from_bytes(&bytes).expect("parser output is canonical idoc");
    model::from_tape(&doc)
}

proptest! {
    /// Compact text: serde's value and ours agree structurally.
    #[test]
    fn agrees_with_serde_on_compact_text(v in serde_value_strategy()) {
        let text = serde_json::to_string(&v).expect("serializes");
        let ours = parse_ours(text.as_bytes());
        let expected = model_of(&v);
        prop_assert!(strict_eq(&ours, &expected), "ours {ours:?} != serde {expected:?} on {text}");
    }

    /// Pretty-printed text (whitespace everywhere): same agreement.
    #[test]
    fn agrees_with_serde_on_pretty_text(v in serde_value_strategy()) {
        let text = serde_json::to_string_pretty(&v).expect("serializes");
        let ours = parse_ours(text.as_bytes());
        let expected = model_of(&v);
        prop_assert!(strict_eq(&ours, &expected), "pretty text disagreement on {text}");
    }

    /// Round-trip through the oracle: reparse serde's print of OUR value.
    #[test]
    fn oracle_reparse_agrees(v in serde_value_strategy()) {
        let text = serde_json::to_string(&v).expect("serializes");
        let reparsed: serde_json::Value = serde_json::from_str(&text).expect("oracle reparses");
        prop_assert!(strict_eq(&model_of(&reparsed), &parse_ours(text.as_bytes())));
    }

    /// Canonical stability: parser output is valid canonical idoc whose
    /// model re-encodes to the identical bytes.
    #[test]
    fn parser_output_is_canonical(v in serde_value_strategy()) {
        let text = serde_json::to_string(&v).expect("serializes");
        let mut p = JsonParser::new();
        let bytes = p.parse(text.as_bytes()).expect("parses");
        let doc = TapeDoc::from_bytes(&bytes).expect("validates");
        let re = model::encode(&model::from_tape(&doc)).expect("re-encodes");
        prop_assert_eq!(re, bytes, "canonical stability on {}", text);
    }
}
