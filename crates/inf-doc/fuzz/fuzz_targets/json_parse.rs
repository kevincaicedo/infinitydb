//! JSON parser fuzz (M3-S05/S06/S07, L9): arbitrary bytes never panic;
//! accepted documents produce canonical idoc (validator agrees, model
//! re-encodes byte-identically); the S06 serializer closes the loop
//! (canonical text reparses to the identical tape — the fixpoint oracle
//! under fuzz); an S07 tight-limits parser must reject-or-agree with
//! memory bounded by its caps; and wherever serde_json also accepts the
//! input, the two parsers agree on the value (bit-exact f64) under our
//! number model. Verdict parity is deliberately NOT asserted —
//! depth-limit off-by-ones and huge-number handling differ by design;
//! value parity on the mutual accept set is the differential AC's fuzz
//! form.

#![no_main]

use libfuzzer_sys::fuzz_target;

use inf_doc::model::{self, Value};
use inf_doc::{JsonParser, ParseLimits, TapeDoc};

/// S07 arm: hostile caps far below the defaults.
const TIGHT: ParseLimits = ParseLimits { max_depth: 8, max_text: 1 << 20, max_body: 256 };

/// The tight parser either agrees byte-for-byte or rejects with its held
/// memory bounded by the caps — never a panic, never unbounded growth.
fn tight_limits_arm(data: &[u8], accepted: Option<&[u8]>) {
    let mut tight = JsonParser::with_limits(TIGHT);
    let mut out = Vec::new();
    match tight.parse_into(data, &mut out) {
        Ok(()) => {
            let full = accepted.expect("tight limits accept ⊆ default limits accept");
            assert_eq!(out, full, "limits must not change accepted bytes");
        }
        Err(_) => {
            assert!(
                out.len() <= inf_doc::HEADER_LEN + TIGHT.max_body + 16,
                "rejection left an over-cap output ({} bytes)",
                out.len()
            );
        }
    }
}

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

fn model_of(v: &serde_json::Value) -> Value {
    match v {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::I64(i)
            } else if let Some(u) = n.as_u64() {
                Value::F64(u as f64)
            } else {
                Value::F64(n.as_f64().expect("serde numbers are finite"))
            }
        }
        serde_json::Value::String(s) => Value::Str(s.clone()),
        serde_json::Value::Array(items) => Value::Arr(items.iter().map(model_of).collect()),
        serde_json::Value::Object(entries) => {
            Value::Obj(entries.iter().map(|(k, v)| (k.clone(), model_of(v))).collect())
        }
    }
}

fuzz_target!(|data: &[u8]| {
    let mut parser = JsonParser::new();
    let Ok(bytes) = parser.parse(data) else {
        // Rejection is a fine outcome; panics are the bug — in the
        // default parser and in the tight-limits one alike.
        tight_limits_arm(data, None);
        return;
    };
    let doc = TapeDoc::from_bytes(&bytes).expect("parser output validates as canonical idoc");
    let ours = model::from_tape(&doc);
    let re = model::encode(&ours).expect("model re-encodes");
    assert_eq!(re, bytes, "canonical stability: parse output re-encodes identically");
    // S06 fixpoint oracle: canonical text reparses to the identical tape.
    let mut text = Vec::new();
    inf_doc::serialize_canonical_into(doc.root().into(), &mut text);
    let reparsed = parser.parse(&text).expect("canonical text reparses");
    assert_eq!(reparsed, bytes, "serialize→parse fixpoint diverged");
    tight_limits_arm(data, Some(&bytes));
    if let Ok(serde_value) = serde_json::from_slice::<serde_json::Value>(data) {
        assert!(
            strict_eq(&ours, &model_of(&serde_value)),
            "value divergence from serde_json on mutually-accepted input"
        );
    }
});
