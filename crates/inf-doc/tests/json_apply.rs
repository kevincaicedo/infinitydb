//! M3-S11/S12 apply-engine suite (ADR-0041 D5/D8): table-pinned scalar
//! mutation semantics (`oracle-pending` where RedisJSON's byte behavior
//! is unverifiable without the S21 container — every such pin is marked),
//! the §3.4 R4 abort contract, the R5 overlap corpus, and a differential
//! property against an independent reference mutation over the model
//! tree applied in **reverse canonical order** — the literal R5 wording,
//! which the engine realizes as a containment drop; equal bytes prove
//! the realization faithful.

use inf_doc::apply::{ApplyError, ApplyOp, ApplyOutcome, MatchResult, Number, apply};
use inf_doc::limits::DEPTH_MAX;
use inf_doc::model::{self, Value};
use inf_doc::path::{EvalLimits, compile, eval};
use inf_doc::{
    CanonicalDoc, DeltaDecodeError, DeltaOpcode, DocLimits, DocValue, JsonParser, TapeDoc, Written,
    decode_apply_op, encode_apply_op, serialize_canonical_into,
};
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
    apply(&doc, &program, op, &EvalLimits::default(), DocLimits::FORMAT)
}

fn applied_json(json: &str, path: &str, op: &ApplyOp<'_>) -> (String, ApplyOutcome) {
    let outcome = run(json, path, op).expect("apply succeeds");
    let document = outcome.document.as_ref().expect("an edit applied");
    (json_of(document.as_bytes()), outcome.clone())
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
    assert!(outcome.document.is_none());
    assert_eq!(outcome.applied, 0);
}

/// Review C10: an index the evaluator used to wrap onto element 0 must
/// be a no-op on the mutation lane — for DEL, SET, and the numeric op
/// through the canonical `apply` path (the non-simple shapes that skip
/// the in-place probe), and through a giant-step slice (C11).
#[test]
fn indices_beyond_the_u32_width_mutate_nothing() {
    let frag = fragment(&Value::I64(999));
    for path in ["$[4294967296]", "$[4294967297]", "$[8589934592]", "$[9223372036854775807]"] {
        let outcome = run(r#"[10,20,30]"#, path, &ApplyOp::Del).expect("no-op succeeds");
        assert!(outcome.document.is_none(), "DEL {path} must not edit");
        assert_eq!(outcome.applied, 0);
        let outcome = run(r#"[10,20,30]"#, path, &ApplyOp::SetReplace { fragment: &frag })
            .expect("no-op succeeds");
        assert!(outcome.document.is_none(), "SET {path} must not edit");
        assert_eq!(outcome.applied, 0);
    }
    let union = "$[4294967296,4294967296]";
    let outcome =
        run(r#"[10,20,30]"#, union, &ApplyOp::NumIncrBy(Number::I64(1000))).expect("no-op");
    assert!(outcome.document.is_none(), "NUMINCRBY {union} must not edit");
    assert_eq!(outcome.applied, 0);
    // The slice cursor at i64::MAX: exactly one element, then the walk ends.
    let (json, outcome) = applied_json(
        r#"[10,20,30]"#,
        "$[1::9223372036854775807]",
        &ApplyOp::NumIncrBy(Number::I64(1)),
    );
    assert_eq!(json, r#"[10,21,30]"#);
    assert_eq!(outcome.applied, 1);
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
    assert!(outcome.document.is_none());
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
        DocLimits::new(DEPTH_MAX, bytes.len()), // a cap the grown document must exceed
    )
    .expect_err("cap binds");
    assert_eq!(err, ApplyError::TooLarge);
}

#[test]
fn all_skipped_is_a_no_op_with_no_bytes() {
    let outcome = run(r#"{"a":"s"}"#, "$.a", &ApplyOp::Toggle).expect("skip succeeds");
    assert!(outcome.document.is_none(), "no edit ⇒ no rewrite ⇒ no version bump (ADR-0041 D8)");
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
    /// A member write at the matched object: its key, value and fragment.
    SetMember(String, Value, Vec<u8>),
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
            OwnedOp::SetMember(key, _, frag) => {
                ApplyOp::SetMember { key: key.as_bytes(), fragment: frag }
            }
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
        OwnedOp::SetMember(key, value, _) => {
            let Value::Obj(entries) = node else { return false };
            match entries.iter_mut().find(|(k, _)| k == key) {
                Some((_, slot)) => *slot = value.clone(),
                None => entries.push((key.clone(), value.clone())),
            }
        }
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

        let applied =
            apply(&doc, &program, &op.borrow(), &EvalLimits::default(), DocLimits::FORMAT);
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
        match &outcome.document {
            Some(document) => {
                prop_assert_eq!(document.as_bytes(), &expected[..], "engine ≡ reference bytes")
            }
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
    assert!(outcome.document.is_none());
    assert_eq!(outcome.results, vec![MatchResult::PoppedEmpty]);
}

#[test]
fn arrpop_resolves_popped_values_via_value_at() {
    let bytes = tape_of(r#"{"a":[1,{"k":"v"},3]}"#);
    let doc = TapeDoc::from_bytes(&bytes).expect("validates");
    let program = compile(b"$.a").expect("compiles");
    let outcome = apply(
        &doc,
        &program,
        &ApplyOp::ArrPop { index: 1 },
        &EvalLimits::default(),
        DocLimits::FORMAT,
    )
    .expect("pop succeeds");
    let MatchResult::Popped(at) = outcome.results[0] else { panic!("array match pops") };
    let mut text = Vec::new();
    inf_doc::serialize_into(
        inf_doc::DocValue::from(doc.value_at(at as usize)),
        &inf_doc::SerializeOpts::default(),
        &mut text,
    );
    assert_eq!(text, br#"{"k":"v"}"#);
    assert_eq!(
        json_of(outcome.document.as_ref().expect("edit applied").as_bytes()),
        r#"{"a":[1,3]}"#
    );
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
    assert!(outcome.document.is_none());
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
        assert!(outcome.document.is_none(), "non-arrays skip for {op:?}");
        assert_eq!(outcome.results, vec![MatchResult::Skipped, MatchResult::Skipped]);
    }
}

// ---- MERGE (M3-S14, ADR-0042 D6; oracle-pending S21) -------------------------

fn merge_json(target: &str, path: &str, patch_json: &str) -> String {
    let patch_doc = tape_of(patch_json);
    let patch = &patch_doc[inf_doc::HEADER_LEN..];
    let outcome = run(target, path, &ApplyOp::Merge { patch }).expect("merge succeeds");
    match outcome.document {
        Some(document) => json_of(document.as_bytes()),
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
    assert!(outcome.document.is_none(), "byte-equal merge must not rewrite (ADR-0041 D8)");
    assert_eq!(outcome.applied, 0);
}

#[test]
fn merge_absent_document_strips_nulls_through_object_chains_only() {
    let strip = |patch_json: &str| {
        let doc = tape_of(patch_json);
        json_of(inf_doc::merge_absent_document(&doc[inf_doc::HEADER_LEN..]).as_bytes())
    };
    assert_eq!(strip(r#"{"a":1,"b":null,"c":{"d":null,"e":2}}"#), r#"{"a":1,"c":{"e":2}}"#);
    // Arrays and scalars are literal — nulls inside arrays survive.
    assert_eq!(strip(r#"{"a":[null,1]}"#), r#"{"a":[null,1]}"#);
    assert_eq!(strip(r#"[null]"#), r#"[null]"#);
    assert_eq!(strip("null"), "null");
    assert_eq!(strip("3"), "3");
}

// ---- the depth cliff (ADR-0169 D3, Falsifier 1) --------------------------------

/// JSON text: `levels` nested arrays around `inner`.
fn nested_arrays(levels: usize, inner: &str) -> String {
    format!("{}{inner}{}", "[".repeat(levels), "]".repeat(levels))
}

/// JSON text: `levels` nested objects `{"a":…}` around `inner`.
fn nested_objects(levels: usize, inner: &str) -> String {
    format!("{}{inner}{}", r#"{"a":"#.repeat(levels), "}".repeat(levels))
}

/// The header-less canonical fragment of `json`.
fn fragment_of(json: &str) -> Vec<u8> {
    tape_of(json)[inf_doc::HEADER_LEN..].to_vec()
}

/// A replay reader refuses nothing `apply` returns: the check the replay
/// validator makes of every `DocFull` and checkpoint image.
fn assert_replays(result: &Result<ApplyOutcome, ApplyError>, what: &str) {
    if let Ok(ApplyOutcome { document: Some(document), .. }) = result {
        let reread = TapeDoc::from_bytes(document.as_bytes()).map(|_| ());
        assert_eq!(reread, Ok(()), "{what}: apply returned a document replay refuses");
    }
}

/// The five ops that compose a site's depth with an operand's nesting.
#[derive(Copy, Clone, Debug)]
enum Deepening {
    SetReplace,
    SetMember,
    Merge,
    ArrAppend,
    ArrInsert,
}

/// Where a row's sites sit. ADR-0169 D3 counts only kept edits and the
/// deepest one decides, so the spread layouts keep two sites at different
/// depths, the deeper one first and then last in document order.
#[derive(Copy, Clone, Debug)]
enum Layout {
    /// `$.d` over `{"d":S}`: one site, enclosed by the root.
    Single,
    /// `$..d` over `[{"c":{"d":S}},{"d":S}]`: sites enclosed by 3 and 2.
    SpreadDeepFirst,
    /// `$..d` over `[{"d":S},{"c":{"d":S}}]`: sites enclosed by 2 and 3.
    SpreadDeepLast,
}

impl Layout {
    const ALL: [Layout; 3] = [Layout::Single, Layout::SpreadDeepFirst, Layout::SpreadDeepLast];

    /// The pre-image around `site` (the site value's JSON text) and the
    /// path that matches every site.
    fn doc(self, site: &str) -> (String, &'static str) {
        let shallow = format!(r#"{{"d":{site}}}"#);
        let deep = format!(r#"{{"c":{shallow}}}"#);
        match self {
            Layout::Single => (shallow, "$.d"),
            Layout::SpreadDeepFirst => (format!("[{deep},{shallow}]"), "$..d"),
            Layout::SpreadDeepLast => (format!("[{shallow},{deep}]"), "$..d"),
        }
    }

    /// The containers enclosing the deepest site.
    fn deepest_enclosing(self) -> usize {
        match self {
            Layout::Single => 1,
            Layout::SpreadDeepFirst | Layout::SpreadDeepLast => 3,
        }
    }

    fn sites(self) -> u32 {
        match self {
            Layout::Single => 1,
            Layout::SpreadDeepFirst | Layout::SpreadDeepLast => 2,
        }
    }
}

/// One deepening op at composed depth `composed` at its layout's deepest
/// site, per ADR-0169 D3's table: the containers enclosing the written
/// bytes plus what the op writes.
struct CliffRow {
    kind: Deepening,
    doc: String,
    path: &'static str,
    operand: Vec<u8>,
}

impl CliffRow {
    fn new(kind: Deepening, layout: Layout, composed: usize) -> CliffRow {
        let below = composed - layout.deepest_enclosing();
        let (site, operand) = match kind {
            // The fragment replaces the site: its own nesting.
            Deepening::SetReplace => ("0", fragment_of(&nested_arrays(below, "0"))),
            // The member lands inside the site: one more.
            Deepening::SetMember => ("{}", fragment_of(&nested_arrays(below - 1, "0"))),
            // An object patch merged into a scalar keeps every container.
            Deepening::Merge => ("0", fragment_of(&nested_objects(below, "0"))),
            // Elements land inside the site: one more.
            Deepening::ArrAppend | Deepening::ArrInsert => {
                let element = fragment_of(&nested_arrays(below - 1, "0"));
                let operand =
                    inf_doc::array_operand(&[&element]).expect("a 127-level element wraps");
                ("[]", operand)
            }
        };
        let (doc, path) = layout.doc(site);
        CliffRow { kind, doc, path, operand }
    }

    fn op(&self) -> ApplyOp<'_> {
        match self.kind {
            Deepening::SetReplace => ApplyOp::SetReplace { fragment: &self.operand },
            Deepening::SetMember => ApplyOp::SetMember { key: b"x", fragment: &self.operand },
            Deepening::Merge => ApplyOp::Merge { patch: &self.operand },
            Deepening::ArrAppend => ApplyOp::ArrAppend { elements: &self.operand },
            Deepening::ArrInsert => ApplyOp::ArrInsert { index: 0, elements: &self.operand },
        }
    }
}

/// One cliff row: accepted with every site kept at composed depth 127 and
/// 128, refused at 129, and never a document the replay validator refuses.
fn assert_cliff_row(kind: Deepening, layout: Layout, composed: usize) {
    let row = CliffRow::new(kind, layout, composed);
    let result = run(&row.doc, row.path, &row.op());
    let what = format!("{kind:?} {layout:?} at composed depth {composed}");
    assert_replays(&result, &what);
    if composed <= DEPTH_MAX {
        let outcome = result.expect(&what);
        assert!(outcome.document.is_some(), "{what}: an edit applied");
        assert_eq!(outcome.applied, layout.sites(), "{what}: every site applied");
    } else {
        assert_eq!(result.err(), Some(ApplyError::DepthExceeded), "{what}: refused");
    }
}

/// ADR-0169 Falsifier 1: the five deepening ops accept at composed depth
/// 127 and 128 and refuse 129, measured at the deepest kept site whether it
/// comes first or last; the other eight never refuse at a container site
/// enclosed by `DEPTH_MAX − 1` containers or a scalar site enclosed by
/// `DEPTH_MAX`. Every document `apply` returns passes the replay validator
/// — the pre-fix engine returned a 129-level one.
#[test]
fn apply_output_replays_at_the_depth_cliff() {
    let kinds = [
        Deepening::SetReplace,
        Deepening::SetMember,
        Deepening::Merge,
        Deepening::ArrAppend,
        Deepening::ArrInsert,
    ];
    for composed in [DEPTH_MAX - 1, DEPTH_MAX, DEPTH_MAX + 1] {
        for layout in Layout::ALL {
            for kind in kinds {
                assert_cliff_row(kind, layout, composed);
            }
        }
    }
    let site = nested_arrays(DEPTH_MAX - 1, r#"[1,"s",true]"#);
    let container = format!("${}", "[0]".repeat(DEPTH_MAX - 1));
    let scalar = |index: usize| format!("{container}[{index}]");
    let unchecked: [(String, ApplyOp<'_>); 10] = [
        (container.clone(), ApplyOp::Del),
        (container.clone(), ApplyOp::Clear),
        (container.clone(), ApplyOp::ArrPop { index: -1 }),
        (container.clone(), ApplyOp::ArrTrim { start: 0, stop: 0 }),
        (scalar(0), ApplyOp::NumIncrBy(Number::I64(1))),
        (scalar(0), ApplyOp::NumMultBy(Number::I64(3))),
        (scalar(1), ApplyOp::StrAppend(b"+t")),
        (scalar(2), ApplyOp::Toggle),
        (scalar(0), ApplyOp::Del),
        (scalar(0), ApplyOp::Clear),
    ];
    for (path, op) in &unchecked {
        let result = run(&site, path, op);
        let what = format!("{op:?} at the deepest site");
        assert_replays(&result, &what);
        let outcome = result.expect(&what);
        assert!(outcome.document.is_some(), "{what}: an edit applied");
    }
}

/// ADR-0169 D3 exactness: a match superseded by a kept ancestor edit is
/// absent from the output, so it cannot refuse it. `$..a` over
/// `{"a":{"a":{"a":0}}}` keeps only `$.a`'s replacement: a 127-level
/// fragment composes 128 there while the dropped inner matches would
/// compose 129 and 130; one level more composes 129 at the kept edit.
#[test]
fn superseded_matches_do_not_count_toward_the_depth() {
    let doc = r#"{"a":{"a":{"a":0}}}"#;
    let kept = wrap(&[false; DEPTH_MAX - 1], Value::I64(0));
    let result = run(doc, "$..a", &ApplyOp::SetReplace { fragment: &fragment(&kept) });
    assert_replays(&result, "the kept edit at composed depth 128");
    let outcome = result.expect("the kept edit composes 128");
    let document = outcome.document.expect("an edit applied");
    let model = model::encode(&Value::Obj(vec![("a".into(), kept)])).expect("128 levels encode");
    assert_eq!(document.as_bytes(), &model[..], "the output is the kept edit alone");
    let past = wrap(&[false; DEPTH_MAX], Value::I64(0));
    let result = run(doc, "$..a", &ApplyOp::SetReplace { fragment: &fragment(&past) });
    assert_eq!(result.err(), Some(ApplyError::DepthExceeded), "the kept edit composes 129");
}

/// ADR-0169 D3: `array_operand` refuses an element the operand decoder
/// would refuse inside its wrapper — a 128-level element nests the wrapper
/// 129 — and wraps a 127-level one into an operand `decode_apply_op` takes.
#[test]
fn array_operand_refuses_what_decode_refuses() {
    for levels in [DEPTH_MAX - 2, DEPTH_MAX - 1, DEPTH_MAX] {
        let element = fragment_of(&nested_arrays(levels, "0"));
        match inf_doc::array_operand(&[&element]) {
            Ok(operand) => assert_wrapped_operand_decodes(levels, &operand),
            Err(error) => assert_eq!((levels, error), (DEPTH_MAX, ApplyError::DepthExceeded)),
        }
    }
}

fn assert_wrapped_operand_decodes(levels: usize, operand: &[u8]) {
    let decoded = decode_apply_op(DeltaOpcode::ArrAppend as u8, operand);
    assert!(
        decoded.is_ok(),
        "array_operand wrapped a {levels}-level element the decoder refuses: {decoded:?}"
    );
    assert!(levels < DEPTH_MAX, "a {levels}-level element was wrapped");
}

// ---- writer/reader agreement over the opcode table (ADR-0169 Falsifier 2) -------

/// One generated case: a pre-image whose single match sits under
/// `site_enclosing` containers, the path to it, and the op.
#[derive(Clone, Debug)]
struct AgreementCase {
    doc: Value,
    steps: Vec<u32>,
    path: String,
    op: OwnedOp,
    site_enclosing: usize,
}

/// What `apply` did with a case, in bytes a planted canary can forge.
#[derive(Debug)]
enum Observed {
    Document(Vec<u8>),
    NoEdit,
    Refused(ApplyError),
}

/// The checker's verdict on one agreeing case.
#[derive(Copy, Clone, Debug, PartialEq)]
enum Judgement {
    Accepted { model_depth: usize },
    Refused { model_depth: usize },
    NoEdit,
}

/// A disagreement between the writer and a reader, or the model.
#[derive(Debug, PartialEq)]
enum Violation {
    /// The replay validator refuses a returned document (canary a).
    OutputRefused(inf_doc::DocError),
    /// The durable operand does not decode (canary b).
    OperandRefused(DeltaDecodeError),
    /// Re-executing the decoded operand does not reproduce the bytes.
    ReplayDiffers,
    /// The engine's bytes differ from the model's output.
    ModelDiffers,
    /// A document the independent model nests past `DEPTH_MAX`.
    AcceptedPastBound { model_depth: usize },
    /// A depth refusal the independent model nests within `DEPTH_MAX`
    /// (canary c).
    RefusedWithinBound { model_depth: usize },
    /// Any other refusal: the generator keeps every op in range.
    Unexpected(ApplyError),
}

/// Containers on the deepest path: 0 for a scalar. Recursive: test code
/// over at most `DEPTH_MAX + 2` levels, independent of the engine's walk.
fn model_nesting(value: &Value) -> usize {
    match value {
        Value::Obj(entries) => 1 + entries.iter().map(|(_, v)| model_nesting(v)).max().unwrap_or(0),
        Value::Arr(items) => 1 + items.iter().map(model_nesting).max().unwrap_or(0),
        Value::Null | Value::Bool(_) | Value::I64(_) | Value::F64(_) | Value::Str(_) => 0,
    }
}

fn observe(case: &AgreementCase) -> Observed {
    let bytes = model::encode(&case.doc).expect("the generator stays within the format");
    let doc = TapeDoc::from_bytes(&bytes).expect("validates");
    let program = compile(case.path.as_bytes()).expect("generated path compiles");
    match apply(&doc, &program, &case.op.borrow(), &EvalLimits::default(), DocLimits::FORMAT) {
        Ok(ApplyOutcome { document: Some(document), .. }) => {
            Observed::Document(document.as_bytes().to_vec())
        }
        Ok(ApplyOutcome { document: None, .. }) => Observed::NoEdit,
        Err(error) => Observed::Refused(error),
    }
}

/// ADR-0169 Falsifier 2's checker. A returned document must pass the
/// replay validator, its operand must decode, and re-executing the decoded
/// operand under replay's recorded bound must reproduce it; a depth
/// refusal must coincide with the model's output nesting past `DEPTH_MAX`.
fn judge(case: &AgreementCase, observed: Observed) -> Result<Judgement, Violation> {
    let mut model_out = case.doc.clone();
    model_apply_at(&mut model_out, &case.steps, &case.op);
    let model_depth = model_nesting(&model_out);
    let bytes = match observed {
        Observed::NoEdit => return Ok(Judgement::NoEdit),
        Observed::Refused(ApplyError::DepthExceeded) => {
            return match model_depth > DEPTH_MAX {
                true => Ok(Judgement::Refused { model_depth }),
                false => Err(Violation::RefusedWithinBound { model_depth }),
            };
        }
        Observed::Refused(error) => return Err(Violation::Unexpected(error)),
        Observed::Document(bytes) => bytes,
    };
    TapeDoc::from_bytes(&bytes).map_err(Violation::OutputRefused)?;
    let op = case.op.borrow();
    let mut operand = Vec::new();
    let opcode = encode_apply_op(&op, &mut operand);
    let decoded = decode_apply_op(opcode as u8, &operand).map_err(Violation::OperandRefused)?;
    let pre = model::encode(&case.doc).expect("the generator stays within the format");
    let pre = TapeDoc::from_bytes(&pre).expect("validates");
    let program = compile(case.path.as_bytes()).expect("generated path compiles");
    // Replay's bound: the recorded idoc length less the header (ADR-0169 D2).
    let recorded = DocLimits::new(DEPTH_MAX, bytes.len() - inf_doc::HEADER_LEN);
    let replayed = apply(&pre, &program, &decoded, &EvalLimits { max_matches: 1 }, recorded);
    let again = replayed.ok().and_then(|outcome| outcome.document);
    if again.as_ref().map(CanonicalDoc::as_bytes) != Some(&bytes[..]) {
        return Err(Violation::ReplayDiffers);
    }
    if model_depth > DEPTH_MAX {
        return Err(Violation::AcceptedPastBound { model_depth });
    }
    if model::encode(&model_out).ok().as_deref() != Some(&bytes[..]) {
        return Err(Violation::ModelDiffers);
    }
    Ok(Judgement::Accepted { model_depth })
}

/// `leaf` under `kinds.len()` containers, outermost first: `true` is an
/// object `{"a":…}`, `false` an array `[…]`.
fn wrap(kinds: &[bool], leaf: Value) -> Value {
    kinds.iter().rev().fold(leaf, |inner, &object| match object {
        true => Value::Obj(vec![("a".into(), inner)]),
        false => Value::Arr(vec![inner]),
    })
}

fn case_at(kinds: &[bool], site: Value, op: OwnedOp) -> AgreementCase {
    let mut path = String::from("$");
    for &object in kinds {
        path.push_str(if object { ".a" } else { "[0]" });
    }
    AgreementCase {
        doc: wrap(kinds, site),
        steps: vec![0; kinds.len()],
        path,
        op,
        site_enclosing: kinds.len(),
    }
}

fn arb_kinds(levels: usize) -> impl Strategy<Value = Vec<bool>> {
    proptest::collection::vec(any::<bool>(), levels)
}

/// Composed-depth targets: both sides of the cliff, weighted, and around.
fn arb_composed() -> impl Strategy<Value = usize> {
    prop_oneof![
        2 => Just(DEPTH_MAX),
        2 => Just(DEPTH_MAX + 1),
        1 => (DEPTH_MAX - 3)..=(DEPTH_MAX + 2),
    ]
}

/// A deepening case: the site under `enclosing` containers and an operand
/// nesting `composed − enclosing − inside`, where `inside` is 1 when the op
/// writes inside the matched container (ADR-0169 D3).
fn arb_deepening(
    inside: usize,
    site_max: usize,
    build: fn(Vec<bool>, Value) -> AgreementCase,
) -> BoxedStrategy<AgreementCase> {
    arb_composed()
        .prop_flat_map(move |composed| {
            let low = composed.saturating_sub(DEPTH_MAX + inside);
            let high = site_max.min(composed - inside);
            (Just(composed), low..=high)
        })
        .prop_flat_map(move |(composed, enclosing)| {
            (arb_kinds(enclosing), arb_kinds(composed - enclosing - inside))
        })
        .prop_map(move |(site, operand)| build(site, wrap(&operand, Value::I64(7))))
        .boxed()
}

/// A site under `DEPTH_MAX − 1` or `DEPTH_MAX` containers (weighted), or
/// shallower: the unchecked ops' deepest reach.
fn arb_deep_site(site_max: usize) -> impl Strategy<Value = Vec<bool>> {
    prop_oneof![3 => Just(site_max), 1 => (site_max - 4)..=site_max].prop_flat_map(arb_kinds)
}

fn arb_op_for(opcode: DeltaOpcode) -> BoxedStrategy<AgreementCase> {
    let number = Value::I64(5);
    let array = Value::Arr(vec![Value::I64(1), Value::I64(2), Value::I64(3)]);
    match opcode {
        DeltaOpcode::SetReplace => arb_deepening(0, DEPTH_MAX, |site, value| {
            let frag = fragment(&value);
            case_at(&site, Value::I64(1), OwnedOp::SetReplace(value, frag))
        }),
        DeltaOpcode::SetMember => arb_deepening(1, DEPTH_MAX - 1, |site, value| {
            let frag = fragment(&value);
            let op = OwnedOp::SetMember("m".into(), value, frag);
            case_at(&site, Value::Obj(vec![("b".into(), Value::I64(1))]), op)
        }),
        DeltaOpcode::Merge => arb_deepening(0, DEPTH_MAX, |site, value| {
            let patch = match value {
                Value::Arr(items) => Value::Obj(vec![("p".into(), Value::Arr(items))]),
                other => other,
            };
            let frag = fragment(&patch);
            case_at(&site, Value::I64(1), OwnedOp::Merge(patch, frag))
        })
        .prop_filter("an object patch keeps the target's nesting", |case| {
            let OwnedOp::Merge(patch, _) = &case.op else { return true };
            case.site_enclosing + model_nesting(patch) <= DEPTH_MAX + 2
        })
        .boxed(),
        DeltaOpcode::ArrAppend => arb_deepening(1, DEPTH_MAX - 1, |site, value| {
            let values = vec![Value::I64(0), value];
            let operand = arr_operand(&values);
            case_at(&site, Value::Arr(vec![Value::I64(1)]), OwnedOp::ArrAppend(values, operand))
        }),
        DeltaOpcode::ArrInsert => arb_deepening(1, DEPTH_MAX - 1, |site, value| {
            let values = vec![value];
            let operand = arr_operand(&values);
            let op = OwnedOp::ArrInsert(-1, values, operand);
            case_at(&site, Value::Arr(vec![Value::I64(1)]), op)
        }),
        DeltaOpcode::Del => arb_deep_site(DEPTH_MAX)
            .prop_map(move |site| case_at(&site, number.clone(), OwnedOp::Del))
            .boxed(),
        DeltaOpcode::NumIncrBy => arb_deep_site(DEPTH_MAX)
            .prop_map(move |site| {
                case_at(&site, number.clone(), OwnedOp::NumIncrBy(Number::I64(3)))
            })
            .boxed(),
        DeltaOpcode::NumMultBy => arb_deep_site(DEPTH_MAX)
            .prop_map(move |site| {
                case_at(&site, number.clone(), OwnedOp::NumMultBy(Number::F64(1.5)))
            })
            .boxed(),
        DeltaOpcode::StrAppend => arb_deep_site(DEPTH_MAX)
            .prop_map(|site| {
                case_at(&site, Value::Str("s".into()), OwnedOp::StrAppend(b"+t".to_vec()))
            })
            .boxed(),
        DeltaOpcode::Toggle => arb_deep_site(DEPTH_MAX)
            .prop_map(|site| case_at(&site, Value::Bool(true), OwnedOp::Toggle))
            .boxed(),
        DeltaOpcode::Clear => arb_deep_site(DEPTH_MAX - 1)
            .prop_map(move |site| case_at(&site, array.clone(), OwnedOp::Clear))
            .boxed(),
        DeltaOpcode::ArrPop => (arb_deep_site(DEPTH_MAX - 1), -4i64..4)
            .prop_map(move |(site, index)| case_at(&site, array.clone(), OwnedOp::ArrPop(index)))
            .boxed(),
        DeltaOpcode::ArrTrim => (arb_deep_site(DEPTH_MAX - 1), 0i64..2, 0i64..2)
            .prop_map(move |(site, start, stop)| {
                case_at(&site, array.clone(), OwnedOp::ArrTrim(start, stop))
            })
            .boxed(),
    }
}

/// What one opcode's run reached (ADR-0169 Falsifier 2's engagement).
#[derive(Default, Debug)]
struct Reach {
    checked: bool,
    accepted_at_bound: bool,
    refused_past_bound: bool,
    applied_deep: bool,
}

impl Reach {
    fn note(&mut self, case: &AgreementCase, judgement: Judgement) {
        // The op's own table row decides, never a list in this test.
        self.checked = !matches!(case.op.borrow().written(), Written::WithinPreImage);
        match judgement {
            Judgement::Accepted { model_depth } => {
                self.accepted_at_bound |= model_depth == DEPTH_MAX;
                self.applied_deep |= case.site_enclosing >= DEPTH_MAX - 1;
            }
            Judgement::Refused { model_depth } => {
                self.refused_past_bound |= model_depth == DEPTH_MAX + 1;
            }
            Judgement::NoEdit => {}
        }
    }

    fn engaged(&self) -> bool {
        match self.checked {
            true => self.accepted_at_bound && self.refused_past_bound,
            false => self.applied_deep,
        }
    }
}

const AGREEMENT_CASES_PER_OPCODE: u32 = 64;

/// ADR-0169 Falsifier 2 over every opcode the delta codec knows: the
/// writer never returns a document or an operand a reader refuses, replay
/// reproduces its bytes, and a depth refusal happens exactly when the
/// independent model nests past `DEPTH_MAX`. An opcode that never reached
/// its cliff (checked) or its deepest site (unchecked) is VACUOUS — red.
#[test]
fn writer_reader_agreement_over_the_opcode_table() {
    use proptest::test_runner::{Config, RngAlgorithm, TestRng, TestRunner};
    let config = Config {
        cases: AGREEMENT_CASES_PER_OPCODE,
        failure_persistence: None,
        ..Config::default()
    };
    for &opcode in DeltaOpcode::ALL {
        let reach = std::cell::RefCell::new(Reach::default());
        let rng = TestRng::deterministic_rng(RngAlgorithm::ChaCha);
        let mut runner = TestRunner::new_with_rng(config.clone(), rng);
        let verdict = runner.run(&arb_op_for(opcode), |case| {
            let judgement = judge(&case, observe(&case))
                .map_err(|violation| TestCaseError::fail(format!("{violation:?}")))?;
            reach.borrow_mut().note(&case, judgement);
            Ok(())
        });
        if let Err(failure) = verdict {
            panic!("{opcode:?}: {failure}");
        }
        let reach = reach.into_inner();
        assert!(reach.engaged(), "VACUOUS: {opcode:?} never reached its cliff: {reach:?}");
    }
}

/// Raw bytes: one more array around a canonical body, past the builder's
/// own refusal, as a planted canary needs.
fn raw_array_around(body: &[u8]) -> Vec<u8> {
    let len = u32::try_from(body.len()).expect("a canary body is small");
    let mut out = vec![0xA8];
    out.extend_from_slice(&len.to_le_bytes()[..3]);
    out.extend_from_slice(body);
    out
}

/// Raw bytes: a v1 header over `body`.
fn raw_document(body: &[u8]) -> Vec<u8> {
    let mut out = tape_of("0")[..inf_doc::HEADER_LEN].to_vec();
    out[4..8].copy_from_slice(&u32::try_from(body.len()).expect("small").to_le_bytes());
    out.extend_from_slice(body);
    out
}

/// The checker's canaries (ADR-0169 Falsifier 2), in the property's own
/// binary: each planted disagreement must turn it red with its typed
/// violation.
#[test]
fn agreement_checker_goes_red_on_planted_outputs() {
    let shallow = |op: OwnedOp| case_at(&[false], Value::Arr(vec![Value::I64(1)]), op);
    let deepest = fragment_of(&nested_arrays(DEPTH_MAX, "0"));
    // (a) A returned document that nests 129.
    let planted = raw_document(&raw_array_around(&deepest));
    assert_eq!(
        judge(&shallow(OwnedOp::Toggle), Observed::Document(planted)),
        Err(Violation::OutputRefused(inf_doc::DocError::DepthExceeded))
    );
    // (b) An `ArrAppend` whose operand wrapper nests 129.
    let case = shallow(OwnedOp::ArrAppend(Vec::new(), raw_array_around(&deepest)));
    assert_eq!(
        judge(&case, Observed::Document(tape_of("[1,2]"))),
        Err(Violation::OperandRefused(DeltaDecodeError::BadFragment(
            inf_doc::DocError::DepthExceeded
        )))
    );
    // (c) A depth refusal whose model output nests exactly 128.
    let value = wrap(&[false; DEPTH_MAX - 1], Value::I64(0));
    let frag = fragment(&value);
    let case = shallow(OwnedOp::SetReplace(value, frag));
    assert_eq!(
        judge(&case, Observed::Refused(ApplyError::DepthExceeded)),
        Err(Violation::RefusedWithinBound { model_depth: DEPTH_MAX })
    );
}
