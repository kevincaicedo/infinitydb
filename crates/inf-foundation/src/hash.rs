//! Hashing: the **key hash** ([`KeyHasher`] — SipHash-1-3 under a
//! per-store 128-bit secret, ADR-0094) and the **digest** fold
//! ([`hash64`] — a wyhash-style folded multiply, stable, unkeyed).
//!
//! The two are not interchangeable. Every index a client can populate
//! (the memory-mode and tiered indexes, the tiered 64-bit sidecar, the
//! checkpoint's `(hash, addr)` refs, the ADR-0076 primary-key refs)
//! places entries by [`KeyHasher`]: a PRF under a secret the client
//! never learns, so no key set an attacker chooses degrades a probe
//! chain or forges 64-bit "exact" evidence. The secret is a value
//! carried by the store (injected — L7), persisted once per data
//! directory (ADR-0094 D2), never a constant in this crate.
//!
//! [`hash64`] stays for inputs the engine itself produces — the `.ick`
//! checkpoint digest chain (ADR-0016), the keyspace determinism digest,
//! the simulated filesystem's path digest. Its fold has a known
//! seed-independent weakness (a block whose even word equals `P1`
//! zeroes the running state — the wyhash "zero multiplier"), which is
//! harmless for a digest of trusted bytes and disqualifying for a
//! table keyed by client bytes; ADR-0094's context section has the
//! measurement. Stability is part of its contract: the digests it
//! signs are persisted, so it may never change without an ADR.

/// Whether this build's [`KeyHasher`] carries the ADR-0094 D3 collision
/// oracle (a 48-byte `{shadow-collide}` key hashes its first 32 bytes
/// only). `true` in the store suite and the simulators, `false` in every
/// shipping binary — ADR-0107: the feature reaches a build only through
/// `[dev-dependencies]` or the simulator's explicit `dst` feature, and
/// `scripts/check-shipping-features.sh` asserts the graph.
pub const COLLISION_ORACLE: bool = cfg!(feature = "collision-oracle");

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

/// Digest `data` with `seed` — stable across platforms and releases.
/// **Not a key hash** (ADR-0094): its fold can be zeroed by a chosen
/// block regardless of `seed`; use [`KeyHasher`] for anything a client
/// can place in a table.
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

// ---- the key hash: SipHash-1-3 under a secret (ADR-0094 D1) ------------

#[inline(always)]
fn sip_round(v: &mut [u64; 4]) {
    v[0] = v[0].wrapping_add(v[1]);
    v[1] = v[1].rotate_left(13);
    v[1] ^= v[0];
    v[0] = v[0].rotate_left(32);
    v[2] = v[2].wrapping_add(v[3]);
    v[3] = v[3].rotate_left(16);
    v[3] ^= v[2];
    v[0] = v[0].wrapping_add(v[3]);
    v[3] = v[3].rotate_left(21);
    v[3] ^= v[0];
    v[2] = v[2].wrapping_add(v[1]);
    v[1] = v[1].rotate_left(17);
    v[1] ^= v[2];
    v[2] = v[2].rotate_left(32);
}

/// SipHash-c-d over `data` under the 128-bit key `(k0, k1)` — the
/// reference algorithm (Aumasson–Bernstein), little-endian message
/// words, the length byte in the final word's top byte. `C`
/// compression rounds per word, `D` finalization rounds.
#[inline]
fn siphash<const C: usize, const D: usize>(k0: u64, k1: u64, data: &[u8]) -> u64 {
    let mut v = [
        k0 ^ 0x736f_6d65_7073_6575,
        k1 ^ 0x646f_7261_6e64_6f6d,
        k0 ^ 0x6c79_6765_6e65_7261,
        k1 ^ 0x7465_6462_7974_6573,
    ];
    let mut words = data.chunks_exact(8);
    for word in &mut words {
        let m = u64::from_le_bytes(word.try_into().expect("8-byte chunk"));
        v[3] ^= m;
        for _ in 0..C {
            sip_round(&mut v);
        }
        v[0] ^= m;
    }
    let mut last = (data.len() as u64) << 56;
    for (i, &byte) in words.remainder().iter().enumerate() {
        last |= u64::from(byte) << (8 * i);
    }
    v[3] ^= last;
    for _ in 0..C {
        sip_round(&mut v);
    }
    v[0] ^= last;
    v[2] ^= 0xff;
    for _ in 0..D {
        sip_round(&mut v);
    }
    v[0] ^ v[1] ^ v[2] ^ v[3]
}

/// SipHash-1-3 of `data` under `(k0, k1)` — the key hash's function
/// (ADR-0094 D1; Redis ≥ 4.0 and Rust's `DefaultHasher` use the same
/// parameters). Prefer [`KeyHasher::hash`], which carries the secret.
#[inline]
#[must_use]
pub fn siphash13(k0: u64, k1: u64, data: &[u8]) -> u64 {
    siphash::<1, 3>(k0, k1, data)
}

/// The hashtag every forced-collision key starts with (ADR-0094 D3):
/// under the `collision-oracle` feature a 48-byte key with this prefix
/// hashes its first 32 bytes only, so keys differing in the last 16
/// bytes are distinct real keys with one hash — and the shared hashtag
/// routes them to one cell. Inert without the feature.
pub const COLLISION_KEY_PREFIX: &[u8; 16] = b"{shadow-collide}";

/// The key hash: SipHash-1-3 under a 128-bit secret (ADR-0094). A
/// `Copy` value every store carries — the secret is injected (a data
/// directory's `key-hash.toml`, a simulator's seed, a test's fixed
/// value), never a constant, and a hash is only meaningful to the
/// store whose hasher computed it (`hash_key` is an instance method on
/// every store for that reason).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct KeyHasher {
    k0: u64,
    k1: u64,
}

/// The identity of a secret (ADR-0094 D6): a PRF output under it, so it
/// names the secret without revealing it, and two secrets share an id
/// with probability 2⁻⁶⁴. Persisted where the hashes are — the MANIFEST
/// (epoch 3) — so a boot can tell whether the secret it holds is the one
/// that placed a checkpoint's refs, and refuse before applying any.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct KeyHashId(u64);

/// The fixed message whose SipHash under the secret is the identity.
const KEY_HASH_ID_DOMAIN: &[u8] = b"infinitydb/key-hash-id/v1";

impl KeyHashId {
    /// The 64-bit wire value (the MANIFEST carries it LE).
    #[must_use]
    pub const fn to_u64(self) -> u64 {
        self.0
    }

    /// From the wire value.
    #[must_use]
    pub const fn from_u64(raw: u64) -> KeyHashId {
        KeyHashId(raw)
    }
}

impl core::fmt::Display for KeyHashId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:#018x}", self.0)
    }
}

impl KeyHasher {
    /// A hasher over the 128-bit secret `(k0, k1)`.
    #[must_use]
    pub const fn from_keys(k0: u64, k1: u64) -> KeyHasher {
        KeyHasher { k0, k1 }
    }

    /// A hasher derived from one 64-bit seed (the simulators: the
    /// scenario seed decides placement, deterministically — L7).
    #[must_use]
    pub fn from_seed(seed: u64) -> KeyHasher {
        KeyHasher { k0: mix(seed ^ P0, P1), k1: mix(seed.rotate_left(32) ^ P2, P3) }
    }

    /// The secret (the data directory's file writes it; `INFO` never).
    #[must_use]
    pub const fn keys(&self) -> (u64, u64) {
        (self.k0, self.k1)
    }

    /// The secret's identity (ADR-0094 D6) — what a MANIFEST names.
    #[must_use]
    pub fn identity(&self) -> KeyHashId {
        KeyHashId(siphash13(self.k0, self.k1, KEY_HASH_ID_DOMAIN))
    }

    /// The hash of `key` under this secret.
    #[inline]
    #[must_use]
    pub fn hash(&self, key: &[u8]) -> u64 {
        #[cfg(feature = "collision-oracle")]
        if key.len() == 48 && key[..16] == *COLLISION_KEY_PREFIX {
            return siphash13(self.k0, self.k1, &key[..32]);
        }
        siphash13(self.k0, self.k1, key)
    }
}

impl Default for KeyHasher {
    /// A fixed secret — unit tests and models only. A node resolves its
    /// own (ADR-0094 D2); a simulator derives one from its seed.
    fn default() -> KeyHasher {
        KeyHasher::from_keys(0x1AF1_D8A5_0DB5_EED1, 0xE703_7ED1_A0B4_28DB)
    }
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
    /// ADR-0094 D6: the identity is a function of the secret alone —
    /// stable across hashers holding the same keys, distinct across
    /// secrets, and never the hash of any key (the domain is fixed and
    /// hashed directly, not through `hash`).
    #[test]
    fn identity_names_the_secret_without_being_a_key_hash() {
        use super::{KeyHashId, KeyHasher};
        let a = KeyHasher::from_keys(1, 2);
        assert_eq!(a.identity(), KeyHasher::from_keys(1, 2).identity());
        assert_ne!(a.identity(), KeyHasher::from_keys(2, 1).identity());
        assert_ne!(a.identity(), KeyHasher::from_seed(7).identity());
        assert_eq!(KeyHashId::from_u64(a.identity().to_u64()), a.identity());
        assert_eq!(a.identity().to_string().len(), 18, "{}", a.identity());
    }

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

    /// ADR-0094 K4: the reference SipHash-2-4 vector (Aumasson–Bernstein,
    /// key `00…0f`, the empty message and the 15-byte `00…0e`) pins the
    /// round-generic core.
    #[test]
    fn siphash24_reference_vectors() {
        let (k0, k1) = (0x0706_0504_0302_0100, 0x0f0e_0d0c_0b0a_0908);
        assert_eq!(siphash::<2, 4>(k0, k1, b""), 0x726f_db47_dd0e_0e31);
        let msg: Vec<u8> = (0u8..15).collect();
        assert_eq!(siphash::<2, 4>(k0, k1, &msg), 0xa129_ca61_49be_45e5);
    }

    /// ADR-0094 K4: SipHash-1-3 agrees with the standard library's
    /// `DefaultHasher` (SipHash-1-3 under the zero key) at every length
    /// class up to 200 bytes — the streaming word/tail rules included.
    #[test]
    fn siphash13_matches_the_standard_library() {
        use std::hash::Hasher;
        let mut data = Vec::new();
        for len in 0..=200usize {
            data.clear();
            data.extend((0..len).map(|i| (i as u8).wrapping_mul(31).wrapping_add(7)));
            let mut std_hasher = std::hash::DefaultHasher::new();
            std_hasher.write(&data);
            assert_eq!(siphash13(0, 0, &data), std_hasher.finish(), "len {len}");
        }
    }

    /// ADR-0094 D1: the key hash is secret-sensitive, key-sensitive, and
    /// the zero-multiplier family that collapses `hash64` for every seed
    /// spreads under it.
    #[test]
    fn key_hasher_is_keyed_and_spreads_the_hash64_zero_family() {
        let a = KeyHasher::from_seed(1);
        let b = KeyHasher::from_seed(2);
        assert_ne!(a, b);
        assert_ne!(a.hash(b"key:1"), b.hash(b"key:1"), "secret-sensitive");
        assert_ne!(a.hash(b"key:1"), a.hash(b"key:2"), "key-sensitive");
        assert_eq!(a.hash(b"key:1"), KeyHasher::from_seed(1).hash(b"key:1"), "deterministic");
        // The 16-byte family: a = P1 in `hash64`'s ≤16 path (bytes 0..4
        // and 8..12), the other eight bytes free.
        let mut key = [0u8; 16];
        key[0..4].copy_from_slice(&((P1 >> 32) as u32).to_le_bytes());
        key[8..12].copy_from_slice(&((P1 & 0xffff_ffff) as u32).to_le_bytes());
        let mut digest_seen = std::collections::HashSet::new();
        let mut keyed_seen = std::collections::HashSet::new();
        for i in 0..256u32 {
            key[4..8].copy_from_slice(&i.to_le_bytes());
            key[12..16].copy_from_slice(&i.wrapping_mul(7).to_le_bytes());
            digest_seen.insert(hash64(&key, 0x1AF1_D8A5_0DB5_EED1));
            keyed_seen.insert(a.hash(&key));
        }
        assert_eq!(digest_seen.len(), 1, "the digest's zero-multiplier family (ADR-0094)");
        assert_eq!(keyed_seen.len(), 256, "the key hash spreads it");
    }

    /// ADR-0094 D3: the oracle mode collides exactly the forced shape
    /// (48 bytes, the hashtag prefix, one 32-byte head) and nothing else.
    #[cfg(feature = "collision-oracle")]
    #[test]
    fn collision_oracle_mode_collides_only_the_forced_shape() {
        let h = KeyHasher::from_seed(9);
        let mut a = [0u8; 48];
        a[..16].copy_from_slice(COLLISION_KEY_PREFIX);
        a[16..32].copy_from_slice(&[0x11; 16]);
        let mut b = a;
        b[32..].copy_from_slice(&[0x22; 16]);
        assert_ne!(a, b);
        assert_eq!(h.hash(&a), h.hash(&b), "same 32-byte head");
        let mut c = a;
        c[20] ^= 1;
        assert_ne!(h.hash(&a), h.hash(&c), "a different head");
        assert_ne!(h.hash(&a[..47]), h.hash(&b[..47]), "47 bytes is not the shape");
        let mut d = a;
        d[0] = b'x';
        let mut e = b;
        e[0] = b'x';
        assert_ne!(h.hash(&d), h.hash(&e), "another prefix is not the shape");
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
