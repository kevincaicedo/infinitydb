//! JSON stage-1 structural scan (M3-S05, simdjson technique): classify a
//! byte stream into 64-byte block masks (quote / backslash / structural /
//! whitespace), resolve escapes and string spans with branchless bit
//! arithmetic, and emit the **structural index** — the byte offsets of
//! every token start the stage-2 tape builder needs:
//!
//! - every unescaped `"` (string opens *and* closes pair up in the index),
//! - every `{ } [ ] : ,` outside strings,
//! - every scalar start (first byte of a number/`true`/`false`/`null` run)
//!   outside strings.
//!
//! Escape resolution is deliberately context-free (a char after an odd
//! backslash run is "escaped" even outside a string, exactly like
//! simdjson) — a stray `\` outside a string is a grammar error stage 2
//! reports; what matters here is that the scalar oracle and every SIMD
//! tier emit **identical** indices for arbitrary bytes (the equivalence
//! proptest is the proof).
//!
//! Tiers: AVX2 (32-byte compares) → SSE2 baseline (16-byte) on x86-64,
//! NEON on aarch64 (the `vshrn` nibble-movemask idiom, as in `crlf.rs`),
//! and the fully safe per-byte state machine as the portability tier and
//! correctness oracle. Runtime dispatch follows the cached-`AtomicU8`
//! pattern from `crlf.rs`.

#[cfg(target_arch = "x86_64")]
use core::arch::x86_64::{
    __m128i, __m256i, _mm_cmpeq_epi8, _mm_loadu_si128, _mm_movemask_epi8, _mm_or_si128,
    _mm_set1_epi8, _mm256_cmpeq_epi8, _mm256_loadu_si256, _mm256_movemask_epi8, _mm256_or_si256,
    _mm256_set1_epi8, _mm256_storeu_si256,
};
#[cfg(target_arch = "x86_64")]
use std::sync::atomic::{AtomicU8, Ordering};

#[cfg(target_arch = "x86_64")]
const SIMD_LEVEL_UNKNOWN: u8 = 0;
#[cfg(target_arch = "x86_64")]
const SIMD_LEVEL_AVX2: u8 = 1;
#[cfg(target_arch = "x86_64")]
const SIMD_LEVEL_SSE2: u8 = 2;

const BLOCK: usize = 64;

/// One 64-byte block classified into bitmasks (bit i = byte i).
#[derive(Copy, Clone, Default)]
struct BlockMasks {
    backslash: u64,
    quote_raw: u64,
    /// `{ } [ ] : ,`
    op: u64,
    /// ` ` `\t` `\n` `\r`
    ws: u64,
}

/// Carries across blocks.
#[derive(Copy, Clone, Default)]
struct ScanState {
    /// Bit 0: the next block's byte 0 is escaped by a trailing odd run.
    prev_escaped: u64,
    /// All-ones while inside a string at the block boundary.
    prev_in_string: u64,
    /// Bit 0: the previous block's last byte was a non-quote scalar.
    prev_nonquote_scalar: u64,
}

/// Scan `input` and write the structural index into `out[..n]`, returning
/// `n`. `out` is treated as reusable scratch: it grows on demand and is
/// **not** truncated — entries at `n..` are stale garbage from earlier
/// scans. (Exact-length behavior would force a truncate/regrow memset
/// cycle every call; returning the count keeps the buffer at its
/// high-water mark so steady-state scans allocate and zero nothing.)
/// Dispatches to the fastest tier available on this CPU.
#[inline]
pub fn json_scan_structurals(input: &[u8], out: &mut Vec<u32>) -> usize {
    out.clear();
    out.reserve(input.len() / 4 + 8);

    #[cfg(target_arch = "x86_64")]
    {
        x86_scan(input, out)
    }

    #[cfg(target_arch = "aarch64")]
    {
        neon_scan(input, out)
    }

    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        scalar_scan_into(input, out);
        out.len()
    }
}

/// Scalar per-byte oracle (and the portability tier): an independent
/// state machine — not the bit tricks — so the equivalence proptest
/// actually verifies the SIMD arithmetic. Same contract as
/// [`json_scan_structurals`]: the index lands in `out[..n]`.
pub fn scalar_json_scan_structurals(input: &[u8], out: &mut Vec<u32>) -> usize {
    out.clear();
    scalar_scan_into(input, out);
    out.len()
}

fn scalar_scan_into(input: &[u8], out: &mut Vec<u32>) {
    let mut escaped_next = false;
    let mut parity_in_string = false;
    let mut prev_nonquote_scalar = false;
    for (i, &b) in input.iter().enumerate() {
        let is_escaped = escaped_next;
        escaped_next = !is_escaped && b == b'\\';
        let quote = b == b'"' && !is_escaped;
        if quote {
            parity_in_string = !parity_in_string;
        }
        let in_string = parity_in_string;
        let is_op = matches!(b, b'{' | b'}' | b'[' | b']' | b':' | b',');
        let is_ws = matches!(b, b' ' | b'\t' | b'\n' | b'\r');
        let nonquote_scalar = !is_op && !is_ws && !quote;
        let emit = quote
            || (is_op && !in_string)
            || (nonquote_scalar && !in_string && !prev_nonquote_scalar);
        prev_nonquote_scalar = nonquote_scalar;
        if emit {
            out.push(i as u32);
        }
    }
}

// ---- shared block arithmetic (safe u64 bit tricks) --------------------------

/// Characters preceded by an odd run of backslashes: in `\\\\` bytes 1
/// and 3 are escaped; the carry marks byte 0 of the next block. Blocks
/// without backslashes (the overwhelmingly common case) take the O(1)
/// path; blocks with them run a 64-step bit-serial resolve — correct by
/// construction and still multiple GB/s worst-case. The fully branchless
/// carry-add algorithm (Langdale & Lemire) is a named lever if the
/// stage-1 A/B ever shows escape-heavy documents on the floor.
fn find_escaped(backslash: u64, prev_escaped: &mut u64) -> u64 {
    if backslash == 0 {
        let escaped = *prev_escaped;
        *prev_escaped = 0;
        return escaped;
    }
    let mut escaped = 0u64;
    let mut is_escaped = *prev_escaped != 0;
    for i in 0..64u32 {
        let bit = 1u64 << i;
        if is_escaped {
            escaped |= bit;
            is_escaped = false;
        } else if backslash & bit != 0 {
            is_escaped = true;
        }
    }
    *prev_escaped = u64::from(is_escaped);
    escaped
}

/// Prefix XOR: bit i of the result = XOR of bits 0..=i (the string-span
/// mask from quote bits — a carry-less multiply by ~0, as a shift ladder).
#[inline]
fn prefix_xor(x: u64) -> u64 {
    let mut x = x;
    x ^= x << 1;
    x ^= x << 2;
    x ^= x << 4;
    x ^= x << 8;
    x ^= x << 16;
    x ^= x << 32;
    x
}

/// Finish one classified block: resolve escapes and string spans, then
/// push the emitted indices. (The simdjson slot-style flatten — eight
/// unconditional writes per round through a high-water buffer — was
/// A/B'd in the S05 slice 2 and **lost on the budget shapes**: −1.5%
/// gate / −2.3% medium against +4–6% on small/large; the push loop's
/// branch predicts well at real structural densities. Recorded, not
/// merged — the M0-S14 rule.)
fn flush_block(base: usize, masks: BlockMasks, state: &mut ScanState, out: &mut Vec<u32>) {
    let escaped = find_escaped(masks.backslash, &mut state.prev_escaped);
    let quote = masks.quote_raw & !escaped;
    let in_string = prefix_xor(quote) ^ state.prev_in_string;
    state.prev_in_string = 0u64.wrapping_sub(in_string >> 63);
    let scalar = !(masks.op | masks.ws);
    // An ESCAPED quote byte counts as scalar content (mirrors the oracle:
    // `quote` means unescaped quote only).
    let nonquote_scalar = scalar & !quote;
    let follows_nonquote_scalar = (nonquote_scalar << 1) | state.prev_nonquote_scalar;
    state.prev_nonquote_scalar = nonquote_scalar >> 63;
    let scalar_start = nonquote_scalar & !follows_nonquote_scalar & !in_string;
    let mut emit = (masks.op & !in_string) | quote | scalar_start;
    // One reserve covers the block's worst case (64 indices), so the loop
    // writes unchecked — removing a `Vec` growth branch per index while
    // keeping the well-predicted bit-loop shape (ADR-0047 K3; distinct
    // from the Rejected slot-flatten above, which replaced the loop).
    out.reserve(64);
    let mut n = out.len();
    // SAFETY: `reserve(64)` guarantees capacity for the at-most-64 set
    // bits of `emit`; `set_len(n)` exposes exactly the written prefix.
    #[allow(unsafe_code)]
    unsafe {
        let dst = out.as_mut_ptr();
        while emit != 0 {
            *dst.add(n) = (base + emit.trailing_zeros() as usize) as u32;
            n += 1;
            emit &= emit - 1;
        }
        out.set_len(n);
    }
}

/// Drive the block pipeline with a per-block mask classifier, padding the
/// tail with spaces (whitespace emits nothing, so padding is inert). The
/// tail goes through the same classifier as full blocks. Returns the
/// index count (`out[..n]` semantics).
fn scan_blocks(input: &[u8], out: &mut Vec<u32>, classify: impl Fn(&[u8]) -> BlockMasks) -> usize {
    let mut state = ScanState::default();
    let mut offset = 0;
    while offset + BLOCK <= input.len() {
        let masks = classify(&input[offset..offset + BLOCK]);
        flush_block(offset, masks, &mut state, out);
        offset += BLOCK;
    }
    let tail = input.len() - offset;
    if tail > 0 {
        let mut padded = [b' '; BLOCK];
        padded[..tail].copy_from_slice(&input[offset..]);
        flush_block(offset, classify(&padded), &mut state, out);
        // Padding is whitespace: it can emit nothing and cannot extend a
        // string or escape, so no index ≥ input.len() is produced.
        debug_assert!(out.last().is_none_or(|&i| (i as usize) < input.len()));
    }
    out.len()
}

// ---- x86-64 tiers -----------------------------------------------------------

#[cfg(target_arch = "x86_64")]
#[inline]
fn x86_scan(input: &[u8], out: &mut Vec<u32>) -> usize {
    static SIMD_LEVEL: AtomicU8 = AtomicU8::new(SIMD_LEVEL_UNKNOWN);

    let mut level = SIMD_LEVEL.load(Ordering::Relaxed);
    if level == SIMD_LEVEL_UNKNOWN {
        level = if std::arch::is_x86_feature_detected!("avx2") {
            SIMD_LEVEL_AVX2
        } else {
            SIMD_LEVEL_SSE2
        };
        SIMD_LEVEL.store(level, Ordering::Relaxed);
    }

    if level == SIMD_LEVEL_AVX2 {
        // SAFETY: runtime dispatch above guarantees AVX2 before calling.
        unsafe { avx2_scan(input, out) }
    } else {
        sse2_scan(input, out)
    }
}

/// AVX2 block classifier: two 32-byte halves per 64-byte block; 10
/// compares per half (`|0x20` folds `[`→`{`, `]`→`}` so the six
/// structural chars need four compares).
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn avx2_scan(input: &[u8], out: &mut Vec<u32>) -> usize {
    #[target_feature(enable = "avx2")]
    #[inline]
    unsafe fn half(ptr: *const u8) -> (u32, u32, u32, u32) {
        // SAFETY: the caller passes `ptr..ptr+32` inside the input slice
        // (the `offset + BLOCK <= len` loop bound); unaligned loads are
        // what `_mm256_loadu_si256` is for.
        let v = unsafe { _mm256_loadu_si256(ptr.cast::<__m256i>()) };
        let bs = _mm256_movemask_epi8(_mm256_cmpeq_epi8(v, _mm256_set1_epi8(b'\\' as i8))) as u32;
        let quote = _mm256_movemask_epi8(_mm256_cmpeq_epi8(v, _mm256_set1_epi8(b'"' as i8))) as u32;
        // Fold case: `[` (0x5B) | 0x20 = `{` (0x7B), `]` | 0x20 = `}`.
        let folded = _mm256_or_si256(v, _mm256_set1_epi8(0x20));
        let op = _mm256_movemask_epi8(_mm256_or_si256(
            _mm256_or_si256(
                _mm256_cmpeq_epi8(folded, _mm256_set1_epi8(b'{' as i8)),
                _mm256_cmpeq_epi8(folded, _mm256_set1_epi8(b'}' as i8)),
            ),
            _mm256_or_si256(
                _mm256_cmpeq_epi8(v, _mm256_set1_epi8(b':' as i8)),
                _mm256_cmpeq_epi8(v, _mm256_set1_epi8(b',' as i8)),
            ),
        )) as u32;
        let ws = _mm256_movemask_epi8(_mm256_or_si256(
            _mm256_or_si256(
                _mm256_cmpeq_epi8(v, _mm256_set1_epi8(b' ' as i8)),
                _mm256_cmpeq_epi8(v, _mm256_set1_epi8(b'\t' as i8)),
            ),
            _mm256_or_si256(
                _mm256_cmpeq_epi8(v, _mm256_set1_epi8(b'\n' as i8)),
                _mm256_cmpeq_epi8(v, _mm256_set1_epi8(b'\r' as i8)),
            ),
        )) as u32;
        (bs, quote, op, ws)
    }

    let mut state = ScanState::default();
    let mut offset = 0;
    let ptr = input.as_ptr();
    while offset + BLOCK <= input.len() {
        // SAFETY: `offset + 64 <= len`, so both 32-byte unaligned loads
        // stay inside the slice.
        let (bs0, q0, op0, ws0) = unsafe { half(ptr.add(offset)) };
        // SAFETY: as above — the second half ends exactly at offset + 64.
        let (bs1, q1, op1, ws1) = unsafe { half(ptr.add(offset + 32)) };
        let masks = BlockMasks {
            backslash: (bs0 as u64) | ((bs1 as u64) << 32),
            quote_raw: (q0 as u64) | ((q1 as u64) << 32),
            op: (op0 as u64) | ((op1 as u64) << 32),
            ws: (ws0 as u64) | ((ws1 as u64) << 32),
        };
        flush_block(offset, masks, &mut state, out);
        offset += BLOCK;
    }
    let tail = input.len() - offset;
    if tail > 0 {
        let mut padded = [b' '; BLOCK];
        padded[..tail].copy_from_slice(&input[offset..]);
        // The padded tail rides the same AVX2 classifier as full blocks.
        // SAFETY: `padded` is a 64-byte stack array; both halves in bounds.
        let (bs0, q0, op0, ws0) = unsafe { half(padded.as_ptr()) };
        // SAFETY: as above.
        let (bs1, q1, op1, ws1) = unsafe { half(padded.as_ptr().add(32)) };
        let masks = BlockMasks {
            backslash: (bs0 as u64) | ((bs1 as u64) << 32),
            quote_raw: (q0 as u64) | ((q1 as u64) << 32),
            op: (op0 as u64) | ((op1 as u64) << 32),
            ws: (ws0 as u64) | ((ws1 as u64) << 32),
        };
        flush_block(offset, masks, &mut state, out);
    }
    out.len()
}

/// SSE2 baseline: four 16-byte quarters per block, same classification.
#[cfg(target_arch = "x86_64")]
fn sse2_scan(input: &[u8], out: &mut Vec<u32>) -> usize {
    #[inline]
    fn quarter(chunk: &[u8]) -> (u16, u16, u16, u16) {
        debug_assert_eq!(chunk.len(), 16);
        // SAFETY: SSE2 is baseline on x86-64; the load reads exactly the
        // 16 bytes of `chunk` (length asserted above).
        unsafe {
            let v = _mm_loadu_si128(chunk.as_ptr().cast::<__m128i>());
            let bs = _mm_movemask_epi8(_mm_cmpeq_epi8(v, _mm_set1_epi8(b'\\' as i8))) as u16;
            let quote = _mm_movemask_epi8(_mm_cmpeq_epi8(v, _mm_set1_epi8(b'"' as i8))) as u16;
            let folded = _mm_or_si128(v, _mm_set1_epi8(0x20));
            let op = _mm_movemask_epi8(_mm_or_si128(
                _mm_or_si128(
                    _mm_cmpeq_epi8(folded, _mm_set1_epi8(b'{' as i8)),
                    _mm_cmpeq_epi8(folded, _mm_set1_epi8(b'}' as i8)),
                ),
                _mm_or_si128(
                    _mm_cmpeq_epi8(v, _mm_set1_epi8(b':' as i8)),
                    _mm_cmpeq_epi8(v, _mm_set1_epi8(b',' as i8)),
                ),
            )) as u16;
            let ws = _mm_movemask_epi8(_mm_or_si128(
                _mm_or_si128(
                    _mm_cmpeq_epi8(v, _mm_set1_epi8(b' ' as i8)),
                    _mm_cmpeq_epi8(v, _mm_set1_epi8(b'\t' as i8)),
                ),
                _mm_or_si128(
                    _mm_cmpeq_epi8(v, _mm_set1_epi8(b'\n' as i8)),
                    _mm_cmpeq_epi8(v, _mm_set1_epi8(b'\r' as i8)),
                ),
            )) as u16;
            (bs, quote, op, ws)
        }
    }

    scan_blocks(input, out, |block| {
        let mut m = BlockMasks::default();
        for (i, chunk) in block.chunks_exact(16).enumerate() {
            let (bs, q, op, ws) = quarter(chunk);
            let shift = i * 16;
            m.backslash |= (bs as u64) << shift;
            m.quote_raw |= (q as u64) << shift;
            m.op |= (op as u64) << shift;
            m.ws |= (ws as u64) << shift;
        }
        m
    })
}

// ---- aarch64 tier -----------------------------------------------------------

/// NEON: 16-lane compares collapsed with the `vshrn` nibble-movemask idiom
/// (one nibble per lane), exactly as `crlf.rs` does.
#[cfg(target_arch = "aarch64")]
fn neon_scan(input: &[u8], out: &mut Vec<u32>) -> usize {
    use core::arch::aarch64::{
        uint8x16_t, vceqq_u8, vdupq_n_u8, vget_lane_u64, vld1q_u8, vorrq_u8, vreinterpret_u64_u8,
        vreinterpretq_u16_u8, vshrn_n_u16,
    };

    /// Collapse a 16-lane compare result into 16 mask bits (`vshrn`
    /// nibble-movemask: one 0x0/0xF nibble per lane after the shift).
    #[inline]
    fn to_bits(v: uint8x16_t) -> u16 {
        // SAFETY: NEON is baseline on aarch64; pure register ops.
        let packed = unsafe {
            let nibbles = vshrn_n_u16::<4>(vreinterpretq_u16_u8(v));
            vget_lane_u64::<0>(vreinterpret_u64_u8(nibbles))
        };
        let mut bits = 0u16;
        for lane in 0..16 {
            if (packed >> (lane * 4)) & 1 != 0 {
                bits |= 1 << lane;
            }
        }
        bits
    }

    scan_blocks(input, out, |block| {
        let mut m = BlockMasks::default();
        for (i, chunk) in block.chunks_exact(16).enumerate() {
            // SAFETY: `chunk` is exactly 16 bytes; NEON is baseline.
            let (bs, q, op, ws) = unsafe {
                let v = vld1q_u8(chunk.as_ptr());
                let bs = to_bits(vceqq_u8(v, vdupq_n_u8(b'\\')));
                let q = to_bits(vceqq_u8(v, vdupq_n_u8(b'"')));
                let op = to_bits(vorrq_u8(
                    vorrq_u8(vceqq_u8(v, vdupq_n_u8(b'{')), vceqq_u8(v, vdupq_n_u8(b'}'))),
                    vorrq_u8(
                        vorrq_u8(vceqq_u8(v, vdupq_n_u8(b'[')), vceqq_u8(v, vdupq_n_u8(b']'))),
                        vorrq_u8(vceqq_u8(v, vdupq_n_u8(b':')), vceqq_u8(v, vdupq_n_u8(b','))),
                    ),
                ));
                let ws = to_bits(vorrq_u8(
                    vorrq_u8(vceqq_u8(v, vdupq_n_u8(b' ')), vceqq_u8(v, vdupq_n_u8(b'\t'))),
                    vorrq_u8(vceqq_u8(v, vdupq_n_u8(b'\n')), vceqq_u8(v, vdupq_n_u8(b'\r'))),
                ));
                (bs, q, op, ws)
            };
            let shift = i * 16;
            m.backslash |= (bs as u64) << shift;
            m.quote_raw |= (q as u64) << shift;
            m.op |= (op as u64) << shift;
            m.ws |= (ws as u64) << shift;
        }
        m
    });
}

// ---- fused string-content copy (ADR-0047 K1) --------------------------------

/// Append `src` to `out` while scanning for JSON string specials — a raw
/// backslash or a control byte (< 0x20). One pass replaces the parser's
/// separate scan (`find_special`) + copy (`append_from_input`) passes on
/// escape-free string content, which dominates real corpora.
///
/// Returns `None` with `src` fully appended, or `Some(i)` — the index in
/// `src` of the first special byte — with `out` logically unchanged
/// (its length is restored; bytes beyond it are spare capacity). The
/// caller owns escape decoding and error typing, so accept/reject
/// behavior stays byte-identical to the two-pass path.
#[inline]
pub fn json_copy_unescaped(src: &[u8], out: &mut Vec<u8>) -> Option<usize> {
    #[cfg(all(target_arch = "x86_64", not(miri)))]
    {
        static SIMD_LEVEL: AtomicU8 = AtomicU8::new(SIMD_LEVEL_UNKNOWN);
        if src.len() >= 32 {
            let mut level = SIMD_LEVEL.load(Ordering::Relaxed);
            if level == SIMD_LEVEL_UNKNOWN {
                level = if std::arch::is_x86_feature_detected!("avx2") {
                    SIMD_LEVEL_AVX2
                } else {
                    SIMD_LEVEL_SSE2
                };
                SIMD_LEVEL.store(level, Ordering::Relaxed);
            }
            if level == SIMD_LEVEL_AVX2 {
                // SAFETY: runtime dispatch above guarantees AVX2.
                return unsafe { avx2_copy_unescaped(src, out) };
            }
        }
    }
    scalar_json_copy_unescaped(src, out)
}

/// Safe SWAR tier (and the portability path): the same fused
/// scan-while-copy contract as [`json_copy_unescaped`], word-at-a-time.
/// No SSE2 tier — the word loop is the measured fallback, and an
/// unmeasured port would be L4 theater (the `utf8_is_valid` precedent).
pub fn scalar_json_copy_unescaped(src: &[u8], out: &mut Vec<u8>) -> Option<usize> {
    let base = out.len();
    let len = src.len();
    let mut i = 0;
    while i + 8 <= len {
        let w = u64::from_le_bytes(src[i..i + 8].try_into().expect("8-byte chunk"));
        let hit = word_special(w);
        if hit != 0 {
            out.truncate(base);
            return Some(i + (hit.trailing_zeros() / 8) as usize);
        }
        out.extend_from_slice(&w.to_le_bytes());
        i += 8;
    }
    while i < len {
        let b = src[i];
        if b < 0x20 || b == b'\\' {
            out.truncate(base);
            return Some(i);
        }
        out.push(b);
        i += 1;
    }
    None
}

/// Short-string fused copy (ADR-0047 K2): the `len <= 31` companion of
/// [`json_copy_unescaped`] for the fixstr path, where per-word `Vec`
/// extends dominate. One 32-byte load classifies and copies the whole
/// string: `window` must expose at least 32 readable bytes starting at
/// the string content (the caller checks input slack), `1 <= len <= 31`.
/// Returns `false` — with `out` logically unchanged — when a special
/// byte (backslash or control) sits inside `len`.
#[inline]
pub fn json_copy_unescaped_short(window: &[u8], len: usize, out: &mut Vec<u8>) -> bool {
    assert!(window.len() >= 32, "caller guarantees 32 bytes of window");
    assert!((1..=31).contains(&len), "fixstr payload length");
    #[cfg(all(target_arch = "x86_64", not(miri)))]
    {
        static SIMD_LEVEL: AtomicU8 = AtomicU8::new(SIMD_LEVEL_UNKNOWN);
        let mut level = SIMD_LEVEL.load(Ordering::Relaxed);
        if level == SIMD_LEVEL_UNKNOWN {
            level = if std::arch::is_x86_feature_detected!("avx2") {
                SIMD_LEVEL_AVX2
            } else {
                SIMD_LEVEL_SSE2
            };
            SIMD_LEVEL.store(level, Ordering::Relaxed);
        }
        if level == SIMD_LEVEL_AVX2 {
            // SAFETY: runtime dispatch above guarantees AVX2; the length
            // preconditions were asserted at entry.
            return unsafe { avx2_copy_unescaped_short(window, len, out) };
        }
    }
    scalar_json_copy_unescaped_short(window, len, out)
}

/// Safe SWAR tier of [`json_copy_unescaped_short`] (and the Miri path).
pub fn scalar_json_copy_unescaped_short(window: &[u8], len: usize, out: &mut Vec<u8>) -> bool {
    assert!(window.len() >= 32);
    assert!((1..=31).contains(&len));
    let base = out.len();
    let mut p = 0;
    while p < len {
        let w = u64::from_le_bytes(window[p..p + 8].try_into().expect("slack-checked word"));
        let mut hit = word_special(w);
        if hit != 0 {
            let live = len - p;
            if live < 8 {
                hit &= u64::MAX >> ((8 - live) * 8);
            }
            if hit != 0 {
                out.truncate(base);
                return false;
            }
        }
        out.extend_from_slice(&w.to_le_bytes());
        p += 8;
    }
    out.truncate(base + len);
    true
}

/// AVX2 tier: one load, one masked classify, one store.
#[cfg(all(target_arch = "x86_64", not(miri)))]
#[target_feature(enable = "avx2")]
unsafe fn avx2_copy_unescaped_short(window: &[u8], len: usize, out: &mut Vec<u8>) -> bool {
    use core::arch::x86_64::_mm256_min_epu8;
    debug_assert!(window.len() >= 32);
    debug_assert!((1..=31).contains(&len));
    // SAFETY: `window.len() >= 32` (asserted by the dispatcher).
    let v = unsafe { _mm256_loadu_si256(window.as_ptr().cast::<__m256i>()) };
    let backslash = _mm256_cmpeq_epi8(v, _mm256_set1_epi8(b'\\' as i8));
    let control = _mm256_cmpeq_epi8(_mm256_min_epu8(v, _mm256_set1_epi8(0x1F)), v);
    let special = _mm256_movemask_epi8(_mm256_or_si256(backslash, control)) as u32;
    if special & ((1u32 << len) - 1) != 0 {
        return false;
    }
    let base = out.len();
    out.reserve(32);
    // SAFETY: `reserve(32)` guarantees the 32-byte store fits in spare
    // capacity; `set_len(base + len)` exposes only the first `len` bytes,
    // all initialized by the store (the tail stays spare capacity).
    unsafe {
        _mm256_storeu_si256(out.as_mut_ptr().add(base).cast::<__m256i>(), v);
        out.set_len(base + len);
    }
    true
}

/// SWAR special detector for one LE word: high bit set per byte that is a
/// backslash (0x5C) or a raw control (< 0x20).
#[inline]
fn word_special(w: u64) -> u64 {
    const LO: u64 = 0x0101_0101_0101_0101;
    const HI: u64 = 0x8080_8080_8080_8080;
    let control = w.wrapping_sub(LO * 0x20) & !w & HI;
    let x = w ^ (LO * 0x5C);
    let backslash = x.wrapping_sub(LO) & !x & HI;
    control | backslash
}

/// AVX2 tier: 32-byte blocks, classify-and-store straight-line; the final
/// block overlaps backward (`src.len() >= 32`, dispatcher-guaranteed) —
/// the re-covered prefix already scanned clean, so any set mask bit is a
/// genuinely new position.
#[cfg(all(target_arch = "x86_64", not(miri)))]
#[target_feature(enable = "avx2")]
unsafe fn avx2_copy_unescaped(src: &[u8], out: &mut Vec<u8>) -> Option<usize> {
    #[target_feature(enable = "avx2")]
    #[inline]
    unsafe fn special_mask(v: __m256i) -> u32 {
        use core::arch::x86_64::_mm256_min_epu8;
        let backslash = _mm256_cmpeq_epi8(v, _mm256_set1_epi8(b'\\' as i8));
        // Unsigned v < 0x20 ⟺ min(v, 0x1F) == v.
        let control = _mm256_cmpeq_epi8(_mm256_min_epu8(v, _mm256_set1_epi8(0x1F)), v);
        _mm256_movemask_epi8(_mm256_or_si256(backslash, control)) as u32
    }

    let len = src.len();
    debug_assert!(len >= 32, "dispatcher sends >= 32-byte content only");
    let base = out.len();
    out.reserve(len);
    let src_ptr = src.as_ptr();
    // SAFETY: `reserve(len)` above guarantees capacity >= base + len; every
    // store below lands inside `[base, base + len)` of that reservation.
    let dst = unsafe { out.as_mut_ptr().add(base) };
    let mut i = 0;
    while i + 32 <= len {
        // SAFETY: `i + 32 <= len` bounds the load inside `src`.
        let v = unsafe { _mm256_loadu_si256(src_ptr.add(i).cast::<__m256i>()) };
        // SAFETY: pure register arithmetic; the fn is only target_feature-gated.
        let special = unsafe { special_mask(v) };
        // SAFETY: `base + i + 32 <= base + len` — inside the reservation.
        // Store before the branch: the block is copied either way, and a
        // `Some` return leaves it as spare capacity (never exposed).
        unsafe { _mm256_storeu_si256(dst.add(i).cast::<__m256i>(), v) };
        if special != 0 {
            return Some(i + special.trailing_zeros() as usize);
        }
        i += 32;
    }
    if i < len {
        let off = len - 32;
        // SAFETY: `len >= 32`, so `off..off + 32` is inside `src`; the
        // matching store is inside the reservation as above.
        let v = unsafe { _mm256_loadu_si256(src_ptr.add(off).cast::<__m256i>()) };
        // SAFETY: pure register arithmetic; the fn is only target_feature-gated.
        let special = unsafe { special_mask(v) };
        // SAFETY: the store lands at `base + off .. base + len` — inside the
        // reservation; the re-covered prefix rewrites identical bytes.
        unsafe { _mm256_storeu_si256(dst.add(off).cast::<__m256i>(), v) };
        if special != 0 {
            let hit = off + special.trailing_zeros() as usize;
            // The overlapped prefix `off..i` was scanned clean by earlier
            // blocks, so the first set bit is at or past `i`.
            debug_assert!(hit >= i, "hit in a re-covered clean prefix");
            return Some(hit);
        }
    }
    // SAFETY: exactly `len` bytes at `base..base + len` were initialized by
    // the stores above (full blocks + the backward-overlapped tail).
    unsafe { out.set_len(base + len) };
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(input: &[u8]) -> Vec<u32> {
        let mut out = Vec::new();
        let n = json_scan_structurals(input, &mut out);
        out.truncate(n);
        out
    }

    fn scalar(input: &[u8]) -> Vec<u32> {
        let mut out = Vec::new();
        let n = scalar_json_scan_structurals(input, &mut out);
        out.truncate(n);
        out
    }

    #[test]
    fn emits_ops_quotes_and_scalar_starts() {
        let doc = br#"{"a":12, "b":[true,null]}"#;
        let idx = scan(doc);
        assert_eq!(idx, scalar(doc));
        // {  "a  "  :  1(2)  ,  "b  "  :  [  t...  ,  n...  ]  }
        assert_eq!(idx, vec![0u32, 1, 3, 4, 5, 7, 9, 11, 12, 13, 14, 18, 19, 23, 24]);
    }

    #[test]
    fn escaped_quotes_stay_in_string() {
        let doc = br#"{"k\"ey":1}"#;
        let idx = scan(doc);
        assert_eq!(idx, scalar(doc));
        // { open close : 1 } — the escaped quote emits nothing.
        assert_eq!(idx, vec![0u32, 1, 7, 8, 9, 10]);
    }

    #[test]
    fn structural_chars_inside_strings_do_not_emit() {
        let doc = br#"["{}[]:,","x"]"#;
        let idx = scan(doc);
        assert_eq!(idx, scalar(doc));
        assert_eq!(idx, vec![0u32, 1, 8, 9, 10, 12, 13]);
    }

    #[test]
    fn backslash_runs_resolve_across_block_boundaries() {
        // A string whose escape run straddles the 64-byte boundary.
        let mut doc = Vec::from(&br#"{"k":""#[..]);
        while doc.len() < 63 {
            doc.push(b'x');
        }
        doc.extend_from_slice(br#"\\\""#); // escaped backslash + escaped quote
        doc.extend_from_slice(br#"""#);
        doc.extend_from_slice(b"}");
        assert_eq!(scan(&doc), scalar(&doc));
    }

    #[test]
    fn empty_and_ws_only_inputs_emit_nothing() {
        assert!(scan(b"").is_empty());
        assert!(scan(b" \t\r\n  ").is_empty());
    }

    /// Every tier must emit identical indices for arbitrary bytes — the
    /// crlf.rs equivalence-suite pattern. JSON-ish inputs weight the
    /// interesting characters; raw bytes cover the rest.
    mod equivalence {
        use super::*;
        use proptest::prelude::*;

        fn all_tiers(input: &[u8]) -> Vec<Vec<u32>> {
            let mut dispatched = Vec::new();
            let n = json_scan_structurals(input, &mut dispatched);
            dispatched.truncate(n);
            let mut scalar = Vec::new();
            let n = scalar_json_scan_structurals(input, &mut scalar);
            scalar.truncate(n);
            let mut tiers = vec![dispatched, scalar];
            #[cfg(target_arch = "x86_64")]
            {
                let mut sse2 = Vec::new();
                let n = super::super::sse2_scan(input, &mut sse2);
                sse2.truncate(n);
                tiers.push(sse2);
            }
            tiers
        }

        proptest! {
            #[test]
            fn tiers_agree_on_jsonish_bytes(input in proptest::collection::vec(
                prop_oneof![
                    Just(b'"'), Just(b'\\'), Just(b'{'), Just(b'}'), Just(b'['),
                    Just(b']'), Just(b':'), Just(b','), Just(b' '), Just(b'\n'),
                    Just(b'a'), Just(b'1'), Just(0xC3u8),
                ],
                0..300,
            )) {
                let tiers = all_tiers(&input);
                for t in &tiers[1..] {
                    prop_assert_eq!(&tiers[0], t);
                }
            }

            #[test]
            fn tiers_agree_on_arbitrary_bytes(input in proptest::collection::vec(
                any::<u8>(),
                0..300,
            )) {
                let tiers = all_tiers(&input);
                for t in &tiers[1..] {
                    prop_assert_eq!(&tiers[0], t);
                }
            }
        }
    }

    /// Fused copy kernel (ADR-0047 K1): every tier must agree with an
    /// independent per-byte oracle on both the verdict and the appended
    /// bytes, across block boundaries and pre-existing output content.
    mod copy_unescaped {
        use super::*;

        /// Independent oracle — a position scan, not the bit tricks.
        fn oracle(src: &[u8]) -> Option<usize> {
            src.iter().position(|&b| b < 0x20 || b == b'\\')
        }

        fn check(src: &[u8]) {
            for tier in [json_copy_unescaped, scalar_json_copy_unescaped] {
                let mut out = b"pre".to_vec();
                let verdict = tier(src, &mut out);
                assert_eq!(verdict, oracle(src), "verdict for {src:?}");
                match verdict {
                    None => {
                        assert_eq!(&out[..3], b"pre");
                        assert_eq!(&out[3..], src, "appended bytes for {src:?}");
                    }
                    Some(_) => assert_eq!(out, b"pre", "out must be untouched"),
                }
            }
        }

        #[test]
        fn boundary_sweep_matches_the_oracle() {
            // Every length crossing the word and AVX2 block boundaries,
            // with each special byte planted at every position.
            for len in [0, 1, 7, 8, 9, 31, 32, 33, 63, 64, 65, 100] {
                let clean: Vec<u8> = (0..len).map(|i| b'a' + (i % 26) as u8).collect();
                check(&clean);
                for special in [b'\\', 0x1F, 0x00, b'\n'] {
                    for at in 0..len {
                        let mut src = clean.clone();
                        src[at] = special;
                        check(&src);
                    }
                }
            }
        }

        #[test]
        fn multibyte_utf8_content_is_not_special() {
            check("padded-\u{00E9}\u{4E16}\u{1F600}-content-past-32-bytes!".as_bytes());
        }

        /// Short-kernel sweep: every `len`, every special position inside
        /// and *outside* the live length (outside must not veto), both
        /// tiers vs the oracle.
        #[test]
        fn short_kernel_matches_the_oracle_with_masked_tail() {
            let tiers = [json_copy_unescaped_short, scalar_json_copy_unescaped_short];
            for len in 1..=31usize {
                let mut window = [0u8; 40];
                for (i, b) in window.iter_mut().enumerate() {
                    *b = b'a' + (i % 26) as u8;
                }
                for special_at in 0..40usize {
                    let mut w = window;
                    w[special_at] = b'\\';
                    let expect = special_at >= len;
                    for tier in tiers {
                        let mut out = b"pre".to_vec();
                        let ok = tier(&w[..40], len, &mut out);
                        assert_eq!(ok, expect, "len {len}, special at {special_at}");
                        if ok {
                            assert_eq!(&out[..3], b"pre");
                            assert_eq!(&out[3..], &w[..len]);
                        } else {
                            assert_eq!(out, b"pre");
                        }
                    }
                }
            }
        }

        mod equivalence {
            use super::*;
            use proptest::prelude::*;

            proptest! {
                #[test]
                fn tiers_agree_on_arbitrary_bytes(src in proptest::collection::vec(
                    any::<u8>(),
                    0..200,
                )) {
                    check(&src);
                }

                #[test]
                fn tiers_agree_on_stringish_bytes(src in proptest::collection::vec(
                    prop_oneof![
                        9 => (0x20u8..0x7F).prop_map(|b| b),
                        1 => prop_oneof![Just(b'\\'), Just(0x1Fu8), Just(0xC3u8)],
                    ],
                    0..200,
                )) {
                    check(&src);
                }
            }
        }
    }
}
