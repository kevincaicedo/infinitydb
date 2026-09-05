#![allow(
    clippy::disallowed_methods,
    reason = "bench target: the wall clock is the instrument, not cell code"
)]
//! M4-S01 budget bench (§4.1): resolver cost on RAM-resident addresses vs
//! the M3 arena-offset path — budget ≤ 2 ns added. The tiered read is
//! `resolve` (two watermark compares) + ring `bytes` (mask + add + deref);
//! the arena read is chunk-table load + base + offset + deref. Both rows
//! touch one byte per record so the memory traffic is identical and the
//! delta isolates address arithmetic.
//!
//! Custom harness (the `store` bench precedent): steady-state sweeps over
//! shuffled address arrays at a cache-resident and a miss-bound set size.
//!
//! Run: `taskset -c 4 cargo bench -p inf-store --bench resolver`
//! Artifact: 3–5 replicates recorded under `.artifacts/m4/s01/`.

use std::hint::black_box;
use std::time::Instant;

use inf_alloc::{Arena, ArenaAddr, ArenaConfig};
use inf_store::{AddrClass, AddressSpace, AddressSpaceConfig, LogicalAddr};

const RECORD_LEN: usize = 64;
const ROUNDS: usize = 30;

fn shuffle<T>(items: &mut [T], mut seed: u64) {
    for i in (1..items.len()).rev() {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        items.swap(i, (seed % (i as u64 + 1)) as usize);
    }
}

/// ns/op for one full sweep, best of `ROUNDS` (steady-state floor).
fn best_sweep(mut sweep: impl FnMut() -> u64, ops: usize) -> f64 {
    let mut best = f64::MAX;
    for _ in 0..ROUNDS {
        let t = Instant::now();
        black_box(sweep());
        let ns = t.elapsed().as_nanos() as f64 / ops as f64;
        if ns < best {
            best = ns;
        }
    }
    best
}

fn bench_set(n: usize, label: &str) {
    // M3 baseline: records in the arena, addresses are arena offsets.
    let mut arena = Arena::new(ArenaConfig::default());
    let mut arena_addrs: Vec<ArenaAddr> = Vec::with_capacity(n);
    for i in 0..n {
        let addr = arena.alloc(RECORD_LEN).expect("arena budget");
        arena.bytes_mut(addr, RECORD_LEN).fill(i as u8);
        arena_addrs.push(addr);
    }
    // M4 tiered space sized so the whole set is RAM-resident (mutable).
    let ring = (n * RECORD_LEN * 2).next_power_of_two();
    let mut space = AddressSpace::new(AddressSpaceConfig {
        reserve_bytes: ring,
        page_bytes: 1 << 20,
        life_origin: LogicalAddr::ZERO,
    })
    .expect("reservation");
    let mut logical_addrs: Vec<LogicalAddr> = Vec::with_capacity(n);
    for i in 0..n {
        let addr = space.alloc(RECORD_LEN).expect("ring fits the set");
        space.bytes_mut(addr, RECORD_LEN).fill(i as u8);
        logical_addrs.push(addr);
    }
    shuffle(&mut arena_addrs, 0xC0FFEE);
    shuffle(&mut logical_addrs, 0xC0FFEE);

    let arena_ns = best_sweep(
        || arena_addrs.iter().map(|&a| u64::from(arena.bytes(a, RECORD_LEN)[0])).sum(),
        n,
    );
    let tiered_ns = best_sweep(
        || {
            logical_addrs
                .iter()
                .map(|&a| {
                    debug_assert_eq!(space.resolve(a), AddrClass::Mutable);
                    let class_is_ram = !matches!(space.resolve(a), AddrClass::Cold);
                    u64::from(class_is_ram) + u64::from(space.bytes(a, RECORD_LEN)[0])
                })
                .sum()
        },
        n,
    );
    let resolve_ns = best_sweep(|| logical_addrs.iter().map(|&a| space.resolve(a) as u64).sum(), n);

    println!(
        "{label:<26} arena {arena_ns:6.2} ns/op | resolve+read {tiered_ns:6.2} ns/op \
         (delta {:+.2}) | resolve-only {resolve_ns:5.2} ns/op",
        tiered_ns - arena_ns
    );
}

fn main() {
    println!("--- M4-S01 resolver budget bench (≤ 2 ns added on RAM addresses) ---");
    bench_set(32 << 10, "32K records (cache-hot)");
    bench_set(4 << 20, "4M records (miss-bound)");
}
