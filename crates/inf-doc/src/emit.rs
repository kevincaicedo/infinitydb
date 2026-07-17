//! Canonical byte-emission primitives (ADR-0036 D3): the one place that
//! chooses encodings — fixint vs `0xA3`+varint for i64, fixstr/str8/str24
//! width for strings, the u24 container-length backpatch. Two drivers with
//! different invariant strategies consume it:
//!
//! - [`TapeBuilder`](crate::TapeBuilder) wraps every call in typed-error
//!   guards (size cap, depth, key/value parity) — its drivers walk
//!   arbitrary model/mutation state and must be refused, not trusted.
//! - The S05 `JsonParser` calls these directly: its grammar machine is
//!   the invariant authority (alternation, depth, and the S07 size cap
//!   are enforced by the parser itself), so the hot ingest path pays no
//!   second bookkeeping stack and no per-value `Result` plumbing.
//!
//! One value, one encoding (L7) holds because width selection lives here
//! alone; the goldens pin it byte-exact from both drivers.

use crate::limits::DOC_BYTES_MAX;
use crate::tape::{
    FIXINT_MAX, FIXINT_MIN, FIXSTR_BASE, FIXSTR_MAX_LEN, STR24_MIN_LEN, TAG_F64, TAG_FALSE,
    TAG_I64, TAG_NULL, TAG_STR8, TAG_STR24, TAG_TRUE, zigzag,
};

#[inline]
pub(crate) fn null(out: &mut Vec<u8>) {
    out.push(TAG_NULL);
}

#[inline]
pub(crate) fn bool(out: &mut Vec<u8>, v: bool) {
    out.push(if v { TAG_TRUE } else { TAG_FALSE });
}

/// Worst-case i64 emission: tag + 10-byte varint.
pub(crate) const I64_MAX_LEN: usize = 11;

#[inline]
pub(crate) fn i64(out: &mut Vec<u8>, v: i64) {
    let mut encoded = [0u8; I64_MAX_LEN];
    let len = i64_into(&mut encoded, v);
    out.extend_from_slice(&encoded[..len]);
}

/// Canonical i64 bytes into caller stack storage.
#[inline]
pub(crate) fn i64_into(out: &mut [u8; I64_MAX_LEN], v: i64) -> usize {
    if (FIXINT_MIN..=FIXINT_MAX).contains(&v) {
        out[0] = v as u8; // two's complement byte IS the fixint tag
        return 1;
    }
    out[0] = TAG_I64;
    let mut raw = zigzag(v);
    let mut at = 1;
    while raw >= 0x80 {
        out[at] = (raw as u8) | 0x80;
        raw >>= 7;
        at += 1;
    }
    out[at] = raw as u8;
    at + 1
}

#[inline]
pub(crate) fn i64_len(v: i64) -> usize {
    if (FIXINT_MIN..=FIXINT_MAX).contains(&v) {
        1
    } else {
        let bits = zigzag(v);
        1 + (64 - bits.leading_zeros() as usize).max(1).div_ceil(7)
    }
}

/// F64 emission cost: tag + 8 payload bytes.
pub(crate) const F64_LEN: usize = 9;

/// `v` must be finite — both drivers refuse NaN/±Inf with a typed error
/// before bytes exist (the RedisJSON model).
#[inline]
pub(crate) fn f64(out: &mut Vec<u8>, v: f64) {
    let mut encoded = [0u8; F64_LEN];
    f64_into(&mut encoded, v);
    out.extend_from_slice(&encoded);
}

#[inline]
pub(crate) fn f64_into(out: &mut [u8; F64_LEN], v: f64) {
    debug_assert!(v.is_finite(), "non-finite f64 refused before emission");
    out[0] = TAG_F64;
    out[1..].copy_from_slice(&v.to_bits().to_le_bytes());
}

/// Header bytes a string of `len` costs (also the S05 duplicate-key
/// span arithmetic: key bytes sit exactly this far into the entry).
#[inline]
pub(crate) const fn str_header_len(len: usize) -> usize {
    if len <= FIXSTR_MAX_LEN {
        1
    } else if len < STR24_MIN_LEN {
        2
    } else {
        4
    }
}

/// The canonical string header alone (tag + width-selected length) — the
/// S05 parser follows it with its own overlapped payload copy.
#[inline]
pub(crate) fn str_header(out: &mut Vec<u8>, len: usize) {
    if len <= FIXSTR_MAX_LEN {
        out.push(FIXSTR_BASE + len as u8);
    } else if len < STR24_MIN_LEN {
        out.push(TAG_STR8);
        out.push(len as u8);
    } else {
        out.push(TAG_STR24);
        let bytes = (len as u32).to_le_bytes();
        out.extend_from_slice(&bytes[..3]);
    }
}

/// The caller owns the UTF-8 invariant (`&str` at the public builder
/// surface; whole-input validation + valid-by-construction unescaping in
/// the parser) — asserted here as the pair check, never re-paid.
#[inline]
pub(crate) fn str(out: &mut Vec<u8>, s: &[u8]) {
    debug_assert!(core::str::from_utf8(s).is_ok(), "emit::str payload is UTF-8");
    str_header(out, s.len());
    out.extend_from_slice(s);
}

/// Container open cost: tag + 3-byte length placeholder.
pub(crate) const CONTAINER_OPEN_LEN: usize = 4;

/// Open a container: tag + u24 placeholder; returns the placeholder
/// offset for [`end`].
#[inline]
pub(crate) fn begin(out: &mut Vec<u8>, tag: u8) -> usize {
    out.push(tag);
    let len_at = out.len();
    out.extend_from_slice(&[0, 0, 0]);
    len_at
}

/// Backpatch the u24 body length recorded by [`begin`]. Fixed width means
/// children never move (the D3 backpatch argument).
#[inline]
pub(crate) fn end(out: &mut [u8], len_at: usize) {
    let body_len = out.len() - (len_at + 3);
    debug_assert!(body_len <= DOC_BYTES_MAX, "cap enforced incrementally by the driver");
    let bytes = (body_len as u32).to_le_bytes();
    out[len_at..len_at + 3].copy_from_slice(&bytes[..3]);
}
