//! Order-preserving typed index-key encoding **v1** (M4.5-S02,
//! ADR-0074; the §3.2 freeze row "Typed index-key encoding v1").
//!
//! Index keys are compared as raw bytes — memcmp is the tree's fast path
//! — so every typed layout here preserves its type's order under byte
//! comparison with the shorter-is-smaller tie-break [`crate::OrderedMap`]
//! implements. Keys are **pure payload** (ADR-0074 D1): no per-key type
//! tag or version byte — the type lives in the S03 registry entry, and
//! [`INDEX_KEY_ENCODING_VERSION`] is bound into every checkpoint sidecar
//! header (ADR-0073 D5.2). Numeric/bool keys are exactly 8 bytes (the
//! tree's `Fixed8` scheme); utf8 keys are `VarKey`.
//!
//! The **§3.1 one-truth-table rule** lives here: [`index_scalar_coerce`]
//! is the entire admission + numeric-coercion table, imported by both
//! the S04 maintenance hook and the S07/S08 predicate VM — two call
//! sites, one function, so index membership and VM verdicts cannot
//! diverge where the equivalence oracle looks. [`compare_i64_f64`] is
//! the VM's exact cross-numeric comparison, consistent with encoded-byte
//! order wherever both sides admit (proptested, fuzzed).
//!
//! The hot path never decodes: [`index_key_decode`] serves `EXPLAIN` and
//! debug rendering only, and is canonical-strict (accepts exactly what
//! the encoder can produce) so the fuzz target's round-trip laws hold.

use core::cmp::Ordering;

use crate::ordered::ORDERED_KEY_MAX;

/// Encoding version bound into registry entries and sidecar headers
/// (ADR-0073 D5.2) — never into key bytes (ADR-0074 D1). A future v2
/// refuses v1 sidecar bytes into a v2 tree: mismatch ⇒ rebuild, logged.
pub const INDEX_KEY_ENCODING_VERSION: u16 = 1;

/// The sign bit of an 8-byte word — both numeric layouts pivot on it
/// (i64 offset-binary, f64 total-order sign-flip).
const SIGN_BIT: u64 = 0x8000_0000_0000_0000;

/// `2^63` as f64 (exact). The numeric-range guard for i64↔f64 coercion:
/// the saturating `as i64` back-cast would let `i64::MAX as f64` (which
/// rounds to 2^63) "round-trip" — range-check first (ADR-0074 D4.1).
const TWO_POW_63: f64 = 9_223_372_036_854_775_808.0;

/// Declared key type of one index — one index, one type, one tree
/// (§3.2 registry freeze row). Cross-type byte order is meaningless by
/// construction and the encoding spends no bytes disambiguating it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexKeyType {
    /// UTF-8 string keys (`VarKey` tree scheme).
    Utf8,
    /// Signed-integer keys, exactly 8 encoded bytes (`Fixed8`).
    I64,
    /// IEEE-754 double keys, exactly 8 encoded bytes (`Fixed8`).
    F64,
    /// Boolean keys, exactly 8 encoded bytes (`Fixed8`).
    Bool,
}

impl IndexKeyType {
    /// True for the exactly-8-byte encodings — the S03 registry picks
    /// the tree's key scheme (`Fixed8` vs `VarKey`) by this.
    pub fn fixed8(self) -> bool {
        !matches!(self, IndexKeyType::Utf8)
    }
}

/// A scalar as the maintenance hook and predicate VM see it — mapped
/// 1:1 from `inf_doc::DocValue` scalars (containers are never
/// indexable; the S04 evaluator yields scalars only).
#[derive(Clone, Copy, Debug)]
pub enum IndexScalar<'a> {
    /// Explicit JSON null — **null-absent**: never produces an entry.
    Null,
    Bool(bool),
    I64(i64),
    F64(f64),
    Utf8(&'a str),
}

/// Why a value produced no entry (ADR-0074 D6). `Sparse` is the
/// DynamoDB sparse-index *feature* (type mismatch, null); the other
/// three are counted anomalies (`idx_skipped_inexact` /
/// `idx_skipped_nan` / `idx_skipped_toolong`, wired by S04) — nothing
/// skips silently (L10).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeySkip {
    /// Type-mismatched value or null: no entry, by sparse semantics.
    Sparse,
    /// Numeric value not losslessly representable in the declared type.
    Inexact,
    /// NaN at an indexed f64/i64 path: rejected, counted, never a panic.
    NotANumber,
    /// Encoded key would exceed [`ORDERED_KEY_MAX`] bytes.
    TooLong,
}

/// Caller-owned key storage — S04 keeps these in per-cell scratch; no
/// per-operation allocation on the hot path. Deliberately not `Copy`:
/// a silent 1 KiB copy is exactly the hidden cost the style forbids.
#[derive(Clone, Debug)]
pub struct IndexKeyBuf {
    len: u16,
    bytes: [u8; ORDERED_KEY_MAX],
}

impl Default for IndexKeyBuf {
    fn default() -> Self {
        IndexKeyBuf { len: 0, bytes: [0u8; ORDERED_KEY_MAX] }
    }
}

impl IndexKeyBuf {
    pub fn new() -> Self {
        Self::default()
    }

    /// The encoded key (empty before the first successful encode).
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len as usize]
    }

    /// Store an 8-byte big-endian word (every `Fixed8` layout).
    fn put_word(&mut self, word: u64) {
        self.bytes[..8].copy_from_slice(&word.to_be_bytes());
        self.len = 8;
    }
}

/// **The §3.1 truth table** (ADR-0074 D4): what a declared `key_type`
/// index does with `value`. Returns the (possibly coerced) scalar that
/// [`index_key_encode`] will lay out, or the typed skip. One function,
/// two importers — the S04 encoder side and the S07 VM side; the VM
/// never re-derives admission.
pub fn index_scalar_coerce(
    key_type: IndexKeyType,
    value: IndexScalar<'_>,
) -> Result<IndexScalar<'_>, KeySkip> {
    match (key_type, value) {
        (IndexKeyType::I64, IndexScalar::I64(_)) => Ok(value),
        (IndexKeyType::I64, IndexScalar::F64(f)) => coerce_f64_to_i64(f),
        (IndexKeyType::F64, IndexScalar::I64(v)) => coerce_i64_to_f64(v),
        (IndexKeyType::F64, IndexScalar::F64(f)) => {
            if f.is_nan() {
                Err(KeySkip::NotANumber)
            } else {
                // ±∞ admitted — they order totally; -0.0 canonicalizes
                // to +0.0 at encode (ADR-0074 D2).
                Ok(value)
            }
        }
        (IndexKeyType::Bool, IndexScalar::Bool(_)) => Ok(value),
        (IndexKeyType::Utf8, IndexScalar::Utf8(s)) => {
            if utf8_encoded_len(s) > ORDERED_KEY_MAX {
                Err(KeySkip::TooLong)
            } else {
                Ok(value)
            }
        }
        // Everything else — null anywhere, and every non-numeric type
        // mismatch — is the sparse-index rule: no entry, not an anomaly.
        _ => Err(KeySkip::Sparse),
    }
}

/// i64 → a declared f64 index: admit iff the conversion is lossless
/// (ADR-0074 D4.1 — the rule is exactness, not the `|v| ≤ 2⁵³` band).
fn coerce_i64_to_f64(v: i64) -> Result<IndexScalar<'static>, KeySkip> {
    let f = v as f64;
    // Range guard before the round-trip check: `i64::MAX as f64` rounds
    // to 2^63 and the saturating back-cast would falsely "round-trip".
    if (-TWO_POW_63..TWO_POW_63).contains(&f) && f as i64 == v {
        Ok(IndexScalar::F64(f))
    } else {
        Err(KeySkip::Inexact)
    }
}

/// f64 → a declared i64 index: admit iff finite, integral, in range —
/// then `as i64` is exact by construction.
fn coerce_f64_to_i64(f: f64) -> Result<IndexScalar<'static>, KeySkip> {
    if f.is_nan() {
        return Err(KeySkip::NotANumber);
    }
    // trunc() of an in-range value is an integral f64 in [-2^63, 2^63),
    // exactly convertible; fractional / ±∞ / out-of-range are Inexact.
    if (-TWO_POW_63..TWO_POW_63).contains(&f) && f.trunc() == f {
        Ok(IndexScalar::I64(f as i64))
    } else {
        Err(KeySkip::Inexact)
    }
}

/// Encoded length of a utf8 key: raw + one escape byte per NUL + the
/// terminator (ADR-0074 D3 — the cap is on *encoded* length).
fn utf8_encoded_len(s: &str) -> usize {
    let nuls = s.as_bytes().iter().filter(|&&b| b == 0).count();
    s.len() + nuls + 1
}

/// Encode `value` for a `key_type` index into `out` (ADR-0074 D2).
/// `Ok` ⇒ `out.as_bytes()` is the tree key; `Err` ⇒ no entry, reason
/// typed. Coercion and every admission rule run through
/// [`index_scalar_coerce`] — this function adds layout only.
pub fn index_key_encode(
    key_type: IndexKeyType,
    value: IndexScalar<'_>,
    out: &mut IndexKeyBuf,
) -> Result<(), KeySkip> {
    let admitted = index_scalar_coerce(key_type, value)?;
    match admitted {
        IndexScalar::I64(v) => out.put_word((v as u64) ^ SIGN_BIT),
        IndexScalar::F64(f) => out.put_word(f64_key_word(f)),
        IndexScalar::Bool(b) => out.put_word(u64::from(b)),
        IndexScalar::Utf8(s) => encode_utf8(s, out),
        // The truth table never admits null (null-absent, D4).
        IndexScalar::Null => unreachable!("coerce admitted null"),
    }
    debug_assert!(!out.as_bytes().is_empty(), "every key is >= 1 byte");
    debug_assert!(!key_type.fixed8() || out.len == 8, "fixed8 keys are exactly 8 bytes");
    Ok(())
}

/// f64 total-order sign-flip on the canonicalized value (ADR-0074 D2):
/// `-0.0` normalizes to `+0.0`; negatives invert wholly (reversing
/// their magnitude order), positives set the sign bit — so unsigned
/// byte order runs `-∞ .. -0.0=+0.0 .. +∞`.
fn f64_key_word(f: f64) -> u64 {
    debug_assert!(!f.is_nan(), "NaN is rejected by coerce before encode");
    let canonical = if f == 0.0 { 0.0 } else { f };
    let bits = canonical.to_bits();
    if bits & SIGN_BIT != 0 { !bits } else { bits | SIGN_BIT }
}

/// String layout: per-byte escape `0x00 → [0x00, 0xFF]`, then the
/// `0x00` terminator. The escape code is prefix-free and per-byte
/// order-preserving: memcmp ≡ raw byte order ≡ code-point order, and
/// `s` starts_with `p` ⟺ `enc(s)` starts_with `escape(p)` — the S09
/// `begins_with` bound construction (ADR-0074 D2).
fn encode_utf8(s: &str, out: &mut IndexKeyBuf) {
    let bytes = s.as_bytes();
    // Fast path: no NUL — one copy + terminator. The scan is bounded by
    // the D3 cap check in coerce (encoded length ≤ ORDERED_KEY_MAX).
    if !bytes.contains(&0) {
        out.bytes[..bytes.len()].copy_from_slice(bytes);
        out.bytes[bytes.len()] = 0x00;
        out.len = (bytes.len() + 1) as u16;
        return;
    }
    let mut len: usize = 0;
    for &b in bytes {
        out.bytes[len] = b;
        len += 1;
        if b == 0x00 {
            out.bytes[len] = 0xFF;
            len += 1;
        }
    }
    out.bytes[len] = 0x00;
    out.len = (len + 1) as u16;
}

/// The terminator-less escape image of `s`, truncated to
/// [`ORDERED_KEY_MAX`] bytes; returns the **untruncated** escaped
/// length so callers can tell truncation happened. This is the S09
/// bound-construction primitive (ADR-0080 D3): `begins_with` lower
/// bounds are the full image (prefix safety, D2), and over-cap
/// comparison literals bind at the truncated image — the escape rule
/// stays in this module (the §3.1 one-implementation discipline), it
/// is never re-derived by a consumer.
pub fn index_key_escape_prefix(s: &str, out: &mut IndexKeyBuf) -> usize {
    let mut len: usize = 0;
    let mut full: usize = 0;
    for &b in s.as_bytes() {
        full += 1;
        if len < ORDERED_KEY_MAX {
            out.bytes[len] = b;
            len += 1;
        }
        if b == 0x00 {
            full += 1;
            if len < ORDERED_KEY_MAX {
                out.bytes[len] = 0xFF;
                len += 1;
            }
        }
    }
    out.len = len as u16;
    debug_assert_eq!(len, full.min(ORDERED_KEY_MAX));
    full
}

/// Exact i64-vs-f64 comparison — the VM's cross-numeric compare
/// (ADR-0074 D5). No lossy casts: range-classify, then compare integer
/// parts via exact truncation, then the fractional sign. NaN is a
/// precondition violation — the VM rejects NaN before comparing (D4.4).
pub fn compare_i64_f64(a: i64, b: f64) -> Ordering {
    debug_assert!(!b.is_nan(), "NaN is rejected before comparison (ADR-0074 D4)");
    if b >= TWO_POW_63 {
        return Ordering::Less; // +∞ included: every i64 < 2^63 ≤ b.
    }
    if b < -TWO_POW_63 {
        return Ordering::Greater; // -∞ included.
    }
    // b ∈ [-2^63, 2^63): trunc(b) is an integral f64 in the same range,
    // hence exactly convertible to i64.
    let truncated = b.trunc();
    let whole = truncated as i64;
    match a.cmp(&whole) {
        // Equal integer parts: the discarded fraction decides. trunc
        // rounds toward zero, so b > truncated ⇔ positive fraction.
        Ordering::Equal => {
            if b > truncated {
                Ordering::Less
            } else if b < truncated {
                Ordering::Greater
            } else {
                Ordering::Equal
            }
        }
        unequal => unequal,
    }
}

/// A decoded key for `EXPLAIN`/debug rendering (ADR-0074 D7). The
/// `String` allocation is deliberate — this type never appears on the
/// hot path (the hot path never decodes).
#[derive(Clone, Debug, PartialEq)]
pub enum DecodedIndexKey {
    Utf8(String),
    I64(i64),
    F64(f64),
    Bool(bool),
}

/// Canonical-strict decode failures. Operating conditions (a stale or
/// hand-built key byte string is untrusted input), never panics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexKeyDecodeError {
    /// Wrong byte length for the type (or empty / over the D3 cap).
    Length,
    /// A bool word other than 0 or 1.
    NotABool,
    /// A bit pattern the f64 encoder cannot produce (NaN, `-0.0`).
    NotCanonicalF64,
    /// `0x00` inside the body not followed by `0xFF`.
    Escape,
    /// Missing (or non-final) terminator.
    Terminator,
    /// Unescaped body is not valid UTF-8.
    Utf8,
}

/// Decode `bytes` as a `key_type` key — **debug/EXPLAIN only**; the hot
/// path never decodes. Canonical-strict: accepts exactly the byte
/// strings [`index_key_encode`] can produce, which gives the fuzz
/// target its two laws: `decode(encode(v)) == v` and
/// `decode(b) is Ok ⇒ encode(decode(b)) == b`.
pub fn index_key_decode(
    key_type: IndexKeyType,
    bytes: &[u8],
) -> Result<DecodedIndexKey, IndexKeyDecodeError> {
    if key_type.fixed8() {
        let word_bytes: [u8; 8] = bytes.try_into().map_err(|_| IndexKeyDecodeError::Length)?;
        let word = u64::from_be_bytes(word_bytes);
        return match key_type {
            IndexKeyType::I64 => Ok(DecodedIndexKey::I64((word ^ SIGN_BIT) as i64)),
            IndexKeyType::F64 => decode_f64_word(word),
            IndexKeyType::Bool => match word {
                0 => Ok(DecodedIndexKey::Bool(false)),
                1 => Ok(DecodedIndexKey::Bool(true)),
                _ => Err(IndexKeyDecodeError::NotABool),
            },
            IndexKeyType::Utf8 => unreachable!("utf8 is not fixed8"),
        };
    }
    decode_utf8_key(bytes)
}

/// Invert the total-order sign-flip; reject what encode cannot emit.
fn decode_f64_word(word: u64) -> Result<DecodedIndexKey, IndexKeyDecodeError> {
    // Encode maps positives to sign-set words and negatives to
    // sign-clear words — invert accordingly.
    let bits = if word & SIGN_BIT != 0 { word & !SIGN_BIT } else { !word };
    let f = f64::from_bits(bits);
    if f.is_nan() {
        return Err(IndexKeyDecodeError::NotCanonicalF64);
    }
    // -0.0 canonicalizes to +0.0 before encode, so its pattern (the
    // all-ones word minus sign) is unreachable — reject, keeping
    // decode∘encode the identity on the byte side.
    if f == 0.0 && bits & SIGN_BIT != 0 {
        return Err(IndexKeyDecodeError::NotCanonicalF64);
    }
    Ok(DecodedIndexKey::F64(f))
}

/// Iterative unescape with explicit bounds (a decoder in the L9 sense).
fn decode_utf8_key(bytes: &[u8]) -> Result<DecodedIndexKey, IndexKeyDecodeError> {
    if bytes.is_empty() || bytes.len() > ORDERED_KEY_MAX {
        return Err(IndexKeyDecodeError::Length);
    }
    if *bytes.last().expect("nonempty checked above") != 0x00 {
        return Err(IndexKeyDecodeError::Terminator);
    }
    let body = &bytes[..bytes.len() - 1];
    let mut raw = Vec::with_capacity(body.len());
    let mut at: usize = 0;
    while at < body.len() {
        let b = body[at];
        if b == 0x00 {
            // Inside the body a NUL is always the 2-byte escape; a lone
            // 0x00 here would be an early terminator.
            if at + 1 >= body.len() || body[at + 1] != 0xFF {
                return Err(IndexKeyDecodeError::Escape);
            }
            raw.push(0x00);
            at += 2;
        } else {
            raw.push(b);
            at += 1;
        }
    }
    let s = String::from_utf8(raw).map_err(|_| IndexKeyDecodeError::Utf8)?;
    debug_assert!(utf8_encoded_len(&s) == bytes.len(), "decode/encode length agree");
    Ok(DecodedIndexKey::Utf8(s))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// Encode via the public path; panic on skip only where the test
    /// guarantees admission.
    fn key_bytes(key_type: IndexKeyType, value: IndexScalar<'_>) -> Vec<u8> {
        let mut buf = IndexKeyBuf::new();
        index_key_encode(key_type, value, &mut buf).expect("test value admits");
        buf.as_bytes().to_vec()
    }

    fn encode_outcome(key_type: IndexKeyType, value: IndexScalar<'_>) -> Result<Vec<u8>, KeySkip> {
        let mut buf = IndexKeyBuf::new();
        index_key_encode(key_type, value, &mut buf).map(|()| buf.as_bytes().to_vec())
    }

    /// Typed f64 order for the property tests: std `partial_cmp`, total
    /// on the non-NaN domain the encoder admits (-0.0 == +0.0 included).
    fn f64_order(a: f64, b: f64) -> Ordering {
        a.partial_cmp(&b).expect("non-NaN by construction")
    }

    #[test]
    fn golden_vectors_i64() {
        // The offset-binary corners: byte order must walk MIN → MAX.
        let rows: [(i64, [u8; 8]); 5] = [
            (i64::MIN, [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]),
            (-1, [0x7F, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]),
            (0, [0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]),
            (1, [0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01]),
            (i64::MAX, [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]),
        ];
        for (value, want) in rows {
            assert_eq!(key_bytes(IndexKeyType::I64, IndexScalar::I64(value)), want, "{value}");
            assert_eq!(index_key_decode(IndexKeyType::I64, &want), Ok(DecodedIndexKey::I64(value)));
        }
    }

    #[test]
    fn golden_vectors_f64() {
        // Sign-flip corners, including the -0.0 ≡ +0.0 canonicalization.
        let rows: [(f64, [u8; 8]); 6] = [
            (f64::NEG_INFINITY, [0x00, 0x0F, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]),
            (-1.5, [0x40, 0x07, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]),
            (-0.0, [0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]),
            (0.0, [0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]),
            (1.5, [0xBF, 0xF8, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]),
            (f64::INFINITY, [0xFF, 0xF0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]),
        ];
        for (value, want) in rows {
            assert_eq!(key_bytes(IndexKeyType::F64, IndexScalar::F64(value)), want, "{value}");
        }
        // Decode returns the canonical value: -0.0's bytes decode +0.0.
        assert_eq!(index_key_decode(IndexKeyType::F64, &rows[2].1), Ok(DecodedIndexKey::F64(0.0)));
    }

    #[test]
    fn golden_vectors_bool_and_utf8() {
        assert_eq!(
            key_bytes(IndexKeyType::Bool, IndexScalar::Bool(false)),
            [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
        );
        assert_eq!(
            key_bytes(IndexKeyType::Bool, IndexScalar::Bool(true)),
            [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01]
        );
        let rows: [(&str, &[u8]); 4] = [
            ("", &[0x00]),
            ("a", &[0x61, 0x00]),
            ("a\0b", &[0x61, 0x00, 0xFF, 0x62, 0x00]),
            ("é", &[0xC3, 0xA9, 0x00]),
        ];
        for (value, want) in rows {
            assert_eq!(key_bytes(IndexKeyType::Utf8, IndexScalar::Utf8(value)), want, "{value:?}");
            assert_eq!(
                index_key_decode(IndexKeyType::Utf8, want),
                Ok(DecodedIndexKey::Utf8(value.to_string()))
            );
        }
    }

    #[test]
    fn truth_table_corners() {
        // NaN: typed, counted, never a panic (ADR-0074 D4.4).
        assert_eq!(
            encode_outcome(IndexKeyType::F64, IndexScalar::F64(f64::NAN)),
            Err(KeySkip::NotANumber)
        );
        assert_eq!(
            encode_outcome(IndexKeyType::I64, IndexScalar::F64(f64::NAN)),
            Err(KeySkip::NotANumber)
        );
        // The §3.1 user-facing pair: 10 and 10.5 both land in f64.
        assert!(encode_outcome(IndexKeyType::F64, IndexScalar::I64(10)).is_ok());
        assert!(encode_outcome(IndexKeyType::F64, IndexScalar::F64(10.5)).is_ok());
        // ...and 10 / 10.0 collide byte-identically (both index types).
        assert_eq!(
            key_bytes(IndexKeyType::F64, IndexScalar::I64(10)),
            key_bytes(IndexKeyType::F64, IndexScalar::F64(10.0)),
        );
        assert_eq!(
            key_bytes(IndexKeyType::I64, IndexScalar::F64(10.0)),
            key_bytes(IndexKeyType::I64, IndexScalar::I64(10)),
        );
        // Fractional into i64 is Inexact, not Sparse: it is numeric.
        assert_eq!(
            encode_outcome(IndexKeyType::I64, IndexScalar::F64(10.5)),
            Err(KeySkip::Inexact)
        );
        // Null is absent everywhere; type mismatches are Sparse.
        for key_type in
            [IndexKeyType::Utf8, IndexKeyType::I64, IndexKeyType::F64, IndexKeyType::Bool]
        {
            assert_eq!(
                encode_outcome(key_type, IndexScalar::Null),
                Err(KeySkip::Sparse),
                "{key_type:?}"
            );
        }
        assert_eq!(
            encode_outcome(IndexKeyType::I64, IndexScalar::Utf8("10")),
            Err(KeySkip::Sparse)
        );
        assert_eq!(
            encode_outcome(IndexKeyType::Utf8, IndexScalar::Bool(true)),
            Err(KeySkip::Sparse)
        );
    }

    #[test]
    fn boundary_pins_2_pow_53_and_2_pow_63() {
        const P53: i64 = 1 << 53;
        // The contiguous band edge: 2^53-1, 2^53 exact; 2^53+1 is the
        // first integer f64 cannot hold; 2^53+2 is exact again (even) —
        // the pin that shows the rule is losslessness, not the band.
        assert!(encode_outcome(IndexKeyType::F64, IndexScalar::I64(P53 - 1)).is_ok());
        assert!(encode_outcome(IndexKeyType::F64, IndexScalar::I64(P53)).is_ok());
        assert_eq!(
            encode_outcome(IndexKeyType::F64, IndexScalar::I64(P53 + 1)),
            Err(KeySkip::Inexact)
        );
        assert!(encode_outcome(IndexKeyType::F64, IndexScalar::I64(P53 + 2)).is_ok());
        assert!(encode_outcome(IndexKeyType::F64, IndexScalar::I64(1 << 60)).is_ok());
        // 2^63 corners: MIN is exact; MAX rounds to 2^63 and must NOT
        // survive the saturating round-trip (ADR-0074 D4.1).
        assert!(encode_outcome(IndexKeyType::F64, IndexScalar::I64(i64::MIN)).is_ok());
        assert_eq!(
            encode_outcome(IndexKeyType::F64, IndexScalar::I64(i64::MAX)),
            Err(KeySkip::Inexact)
        );
        // f64 → i64 at the top: 2^63 is out of range; the next f64 down
        // (2^63 - 1024) is integral and in range — admits exactly.
        assert_eq!(
            encode_outcome(IndexKeyType::I64, IndexScalar::F64(TWO_POW_63)),
            Err(KeySkip::Inexact)
        );
        let below = 9_223_372_036_854_774_784.0_f64;
        assert_eq!(
            index_scalar_coerce(IndexKeyType::I64, IndexScalar::F64(below)).map(|v| match v {
                IndexScalar::I64(v) => v,
                _ => unreachable!("i64 coercion yields i64"),
            }),
            Ok(9_223_372_036_854_774_784)
        );
        assert_eq!(
            index_scalar_coerce(IndexKeyType::I64, IndexScalar::F64(-TWO_POW_63)).map(
                |v| match v {
                    IndexScalar::I64(v) => v,
                    _ => unreachable!("i64 coercion yields i64"),
                }
            ),
            Ok(i64::MIN)
        );
        // ±∞: admitted by f64 indexes, Inexact for i64 indexes.
        assert!(encode_outcome(IndexKeyType::F64, IndexScalar::F64(f64::INFINITY)).is_ok());
        assert_eq!(
            encode_outcome(IndexKeyType::I64, IndexScalar::F64(f64::INFINITY)),
            Err(KeySkip::Inexact)
        );
        // -0.0 into i64 admits as 0 (consistent with -0.0 == 0.0).
        assert_eq!(
            key_bytes(IndexKeyType::I64, IndexScalar::F64(-0.0)),
            key_bytes(IndexKeyType::I64, IndexScalar::I64(0)),
        );
    }

    #[test]
    fn too_long_rule_is_on_encoded_length() {
        // 511 raw NULs encode to 2·511+1 = 1023 ≤ 1024: admitted.
        let nuls_511 = "\0".repeat(511);
        assert!(encode_outcome(IndexKeyType::Utf8, IndexScalar::Utf8(&nuls_511)).is_ok());
        // 512 raw NULs encode to 1025: TooLong, counted (ADR-0074 D3).
        let nuls_512 = "\0".repeat(512);
        assert_eq!(
            encode_outcome(IndexKeyType::Utf8, IndexScalar::Utf8(&nuls_512)),
            Err(KeySkip::TooLong)
        );
        // 1023 plain bytes encode to exactly 1024: the fit boundary.
        let plain_1023 = "x".repeat(1023);
        assert!(encode_outcome(IndexKeyType::Utf8, IndexScalar::Utf8(&plain_1023)).is_ok());
        let plain_1024 = "x".repeat(1024);
        assert_eq!(
            encode_outcome(IndexKeyType::Utf8, IndexScalar::Utf8(&plain_1024)),
            Err(KeySkip::TooLong)
        );
    }

    #[test]
    fn decode_rejects_non_canonical_input() {
        // f64 patterns encode cannot produce: NaN and -0.0.
        let neg_zero_pattern = (!(-0.0_f64).to_bits()).to_be_bytes();
        assert_eq!(
            index_key_decode(IndexKeyType::F64, &neg_zero_pattern),
            Err(IndexKeyDecodeError::NotCanonicalF64)
        );
        let nan_pattern = (f64::NAN.to_bits() | SIGN_BIT).to_be_bytes();
        assert_eq!(
            index_key_decode(IndexKeyType::F64, &nan_pattern),
            Err(IndexKeyDecodeError::NotCanonicalF64)
        );
        assert_eq!(
            index_key_decode(IndexKeyType::Bool, &2u64.to_be_bytes()),
            Err(IndexKeyDecodeError::NotABool)
        );
        // String framing damage: empty, unterminated, early terminator,
        // bad escape, trailing garbage after an escape at the end.
        assert_eq!(index_key_decode(IndexKeyType::Utf8, &[]), Err(IndexKeyDecodeError::Length));
        assert_eq!(
            index_key_decode(IndexKeyType::Utf8, &[0x61]),
            Err(IndexKeyDecodeError::Terminator)
        );
        assert_eq!(
            index_key_decode(IndexKeyType::Utf8, &[0x61, 0x00, 0x62, 0x00]),
            Err(IndexKeyDecodeError::Escape)
        );
        assert_eq!(
            index_key_decode(IndexKeyType::Utf8, &[0x00, 0xFF, 0xFF, 0x00, 0x00]),
            Err(IndexKeyDecodeError::Escape)
        );
        // Invalid UTF-8 after unescape.
        assert_eq!(
            index_key_decode(IndexKeyType::Utf8, &[0xFF, 0x00]),
            Err(IndexKeyDecodeError::Utf8)
        );
        // Wrong lengths for fixed8 types.
        assert_eq!(
            index_key_decode(IndexKeyType::I64, &[0x00; 7]),
            Err(IndexKeyDecodeError::Length)
        );
        assert_eq!(
            index_key_decode(IndexKeyType::F64, &[0x00; 9]),
            Err(IndexKeyDecodeError::Length)
        );
    }

    proptest! {
        /// Byte order ≡ typed order, per type (the order-preservation
        /// property; the 10⁶ storm in tests/ scales this shape up).
        #[test]
        fn order_i64(a: i64, b: i64) {
            let ka = key_bytes(IndexKeyType::I64, IndexScalar::I64(a));
            let kb = key_bytes(IndexKeyType::I64, IndexScalar::I64(b));
            prop_assert_eq!(ka.cmp(&kb), a.cmp(&b));
        }

        #[test]
        fn order_f64(a: f64, b: f64) {
            prop_assume!(!a.is_nan() && !b.is_nan());
            let ka = key_bytes(IndexKeyType::F64, IndexScalar::F64(a));
            let kb = key_bytes(IndexKeyType::F64, IndexScalar::F64(b));
            prop_assert_eq!(ka.cmp(&kb), f64_order(a, b));
        }

        #[test]
        fn order_utf8(a: String, b: String) {
            let ka = key_bytes(IndexKeyType::Utf8, IndexScalar::Utf8(&a));
            let kb = key_bytes(IndexKeyType::Utf8, IndexScalar::Utf8(&b));
            prop_assert_eq!(ka.cmp(&kb), a.as_bytes().cmp(b.as_bytes()));
        }

        /// Round-trip: decode ∘ encode is the identity (both string
        /// paths — escaped and fast).
        #[test]
        fn round_trip_utf8(s in "[\\x00-\\x7Fé☃]{0,64}") {
            let enc = key_bytes(IndexKeyType::Utf8, IndexScalar::Utf8(&s));
            prop_assert_eq!(
                index_key_decode(IndexKeyType::Utf8, &enc),
                Ok(DecodedIndexKey::Utf8(s))
            );
        }

        /// Prefix safety (ADR-0074 D2, the S09 begins_with contract):
        /// `p` prefix of `s` ⟺ `enc(s)` starts with `escape(p)`
        /// (= enc(p) minus its final terminator byte). Both directions.
        #[test]
        fn utf8_escape_is_prefix_safe(s in "[\\x00-\\x7F]{0,32}", t in "[\\x00-\\x7F]{0,32}") {
            let enc_s = key_bytes(IndexKeyType::Utf8, IndexScalar::Utf8(&s));
            for (candidate, is_prefix) in [
                // Every true prefix of s (char-boundary splits).
                (s.clone(), true),
                (s.chars().take(1).collect::<String>(), true),
                // t, which is a prefix of s only if the bytes say so.
                (t.clone(), s.as_bytes().starts_with(t.as_bytes())),
            ] {
                let enc_p = key_bytes(IndexKeyType::Utf8, IndexScalar::Utf8(&candidate));
                let escape_p = &enc_p[..enc_p.len() - 1];
                prop_assert_eq!(
                    enc_s.starts_with(escape_p),
                    is_prefix,
                    "s={:?} p={:?}", s, candidate
                );
            }
        }

        /// The S09 bound primitive (ADR-0080 D3): within the cap the
        /// image IS enc(s) minus its terminator; the returned length is
        /// the untruncated escaped length either way.
        #[test]
        fn escape_prefix_matches_encoding(s in "[\\x00-\\x7Fé]{0,64}") {
            let mut image = IndexKeyBuf::new();
            let full = index_key_escape_prefix(&s, &mut image);
            let enc = key_bytes(IndexKeyType::Utf8, IndexScalar::Utf8(&s));
            prop_assert_eq!(image.as_bytes(), &enc[..enc.len() - 1]);
            prop_assert_eq!(full, enc.len() - 1);
        }

        /// The consistency law (ADR-0074 D5): wherever two numerics
        /// both admit into one index type, byte order ≡ the VM's
        /// comparison verdict.
        #[test]
        fn coercion_agreement(a: i64, b: f64) {
            prop_assume!(!b.is_nan());
            for key_type in [IndexKeyType::I64, IndexKeyType::F64] {
                let ka = encode_outcome(key_type, IndexScalar::I64(a));
                let kb = encode_outcome(key_type, IndexScalar::F64(b));
                if let (Ok(ka), Ok(kb)) = (ka, kb) {
                    prop_assert_eq!(ka.cmp(&kb), compare_i64_f64(a, b), "{:?}", key_type);
                }
            }
        }

        /// Over-cap strings truncate the image at the tree's key cap —
        /// the truncated-image bound rule (ADR-0080 D3) stands on the
        /// image being exactly `enc(s)[..ORDERED_KEY_MAX]`.
        #[test]
        fn escape_prefix_truncates_at_the_cap(head in "[\\x00-\\x7F]{0,8}") {
            let long = format!("{head}{}", "x".repeat(ORDERED_KEY_MAX + 8));
            let mut image = IndexKeyBuf::new();
            let full = index_key_escape_prefix(&long, &mut image);
            let nuls = head.as_bytes().iter().filter(|&&b| b == 0).count();
            prop_assert_eq!(full, long.len() + nuls);
            prop_assert_eq!(image.as_bytes().len(), ORDERED_KEY_MAX);
            let mut expected = Vec::new();
            for &b in long.as_bytes() {
                expected.push(b);
                if b == 0 {
                    expected.push(0xFF);
                }
            }
            prop_assert_eq!(image.as_bytes(), &expected[..ORDERED_KEY_MAX]);
        }

        /// compare_i64_f64 agrees with an independent exact reference,
        /// built from a different decomposition than the implementation
        /// (float compare where `a` is exact; scaled i128 arithmetic
        /// where `b*4` is integral; magnitude classification otherwise).
        #[test]
        fn compare_is_exact(a: i64, b: f64) {
            prop_assume!(b.is_finite());
            let reference = if a.unsigned_abs() <= (1u64 << 53) {
                // a as f64 is exact, so IEEE compare IS the real order.
                f64_order(a as f64, b)
            } else if b >= TWO_POW_63 {
                Ordering::Less
            } else if b < -TWO_POW_63 {
                Ordering::Greater
            } else if (b * 4.0).fract() == 0.0 {
                // b is a multiple of 0.25 in [-2^63, 2^63): scaling by
                // 4 is exact (power of two) and 4a/4b both fit i128.
                (i128::from(a) * 4).cmp(&((b * 4.0) as i128))
            } else {
                // b has a sub-0.25 fraction, so ulp(b) < 0.25 and
                // |b| < 2^51 — while |a| > 2^53: sign of a decides.
                if a > 0 { Ordering::Greater } else { Ordering::Less }
            };
            prop_assert_eq!(compare_i64_f64(a, b), reference);
        }
    }
}
