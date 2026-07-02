//! Log record format **v1** (M2-S01; freezes at M2 exit — milestone §3.2,
//! ADR-0011). Every later subsystem replays these bytes, so the format is
//! canonical by construction: one value, one encoding, byte-exact
//! decode→encode round-trips (L7 — replay digests depend on it).
//!
//! ```text
//! record := varint(body_len) body
//! body   := type: u8 · flags: u8 · varint(ns) · payload
//! ```
//!
//! Varints are the canonical LEB128 from `inf-foundation` (non-minimal
//! encodings are decode errors). `body_len` covers everything after the
//! length prefix. The v1 vocabulary covers the M1 surface — string
//! post-images, deletes, absolute expiry, namespace DDL (ADR-0011 records
//! the post-image-vs-delta trade; M3 adds collection deltas through new
//! `type` tags without touching this framing). Unknown types and unknown
//! flag bits are **fail-stop decode errors**, never skipped (§8.4 honesty:
//! replaying a newer log on an older binary must refuse, not corrupt).

use core::fmt;

use inf_foundation::varint;

/// Namespace id as recorded in the log. The store layer (M2-S08) owns the
/// name↔id catalog; the log spine treats it as an opaque routing tag.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct NsId(pub u32);

/// Record type tags, v1. Discriminants are wire format — never reuse or
/// renumber (the registry only grows: M3 collection ops, M2-S10 `ckpt-begin`
/// claim tag 5 onward).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum RecordType {
    /// Full value image of a string key (replay = blind upsert, idempotent).
    StringPostImage = 1,
    /// Key removal.
    Delete = 2,
    /// Absolute expiry in unix milliseconds. Absolute — not relative — so
    /// replay is deterministic regardless of when it runs (L7).
    ExpireAt = 3,
    /// Namespace DDL (payload vocabulary owned by M2-S08).
    NsOp = 4,
}

impl RecordType {
    #[inline]
    fn from_u8(tag: u8) -> Option<RecordType> {
        Some(match tag {
            1 => RecordType::StringPostImage,
            2 => RecordType::Delete,
            3 => RecordType::ExpireAt,
            4 => RecordType::NsOp,
            _ => return None,
        })
    }
}

/// v1 defines no flag bits; the byte is reserved wire space. Encoders write
/// zero, decoders reject anything else (one value, one encoding).
const RECORD_FLAGS_V1: u8 = 0;

/// A decoded record, borrowing payload bytes from the frame body —
/// zero-copy on the replay path. Field invariants (flags == 0, known type)
/// hold by construction: invalid records are unrepresentable as views.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum RecordView<'a> {
    StringPostImage { ns: NsId, key: &'a [u8], value: &'a [u8] },
    Delete { ns: NsId, key: &'a [u8] },
    ExpireAt { ns: NsId, at_unix_ms: u64, key: &'a [u8] },
    NsOp { ns: NsId, payload: &'a [u8] },
}

impl RecordView<'_> {
    /// Exact encoded size, length prefix included. Cheap arithmetic — used
    /// by the staging accounting (`log_staging_bytes`, L5) and the frame
    /// builder.
    #[must_use]
    pub fn encoded_len(&self) -> usize {
        let body = self.body_len();
        varint_len(body as u64) + body
    }

    fn body_len(&self) -> usize {
        // type + flags = 2 bytes, then per-variant fields.
        2 + match *self {
            RecordView::StringPostImage { ns, key, value } => {
                varint_len(u64::from(ns.0)) + varint_len(key.len() as u64) + key.len() + value.len()
            }
            RecordView::Delete { ns, key } => varint_len(u64::from(ns.0)) + key.len(),
            RecordView::ExpireAt { ns, at_unix_ms, key } => {
                varint_len(u64::from(ns.0)) + varint_len(at_unix_ms) + key.len()
            }
            RecordView::NsOp { ns, payload } => varint_len(u64::from(ns.0)) + payload.len(),
        }
    }

    /// Append the canonical encoding to `out`. Writes straight into the
    /// caller's staging memory — no intermediate buffer (M2-S01 task).
    pub fn encode_into(&self, out: &mut Vec<u8>) {
        varint::encode_u64(self.body_len() as u64, out);
        match *self {
            RecordView::StringPostImage { ns, key, value } => {
                out.push(RecordType::StringPostImage as u8);
                out.push(RECORD_FLAGS_V1);
                varint::encode_u64(u64::from(ns.0), out);
                varint::encode_u64(key.len() as u64, out);
                out.extend_from_slice(key);
                out.extend_from_slice(value);
            }
            RecordView::Delete { ns, key } => {
                out.push(RecordType::Delete as u8);
                out.push(RECORD_FLAGS_V1);
                varint::encode_u64(u64::from(ns.0), out);
                out.extend_from_slice(key);
            }
            RecordView::ExpireAt { ns, at_unix_ms, key } => {
                out.push(RecordType::ExpireAt as u8);
                out.push(RECORD_FLAGS_V1);
                varint::encode_u64(u64::from(ns.0), out);
                varint::encode_u64(at_unix_ms, out);
                out.extend_from_slice(key);
            }
            RecordView::NsOp { ns, payload } => {
                out.push(RecordType::NsOp as u8);
                out.push(RECORD_FLAGS_V1);
                varint::encode_u64(u64::from(ns.0), out);
                out.extend_from_slice(payload);
            }
        }
    }
}

/// Why a record failed to decode. Inside a CRC-valid frame every variant
/// means corruption-or-bug and replay must fail-stop (§8.4); none of these
/// is ever skipped silently.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum RecordDecodeError {
    /// Buffer ends before the record's declared extent.
    Truncated,
    /// Malformed or non-canonical LEB128.
    Varint,
    /// Type tag outside the registered vocabulary.
    UnknownType(u8),
    /// Flag bits v1 does not define.
    UnknownFlags(u8),
    /// Namespace id exceeds `u32`.
    NsOutOfRange(u64),
    /// A declared key length overruns the record body.
    KeyOverrun,
}

impl fmt::Display for RecordDecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RecordDecodeError::Truncated => write!(f, "record truncated"),
            RecordDecodeError::Varint => write!(f, "malformed or non-canonical varint"),
            RecordDecodeError::UnknownType(tag) => write!(f, "unknown record type {tag}"),
            RecordDecodeError::UnknownFlags(bits) => {
                write!(f, "unknown record flag bits {bits:#04x}")
            }
            RecordDecodeError::NsOutOfRange(ns) => write!(f, "namespace id {ns} out of range"),
            RecordDecodeError::KeyOverrun => write!(f, "key length overruns record body"),
        }
    }
}

impl std::error::Error for RecordDecodeError {}

/// Decode one record from the front of `buf`. Returns the view and the
/// exact byte count consumed; `decode` then `encode_into` reproduces the
/// input bytes exactly (the fuzz target asserts this).
pub fn decode_record(buf: &[u8]) -> Result<(RecordView<'_>, usize), RecordDecodeError> {
    let (body_len, prefix_len) = varint::decode_u64(buf).ok_or(if buf.is_empty() {
        RecordDecodeError::Truncated
    } else {
        RecordDecodeError::Varint
    })?;
    let body_len = usize::try_from(body_len).map_err(|_| RecordDecodeError::Truncated)?;
    let body = buf
        .get(prefix_len..prefix_len.checked_add(body_len).ok_or(RecordDecodeError::Truncated)?)
        .ok_or(RecordDecodeError::Truncated)?;

    let [tag, flags, fields @ ..] = body else {
        return Err(RecordDecodeError::Truncated);
    };
    let record_type = RecordType::from_u8(*tag).ok_or(RecordDecodeError::UnknownType(*tag))?;
    if *flags != RECORD_FLAGS_V1 {
        return Err(RecordDecodeError::UnknownFlags(*flags));
    }
    let (ns_raw, ns_len) = varint::decode_u64(fields).ok_or(if fields.is_empty() {
        RecordDecodeError::Truncated
    } else {
        RecordDecodeError::Varint
    })?;
    let ns = NsId(u32::try_from(ns_raw).map_err(|_| RecordDecodeError::NsOutOfRange(ns_raw))?);
    let payload = &fields[ns_len..];

    let view = match record_type {
        RecordType::StringPostImage => {
            let (klen, klen_len) = varint::decode_u64(payload).ok_or(if payload.is_empty() {
                RecordDecodeError::Truncated
            } else {
                RecordDecodeError::Varint
            })?;
            let klen = usize::try_from(klen).map_err(|_| RecordDecodeError::KeyOverrun)?;
            let rest = &payload[klen_len..];
            if klen > rest.len() {
                return Err(RecordDecodeError::KeyOverrun);
            }
            let (key, value) = rest.split_at(klen);
            RecordView::StringPostImage { ns, key, value }
        }
        RecordType::Delete => RecordView::Delete { ns, key: payload },
        RecordType::ExpireAt => {
            let (at_unix_ms, at_len) =
                varint::decode_u64(payload).ok_or(if payload.is_empty() {
                    RecordDecodeError::Truncated
                } else {
                    RecordDecodeError::Varint
                })?;
            RecordView::ExpireAt { ns, at_unix_ms, key: &payload[at_len..] }
        }
        RecordType::NsOp => RecordView::NsOp { ns, payload },
    };
    Ok((view, prefix_len + body_len))
}

/// Byte length of the canonical LEB128 encoding of `v`.
#[inline]
fn varint_len(v: u64) -> usize {
    if v == 0 { 1 } else { ((64 - v.leading_zeros()) as usize).div_ceil(7) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(view: RecordView<'_>) -> Vec<u8> {
        let mut buf = Vec::new();
        view.encode_into(&mut buf);
        assert_eq!(buf.len(), view.encoded_len(), "encoded_len must be exact");
        let (decoded, consumed) = decode_record(&buf).expect("canonical bytes decode");
        assert_eq!(consumed, buf.len());
        assert_eq!(decoded, view);
        buf
    }

    #[test]
    fn all_variants_round_trip() {
        round_trip(RecordView::StringPostImage {
            ns: NsId(0),
            key: b"user:42",
            value: b"hello world",
        });
        round_trip(RecordView::StringPostImage { ns: NsId(u32::MAX), key: b"", value: b"" });
        round_trip(RecordView::Delete { ns: NsId(3), key: b"gone" });
        round_trip(RecordView::ExpireAt { ns: NsId(7), at_unix_ms: u64::MAX, key: b"session" });
        round_trip(RecordView::NsOp { ns: NsId(1), payload: b"create ledger" });
    }

    #[test]
    fn unknown_type_and_flags_fail_stop() {
        let mut buf = Vec::new();
        RecordView::Delete { ns: NsId(1), key: b"k" }.encode_into(&mut buf);
        let mut bad_type = buf.clone();
        bad_type[1] = 0xEE;
        assert_eq!(decode_record(&bad_type), Err(RecordDecodeError::UnknownType(0xEE)));
        let mut bad_flags = buf;
        bad_flags[2] = 0x01;
        assert_eq!(decode_record(&bad_flags), Err(RecordDecodeError::UnknownFlags(0x01)));
    }

    #[test]
    fn truncation_is_detected() {
        let mut buf = Vec::new();
        RecordView::StringPostImage { ns: NsId(9), key: b"key", value: b"value" }
            .encode_into(&mut buf);
        for cut in 0..buf.len() {
            assert!(decode_record(&buf[..cut]).is_err(), "cut at {cut} must not decode");
        }
    }

    #[test]
    fn key_overrun_is_detected() {
        // StringPostImage claiming klen beyond its body.
        let mut buf = Vec::new();
        RecordView::StringPostImage { ns: NsId(1), key: b"abc", value: b"" }.encode_into(&mut buf);
        // body: type flags ns klen(3) 'a' 'b' 'c' — bump klen to 200.
        let klen_index = buf.len() - 4;
        assert_eq!(buf[klen_index], 3);
        buf[klen_index] = 200;
        assert_eq!(decode_record(&buf), Err(RecordDecodeError::KeyOverrun));
    }
}
