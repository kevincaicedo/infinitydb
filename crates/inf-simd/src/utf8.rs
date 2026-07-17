//! Whole-input UTF-8 validation (M3-S05 slice 3 — the named SIMD UTF-8
//! lever): the Keiser–Lemire lookup algorithm ("Validating UTF-8 in less
//! than one instruction per byte"), the same classifier simdjson runs.
//!
//! One AVX2 tier plus `std::str::from_utf8` as the portability tier *and*
//! the correctness oracle. There is deliberately no SSE2/NEON port yet:
//! std's validator is already word-optimized, the reference box has AVX2,
//! and an unmeasured port would be L4 theater — the tier gap is recorded,
//! not hidden. Runtime dispatch follows the cached-`AtomicU8` pattern from
//! `crlf.rs`.
//!
//! The verdict is boolean by design. Callers that need the exact error
//! offset re-run `std::str::from_utf8` on the (cold) reject path — and by
//! deferring to std's verdict there, a hypothetical kernel false-negative
//! degrades to a wasted std pass, never a wrong answer. The false-*accept*
//! direction is hunted continuously: the equivalence proptests below drive
//! arbitrary and adversarial bytes against std, and the `json_parse` fuzz
//! differential (serde_json rejects invalid UTF-8) would surface any
//! divergence on every fuzz run.

#[cfg(target_arch = "x86_64")]
use core::arch::x86_64::{
    __m256i, _mm256_alignr_epi8, _mm256_and_si256, _mm256_loadu_si256, _mm256_movemask_epi8,
    _mm256_or_si256, _mm256_permute2x128_si256, _mm256_set1_epi8, _mm256_setr_epi8,
    _mm256_setzero_si256, _mm256_shuffle_epi8, _mm256_srli_epi16, _mm256_subs_epu8,
    _mm256_testz_si256, _mm256_xor_si256,
};
#[cfg(target_arch = "x86_64")]
use std::sync::atomic::{AtomicU8, Ordering};

/// `true` iff `input` is valid UTF-8 — bit-for-bit the `std` verdict
/// (property-tested). Dispatches to AVX2 where available; elsewhere it
/// *is* `std::str::from_utf8`.
#[inline]
pub fn utf8_is_valid(input: &[u8]) -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        static LEVEL: AtomicU8 = AtomicU8::new(0);
        let mut level = LEVEL.load(Ordering::Relaxed);
        if level == 0 {
            level = if std::arch::is_x86_feature_detected!("avx2") { 1 } else { 2 };
            LEVEL.store(level, Ordering::Relaxed);
        }
        if level == 1 {
            // SAFETY: dispatch above guarantees AVX2 before calling.
            return unsafe { avx2_utf8_is_valid(input) };
        }
    }
    core::str::from_utf8(input).is_ok()
}

/// Error-class bits, one per way a 2-byte prefix can be malformed. The
/// three nibble lookups (`prev1` high/low, current high) each produce a
/// candidate-error set; a real error is a bit present in all three —
/// Keiser–Lemire Table 8/9, transcribed from the paper (simdjson's
/// `utf8_lookup4`).
#[cfg(target_arch = "x86_64")]
mod bits {
    pub const TOO_SHORT: u8 = 1 << 0; // lead byte followed by a non-continuation
    pub const TOO_LONG: u8 = 1 << 1; // ASCII followed by a continuation
    pub const OVERLONG_3: u8 = 1 << 2; // E0 followed by < A0
    pub const TOO_LARGE: u8 = 1 << 3; // F4 followed by > 8F (> U+10FFFF)
    pub const SURROGATE: u8 = 1 << 4; // ED followed by ≥ A0
    pub const OVERLONG_2: u8 = 1 << 5; // C0/C1 lead
    pub const TOO_LARGE_1000: u8 = 1 << 6; // > F4 lead
    pub const OVERLONG_4: u8 = 1 << 6; // F0 followed by < 90 (shares the bit)
    pub const TWO_CONTS: u8 = 1 << 7; // two continuations in a row
    pub const CARRY: u8 = TOO_SHORT | TOO_LONG | TWO_CONTS; // low nibble can't refute these
}

/// Validate as 32-byte blocks: ASCII blocks (the JSON norm) cost one
/// movemask; non-ASCII blocks run the three-shuffle classifier plus the
/// 3/4-byte continuation cross-check. The trailing partial block is
/// zero-padded into a stack buffer — NUL is ASCII, so padding terminates
/// any dangling sequence exactly like end-of-input must.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn avx2_utf8_is_valid(input: &[u8]) -> bool {
    // SAFETY: `#[target_feature(avx2)]`, reached only via the checked
    // dispatch. All loads are `loadu` on regions proven in bounds:
    // 32-byte `chunks_exact(32)` slices and one stack pad buffer.
    unsafe {
        let mut error = _mm256_setzero_si256();
        let mut prev = _mm256_setzero_si256();
        let mut prev_incomplete = _mm256_setzero_si256();
        let mut chunks = input.chunks_exact(32);
        for chunk in &mut chunks {
            let current = _mm256_loadu_si256(chunk.as_ptr() as *const __m256i);
            step(current, &mut prev, &mut prev_incomplete, &mut error);
        }
        let tail = chunks.remainder();
        if !tail.is_empty() {
            let mut pad = [0u8; 32];
            pad[..tail.len()].copy_from_slice(tail);
            let current = _mm256_loadu_si256(pad.as_ptr() as *const __m256i);
            step(current, &mut prev, &mut prev_incomplete, &mut error);
        }
        // A sequence still open at end-of-input is an error the block loop
        // could not see (nothing followed it to refute).
        error = _mm256_or_si256(error, prev_incomplete);
        _mm256_testz_si256(error, error) == 1
    }
}

/// One 32-byte block through the validator state.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
#[inline]
unsafe fn step(
    current: __m256i,
    prev: &mut __m256i,
    prev_incomplete: &mut __m256i,
    error: &mut __m256i,
) {
    // SAFETY: value-only intrinsics plus calls into sibling
    // `#[target_feature(avx2)]` fns — no memory access; the caller holds
    // the AVX2 contract.
    unsafe {
        if _mm256_movemask_epi8(current) == 0 {
            // Pure ASCII: valid unless the previous block ended mid-sequence.
            *error = _mm256_or_si256(*error, *prev_incomplete);
        } else {
            let sc = classify(current, *prev);
            *error = _mm256_or_si256(*error, sc);
            *prev_incomplete = incomplete(current);
        }
        *prev = current;
    }
}

/// Bytes shifted right by `16 - IMM` with carry-in from the previous
/// block (simdjson's `prev<N>` with `IMM = 16 - N`): per-lane `alignr`
/// over `[prev_hi : cur_lo]`.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
#[inline]
unsafe fn shift_in<const IMM: i32>(current: __m256i, prev: __m256i) -> __m256i {
    let joined = _mm256_permute2x128_si256(prev, current, 0x21);
    _mm256_alignr_epi8::<IMM>(current, joined)
}

/// The Keiser–Lemire classifier for one non-ASCII block: three nibble
/// lookups intersect into per-byte error classes, then the 3/4-byte
/// continuation obligation is cross-checked against `TWO_CONTS`.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
#[inline]
unsafe fn classify(current: __m256i, prev: __m256i) -> __m256i {
    use bits::*;
    // SAFETY: value-only intrinsics plus calls into sibling
    // `#[target_feature(avx2)]` fns — no memory access; the caller holds
    // the AVX2 contract.
    unsafe {
        let prev1 = shift_in::<15>(current, prev);
        let low_nibbles = _mm256_set1_epi8(0x0F);
        let prev1_high = _mm256_and_si256(_mm256_srli_epi16(prev1, 4), low_nibbles);
        let prev1_low = _mm256_and_si256(prev1, low_nibbles);
        let cur_high = _mm256_and_si256(_mm256_srli_epi16(current, 4), low_nibbles);

        #[rustfmt::skip]
        let byte_1_high = shuffle16(prev1_high, [
            TOO_LONG, TOO_LONG, TOO_LONG, TOO_LONG,
            TOO_LONG, TOO_LONG, TOO_LONG, TOO_LONG,
            TWO_CONTS, TWO_CONTS, TWO_CONTS, TWO_CONTS,
            TOO_SHORT | OVERLONG_2,
            TOO_SHORT,
            TOO_SHORT | OVERLONG_3 | SURROGATE,
            TOO_SHORT | TOO_LARGE | TOO_LARGE_1000 | OVERLONG_4,
        ]);
        #[rustfmt::skip]
        let byte_1_low = shuffle16(prev1_low, [
            CARRY | OVERLONG_3 | OVERLONG_2 | OVERLONG_4,
            CARRY | OVERLONG_2,
            CARRY, CARRY,
            CARRY | TOO_LARGE,
            CARRY | TOO_LARGE | TOO_LARGE_1000,
            CARRY | TOO_LARGE | TOO_LARGE_1000,
            CARRY | TOO_LARGE | TOO_LARGE_1000,
            CARRY | TOO_LARGE | TOO_LARGE_1000,
            CARRY | TOO_LARGE | TOO_LARGE_1000,
            CARRY | TOO_LARGE | TOO_LARGE_1000,
            CARRY | TOO_LARGE | TOO_LARGE_1000,
            CARRY | TOO_LARGE | TOO_LARGE_1000,
            // 0xD: the ED lead — the one place SURROGATE can be refuted.
            CARRY | TOO_LARGE | TOO_LARGE_1000 | SURROGATE,
            CARRY | TOO_LARGE | TOO_LARGE_1000,
            CARRY | TOO_LARGE | TOO_LARGE_1000,
        ]);
        #[rustfmt::skip]
        let byte_2_high = shuffle16(cur_high, [
            TOO_SHORT, TOO_SHORT, TOO_SHORT, TOO_SHORT,
            TOO_SHORT, TOO_SHORT, TOO_SHORT, TOO_SHORT,
            TOO_LONG | OVERLONG_2 | TWO_CONTS | OVERLONG_3 | TOO_LARGE_1000 | OVERLONG_4,
            TOO_LONG | OVERLONG_2 | TWO_CONTS | OVERLONG_3 | TOO_LARGE,
            TOO_LONG | OVERLONG_2 | TWO_CONTS | SURROGATE | TOO_LARGE,
            TOO_LONG | OVERLONG_2 | TWO_CONTS | SURROGATE | TOO_LARGE,
            TOO_SHORT, TOO_SHORT, TOO_SHORT, TOO_SHORT,
        ]);
        let special = _mm256_and_si256(_mm256_and_si256(byte_1_high, byte_1_low), byte_2_high);

        // Bytes that MUST be continuations because a 3/4-byte lead sits
        // 2/3 positions back. `TWO_CONTS` (0x80) marks bytes the shuffle
        // pass *found* to be continuation-after-continuation; XOR flags
        // any disagreement in either direction.
        let prev2 = shift_in::<14>(current, prev);
        let prev3 = shift_in::<13>(current, prev);
        let is_third = _mm256_subs_epu8(prev2, _mm256_set1_epi8((0xE0u8 - 0x80) as i8));
        let is_fourth = _mm256_subs_epu8(prev3, _mm256_set1_epi8((0xF0u8 - 0x80) as i8));
        let must32 = _mm256_or_si256(is_third, is_fourth);
        let must32_80 = _mm256_and_si256(must32, _mm256_set1_epi8(0x80u8 as i8));
        _mm256_xor_si256(must32_80, special)
    }
}

/// Nonzero bytes where the block's tail opens a sequence it cannot close
/// (a 2/3/4-byte lead within the last 1/2/3 bytes).
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
#[inline]
unsafe fn incomplete(current: __m256i) -> __m256i {
    #[rustfmt::skip]
    let max_value = _mm256_setr_epi8(
        -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1,
        -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1,
        (0xF0u8 - 1) as i8, (0xE0u8 - 1) as i8, (0xC0u8 - 1) as i8,
    );
    _mm256_subs_epu8(current, max_value)
}

/// Per-byte 16-entry table lookup over nibble indices (both lanes carry
/// the same table).
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
#[inline]
unsafe fn shuffle16(nibbles: __m256i, t: [u8; 16]) -> __m256i {
    #[rustfmt::skip]
    let table = _mm256_setr_epi8(
        t[0] as i8, t[1] as i8, t[2] as i8, t[3] as i8, t[4] as i8, t[5] as i8, t[6] as i8,
        t[7] as i8, t[8] as i8, t[9] as i8, t[10] as i8, t[11] as i8, t[12] as i8, t[13] as i8,
        t[14] as i8, t[15] as i8,
        t[0] as i8, t[1] as i8, t[2] as i8, t[3] as i8, t[4] as i8, t[5] as i8, t[6] as i8,
        t[7] as i8, t[8] as i8, t[9] as i8, t[10] as i8, t[11] as i8, t[12] as i8, t[13] as i8,
        t[14] as i8, t[15] as i8,
    );
    _mm256_shuffle_epi8(table, nibbles)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agree(input: &[u8]) {
        assert_eq!(
            utf8_is_valid(input),
            core::str::from_utf8(input).is_ok(),
            "kernel disagrees with std on {input:x?}"
        );
    }

    /// Every UTF-8 error class, each also shifted to straddle the 32-byte
    /// block boundary and placed at end-of-input (the incomplete check).
    #[test]
    fn adversarial_corpus_matches_std() {
        let cases: &[&[u8]] = &[
            b"",
            b"plain ascii",
            "καλημέρα κόσμε ✨ 𝄞".as_bytes(),
            &[0x80],                   // stray continuation
            &[0xBF],                   // stray continuation (high)
            &[0xC0, 0x80],             // overlong 2-byte
            &[0xC1, 0xBF],             // overlong 2-byte
            &[0xC2],                   // truncated 2-byte
            &[0xC2, 0x80],             // minimal valid 2-byte
            &[0xC2, 0x41],             // 2-byte lead + ASCII
            &[0xE0, 0x80, 0x80],       // overlong 3-byte
            &[0xE0, 0x9F, 0xBF],       // overlong 3-byte (max)
            &[0xE0, 0xA0, 0x80],       // minimal valid 3-byte
            &[0xED, 0x9F, 0xBF],       // U+D7FF: last before surrogates
            &[0xED, 0xA0, 0x80],       // surrogate U+D800
            &[0xED, 0xBF, 0xBF],       // surrogate U+DFFF
            &[0xEE, 0x80, 0x80],       // U+E000: first after surrogates
            &[0xE2, 0x82],             // truncated 3-byte
            &[0xE2, 0x82, 0x41],       // 3-byte cut short by ASCII
            &[0xF0, 0x80, 0x80, 0x80], // overlong 4-byte
            &[0xF0, 0x8F, 0xBF, 0xBF], // overlong 4-byte (max)
            &[0xF0, 0x90, 0x80, 0x80], // minimal valid 4-byte
            &[0xF4, 0x8F, 0xBF, 0xBF], // U+10FFFF: the ceiling
            &[0xF4, 0x90, 0x80, 0x80], // above U+10FFFF
            &[0xF5, 0x80, 0x80, 0x80], // invalid lead F5
            &[0xFF],                   // invalid lead FF
            &[0xF0, 0x90, 0x80],       // truncated 4-byte
            &[0x41, 0x80],             // ASCII + continuation
            &[0xC2, 0x80, 0x80],       // valid 2-byte + stray cont
        ];
        for case in cases {
            agree(case);
            for pad_len in [29usize, 30, 31, 32, 61, 62, 63, 64] {
                let mut padded = vec![b'a'; pad_len];
                padded.extend_from_slice(case);
                agree(&padded); // straddles / ends a block
                padded.extend_from_slice(b"tail bytes after the sequence");
                agree(&padded);
            }
        }
    }

    /// The block-boundary ASCII fast path must still see a dangling
    /// sequence from the previous block.
    #[test]
    fn incomplete_before_ascii_block_is_rejected() {
        let mut v = vec![b'x'; 31];
        v.push(0xE2); // block 0 ends with an open 3-byte sequence
        v.extend_from_slice(&[b'y'; 32]); // block 1 is pure ASCII
        agree(&v);
        assert!(!utf8_is_valid(&v));
    }

    proptest::proptest! {
        /// Arbitrary bytes: the kernel is the std verdict, bit for bit.
        #[test]
        fn equivalence_arbitrary(input in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..600)) {
            agree(&input);
        }

        /// Valid text of every width class, optionally corrupted at one
        /// position — the mutation walks the accept/reject boundary.
        #[test]
        fn equivalence_mutated_text(
            s in "[\\x00-\\x7F\u{80}-\u{7FF}\u{800}-\u{D7FF}\u{10000}-\u{10FFFF}]{0,120}",
            flip in proptest::prelude::any::<(usize, u8)>(),
        ) {
            let mut bytes = s.into_bytes();
            agree(&bytes);
            if !bytes.is_empty() {
                let at = flip.0 % bytes.len();
                bytes[at] ^= flip.1;
                agree(&bytes);
            }
        }
    }
}
