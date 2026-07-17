//! S02 density budgets (ADR-0036 D9): idoc ≤ 1.15× msgpack and ≤ 0.85×
//! JSON text, asserted on the six seeded M3-S20 reference-corpus shapes.
//!
//! The msgpack comparator is computed arithmetically (sizes, not a codec)
//! so the measurement instrument shares no code or dependency with the
//! system under test (the inf-compare rule at unit scale). Run with
//! `--nocapture` to see the per-shape table; the *aggregate* governs (the
//! gate wording is "on the reference corpus"), per-shape rows keep the
//! worst case visible.

use inf_doc::model::{self, Value};
use inf_doc::{JsonParser, TapeDoc};

#[allow(dead_code, unused_imports)] // shared generator also contains its CLI and witness tests
#[path = "../../../bins/inf-bench/src/doc_corpus.rs"]
mod doc_corpus;

/// msgpack encoded size, computed per the spec (minimal-width forms).
fn msgpack_size(v: &Value) -> usize {
    match v {
        Value::Null | Value::Bool(_) => 1,
        Value::I64(n) => match *n {
            -32..=127 => 1,
            128..=255 => 2,
            256..=65_535 => 3,
            65_536..=4_294_967_295 => 5,
            n if n < 0 && n >= i8::MIN as i64 => 2,
            n if n < 0 && n >= i16::MIN as i64 => 3,
            n if n < 0 && n >= i32::MIN as i64 => 5,
            _ => 9,
        },
        Value::F64(_) => 9,
        Value::Str(s) => {
            let l = s.len();
            l + if l <= 31 {
                1
            } else if l <= 255 {
                2
            } else if l <= 65_535 {
                3
            } else {
                5
            }
        }
        Value::Arr(items) => {
            let hdr = if items.len() <= 15 {
                1
            } else if items.len() <= 65_535 {
                3
            } else {
                5
            };
            hdr + items.iter().map(msgpack_size).sum::<usize>()
        }
        Value::Obj(entries) => {
            let hdr = if entries.len() <= 15 {
                1
            } else if entries.len() <= 65_535 {
                3
            } else {
                5
            };
            hdr + entries
                .iter()
                .map(|(k, val)| msgpack_size(&Value::Str(k.clone())) + msgpack_size(val))
                .sum::<usize>()
        }
    }
}

#[test]
fn density_budgets_hold_on_the_reference_corpus() {
    let mut total_idoc = 0usize;
    let mut total_msgpack = 0usize;
    let mut total_text = 0usize;
    println!(
        "{:<12} {:>8} {:>8} {:>8} {:>12} {:>10}",
        "shape", "idoc", "msgpack", "text", "vs msgpack", "vs text"
    );
    let mut parser = JsonParser::new();
    for corpus_doc in doc_corpus::generate(doc_corpus::CANONICAL_SEED) {
        let name = corpus_doc.name;
        let bytes = parser.parse(corpus_doc.json.as_bytes()).expect("reference corpus parses");
        let tape = TapeDoc::from_bytes(&bytes).expect("parser emits valid idoc");
        let value = model::from_tape(&tape);
        let idoc = model::encode(&value).expect("encodes").len();
        let msgpack = msgpack_size(&value);
        let text = corpus_doc.json.len();
        println!(
            "{:<12} {:>8} {:>8} {:>8} {:>11.3}x {:>9.3}x",
            name,
            idoc,
            msgpack,
            text,
            idoc as f64 / msgpack as f64,
            idoc as f64 / text as f64
        );
        total_idoc += idoc;
        total_msgpack += msgpack;
        total_text += text;
    }
    let vs_msgpack = total_idoc as f64 / total_msgpack as f64;
    let vs_text = total_idoc as f64 / total_text as f64;
    println!("aggregate: {vs_msgpack:.3}x msgpack, {vs_text:.3}x text");
    assert!(vs_msgpack <= 1.15, "idoc {vs_msgpack:.3}x msgpack exceeds the 1.15x budget");
    assert!(vs_text <= 0.85, "idoc {vs_text:.3}x JSON text exceeds the 0.85x budget");
}
