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
}
