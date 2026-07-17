//! S01 golden vectors: committed `.idoc` files under `tests/golden/` —
//! decoding them is a CI test forever (plan AC). A format change that
//! alters any byte here is an ADR event, not a refactor.
//!
//! Regenerate deliberately with `GOLDEN_REGEN=1 cargo test -p inf-doc
//! --test golden` and review the diff like the wire format it is.

use std::path::PathBuf;

use inf_doc::model::{self, Value};
use inf_doc::{DocValue, TapeDoc};

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

/// The vector table. Names are file stems; values are constructed in code
/// (the committed bytes are the contract, the constructors document them).
fn vectors() -> Vec<(&'static str, Value)> {
    vec![
        ("scalar_null", Value::Null),
        ("scalar_true", Value::Bool(true)),
        ("scalar_false", Value::Bool(false)),
        // fixint boundaries and their varint neighbors.
        ("int_fixmin", Value::I64(-32)),
        ("int_fixmax", Value::I64(127)),
        ("int_below_fix", Value::I64(-33)),
        ("int_above_fix", Value::I64(128)),
        ("int_min", Value::I64(i64::MIN)),
        ("int_max", Value::I64(i64::MAX)),
        // f64: bit-exactness incl. -0.0 (D4).
        ("f64_pi", Value::F64(core::f64::consts::PI)),
        ("f64_neg_zero", Value::F64(-0.0)),
        ("f64_1e308", Value::F64(1e308)),
        // String width boundaries (fixstr/str8/str24) + content edges.
        ("str_empty", Value::Str(String::new())),
        ("str_len31", Value::Str("x".repeat(31))),
        ("str_len32", Value::Str("x".repeat(32))),
        ("str_len255", Value::Str("x".repeat(255))),
        ("str_len256", Value::Str("x".repeat(256))),
        ("str_unicode", Value::Str("héllo \u{1F30D} — ключ 键".into())),
        ("str_nul_byte", Value::Str("a\u{0}b".into())),
        ("arr_empty", Value::Arr(vec![])),
        ("obj_empty", Value::Obj(vec![])),
        // Key insertion order is durable contract (D5): z before a.
        (
            "key_order",
            Value::Obj(vec![("zeta".into(), Value::I64(1)), ("alpha".into(), Value::I64(2))]),
        ),
        // Duplicate keys: representable, order-preserved, get() = first
        // match (D5 pinning — see the assertion below).
        ("dup_keys", Value::Obj(vec![("k".into(), Value::I64(1)), ("k".into(), Value::I64(2))])),
        // The ADR-0036 D9 worked example.
        (
            "adr_example",
            Value::Obj(vec![
                ("name".into(), Value::Str("Lens".into())),
                ("price".into(), Value::I64(4999)),
                ("tags".into(), Value::Arr(vec![Value::Str("optics".into())])),
            ]),
        ),
        // Mixed nesting exercising every tag in one vector.
        (
            "nested_mixed",
            Value::Obj(vec![
                ("s".into(), Value::Str("value".into())),
                ("n".into(), Value::Null),
                ("b".into(), Value::Bool(false)),
                ("i".into(), Value::I64(-1_000_000)),
                ("f".into(), Value::F64(2.5)),
                (
                    "a".into(),
                    Value::Arr(vec![
                        Value::I64(0),
                        Value::Obj(vec![("deep".into(), Value::Arr(vec![Value::Null]))]),
                    ]),
                ),
            ]),
        ),
        ("deep_16", {
            let mut v: Value = Value::I64(42);
            for _ in 0..16 {
                v = Value::Obj(vec![("d".into(), v)]);
            }
            v
        }),
    ]
}

#[test]
fn golden_vectors_decode_forever() {
    let dir = golden_dir();
    let regen = std::env::var_os("GOLDEN_REGEN").is_some();
    if regen {
        std::fs::create_dir_all(&dir).expect("create golden dir");
    }
    for (name, value) in vectors() {
        let encoded = model::encode(&value).expect("vector encodes");
        let path = dir.join(format!("{name}.idoc"));
        if regen {
            std::fs::write(&path, &encoded).expect("write golden");
            continue;
        }
        let committed = std::fs::read(&path)
            .unwrap_or_else(|e| panic!("missing golden {name}: {e} (GOLDEN_REGEN=1 to create)"));
        // Encoding is frozen: today's encoder reproduces the committed
        // bytes, and the committed bytes decode to the constructor value.
        assert_eq!(committed, encoded, "golden {name}: encoder drifted from committed bytes");
        let doc = TapeDoc::from_bytes(&committed)
            .unwrap_or_else(|e| panic!("golden {name} fails validation: {e}"));
        assert_eq!(model::from_tape(&doc), value, "golden {name}: decode drifted");
    }
}

/// ADR-0038 golden vectors: the interned form is a wire-shaped record
/// format too — its bytes freeze the day they ship. Exercised by the
/// `doc-intern-keys` CI lane (justfile).
#[cfg(feature = "doc-intern-keys")]
#[test]
fn interned_golden_vectors_decode_forever() {
    use inf_doc::intern;
    let dir = golden_dir();
    let regen = std::env::var_os("GOLDEN_REGEN").is_some();
    let vectors: Vec<(&str, Value)> = vec![
        // The wide shape interning exists for: repeated keys across
        // elements — both keys win the D2 rule.
        (
            "intern_wide",
            Value::Arr(
                (0..4)
                    .map(|i| {
                        Value::Obj(vec![
                            ("identifier".into(), Value::I64(i)),
                            ("display_name".into(), Value::Str(format!("row{i}"))),
                        ])
                    })
                    .collect(),
            ),
        ),
        // Mixed: "shared_key" wins; the unique keys stay plain.
        (
            "intern_mixed",
            Value::Obj(vec![
                ("first".into(), Value::Obj(vec![("shared_key".into(), Value::I64(1))])),
                ("second".into(), Value::Obj(vec![("shared_key".into(), Value::I64(2))])),
                ("third".into(), Value::Obj(vec![("shared_key".into(), Value::I64(3))])),
            ]),
        ),
    ];
    for (name, value) in vectors {
        let plain = model::encode(&value).expect("vector encodes");
        let encoded = intern::intern(&plain).expect("vector interns by construction");
        let path = dir.join(format!("{name}.idoc"));
        if regen {
            std::fs::create_dir_all(&dir).expect("create golden dir");
            std::fs::write(&path, &encoded).expect("write golden");
            continue;
        }
        let committed = std::fs::read(&path)
            .unwrap_or_else(|e| panic!("missing golden {name}: {e} (GOLDEN_REGEN=1 to create)"));
        assert_eq!(committed, encoded, "golden {name}: intern transform drifted");
        let doc = TapeDoc::from_bytes(&committed)
            .unwrap_or_else(|e| panic!("golden {name} fails validation: {e}"));
        assert_eq!(model::from_tape(&doc), value, "golden {name}: decode drifted");
        assert_eq!(intern::unintern(&committed), plain, "golden {name}: unintern drifted");
    }
}

/// D5 pinning: on a duplicate-key tape, `get()` returns the first match.
#[test]
fn dup_keys_get_returns_first_match() {
    let path = golden_dir().join("dup_keys.idoc");
    if !path.exists() && std::env::var_os("GOLDEN_REGEN").is_some() {
        // Regen runs race the writer test; the vector table is the source.
        let (_, v) = vectors().into_iter().find(|(n, _)| *n == "dup_keys").expect("in table");
        std::fs::create_dir_all(golden_dir()).expect("create golden dir");
        std::fs::write(&path, model::encode(&v).expect("encodes")).expect("write golden");
    }
    let bytes = std::fs::read(&path).expect("dup_keys golden exists (GOLDEN_REGEN=1 to create)");
    let doc = TapeDoc::from_bytes(&bytes).expect("validates");
    let DocValue::Obj(obj) = DocValue::from(doc.root()) else { panic!("object root") };
    let Some(DocValue::I64(v)) = obj.get(b"k") else { panic!("k resolves") };
    assert_eq!(v, 1, "first-match rule (ADR-0036 D5)");
}
