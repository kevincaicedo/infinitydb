//! M3-S05 number/UTF-8/grammar edge corpus — the classic traps, each
//! pinned by test. Decisions follow ADR-0036 D4/D5 and the
//! serde_json/RedisJSON lineage; rows marked `oracle-pending` re-verify
//! against the dockerized RedisJSON oracle at S21 (the local pinned
//! redis 8.0.5 ships without the JSON module — S05 ledger).

use inf_doc::apply::Number;
use inf_doc::model::{self, Value};
use inf_doc::{JsonErrorKind, JsonParser, ParseLimits, TapeDoc, parse_number_token};

fn parse(text: &str) -> Result<Value, (usize, JsonErrorKind)> {
    let mut p = JsonParser::new();
    match p.parse(text.as_bytes()) {
        Ok(bytes) => {
            let doc = TapeDoc::from_bytes(&bytes).expect("parser output is canonical idoc");
            Ok(model::from_tape(&doc))
        }
        Err(e) => Err((e.offset, e.kind)),
    }
}

fn value(text: &str) -> Value {
    parse(text).unwrap_or_else(|e| panic!("{text:?} must parse, got {e:?}"))
}

fn kind(text: &str) -> JsonErrorKind {
    parse(text).map(|v| panic!("{text:?} must reject, got {v:?}")).unwrap_err().1
}

#[test]
fn integer_boundaries() {
    assert_eq!(value("0"), Value::I64(0));
    assert_eq!(value("9223372036854775807"), Value::I64(i64::MAX));
    assert_eq!(value("-9223372036854775808"), Value::I64(i64::MIN));
    // Outside i64: the ADR-0036 D4 f64 fallback (oracle-pending: RedisJSON
    // may keep u64-range integers exact — candidate S21 deviation).
    assert_eq!(value("9223372036854775808"), Value::F64(9.223372036854776e18));
    assert_eq!(value("-9223372036854775809"), Value::F64(-9.223372036854776e18));
    assert_eq!(value("123456789012345678901234567890"), Value::F64(1.2345678901234568e29));
}

#[test]
fn negative_zero_and_float_forms() {
    // serde_json lineage: "-0" keeps its sign as f64 (oracle-pending).
    let Value::F64(v) = value("-0") else { panic!("-0 is f64") };
    assert_eq!(v.to_bits(), (-0.0f64).to_bits());
    let Value::F64(v) = value("-0.0") else { panic!("-0.0 is f64") };
    assert_eq!(v.to_bits(), (-0.0f64).to_bits());
    assert_eq!(value("0.0"), Value::F64(0.0));
    assert_eq!(value("1e308"), Value::F64(1e308));
    assert_eq!(value("5e-324"), Value::F64(5e-324)); // smallest denormal
    assert_eq!(value("1e-400"), Value::F64(0.0)); // underflow → zero, like the oracle
    let Value::F64(subnormal_edge) = value("2.2250738585072011e-308") else {
        panic!("subnormal edge is f64")
    };
    assert_eq!(
        subnormal_edge.to_bits(),
        "2.2250738585072011e-308".parse::<f64>().expect("parses").to_bits()
    );
    // 17-significant-digit round-trip exactness (Eisel–Lemire).
    assert_eq!(value("1.7976931348623157e308"), Value::F64(f64::MAX));
}

#[test]
fn number_rejections() {
    assert_eq!(kind("1e400"), JsonErrorKind::NumberOutOfRange);
    assert_eq!(kind("-1e400"), JsonErrorKind::NumberOutOfRange);
    assert_eq!(kind("01"), JsonErrorKind::InvalidNumber);
    assert_eq!(kind("-01"), JsonErrorKind::InvalidNumber);
    assert_eq!(kind("+1"), JsonErrorKind::UnexpectedCharacter(b'+'));
    assert_eq!(kind("1."), JsonErrorKind::InvalidNumber);
    assert_eq!(kind(".5"), JsonErrorKind::UnexpectedCharacter(b'.'));
    assert_eq!(kind("1e"), JsonErrorKind::InvalidNumber);
    assert_eq!(kind("1e+"), JsonErrorKind::InvalidNumber);
    assert_eq!(kind("-"), JsonErrorKind::InvalidNumber);
    assert_eq!(kind("1a"), JsonErrorKind::UnexpectedCharacter(b'a'));
    assert_eq!(kind("NaN"), JsonErrorKind::UnexpectedCharacter(b'N'));
    assert_eq!(kind("Infinity"), JsonErrorKind::UnexpectedCharacter(b'I'));
}

#[test]
fn standalone_number_token_uses_the_ingest_grammar() {
    assert_eq!(parse_number_token(b" \t42\r\n").unwrap(), Number::I64(42));
    let Number::F64(negative_zero) = parse_number_token(b"-0").unwrap() else {
        panic!("negative zero stays a float")
    };
    assert_eq!(negative_zero.to_bits(), (-0.0f64).to_bits());

    let trailing = parse_number_token(b"1 false").expect_err("a second token rejects");
    assert_eq!(trailing.offset, 1);
    assert_eq!(trailing.kind, JsonErrorKind::UnexpectedCharacter(b' '));
    assert_eq!(
        parse_number_token(b" 01 ").expect_err("leading zero rejects").kind,
        JsonErrorKind::InvalidNumber
    );
}

#[test]
fn literals_and_their_terminators() {
    assert_eq!(value("true"), Value::Bool(true));
    assert_eq!(value("false"), Value::Bool(false));
    assert_eq!(value("null"), Value::Null);
    assert_eq!(
        value("[true,false,null]"),
        Value::Arr(vec![Value::Bool(true), Value::Bool(false), Value::Null,])
    );
    assert_eq!(kind("truee"), JsonErrorKind::UnexpectedCharacter(b'e'));
    assert_eq!(kind("nul"), JsonErrorKind::UnexpectedCharacter(b'n'));
    assert_eq!(kind("tRue"), JsonErrorKind::UnexpectedCharacter(b't'));
}

#[test]
fn unicode_escapes_and_surrogates() {
    assert_eq!(value(r#""A""#), Value::Str("A".into()));
    assert_eq!(value(r#""\u0000""#), Value::Str("\u{0}".into())); // NUL via escape
    assert_eq!(value(r#""😀""#), Value::Str("😀".into())); // surrogate pair
    assert_eq!(value(r#""é café""#), Value::Str("é café".into()));
    // Lone / out-of-order surrogates reject (serde lineage; oracle-pending).
    assert_eq!(kind(r#""\uD800""#), JsonErrorKind::LoneSurrogate);
    assert_eq!(kind(r#""\uDC00""#), JsonErrorKind::LoneSurrogate);
    assert_eq!(kind(r#""\uD800A""#), JsonErrorKind::LoneSurrogate);
    assert_eq!(kind(r#""\uD800\uD800""#), JsonErrorKind::LoneSurrogate);
    assert_eq!(kind(r#""\uZZZZ""#), JsonErrorKind::InvalidUnicodeEscape);
    assert_eq!(kind(r#""\u00""#), JsonErrorKind::InvalidUnicodeEscape);
}

#[test]
fn string_content_edges() {
    assert_eq!(value(r#""héllo 🌍 ключ 键""#), Value::Str("héllo 🌍 ключ 键".into()));
    assert_eq!(value(r#""\"\\\/\b\f\n\r\t""#), Value::Str("\"\\/\u{8}\u{c}\n\r\t".into()));
    assert_eq!(kind(r#""\x41""#), JsonErrorKind::InvalidEscape);
    assert_eq!(kind("\"a\u{0}b\""), JsonErrorKind::ControlCharacter); // raw NUL
    assert_eq!(kind("\"a\tb\""), JsonErrorKind::ControlCharacter); // raw tab
    assert_eq!(kind("\"never closed"), JsonErrorKind::UnterminatedString);
    // Invalid UTF-8 inside a string.
    let mut bad = Vec::from(&b"\"ab"[..]);
    bad.push(0xFF);
    bad.extend_from_slice(b"\"");
    let e = JsonParser::new().parse(&bad).unwrap_err();
    assert_eq!(e.kind, JsonErrorKind::InvalidUtf8);
    // DEL (0x7F) is NOT a control char per JSON.
    assert_eq!(value("\"a\u{7f}b\""), Value::Str("a\u{7f}b".into()));
}

#[test]
fn duplicate_keys_last_value_first_position() {
    // The ADR-0036 D5 rule (IndexMap semantics; oracle-pending at S21).
    assert_eq!(
        value(r#"{"a":1,"b":2,"a":3}"#),
        Value::Obj(vec![("a".into(), Value::I64(3)), ("b".into(), Value::I64(2))])
    );
    assert_eq!(
        value(r#"{"a":{"x":1},"a":[2],"a":3}"#),
        Value::Obj(vec![("a".into(), Value::I64(3))])
    );
    // Escaped and raw spellings of the same key are the same key.
    assert_eq!(value(r#"{"key":1,"key":2}"#), Value::Obj(vec![("key".into(), Value::I64(2))]));
    // Nested objects dedup independently.
    assert_eq!(
        value(r#"{"o":{"a":1,"a":2},"a":9}"#),
        Value::Obj(vec![
            ("o".into(), Value::Obj(vec![("a".into(), Value::I64(2))])),
            ("a".into(), Value::I64(9)),
        ])
    );
}

#[test]
fn wide_object_duplicates_take_the_sorted_path() {
    // Above LINEAR_SCAN_MAX (256) detection defers to the close-time sort.
    let mut text = String::from("{");
    for i in 0..400 {
        text.push_str(&format!("\"k{i:03}\":{i},"));
    }
    text.push_str("\"k007\":-1}");
    let Value::Obj(entries) = value(&text) else { panic!("object") };
    assert_eq!(entries.len(), 400);
    assert_eq!(entries[7], ("k007".to_string(), Value::I64(-1)), "last value, first position");
    assert_eq!(entries[8].0, "k008", "order otherwise untouched");
}

#[test]
fn grammar_and_structure() {
    assert_eq!(value("{}"), Value::Obj(vec![]));
    assert_eq!(value("[]"), Value::Arr(vec![]));
    assert_eq!(value(" [ 1 , 2 ] "), Value::Arr(vec![Value::I64(1), Value::I64(2)]));
    assert_eq!(value("\"root scalar\""), Value::Str("root scalar".into())); // RedisJSON allows
    assert_eq!(kind(""), JsonErrorKind::UnexpectedEnd);
    assert_eq!(kind("   "), JsonErrorKind::UnexpectedEnd);
    assert_eq!(kind("[1,2"), JsonErrorKind::UnexpectedEnd);
    assert_eq!(kind("[1,]"), JsonErrorKind::UnexpectedCharacter(b']'));
    assert_eq!(kind("{,}"), JsonErrorKind::UnexpectedCharacter(b','));
    assert_eq!(kind("{\"a\"}"), JsonErrorKind::UnexpectedCharacter(b'}'));
    assert_eq!(kind("{\"a\":}"), JsonErrorKind::UnexpectedCharacter(b'}'));
    assert_eq!(kind("{\"a\":1,}"), JsonErrorKind::UnexpectedCharacter(b'}'));
    assert_eq!(kind("{1:2}"), JsonErrorKind::UnexpectedCharacter(b'1'));
    assert_eq!(kind("[1}"), JsonErrorKind::UnexpectedCharacter(b'}'));
    assert_eq!(kind("1 2"), JsonErrorKind::TrailingCharacters);
    assert_eq!(kind("{} {}"), JsonErrorKind::TrailingCharacters);
    assert_eq!(kind("\u{FEFF}1"), JsonErrorKind::UnexpectedCharacter(0xEF)); // BOM rejects
    assert_eq!(kind("\\"), JsonErrorKind::UnexpectedCharacter(b'\\'));
}

#[test]
fn depth_cap_binds_at_129() {
    let ok = format!("{}1{}", "[".repeat(128), "]".repeat(128));
    assert!(parse(&ok).is_ok(), "depth 128 parses");
    let too_deep = format!("{}1{}", "[".repeat(129), "]".repeat(129));
    assert_eq!(kind(&too_deep), JsonErrorKind::DepthExceeded);
}

#[test]
fn size_cap_rejects_incrementally() {
    let mut p = JsonParser::with_limits(ParseLimits { max_body: 64, ..ParseLimits::default() });
    let text = format!("[{}]", (0..64).map(|i| i.to_string()).collect::<Vec<_>>().join(","));
    let e = p.parse(text.as_bytes()).unwrap_err();
    assert_eq!(e.kind, JsonErrorKind::DocumentTooLarge);
}

#[test]
fn error_offsets_are_exact() {
    let e = JsonParser::new().parse(b"[1, x]").unwrap_err();
    assert_eq!((e.offset, e.kind), (4, JsonErrorKind::UnexpectedCharacter(b'x')));
    let e = JsonParser::new().parse(b"{\"a\": 01}").unwrap_err();
    assert_eq!((e.offset, e.kind), (6, JsonErrorKind::InvalidNumber));
    let display = format!("{e}");
    assert_eq!(display, "invalid number at offset 6");
}
