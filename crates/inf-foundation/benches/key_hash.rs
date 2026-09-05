#![allow(
    clippy::disallowed_methods,
    reason = "bench target: the wall clock is the instrument, not cell code"
)]
//! ADR-0094 D4 — the key hash's per-key cost, both functions in one run:
//! `hash64` (the M0 digest fold the index used to place keys with) and
//! `KeyHasher::hash` (SipHash-1-3 under a secret, the key hash now), at
//! the key lengths the compat corpus and the gate rows use. Same box,
//! same loop, interleaved legs; the number the ledger discloses is the
//! per-key delta at 16 and 32 bytes (Correctness-only: the security
//! argument ships the change, the cost is measured, not accepted).
//!
//!   cargo bench -p inf-foundation --bench key_hash

use std::hint::black_box;
use std::time::Instant;

use inf_foundation::{KeyHasher, hash64};

const ITERS: u64 = 20_000_000;
const LENGTHS: [usize; 6] = [8, 16, 32, 48, 64, 128];

fn leg(name: &str, len: usize, f: impl Fn(&[u8]) -> u64) -> f64 {
    let key: Vec<u8> = (0..len).map(|i| (i as u8).wrapping_mul(29).wrapping_add(3)).collect();
    // Warm.
    let mut acc = 0u64;
    for _ in 0..ITERS / 10 {
        acc ^= f(black_box(&key));
    }
    let started = Instant::now();
    for _ in 0..ITERS {
        acc ^= f(black_box(&key));
    }
    let ns = started.elapsed().as_nanos() as f64 / ITERS as f64;
    black_box(acc);
    println!("{name:>10} len {len:>4}: {ns:6.2} ns/key");
    ns
}

fn main() {
    let hasher = KeyHasher::from_seed(0xC0FFEE);
    println!("key_hash A/B (ADR-0094 D4): {ITERS} iterations per leg, legs interleaved");
    for &len in &LENGTHS {
        // Interleaved: A, B, A, B — the median of two per arm is what
        // the ledger quotes; the raw lines are the artifact.
        let a1 = leg("hash64", len, |k| hash64(k, 0x1AF1_D8A5_0DB5_EED1));
        let b1 = leg("siphash13", len, |k| hasher.hash(k));
        let a2 = leg("hash64", len, |k| hash64(k, 0x1AF1_D8A5_0DB5_EED1));
        let b2 = leg("siphash13", len, |k| hasher.hash(k));
        let a = f64::midpoint(a1, a2);
        let b = f64::midpoint(b1, b2);
        println!("   delta len {len:>4}: {:+6.2} ns/key ({:.2} x)", b - a, b / a);
    }
}
