//! M3-S09 evaluator suite: differential against an independent naive
//! reference interpreter (nodelist-at-a-time over `model::Value` — a
//! deliberately different algorithm from the frame-stack DFS), the
//! determinism and form-agnosticism properties, the §3.4 R5 overlap
//! corpus, and the budget/yield/resume contract (ADR-0040 D4–D6).
//!
//! RedisJSON reply parity for these shapes is S21's (oracle-pending, the
//! recorded blocker); this suite pins *our* frozen semantics.

use inf_alloc::arena::{Arena, ArenaConfig};
use inf_doc::model::{self, Value};
use inf_doc::path::{
    self, EvalLimits, EvalStep, Matches, Member, PathAst, Segment, SliceSpec, compile, eval,
    eval_budgeted, eval_visit, resolve,
};
use inf_doc::{ArenaDoc, DocValue, TapeDoc};
use proptest::prelude::*;

// ---------------------------------------------------------------------
// Reference interpreter (test-only: recursion + materialized nodelists
// are fine off the data plane).

fn ref_eval(ast: &PathAst, root: &Value) -> Vec<Vec<u32>> {
    let mut list: Vec<(Vec<u32>, &Value)> = vec![(Vec::new(), root)];
    for segment in &ast.segments {
        let mut next = Vec::new();
        for (steps, node) in &list {
            ref_select(segment, steps, node, &mut next);
        }
        list = next;
    }
    list.into_iter().map(|(steps, _)| steps).collect()
}

fn ref_select<'v>(
    segment: &Segment,
    steps: &[u32],
    node: &'v Value,
    out: &mut Vec<(Vec<u32>, &'v Value)>,
) {
    let child = |steps: &[u32], ord: u32| {
        let mut s = steps.to_vec();
        s.push(ord);
        s
    };
    match segment {
        Segment::Child(name) => {
            if let Value::Obj(entries) = node
                && let Some((ord, (_, value))) =
                    entries.iter().enumerate().find(|(_, (k, _))| k.as_bytes() == &name[..])
            {
                out.push((child(steps, ord as u32), value));
            }
        }
        Segment::ChildAny => match node {
            Value::Obj(entries) => {
                for (ord, (_, value)) in entries.iter().enumerate() {
                    out.push((child(steps, ord as u32), value));
                }
            }
            Value::Arr(items) => {
                for (ord, value) in items.iter().enumerate() {
                    out.push((child(steps, ord as u32), value));
                }
            }
            _ => {}
        },
        Segment::Index(i) => {
            if let Value::Arr(items) = node {
                let resolved = if *i < 0 { *i + items.len() as i64 } else { *i };
                if (0..items.len() as i64).contains(&resolved) {
                    out.push((child(steps, resolved as u32), &items[resolved as usize]));
                }
            }
        }
        Segment::Slice(spec) => {
            if let Value::Arr(items) = node {
                for ord in ref_slice_indices(spec, items.len() as i64) {
                    out.push((child(steps, ord as u32), &items[ord as usize]));
                }
            }
        }
        Segment::Union(members) => {
            for member in members {
                let as_segment = match member {
                    Member::Name(n) => Segment::Child(n.clone()),
                    Member::Index(i) => Segment::Index(*i),
                    Member::Slice(s) => Segment::Slice(*s),
                };
                ref_select(&as_segment, steps, node, out);
            }
        }
        Segment::Descend(inner) => {
            // Pre-order: the node itself, then descendants in document
            // order (grammar §3; ADR-0040 D4).
            ref_select(inner, steps, node, out);
            match node {
                Value::Obj(entries) => {
                    for (ord, (_, value)) in entries.iter().enumerate() {
                        ref_select(segment, &child(steps, ord as u32), value, out);
                    }
                }
                Value::Arr(items) => {
                    for (ord, value) in items.iter().enumerate() {
                        ref_select(segment, &child(steps, ord as u32), value, out);
                    }
                }
                _ => {}
            }
        }
    }
}

/// Python slice indices — independently re-derived (grammar §4) in
/// `i128`, so the oracle is overflow-free at every `i64` extreme the
/// parser admits (review C11) without sharing the engine's saturating
/// cursor.
fn ref_slice_indices(spec: &SliceSpec, len: i64) -> Vec<i64> {
    let len = i128::from(len);
    let step = i128::from(spec.step.unwrap_or(1));
    assert_ne!(step, 0);
    let resolve = |v: i64| {
        let v = i128::from(v);
        if v < 0 { v + len } else { v }
    };
    let mut out = Vec::new();
    if step > 0 {
        let start = spec.start.map(resolve).unwrap_or(0).clamp(0, len);
        let stop = spec.end.map(resolve).unwrap_or(len).clamp(0, len);
        let mut i = start;
        while i < stop {
            out.push(i64::try_from(i).expect("in-range index"));
            i += step;
        }
    } else {
        let start = spec.start.map(resolve).unwrap_or(len - 1).clamp(-1, len - 1);
        let stop = spec.end.map(resolve).unwrap_or(-1).clamp(-1, len - 1);
        let mut i = start;
        while i > stop {
            out.push(i64::try_from(i).expect("in-range index"));
            i += step;
        }
    }
    out
}

// ---------------------------------------------------------------------
// Harness helpers.

fn matches_as_vecs(matches: &Matches) -> Vec<Vec<u32>> {
    matches.iter().map(|s| s.to_vec()).collect()
}

/// Evaluate a path text against a model doc on BOTH physical forms;
/// asserts they agree and returns the (tape-derived) raw match paths.
fn eval_both_forms(text: &str, value: &Value) -> Vec<Vec<u32>> {
    let program = compile(text.as_bytes()).expect("path compiles");
    let bytes = model::encode(value).expect("doc encodes");
    let doc = TapeDoc::from_bytes(&bytes).expect("validates");
    let tape_matches =
        eval(&program, DocValue::from(doc.root()), &EvalLimits::default()).expect("eval");
    let mut arena = Arena::new(ArenaConfig::default());
    let adoc = ArenaDoc::from_tape(&doc, &mut arena).expect("morphs");
    let arena_matches =
        eval(&program, adoc.root_value(&arena), &EvalLimits::default()).expect("eval");
    assert_eq!(tape_matches, arena_matches, "tape ≡ arena for {text:?} (ADR-0040 D5)");
    adoc.free(&mut arena);
    matches_as_vecs(&tape_matches)
}

fn store() -> Value {
    Value::Obj(vec![
        (
            "store".into(),
            Value::Obj(vec![
                (
                    "book".into(),
                    Value::Arr(vec![
                        Value::Obj(vec![
                            ("title".into(), Value::Str("Sayings".into())),
                            ("price".into(), Value::F64(8.95)),
                        ]),
                        Value::Obj(vec![
                            ("title".into(), Value::Str("Sword".into())),
                            ("price".into(), Value::F64(12.99)),
                        ]),
                        Value::Obj(vec![
                            ("title".into(), Value::Str("Moby".into())),
                            ("isbn".into(), Value::Str("0-553-21311-3".into())),
                        ]),
                    ]),
                ),
                ("bicycle".into(), Value::Obj(vec![("price".into(), Value::F64(19.95))])),
            ]),
        ),
        ("expensive".into(), Value::I64(10)),
    ])
}

// ---------------------------------------------------------------------
// Fixed-shape semantics (each row also proves tape ≡ arena).

#[test]
fn selector_semantics_on_the_store_doc() {
    let doc = store();
    // (path, expected raw match paths)
    let cases: &[(&str, &[&[u32]])] = &[
        ("$", &[&[]]),
        ("$.expensive", &[&[1]]),
        ("$.store.bicycle.price", &[&[0, 1, 0]]),
        ("$.missing", &[]),
        ("$.store.book[0].title", &[&[0, 0, 0, 0]]),
        ("$.store.book[-1].title", &[&[0, 0, 2, 0]]),
        ("$.store.book[3]", &[]),
        ("$.store.book[-4]", &[]),
        ("$.store.book[*].title", &[&[0, 0, 0, 0], &[0, 0, 1, 0], &[0, 0, 2, 0]]),
        ("$.store.*", &[&[0, 0], &[0, 1]]),
        ("$.store.book[1:]", &[&[0, 0, 1], &[0, 0, 2]]),
        ("$.store.book[::-1]", &[&[0, 0, 2], &[0, 0, 1], &[0, 0, 0]]),
        ("$.store.book[0:3:2]", &[&[0, 0, 0], &[0, 0, 2]]),
        ("$.store.book[2,0]", &[&[0, 0, 2], &[0, 0, 0]]), // member order, not doc order
        ("$.store.book[0,0]", &[&[0, 0, 0], &[0, 0, 0]]), // duplicates preserved (raw)
        ("$..price", &[&[0, 0, 0, 1], &[0, 0, 1, 1], &[0, 1, 0]]),
        ("$..isbn", &[&[0, 0, 2, 1]]),
        ("$..book[0]", &[&[0, 0, 0]]),
        (
            "$.store.book[*]['title','price']",
            &[&[0, 0, 0, 0], &[0, 0, 0, 1], &[0, 0, 1, 0], &[0, 0, 1, 1], &[0, 0, 2, 0]],
        ),
        // Type mismatches select nothing, silently (grammar §3).
        ("$.expensive[0]", &[]),
        ("$.expensive.*", &[]),
        ("$.store.book.title", &[]),
    ];
    for (text, want) in cases {
        let got = eval_both_forms(text, &doc);
        let want: Vec<Vec<u32>> = want.iter().map(|s| s.to_vec()).collect();
        assert_eq!(got, want, "raw matches for {text:?}");
    }
}

/// `$..price` order note pinned: descend is pre-order (self before
/// children), so `store.bicycle.price` ([0,1,0]) arrives after the book
/// prices — raw order is program order, canonical order re-sorts.
#[test]
fn overlap_corpus_ancestor_descendant() {
    // {"a": {"a": {"a": 1}}, "b": [{"a": 2}]}
    let doc = Value::Obj(vec![
        ("a".into(), Value::Obj(vec![("a".into(), Value::Obj(vec![("a".into(), Value::I64(1))]))])),
        ("b".into(), Value::Arr(vec![Value::Obj(vec![("a".into(), Value::I64(2))])])),
    ]);
    let program = compile(b"$..a").expect("compiles");
    let bytes = model::encode(&doc).expect("encodes");
    let tape = TapeDoc::from_bytes(&bytes).expect("validates");
    let matches = eval(&program, DocValue::from(tape.root()), &EvalLimits::default()).expect("ok");
    // Raw: pre-order self-first — root.a, then inside it a.a, a.a.a,
    // then the array element's a.
    assert_eq!(matches_as_vecs(&matches), vec![vec![0], vec![0, 0], vec![0, 0, 0], vec![1, 0, 0]]);
    let canonical = matches.canonical();
    assert!(canonical.any_overlap, "ancestor+descendant matches must be flagged (§3.4 R5)");
    let ordered: Vec<&[u32]> = canonical.ids.iter().map(|&id| matches.get(id as usize)).collect();
    assert_eq!(ordered, vec![&[0][..], &[0, 0], &[0, 0, 0], &[1, 0, 0]], "document order");

    // Disjoint matches carry no overlap flag.
    let program = compile(b"$.*").expect("compiles");
    let matches = eval(&program, DocValue::from(tape.root()), &EvalLimits::default()).expect("ok");
    assert!(!matches.canonical().any_overlap);

    // Union duplicates dedup in the canonical view, keep raw arity.
    let program = compile(b"$['a','a']").expect("compiles");
    let matches = eval(&program, DocValue::from(tape.root()), &EvalLimits::default()).expect("ok");
    assert_eq!(matches.len(), 2);
    assert_eq!(matches.canonical().ids.len(), 1);
}

#[test]
fn resolve_walks_location_paths() {
    let doc = store();
    let bytes = model::encode(&doc).expect("encodes");
    let tape = TapeDoc::from_bytes(&bytes).expect("validates");
    let root = DocValue::from(tape.root());
    let program = compile(b"$..isbn").expect("compiles");
    let matches = eval(&program, root, &EvalLimits::default()).expect("ok");
    assert_eq!(matches.len(), 1);
    let Some(DocValue::Str(s)) = resolve(root, matches.get(0)) else {
        panic!("isbn resolves to a string");
    };
    assert_eq!(s.as_bytes(), b"0-553-21311-3");
    assert!(resolve(root, &[9, 9]).is_none());
}

#[test]
fn match_cap_is_a_typed_error() {
    let doc = Value::Arr((0..64).map(Value::I64).collect());
    let bytes = model::encode(&doc).expect("encodes");
    let tape = TapeDoc::from_bytes(&bytes).expect("validates");
    let program = compile(b"$[*]").expect("compiles");
    let err = eval(&program, DocValue::from(tape.root()), &EvalLimits { max_matches: 63 })
        .expect_err("caps");
    assert_eq!(err, path::EvalError::TooManyMatches);
}

/// The §4.1 budget row: `Descend` yields every M nodes; resume completes
/// with matches identical to the unbudgeted run, and the yielded state
/// is owned (no document borrows — it outlives this scope trivially).
#[test]
fn budgeted_descend_yields_and_resumes() {
    let doc = store();
    let bytes = model::encode(&doc).expect("encodes");
    let tape = TapeDoc::from_bytes(&bytes).expect("validates");
    let root = DocValue::from(tape.root());
    let program = compile(b"$..*").expect("compiles");
    let unbudgeted = eval(&program, root, &EvalLimits::default()).expect("ok");
    for budget in 1..=8u64 {
        let mut state = None;
        let mut rounds = 0usize;
        let matches = loop {
            match eval_budgeted(&program, root, &EvalLimits::default(), budget, state.take())
                .expect("ok")
            {
                EvalStep::Done(m) => break m,
                EvalStep::Yield(s) => {
                    state = Some(s);
                    rounds += 1;
                    assert!(rounds < 10_000, "budgeted eval must terminate");
                }
            }
        };
        assert!(rounds > 0, "budget {budget} must yield at least once on $..*");
        assert_eq!(matches, unbudgeted, "resume ≡ straight run at budget {budget}");
    }
}

// ---------------------------------------------------------------------
// Integer-width bounds (review C10 / C11): the parser admits any i64;
// the evaluator must treat every out-of-array value as no match and
// every step magnitude as a bounded walk — on both evaluation lanes.

fn tape_root_of(value: &Value) -> Vec<u8> {
    model::encode(value).expect("encodes")
}

/// `[10,20,30]` — three elements, so `2³²` aliases element 0 and
/// `2³²+1` element 1 under a wrapping `as u32`.
fn three() -> Value {
    Value::Arr(vec![Value::I64(10), Value::I64(20), Value::I64(30)])
}

/// Every match `eval_visit` delivers for `text`, as raw ordinals of the
/// scalar it landed on (this suite's documents are integer arrays).
fn visited(text: &str, value: &Value) -> Vec<i64> {
    let program = compile(text.as_bytes()).expect("compiles");
    let bytes = tape_root_of(value);
    let tape = TapeDoc::from_bytes(&bytes).expect("validates");
    let mut seen = Vec::new();
    let outcome = eval_visit(&program, DocValue::from(tape.root()), u64::MAX, |node| {
        let DocValue::I64(v) = node else { panic!("integer array fixture") };
        seen.push(v);
        std::ops::ControlFlow::Continue(())
    });
    assert_eq!(outcome.end, path::VisitEnd::Complete, "{text}");
    seen
}

#[test]
fn index_beyond_the_u32_width_selects_nothing_on_every_lane() {
    let doc = three();
    for raw in EXTREME_INTS {
        // Every extreme resolves outside [0, 3): the positive ones are
        // ≥ 2³² − 1, the negative ones are below −len.
        let simple = format!("$[{raw}]");
        let union = format!("$[{raw},{raw}]");
        let descend = format!("$..[{raw}]");
        for text in [&simple, &union, &descend] {
            assert!(eval_both_forms(text, &doc).is_empty(), "{text} must select nothing");
            assert!(visited(text, &doc).is_empty(), "{text} must visit nothing");
        }
        // Reference agreement on the same shapes.
        let ast = path::parse_ast(simple.as_bytes()).expect("parses");
        assert!(ref_eval(&ast, &doc).is_empty(), "reference: {simple}");
    }
    // The in-range neighbours still resolve (the guard is exact).
    assert_eq!(eval_both_forms("$[2]", &doc), vec![vec![2]]);
    assert_eq!(eval_both_forms("$[-3]", &doc), vec![vec![0]]);
    assert!(eval_both_forms("$[3]", &doc).is_empty());
    assert!(eval_both_forms("$[-4]", &doc).is_empty());
}

#[test]
fn giant_slice_steps_yield_the_python_index_set_and_terminate() {
    let doc = three();
    let cases: &[(&str, &[u32])] = &[
        ("$[1::9223372036854775807]", &[1]),
        ("$[::9223372036854775807]", &[0]),
        ("$[2:100:9223372036854775806]", &[2]),
        ("$[-1::9223372036854775807]", &[2]),
        ("$[1::4294967296]", &[1]),
        ("$[::-9223372036854775808]", &[2]),
        ("$[2:0:-9223372036854775808]", &[2]),
        ("$[1::-9223372036854775807]", &[1]),
        ("$[::-4294967296]", &[2]),
        // Extreme bounds clamp; the walk is then the ordinary one.
        ("$[-9223372036854775808:9223372036854775807]", &[0, 1, 2]),
        ("$[9223372036854775807:-9223372036854775808:-1]", &[2, 1, 0]),
        ("$[4294967296:]", &[]),
        ("$[:4294967296]", &[0, 1, 2]),
    ];
    for (text, want) in cases {
        let want: Vec<Vec<u32>> = want.iter().map(|&o| vec![o]).collect();
        assert_eq!(eval_both_forms(text, &doc), want, "{text}");
        let ast = path::parse_ast(text.as_bytes()).expect("parses");
        assert_eq!(ref_eval(&ast, &doc), want, "reference: {text}");
        let visited_ords: Vec<i64> = visited(text, &doc);
        let want_values: Vec<i64> = want.iter().map(|s| 10 * (i64::from(s[0]) + 1)).collect();
        assert_eq!(visited_ords, want_values, "visit lane: {text}");
    }
    // Empty arrays: every extreme is a no-op walk.
    let empty = Value::Arr(vec![]);
    for text in ["$[::9223372036854775807]", "$[::-9223372036854775808]", "$[1::4294967296]"] {
        assert!(eval_both_forms(text, &empty).is_empty(), "{text} on []");
    }
}

/// The saved `Progress::Slice` cursor crosses a yield at the extreme:
/// resuming after the first element must end the slice, never re-walk it.
#[test]
fn giant_slice_step_resumes_across_a_yield() {
    let doc = three();
    let bytes = tape_root_of(&doc);
    let tape = TapeDoc::from_bytes(&bytes).expect("validates");
    let root = DocValue::from(tape.root());
    for text in
        ["$[1::9223372036854775807]", "$[::-9223372036854775808]", "$[*][::9223372036854775807]"]
    {
        let program = compile(text.as_bytes()).expect("compiles");
        let straight = eval(&program, root, &EvalLimits::default()).expect("ok");
        for budget in 1..=3u64 {
            let mut state = None;
            let mut rounds = 0;
            let resumed = loop {
                match eval_budgeted(&program, root, &EvalLimits::default(), budget, state.take())
                    .expect("ok")
                {
                    EvalStep::Done(m) => break m,
                    EvalStep::Yield(s) => {
                        state = Some(s);
                        rounds += 1;
                        assert!(rounds < 64, "{text} must terminate at budget {budget}");
                    }
                }
            };
            assert_eq!(resumed, straight, "{text} at budget {budget}");
        }
    }
}

// ---------------------------------------------------------------------
// Differential property: 10⁵ (doc × path) pairs in the release AC run.

fn arb_value() -> impl Strategy<Value = Value> {
    let key = prop_oneof![Just("a"), Just("b"), Just("k"), Just("z9")].prop_map(String::from);
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        (-1000i64..1000).prop_map(Value::I64),
        Just(Value::Str("s".into())),
    ];
    leaf.prop_recursive(4, 64, 5, move |inner| {
        prop_oneof![
            proptest::collection::vec(inner.clone(), 0..5).prop_map(Value::Arr),
            proptest::collection::vec((key.clone(), inner), 0..5).prop_map(|entries| {
                // Unique keys: canonical documents (the parser enforces
                // last-wins upstream; the model here must be canonical).
                let mut seen = std::collections::BTreeSet::new();
                Value::Obj(entries.into_iter().filter(|(k, _)| seen.insert(k.clone())).collect())
            }),
        ]
    })
}

/// The integer widths review C10/C11 found unguarded: every value a
/// wrapping `as u32` aliases onto a real ordinal, and the `i64` extremes
/// the parser admits. Mixed in at low weight so the document-shaped
/// draws keep their coverage.
static EXTREME_INTS: [i64; 10] = [
    u32::MAX as i64,
    1 << 32,
    (1 << 32) + 1,
    1 << 33,
    i64::MAX - 1,
    i64::MAX,
    -(1 << 32),
    -(1 << 32) - 1,
    i64::MIN + 1,
    i64::MIN,
];

fn arb_index() -> BoxedStrategy<i64> {
    prop_oneof![8 => -6i64..6, 1 => proptest::sample::select(&EXTREME_INTS[..])].boxed()
}

fn arb_step() -> BoxedStrategy<i64> {
    prop_oneof![
        8 => prop_oneof![(-3i64..0), (1i64..3)],
        1 => proptest::sample::select(&EXTREME_INTS[..]),
    ]
    .boxed()
}

fn arb_path_ast() -> impl Strategy<Value = PathAst> {
    let name = prop_oneof![Just("a"), Just("b"), Just("k"), Just("z9"), Just("q")]
        .prop_map(|s| s.as_bytes().to_vec());
    let slice = (
        proptest::option::of(arb_index()),
        proptest::option::of(arb_index()),
        proptest::option::of(arb_step()),
    )
        .prop_map(|(start, end, step)| SliceSpec { start, end, step });
    let member = prop_oneof![
        name.clone().prop_map(Member::Name),
        arb_index().prop_map(Member::Index),
        slice.clone().prop_map(Member::Slice),
    ];
    let selector = prop_oneof![
        4 => name.prop_map(Segment::Child),
        2 => Just(Segment::ChildAny),
        2 => arb_index().prop_map(Segment::Index),
        1 => slice.prop_map(Segment::Slice),
        1 => proptest::collection::vec(member, 2..4).prop_map(Segment::Union),
    ];
    let segment = prop_oneof![
        3 => selector.clone(),
        1 => selector.prop_map(|s| Segment::Descend(Box::new(s))),
    ];
    (any::<bool>(), proptest::collection::vec(segment, 0..5))
        .prop_map(|(legacy, segments)| PathAst { legacy, segments })
}

proptest! {
    /// The S09 differential AC: identical match sets and order vs the
    /// reference interpreter, on both physical forms.
    #[test]
    fn differential_vs_reference(value in arb_value(), ast in arb_path_ast()) {
        let text = path::ast::print(&ast);
        let expected = ref_eval(&ast, &value);
        let got = eval_both_forms(&text, &value);
        prop_assert_eq!(&got, &expected, "path {} over {:?}", text, value);
    }

    /// Determinism (L7): two evaluations agree — beyond the debug-build
    /// double-run inside `eval`, this pins release behavior too.
    #[test]
    fn evaluation_is_deterministic(value in arb_value(), ast in arb_path_ast()) {
        let text = path::ast::print(&ast);
        let program = compile(text.as_bytes()).expect("compiles");
        let bytes = model::encode(&value).expect("encodes");
        let tape = TapeDoc::from_bytes(&bytes).expect("validates");
        let root = DocValue::from(tape.root());
        let first = eval(&program, root, &EvalLimits::default()).expect("ok");
        let second = eval(&program, root, &EvalLimits::default()).expect("ok");
        prop_assert_eq!(first, second);
    }

    /// Budgeted evaluation converges to the straight run for any budget.
    #[test]
    fn budgeted_eval_matches_unbudgeted(
        value in arb_value(),
        ast in arb_path_ast(),
        budget in 1u64..16,
    ) {
        let text = path::ast::print(&ast);
        let program = compile(text.as_bytes()).expect("compiles");
        let bytes = model::encode(&value).expect("encodes");
        let tape = TapeDoc::from_bytes(&bytes).expect("validates");
        let root = DocValue::from(tape.root());
        let unbudgeted = eval(&program, root, &EvalLimits::default()).expect("ok");
        let mut state = None;
        let mut rounds = 0usize;
        let resumed = loop {
            match eval_budgeted(&program, root, &EvalLimits::default(), budget, state.take())
                .expect("ok")
            {
                EvalStep::Done(m) => break m,
                EvalStep::Yield(s) => {
                    state = Some(s);
                    rounds += 1;
                    prop_assert!(rounds < 100_000, "must terminate");
                }
            }
        };
        prop_assert_eq!(resumed, unbudgeted);
    }

    /// M4.5-S08 congruence pair-assertion: the streaming entry
    /// (`eval_visit`) delivers exactly the values the recording entry
    /// (`eval`) locates — same multiset, same raw order — and its node
    /// accounting matches `eval_budgeted`'s yield behavior at every
    /// budget. Two walks over one `advance` core must agree everywhere
    /// or the predicate VM's verdicts fork from path semantics.
    #[test]
    fn visit_streams_the_recorded_matches(value in arb_value(), ast in arb_path_ast(), budget in 0u64..24) {
        let text = path::ast::print(&ast);
        let program = compile(text.as_bytes()).expect("compiles");
        let bytes = model::encode(&value).expect("encodes");
        let tape = TapeDoc::from_bytes(&bytes).expect("validates");
        let root = DocValue::from(tape.root());
        let matches = eval(&program, root, &EvalLimits::default()).expect("ok");
        let expected: Vec<Value> = matches
            .iter()
            .map(|steps| model::from_cursor(path::resolve(root, steps).expect("match resolves")))
            .collect();
        let mut streamed: Vec<Value> = Vec::new();
        let outcome = eval_visit(&program, root, u64::MAX, |v| {
            streamed.push(model::from_cursor(v));
            core::ops::ControlFlow::Continue(())
        });
        prop_assert_eq!(outcome.end, path::VisitEnd::Complete);
        prop_assert_eq!(&streamed, &expected, "visit ≡ eval for {}", text);
        // Budget congruence: eval_visit exhausts exactly where
        // eval_budgeted yields (same meter, ADR-0040 D6), and a
        // completing walk reports nodes within the budget.
        let bounded = eval_visit(&program, root, budget, |_| {
            core::ops::ControlFlow::Continue(())
        });
        let step = eval_budgeted(&program, root, &EvalLimits::default(), budget, None).expect("ok");
        match step {
            EvalStep::Done(_) => {
                prop_assert_eq!(bounded.end, path::VisitEnd::Complete);
                prop_assert!(bounded.nodes_visited <= budget);
            }
            EvalStep::Yield(_) => {
                prop_assert_eq!(bounded.end, path::VisitEnd::Exhausted);
                prop_assert_eq!(bounded.nodes_visited, budget);
            }
        }
    }

    /// The visitor's early break stops the walk and reports it.
    #[test]
    fn visit_break_stops_the_walk(value in arb_value(), ast in arb_path_ast()) {
        let text = path::ast::print(&ast);
        let program = compile(text.as_bytes()).expect("compiles");
        let bytes = model::encode(&value).expect("encodes");
        let tape = TapeDoc::from_bytes(&bytes).expect("validates");
        let root = DocValue::from(tape.root());
        let total = eval(&program, root, &EvalLimits::default()).expect("ok").len();
        let mut seen = 0usize;
        let outcome = eval_visit(&program, root, u64::MAX, |_| {
            seen += 1;
            core::ops::ControlFlow::Break(())
        });
        if total == 0 {
            prop_assert_eq!(outcome.end, path::VisitEnd::Complete);
            prop_assert_eq!(seen, 0);
        } else {
            prop_assert_eq!(outcome.end, path::VisitEnd::Stopped);
            prop_assert_eq!(seen, 1);
        }
    }
}
