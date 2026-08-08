//! Stable 64-bit hashing for short keys (wyhash-style folded multiply).
//!
//! Stability is part of the contract: hashes feed the per-cell index and the
//! deterministic simulator (L7), so the function may never change without an
//! ADR and an index-migration story. Quality bar: passes the avalanche and
//! distribution sanity tests below; throughput target is measured at the
//! index level (M0-S14), not assumed (L4).

const P0: u64 = 0xa076_1d64_78bd_642f;
const P1: u64 = 0xe703_7ed1_a0b4_28db;
const P2: u64 = 0x8ebc_6af0_9c88_c6e3;
const P3: u64 = 0x5899_65cc_7537_4cc3;

#[inline(always)]
fn mix(a: u64, b: u64) -> u64 {
    let r = u128::from(a).wrapping_mul(u128::from(b));
    (r as u64) ^ ((r >> 64) as u64)
}

#[inline(always)]
fn read_u64(b: &[u8]) -> u64 {
    u64::from_le_bytes(b[..8].try_into().expect("caller guarantees 8 bytes"))
}

#[inline(always)]
fn read_u32(b: &[u8]) -> u64 {
    u64::from(u32::from_le_bytes(b[..4].try_into().expect("caller guarantees 4 bytes")))
}

/// Hash `data` with `seed`. Stable across platforms and releases.
#[inline]
pub fn hash64(data: &[u8], seed: u64) -> u64 {
    let len = data.len();
    let mut seed = seed ^ mix(seed ^ P0, P1);
    let a: u64;
    let b: u64;

    if len <= 16 {
        if len >= 4 {
            // Two possibly-overlapping 4-byte reads from each end.
            a = (read_u32(data) << 32) | read_u32(&data[(len >> 3) << 2..]);
            let tail = len - 4;
            b = (read_u32(&data[tail..]) << 32) | read_u32(&data[tail - ((len >> 3) << 2)..]);
        } else if len > 0 {
            a = (u64::from(data[0]) << 16)
                | (u64::from(data[len >> 1]) << 8)
                | u64::from(data[len - 1]);
            b = 0;
        } else {
            a = 0;
            b = 0;
        }
    } else {
        let mut rest = data;
        if rest.len() > 48 {
            let mut s1 = seed;
            let mut s2 = seed;
            while rest.len() > 48 {
                seed = mix(read_u64(rest) ^ P1, read_u64(&rest[8..]) ^ seed);
                s1 = mix(read_u64(&rest[16..]) ^ P2, read_u64(&rest[24..]) ^ s1);
                s2 = mix(read_u64(&rest[32..]) ^ P3, read_u64(&rest[40..]) ^ s2);
                rest = &rest[48..];
            }
            seed ^= s1 ^ s2;
        }
        while rest.len() > 16 {
            seed = mix(read_u64(rest) ^ P1, read_u64(&rest[8..]) ^ seed);
            rest = &rest[16..];
        }
        a = read_u64(&data[len - 16..]);
        b = read_u64(&data[len - 8..]);
    }

    mix(P1 ^ (len as u64), mix(a ^ P1, b ^ seed))
}

// ---- trusted-integer table hashing -------------------------------------------

/// Folded-multiply [`core::hash::Hasher`] for **trusted integer keys**
/// (fabric tokens, completion tokens, cell ids): one 128-bit multiply per
/// integer write instead of SipHash's per-byte rounds. Not DoS-resistant —
/// use only for tables whose keys are internally generated (cell-local
/// gates, driver op tables), never attacker-chosen (client keys keep
/// [`hash64`]'s full mixing via the byte-slice fallback).
#[derive(Default, Clone, Copy)]
pub struct IntHasher(u64);

impl core::hash::Hasher for IntHasher {
    #[inline(always)]
    fn finish(&self) -> u64 {
        self.0
    }

    /// Byte-slice fallback (derived `Hash` on non-integer fields): full
    /// [`hash64`] quality, seeded by accumulated state.
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        self.0 = hash64(bytes, self.0);
    }

    #[inline(always)]
    fn write_u8(&mut self, v: u8) {
        self.write_u64(u64::from(v));
    }

    #[inline(always)]
    fn write_u16(&mut self, v: u16) {
        self.write_u64(u64::from(v));
    }

    #[inline(always)]
    fn write_u32(&mut self, v: u32) {
        self.write_u64(u64::from(v));
    }

    #[inline(always)]
    fn write_u64(&mut self, v: u64) {
        self.0 = mix(v ^ P2, self.0 ^ P1);
    }

    #[inline(always)]
    fn write_u128(&mut self, v: u128) {
        self.write_u64(v as u64);
        self.write_u64((v >> 64) as u64);
    }

    #[inline(always)]
    fn write_usize(&mut self, v: usize) {
        self.write_u64(v as u64);
    }

    #[inline(always)]
    fn write_i8(&mut self, v: i8) {
        self.write_u64(v as u8 as u64);
    }

    #[inline(always)]
    fn write_i16(&mut self, v: i16) {
        self.write_u64(v as u16 as u64);
    }

    #[inline(always)]
    fn write_i32(&mut self, v: i32) {
        self.write_u64(v as u32 as u64);
    }

    #[inline(always)]
    fn write_i64(&mut self, v: i64) {
        self.write_u64(v as u64);
    }

    #[inline(always)]
    fn write_isize(&mut self, v: isize) {
        self.write_u64(v as u64);
    }
}

/// [`core::hash::BuildHasher`] for [`IntHasher`] tables
/// (`HashMap<K, V, BuildIntHasher>`).
#[derive(Default, Clone, Copy)]
pub struct BuildIntHasher;

impl core::hash::BuildHasher for BuildIntHasher {
    type Hasher = IntHasher;

    #[inline(always)]
    fn build_hasher(&self) -> IntHasher {
        IntHasher(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_and_seed_sensitive() {
        assert_eq!(hash64(b"key:1", 0), hash64(b"key:1", 0));
        assert_ne!(hash64(b"key:1", 0), hash64(b"key:1", 1));
        assert_ne!(hash64(b"key:1", 0), hash64(b"key:2", 0));
    }

    #[test]
    fn all_lengths_consume_full_input() {
        // Flipping the last byte must change the hash at every length class
        // (tail handling is where read-window bugs hide).
        for len in 1..=128usize {
            let mut a = vec![0xABu8; len];
            let h1 = hash64(&a, 7);
            *a.last_mut().expect("non-empty") ^= 1;
            assert_ne!(h1, hash64(&a, 7), "tail byte ignored at len {len}");
        }
    }

    #[test]
    fn avalanche_sanity() {
        // One flipped input bit should move roughly half the output bits.
        let base = hash64(b"avalanche-probe", 0);
        let mut total = 0u32;
        let mut samples = 0u32;
        for byte in 0..15usize {
            for bit in 0..8u8 {
                let mut input = *b"avalanche-probe";
                input[byte] ^= 1 << bit;
                total += (hash64(&input, 0) ^ base).count_ones();
                samples += 1;
            }
        }
        let mean = f64::from(total) / f64::from(samples);
        assert!((24.0..40.0).contains(&mean), "poor avalanche: mean {mean} bits");
    }

    #[test]
    fn empty_input_is_defined() {
        let h = hash64(b"", 0);
        assert_eq!(h, hash64(b"", 0));
    }

    #[test]
    fn int_hasher_distributes_sequential_tokens() {
        // Fabric tokens are sequential u64s; low bits (hashbrown's bucket
        // mask) must not collide over a realistic window.
        use core::hash::{BuildHasher, Hasher};
        let mut low7 = std::collections::HashSet::new();
        for token in 0u64..128 {
            let mut h = BuildIntHasher.build_hasher();
            h.write_u64(token);
            low7.insert(h.finish() & 0x7F);
        }
        // Random-quality bar: 128 balls into 128 bins leaves ~81 distinct
        // in expectation (birthday); degenerate mixing would leave ≤ 16.
        assert!(low7.len() >= 72, "sequential tokens collapse: {}", low7.len());
    }

    #[test]
    fn int_hasher_width_and_sign_insensitive_widening() {
        use core::hash::{BuildHasher, Hasher};
        let one = |f: &dyn Fn(&mut IntHasher)| {
            let mut h = BuildIntHasher.build_hasher();
            f(&mut h);
            h.finish()
        };
        // Widening writes agree (u8/u16/u32 route through write_u64).
        assert_eq!(one(&|h| h.write_u8(7)), one(&|h| h.write_u64(7)));
        assert_eq!(one(&|h| h.write_u16(7)), one(&|h| h.write_u64(7)));
        assert_eq!(one(&|h| h.write_u32(7)), one(&|h| h.write_u64(7)));
        // Distinct values hash distinctly.
        assert_ne!(one(&|h| h.write_u64(1)), one(&|h| h.write_u64(2)));
    }
}
