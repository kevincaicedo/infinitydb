//! M3-S06 serializer ACs:
//!
//! - **Fixpoint** (plan AC 1): serialize → parse → serialize is
//!   byte-stable. Proven through the stronger canonical identity
//!   `parse(serialize(tape)) == tape` — if the tape round-trips, the text
//!   fixes in one step. The 10⁶-doc release run is
//!   `PROPTEST_CASES=200000 cargo test --release -p inf-doc --test
//!   json_fixpoint` (5 properties × 200k), recorded in the ledger.
//! - **Form-agnostic canonical bytes** (the E8 comparator requirement):
//!   tape and arena serialize identically.
//! - **serde_json parity**: compact output equals `serde_json::to_string`
//!   and the pretty shape equals `to_string_pretty` under the matching
//!   options — the mechanical pin of the RedisJSON formatter lineage
//!   (both format f64 through the same shortest-round-trip crate).
//!   RedisJSON itself is oracle-diffed at S21 (no JSON module in the
//!   local pinned oracle — the S05 ledger records the probe).
//! - **Number-edge corpus** (plan AC 2): 1e±308, denormals, 17-digit
//!   shortest forms, −0.0, i64 boundaries — pinned bytes, serde-parity
//!   asserted; RedisJSON byte-diff is S21's (`oracle-pending`).

use proptest::collection::vec;
use proptest::prelude::*;

use inf_alloc::arena::{Arena, ArenaConfig};
use inf_doc::arena::ArenaDoc;
use inf_doc::model::{self, Value};
use inf_doc::{JsonParser, SerializeOpts, TapeDoc, serialize_canonical_into, serialize_into};

/// Model documents over the full scalar range (bit-exact f64 included).
fn value_strategy() -> impl Strategy<Value = Value> {
    let key = prop_oneof![
        Just("id".to_string()),
        Just("k".to_string()),
        Just("контроль".to_string()),
        ".{0,12}",
    ];
    let scalar = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(Value::I64),
        any::<f64>().prop_filter("finite", |f| f.is_finite()).prop_map(Value::F64),
        ".{0,24}".prop_map(Value::Str),
        Just(Value::Str("line\nbreak\t\"quoted\" \\slash\u{0}\u{1F}\u{7F}\u{1F30D}".into())),
    ];
    scalar.prop_recursive(5, 96, 8, move |inner| {
        let key = key.clone();
        prop_oneof![
            vec(inner.clone(), 0..8).prop_map(Value::Arr),
            vec((key, inner), 0..8).prop_map(|entries| {
                // Dedup keys (first occurrence wins) so `entries` is
                // already canonical — encode refuses duplicates.
                let mut seen = std::collections::HashSet::new();
                Value::Obj(entries.into_iter().filter(|(k, _)| seen.insert(k.clone())).collect())
            }),
        ]
    })
}

fn serde_of(v: &Value) -> serde_json::Value {
    match v {
        Value::Null => serde_json::Value::Null,
        Value::Bool(b) => serde_json::Value::from(*b),
        Value::I64(i) => serde_json::Value::from(*i),
        Value::F64(f) => serde_json::Value::from(*f),
        Value::Str(s) => serde_json::Value::from(s.as_str()),
        Value::Arr(items) => serde_json::Value::Array(items.iter().map(serde_of).collect()),
        Value::Obj(entries) => serde_json::Value::Object(
            entries.iter().map(|(k, v)| (k.clone(), serde_of(v))).collect(),
        ),
    }
}

fn tape_of(v: &Value) -> Vec<u8> {
    model::encode(v).expect("generated values encode")
}

fn canonical_text(bytes: &[u8]) -> Vec<u8> {
    let doc = TapeDoc::from_bytes(bytes).expect("validates");
    let mut out = Vec::new();
    serialize_canonical_into(doc.root().into(), &mut out);
    out
}

proptest! {
    /// The canonical identity: tape → text → tape reproduces the bytes
    /// (implies the AC's fixpoint: one serialize step reaches the fixed
    /// point). Bit-exact f64 by the shortest-round-trip argument.
    #[test]
    fn text_round_trip_reproduces_canonical_tape(v in value_strategy()) {
        let bytes = tape_of(&v);
        let text = canonical_text(&bytes);
        let mut p = JsonParser::new();
        let reparsed = p.parse(&text).expect("serialized text parses");
        prop_assert_eq!(reparsed, bytes, "text: {}", String::from_utf8_lossy(&text));
    }

    /// The AC as literally written: serialize → parse → serialize is
    /// byte-stable.
    #[test]
    fn serialize_parse_serialize_is_byte_stable(v in value_strategy()) {
        let text = canonical_text(&tape_of(&v));
        let mut p = JsonParser::new();
        let reparsed = p.parse(&text).expect("parses");
        let text2 = canonical_text(&reparsed);
        prop_assert_eq!(&text, &text2);
    }

    /// Physical form never affects canonical bytes (the E8 comparator
    /// stands on this): arena serialization ≡ tape serialization.
    #[test]
    fn arena_and_tape_serialize_identically(v in value_strategy()) {
        let bytes = tape_of(&v);
        let tape_text = canonical_text(&bytes);
        let doc = TapeDoc::from_bytes(&bytes).expect("validates");
        let mut arena = Arena::new(ArenaConfig::default());
        let morphed = ArenaDoc::from_tape(&doc, &mut arena).expect("morphs");
        let mut arena_text = Vec::new();
        serialize_canonical_into(morphed.root_value(&arena), &mut arena_text);
        prop_assert_eq!(tape_text, arena_text);
    }

    /// Compact output is byte-identical to serde_json (the formatter
    /// lineage pin — escaping, number text, separators).
    #[test]
    fn compact_matches_serde_json(v in value_strategy()) {
        let ours = canonical_text(&tape_of(&v));
        let serde_text = serde_json::to_string(&serde_of(&v)).expect("serializes");
        prop_assert_eq!(
            String::from_utf8(ours).expect("valid UTF-8"),
            serde_text
        );
    }

    /// Pretty shape under INDENT "  " / NEWLINE "\n" / SPACE " " is
    /// byte-identical to serde_json::to_string_pretty.
    #[test]
    fn pretty_matches_serde_json_pretty(v in value_strategy()) {
        let bytes = tape_of(&v);
        let doc = TapeDoc::from_bytes(&bytes).expect("validates");
        let opts = SerializeOpts { indent: b"  ", newline: b"\n", space: b" " };
        let mut ours = Vec::new();
        serialize_into(doc.root().into(), &opts, &mut ours);
        let serde_text = serde_json::to_string_pretty(&serde_of(&v)).expect("serializes");
        prop_assert_eq!(
            String::from_utf8(ours).expect("valid UTF-8"),
            serde_text
        );
    }
}

/// Number-edge corpus (plan AC): pinned via serde parity now; RedisJSON
/// byte-diff at S21 (`oracle-pending` — the local pinned oracle ships
/// without the JSON module; ledger records the probe).
#[test]
fn number_edge_corpus_matches_serde() {
    let edges: &[Value] = &[
        Value::F64(1e308),
        Value::F64(-1e308),
        Value::F64(f64::MAX),
        Value::F64(f64::MIN_POSITIVE),
        Value::F64(5e-324), // smallest denormal
        Value::F64(-0.0),
        Value::F64(0.1),
        Value::F64(0.30000000000000004),
        Value::F64(1.7976931348623157e308),
        Value::F64(2.2250738585072014e-308),
        Value::F64(123456789.12345679),
        Value::F64(1e15),
        Value::F64(1e16),
        Value::F64(1e21),
        Value::F64(1e22),
        Value::F64(9.007199254740992e15), // 2^53
        Value::I64(i64::MAX),
        Value::I64(i64::MIN),
        Value::I64(0),
        Value::I64(-1),
    ];
    for v in edges {
        let ours = canonical_text(&tape_of(v));
        let serde_text = serde_json::to_string(&serde_of(v)).expect("serializes");
        assert_eq!(String::from_utf8(ours).expect("valid UTF-8"), serde_text, "edge value {v:?}");
    }
}

/// Empty containers print with nothing inside, in both modes (the
/// serde_json pretty rule RedisJSON inherits).
#[test]
fn empty_containers_print_compact() {
    for (v, expected) in [(Value::Obj(vec![]), "{}"), (Value::Arr(vec![]), "[]")] {
        let bytes = tape_of(&v);
        assert_eq!(canonical_text(&bytes), expected.as_bytes());
        let doc = TapeDoc::from_bytes(&bytes).expect("validates");
        let opts = SerializeOpts { indent: b"\t", newline: b"\n", space: b" " };
        let mut pretty = Vec::new();
        serialize_into(doc.root().into(), &opts, &mut pretty);
        assert_eq!(pretty, expected.as_bytes());
    }
}

/// Formatting options are arbitrary byte strings (RedisJSON semantics) —
/// a non-whitespace INDENT must land in the output verbatim.
#[test]
fn options_are_arbitrary_bytes() {
    let v = Value::Obj(vec![("a".into(), Value::Arr(vec![Value::I64(1), Value::I64(2)]))]);
    let bytes = tape_of(&v);
    let doc = TapeDoc::from_bytes(&bytes).expect("validates");
    let opts = SerializeOpts { indent: b"><", newline: b"|", space: b"_" };
    let mut out = Vec::new();
    serialize_into(doc.root().into(), &opts, &mut out);
    assert_eq!(String::from_utf8(out).expect("valid UTF-8"), "{|><\"a\":_[|><><1,|><><2|><]|}");
}

/// The serializer appends — it never disturbs bytes already in the buffer
/// (the RESP reserve/back-patch contract depends on this).
#[test]
fn serializer_appends_to_existing_buffer() {
    let bytes = tape_of(&Value::I64(42));
    let doc = TapeDoc::from_bytes(&bytes).expect("validates");
    let mut out = b"$XXXXXXXX\r\n".to_vec();
    let reserved = out.len();
    serialize_canonical_into(doc.root().into(), &mut out);
    assert_eq!(&out[..reserved], b"$XXXXXXXX\r\n");
    assert_eq!(&out[reserved..], b"42");
}
