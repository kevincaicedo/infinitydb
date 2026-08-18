//! M4.5-S08 differential oracle: the predicate VM against a naive
//! `serde_json` reference evaluator — identical verdicts *and* flag
//! semantics on generated (document, predicate) pairs. The reference is
//! the spec (obviously correct over `serde_json::Value`, recursion and
//! materialized nodelists fine off the data plane) and shares no code
//! with the VM: paths are walked from the generated segment model, and
//! the exact i64/f64 compare is written independently of
//! `inf_store::index_key::compare_i64_f64`, so a bug in the production
//! table cannot vanish into the oracle.
//!
//! The generator deliberately emits the adversarial shapes the plan
//! names: missing paths, cross-type mismatches, the 2^53/2^63 numeric
//! edges, `-0.0`, explicit nulls, and mixed-type arrays (multi-match
//! paths where existential semantics and `NE ≢ NOT(EQ)` bite).
//!
//! Release AC lane: `PROPTEST_CASES=1000000 cargo test --release
//! -p inf-query --test predicate_differential`.

use std::cmp::Ordering;

use inf_doc::{DocValue, JsonParser, TapeDoc, path};
use inf_query::predicate::{
    CmpOp, Constant, EvalFlags, Predicate, PredicateProgram, PredicateVm, encode,
};
use proptest::prelude::*;
use serde_json::{Map, Number, Value};

// ---------------------------------------------------------------------
// Path model: one generated shape, rendered for the VM (path text →
// compiled program) and walked directly by the reference.

#[derive(Clone, Debug)]
enum Seg {
    Member(&'static str),
    Index(u8),
    Wild,
    Descend(&'static str),
}

const KEYS: &[&str] = &["a", "b", "n", "s", "items", "tags", "nested"];

fn render(segs: &[Seg]) -> String {
    let mut text = String::from("$");
    for seg in segs {
        match seg {
            Seg::Member(key) => {
                text.push('.');
                text.push_str(key);
            }
            Seg::Index(i) => text.push_str(&format!("[{i}]")),
            Seg::Wild => text.push_str("[*]"),
            Seg::Descend(key) => {
                text.push_str("..");
                text.push_str(key);
            }
        }
    }
    text
}

/// Reference path resolution: the ADR-0040 selector subset the
/// generator emits (member, index, wildcard, descend-member), as plain
/// recursion over `Value`. Order and duplicates are irrelevant — every
/// consumer below is existential over the value multiset.
fn resolve<'v>(value: &'v Value, segs: &[Seg], out: &mut Vec<&'v Value>) {
    let Some((seg, rest)) = segs.split_first() else {
        out.push(value);
        return;
    };
    match seg {
        Seg::Member(key) => {
            if let Value::Object(map) = value
                && let Some(child) = map.get(*key)
            {
                resolve(child, rest, out);
            }
        }
        Seg::Index(i) => {
            if let Value::Array(items) = value
                && let Some(child) = items.get(usize::from(*i))
            {
                resolve(child, rest, out);
            }
        }
        Seg::Wild => match value {
            Value::Object(map) => {
                for child in map.values() {
                    resolve(child, rest, out);
                }
            }
            Value::Array(items) => {
                for child in items {
                    resolve(child, rest, out);
                }
            }
            _ => {}
        },
        Seg::Descend(key) => descend(value, key, rest, out),
    }
}

/// `..k`: apply `.k` to every node in pre-order — self first, then all
/// children recursively (the ADR-0040 descend shape).
fn descend<'v>(value: &'v Value, key: &str, rest: &[Seg], out: &mut Vec<&'v Value>) {
    if let Value::Object(map) = value
        && let Some(child) = map.get(key)
    {
        resolve(child, rest, out);
    }
    match value {
        Value::Object(map) => {
            for child in map.values() {
                descend(child, key, rest, out);
            }
        }
        Value::Array(items) => {
            for child in items {
                descend(child, key, rest, out);
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------
// Reference predicate model + evaluator.

#[derive(Clone, Debug)]
enum Tp {
    And(Vec<Tp>),
    Or(Vec<Tp>),
    Not(Box<Tp>),
    Cmp { op: CmpOp, path: Vec<Seg>, constant: Constant },
    Between { path: Vec<Seg>, lo: Constant, hi: Constant },
    BeginsWith { path: Vec<Seg>, prefix: String },
    In { path: Vec<Seg>, members: Vec<Constant> },
    Exists { path: Vec<Seg> },
}

fn lower(tp: &Tp) -> Predicate {
    let compiled =
        |segs: &[Seg]| path::compile(render(segs).as_bytes()).expect("rendered path compiles");
    match tp {
        Tp::And(children) => Predicate::And(children.iter().map(lower).collect()),
        Tp::Or(children) => Predicate::Or(children.iter().map(lower).collect()),
        Tp::Not(inner) => Predicate::Not(Box::new(lower(inner))),
        Tp::Cmp { op, path, constant } => {
            Predicate::Cmp { op: *op, path: compiled(path), constant: constant.clone() }
        }
        Tp::Between { path, lo, hi } => {
            Predicate::Between { path: compiled(path), lo: lo.clone(), hi: hi.clone() }
        }
        Tp::BeginsWith { path, prefix } => {
            Predicate::BeginsWith { path: compiled(path), prefix: prefix.clone() }
        }
        Tp::In { path, members } => {
            Predicate::In { path: compiled(path), members: members.clone() }
        }
        Tp::Exists { path } => Predicate::Exists { path: compiled(path) },
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct RefFlags {
    missing: bool,
    mismatch: bool,
}

/// Naive exact i64-vs-f64 compare, written independently of the
/// production `compare_i64_f64`: classify against the 2^63 band (where
/// sign alone decides), then compare integer parts exactly (both fit
/// i128), then let f64 arithmetic settle the fraction — `whole` is
/// exact, so `whole vs b` is the answer whenever the integer parts tie.
fn naive_cmp_i64_f64(a: i64, b: f64) -> Ordering {
    assert!(!b.is_nan(), "reference compares finite values only");
    if b >= 9_223_372_036_854_775_808.0 {
        return Ordering::Less;
    }
    if b < -9_223_372_036_854_775_808.0 {
        return Ordering::Greater;
    }
    let whole = b.trunc();
    match i128::from(a).cmp(&(whole as i128)) {
        Ordering::Equal => whole.partial_cmp(&b).expect("finite pair orders"),
        unequal => unequal,
    }
}

/// The reference's D4 table cell. Independent of the VM's `relation`.
fn ref_relation(value: &Value, constant: &Constant) -> Option<Ordering> {
    match (value, constant) {
        (Value::Number(n), Constant::I64(c)) => Some(match n.as_i64() {
            Some(v) => v.cmp(c),
            None => {
                let v = n.as_f64().expect("generator emits i64-range ints or finite floats");
                naive_cmp_i64_f64(*c, v).reverse()
            }
        }),
        (Value::Number(n), Constant::F64(c)) => Some(match n.as_i64() {
            Some(v) => naive_cmp_i64_f64(v, *c),
            None => {
                let v = n.as_f64().expect("generator emits i64-range ints or finite floats");
                v.partial_cmp(c).expect("finite pair orders")
            }
        }),
        (Value::Bool(v), Constant::Bool(c)) => Some(v.cmp(c)),
        (Value::String(v), Constant::Utf8(c)) => Some(v.as_bytes().cmp(c.as_bytes())),
        _ => None,
    }
}

fn ref_cmp_matches(op: CmpOp, ordering: Ordering) -> bool {
    match op {
        CmpOp::Eq => ordering == Ordering::Equal,
        CmpOp::Ne => ordering != Ordering::Equal,
        CmpOp::Lt => ordering == Ordering::Less,
        CmpOp::Le => ordering != Ordering::Greater,
        CmpOp::Gt => ordering == Ordering::Greater,
        CmpOp::Ge => ordering != Ordering::Less,
    }
}

/// One existential leaf: every resolved value tested (leaf-atomic —
/// the VM's disclosed refinement), MISSING on the empty set.
fn ref_leaf(
    root: &Value,
    segs: &[Seg],
    flags: &mut RefFlags,
    mut test: impl FnMut(&Value, &mut RefFlags) -> bool,
) -> bool {
    let mut values = Vec::new();
    resolve(root, segs, &mut values);
    if values.is_empty() {
        flags.missing = true;
        return false;
    }
    let mut satisfied = false;
    for value in values {
        if test(value, flags) {
            satisfied = true;
        }
    }
    satisfied
}

/// The reference evaluator: connectives short-circuit (skipped operands
/// contribute no flags — the D3 skip semantics), NOT flips the verdict
/// only, leaves per the D4/D5 rules.
fn ref_eval(tp: &Tp, root: &Value, flags: &mut RefFlags) -> bool {
    match tp {
        Tp::And(children) => {
            for child in children {
                if !ref_eval(child, root, flags) {
                    return false;
                }
            }
            true
        }
        Tp::Or(children) => {
            for child in children {
                if ref_eval(child, root, flags) {
                    return true;
                }
            }
            false
        }
        Tp::Not(inner) => !ref_eval(inner, root, flags),
        Tp::Cmp { op, path, constant } => {
            ref_leaf(root, path, flags, |value, flags| match ref_relation(value, constant) {
                Some(ordering) => ref_cmp_matches(*op, ordering),
                None => {
                    flags.mismatch = true;
                    false
                }
            })
        }
        Tp::Between { path, lo, hi } => ref_leaf(root, path, flags, |value, flags| {
            match (ref_relation(value, lo), ref_relation(value, hi)) {
                (Some(versus_lo), Some(versus_hi)) => {
                    versus_lo != Ordering::Less && versus_hi != Ordering::Greater
                }
                _ => {
                    flags.mismatch = true;
                    false
                }
            }
        }),
        Tp::BeginsWith { path, prefix } => {
            ref_leaf(root, path, flags, |value, flags| match value {
                Value::String(s) => s.as_bytes().starts_with(prefix.as_bytes()),
                _ => {
                    flags.mismatch = true;
                    false
                }
            })
        }
        Tp::In { path, members } => ref_leaf(root, path, flags, |value, flags| {
            for member in members {
                match ref_relation(value, member) {
                    Some(Ordering::Equal) => return true,
                    Some(_) => {}
                    None => {
                        // Family-homogeneous members: incomparable with
                        // one ⇒ incomparable with all (the VM's
                        // classify-once rule, observably identical).
                        flags.mismatch = true;
                        return false;
                    }
                }
            }
            false
        }),
        Tp::Exists { path } => {
            let mut values = Vec::new();
            resolve(root, path, &mut values);
            !values.is_empty()
        }
    }
}

// ---------------------------------------------------------------------
// Generators: adversarial by construction — shared key alphabet and
// overlapping scalar pools so hits, misses, and mismatches all occur.

fn arb_i64() -> impl Strategy<Value = i64> {
    prop_oneof![
        4 => -6i64..7,
        1 => Just(9_007_199_254_740_992),  // 2^53
        1 => Just(9_007_199_254_740_993),  // 2^53 + 1 (not f64-exact)
        1 => Just(-9_007_199_254_740_993),
        1 => Just(i64::MAX),
        1 => Just(i64::MIN),
        1 => any::<i64>(),
    ]
}

fn arb_f64() -> impl Strategy<Value = f64> {
    prop_oneof![
        4 => -6.0f64..7.0,
        1 => Just(0.0),
        1 => Just(-0.0),
        1 => Just(0.5),
        1 => Just(9_007_199_254_740_992.0),   // 2^53
        1 => Just(9.3e18),                    // above 2^63
        1 => Just(-9.3e18),
        1 => Just(1e300),
        1 => Just(5e-324),
        1 => any::<f64>().prop_filter("finite constants/values only", |f| f.is_finite()),
    ]
}

fn arb_string() -> impl Strategy<Value = String> {
    prop_oneof![
        Just(String::new()),
        Just("a".to_owned()),
        Just("ab".to_owned()),
        Just("x".to_owned()),
        Just("é".to_owned()),
        Just("a\0b".to_owned()),
        proptest::collection::vec(prop_oneof![Just('a'), Just('b'), Just('9')], 0..4)
            .prop_map(|chars| chars.into_iter().collect()),
    ]
}

fn arb_scalar() -> impl Strategy<Value = Value> {
    prop_oneof![
        4 => arb_i64().prop_map(Value::from),
        3 => arb_f64().prop_map(|f| Value::Number(Number::from_f64(f).expect("finite"))),
        2 => arb_string().prop_map(Value::String),
        1 => any::<bool>().prop_map(Value::Bool),
        1 => Just(Value::Null),
    ]
}

fn arb_key() -> impl Strategy<Value = &'static str> {
    proptest::sample::select(KEYS)
}

fn obj_of(entries: Vec<(&'static str, Value)>) -> Value {
    let mut map = Map::new();
    for (key, value) in entries {
        map.insert(key.to_owned(), value); // duplicate keys collapse, last wins
    }
    Value::Object(map)
}

/// Documents: an object root over the shared key alphabet; values mix
/// scalars, mixed-type arrays (the multi-match battleground), and
/// nested objects.
fn arb_doc() -> impl Strategy<Value = Value> {
    let node = arb_scalar().prop_recursive(3, 24, 4, |inner| {
        prop_oneof![
            3 => proptest::collection::vec(inner.clone(), 0..5).prop_map(Value::Array),
            2 => proptest::collection::vec((arb_key(), inner), 0..4).prop_map(obj_of),
            2 => arb_scalar(),
        ]
    });
    proptest::collection::vec((arb_key(), node), 0..5).prop_map(obj_of)
}

fn arb_path() -> impl Strategy<Value = Vec<Seg>> {
    let seg = prop_oneof![
        5 => arb_key().prop_map(Seg::Member),
        2 => (0u8..5).prop_map(Seg::Index),
        2 => Just(Seg::Wild),
        1 => arb_key().prop_map(Seg::Descend),
    ];
    proptest::collection::vec(seg, 0..4)
}

fn arb_constant() -> impl Strategy<Value = Constant> {
    prop_oneof![
        arb_numeric_constant(),
        arb_string().prop_map(Constant::Utf8),
        any::<bool>().prop_map(Constant::Bool),
    ]
}

fn arb_numeric_constant() -> impl Strategy<Value = Constant> {
    prop_oneof![arb_i64().prop_map(Constant::I64), arb_f64().prop_map(Constant::F64)]
}

fn arb_cmp_op() -> impl Strategy<Value = CmpOp> {
    prop_oneof![
        Just(CmpOp::Eq),
        Just(CmpOp::Ne),
        Just(CmpOp::Lt),
        Just(CmpOp::Le),
        Just(CmpOp::Gt),
        Just(CmpOp::Ge),
    ]
}

/// Leaves respect the D2.4 family rules (the encoder enforces them);
/// the crafted-bytes suite owns the violation side.
fn arb_leaf() -> impl Strategy<Value = Tp> {
    prop_oneof![
        4 => (arb_cmp_op(), arb_path(), arb_constant())
            .prop_map(|(op, path, constant)| Tp::Cmp { op, path, constant }),
        2 => (arb_path(), arb_numeric_constant(), arb_numeric_constant())
            .prop_map(|(path, lo, hi)| Tp::Between { path, lo, hi }),
        1 => (arb_path(), arb_string(), arb_string()).prop_map(|(path, lo, hi)| {
            Tp::Between { path, lo: Constant::Utf8(lo), hi: Constant::Utf8(hi) }
        }),
        2 => (arb_path(), arb_string()).prop_map(|(path, prefix)| Tp::BeginsWith { path, prefix }),
        2 => (arb_path(), proptest::collection::vec(arb_numeric_constant(), 1..=5))
            .prop_map(|(path, members)| Tp::In { path, members }),
        1 => (arb_path(), proptest::collection::vec(arb_string(), 1..=4)).prop_map(
            |(path, members)| Tp::In {
                path,
                members: members.into_iter().map(Constant::Utf8).collect(),
            }
        ),
        2 => arb_path().prop_map(|path| Tp::Exists { path }),
    ]
}

fn arb_tp() -> impl Strategy<Value = Tp> {
    arb_leaf().prop_recursive(4, 24, 3, |inner| {
        prop_oneof![
            proptest::collection::vec(inner.clone(), 2..=3).prop_map(Tp::And),
            proptest::collection::vec(inner.clone(), 2..=3).prop_map(Tp::Or),
            inner.prop_map(|inner| Tp::Not(Box::new(inner))),
        ]
    })
}

// ---------------------------------------------------------------------
// The oracle.

/// One differential case: reference verdict/flags ≡ VM verdict/flags,
/// plus the determinism and exact-fuel-boundary laws on the same pair.
fn check_case(tp: &Tp, doc: &Value) -> Result<(), TestCaseError> {
    let mut ref_flags = RefFlags::default();
    let ref_verdict = ref_eval(tp, doc, &mut ref_flags);

    let program = encode(&lower(tp)).expect("generated predicates are within bounds");
    let vm = PredicateVm::new(
        &PredicateProgram::from_bytes(program.as_bytes()).expect("encoder output validates"),
    );
    let text = doc.to_string();
    let bytes = JsonParser::new().parse(text.as_bytes()).expect("generated docs parse");
    let tape = TapeDoc::from_bytes(&bytes).expect("parser emits valid idoc");
    let root = DocValue::from(tape.root());

    let outcome = vm.eval(root, u64::MAX).expect("unbounded fuel completes");
    prop_assert_eq!(outcome.verdict, ref_verdict, "verdict: {:?} over {}", tp, text);
    let expected = EvalFlags { missing: ref_flags.missing, type_mismatch: ref_flags.mismatch };
    prop_assert_eq!(outcome.flags, expected, "flags: {:?} over {}", tp, text);

    let again = vm.eval(root, u64::MAX).expect("deterministic");
    prop_assert_eq!(again, outcome, "verdict, flags, and fuel are deterministic (L7)");
    let exact = vm.eval(root, outcome.fuel_used).expect("the exact budget suffices");
    prop_assert_eq!(exact, outcome);
    prop_assert!(
        vm.eval(root, outcome.fuel_used - 1).is_err(),
        "one unit less must exhaust (fuel_used {})",
        outcome.fuel_used
    );
    Ok(())
}

proptest! {
    /// The S08 plan AC (release lane reaches 10⁶ pairs via
    /// `PROPTEST_CASES`): identical verdicts incl. missing-path and
    /// type-mismatch flag semantics.
    #[test]
    fn vm_matches_reference(tp in arb_tp(), doc in arb_doc()) {
        check_case(&tp, &doc)?;
    }
}

/// Fixed regressions: the cells most likely to fork implementations,
/// pinned outside the generative lane.
#[test]
fn pinned_divergence_candidates() {
    let cases: &[(&str, Tp)] = &[
        // NE ≢ NOT(EQ) on a multi-match path.
        (
            r#"{"tags":[1,2]}"#,
            Tp::Not(Box::new(Tp::Cmp {
                op: CmpOp::Eq,
                path: vec![Seg::Member("tags"), Seg::Wild],
                constant: Constant::I64(1),
            })),
        ),
        (
            r#"{"tags":[1,2]}"#,
            Tp::Cmp {
                op: CmpOp::Ne,
                path: vec![Seg::Member("tags"), Seg::Wild],
                constant: Constant::I64(1),
            },
        ),
        // The 2^53 + 1 edge through EQ both directions.
        (
            r#"{"n":9007199254740993}"#,
            Tp::Cmp {
                op: CmpOp::Eq,
                path: vec![Seg::Member("n")],
                constant: Constant::F64(9_007_199_254_740_992.0),
            },
        ),
        // -0.0 in the document vs +0.0 constant.
        (
            r#"{"n":-0.0}"#,
            Tp::Cmp { op: CmpOp::Eq, path: vec![Seg::Member("n")], constant: Constant::F64(0.0) },
        ),
        // NOT over a missing path keeps the flag, flips the verdict.
        (
            r#"{"a":1}"#,
            Tp::Not(Box::new(Tp::Cmp {
                op: CmpOp::Eq,
                path: vec![Seg::Member("b")],
                constant: Constant::I64(5),
            })),
        ),
        // Explicit null: EXISTS true, comparison flagged.
        (r#"{"a":null}"#, Tp::Exists { path: vec![Seg::Member("a")] }),
        (
            r#"{"a":null}"#,
            Tp::Cmp { op: CmpOp::Ne, path: vec![Seg::Member("a")], constant: Constant::I64(1) },
        ),
        // Numeric coercion inside IN.
        (
            r#"{"n":10}"#,
            Tp::In {
                path: vec![Seg::Member("n")],
                members: vec![Constant::I64(3), Constant::F64(10.0)],
            },
        ),
        // Descend multi-match across nesting levels.
        (
            r#"{"a":{"n":1,"nested":{"n":"x"}}}"#,
            Tp::Cmp { op: CmpOp::Gt, path: vec![Seg::Descend("n")], constant: Constant::I64(0) },
        ),
    ];
    for (json, tp) in cases {
        let doc: Value = serde_json::from_str(json).expect("pin parses");
        check_case(tp, &doc).expect("pinned case agrees");
    }
}
