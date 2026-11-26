//! The resumable serializer against its two oracles: the serializer as it
//! stood before (`frozen`, which shares no code with the walk) and
//! `serde_json`. Every row holds a step to its budgets from the step's own
//! output, never from the emitter's counters: bytes from the slice it
//! wrote, values from the tokens in that slice, scans from the tally in the
//! scan loop.

use core::num::NonZeroU32;

use inf_alloc::arena::{Arena, ArenaConfig};
use proptest::collection::vec;
use proptest::prelude::*;

use crate::error::DocError;
use crate::header::{self, HEADER_LEN};
use crate::limits::{
    CHECK_OVERSHOOT_BYTES_MAX, DEPTH_MAX, DOC_BYTES_MAX, EMIT_OVERSHOOT_BYTES_MAX, ESCAPE_BYTES_MAX,
};
use crate::model::{self, Value};
use crate::ser::{
    CheckStep, EmitError, EmitStep, Measure, MeasureStep, OwnedEmit, Reply, SerializeOpts,
    SlicedStep, StepBudget, measure_sliced, serialize_into, serialize_into_bounded,
    serialize_reply_into, serialize_reply_into_bounded, serialize_sliced,
};
use crate::ser_frozen as frozen;
use crate::tape::{self, TAG_ARR, TAG_NULL, TAG_OBJ, TAG_STR24, TapeDoc};
use crate::{ArenaDoc, DocValue};

/// The budgets of the design's table: 1, an escape and its neighbours, an
/// atom's bound and its neighbours, a slice's work budget, and no bound.
const BUDGETS: [u32; 11] = [1, 2, 5, 6, 7, 31, 32, 33, 4_095, 4_096, u32::MAX];

const COMPACT: SerializeOpts<'static> = SerializeOpts { indent: b"", newline: b"", space: b"" };
/// Separators of several bytes, one multi-byte: a step edge falls inside each.
const FORMATTED: SerializeOpts<'static> =
    SerializeOpts { indent: "→ ".as_bytes(), newline: b"\r\n", space: b"  " };

fn nz(n: u32) -> NonZeroU32 {
    NonZeroU32::new(n).expect("a budget is at least 1")
}

/// A step's budgets: bytes, then values.
fn budget((bytes_max, nodes_max): (u32, u32)) -> StepBudget {
    StepBudget { bytes_max: nz(bytes_max), nodes_max: nz(nodes_max) }
}

fn root(idoc: &[u8]) -> DocValue<'_> {
    TapeDoc::from_bytes(idoc).expect("the fixture validates").root().into()
}

/// A plain document around a raw body.
fn raw_doc(body: &[u8]) -> Vec<u8> {
    let mut doc = vec![0u8; HEADER_LEN];
    header::patch(&mut doc, 0, u32::try_from(body.len()).expect("a body below the cap"));
    doc.extend_from_slice(body);
    doc
}

/// A container token around a raw body.
fn raw_container(tag: u8, body: &[u8]) -> Vec<u8> {
    let len = u32::try_from(body.len()).expect("a body below the cap").to_le_bytes();
    let mut token = vec![tag, len[0], len[1], len[2]];
    token.extend_from_slice(body);
    token
}

/// `depth` arrays around a null.
fn nested(depth: usize) -> Vec<u8> {
    let mut body = vec![TAG_NULL];
    for _ in 0..depth {
        body = raw_container(TAG_ARR, &body);
    }
    raw_doc(&body)
}

fn encode(value: &Value) -> Vec<u8> {
    model::encode(value).expect("the fixture encodes")
}

fn text(s: &str) -> Value {
    Value::Str(s.to_owned())
}

/// Small documents of every token kind: each root kind, both string
/// widths, escapes of every length, multi-byte text, empty containers and
/// strings, the number extremes, and the deepest nesting.
fn corpus() -> Vec<Vec<u8>> {
    let mixed = Value::Obj(vec![
        ("a".into(), Value::I64(1)),
        ("quote\"back\\slash".into(), text("line\nfeed\ttab\u{1}\u{1f}\u{7f}")),
        ("k".repeat(40), text(&"v".repeat(300))),
        ("k".repeat(300), Value::Arr(vec![])),
        ("日本語".into(), text("héllo → 🎉")),
        ("".into(), text("")),
        ("nested".into(), Value::Obj(vec![("empty".into(), Value::Obj(vec![]))])),
        (
            "numbers".into(),
            Value::Arr(vec![
                Value::I64(0),
                Value::I64(-1),
                Value::I64(9),
                Value::I64(10),
                Value::I64(127),
                Value::I64(128),
                Value::I64(i64::MIN),
                Value::I64(i64::MAX),
                Value::F64(1.5),
                Value::F64(-1.234_567_890_123_456_7e-308),
                Value::Bool(true),
                Value::Bool(false),
                Value::Null,
            ]),
        ),
    ]);
    let corpus = vec![
        encode(&Value::Null),
        encode(&Value::Bool(false)),
        encode(&Value::I64(i64::MIN)),
        encode(&Value::F64(-1.234_567_890_123_456_7e-308)),
        encode(&text("")),
        encode(&text("root \"string\"\u{0}")),
        encode(&Value::Arr(vec![])),
        encode(&Value::Obj(vec![])),
        encode(&Value::Obj(vec![("a".into(), Value::I64(1))])),
        encode(&Value::Arr(vec![mixed.clone(), mixed.clone(), Value::Arr(vec![mixed.clone()])])),
        encode(&mixed),
        nested(DEPTH_MAX),
    ];
    // The same documents with their repeated keys tabled.
    #[cfg(feature = "doc-intern-keys")]
    let corpus = {
        let interned: Vec<_> = corpus.iter().filter_map(|doc| crate::intern::intern(doc)).collect();
        assert!(!interned.is_empty(), "VACUOUS: no fixture interns");
        [corpus, interned].concat()
    };
    corpus
}

/// The serializer as it stood, whole.
fn whole(value: DocValue<'_>, opts: &SerializeOpts<'_>) -> Vec<u8> {
    let mut out = Vec::new();
    frozen::serialize_into(value, opts, &mut out);
    out
}

/// What every step holds, read from the step: at most its byte budget and
/// one unsplit token written, no scan past its window, and a yield that
/// wrote.
fn hold_step(step: &SlicedStep<'_>, bytes_max: u32) {
    let budget = usize::try_from(bytes_max).expect("usize holds a u32");
    let wrote = step.bytes.len();
    assert!(
        wrote <= budget.saturating_add(EMIT_OVERSHOOT_BYTES_MAX),
        "a step wrote past its byte budget: {wrote} B under {budget}"
    );
    assert!(
        step.examined <= budget.saturating_add(ESCAPE_BYTES_MAX),
        "a step examined past its window: {} B under {budget}",
        step.examined
    );
    assert!(step.end == EmitStep::Done || wrote > 0, "a yield wrote nothing");
}

/// One sliced run under one budget pair, every step held. `whole_len`
/// bounds the run: a walk that repeats itself fails here, and never loops.
fn sliced_with(
    value: DocValue<'_>,
    opts: &SerializeOpts<'_>,
    base_depth: usize,
    (bytes_max, nodes_max): (u32, u32),
    whole_len: usize,
    each: &mut dyn FnMut(&SlicedStep<'_>),
) -> Vec<u8> {
    let mut out = Vec::new();
    let mut total = 0;
    let walked = serialize_sliced(
        value,
        opts,
        base_depth,
        &mut out,
        &mut || budget((bytes_max, nodes_max)),
        &mut |step| {
            hold_step(step, bytes_max);
            total += step.bytes.len();
            assert!(total <= whole_len, "the sliced bytes run past the whole run's length");
            each(step);
        },
    );
    assert!(walked, "the fixture is a tape value");
    out
}

fn sliced(
    value: DocValue<'_>,
    opts: &SerializeOpts<'_>,
    budgets: (u32, u32),
    whole_len: usize,
) -> Vec<u8> {
    sliced_with(value, opts, 0, budgets, whole_len, &mut |_| {})
}

/// The counting sink's total for one value. `whole_len` bounds the run:
/// a measure that repeats itself ends past it, with a wrong length.
fn measured(
    value: DocValue<'_>,
    opts: &SerializeOpts<'_>,
    base: usize,
    budgets: (u32, u32),
    whole_len: u64,
) -> u64 {
    measure_sliced(value, opts, base, &mut || budget(budgets), whole_len)
        .expect("the fixture is a tape value")
        .expect("a length below u64")
}

fn len_u64(bytes: &[u8]) -> u64 {
    u64::try_from(bytes.len()).expect("u64 holds a length")
}

/// Budgets row: for every budget pair, the concatenated steps are the
/// whole run's bytes, and every step holds its budgets.
#[test]
fn sliced_steps_equal_the_whole_run_at_every_budget_pair() {
    for idoc in corpus() {
        let value = root(&idoc);
        for opts in [COMPACT, FORMATTED] {
            let want = whole(value, &opts);
            for bytes_max in BUDGETS {
                for nodes_max in BUDGETS {
                    let got = sliced(value, &opts, (bytes_max, nodes_max), want.len());
                    assert!(got == want, "the sliced bytes differ at ({bytes_max}, {nodes_max})");
                }
            }
        }
    }
}

fn arb_value() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(Value::I64),
        any::<f64>().prop_filter("finite", |f| f.is_finite()).prop_map(Value::F64),
        any::<String>().prop_map(Value::Str),
    ];
    leaf.prop_recursive(5, 64, 6, |inner| {
        prop_oneof![
            vec(inner.clone(), 0..6).prop_map(Value::Arr),
            vec((any::<String>(), inner), 0..6).prop_map(Value::Obj),
        ]
    })
}

proptest! {
    /// The property behind the budgets row: any document, under any
    /// schedule of budgets that changes from step to step, in either form.
    #[test]
    fn any_schedule_of_budgets_writes_the_whole_runs_bytes(
        value in arb_value(),
        schedule in vec((1u32..=48, 1u32..=6), 1..12),
        formatted in any::<bool>(),
    ) {
        let idoc = encode(&value);
        let value = root(&idoc);
        let opts = if formatted { FORMATTED } else { COMPACT };
        let want = whole(value, &opts);
        let mut turn = 0;
        let mut next = || {
            turn += 1;
            budget(schedule[turn % schedule.len()])
        };
        // `want.len()` bounds the run, as in `sliced_with`: every yield
        // writes, so a walk that repeats itself fails at the step that
        // passes the whole run's length, and never loops.
        let mut got = Vec::new();
        let mut total = 0;
        let walked = serialize_sliced(value, &opts, 0, &mut got, &mut next, &mut |step| {
            assert!(step.end == EmitStep::Done || !step.bytes.is_empty(), "a yield wrote nothing");
            total += step.bytes.len();
            assert!(total <= want.len(), "the sliced bytes run past the whole run's length");
        });
        prop_assert!(walked);
        prop_assert_eq!(&got, &want);
        let mut unsliced = Vec::new();
        serialize_into(value, &opts, &mut unsliced);
        prop_assert_eq!(&unsliced, &want);
    }
}

/// Bounded-mapping row: `Ok` and `Err` are the frozen walker's at every
/// limit around the total, `Ok` bytes are equal, and a refusal leaves at
/// most one unsplit token past the limit.
fn hold_bounded(value: DocValue<'_>, opts: &SerializeOpts<'_>, prefix: usize, limit: usize) {
    let mut got = vec![b'#'; prefix];
    let mut want = got.clone();
    let verdict = serialize_into_bounded(value, opts, &mut got, limit);
    let frozen_verdict = frozen::serialize_into_bounded(value, opts, &mut want, limit);
    assert!(verdict == frozen_verdict, "the bounded verdict differs at limit {limit}");
    if verdict.is_ok() {
        assert!(got == want, "the bounded bytes differ at limit {limit}");
    } else {
        let end = limit.saturating_add(EMIT_OVERSHOOT_BYTES_MAX).max(prefix);
        assert!(got.len() <= end, "a refusal left {} B, limit {limit}", got.len());
    }
}

#[test]
fn the_bounded_mapping_agrees_with_the_frozen_walker_at_every_limit() {
    for idoc in corpus() {
        let value = root(&idoc);
        for opts in [COMPACT, FORMATTED] {
            let total = whole(value, &opts).len();
            for prefix in [0, 3] {
                let end = prefix + total;
                // No budget at all, the buffer's own length, and the total's band.
                hold_bounded(value, &opts, prefix, 0);
                hold_bounded(value, &opts, prefix, prefix);
                for limit in end.saturating_sub(40)..=end + 40 {
                    hold_bounded(value, &opts, prefix, limit);
                }
            }
        }
    }
}

/// The reply budget's own falsifiers, at lowered budgets: a break of one
/// long `NEWLINE` per element, and one document under nine `$` paths.
#[test]
fn a_reply_over_a_lowered_budget_is_refused_as_the_frozen_walker_refuses_it() {
    let elements = Value::Arr((0..200).map(Value::I64).collect());
    let idoc = encode(&elements);
    let newline = vec![b'\n'; 64 << 10];
    let opts = SerializeOpts { indent: b" ", newline: &newline, space: b"" };
    let total = whole(root(&idoc), &opts).len();
    for limit in [1, 1 << 10, 64 << 10, (64 << 10) + 1, 1 << 20, total - 1, total, total + 1] {
        hold_bounded(root(&idoc), &opts, 0, limit);
    }
    let wide = encode(&Value::Arr(vec![text(&"w".repeat(100_000)); 16]));
    let paths: Vec<_> = (0..9).map(|i| format!("$.path{i}")).collect();
    let members = |value| paths.iter().map(|p| (p.as_bytes(), matches(value))).collect();
    let reply = Reply::Object(members(root(&wide)));
    let mut all = Vec::new();
    frozen::serialize_reply_into(&reply, &COMPACT, &mut all);
    for limit in [0, 1, 100_000, 1_600_000, all.len() - 1, all.len(), all.len() + 1] {
        let (mut got, mut want) = (Vec::new(), Vec::new());
        let verdict = serialize_reply_into_bounded(&reply, &COMPACT, &mut got, limit);
        let frozen_verdict =
            frozen::serialize_reply_into_bounded(&reply, &COMPACT, &mut want, limit);
        assert!(verdict == frozen_verdict, "the bounded verdict differs at limit {limit}");
        assert!(verdict.is_err() || got == want, "the bounded bytes differ at limit {limit}");
        let end = limit + EMIT_OVERSHOOT_BYTES_MAX;
        assert!(verdict.is_ok() || got.len() <= end, "a refusal left {} B", got.len());
    }
}

/// A `$` path's reply over one match.
fn matches(value: DocValue<'_>) -> Reply<'_> {
    Reply::Array(vec![Reply::Value(value)])
}

/// The three shapes `JSON.GET` builds: a legacy path (base 0), one `$`
/// path (base 1), and two paths (base 2).
fn shapes(value: DocValue<'_>) -> [Reply<'_>; 3] {
    [
        Reply::Value(value),
        Reply::Array(vec![Reply::Value(value), Reply::Value(value)]),
        Reply::Object(vec![
            (b"$.a".as_slice(), matches(value)),
            (b"$[\"quoted\"]".as_slice(), Reply::Array(vec![])),
            (b"$..b".as_slice(), Reply::Array(vec![Reply::Value(value), Reply::Value(value)])),
        ]),
    ]
}

fn hold_reply(value: DocValue<'_>) {
    for opts in [COMPACT, FORMATTED] {
        for reply in shapes(value) {
            let (mut got, mut want) = (Vec::new(), Vec::new());
            serialize_reply_into(&reply, &opts, &mut got);
            frozen::serialize_reply_into(&reply, &opts, &mut want);
            assert!(got == want, "the reply bytes differ");
            let mut bounded = Vec::new();
            serialize_reply_into_bounded(&reply, &opts, &mut bounded, want.len())
                .expect("a reply of exactly its budget");
            assert!(bounded == want, "the bounded reply bytes differ");
            let mut short = Vec::new();
            let refused = serialize_reply_into_bounded(&reply, &opts, &mut short, want.len() - 1);
            assert!(refused.is_err(), "a reply one byte over its budget was served");
        }
    }
}

/// `JSON.GET`'s path row: the reply walker's bytes are the frozen walker's
/// for every shape, in both forms, over tape, interned and tree values.
#[test]
fn the_reply_walk_agrees_with_the_frozen_walker() {
    for idoc in corpus() {
        hold_reply(root(&idoc));
        let mut arena = Arena::new(ArenaConfig::default());
        let doc = TapeDoc::from_bytes(&idoc).expect("the fixture validates");
        let tree = ArenaDoc::from_tape(&doc, &mut arena).expect("the fixture morphs");
        hold_reply(tree.root_value(&arena));
        tree.free(&mut arena);
    }
}

/// Strings row: a string of nothing but six-byte escapes, as a root, a key
/// and a value, with a step edge at every offset of an escape. No step
/// ends inside one.
#[test]
fn a_string_of_escapes_straddles_every_step_edge() {
    let escapes = "\u{0}".repeat(100);
    let as_root = encode(&text(&escapes));
    let nested = encode(&Value::Arr(vec![Value::Obj(vec![(escapes.clone(), text(&escapes))])]));
    for opts in [COMPACT, FORMATTED] {
        let want_root = whole(root(&as_root), &opts);
        let want_nested = whole(root(&nested), &opts);
        for bytes_max in 1..=40 {
            let mut body = 0;
            let mut whole_escapes = |step: &SlicedStep<'_>| {
                let quotes = step.bytes.iter().filter(|&&b| b == b'"').count();
                body += step.bytes.len() - quotes;
                assert!(body % ESCAPE_BYTES_MAX == 0, "a step ended inside an escape");
            };
            let budgets = (bytes_max, 1);
            let got =
                sliced_with(root(&as_root), &opts, 0, budgets, want_root.len(), &mut whole_escapes);
            assert!(got == want_root, "a resumed string's bytes differ at {bytes_max}");
            let got = sliced(root(&nested), &opts, (bytes_max, 2), want_nested.len());
            assert!(got == want_nested, "a resumed string's bytes differ at {bytes_max}");
        }
    }
}

/// Strings row: multi-byte text passes verbatim, so a step may end inside
/// a character and the concatenation is still the text.
#[test]
fn multi_byte_text_split_across_steps_stays_valid() {
    let idoc = encode(&Value::Obj(vec![("日本語🎉".into(), text("é→🎉 日本語"))]));
    let want = whole(root(&idoc), &COMPACT);
    for bytes_max in 1..=16 {
        let got = sliced(root(&idoc), &COMPACT, (bytes_max, 1), want.len());
        assert!(got == want, "multi-byte text differs at {bytes_max}");
        assert!(str::from_utf8(&got).is_ok(), "the concatenation is not UTF-8");
    }
}

/// A plain document through the check phase of an [`OwnedEmit`]. A check
/// step examines a token or a byte, so the run is bounded by the
/// document's length: a check that stands still fails here.
fn checked_under(idoc: &[u8], budgets: (u32, u32)) -> OwnedEmit<Measure> {
    let mut check = OwnedEmit::new(idoc.to_vec()).expect("a plain document");
    for _ in 0..=idoc.len() {
        match check.step(budget(budgets)) {
            CheckStep::Yield(next) => check = next,
            CheckStep::Checked(measure) => return measure,
            CheckStep::Refused(error) => panic!("the fixture was refused: {error}"),
        }
    }
    panic!("the check ran past its document's length");
}

/// One document through the three phases of an [`OwnedEmit`], every step
/// held: the bytes emitted, and the length measured. `whole_len` bounds
/// the measure and the emit, as it bounds a sliced run: a yield counts or
/// writes a byte, so a walk that repeats itself fails here.
fn owned(
    idoc: &[u8],
    opts: &SerializeOpts<'_>,
    budgets: (u32, u32),
    whole_len: usize,
) -> (Vec<u8>, u64) {
    let window = usize::try_from(budgets.0).expect("usize holds a u32");
    let mut measure = checked_under(idoc, budgets);
    let mut steps = 0;
    let mut emit = loop {
        steps += 1;
        assert!(steps <= whole_len + 1, "the measure runs past the whole run's length");
        match measure.step(opts, budget(budgets)) {
            MeasureStep::Yield(next) => {
                assert!(next.examined() <= window.saturating_add(ESCAPE_BYTES_MAX));
                measure = next;
            }
            MeasureStep::Measured(emit) => break emit,
            MeasureStep::Refused(_, error) => panic!("the measure was refused: {error}"),
        }
    };
    let mut out = Vec::new();
    loop {
        let before = out.len();
        let end = emit.step(opts, &mut out, budget(budgets)).expect("an emit step");
        let step = SlicedStep { bytes: &out[before..], examined: emit.examined(), end };
        hold_step(&step, budgets.0);
        assert!(out.len() <= whole_len, "the owned bytes run past the whole run's length");
        if end == EmitStep::Done {
            let ended = out.len();
            assert_eq!(emit.step(opts, &mut out, budget((1, 1))), Ok(EmitStep::Done));
            assert_eq!(out.len(), ended, "a step past the end wrote");
            return (out, emit.length());
        }
    }
}

/// Root-string row: a root string is stepped like any string, through the
/// core step, the bounded entry, the counting sink and an [`OwnedEmit`].
fn hold_root_string(idoc: &[u8], encoded_len: usize) {
    let value = root(idoc);
    let want = whole(value, &COMPACT);
    assert_eq!(want.len(), encoded_len);
    let slice = (64 << 10, 4_096);
    assert!(sliced(value, &COMPACT, slice, want.len()) == want, "the sliced root string differs");
    let length = len_u64(&want);
    assert_eq!(measured(value, &COMPACT, 0, slice, length), length, "the measure differs");
    let mut bounded = Vec::new();
    serialize_into_bounded(value, &COMPACT, &mut bounded, want.len()).expect("exactly its budget");
    assert!(bounded == want, "the bounded root string differs");
    bounded.clear();
    let limit = want.len() - 1;
    assert!(serialize_into_bounded(value, &COMPACT, &mut bounded, limit).is_err());
    assert!(bounded.len() <= limit + EMIT_OVERSHOOT_BYTES_MAX, "a refusal left a string");
    let (emitted, length) = owned(idoc, &COMPACT, slice, want.len());
    assert!(emitted == want, "the owned root string differs");
    assert_eq!(length, len_u64(&want), "the measure differs");
}

#[test]
fn a_root_string_is_stepped_at_every_entry() {
    // 819,200 control characters: 819,212 stored bytes, six times that encoded.
    let probe = encode(&text(&"\u{0}".repeat(819_200)));
    assert_eq!(probe.len(), 819_212);
    hold_root_string(&probe, 4_915_202);
    let want = whole(root(&probe), &COMPACT);
    for budgets in [(1, 1), (5, 1), (6, 1), (7, 1), (33, 1)] {
        let got = sliced(root(&probe[..]), &COMPACT, budgets, want.len());
        assert!(got == want, "the sliced root string differs at {budgets:?}");
    }
    // A root string at the document cap: a str24 token of every body byte left.
    let payload = DOC_BYTES_MAX - 4;
    let body = raw_container(TAG_STR24, &vec![0u8; payload]);
    hold_root_string(&raw_doc(&body), payload * ESCAPE_BYTES_MAX + 2);
}

/// Scan row: over a clean string a step reads no further than it may
/// write, so the scans of a string sum to its length, not to its square.
#[test]
fn a_step_scans_no_further_than_its_window() {
    let payload = DOC_BYTES_MAX - 4;
    let idoc = raw_doc(&raw_container(TAG_STR24, &vec![b'a'; payload]));
    let value = root(&idoc);
    let want = whole(value, &COMPACT);
    let mut examined = 0;
    let mut tally = |step: &SlicedStep<'_>| examined += step.examined;
    let got = sliced_with(value, &COMPACT, 0, (64 << 10, 4_096), want.len(), &mut tally);
    assert!(got == want, "the clean string's bytes differ");
    assert_eq!(examined, payload, "every byte of the string is scanned once");
    let length = len_u64(&want);
    assert_eq!(measured(value, &COMPACT, 0, (64 << 10, 4_096), length), length);
}

/// Shape row, by values: one value a tape byte, each writing one or a few
/// bytes. The step's count of them is read from its bytes.
fn hold_work(idoc: &[u8], token: u8, nodes_max: u32) {
    let value = root(idoc);
    let want = whole(value, &COMPACT);
    let most = usize::try_from(nodes_max).expect("usize holds a u32") + 1;
    let mut count = |step: &SlicedStep<'_>| {
        let visited = step.bytes.iter().filter(|&&b| b == token).count();
        assert!(visited <= most, "a step visited past its work budget: {visited} values");
    };
    for bytes_max in [u32::MAX, 64 << 10] {
        let budgets = (bytes_max, nodes_max);
        let got = sliced_with(value, &COMPACT, 0, budgets, want.len(), &mut count);
        assert!(got == want, "the bytes differ under a work budget of {nodes_max}");
    }
    let length = len_u64(&want);
    assert_eq!(measured(value, &COMPACT, 0, (u32::MAX, nodes_max), length), length);
}

#[test]
fn a_step_visits_at_most_its_work_budget_and_one_value() {
    // The cap in `null`s: every value one byte of tape and five of text.
    let nulls = raw_doc(&raw_container(TAG_ARR, &vec![TAG_NULL; DOC_BYTES_MAX - 4]));
    hold_work(&nulls, b'n', 4_096);
    // The cap in empty arrays: every value an opener.
    let empties = raw_container(TAG_ARR, &[]).repeat((DOC_BYTES_MAX - 4) / 4);
    hold_work(&raw_doc(&raw_container(TAG_ARR, &empties)), b'[', 4_096);
    // An object of 10⁶ empty objects under one-byte keys.
    let mut entry = vec![0x81, b'k'];
    entry.extend(raw_container(TAG_OBJ, &[]));
    hold_work(&raw_doc(&raw_container(TAG_OBJ, &entry.repeat(1_000_000))), b'{', 4_096);
    // A work budget of one value: a step a value.
    hold_work(&raw_doc(&raw_container(TAG_ARR, &vec![TAG_NULL; 100_000])), b'n', 1);
}

/// `depth` arrays around a null, formatted with one separator set, in
/// bytes: each level writes a break before its element and one before its
/// closer.
fn nested_formatted_len(depth: u64, base: u64, indent: u64, newline: u64) -> u64 {
    let brackets = 2 * depth + 4;
    let copies: u64 = (1..=depth).map(|level| (base + level) + (base + level - 1)).sum();
    brackets + 2 * depth * newline + copies * indent
}

/// Formatting row: a separator is resumable like a string. A break longer
/// than a step yields inside a copy; one longer than `u32::MAX` bytes
/// spans steps of any budget; the measure is the sum either way.
#[test]
fn a_separator_yields_inside_a_copy_and_the_measure_is_the_sum() {
    let deep = nested(DEPTH_MAX);
    let depth = u64::try_from(DEPTH_MAX).expect("u64 holds a depth");
    // One byte of indent at the deepest nesting, a step edge at every byte.
    let narrow = SerializeOpts { indent: b"\t", newline: b"\n", space: b" " };
    let want = whole(root(&deep), &narrow);
    assert_eq!(len_u64(&want), nested_formatted_len(depth, 0, 1, 1));
    for bytes_max in [1, 2, 3, 127, 128, 129] {
        let got = sliced(root(&deep), &narrow, (bytes_max, 1), want.len());
        assert!(got == want, "the break's bytes differ at {bytes_max}");
    }
    // 64 KiB of indent: a step of 4,096 bytes ends inside every copy.
    let block = vec![b' '; 64 << 10];
    let wide = SerializeOpts { indent: &block, newline: b"\n", space: b"" };
    let shallow = nested(8);
    let want = whole(root(&shallow), &wide);
    let mut steps = 0;
    let mut count = |_: &SlicedStep<'_>| steps += 1;
    let got = sliced_with(root(&shallow), &wide, 0, (4_096, 4_096), want.len(), &mut count);
    assert!(got == want, "the wide break's bytes differ");
    // Eight levels write 64 copies; more steps than copies ended inside one.
    assert!(steps > 64, "VACUOUS: {steps} steps over 64 copies");
    // The counting sink at the deepest nesting, where no buffer could hold the text.
    for (indent_len, base) in [(64 << 10, 0), (1 << 20, 0), (33 << 20, 1)] {
        let block = vec![b' '; indent_len];
        let opts = SerializeOpts { indent: &block, newline: b"\n", space: b"" };
        let indent = len_u64(&block);
        let sum = nested_formatted_len(depth, base, indent, 1);
        let base = usize::try_from(base).expect("usize holds a base");
        for budgets in [(u32::MAX, u32::MAX), (64 << 10, 4_096)] {
            let got = measured(root(&deep), &opts, base, budgets, sum);
            assert_eq!(got, sum, "the measure differs");
        }
    }
}

/// Measure row: the counting sink's total is the emitted length, for every
/// document, form and budget pair, with a wrapper's depth as its base.
#[test]
fn the_measure_equals_the_emitted_length() {
    for idoc in corpus() {
        let value = root(&idoc);
        for opts in [COMPACT, FORMATTED] {
            let want = len_u64(&whole(value, &opts));
            for budgets in [(1, 1), (7, 2), (33, 4_096), (u32::MAX, u32::MAX)] {
                let got = measured(value, &opts, 0, budgets, want);
                assert_eq!(got, want, "the measure differs");
            }
            // A value under two wrapper levels: its breaks carry the base.
            let reply = Reply::Array(vec![Reply::Array(vec![Reply::Value(value)])]);
            let mut wrapped = Vec::new();
            frozen::serialize_reply_into(&reply, &opts, &mut wrapped);
            // The wrappers' own text: four brackets, four breaks, at depths 1, 2, 1 and 0.
            let outer = 4 + 4 * opts.newline.len() + 4 * opts.indent.len();
            let inner = len_u64(&wrapped[outer..]);
            let got = measured(value, &opts, 2, (33, 4_096), inner);
            assert_eq!(got, inner, "the measure differs");
        }
    }
}

/// `OwnedEmit` row: only a plain document's bytes are taken.
#[test]
fn owned_emit_takes_a_plain_document_and_nothing_else() {
    let plain = encode(&Value::Obj(vec![("a".into(), Value::I64(1))]));
    assert!(OwnedEmit::new(plain.clone()).is_ok());
    assert_eq!(OwnedEmit::new(Vec::new()).err(), Some(DocError::Truncated));
    assert_eq!(OwnedEmit::new(plain[..HEADER_LEN - 1].to_vec()).err(), Some(DocError::Truncated));
    let mut magic = plain.clone();
    magic[0] ^= 0xFF;
    assert_eq!(OwnedEmit::new(magic).err(), Some(DocError::BadMagic));
    let mut version = plain.clone();
    version[2] = 9;
    assert_eq!(OwnedEmit::new(version).err(), Some(DocError::UnsupportedVersion(9)));
    let mut reserved = plain.clone();
    reserved[3] = 0b10;
    assert_eq!(OwnedEmit::new(reserved).err(), Some(DocError::UnsupportedFlags(0b10)));
    let mut long = plain.clone();
    long.push(TAG_NULL);
    assert_eq!(OwnedEmit::new(long).err(), Some(DocError::BadLength));
    // The interned form: refused here as the canonical check refuses it.
    let mut interned = plain;
    interned[3] = 1;
    let refusal = OwnedEmit::new(interned.clone()).err();
    assert!(tape::CanonicalDoc::validate(&interned).is_err());
    #[cfg(feature = "doc-intern-keys")]
    assert_eq!(refusal, Some(DocError::NonCanonical("interned form")));
    #[cfg(not(feature = "doc-intern-keys"))]
    assert_eq!(refusal, Some(DocError::UnsupportedFlags(1)));
}

/// The stepped check's verdict on `idoc`, every step held to its budgets
/// from the walk's own tally.
fn stepped_verdict(idoc: &[u8], (bytes_max, nodes_max): (u32, u32)) -> Result<(), DocError> {
    let bytes = usize::try_from(bytes_max).expect("usize holds a u32");
    let tokens = usize::try_from(nodes_max).expect("usize holds a u32");
    let hold = |tally: tape::CheckTally, ended: bool| {
        let window = bytes.saturating_add(CHECK_OVERSHOOT_BYTES_MAX);
        let over = tally.tokens > tokens || tally.string_bytes > window;
        assert!(!over, "a check step examined past its budgets: {tally:?}");
        assert!(ended || tally.tokens + tally.string_bytes > 0, "a check step stood still");
    };
    let mut check = OwnedEmit::new(idoc.to_vec())?;
    // A step examines a token or a byte: the document's length bounds the run.
    for _ in 0..=idoc.len() {
        match check.step(budget((bytes_max, nodes_max))) {
            CheckStep::Yield(next) => {
                hold(next.checked(), false);
                check = next;
            }
            CheckStep::Checked(measure) => {
                hold(measure.checked(), true);
                return Ok(());
            }
            CheckStep::Refused(error) => return Err(error),
        }
    }
    panic!("the check ran past its document's length");
}

/// The verdict of the check as it stood, on a plain document's bytes.
fn frozen_verdict(idoc: &[u8]) -> Result<(), DocError> {
    header::decode(idoc)?;
    tape::frozen::validate_body(&idoc[HEADER_LEN..], &[])
}

/// A small document damaged every way one byte can damage it: cut at
/// every offset and flipped at every offset, beside a 129th container,
/// bytes after the root, no root, and an object closed inside a pair. Some
/// flips leave a document the check still passes.
fn damaged_corpus() -> Vec<Vec<u8>> {
    let good = encode(&Value::Arr(vec![
        Value::Obj(vec![("key".into(), text(&"é".repeat(20)))]),
        Value::F64(2.5),
        Value::I64(1 << 40),
    ]));
    let body = &good[HEADER_LEN..];
    let mut damaged = vec![nested(DEPTH_MAX + 1), raw_doc(&[]), raw_doc(&[TAG_NULL, TAG_NULL])];
    for cut in 1..body.len() {
        damaged.push(raw_doc(&body[..cut]));
    }
    for at in 0..body.len() {
        let mut flipped = body.to_vec();
        flipped[at] ^= 0x5A;
        damaged.push(raw_doc(&flipped));
    }
    damaged.push(raw_doc(&raw_container(TAG_OBJ, &[0x81, b'k'])));
    damaged
}

/// `OwnedEmit` row: the stepped check answers what the check as it stood
/// answers, on documents it passes and on documents it refuses, at every
/// budget pair, and no step examines past its budgets.
#[test]
fn a_check_step_examines_at_most_its_budgets() {
    let plain = corpus().into_iter().filter(|doc| doc[3] == 0);
    let mut refusals = 0;
    for idoc in plain.chain(damaged_corpus()) {
        let want = frozen_verdict(&idoc);
        refusals += usize::from(want.is_err());
        for bytes_max in BUDGETS {
            for nodes_max in BUDGETS {
                let got = stepped_verdict(&idoc, (bytes_max, nodes_max));
                assert!(got == want, "the check's verdict differs at ({bytes_max}, {nodes_max})");
            }
        }
    }
    assert!(refusals > 60, "VACUOUS: {refusals} refused fixtures");
    // The cap in tokens, and the cap in one clean string, at a slice's budgets.
    let nulls = raw_doc(&raw_container(TAG_ARR, &vec![TAG_NULL; DOC_BYTES_MAX - 4]));
    assert_eq!(stepped_verdict(&nulls, (64 << 10, 4_096)), Ok(()));
    let clean = raw_doc(&raw_container(TAG_STR24, &vec![b'a'; DOC_BYTES_MAX - 4]));
    assert_eq!(stepped_verdict(&clean, (64 << 10, 4_096)), Ok(()));
}

/// `OwnedEmit` row: a window of the string check may end inside a
/// character. The check resumes at the last boundary the window proved, so
/// a string of four-byte characters passes at windows of four and five
/// bytes, and a wrong sequence is refused wherever a window edge falls.
#[test]
fn a_check_window_that_ends_inside_a_character_resumes_at_its_boundary() {
    let payload = (DOC_BYTES_MAX - 4) / 4 * 4;
    let wide = raw_doc(&raw_container(TAG_STR24, &"🎉".repeat(payload / 4).into_bytes()));
    assert_eq!(frozen_verdict(&wide), Ok(()));
    for bytes_max in 1..=5 {
        let got = stepped_verdict(&wide, (bytes_max, 1));
        assert!(got == Ok(()), "the check's verdict on four-byte characters differs: {got:?}");
    }
    // A bad continuation byte, an overlong form, a surrogate, a cut character.
    let wrong: [&[u8]; 4] = [b"\xE2\x28\xA1", b"\xC0\xAF", b"\xED\xA0\x80", b"\xF0\x9F\x8E"];
    for sequence in wrong {
        for lead in 0..=9 {
            for tail in [0, 5] {
                let mut payload = "é".repeat(150).into_bytes();
                payload.extend(vec![b'a'; lead]);
                payload.extend_from_slice(sequence);
                payload.extend(vec![b'z'; tail]);
                let idoc = raw_doc(&raw_container(TAG_STR24, &payload));
                assert_eq!(frozen_verdict(&idoc), Err(DocError::BadUtf8));
                for bytes_max in 1..=12 {
                    let got = stepped_verdict(&idoc, (bytes_max, 1));
                    assert!(got == Err(DocError::BadUtf8), "the check's verdict differs: {got:?}");
                }
            }
        }
    }
}

/// `OwnedEmit` row: check, measure, emit. The emitted bytes are the frozen
/// walker's and the measured length is theirs, at every budget pair.
#[test]
fn owned_emit_measures_then_emits_the_frozen_bytes() {
    for idoc in corpus().into_iter().filter(|doc| doc[3] == 0) {
        for opts in [COMPACT, FORMATTED] {
            let want = whole(root(&idoc), &opts);
            for bytes_max in BUDGETS {
                for nodes_max in [1, 2, 4_096, u32::MAX] {
                    let budgets = (bytes_max, nodes_max);
                    let (emitted, length) = owned(&idoc, &opts, budgets, want.len());
                    assert!(
                        emitted == want,
                        "the owned bytes differ at ({bytes_max}, {nodes_max})"
                    );
                    assert_eq!(length, len_u64(&want), "the measure differs");
                }
            }
        }
    }
}

fn checked(idoc: &[u8]) -> OwnedEmit<Measure> {
    checked_under(idoc, (u32::MAX, u32::MAX))
}

/// `OwnedEmit` row: the three option lengths are fixed by the first
/// measure step. A step with others is refused, writes nothing, and leaves
/// the walk where it was. The first change is to an option no position is
/// inside, so the refusal is what stops it.
#[test]
fn a_step_with_changed_option_lengths_is_refused() {
    let idoc = encode(&Value::Arr(vec![Value::Arr(vec![Value::I64(1), Value::I64(2)])]));
    let want = whole(root(&idoc), &FORMATTED);
    let changed = [
        SerializeOpts { space: b"", ..FORMATTED },
        SerializeOpts { indent: b"\t", ..FORMATTED },
        SerializeOpts { newline: b"\n", ..FORMATTED },
    ];
    let MeasureStep::Yield(mut measure) = checked(&idoc).step(&FORMATTED, budget((5, 1))) else {
        panic!("VACUOUS: the measure ended in its first step");
    };
    for opts in &changed {
        measure = match measure.step(opts, budget((5, 1))) {
            MeasureStep::Refused(measure, error) => {
                assert_eq!(error, EmitError::OptionsChanged, "changed options were measured");
                measure
            }
            MeasureStep::Yield(_) | MeasureStep::Measured(_) => {
                panic!("changed options were measured")
            }
        };
    }
    // A yield counts a byte, so the whole run's length bounds the steps left.
    let mut steps = 0;
    let mut emit = loop {
        steps += 1;
        assert!(steps <= want.len(), "the measure runs past the whole run's length");
        match measure.step(&FORMATTED, budget((5, 1))) {
            MeasureStep::Yield(next) => measure = next,
            MeasureStep::Measured(emit) => break emit,
            MeasureStep::Refused(_, error) => panic!("the measure was refused: {error}"),
        }
    };
    assert_eq!(emit.length(), len_u64(&want));
    let mut out = Vec::new();
    assert_eq!(emit.step(&FORMATTED, &mut out, budget((5, 1))), Ok(EmitStep::Yield));
    let before = out.clone();
    for opts in &changed {
        let refused = emit.step(opts, &mut out, budget((5, 1)));
        assert_eq!(refused, Err(EmitError::OptionsChanged), "changed options were emitted");
        assert!(out == before, "a refused step wrote");
    }
    while emit.step(&FORMATTED, &mut out, budget((5, 1))) == Ok(EmitStep::Yield) {
        assert!(out.len() <= want.len(), "the walk moved under a refused step");
    }
    assert!(out == want, "the walk moved under a refused step");
}

/// A measure whose count would pass `u64::MAX` is refused, and stays so.
#[test]
fn a_measure_past_u64_is_refused() {
    let idoc = encode(&text("four"));
    let measure = checked(&idoc).counted_from(u64::MAX - 5);
    let MeasureStep::Refused(measure, error) = measure.step(&COMPACT, budget((64, 64))) else {
        panic!("a count past u64 was measured");
    };
    assert_eq!(error, EmitError::LengthOverflow);
    let MeasureStep::Refused(_, error) = measure.step(&COMPACT, budget((64, 64))) else {
        panic!("a refused measure went on");
    };
    assert_eq!(error, EmitError::LengthOverflow);
    // Six bytes exactly: the count stops at the last representable length.
    let measure = checked(&idoc).counted_from(u64::MAX - 6);
    assert!(matches!(measure.step(&COMPACT, budget((64, 64))), MeasureStep::Measured(_)));
}

/// An emit that passes its measured length, or ends short of it, says so:
/// the length is already on the wire.
#[test]
fn an_emit_off_its_measured_length_is_a_mismatch() {
    let idoc = encode(&text("four"));
    let measured_at =
        |counted| match checked(&idoc).counted_from(counted).step(&COMPACT, budget((64, 64))) {
            MeasureStep::Measured(emit) => emit,
            MeasureStep::Yield(_) | MeasureStep::Refused(..) => panic!("the fixture measures"),
        };
    let mut out = Vec::new();
    // Measured 6 B over: the walk ends short of the length.
    let mut long = measured_at(6);
    assert_eq!(long.length(), 12);
    assert_eq!(long.step(&COMPACT, &mut out, budget((64, 64))), Err(EmitError::LengthMismatch));
    // An exact measure, for the control.
    let mut exact = measured_at(0);
    out.clear();
    assert_eq!(exact.step(&COMPACT, &mut out, budget((64, 64))), Ok(EmitStep::Done));
    assert_eq!(out, b"\"four\"");
}

/// The differential against `serde_json`: compact text is its `to_vec`,
/// and two-space pretty text its `to_vec_pretty`, on documents with no
/// repeated key.
#[test]
fn sliced_text_is_serde_jsons() {
    let pretty = SerializeOpts { indent: b"  ", newline: b"\n", space: b" " };
    // serde_json's parser stops at 128 levels: the deepest fixture stays out.
    let deepest = nested(DEPTH_MAX);
    for idoc in corpus().into_iter().filter(|doc| *doc != deepest) {
        let value = root(&idoc);
        let Ok(text) = String::from_utf8(whole(value, &COMPACT)) else {
            panic!("the frozen walker wrote text");
        };
        let parsed: serde_json::Value = serde_json::from_str(&text).expect("the text is JSON");
        let got = sliced(value, &COMPACT, (7, 2), text.len());
        assert_eq!(got, serde_json::to_vec(&parsed).expect("serde_json prints"));
        let want = serde_json::to_vec_pretty(&parsed).expect("serde_json prints");
        assert_eq!(sliced(value, &pretty, (7, 2), want.len()), want);
    }
}

/// Tabled-key row: a key of 65,535 control characters, tabled, lies in the
/// dictionary, not in the region. It is stepped like any string.
#[cfg(feature = "doc-intern-keys")]
#[test]
fn a_tabled_key_straddles_every_step_edge() {
    let key = "\u{0}".repeat(usize::from(u16::MAX));
    let entry = |n| Value::Obj(vec![(key.clone(), Value::I64(n))]);
    let plain = encode(&Value::Arr(vec![entry(1), entry(2)]));
    let interned = crate::intern::intern(&plain).expect("a key used twice is tabled");
    assert_eq!(
        OwnedEmit::new(interned.clone()).err(),
        Some(DocError::NonCanonical("interned form"))
    );
    let value = root(&interned);
    for opts in [COMPACT, FORMATTED] {
        let want = whole(value, &opts);
        assert!(want == whole(root(&plain), &opts), "the frozen walker reads the table apart");
        for bytes_max in 1..=13 {
            let got = sliced(value, &opts, (bytes_max, 1), want.len());
            assert!(got == want, "the tabled key's bytes differ at {bytes_max}");
        }
        for bytes_max in BUDGETS {
            for nodes_max in BUDGETS {
                let got = sliced(value, &opts, (bytes_max, nodes_max), want.len());
                assert!(got == want, "the tabled key's bytes differ at ({bytes_max}, {nodes_max})");
            }
        }
        let length = len_u64(&want);
        assert_eq!(measured(value, &opts, 0, (64 << 10, 4_096), length), length);
        for limit in want.len() - 40..=want.len() + 40 {
            hold_bounded(value, &opts, 0, limit);
        }
        hold_bounded(value, &opts, 0, want.len() / 2);
    }
}
