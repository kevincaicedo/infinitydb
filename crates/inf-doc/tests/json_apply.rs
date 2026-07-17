//! M3-S11/S12 apply-engine suite (ADR-0041 D5/D8): table-pinned scalar
//! mutation semantics (`oracle-pending` where RedisJSON's byte behavior
//! is unverifiable without the S21 container — every such pin is marked),
//! the §3.4 R4 abort contract, the R5 overlap corpus, and a differential
//! property against an independent reference mutation over the model
//! tree applied in **reverse canonical order** — the literal R5 wording,
//! which the engine realizes as a containment drop; equal bytes prove
//! the realization faithful.

use inf_doc::apply::{ApplyError, ApplyOp, ApplyOutcome, MatchResult, Number, apply};
use inf_doc::limits::DOC_BYTES_MAX;
use inf_doc::model::{self, Value};
use inf_doc::path::{EvalLimits, compile, eval};
use inf_doc::{DocValue, JsonParser, TapeDoc, serialize_canonical_into};
use proptest::prelude::*;

fn tape_of(json: &str) -> Vec<u8> {
    JsonParser::new().parse(json.as_bytes()).expect("test corpus parses")
}

fn json_of(idoc: &[u8]) -> String {
    let doc = TapeDoc::from_bytes(idoc).expect("apply output validates");
    let mut out = Vec::new();
    serialize_canonical_into(DocValue::from(doc.root()), &mut out);
    String::from_utf8(out).expect("serializer emits UTF-8")
}

fn run(json: &str, path: &str, op: &ApplyOp<'_>) -> Result<ApplyOutcome, ApplyError> {
    let bytes = tape_of(json);
    let doc = TapeDoc::from_bytes(&bytes).expect("validates");
    let program = compile(path.as_bytes()).expect("test path compiles");
    apply(&doc, &program, op, &EvalLimits::default(), DOC_BYTES_MAX)
}

fn applied_json(json: &str, path: &str, op: &ApplyOp<'_>) -> (String, ApplyOutcome) {
    let outcome = run(json, path, op).expect("apply succeeds");
    let bytes = outcome.bytes.as_ref().expect("an edit applied");
    (json_of(bytes), outcome.clone())
}

fn fragment(v: &Value) -> Vec<u8> {
    model::encode_fragment(v).expect("fragment encodes")
}

// ---- NUMINCRBY / NUMMULTBY (oracle-pending: error strings + f64 text
// shapes byte-diff at S21) -------------------------------------------------

#[test]
fn numincrby_multi_match_skips_non_numbers_in_raw_order() {
    let (json, outcome) = applied_json(
        r#"{"a":1,"b":{"a":2.5},"c":[{"a":"s"}]}"#,
        "$..a",
        &ApplyOp::NumIncrBy(Number::I64(1)),
    );
    assert_eq!(json, r#"{"a":2,"b":{"a":3.5},"c":[{"a":"s"}]}"#);
    assert_eq!(
        outcome.results,
        vec![
            MatchResult::Num(Number::I64(2)),
            MatchResult::Num(Number::F64(3.5)),
            MatchResult::Skipped,
        ]
    );
    assert_eq!(outcome.applied, 2);
}

#[test]
fn numincrby_preserves_integers_across_encoded_widths() {
    // 127 is a fixint; 128 needs the varint form — the splice grows the
    // value and every ancestor length re-covers (ADR-0036 D3).
    let (json, _) = applied_json(r#"{"n":[127]}"#, "$.n[0]", &ApplyOp::NumIncrBy(Number::I64(1)));
    assert_eq!(json, r#"{"n":[128]}"#);
    // And the shrink direction: -33 (varint) + 1 = -32 (fixint).
    let (json, _) = applied_json(r#"{"n":-33}"#, "$.n", &ApplyOp::NumIncrBy(Number::I64(1)));
    assert_eq!(json, r#"{"n":-32}"#);
}

#[test]
fn numincrby_promotes_to_f64_on_float_operands() {
    let (json, outcome) = applied_json(r#"{"n":1}"#, "$.n", &ApplyOp::NumIncrBy(Number::F64(0.5)));
    assert_eq!(json, r#"{"n":1.5}"#);
    assert_eq!(outcome.results, vec![MatchResult::Num(Number::F64(1.5))]);
}

#[test]
fn numincrby_overflow_aborts_the_whole_command() {
    // Match 1 of 2 overflows: R4 — nothing mutates, the error is typed.
    let err = run(&format!(r#"[{},1]"#, i64::MAX), "$[*]", &ApplyOp::NumIncrBy(Number::I64(1)))
        .expect_err("overflow aborts");
    assert_eq!(err, ApplyError::Overflow);
}

#[test]
fn nummultby_overflow_and_non_finite_abort() {
    let err = run(&format!(r#"[{}]"#, i64::MAX), "$[0]", &ApplyOp::NumMultBy(Number::I64(2)))
        .expect_err("i64 overflow");
    assert_eq!(err, ApplyError::Overflow);
    let err = run(r#"[1e308]"#, "$[0]", &ApplyOp::NumMultBy(Number::F64(1e308)))
        .expect_err("non-finite f64");
    assert_eq!(err, ApplyError::NotANumber);
}

#[test]
fn nummultby_multiplies_in_place() {
    let (json, outcome) =
        applied_json(r#"{"a":[3,4.0]}"#, "$.a[*]", &ApplyOp::NumMultBy(Number::I64(2)));
    assert_eq!(json, r#"{"a":[6,8.0]}"#);
    assert_eq!(outcome.applied, 2);
}

// ---- STRAPPEND (length unit pinned as bytes — oracle-pending S21) ---------

#[test]
fn strappend_appends_and_reports_byte_lengths() {
    let (json, outcome) =
        applied_json(r#"{"s":"hi","n":1}"#, "$.*", &ApplyOp::StrAppend(b" there"));
    assert_eq!(json, r#"{"s":"hi there","n":1}"#);
    assert_eq!(outcome.results, vec![MatchResult::Len(8), MatchResult::Skipped]);
    assert_eq!(outcome.applied, 1);
}

#[test]
fn strappend_crosses_string_width_classes() {
    // 31 bytes (fixstr max) + 1 → str8 (the header grows a byte).
    let base = "x".repeat(31);
    let (json, outcome) =
        applied_json(&format!(r#"{{"s":"{base}"}}"#), "$.s", &ApplyOp::StrAppend(b"y"));
    assert_eq!(json, format!(r#"{{"s":"{base}y"}}"#));
    assert_eq!(outcome.results, vec![MatchResult::Len(32)]);
}

// ---- TOGGLE ---------------------------------------------------------------

#[test]
fn toggle_flips_booleans_only() {
    let (json, outcome) = applied_json(r#"[true,false,1,"t"]"#, "$[*]", &ApplyOp::Toggle);
    assert_eq!(json, r#"[false,true,1,"t"]"#);
    assert_eq!(
        outcome.results,
        vec![
            MatchResult::Toggled(false),
            MatchResult::Toggled(true),
            MatchResult::Skipped,
            MatchResult::Skipped,
        ]
    );
}

// ---- CLEAR (already-clear arms skip and stay uncounted — ADR-0041 D8;
// oracle-pending S21) --------------------------------------------------------

#[test]
fn clear_empties_containers_and_zeroes_numbers() {
    let (json, outcome) = applied_json(
        r#"{"a":[],"b":[1,2],"c":0,"d":1.5,"e":"s","f":{"x":1}}"#,
        "$.*",
        &ApplyOp::Clear,
    );
    assert_eq!(json, r#"{"a":[],"b":[],"c":0,"d":0,"e":"s","f":{}}"#);
    assert_eq!(outcome.applied, 3, "b, d, f cleared; a/c already clear; e skipped");
}

#[test]
fn clear_on_the_root_empties_the_document() {
    let (json, outcome) = applied_json(r#"{"a":1}"#, "$", &ApplyOp::Clear);
    assert_eq!(json, r#"{}"#);
    assert_eq!(outcome.applied, 1);
}

#[test]
fn clear_overlap_ancestor_supersedes_descendant() {
    // $..* matches both the outer object's members and their children:
    // reverse document order clears descendants first, then the ancestor
    // empties over them (§3.4 R5) — both count against pre-state.
    let (json, outcome) = applied_json(r#"{"o":{"n":5}}"#, "$..*", &ApplyOp::Clear);
    assert_eq!(json, r#"{"o":{}}"#);
    assert_eq!(outcome.applied, 2);
}

// ---- DEL -------------------------------------------------------------------

#[test]
fn del_on_root_is_a_typed_key_lifecycle_error() {
    let err = run(r#"{"a":1}"#, "$", &ApplyOp::Del).expect_err("root delete is not a delta");
    assert_eq!(err, ApplyError::RootDelete);
}

#[test]
fn del_removes_object_members_and_array_elements() {
    let (json, outcome) = applied_json(r#"{"a":1,"b":2}"#, "$.a", &ApplyOp::Del);
    assert_eq!(json, r#"{"b":2}"#);
    assert_eq!(outcome.applied, 1);
    let (json, outcome) = applied_json(r#"[10,20,30]"#, "$[1]", &ApplyOp::Del);
    assert_eq!(json, r#"[10,30]"#);
    assert_eq!(outcome.applied, 1);
}

#[test]
fn del_overlapping_matches_counts_the_pre_state_set() {
    // $..a matches $.a, $.a.a and $.x.a; removing $.a supersedes its
    // nested member (R5 realized as containment drop).
    let (json, outcome) = applied_json(r#"{"a":{"a":1},"x":{"a":2}}"#, "$..a", &ApplyOp::Del);
    assert_eq!(json, r#"{"x":{}}"#);
    assert_eq!(outcome.applied, 3);
}

#[test]
fn del_with_no_matches_changes_nothing() {
    let outcome = run(r#"{"a":1}"#, "$.missing", &ApplyOp::Del).expect("no-op succeeds");
    assert!(outcome.bytes.is_none());
    assert_eq!(outcome.applied, 0);
}

// ---- SET (replace + member create; parent rules per ADR-0041 D6) -----------

#[test]
fn set_replace_swaps_every_match() {
    let frag = fragment(&Value::I64(9));
    let (json, outcome) =
        applied_json(r#"{"a":1,"b":{"a":2}}"#, "$..a", &ApplyOp::SetReplace { fragment: &frag });
    assert_eq!(json, r#"{"a":9,"b":{"a":9}}"#);
    assert_eq!(outcome.applied, 2);
}

#[test]
fn set_member_replaces_in_place_and_appends_new_keys() {
    let frag = fragment(&Value::Bool(true));
    // Existing key: replaced at its position (first-match, ADR-0036 D5).
    let (json, _) =
        applied_json(r#"{"k":1,"z":2}"#, "$", &ApplyOp::SetMember { key: b"k", fragment: &frag });
    assert_eq!(json, r#"{"k":true,"z":2}"#);
    // New key: appended in insertion order.
    let (json, _) =
        applied_json(r#"{"z":2}"#, "$", &ApplyOp::SetMember { key: b"k", fragment: &frag });
    assert_eq!(json, r#"{"z":2,"k":true}"#);
}

#[test]
fn set_member_skips_non_object_parents() {
    let frag = fragment(&Value::I64(1));
    let outcome = run(r#"{"a":[1]}"#, "$.a", &ApplyOp::SetMember { key: b"k", fragment: &frag })
        .expect("skip succeeds");
    assert!(outcome.bytes.is_none());
    assert_eq!(outcome.results, vec![MatchResult::Skipped]);
}

#[test]
fn set_member_same_offset_nested_inserts_order_deepest_first() {
    // The inner object is the outer's last member: both appends land at
    // the same byte offset, and the deeper one must land first (the
    // `Edit::depth` tie-break) — inner bytes belong to the inner extent.
    let frag = fragment(&Value::I64(7));
    let (json, outcome) = applied_json(
        r#"{"a":{"a":{}}}"#,
        "$..a",
        &ApplyOp::SetMember { key: b"k", fragment: &frag },
    );
    assert_eq!(json, r#"{"a":{"a":{"k":7},"k":7}}"#);
    assert_eq!(outcome.applied, 2);
}

// ---- bounds + no-op discipline ---------------------------------------------

#[test]
fn deep_ancestor_chains_re_cover_every_length() {
    // Depth 32: one leaf edit patches all 32 enclosing u24 lengths; the
    // output revalidating end-to-end is the proof.
    let mut json = String::from("1");
    for _ in 0..32 {
        json = format!(r#"{{"d":{json}}}"#);
    }
    let path = format!("$.{}", vec!["d"; 32].join("."));
    let (out, _) = applied_json(&json, &path, &ApplyOp::NumIncrBy(Number::I64(41)));
    assert_eq!(out, json.replace('1', "42"));
}

#[test]
fn post_edit_size_cap_aborts_before_output_exists() {
    let bytes = tape_of(r#"{"s":"xx"}"#);
    let doc = TapeDoc::from_bytes(&bytes).expect("validates");
    let program = compile(b"$.s").expect("compiles");
    let payload = vec![b'y'; 64];
    let err = apply(
        &doc,
        &program,
        &ApplyOp::StrAppend(&payload),
        &EvalLimits::default(),
        bytes.len(), // a cap the grown document must exceed
    )
    .expect_err("cap binds");
    assert_eq!(err, ApplyError::TooLarge);
}

#[test]
fn all_skipped_is_a_no_op_with_no_bytes() {
    let outcome = run(r#"{"a":"s"}"#, "$.a", &ApplyOp::Toggle).expect("skip succeeds");
    assert!(outcome.bytes.is_none(), "no edit ⇒ no rewrite ⇒ no version bump (ADR-0041 D8)");
    assert_eq!(outcome.applied, 0);
}

// ---- differential property: engine splice ≡ reverse-canonical-order
// mutation over the reference model tree --------------------------------------

fn arb_value() -> impl Strategy<Value = Value> {
    let key = prop_oneof![Just("a"), Just("b"), Just("k"), Just("z9")].prop_map(String::from);
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        (-1000i64..1000).prop_map(Value::I64),
        (-100i64..100).prop_map(|n| Value::F64(n as f64 * 0.5)),
        Just(Value::Str("s".into())),
        Just(Value::Str("a-longer-string-payload".into())),
    ];
    leaf.prop_recursive(4, 64, 5, move |inner| {
        prop_oneof![
            proptest::collection::vec(inner.clone(), 0..5).prop_map(Value::Arr),
            proptest::collection::vec((key.clone(), inner), 0..5).prop_map(|entries| {
                let mut seen = std::collections::BTreeSet::new();
                Value::Obj(entries.into_iter().filter(|(k, _)| seen.insert(k.clone())).collect())
            }),
        ]
    })
}

fn arb_op() -> impl Strategy<Value = OwnedOp> {
    let values = || proptest::collection::vec(arb_value(), 1..3);
    prop_oneof![
        (-1000i64..1000).prop_map(|n| OwnedOp::NumIncrBy(Number::I64(n))),
        (-100i64..100).prop_map(|n| OwnedOp::NumIncrBy(Number::F64(n as f64 * 0.25))),
        (-30i64..30).prop_map(|n| OwnedOp::NumMultBy(Number::I64(n))),
        Just(OwnedOp::StrAppend(b"+tail".to_vec())),
        Just(OwnedOp::Toggle),
        Just(OwnedOp::Clear),
        Just(OwnedOp::Del),
        arb_value().prop_map(|v| {
            let frag = fragment(&v);
            OwnedOp::SetReplace(v, frag)
        }),
        values().prop_map(|vs| {
            let operand = arr_operand(&vs);
            OwnedOp::ArrAppend(vs, operand)
        }),
        (values(), -4i64..4).prop_map(|(vs, index)| {
            let operand = arr_operand(&vs);
            OwnedOp::ArrInsert(index, vs, operand)
        }),
        (-4i64..4).prop_map(OwnedOp::ArrPop),
        (-4i64..4, -4i64..4).prop_map(|(start, stop)| OwnedOp::ArrTrim(start, stop)),
        arb_value().prop_map(|v| {
            let frag = fragment(&v);
            OwnedOp::Merge(v, frag)
        }),
    ]
}

/// Owned op mirror (proptest values must be `'static`). Value-carrying
/// ops hold both the model value (reference side) and its canonical
/// fragment/operand (engine side) so neither leg re-derives the other.
#[derive(Clone, Debug)]
enum OwnedOp {
    NumIncrBy(Number),
    NumMultBy(Number),
    StrAppend(Vec<u8>),
    Toggle,
    Clear,
    Del,
    SetReplace(Value, Vec<u8>),
    ArrAppend(Vec<Value>, Vec<u8>),
    ArrInsert(i64, Vec<Value>, Vec<u8>),
    ArrPop(i64),
    ArrTrim(i64, i64),
    Merge(Value, Vec<u8>),
}

impl OwnedOp {
    fn borrow(&self) -> ApplyOp<'_> {
        match self {
            OwnedOp::NumIncrBy(n) => ApplyOp::NumIncrBy(*n),
            OwnedOp::NumMultBy(n) => ApplyOp::NumMultBy(*n),
            OwnedOp::StrAppend(s) => ApplyOp::StrAppend(s),
            OwnedOp::Toggle => ApplyOp::Toggle,
            OwnedOp::Clear => ApplyOp::Clear,
            OwnedOp::Del => ApplyOp::Del,
            OwnedOp::SetReplace(_, frag) => ApplyOp::SetReplace { fragment: frag },
            OwnedOp::ArrAppend(_, operand) => ApplyOp::ArrAppend { elements: operand },
            OwnedOp::ArrInsert(index, _, operand) => {
                ApplyOp::ArrInsert { index: *index, elements: operand }
            }
            OwnedOp::ArrPop(index) => ApplyOp::ArrPop { index: *index },
            OwnedOp::ArrTrim(start, stop) => ApplyOp::ArrTrim { start: *start, stop: *stop },
            OwnedOp::Merge(_, frag) => ApplyOp::Merge { patch: frag },
        }
    }
}

/// RFC 7386 over the model tree — the independent reference for the
/// engine's iterative byte merge (recursion is fine in test code).
fn model_merge(target: &Value, patch: &Value) -> Value {
    let Value::Obj(patch_entries) = patch else { return patch.clone() };
    let mut merged: Vec<(String, Value)> = match target {
        Value::Obj(entries) => entries.clone(),
        _ => Vec::new(),
    };
    for (key, value) in patch_entries {
        let existing = merged.iter().position(|(k, _)| k == key);
        match (existing, matches!(value, Value::Null)) {
            (Some(i), true) => {
                merged.remove(i);
            }
            (None, true) => {}
            (Some(i), false) => merged[i].1 = model_merge(&merged[i].1.clone(), value),
            (None, false) => merged.push((key.clone(), model_merge(&Value::Null, value))),
        }
    }
    Value::Obj(merged)
}

/// Reference mutation at one location path — independent semantics over
/// the owned tree. Returns `true` when the op applies (mirrors
/// `MatchResult::Skipped`). `Del` on the root is unreachable (the
/// command layer owns root deletion).
fn model_apply_at(root: &mut Value, steps: &[u32], op: &OwnedOp) -> bool {
    if let (OwnedOp::Del, [head @ .., last]) = (op, steps) {
        let parent = model_resolve(root, head);
        match parent {
            Value::Obj(entries) => {
                entries.remove(*last as usize);
            }
            Value::Arr(items) => {
                items.remove(*last as usize);
            }
            _ => unreachable!("locations resolve through containers"),
        }
        return true;
    }
    let node = model_resolve(root, steps);
    match op {
        OwnedOp::NumIncrBy(n) | OwnedOp::NumMultBy(n) => {
            let mul = matches!(op, OwnedOp::NumMultBy(_));
            match (&*node, n) {
                (Value::I64(a), Number::I64(b)) => {
                    let r = if mul { a.checked_mul(*b) } else { a.checked_add(*b) };
                    *node = Value::I64(r.expect("generator stays in range"));
                }
                (Value::I64(a), Number::F64(b)) => {
                    *node = Value::F64(if mul { *a as f64 * b } else { *a as f64 + b });
                }
                (Value::F64(a), b) => {
                    let b = match b {
                        Number::I64(v) => *v as f64,
                        Number::F64(v) => *v,
                    };
                    *node = Value::F64(if mul { a * b } else { a + b });
                }
                _ => return false,
            }
        }
        OwnedOp::StrAppend(tail) => {
            let Value::Str(s) = node else { return false };
            s.push_str(core::str::from_utf8(tail).expect("test payload is UTF-8"));
        }
        OwnedOp::Toggle => {
            let Value::Bool(b) = node else { return false };
            *b = !*b;
        }
        OwnedOp::Clear => match node {
            Value::Obj(entries) if !entries.is_empty() => entries.clear(),
            Value::Arr(items) if !items.is_empty() => items.clear(),
            Value::I64(v) if *v != 0 => *node = Value::I64(0),
            Value::F64(f) if *f != 0.0 => *node = Value::I64(0),
            _ => return false,
        },
        OwnedOp::SetReplace(value, _) => *node = value.clone(),
        OwnedOp::ArrAppend(values, _) => {
            let Value::Arr(items) = node else { return false };
            items.extend(values.iter().cloned());
        }
        OwnedOp::ArrInsert(index, values, _) => {
            let Value::Arr(items) = node else { return false };
            // The property only reaches here in range (the engine
            // aborted the whole command otherwise — checked there).
            let resolved = if *index < 0 { index + items.len() as i64 } else { *index };
            for (offset, value) in values.iter().enumerate() {
                items.insert(resolved as usize + offset, value.clone());
            }
        }
        OwnedOp::ArrPop(index) => {
            let Value::Arr(items) = node else { return false };
            if items.is_empty() {
                return true; // PoppedEmpty: a real array match, no edit.
            }
            let len = items.len() as i64;
            let resolved = (if *index < 0 { index + len } else { *index }).clamp(0, len - 1);
            items.remove(resolved as usize);
        }
        OwnedOp::ArrTrim(start, stop) => {
            let Value::Arr(items) = node else { return false };
            if items.is_empty() {
                return true; // Len(0) result, no edit.
            }
            let len = items.len() as i64;
            let resolve = |i: i64| if i < 0 { i + len } else { i };
            let first = resolve(*start).max(0);
            let last = resolve(*stop).min(len - 1);
            if first > last {
                items.clear();
            } else {
                *items = items[first as usize..=last as usize].to_vec();
            }
        }
        OwnedOp::Merge(patch, _) => {
            let merged = model_merge(node, patch);
            if merged == *node {
                return false; // Byte-equal merges skip (ADR-0041 D8).
            }
            *node = merged;
        }
        OwnedOp::Del => unreachable!("handled above"),
    }
    true
}

/// Does the op at this pre-state site produce a byte edit? The
/// ADR-0042 D6 supersede set for the retaining ops: a byte-equal merge
/// or a full-window/empty-array trim edits nothing and supersedes
/// nothing.
fn model_produces_edit(root: &Value, steps: &[u32], op: &OwnedOp) -> bool {
    let mut probe = root.clone();
    match op {
        OwnedOp::Merge(..) => model_apply_at(&mut probe, steps, op),
        OwnedOp::ArrTrim(start, stop) => {
            let Value::Arr(items) = model_resolve(&mut probe, steps) else { return false };
            if items.is_empty() {
                return false;
            }
            let len = items.len() as i64;
            let resolve = |i: i64| if i < 0 { i + len } else { i };
            let first = resolve(*start).max(0);
            let last = resolve(*stop).min(len - 1);
            !(first == 0 && last == len - 1)
        }
        _ => unreachable!("only retaining ops consult the supersede set"),
    }
}

/// Commit one retaining operation's replacement as planned from the
/// immutable pre-command snapshot. Re-evaluating the operation against
/// `root` would let a later descendant edit turn a byte-equal ancestor
/// into an edit, which is precisely the cascade semantics ADR-0042 D6
/// rejects.
fn model_apply_snapshot(root: &mut Value, pre: &Value, steps: &[u32], op: &OwnedOp) {
    let mut planned = pre.clone();
    assert!(model_apply_at(&mut planned, steps, op), "caller selected an edit-producing site");
    let replacement = model_resolve(&mut planned, steps).clone();
    *model_resolve(root, steps) = replacement;
}

fn model_resolve<'v>(root: &'v mut Value, steps: &[u32]) -> &'v mut Value {
    let mut node = root;
    for &step in steps {
        node = match node {
            Value::Obj(entries) => &mut entries[step as usize].1,
            Value::Arr(items) => &mut items[step as usize],
            _ => unreachable!("locations resolve through containers"),
        };
    }
    node
}

proptest! {
    // Release AC run: PROPTEST_CASES=1000000 (ledger records the run).
    #[test]
    fn engine_matches_reference_reverse_order_mutation(
        value in arb_value(),
        op in arb_op(),
        path in prop_oneof![
            Just("$..a"), Just("$..b"), Just("$..k"), Just("$.*"),
            Just("$[*]"), Just("$..*"), Just("$.a.b"), Just("$.k[0]"),
        ],
    ) {
        let bytes = model::encode(&value).expect("model encodes");
        let doc = TapeDoc::from_bytes(&bytes).expect("validates");
        let program = compile(path.as_bytes()).expect("compiles");
        // Root matches make Del a command-layer case — skip that pairing.
        let matches = eval(&program, DocValue::from(doc.root()), &EvalLimits::default())
            .expect("eval succeeds");
        let canon = matches.canonical();
        prop_assume!(!(matches!(op, OwnedOp::Del)
            && canon.ids.iter().any(|&id| matches.get(id as usize).is_empty())));

        let applied = apply(&doc, &program, &op.borrow(), &EvalLimits::default(), DOC_BYTES_MAX);
        let outcome = match applied {
            Ok(outcome) => outcome,
            Err(ApplyError::OutOfBounds) => {
                // §3.4 R4: the engine aborted the whole command on an
                // out-of-range ARRINSERT index (no output exists — state
                // is untouched by construction). The reference must
                // agree such a match exists.
                let OwnedOp::ArrInsert(index, _, _) = &op else {
                    return Err(proptest::test_runner::TestCaseError::fail(
                        "only ARRINSERT aborts out of bounds",
                    ));
                };
                let out_of_bounds = canon.ids.iter().any(|&id| {
                    let mut probe = value.clone();
                    match model_resolve(&mut probe, matches.get(id as usize)) {
                        Value::Arr(items) => {
                            let len = items.len() as i64;
                            let resolved = if *index < 0 { index + len } else { *index };
                            !(0..=len).contains(&resolved)
                        }
                        _ => false,
                    }
                });
                prop_assert!(out_of_bounds, "an engine abort implies a reference OOB match");
                return Ok(());
            }
            Err(e) => {
                return Err(proptest::test_runner::TestCaseError::fail(format!(
                    "generated ops stay in range: {e}"
                )));
            }
        };

        // Reference: mutate the model tree in reverse canonical order
        // (§3.4 R5 as written). The retaining ops — Merge and ArrTrim,
        // whose edits copy pre-mutation subranges — pin snapshot
        // semantics instead (ADR-0042 D6): every site's edit computes
        // against the pre-mutation snapshot and a site inside an
        // *edit-producing* ancestor is superseded; the two readings
        // genuinely differ for exactly these ops.
        let mut reference = value.clone();
        let mut reference_applied = 0u32;
        if matches!(op, OwnedOp::Merge(..) | OwnedOp::ArrTrim(..)) {
            let sites: Vec<&[u32]> =
                canon.ids.iter().map(|&id| matches.get(id as usize)).collect();
            let edits: Vec<&[u32]> = sites
                .iter()
                .copied()
                .filter(|steps| model_produces_edit(&value, steps, &op))
                .collect();
            for steps in sites.iter().rev() {
                let superseded = edits.iter().any(|ancestor| {
                    ancestor.len() < steps.len() && **ancestor == steps[..ancestor.len()]
                });
                let mut probe = value.clone();
                if model_apply_at(&mut probe, steps, &op) {
                    reference_applied += 1; // results report pre-state semantics
                }
                let produces_edit = edits.contains(steps);
                if produces_edit && !superseded {
                    model_apply_snapshot(&mut reference, &value, steps, &op);
                }
            }
        } else {
            for &id in canon.ids.iter().rev() {
                if model_apply_at(&mut reference, matches.get(id as usize), &op) {
                    reference_applied += 1;
                }
            }
        }
        let expected = model::encode(&reference).expect("reference encodes");
        match &outcome.bytes {
            Some(bytes) => prop_assert_eq!(bytes, &expected, "engine ≡ reference bytes"),
            None => prop_assert_eq!(&bytes, &expected, "no-op leaves the document unchanged"),
        }
        // Result census agrees (duplicates collapse onto one site).
        let non_skipped =
            outcome.results.iter().filter(|r| !matches!(r, MatchResult::Skipped)).count() as u32;
        prop_assert_eq!(outcome.applied, non_skipped);
        if canon.ids.len() == matches.len() {
            prop_assert_eq!(reference_applied, outcome.applied);
        }
    }
}

// ---- array ops (M3-S13, ADR-0042 D1–D4; oracle-pending S21) -----------------

fn arr_operand(values: &[Value]) -> Vec<u8> {
    let frags: Vec<Vec<u8>> = values.iter().map(fragment).collect();
    let refs: Vec<&[u8]> = frags.iter().map(|f| &f[..]).collect();
    inf_doc::array_operand(&refs).expect("test operands fit the ceiling")
}

#[test]
fn arrappend_appends_and_reports_lengths() {
    let operand = arr_operand(&[Value::I64(3), Value::Str("x".into())]);
    let (json, outcome) =
        applied_json(r#"{"a":[1,2],"n":1}"#, "$.*", &ApplyOp::ArrAppend { elements: &operand });
    assert_eq!(json, r#"{"a":[1,2,3,"x"],"n":1}"#);
    assert_eq!(outcome.results, vec![MatchResult::Len(4), MatchResult::Skipped]);
    assert_eq!(outcome.applied, 1);
}

#[test]
fn arrinsert_positions_include_prepend_append_and_negative() {
    let operand = arr_operand(&[Value::I64(9)]);
    let insert = |index| ApplyOp::ArrInsert { index, elements: &operand };
    assert_eq!(applied_json(r#"[1,2,3]"#, "$", &insert(0)).0, r#"[9,1,2,3]"#);
    assert_eq!(applied_json(r#"[1,2,3]"#, "$", &insert(2)).0, r#"[1,2,9,3]"#);
    // `len` is the before-the-end sentinel: append (ADR-0042 D3 pin).
    assert_eq!(applied_json(r#"[1,2,3]"#, "$", &insert(3)).0, r#"[1,2,3,9]"#);
    assert_eq!(applied_json(r#"[1,2,3]"#, "$", &insert(-1)).0, r#"[1,2,9,3]"#);
    // Index 0 into an empty array is always legal.
    assert_eq!(applied_json(r#"[]"#, "$", &insert(0)).0, r#"[9]"#);
}

#[test]
fn arrinsert_out_of_bounds_aborts_the_whole_command() {
    // Match 2 of 2 is out of bounds: §3.4 R4 — nothing mutates.
    let operand = arr_operand(&[Value::I64(9)]);
    let err = run(
        r#"{"a":[1,2,3],"b":[1]}"#,
        "$.*",
        &ApplyOp::ArrInsert { index: 2, elements: &operand },
    )
    .expect_err("out of bounds aborts");
    assert_eq!(err, ApplyError::OutOfBounds);
    let err = run(r#"[[1]]"#, "$[0]", &ApplyOp::ArrInsert { index: -2, elements: &operand })
        .expect_err("negative past the front aborts");
    assert_eq!(err, ApplyError::OutOfBounds);
}

#[test]
fn arrpop_defaults_clamps_and_reports_pre_image_offsets() {
    // [10,20,30]: fixint elements at body offsets 4, 5, 6.
    let (json, outcome) = applied_json(r#"[10,20,30]"#, "$", &ApplyOp::ArrPop { index: -1 });
    assert_eq!(json, r#"[10,20]"#);
    assert_eq!(outcome.results, vec![MatchResult::Popped(6)]);
    // Out-of-range rounds to the nearest end (ADR-0042 D3).
    let (json, outcome) = applied_json(r#"[10,20,30]"#, "$", &ApplyOp::ArrPop { index: 99 });
    assert_eq!(json, r#"[10,20]"#);
    assert_eq!(outcome.results, vec![MatchResult::Popped(6)]);
    let (json, outcome) = applied_json(r#"[10,20,30]"#, "$", &ApplyOp::ArrPop { index: -99 });
    assert_eq!(json, r#"[20,30]"#);
    assert_eq!(outcome.results, vec![MatchResult::Popped(4)]);
    // Empty arrays pop nothing and mutate nothing.
    let outcome = run(r#"[]"#, "$", &ApplyOp::ArrPop { index: -1 }).expect("empty pop succeeds");
    assert!(outcome.bytes.is_none());
    assert_eq!(outcome.results, vec![MatchResult::PoppedEmpty]);
}

#[test]
fn arrpop_resolves_popped_values_via_value_at() {
    let bytes = tape_of(r#"{"a":[1,{"k":"v"},3]}"#);
    let doc = TapeDoc::from_bytes(&bytes).expect("validates");
    let program = compile(b"$.a").expect("compiles");
    let outcome =
        apply(&doc, &program, &ApplyOp::ArrPop { index: 1 }, &EvalLimits::default(), DOC_BYTES_MAX)
            .expect("pop succeeds");
    let MatchResult::Popped(at) = outcome.results[0] else { panic!("array match pops") };
    let mut text = Vec::new();
    inf_doc::serialize_into(
        inf_doc::DocValue::from(doc.value_at(at as usize)),
        &inf_doc::SerializeOpts::default(),
        &mut text,
    );
    assert_eq!(text, br#"{"k":"v"}"#);
    assert_eq!(json_of(outcome.bytes.as_ref().expect("edit applied")), r#"{"a":[1,3]}"#);
}

#[test]
fn arrtrim_clamps_empties_and_skips_full_windows() {
    let trim = |start, stop| ApplyOp::ArrTrim { start, stop };
    let (json, outcome) = applied_json(r#"[0,1,2,3,4]"#, "$", &trim(1, 3));
    assert_eq!(json, r#"[1,2,3]"#);
    assert_eq!(outcome.results, vec![MatchResult::Len(3)]);
    // Negative resolution + clamping (never a range error).
    assert_eq!(applied_json(r#"[0,1,2,3,4]"#, "$", &trim(-2, 99)).0, r#"[3,4]"#);
    // start > stop / start ≥ len empty the array.
    let (json, outcome) = applied_json(r#"[0,1,2]"#, "$", &trim(2, 1));
    assert_eq!(json, r#"[]"#);
    assert_eq!(outcome.results, vec![MatchResult::Len(0)]);
    let (json, _) = applied_json(r#"[0,1,2]"#, "$", &trim(5, 9));
    assert_eq!(json, r#"[]"#);
    // A window covering everything is a no-op (ADR-0041 D8).
    let outcome = run(r#"[0,1,2]"#, "$", &trim(0, -1)).expect("full window succeeds");
    assert!(outcome.bytes.is_none());
    assert_eq!(outcome.results, vec![MatchResult::Len(3)]);
}

#[test]
fn array_ops_skip_non_arrays() {
    let operand = arr_operand(&[Value::I64(1)]);
    for op in [
        ApplyOp::ArrAppend { elements: &operand },
        ApplyOp::ArrInsert { index: 0, elements: &operand },
        ApplyOp::ArrPop { index: -1 },
        ApplyOp::ArrTrim { start: 0, stop: 0 },
    ] {
        let outcome = run(r#"{"a":1,"s":"x"}"#, "$.*", &op).expect("skips succeed");
        assert!(outcome.bytes.is_none(), "non-arrays skip for {op:?}");
        assert_eq!(outcome.results, vec![MatchResult::Skipped, MatchResult::Skipped]);
    }
}

// ---- MERGE (M3-S14, ADR-0042 D6; oracle-pending S21) -------------------------

fn merge_json(target: &str, path: &str, patch_json: &str) -> String {
    let patch_doc = tape_of(patch_json);
    let patch = &patch_doc[inf_doc::HEADER_LEN..];
    let outcome = run(target, path, &ApplyOp::Merge { patch }).expect("merge succeeds");
    match outcome.bytes {
        Some(bytes) => json_of(&bytes),
        None => json_of(&tape_of(target)),
    }
}

#[test]
fn rfc_7386_appendix_test_vectors() {
    // RFC 7386 Appendix A, applied at the root site.
    let vectors = [
        (r#"{"a":"b"}"#, r#"{"a":"c"}"#, r#"{"a":"c"}"#),
        (r#"{"a":"b"}"#, r#"{"b":"c"}"#, r#"{"a":"b","b":"c"}"#),
        (r#"{"a":"b"}"#, r#"{"a":null}"#, r#"{}"#),
        (r#"{"a":"b","b":"c"}"#, r#"{"a":null}"#, r#"{"b":"c"}"#),
        (r#"{"a":["b"]}"#, r#"{"a":"c"}"#, r#"{"a":"c"}"#),
        (r#"{"a":"c"}"#, r#"{"a":["b"]}"#, r#"{"a":["b"]}"#),
        (r#"{"a":{"b":"c"}}"#, r#"{"a":{"b":"d","c":null}}"#, r#"{"a":{"b":"d"}}"#),
        (r#"{"a":[{"b":"c"}]}"#, r#"{"a":[1]}"#, r#"{"a":[1]}"#),
        (r#"["a","b"]"#, r#"["c","d"]"#, r#"["c","d"]"#),
        (r#"{"a":"b"}"#, r#"["c"]"#, r#"["c"]"#),
        (r#"{"a":"foo"}"#, r#"null"#, r#"null"#),
        (r#"{"a":"foo"}"#, r#""bar""#, r#""bar""#),
        (r#"{"e":null}"#, r#"{"a":1}"#, r#"{"e":null,"a":1}"#),
        (r#"[1,2]"#, r#"{"a":"b","c":null}"#, r#"{"a":"b"}"#),
        (r#"{}"#, r#"{"a":{"bb":{"ccc":null}}}"#, r#"{"a":{"bb":{}}}"#),
    ];
    for (target, patch, want) in vectors {
        assert_eq!(merge_json(target, "$", patch), want, "MergePatch({target}, {patch})");
    }
}

#[test]
fn merge_null_is_literal_at_every_selected_value() {
    assert_eq!(merge_json(r#"{"a":1,"b":2}"#, "$.a", "null"), r#"{"a":null,"b":2}"#);
    assert_eq!(merge_json(r#"[1,2]"#, "$[0]", "null"), r#"[null,2]"#);
    assert_eq!(merge_json(r#"{"a":1}"#, "$", "null"), r#"null"#);
}

#[test]
fn merge_preserves_key_positions_and_appends_new_keys_in_patch_order() {
    assert_eq!(
        merge_json(r#"{"a":1,"b":2}"#, "$", r#"{"b":9,"z":1,"c":{"n":null,"k":1}}"#),
        r#"{"a":1,"b":9,"z":1,"c":{"k":1}}"#
    );
}

#[test]
fn merge_multi_match_and_nested_sites() {
    assert_eq!(
        merge_json(r#"{"x":{"m":1},"y":{"m":2}}"#, "$.*", r#"{"m":null,"n":7}"#),
        r#"{"x":{"n":7},"y":{"n":7}}"#
    );
}

#[test]
fn merge_overlapping_sites_pin_snapshot_semantics() {
    // `$..*` matches the object and its null member. Both merges compute
    // against the pre-mutation snapshot and the changed ancestor
    // supersedes the contained site (ADR-0042 D6 — the differential
    // found this divergence; the pin is explicit).
    // Reverse-order cascade semantics would answer
    // `[{"b":{"a":false},"a":false}]` instead.
    assert_eq!(
        merge_json(r#"[{"b":null}]"#, "$..*", r#"{"a":false}"#),
        r#"[{"b":null,"a":false}]"#
    );
    // An ancestor whose merge is byte-equal produces no edit and
    // supersedes nothing: `$..*` matches o's value (merge is a no-op —
    // b already holds {"k":1}), b's value (gains the "b" member), and
    // k's value (superseded by its changed parent).
    assert_eq!(
        merge_json(r#"{"o":{"b":{"k":1}}}"#, "$..*", r#"{"b":{"k":1}}"#),
        r#"{"o":{"b":{"k":1,"b":{"k":1}}}}"#
    );
}

#[test]
fn arrtrim_overlapping_sites_pin_snapshot_semantics() {
    // `$..*` matches the outer element (an array) and its inner array.
    // The ancestor trim's kept window copies pre-mutation bytes and
    // supersedes the inner trim (ADR-0042 D6 — found by the 100k
    // differential; reverse-order cascade would answer `[[[]]]`).
    let (json, _) =
        applied_json(r#"[[null,[null]]]"#, "$..*", &ApplyOp::ArrTrim { start: 1, stop: 1 });
    assert_eq!(json, r#"[[[null]]]"#);
}

#[test]
fn merge_of_empty_patch_is_a_no_op() {
    let patch_doc = tape_of("{}");
    let patch = &patch_doc[inf_doc::HEADER_LEN..];
    let outcome = run(r#"{"a":1}"#, "$", &ApplyOp::Merge { patch }).expect("no-op succeeds");
    assert!(outcome.bytes.is_none(), "byte-equal merge must not rewrite (ADR-0041 D8)");
    assert_eq!(outcome.applied, 0);
}

#[test]
fn merge_absent_document_strips_nulls_through_object_chains_only() {
    let strip = |patch_json: &str| {
        let doc = tape_of(patch_json);
        json_of(&inf_doc::merge_absent_document(&doc[inf_doc::HEADER_LEN..]))
    };
    assert_eq!(strip(r#"{"a":1,"b":null,"c":{"d":null,"e":2}}"#), r#"{"a":1,"c":{"e":2}}"#);
    // Arrays and scalars are literal — nulls inside arrays survive.
    assert_eq!(strip(r#"{"a":[null,1]}"#), r#"{"a":[null,1]}"#);
    assert_eq!(strip(r#"[null]"#), r#"[null]"#);
    assert_eq!(strip("null"), "null");
    assert_eq!(strip("3"), "3");
}
