//! 16-way Swiss-table control-group probes (M0-S14, master plan §7.3).
//!
//! One control byte per slot: `0x00..=0x7F` = 7-bit hash fragment (full
//! slot), high-bit set = special (`0x80` empty, `0xFE` tombstone — defined
//! by `inf-store`; this module only distinguishes "high bit"). The index
//! probes 16 slots per step with two masks:
//!
//! - [`eq_mask16`]: which bytes equal a tag (candidate matches).
//! - [`high_bit_mask16`]: which bytes are special (probe-chain bookkeeping).
//!
//! x86-64 uses SSE2 (baseline, no runtime dispatch needed); aarch64 uses
//! NEON with the `vshrn` narrowing movemask (same trick as the CRLF
//! scanner). Both are property-tested against the scalar oracle.
//!
//! The 32-way AVX2 variant (milestone task note) is deliberately not here
//! yet: it changes table geometry, so it ships as an A/B-measured follow-up
//! (L4), not a default.

/// Bitmask of bytes in `group` equal to `tag` (bit i = `group[i] == tag`).
#[inline]
pub fn eq_mask16(group: &[u8; 16], tag: u8) -> u16 {
    imp::eq_mask16(group, tag)
}

/// Bitmask of bytes in `group` with the high bit set (empty/tombstone).
#[inline]
pub fn high_bit_mask16(group: &[u8; 16]) -> u16 {
    imp::high_bit_mask16(group)
}

/// Scalar oracle (and fallback for other targets).
pub fn scalar_eq_mask16(group: &[u8; 16], tag: u8) -> u16 {
    group.iter().enumerate().fold(0u16, |m, (i, &b)| m | (u16::from(b == tag) << i))
}

/// Scalar oracle for [`high_bit_mask16`].
pub fn scalar_high_bit_mask16(group: &[u8; 16]) -> u16 {
    group.iter().enumerate().fold(0u16, |m, (i, &b)| m | (u16::from(b & 0x80 != 0) << i))
}

/// Best-effort read prefetch hint for `ptr` (no-op on unknown targets).
/// Safe to call with any pointer value: prefetch never faults — the CPU
/// drops hints for unmapped addresses.
#[inline]
pub fn prefetch_read(ptr: *const u8) {
    imp::prefetch_read(ptr);
}

#[cfg(target_arch = "x86_64")]
#[allow(unsafe_code)]
mod imp {
    use core::arch::x86_64::{
        __m128i, _MM_HINT_T0, _mm_cmpeq_epi8, _mm_loadu_si128, _mm_movemask_epi8, _mm_prefetch,
        _mm_set1_epi8,
    };

    #[inline]
    pub fn eq_mask16(group: &[u8; 16], tag: u8) -> u16 {
        // SAFETY: SSE2 is baseline on x86-64; loadu reads exactly 16 bytes
        // from the borrowed array (valid, unaligned-tolerant).
        unsafe {
            let g = _mm_loadu_si128(group.as_ptr().cast::<__m128i>());
            let t = _mm_set1_epi8(tag as i8);
            _mm_movemask_epi8(_mm_cmpeq_epi8(g, t)) as u16
        }
    }

    #[inline]
    pub fn high_bit_mask16(group: &[u8; 16]) -> u16 {
        // SAFETY: as above; movemask reads each byte's sign bit directly.
        unsafe {
            let g = _mm_loadu_si128(group.as_ptr().cast::<__m128i>());
            _mm_movemask_epi8(g) as u16
        }
    }

    #[inline]
    pub fn prefetch_read(ptr: *const u8) {
        // SAFETY: prefetch is a hint; it cannot fault on any address.
        unsafe { _mm_prefetch::<_MM_HINT_T0>(ptr.cast()) }
    }
}

#[cfg(target_arch = "aarch64")]
#[allow(unsafe_code)]
mod imp {
    use core::arch::aarch64::{
        vandq_u8, vceqq_u8, vdupq_n_u8, vget_lane_u64, vld1q_u8, vreinterpret_u64_u8,
        vreinterpretq_u16_u8, vshrn_n_u16,
    };

    /// `vshrn` narrowing movemask: 4 bits per lane, then sample every 4th.
    #[inline]
    fn mask_from_cmp(cmp: core::arch::aarch64::uint8x16_t) -> u16 {
        // SAFETY: NEON is baseline on aarch64.
        unsafe {
            let narrowed = vshrn_n_u16::<4>(vreinterpretq_u16_u8(cmp));
            let bits = vget_lane_u64::<0>(vreinterpret_u64_u8(narrowed));
            // Each original lane contributed a 0x0 or 0xF nibble.
            let mut mask = 0u16;
            let mut i = 0;
            while i < 16 {
                if (bits >> (i * 4)) & 0x1 != 0 {
                    mask |= 1 << i;
                }
                i += 1;
            }
            mask
        }
    }

    #[inline]
    pub fn eq_mask16(group: &[u8; 16], tag: u8) -> u16 {
        // SAFETY: vld1q_u8 reads exactly 16 bytes from the borrowed array.
        unsafe {
            let g = vld1q_u8(group.as_ptr());
            mask_from_cmp(vceqq_u8(g, vdupq_n_u8(tag)))
        }
    }

    #[inline]
    pub fn high_bit_mask16(group: &[u8; 16]) -> u16 {
        // SAFETY: as above; isolate the high bit then compare to it.
        unsafe {
            let g = vld1q_u8(group.as_ptr());
            let hi = vandq_u8(g, vdupq_n_u8(0x80));
            mask_from_cmp(vceqq_u8(hi, vdupq_n_u8(0x80)))
        }
    }

    #[inline]
    pub fn prefetch_read(_ptr: *const u8) {
        // aarch64 prefetch intrinsics are unstable; the OoO window covers
        // the dev tier. (Measured decision pending the L4 A/B on x86.)
    }
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
mod imp {
    #[inline]
    pub fn eq_mask16(group: &[u8; 16], tag: u8) -> u16 {
        super::scalar_eq_mask16(group, tag)
    }

    #[inline]
    pub fn high_bit_mask16(group: &[u8; 16]) -> u16 {
        super::scalar_high_bit_mask16(group)
    }

    #[inline]
    pub fn prefetch_read(_ptr: *const u8) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn eq_mask_basics() {
        let mut group = [0x80u8; 16];
        group[3] = 0x2A;
        group[9] = 0x2A;
        group[15] = 0x2A;
        assert_eq!(eq_mask16(&group, 0x2A), (1 << 3) | (1 << 9) | (1 << 15));
        assert_eq!(eq_mask16(&group, 0x2B), 0);
    }

    #[test]
    fn high_bit_mask_distinguishes_special() {
        let mut group = [0u8; 16];
        group[0] = 0x80; // empty
        group[5] = 0xFE; // tombstone
        group[6] = 0x7F; // full (max h2)
        assert_eq!(high_bit_mask16(&group), (1 << 0) | (1 << 5));
    }

    proptest! {
        #[test]
        fn eq_matches_scalar_oracle(group in prop::array::uniform16(any::<u8>()), tag: u8) {
            prop_assert_eq!(eq_mask16(&group, tag), scalar_eq_mask16(&group, tag));
        }

        #[test]
        fn high_bit_matches_scalar_oracle(group in prop::array::uniform16(any::<u8>())) {
            prop_assert_eq!(high_bit_mask16(&group), scalar_high_bit_mask16(&group));
        }
    }
}
