//! Reference document tree (`Value`) + conversions to/from the tape and
//! the unified cursors. **Off the hot path by design**: this model exists
//! for property tests, differential oracles (S05's serde_json diff, S16's
//! mutation model), goldens, and the fuzz canonical-stability check — the
//! data plane traverses cursors and never materializes a tree.
//!
//! `Obj` is an ordered `Vec<(String, Value)>`: insertion order is part of
//! the durable contract (ADR-0036 D5), and duplicate keys are representable
//! here on purpose — the model must be able to describe every tape the
//! decoder accepts, canonical producers or not.

use crate::build::TapeBuilder;
use crate::cursor::{DocValue, ObjCursor};
use crate::error::DocError;
use crate::tape::TapeDoc;

#[derive(Clone, PartialEq, Debug)]
pub enum Value {
    Null,
    Bool(bool),
    I64(i64),
    F64(f64),
    Str(String),
    Arr(Vec<Value>),
    Obj(Vec<(String, Value)>),
}

/// Encode a full document (header + canonical body).
pub fn encode(v: &Value) -> Result<Vec<u8>, DocError> {
    let mut b = TapeBuilder::new();
    emit(v, &mut b)?;
    b.finish()
}

/// Encode a bare canonical fragment (ADR-0036 D8 — `DocDelta` operands).
pub fn encode_fragment(v: &Value) -> Result<Vec<u8>, DocError> {
    let mut b = TapeBuilder::new();
    emit(v, &mut b)?;
    b.finish_fragment()
}

/// One in-flight container during the iterative emit walk.
enum EmitFrame<'v> {
    Arr { items: &'v [Value], idx: usize },
    Obj { entries: &'v [(String, Value)], idx: usize },
}

/// Iterative emit — the builder's depth cap is the only recursion bound we
/// rely on anywhere (no call-stack recursion, decoder rule discipline even
/// off the hot path).
fn emit(root: &Value, b: &mut TapeBuilder) -> Result<(), DocError> {
    let mut stack: Vec<EmitFrame<'_>> = Vec::new();
    emit_value(root, b, &mut stack)?;
    while let Some(top) = stack.last_mut() {
        match top {
            EmitFrame::Arr { items, idx } => {
                if *idx == items.len() {
                    b.end();
                    stack.pop();
                    continue;
                }
                let item = &items[*idx];
                *idx += 1;
                emit_value(item, b, &mut stack)?;
            }
            EmitFrame::Obj { entries, idx } => {
                if *idx == entries.len() {
                    b.end();
                    stack.pop();
                    continue;
                }
                let (key, val) = &entries[*idx];
                *idx += 1;
                b.key(key)?;
                emit_value(val, b, &mut stack)?;
            }
        }
    }
    Ok(())
}

/// Emit one value; containers open a builder scope and push a frame.
fn emit_value<'v>(
    v: &'v Value,
    b: &mut TapeBuilder,
    stack: &mut Vec<EmitFrame<'v>>,
) -> Result<(), DocError> {
    match v {
        Value::Null => b.null(),
        Value::Bool(x) => b.bool(*x),
        Value::I64(x) => b.i64(*x),
        Value::F64(x) => b.f64(*x),
        Value::Str(s) => b.str_value(s),
        Value::Arr(items) => {
            b.begin_arr()?;
            stack.push(EmitFrame::Arr { items, idx: 0 });
            Ok(())
        }
        Value::Obj(entries) => {
            b.begin_obj()?;
            stack.push(EmitFrame::Obj { entries, idx: 0 });
            Ok(())
        }
    }
}

/// Rebuild a `Value` from a validated tape.
pub fn from_tape(doc: &TapeDoc<'_>) -> Value {
    from_cursor(DocValue::from(doc.root()))
}

/// One in-flight container during the iterative decode walk. Children
/// accumulate here and fold into the parent when the cursor drains.
enum BuildFrame<'a> {
    Arr {
        done: Vec<Value>,
        iter: crate::cursor::ArrEntries<'a>,
    },
    Obj {
        done: Vec<(String, Value)>,
        pending_key: Option<String>,
        iter: crate::cursor::ObjEntries<'a>,
    },
}

/// Rebuild a `Value` from any cursor (tape or arena) — iterative.
pub fn from_cursor(root: DocValue<'_>) -> Value {
    let mut stack: Vec<BuildFrame<'_>> = Vec::new();
    // `completed` carries a finished value up to its parent frame (or out).
    let mut completed: Option<Value> = begin(root, &mut stack);
    loop {
        let Some(top) = stack.last_mut() else {
            return completed.expect("walk ends with exactly the root value");
        };
        match top {
            BuildFrame::Arr { done, iter } => {
                if let Some(v) = completed.take() {
                    done.push(v);
                }
                match iter.next() {
                    Some(child) => completed = begin(child, &mut stack),
                    None => {
                        let BuildFrame::Arr { done, .. } = stack.pop().expect("top frame exists")
                        else {
                            unreachable!("matched Arr above")
                        };
                        completed = Some(Value::Arr(done));
                    }
                }
            }
            BuildFrame::Obj { done, pending_key, iter } => {
                if let Some(v) = completed.take() {
                    let key = pending_key.take().expect("value completes a pending key");
                    done.push((key, v));
                }
                match iter.next() {
                    Some((key, child)) => {
                        *pending_key = Some(key.to_str().to_owned());
                        completed = begin(child, &mut stack);
                    }
                    None => {
                        let BuildFrame::Obj { done, .. } = stack.pop().expect("top frame exists")
                        else {
                            unreachable!("matched Obj above")
                        };
                        completed = Some(Value::Obj(done));
                    }
                }
            }
        }
    }
}

/// Convert a scalar immediately, or push a frame for a container and
/// return `None` (its value completes when the frame drains).
fn begin<'a>(v: DocValue<'a>, stack: &mut Vec<BuildFrame<'a>>) -> Option<Value> {
    match v {
        DocValue::Null => Some(Value::Null),
        DocValue::Bool(b) => Some(Value::Bool(b)),
        DocValue::I64(i) => Some(Value::I64(i)),
        DocValue::F64(f) => Some(Value::F64(f)),
        DocValue::Str(s) => Some(Value::Str(s.to_str().to_owned())),
        DocValue::Arr(a) => {
            stack.push(BuildFrame::Arr { done: Vec::new(), iter: a.iter() });
            None
        }
        DocValue::Obj(o) => {
            let iter = ObjCursor::iter(&o);
            stack.push(BuildFrame::Obj { done: Vec::new(), pending_key: None, iter });
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_identity_on_a_mixed_document() {
        let v = Value::Obj(vec![
            ("s".into(), Value::Str("héllo\u{0}world".into())),
            ("n".into(), Value::I64(-1234567890123)),
            ("f".into(), Value::F64(-0.0)),
            ("a".into(), Value::Arr(vec![Value::Null, Value::Bool(true), Value::I64(127)])),
            ("o".into(), Value::Obj(vec![])),
        ]);
        let bytes = encode(&v).expect("encodes");
        let doc = TapeDoc::from_bytes(&bytes).expect("validates");
        assert_eq!(from_tape(&doc), v);
        // -0.0 preserved bit-exactly (PartialEq treats -0.0 == 0.0).
        let Value::Obj(entries) = from_tape(&doc) else { unreachable!() };
        let Value::F64(f) = entries[2].1 else { unreachable!() };
        assert_eq!(f.to_bits(), (-0.0f64).to_bits());
    }

    #[test]
    fn duplicate_keys_are_representable_and_order_preserved() {
        let v = Value::Obj(vec![("k".into(), Value::I64(1)), ("k".into(), Value::I64(2))]);
        let bytes = encode(&v).expect("encodes");
        let doc = TapeDoc::from_bytes(&bytes).expect("dup keys decode (D5: not rejected)");
        assert_eq!(from_tape(&doc), v);
    }
}
