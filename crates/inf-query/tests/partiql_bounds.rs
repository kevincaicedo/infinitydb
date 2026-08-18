//! The S09 range-bound oracle (ADR-0080 D3): for every (declared key
//! type, servable operator, literal, document value) — if the value
//! admits into the index (`index_scalar_coerce`), then *encoded key ∈
//! compiled range* must equal the **production VM's verdict** for the
//! same one-leaf predicate. This is the S15 equivalence obligation
//! (index-derived ≡ scan-derived under declared semantics), proven at
//! the layer that owns the truth-table mapping. Values that do not
//! admit are exempt by declared design (ADR-0074 D4.2, disclosed).
//!
//! Statements are built as text and compiled through the whole pipe,
//! so the oracle also pins literal lexing (`{:?}` round-trip for f64).
//!
//! Release lane: `PROPTEST_CASES=1000000 cargo test --release -p
//! inf-query --test partiql_bounds` (the ledger's 10⁶ row).

use inf_doc::{DocValue, JsonParser, TapeDoc, path};
use inf_query::access::AccessStep;
use inf_query::partiql::{CatalogView, compile};
use inf_query::predicate::{CmpOp, Constant, Predicate, PredicateVm, encode};
use inf_store::{
    IndexId, IndexKeyBuf, IndexKeyType, IndexScalar, IndexSpec, IndexState, NsId, index_key_encode,
    index_scalar_coerce,
};
use proptest::prelude::*;

struct OneIndex {
    spec: IndexSpec,
}

impl CatalogView for OneIndex {
    fn resolve_ns(&self, name: &[u8]) -> Option<NsId> {
        (name == b"ns").then_some(NsId(1))
    }

    fn index_by_name(&self, ns: NsId, name: &[u8]) -> Option<&IndexSpec> {
        (ns == NsId(1) && name == b"idx").then_some(&self.spec)
    }

    fn indexes(&self, ns: NsId) -> impl Iterator<Item = &IndexSpec> {
        (ns == NsId(1)).then_some(&self.spec).into_iter()
    }

    fn catalog_epoch(&self) -> u64 {
        1
    }
}

fn catalog(key_type: IndexKeyType) -> OneIndex {
    OneIndex {
        spec: IndexSpec {
            id: IndexId(1),
            generation: 1,
            ns: NsId(1),
            name: b"idx".to_vec(),
            program: path::compile(b"$.v").expect("fixture path").as_bytes().to_vec(),
            key_type,
            state: IndexState::Ready,
        },
    }
}

/// A literal as it appears in statement text and as a VM constant.
#[derive(Clone, Debug)]
enum Lit {
    I64(i64),
    F64(f64),
    Bool(bool),
    Str(String),
}

impl Lit {
    fn text(&self) -> String {
        match self {
            Lit::I64(v) => format!("{v}"),
            Lit::F64(v) => format!("{v:?}"),
            Lit::Bool(v) => {
                if *v {
                    "TRUE".into()
                } else {
                    "FALSE".into()
                }
            }
            Lit::Str(s) => format!("'{}'", s.replace('\'', "''")),
        }
    }

    fn constant(&self) -> Constant {
        match self {
            Lit::I64(v) => Constant::I64(*v),
            Lit::F64(v) => Constant::F64(*v),
            Lit::Bool(v) => Constant::Bool(*v),
            Lit::Str(s) => Constant::Utf8(s.clone()),
        }
    }
}

#[derive(Clone, Debug)]
enum Op {
    Cmp(CmpOp, Lit),
    Between(Lit, Lit),
    BeginsWith(String),
}

impl Op {
    fn statement(&self) -> String {
        match self {
            Op::Cmp(op, lit) => {
                let op = match op {
                    CmpOp::Eq => "=",
                    CmpOp::Lt => "<",
                    CmpOp::Le => "<=",
                    CmpOp::Gt => ">",
                    CmpOp::Ge => ">=",
                    CmpOp::Ne => unreachable!("!= is never a key condition"),
                };
                format!("SELECT * FROM ns WHERE v {op} {}", lit.text())
            }
            Op::Between(lo, hi) => {
                format!("SELECT * FROM ns WHERE v BETWEEN {} AND {}", lo.text(), hi.text())
            }
            Op::BeginsWith(prefix) => {
                format!("SELECT * FROM ns WHERE begins_with(v, '{}')", prefix.replace('\'', "''"))
            }
        }
    }

    /// The same leaf as a VM predicate — the verdict side of the oracle
    /// is the production evaluator, never a re-derivation.
    fn predicate(&self) -> Predicate {
        let path = path::compile(b"$.v").expect("fixture path");
        match self {
            Op::Cmp(op, lit) => Predicate::Cmp { op: *op, path, constant: lit.constant() },
            Op::Between(lo, hi) => {
                Predicate::Between { path, lo: lo.constant(), hi: hi.constant() }
            }
            Op::BeginsWith(prefix) => Predicate::BeginsWith { path, prefix: prefix.clone() },
        }
    }
}

/// A document scalar at `$.v` — JSON-reachable values only (JSON has no
/// NaN/±∞; those enter no document and therefore no index).
#[derive(Clone, Debug)]
enum DocScalar {
    I64(i64),
    F64(f64),
    Bool(bool),
    Str(String),
}

impl DocScalar {
    fn json(&self) -> String {
        match self {
            DocScalar::I64(v) => format!("{{\"v\": {v}}}"),
            DocScalar::F64(v) => format!("{{\"v\": {}}}", format_json_f64(*v)),
            DocScalar::Bool(v) => format!("{{\"v\": {v}}}"),
            DocScalar::Str(s) => {
                format!("{{\"v\": {}}}", serde_json::Value::String(s.clone()))
            }
        }
    }

    fn scalar(&self) -> IndexScalar<'_> {
        match self {
            DocScalar::I64(v) => IndexScalar::I64(*v),
            DocScalar::F64(v) => IndexScalar::F64(*v),
            DocScalar::Bool(v) => IndexScalar::Bool(*v),
            DocScalar::Str(s) => IndexScalar::Utf8(s),
        }
    }
}

/// JSON float text that parses back to the same f64 and stays lexically
/// a float (a bare integral print like `1e300` is fine; `10` is not —
/// it would flip the idoc scalar to i64).
fn format_json_f64(v: f64) -> String {
    let text = format!("{v:?}");
    if text.contains('.') || text.contains('e') || text.contains('E') {
        text
    } else {
        format!("{text}.0")
    }
}

/// One oracle check. Returns `None` when the case is exempt: the
/// statement rejects (family strictness is its own spec'd behavior,
/// suite-pinned) or the value does not admit into the declared type.
fn check(key_type: IndexKeyType, op: &Op, value: &DocScalar) -> Option<(bool, bool)> {
    let catalog = catalog(key_type);
    let statement = op.statement();
    let compiled = compile(statement.as_bytes(), &catalog).ok()?;
    let AccessStep::IndexRange { lo, hi, .. } = &compiled.access.step else {
        panic!("one-leaf statements compile to index ranges: {statement}");
    };
    assert!(compiled.access.residual.is_none(), "the key condition folds entirely: {statement}");
    let admitted = index_scalar_coerce(key_type, value.scalar()).is_ok();
    if !admitted {
        return None;
    }
    let mut key = IndexKeyBuf::new();
    index_key_encode(key_type, value.scalar(), &mut key).expect("admitted values encode");
    let in_range = lo.admits_from_below(key.as_bytes()) && hi.admits_from_above(key.as_bytes());
    let program = encode(&op.predicate()).expect("one-leaf predicates encode");
    let vm = PredicateVm::new(&program);
    let json = value.json();
    let bytes = JsonParser::new().parse(json.as_bytes()).expect("fixture docs parse");
    let tape = TapeDoc::from_bytes(&bytes).expect("parser emits valid idoc");
    let verdict = vm.eval(DocValue::from(tape.root()), u64::MAX).expect("unbounded fuel").verdict;
    Some((in_range, verdict))
}

fn assert_agreement(key_type: IndexKeyType, op: &Op, value: &DocScalar) {
    if let Some((in_range, verdict)) = check(key_type, op, value) {
        assert_eq!(
            in_range,
            verdict,
            "range/VM divergence: type={key_type:?} stmt={} value={value:?}",
            op.statement()
        );
    }
}

// ---------------------------------------------------------------------
// Deterministic boundary corpus — the edges the ADR names, crossed
// exhaustively (ops × literals × values per type).
// ---------------------------------------------------------------------

const CMPS: [CmpOp; 5] = [CmpOp::Eq, CmpOp::Lt, CmpOp::Le, CmpOp::Gt, CmpOp::Ge];

fn numeric_literals() -> Vec<Lit> {
    let mut lits: Vec<Lit> = Vec::new();
    for v in [
        i64::MIN,
        i64::MIN + 1,
        -1,
        0,
        1,
        (1 << 53) - 1,
        1 << 53,
        (1 << 53) + 1,
        (1 << 62),
        i64::MAX - 1,
        i64::MAX,
    ] {
        lits.push(Lit::I64(v));
    }
    for f in [
        -0.0,
        0.0,
        0.5,
        -2.5,
        10.5,
        9007199254740992.0,
        9007199254740994.0,
        9.223372036854775e18,
        9.223372036854776e18,
        -9.223372036854776e18,
        1e20,
        -1e20,
        1e308,
        5e-324,
    ] {
        lits.push(Lit::F64(f));
    }
    lits
}

fn numeric_values() -> Vec<DocScalar> {
    let mut values: Vec<DocScalar> = Vec::new();
    for v in [
        i64::MIN,
        i64::MIN + 1,
        -3,
        -2,
        0,
        1,
        10,
        11,
        (1 << 53) - 1,
        1 << 53,
        (1 << 53) + 1,
        (1 << 53) + 2,
        i64::MAX - 1,
        i64::MAX,
    ] {
        values.push(DocScalar::I64(v));
    }
    for f in [
        -0.0,
        0.0,
        0.5,
        10.0,
        10.5,
        -2.5,
        -3.0,
        9007199254740992.0,
        9007199254740994.0,
        9.223372036854775e18,
        1e20,
        -1e20,
        1e308,
        5e-324,
    ] {
        values.push(DocScalar::F64(f));
    }
    values
}

#[test]
fn numeric_boundary_corpus() {
    let lits = numeric_literals();
    let values = numeric_values();
    for key_type in [IndexKeyType::I64, IndexKeyType::F64] {
        for lit in &lits {
            for cmp in CMPS {
                let op = Op::Cmp(cmp, lit.clone());
                for value in &values {
                    assert_agreement(key_type, &op, value);
                }
            }
        }
        // BETWEEN over a literal grid, reversed pairs included.
        for lo in lits.iter().step_by(3) {
            for hi in lits.iter().step_by(4) {
                let op = Op::Between(lo.clone(), hi.clone());
                for value in values.iter().step_by(2) {
                    assert_agreement(key_type, &op, value);
                }
            }
        }
    }
}

#[test]
fn utf8_boundary_corpus() {
    let strings: Vec<String> = vec![
        String::new(),
        "a".into(),
        "al".into(),
        "alz".into(),
        "am".into(),
        "b".into(),
        "café".into(),
        "caf".into(),
        "\u{0}".into(),
        "a\u{0}b".into(),
        "a\u{0}".into(),
        "ÿ".into(),
        "\u{10FFFF}".into(),
        "x".repeat(500),
        "x".repeat(1023),
        "x".repeat(1030),
    ];
    let values: Vec<DocScalar> = strings.iter().map(|s| DocScalar::Str(s.clone())).collect();
    for lit in &strings {
        for cmp in CMPS {
            let op = Op::Cmp(cmp, Lit::Str(lit.clone()));
            for value in &values {
                assert_agreement(IndexKeyType::Utf8, &op, value);
            }
        }
        let op = Op::BeginsWith(lit.clone());
        for value in &values {
            assert_agreement(IndexKeyType::Utf8, &op, value);
        }
    }
}

#[test]
fn bool_boundary_corpus() {
    for lit in [false, true] {
        for cmp in CMPS {
            let op = Op::Cmp(cmp, Lit::Bool(lit));
            for value in [false, true] {
                assert_agreement(IndexKeyType::Bool, &op, &DocScalar::Bool(value));
            }
        }
    }
    let op = Op::Between(Lit::Bool(false), Lit::Bool(true));
    assert_agreement(IndexKeyType::Bool, &op, &DocScalar::Bool(false));
    assert_agreement(IndexKeyType::Bool, &op, &DocScalar::Bool(true));
}

// ---------------------------------------------------------------------
// Property lane (the 10⁶ release row)
// ---------------------------------------------------------------------

fn lit_strategy(key_type: IndexKeyType) -> BoxedStrategy<Lit> {
    match key_type {
        IndexKeyType::Utf8 => "[\\x00-\\x7Fé☃]{0,24}".prop_map(Lit::Str).boxed(),
        IndexKeyType::Bool => any::<bool>().prop_map(Lit::Bool).boxed(),
        // Numeric indexes take both numeric families — the cross-type
        // mapping is the interesting surface.
        IndexKeyType::I64 | IndexKeyType::F64 => {
            prop_oneof![any::<i64>().prop_map(Lit::I64), finite_f64().prop_map(Lit::F64),].boxed()
        }
    }
}

fn finite_f64() -> impl Strategy<Value = f64> {
    prop_oneof![
        any::<f64>().prop_filter("finite", |f| f.is_finite()),
        // Bias toward the integral band where coercion edges live.
        (-(1i64 << 54)..(1i64 << 54)).prop_map(|v| v as f64),
        (-(1i64 << 54)..(1i64 << 54)).prop_map(|v| v as f64 + 0.5),
    ]
}

fn value_strategy(key_type: IndexKeyType) -> BoxedStrategy<DocScalar> {
    match key_type {
        IndexKeyType::Utf8 => "[\\x00-\\x7Fé☃]{0,24}".prop_map(DocScalar::Str).boxed(),
        IndexKeyType::Bool => any::<bool>().prop_map(DocScalar::Bool).boxed(),
        IndexKeyType::I64 | IndexKeyType::F64 => prop_oneof![
            any::<i64>().prop_map(DocScalar::I64),
            finite_f64().prop_map(DocScalar::F64),
        ]
        .boxed(),
    }
}

fn op_strategy(key_type: IndexKeyType) -> BoxedStrategy<Op> {
    let cmp = prop::sample::select(CMPS.to_vec());
    let lit = lit_strategy(key_type);
    let base = (cmp, lit.clone()).prop_map(|(op, lit)| Op::Cmp(op, lit));
    match key_type {
        IndexKeyType::Utf8 => prop_oneof![
            base,
            (lit_strategy(key_type), lit).prop_map(|(lo, hi)| Op::Between(lo, hi)),
            "[\\x00-\\x7Fé]{0,16}".prop_map(Op::BeginsWith),
        ]
        .boxed(),
        _ => prop_oneof![
            base,
            (lit_strategy(key_type), lit).prop_map(|(lo, hi)| Op::Between(lo, hi)),
        ]
        .boxed(),
    }
}

fn key_type_strategy() -> impl Strategy<Value = IndexKeyType> {
    prop::sample::select(vec![
        IndexKeyType::I64,
        IndexKeyType::F64,
        IndexKeyType::Utf8,
        IndexKeyType::Bool,
    ])
}

fn case_strategy() -> impl Strategy<Value = (IndexKeyType, Op, DocScalar)> {
    key_type_strategy()
        .prop_flat_map(|key_type| (Just(key_type), op_strategy(key_type), value_strategy(key_type)))
}

/// Two servable conjuncts on the same (single-valued) path fold by
/// byte-space interval intersection (ADR-0080 D1) — membership in the
/// folded range must equal the production VM's verdict of the
/// conjunction, for every admitted value.
fn check_fold(key_type: IndexKeyType, a: &Op, b: &Op, value: &DocScalar) -> Option<(bool, bool)> {
    let catalog = catalog(key_type);
    let leaf = |op: &Op| {
        let statement = op.statement();
        statement["SELECT * FROM ns WHERE ".len()..].to_string()
    };
    let statement = format!("SELECT * FROM ns WHERE {} AND {}", leaf(a), leaf(b));
    let compiled = compile(statement.as_bytes(), &catalog).ok()?;
    let AccessStep::IndexRange { lo, hi, .. } = &compiled.access.step else {
        panic!("two servable conjuncts compile to one range: {statement}");
    };
    assert!(compiled.access.residual.is_none(), "both conjuncts fold: {statement}");
    let admitted = index_scalar_coerce(key_type, value.scalar()).is_ok();
    if !admitted {
        return None;
    }
    let mut key = IndexKeyBuf::new();
    index_key_encode(key_type, value.scalar(), &mut key).expect("admitted values encode");
    let in_range = lo.admits_from_below(key.as_bytes()) && hi.admits_from_above(key.as_bytes());
    let conjunction = Predicate::And(vec![a.predicate(), b.predicate()]);
    let program = encode(&conjunction).expect("two-leaf predicates encode");
    let vm = PredicateVm::new(&program);
    let json = value.json();
    let bytes = JsonParser::new().parse(json.as_bytes()).expect("fixture docs parse");
    let tape = TapeDoc::from_bytes(&bytes).expect("parser emits valid idoc");
    let verdict = vm.eval(DocValue::from(tape.root()), u64::MAX).expect("unbounded fuel").verdict;
    Some((in_range, verdict))
}

fn fold_case_strategy() -> impl Strategy<Value = (IndexKeyType, Op, Op, DocScalar)> {
    key_type_strategy().prop_flat_map(|key_type| {
        (Just(key_type), op_strategy(key_type), op_strategy(key_type), value_strategy(key_type))
    })
}

proptest! {
    /// Range membership ≡ VM verdict for every admitted value.
    #[test]
    fn bounds_agree_with_the_vm((key_type, op, value) in case_strategy()) {
        if let Some((in_range, verdict)) = check(key_type, &op, &value) {
            prop_assert_eq!(
                in_range,
                verdict,
                "range/VM divergence: type={:?} stmt={} value={:?}",
                key_type,
                op.statement(),
                value
            );
        }
    }

    /// Folded (intersected) two-conjunct ranges agree with the VM's
    /// conjunction verdict — the intersection soundness property.
    #[test]
    fn folded_bounds_agree_with_the_vm((key_type, a, b, value) in fold_case_strategy()) {
        if let Some((in_range, verdict)) = check_fold(key_type, &a, &b, &value) {
            prop_assert_eq!(
                in_range,
                verdict,
                "fold/VM divergence: type={:?} a={} b={} value={:?}",
                key_type,
                a.statement(),
                b.statement(),
                value
            );
        }
    }
}
