//! Document-delta opcode v1 (M3-S17, ADR-0043 D4).
//!
//! `inf-log` owns only the opaque opcode byte and operand extent. This
//! module is the one semantic registry: live command capture encodes an
//! [`ApplyOp`], replay decodes the bytes back to the same type, and every
//! foreign fragment crosses the canonical idoc trust boundary here.
// ADR-0144 D2/D3: a decoder scope; docs/lint-scopes.tsv names its tier per lint family.
#![cfg_attr(
    not(test),
    deny(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_possible_wrap,
        clippy::arithmetic_side_effects
    )
)]

use core::fmt;

use inf_foundation::varint;

use crate::apply::{ApplyOp, Number};
use crate::tape::{ValueRef, canonical_fragment};
use crate::{DocError, emit};

/// One `Variant = tag` list emits the enum, [`DeltaOpcode::ALL`] and the
/// decoder's [`DeltaOpcode::from_u8`] (ADR-0169 §Class), so a variant
/// cannot exist outside the table or the decoder. A duplicate discriminant
/// does not compile, and a duplicate tag is an unreachable pattern that
/// `-D warnings` refuses.
macro_rules! delta_opcodes {
    ($($variant:ident = $tag:literal,)+) => {
        #[derive(Copy, Clone, PartialEq, Eq, Debug)]
        #[repr(u8)]
        pub enum DeltaOpcode {
            $($variant = $tag,)+
        }

        impl DeltaOpcode {
            /// Every opcode, in tag order: the table the writer/reader
            /// agreement property iterates.
            pub const ALL: &'static [DeltaOpcode] = &[$(DeltaOpcode::$variant,)+];

            pub const fn from_u8(tag: u8) -> Option<DeltaOpcode> {
                match tag {
                    $($tag => Some(DeltaOpcode::$variant),)+
                    _ => None,
                }
            }
        }
    };
}

delta_opcodes! {
    SetReplace = 1,
    SetMember = 2,
    Del = 3,
    NumIncrBy = 4,
    NumMultBy = 5,
    StrAppend = 6,
    Toggle = 7,
    Clear = 8,
    ArrAppend = 9,
    ArrInsert = 10,
    ArrPop = 11,
    ArrTrim = 12,
    Merge = 13,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum DeltaDecodeError {
    UnknownOpcode(u8),
    Truncated,
    TrailingBytes,
    BadVarint,
    BadUtf8,
    WrongOperandKind,
    /// An `ArrAppend`/`ArrInsert` operand with no element. The writer wraps
    /// at least one value (ADR-0042 D2), and the plan splices what the
    /// operand holds, so an empty one is no record a writer produced.
    EmptyArrayOperand,
    BadFragment(DocError),
}

impl fmt::Display for DeltaDecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DeltaDecodeError::UnknownOpcode(tag) => {
                write!(f, "unknown document delta opcode {tag}")
            }
            DeltaDecodeError::Truncated => write!(f, "truncated document delta operand"),
            DeltaDecodeError::TrailingBytes => write!(f, "trailing document delta operand bytes"),
            DeltaDecodeError::BadVarint => write!(f, "non-canonical document delta varint"),
            DeltaDecodeError::BadUtf8 => write!(f, "document delta member key is not UTF-8"),
            DeltaDecodeError::WrongOperandKind => write!(f, "wrong document delta operand kind"),
            DeltaDecodeError::EmptyArrayOperand => {
                write!(f, "document delta array operand holds no element")
            }
            DeltaDecodeError::BadFragment(error) => {
                write!(f, "invalid canonical document delta fragment: {error}")
            }
        }
    }
}

impl core::error::Error for DeltaDecodeError {}

/// Encode `op`'s canonical durable operand into a recycled caller buffer.
pub fn encode_apply_op(op: &ApplyOp<'_>, out: &mut Vec<u8>) -> DeltaOpcode {
    out.clear();
    match *op {
        ApplyOp::SetReplace { fragment } => {
            out.extend_from_slice(fragment);
            DeltaOpcode::SetReplace
        }
        ApplyOp::SetMember { key, fragment } => {
            varint::encode_u64(key.len() as u64, out);
            out.extend_from_slice(key);
            out.extend_from_slice(fragment);
            DeltaOpcode::SetMember
        }
        ApplyOp::Del => DeltaOpcode::Del,
        ApplyOp::NumIncrBy(number) => {
            encode_number(number, out);
            DeltaOpcode::NumIncrBy
        }
        ApplyOp::NumMultBy(number) => {
            encode_number(number, out);
            DeltaOpcode::NumMultBy
        }
        ApplyOp::StrAppend(bytes) => {
            emit::str(out, bytes);
            DeltaOpcode::StrAppend
        }
        ApplyOp::Toggle => DeltaOpcode::Toggle,
        ApplyOp::Clear => DeltaOpcode::Clear,
        ApplyOp::ArrAppend { elements } => {
            out.extend_from_slice(elements);
            DeltaOpcode::ArrAppend
        }
        ApplyOp::ArrInsert { index, elements } => {
            out.extend_from_slice(&index.to_le_bytes());
            out.extend_from_slice(elements);
            DeltaOpcode::ArrInsert
        }
        ApplyOp::ArrPop { index } => {
            out.extend_from_slice(&index.to_le_bytes());
            DeltaOpcode::ArrPop
        }
        ApplyOp::ArrTrim { start, stop } => {
            out.extend_from_slice(&start.to_le_bytes());
            out.extend_from_slice(&stop.to_le_bytes());
            DeltaOpcode::ArrTrim
        }
        ApplyOp::Merge { patch } => {
            out.extend_from_slice(patch);
            DeltaOpcode::Merge
        }
    }
}

/// Decode and fully validate one durable operand.
pub fn decode_apply_op(tag: u8, bytes: &[u8]) -> Result<ApplyOp<'_>, DeltaDecodeError> {
    let opcode = DeltaOpcode::from_u8(tag).ok_or(DeltaDecodeError::UnknownOpcode(tag))?;
    Ok(match opcode {
        DeltaOpcode::SetReplace => ApplyOp::SetReplace { fragment: fragment(bytes)? },
        DeltaOpcode::SetMember => {
            let (key_len, used) = varint::decode_u64(bytes).ok_or(if bytes.is_empty() {
                DeltaDecodeError::Truncated
            } else {
                DeltaDecodeError::BadVarint
            })?;
            let key_len = usize::try_from(key_len).map_err(|_| DeltaDecodeError::Truncated)?;
            let key_end = used.checked_add(key_len).ok_or(DeltaDecodeError::Truncated)?;
            let key = bytes.get(used..key_end).ok_or(DeltaDecodeError::Truncated)?;
            if core::str::from_utf8(key).is_err() {
                return Err(DeltaDecodeError::BadUtf8);
            }
            let fragment = fragment(bytes.get(key_end..).ok_or(DeltaDecodeError::Truncated)?)?;
            ApplyOp::SetMember { key, fragment }
        }
        DeltaOpcode::Del => {
            require_empty(bytes)?;
            ApplyOp::Del
        }
        DeltaOpcode::NumIncrBy => ApplyOp::NumIncrBy(number(bytes)?),
        DeltaOpcode::NumMultBy => ApplyOp::NumMultBy(number(bytes)?),
        DeltaOpcode::StrAppend => {
            let ValueRef::Str(value) =
                canonical_fragment(bytes).map_err(DeltaDecodeError::BadFragment)?
            else {
                return Err(DeltaDecodeError::WrongOperandKind);
            };
            ApplyOp::StrAppend(value.as_bytes())
        }
        DeltaOpcode::Toggle => {
            require_empty(bytes)?;
            ApplyOp::Toggle
        }
        DeltaOpcode::Clear => {
            require_empty(bytes)?;
            ApplyOp::Clear
        }
        DeltaOpcode::ArrAppend => {
            require_array(bytes)?;
            ApplyOp::ArrAppend { elements: bytes }
        }
        DeltaOpcode::ArrInsert => {
            let (index, tail) = i64_prefix(bytes)?;
            require_array(tail)?;
            ApplyOp::ArrInsert { index, elements: tail }
        }
        DeltaOpcode::ArrPop => {
            let (index, tail) = i64_prefix(bytes)?;
            require_empty(tail)?;
            ApplyOp::ArrPop { index }
        }
        DeltaOpcode::ArrTrim => {
            let (start, tail) = i64_prefix(bytes)?;
            let (stop, tail) = i64_prefix(tail)?;
            require_empty(tail)?;
            ApplyOp::ArrTrim { start, stop }
        }
        DeltaOpcode::Merge => ApplyOp::Merge { patch: fragment(bytes)? },
    })
}

fn fragment(bytes: &[u8]) -> Result<&[u8], DeltaDecodeError> {
    canonical_fragment(bytes).map_err(DeltaDecodeError::BadFragment)?;
    Ok(bytes)
}

fn number(bytes: &[u8]) -> Result<Number, DeltaDecodeError> {
    match canonical_fragment(bytes).map_err(DeltaDecodeError::BadFragment)? {
        ValueRef::I64(value) => Ok(Number::I64(value)),
        ValueRef::F64(value) => Ok(Number::F64(value)),
        ValueRef::Null
        | ValueRef::Bool(_)
        | ValueRef::Str(_)
        | ValueRef::Obj(_)
        | ValueRef::Arr(_) => Err(DeltaDecodeError::WrongOperandKind),
    }
}

/// The one array operand `ArrAppend` and `ArrInsert` carry: a canonical
/// array of at least one element, what `array_operand` writes.
fn require_array(bytes: &[u8]) -> Result<(), DeltaDecodeError> {
    match canonical_fragment(bytes).map_err(DeltaDecodeError::BadFragment)? {
        ValueRef::Arr(elements) if elements.is_empty() => Err(DeltaDecodeError::EmptyArrayOperand),
        ValueRef::Arr(_) => Ok(()),
        ValueRef::Null
        | ValueRef::Bool(_)
        | ValueRef::I64(_)
        | ValueRef::F64(_)
        | ValueRef::Str(_)
        | ValueRef::Obj(_) => Err(DeltaDecodeError::WrongOperandKind),
    }
}

fn require_empty(bytes: &[u8]) -> Result<(), DeltaDecodeError> {
    if bytes.is_empty() { Ok(()) } else { Err(DeltaDecodeError::TrailingBytes) }
}

fn i64_prefix(bytes: &[u8]) -> Result<(i64, &[u8]), DeltaDecodeError> {
    let raw = bytes.get(..8).ok_or(DeltaDecodeError::Truncated)?;
    let value = i64::from_le_bytes(raw.try_into().expect("8-byte prefix"));
    Ok((value, &bytes[8..]))
}

fn encode_number(number: Number, out: &mut Vec<u8>) {
    match number {
        Number::I64(value) => emit::i64(out, value),
        Number::F64(value) => emit::f64(out, value),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{self, Value};

    #[test]
    fn every_opcode_round_trips() {
        let scalar = model::encode_fragment(&Value::I64(7)).expect("fragment");
        let array = model::encode_fragment(&Value::Arr(vec![Value::I64(1)])).expect("fragment");
        let ops = [
            ApplyOp::SetReplace { fragment: &scalar },
            ApplyOp::SetMember { key: b"k", fragment: &scalar },
            ApplyOp::Del,
            ApplyOp::NumIncrBy(Number::I64(2)),
            ApplyOp::NumMultBy(Number::F64(1.5)),
            ApplyOp::StrAppend(b"tail"),
            ApplyOp::Toggle,
            ApplyOp::Clear,
            ApplyOp::ArrAppend { elements: &array },
            ApplyOp::ArrInsert { index: -1, elements: &array },
            ApplyOp::ArrPop { index: 3 },
            ApplyOp::ArrTrim { start: -2, stop: 4 },
            ApplyOp::Merge { patch: &scalar },
        ];
        let mut encoded = Vec::new();
        for op in ops {
            let opcode = encode_apply_op(&op, &mut encoded);
            let decoded = decode_apply_op(opcode as u8, &encoded).expect("decodes");
            let mut again = Vec::new();
            let again_opcode = encode_apply_op(&decoded, &mut again);
            assert_eq!(again_opcode, opcode);
            assert_eq!(again, encoded);
        }
    }

    /// The generated table and decoder agree: `ALL` round-trips through
    /// `from_u8` in tag order, and no other tag decodes.
    #[test]
    fn the_opcode_table_is_the_decoder() {
        for (index, opcode) in DeltaOpcode::ALL.iter().enumerate() {
            assert_eq!(usize::from(*opcode as u8), index + 1);
            assert_eq!(DeltaOpcode::from_u8(*opcode as u8), Some(*opcode));
        }
        let decoded = (0..=u8::MAX).filter_map(DeltaOpcode::from_u8).count();
        assert_eq!(decoded, DeltaOpcode::ALL.len());
    }

    #[test]
    fn wrong_kinds_and_trailing_bytes_are_typed() {
        assert_eq!(decode_apply_op(3, b"x").unwrap_err(), DeltaDecodeError::TrailingBytes);
        assert_eq!(decode_apply_op(9, &[1]).unwrap_err(), DeltaDecodeError::WrongOperandKind);
        assert_eq!(decode_apply_op(99, &[]).unwrap_err(), DeltaDecodeError::UnknownOpcode(99));
    }

    /// The array operand's reader bound is its writer's: at least one
    /// element. `[]` (`TAG_ARR`, length 0) is a canonical fragment, so only
    /// this check keeps a zero-element splice out of the plan.
    #[test]
    fn an_empty_array_operand_is_refused() {
        let empty = [0xA8, 0, 0, 0];
        assert!(canonical_fragment(&empty).is_ok(), "the fixture is a canonical empty array");
        let append = DeltaOpcode::ArrAppend as u8;
        let refused = DeltaDecodeError::EmptyArrayOperand;
        assert_eq!(decode_apply_op(append, &empty).unwrap_err(), refused);
        let mut insert = Vec::new();
        encode_apply_op(&ApplyOp::ArrInsert { index: 0, elements: &empty }, &mut insert);
        assert_eq!(decode_apply_op(DeltaOpcode::ArrInsert as u8, &insert).unwrap_err(), refused);
        let one = [0xA8, 1, 0, 0, 0x07];
        assert!(decode_apply_op(append, &one).is_ok(), "one element decodes");
    }
}
