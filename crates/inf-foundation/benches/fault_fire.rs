#![allow(
    clippy::disallowed_methods,
    reason = "bench target: the wall clock is the instrument, not cell code"
)]
//! M2-S16 zero-cost A/B: the per-call cost of `fault::fire` in both
//! build tiers.
//!
//!   leg A (default, the shipping tier):  cargo bench -p inf-foundation --bench fault_fire
//!   leg B (test tier, armed-empty):      … --features fault-points
//!
//! Leg A must measure indistinguishable from the empty loop (fire is a
//! `const false` the optimizer erases — the compiled-out contract). Leg B
//! prices the test tier's thread-local read + branch. Context: the
//! hottest production site is one `fire` per committed frame (≥ 32 KiB
//! of log), so even leg B's cost per byte is noise. The companion
//! artifact greps the release `infinityd` binary for the injected-fault
//! strings (absence = the machinery, including panic paths, was
//! stripped).

use std::hint::black_box;
use std::time::Instant;

use inf_foundation::fault;

const ITERS: u64 = 200_000_000;

fn main() {
    // Baseline: the measurement loop itself.
    let started = Instant::now();
    let mut acc = 0u64;
    for i in 0..ITERS {
        acc = acc.wrapping_add(black_box(i));
    }
    let empty_ns = started.elapsed().as_nanos() as f64 / ITERS as f64;

    // fire() on an unarmed point, in whatever tier this build carries —
    // a const point name, exactly like every production site.
    let started = Instant::now();
    for i in 0..ITERS {
        if fault::fire("bench_point") {
            acc = acc.wrapping_add(1);
        }
        acc = acc.wrapping_add(black_box(i));
    }
    let fire_ns = started.elapsed().as_nanos() as f64 / ITERS as f64;

    let tier = if cfg!(feature = "fault-points") { "fault-points ON (unarmed)" } else { "OFF" };
    println!("tier: {tier}");
    println!("empty loop: {empty_ns:.3} ns/iter");
    println!("fire loop:  {fire_ns:.3} ns/iter");
    println!("fire cost:  {:.3} ns/call (delta)", (fire_ns - empty_ns).max(0.0));
    black_box(acc);
}
