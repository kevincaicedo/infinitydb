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
//!
//! **This module is the ADR-0049 audited unsafe region** (the ADR-0047 D3
//! escalation): every multi-byte primitive follows one contract —
//! `reserve(worst_case)` once, write at most that many bytes through a
//! raw cursor, `set_len` to exactly what was written — replacing the
//! per-byte `Vec` capacity branches and the per-short-string
//! `#[target_feature]` kernel-call boundary that the gate-shape profile
//! priced (see `crates/inf-doc/SAFETY.md` for the block inventory). No
//! intrinsics, no `transmute`, no pointer outlives its call.

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
    out.reserve(I64_MAX_LEN);
    let n = out.len();
    // SAFETY: `reserve(I64_MAX_LEN)` above; `i64_into_raw` writes at most
    // `I64_MAX_LEN` bytes at `n` and returns the exact count exposed.
    unsafe {
        let len = i64_into_raw(out.as_mut_ptr().add(n), v);
        out.set_len(n + len);
    }
}

/// Canonical i64 bytes through a raw cursor.
///
/// SAFETY contract: `out..out + I64_MAX_LEN` must be writable.
#[inline]
unsafe fn i64_into_raw(out: *mut u8, v: i64) -> usize {
    // SAFETY: caller provides I64_MAX_LEN writable bytes; the fixint arm
    // writes 1, the varint loop at most 1 + 10.
    unsafe {
        if (FIXINT_MIN..=FIXINT_MAX).contains(&v) {
            out.write(v as u8); // two's complement byte IS the fixint tag
            return 1;
        }
        out.write(TAG_I64);
        let mut raw = zigzag(v);
        let mut at = 1;
        while raw >= 0x80 {
            out.add(at).write((raw as u8) | 0x80);
            raw >>= 7;
            at += 1;
        }
        out.add(at).write(raw as u8);
        at + 1
    }
}

/// Canonical i64 bytes into caller stack storage (the apply.rs scalar
/// fast path's shape).
#[inline]
pub(crate) fn i64_into(out: &mut [u8; I64_MAX_LEN], v: i64) -> usize {
    // SAFETY: `out` is exactly I64_MAX_LEN writable bytes.
    unsafe { i64_into_raw(out.as_mut_ptr(), v) }
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
    debug_assert!(v.is_finite(), "non-finite f64 refused before emission");
    out.reserve(F64_LEN);
    let n = out.len();
    // SAFETY: `reserve(F64_LEN)` above; exactly tag + 8 bytes written.
    unsafe {
        let p = out.as_mut_ptr().add(n);
        p.write(TAG_F64);
        p.add(1).cast::<[u8; 8]>().write(v.to_bits().to_le_bytes());
        out.set_len(n + F64_LEN);
    }
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

/// Worst-case string-header emission (str24: tag + u24).
const STR_HEADER_MAX_LEN: usize = 4;

/// The canonical string header alone (tag + width-selected length) — the
/// S05 parser follows it with its own overlapped payload copy.
#[inline]
pub(crate) fn str_header(out: &mut Vec<u8>, len: usize) {
    out.reserve(STR_HEADER_MAX_LEN);
    let n = out.len();
    // SAFETY: `reserve(STR_HEADER_MAX_LEN)` above; each arm writes at
    // most 4 bytes and exposes exactly what it wrote.
    unsafe {
        let p = out.as_mut_ptr().add(n);
        if len <= FIXSTR_MAX_LEN {
            p.write(FIXSTR_BASE + len as u8);
            out.set_len(n + 1);
        } else if len < STR24_MIN_LEN {
            p.write(TAG_STR8);
            p.add(1).write(len as u8);
            out.set_len(n + 2);
        } else {
            // Tag + u24 as one 4-byte store: `len < 2²⁴` (the format
            // ceiling), so byte 3 of `len << 8` is its u24 high byte.
            p.cast::<[u8; 4]>().write((((len as u32) << 8) | u32::from(TAG_STR24)).to_le_bytes());
            out.set_len(n + 4);
        }
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
    out.reserve(CONTAINER_OPEN_LEN);
    let n = out.len();
    // SAFETY: `reserve(CONTAINER_OPEN_LEN)` above; one 4-byte store
    // (tag + zeroed placeholder), exactly 4 bytes exposed.
    unsafe {
        out.as_mut_ptr().add(n).cast::<[u8; 4]>().write(u32::from(tag).to_le_bytes());
        out.set_len(n + CONTAINER_OPEN_LEN);
    }
    n + 1
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

/// Append `input[start..start + len]` through the raw cursor in 8-byte
/// words, riding the source's slack: words may overshoot `len` inside the
/// reservation (the final `set_len` repairs it) but never read past
/// `input`. Exact-copy fallback when the source runs out of slack. For
/// the mid-size strings that ride this path it is a couple of inlined
/// loads/stores instead of a memcpy call plus per-word `Vec` branches.
#[inline]
pub(crate) fn append_overlapped(out: &mut Vec<u8>, input: &[u8], start: usize, len: usize) {
    let end = start + len;
    // Round up to whole words: overshooting stores stay in spare capacity.
    out.reserve(len + 7);
    let base = out.len();
    let mut n = start;
    // SAFETY: `reserve(len + 7)` above bounds every 8-byte store below
    // (`n - start < len` on entry to each iteration); `set_len` exposes
    // exactly `len` bytes, all initialized (full words below, remainder
    // via the safe `extend_from_slice` arm which manages its own length).
    unsafe {
        let p = out.as_mut_ptr().add(base);
        while n < end && n + 8 <= input.len() {
            let w: [u8; 8] = input[n..n + 8].try_into().expect("8-byte chunk");
            p.add(n - start).cast::<[u8; 8]>().write(w);
            n += 8;
        }
        if n >= end {
            out.set_len(base + len);
            return;
        }
        // Source slack exhausted (a string at the very end of the input):
        // expose the words written so far, then grow safely.
        out.set_len(base + (n - start));
    }
    out.extend_from_slice(&input[n..end]);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The raw-cursor primitives against their obvious safe oracles —
    /// byte-for-byte, across pre-existing content and every width arm.
    /// (The goldens + differential pin the full-format behavior; these
    /// pin the region's unsafe blocks in isolation, and run under Miri.)
    #[test]
    fn primitives_match_safe_oracles() {
        // i64: fixint, varint widths, extremes.
        for v in [0i64, 1, -1, FIXINT_MIN, FIXINT_MAX, 128, -129, i64::MAX, i64::MIN] {
            let mut out = b"pre".to_vec();
            i64(&mut out, v);
            let mut oracle = [0u8; I64_MAX_LEN];
            let n = i64_into(&mut oracle, v);
            assert_eq!(&out[3..], &oracle[..n], "i64 {v}");
            assert_eq!(i64_len(v), n, "i64_len {v}");
        }
        // f64.
        let mut out = b"pre".to_vec();
        f64(&mut out, 1.5);
        let mut oracle = [0u8; F64_LEN];
        f64_into(&mut oracle, 1.5);
        assert_eq!(&out[3..], &oracle);
        // str_header: every width arm boundary.
        for len in [0usize, 1, 31, 32, 255, 256, 1 << 20] {
            let mut out = b"pre".to_vec();
            str_header(&mut out, len);
            let expect: Vec<u8> = if len <= FIXSTR_MAX_LEN {
                vec![FIXSTR_BASE + len as u8]
            } else if len < STR24_MIN_LEN {
                vec![TAG_STR8, len as u8]
            } else {
                let b = (len as u32).to_le_bytes();
                vec![TAG_STR24, b[0], b[1], b[2]]
            };
            assert_eq!(&out[3..], &expect, "str_header {len}");
            assert_eq!(str_header_len(len), expect.len());
        }
        // begin: tag + zeroed placeholder, offset points at the u24.
        let mut out = b"pre".to_vec();
        let len_at = begin(&mut out, 0xC1);
        assert_eq!(&out[3..], &[0xC1, 0, 0, 0]);
        assert_eq!(len_at, 4);
    }

    #[test]
    fn append_overlapped_matches_extend() {
        let input: Vec<u8> = (0..100u8).collect();
        for start in [0usize, 1, 50, 90, 92, 99] {
            for len in [0usize, 1, 7, 8, 9, 10] {
                if start + len > input.len() {
                    continue;
                }
                let mut out = b"pre".to_vec();
                append_overlapped(&mut out, &input, start, len);
                assert_eq!(&out[..3], b"pre");
                assert_eq!(&out[3..], &input[start..start + len], "start {start} len {len}");
            }
        }
    }
}
