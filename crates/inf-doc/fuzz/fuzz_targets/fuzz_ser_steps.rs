//! Step-boundary fuzz for the resumable JSON serializer (L9): any
//! document, under any schedule of budgets, in either form.
//!
//! Input: nine bytes of schedule (three steps' budgets, used in turn),
//! three option lengths with the options' bytes, then a document's bytes.
//!
//! Every input enters the stepped canonical check. Its verdict is the
//! one-shot check's, and no step examines past its budgets. A document it
//! passes is measured and emitted in steps: the bytes are the one-shot
//! serializer's and the length is the measured one.
//!
//! Bytes `TapeDoc::from_bytes` accepts, interned tapes among them, are then
//! written by the core step. The references share no stepping with it:
//!
//! - the sliced run's bytes are the one-shot run's, and the counting
//!   sink's total is their length;
//! - compact text is `serde_json`'s for the tape's model value, on a tape
//!   with no repeated key.
//!
//! Every step is held to its budgets from its own output: at most the byte
//! budget and one unsplit token written, no scan past its window, a yield
//! that wrote, and — in compact text, where a value's first byte can be
//! told from a separator's — at most the work budget and one value begun.
//! Every loop is bounded by the whole run's length: a walk that repeats
//! itself or stands still fails an assertion and never runs on.

#![no_main]

use std::num::NonZeroU32;

use libfuzzer_sys::fuzz_target;

use inf_doc::limits::{CHECK_OVERSHOOT_BYTES_MAX, EMIT_OVERSHOOT_BYTES_MAX, ESCAPE_BYTES_MAX};
use inf_doc::model::{self, Value};
use inf_doc::ser::{measure_sliced, serialize_sliced};
use inf_doc::{
    CanonicalDoc, CheckStep, DocError, DocValue, EmitStep, FLAG_INTERNED, Measure, MeasureStep,
    OwnedEmit, SerializeOpts, StepBudget, TapeDoc, serialize_into,
};

/// Three steps' budgets, used in turn.
struct Schedule {
    steps: [StepBudget; 3],
    turn: usize,
}

impl Schedule {
    /// A budget from its input bytes: at least 1, and the widest value is
    /// no bound at all.
    fn budget(raw: u32, widest: u32) -> NonZeroU32 {
        let value = if raw == widest { u32::MAX } else { raw.max(1) };
        NonZeroU32::new(value).expect("a budget is at least 1")
    }

    fn new(head: &[u8; 9]) -> Schedule {
        let step = |at: usize| StepBudget {
            bytes_max: Schedule::budget(
                u32::from(u16::from_le_bytes([head[at], head[at + 1]])),
                u32::from(u16::MAX),
            ),
            nodes_max: Schedule::budget(u32::from(head[at + 2]), 0xFF),
        };
        Schedule { steps: [step(0), step(3), step(6)], turn: 0 }
    }

    /// The next step's budgets.
    fn next(&mut self) -> StepBudget {
        self.turn += 1;
        self.steps[self.turn % self.steps.len()]
    }

    /// The schedule again, from its first step.
    fn restarted(&self) -> Schedule {
        Schedule { steps: self.steps, turn: 0 }
    }
}

fn wide(budget: NonZeroU32) -> usize {
    budget.get() as usize
}

/// What a step holds, read from the bytes it wrote.
fn hold_step(wrote: usize, examined: usize, end: EmitStep, budget: StepBudget) {
    let bytes_max = wide(budget.bytes_max);
    assert!(wrote <= bytes_max.saturating_add(EMIT_OVERSHOOT_BYTES_MAX), "a step wrote {wrote} B");
    assert!(examined <= bytes_max.saturating_add(ESCAPE_BYTES_MAX), "a step examined {examined} B");
    assert!(end == EmitStep::Done || wrote > 0, "a yield wrote nothing");
}

/// The values compact JSON text begins, counted a step's bytes at a time.
/// Its state carries from step to step: a string may straddle one. A
/// value's first byte is an opener, a string's opening quote outside a key
/// position, or the first byte of a number, `null`, `true` or `false`.
#[derive(Default)]
struct ValueStarts {
    in_string: bool,
    escaped: bool,
    in_atom: bool,
    expect_key: bool,
    objects: Vec<bool>,
}

impl ValueStarts {
    /// The values whose first byte lies in `bytes`.
    fn count(&mut self, bytes: &[u8]) -> usize {
        let mut values = 0;
        for &byte in bytes {
            if self.in_string {
                match byte {
                    _ if self.escaped => self.escaped = false,
                    b'\\' => self.escaped = true,
                    b'"' => self.in_string = false,
                    _ => {}
                }
                continue;
            }
            let was_atom = core::mem::take(&mut self.in_atom);
            match byte {
                b'"' => {
                    self.in_string = true;
                    values += usize::from(!core::mem::take(&mut self.expect_key));
                }
                b'{' | b'[' => {
                    values += 1;
                    self.objects.push(byte == b'{');
                    self.expect_key = byte == b'{';
                }
                b'}' | b']' => {
                    self.objects.pop();
                    self.expect_key = false;
                }
                b',' => self.expect_key = self.objects.last() == Some(&true),
                b':' => {}
                _ => {
                    values += usize::from(!was_atom);
                    self.in_atom = true;
                }
            }
        }
        values
    }
}

/// The stepped check's verdict, every step held to its budgets. A check
/// step examines a token or a byte, so the document's length bounds it.
fn stepped_check(idoc: &[u8], schedule: &mut Schedule) -> Result<OwnedEmit<Measure>, DocError> {
    let mut check = OwnedEmit::new(idoc.to_vec())?;
    for _ in 0..=idoc.len() {
        let budget = schedule.next();
        let window = wide(budget.bytes_max).saturating_add(CHECK_OVERSHOOT_BYTES_MAX);
        let hold = |tokens: usize, string_bytes: usize, ended: bool| {
            assert!(tokens <= wide(budget.nodes_max), "a check step examined {tokens} tokens");
            assert!(string_bytes <= window, "a check step examined {string_bytes} B");
            assert!(ended || tokens + string_bytes > 0, "a check step stood still");
        };
        match check.step(budget) {
            CheckStep::Yield(next) => {
                hold(next.checked().tokens, next.checked().string_bytes, false);
                check = next;
            }
            CheckStep::Checked(measure) => {
                hold(measure.checked().tokens, measure.checked().string_bytes, true);
                return Ok(measure);
            }
            CheckStep::Refused(error) => return Err(error),
        }
    }
    panic!("the check ran past its document's length");
}

/// A checked document measured and emitted in steps: its bytes and its
/// measured length. A yield counts or writes a byte, so `whole_len`, the
/// one-shot run's length, bounds both loops.
fn stepped_emit(
    mut measure: OwnedEmit<Measure>,
    opts: &SerializeOpts<'_>,
    schedule: &mut Schedule,
    whole_len: usize,
) -> (Vec<u8>, u64) {
    let mut steps = 0;
    let mut emit = loop {
        steps += 1;
        assert!(steps <= whole_len + 1, "the measure ran past the whole run's length");
        match measure.step(opts, schedule.next()) {
            MeasureStep::Yield(rest) => measure = rest,
            MeasureStep::Measured(emit) => break emit,
            MeasureStep::Refused(_, error) => panic!("the measure was refused: {error}"),
        }
    };
    let mut out = Vec::new();
    loop {
        let budget = schedule.next();
        let before = out.len();
        let end = emit.step(opts, &mut out, budget).expect("an emit step");
        hold_step(out.len() - before, emit.examined(), end, budget);
        assert!(out.len() <= whole_len, "the owned emit wrote past the whole run's length");
        if end == EmitStep::Done {
            return (out, emit.length());
        }
    }
}

/// One sliced run of the core step, every step held; in compact text the
/// values each step begins are held to its work budget too.
fn sliced_run(
    value: DocValue<'_>,
    opts: &SerializeOpts<'_>,
    schedule: &mut Schedule,
    whole_len: usize,
    count_values: bool,
) -> Vec<u8> {
    let mut starts = ValueStarts::default();
    let mut held = schedule.restarted();
    let mut total = 0;
    let mut out = Vec::new();
    let walked = serialize_sliced(value, opts, 0, &mut out, &mut || schedule.next(), &mut |step| {
        let budget = held.next();
        hold_step(step.bytes.len(), step.examined, step.end, budget);
        total += step.bytes.len();
        assert!(total <= whole_len, "the sliced run wrote past the whole run's length");
        if count_values {
            let begun = starts.count(step.bytes);
            assert!(
                begun <= wide(budget.nodes_max).saturating_add(1),
                "a step began {begun} values"
            );
        }
    });
    assert!(walked, "a tape value is walked");
    out
}

/// The model value as `serde_json`'s, or `None` when an object repeats a
/// key: `serde_json`'s map holds one value a key.
fn serde_of(value: &Value) -> Option<serde_json::Value> {
    Some(match value {
        Value::Null => serde_json::Value::Null,
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::I64(i) => serde_json::Value::Number((*i).into()),
        Value::F64(f) => serde_json::Value::Number(serde_json::Number::from_f64(*f)?),
        Value::Str(s) => serde_json::Value::String(s.clone()),
        Value::Arr(items) => {
            serde_json::Value::Array(items.iter().map(serde_of).collect::<Option<_>>()?)
        }
        Value::Obj(entries) => {
            let mut map = serde_json::Map::new();
            for (key, value) in entries {
                if map.insert(key.clone(), serde_of(value)?).is_some() {
                    return None;
                }
            }
            serde_json::Value::Object(map)
        }
    })
}

fuzz_target!(|data: &[u8]| {
    let Some((head, rest)) = data.split_first_chunk::<9>() else {
        return;
    };
    let mut schedule = Schedule::new(head);
    let Some((lens, rest)) = rest.split_first_chunk::<3>() else {
        return;
    };
    let [indent, newline, space] = lens.map(|len| usize::from(len % 4));
    let Some((indent, rest)) = rest.split_at_checked(indent) else {
        return;
    };
    let Some((newline, rest)) = rest.split_at_checked(newline) else {
        return;
    };
    let Some((space, idoc)) = rest.split_at_checked(space) else {
        return;
    };
    let opts = SerializeOpts { indent, newline, space };

    // The stepped check against the one-shot check. The stepped entry
    // refuses the interned form from the header alone, so there the two
    // agree on the refusal and not on its reason.
    let stepped = stepped_check(idoc, &mut schedule);
    let one_shot = CanonicalDoc::validate(idoc).map(|_| ());
    if idoc.len() > 3 && idoc[3] & FLAG_INTERNED != 0 {
        assert!(stepped.is_err() && one_shot.is_err(), "an interned document was taken");
    } else {
        let verdict = stepped.as_ref().map(|_| ()).map_err(|&error| error);
        assert_eq!(verdict, one_shot, "the stepped check's verdict");
    }

    let Ok(doc) = TapeDoc::from_bytes(idoc) else {
        return;
    };
    let value = DocValue::from(doc.root());
    let mut whole = Vec::new();
    serialize_into(value, &opts, &mut whole);
    if let Ok(measure) = stepped {
        let (emitted, length) = stepped_emit(measure, &opts, &mut schedule, whole.len());
        assert_eq!(emitted, whole, "the owned emit's bytes");
        assert_eq!(length, whole.len() as u64, "the measured length");
    }

    let mut budgets = schedule.restarted();
    let sliced = sliced_run(value, &opts, &mut budgets, whole.len(), false);
    assert_eq!(sliced, whole, "the sliced run's bytes");
    let mut budgets = schedule.restarted();
    let length = whole.len() as u64;
    let measured = measure_sliced(value, &opts, 0, &mut || budgets.next(), length);
    assert_eq!(measured, Some(Ok(length)), "the counting sink's total");

    // Compact text: the work budget held from the values each step begins,
    // and the differential against `serde_json`.
    let plain = SerializeOpts::default();
    let mut compact = Vec::new();
    serialize_into(value, &plain, &mut compact);
    let mut budgets = schedule.restarted();
    let sliced = sliced_run(value, &plain, &mut budgets, compact.len(), true);
    assert_eq!(sliced, compact, "the compact sliced run's bytes");
    if let Some(serde_value) = serde_of(&model::from_tape(&doc)) {
        let printed = serde_json::to_vec(&serde_value).expect("serde_json prints a value");
        assert_eq!(compact, printed, "compact text against serde_json");
    }
});
