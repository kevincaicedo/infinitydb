//! M4-S05 budget bench (§4.1): mutable-region mutation cost vs the M3
//! arena path — "SET/`JSON.NUMINCRBY` within 1% of M3 baseline above the
//! boundary". Three rows, each arena-vs-address-space with identical
//! record work so the delta isolates the address arithmetic S05 changes
//! (the resolver-bench methodology applied to the write path):
//!
//! - `in-place rewrite` — the exact-fit SET lane: full record image
//!   rewritten at a stable address.
//! - `scalar patch` — the `JSON.NUMINCRBY` lane (ADR-0043 shape): 8 value
//!   bytes patched + the u24 version bumped in place, no record rewrite.
//! - `relocate` — the size-changing SET lane: arena alloc-copy-free with
//!   an index repoint vs tiered copy-to-tail (S06), amortized over ring
//!   drains / arena reuse.
//!
//! The command-level SET row (full `CellStore::set` layers) joins the
//! campaign when tiered namespaces are command-wired (S22) — this bench
//! proves the substrate, not the command.
//!
//! Custom harness (the `store`/`resolver` bench precedent): steady-state
//! sweeps, best-of-rounds, shuffled key order.
//!
//! Run: `taskset -c 4 cargo bench -p inf-store --bench mutation`
//! Artifact: 3–5 replicates recorded under `.artifacts/m4/s05/`.

use std::hint::black_box;
use std::time::Instant;

use inf_alloc::{Arena, ArenaAddr, ArenaConfig};
use inf_store::KeyHasher;
use inf_store::{AddressSpace, AddressSpaceConfig, DemotionConfig, LogicalAddr, TieredTable};

const KEY_LEN: usize = 16;
const VAL_LEN: usize = 64;
/// v0 record layout, TTL-less: 8 B header + key + value (§7.2). Verified
/// against a real table-written record in `main` before any row runs.
const RECORD_LEN: usize = 8 + KEY_LEN + VAL_LEN;
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

/// Writes the v0 string-record image (no TTL) — the bench-local layout
/// twin of `RecordSpec::write`, asserted equal to a real record once.
fn write_record_image(buf: &mut [u8], key: &[u8], value: &[u8], version: u32) {
    buf[0] = 1 << 4; // TypeTag::String, no flags
    buf[1] = key.len() as u8;
    let vlen = (value.len() as u32).to_le_bytes();
    buf[2..5].copy_from_slice(&vlen[..3]);
    let ver = (version & 0xFF_FFFF).to_le_bytes();
    buf[5..8].copy_from_slice(&ver[..3]);
    buf[8..8 + key.len()].copy_from_slice(key);
    buf[8 + key.len()..].copy_from_slice(value);
}

fn key_of(i: usize) -> [u8; KEY_LEN] {
    let mut key = [b'k'; KEY_LEN];
    key[8..].copy_from_slice(&(i as u64).to_le_bytes());
    key
}

struct Rig {
    arena: Arena,
    arena_addrs: Vec<ArenaAddr>,
    space: AddressSpace,
    logical_addrs: Vec<LogicalAddr>,
}

fn rig(n: usize) -> Rig {
    let mut arena = Arena::new(ArenaConfig::default());
    let mut arena_addrs = Vec::with_capacity(n);
    let ring = (n * RECORD_LEN * 2).next_power_of_two();
    let mut space = AddressSpace::new(AddressSpaceConfig {
        reserve_bytes: ring,
        page_bytes: 1 << 20,
        life_origin: LogicalAddr::ZERO,
    })
    .expect("reservation");
    let mut logical_addrs = Vec::with_capacity(n);
    let value = [b'v'; VAL_LEN];
    for i in 0..n {
        let key = key_of(i);
        let a = arena.alloc(RECORD_LEN).expect("arena budget");
        write_record_image(arena.bytes_mut(a, RECORD_LEN), &key, &value, 1);
        arena_addrs.push(a);
        let l = space.alloc(RECORD_LEN).expect("ring fits");
        write_record_image(space.bytes_mut(l, RECORD_LEN), &key, &value, 1);
        logical_addrs.push(l);
    }
    shuffle(&mut arena_addrs, 0xC0FFEE);
    shuffle(&mut logical_addrs, 0xC0FFEE);
    Rig { arena, arena_addrs, space, logical_addrs }
}

/// Row 1 — exact-fit SET: full record image rewrite at a stable address.
fn bench_in_place(n: usize, label: &str) {
    let mut r = rig(n);
    let value = [b'w'; VAL_LEN];
    let arena_ns = best_sweep(
        || {
            let mut sum = 0u64;
            for (i, &a) in r.arena_addrs.iter().enumerate() {
                let key = key_of(i);
                let buf = r.arena.bytes_mut(a, RECORD_LEN);
                write_record_image(buf, &key, &value, 2);
                sum += u64::from(buf[8]);
            }
            sum
        },
        n,
    );
    let tiered_ns = best_sweep(
        || {
            let mut sum = 0u64;
            for (i, &a) in r.logical_addrs.iter().enumerate() {
                let key = key_of(i);
                let buf = r.space.bytes_mut(a, RECORD_LEN);
                write_record_image(buf, &key, &value, 2);
                sum += u64::from(buf[8]);
            }
            sum
        },
        n,
    );
    println!(
        "in-place rewrite  {label:<22} arena {arena_ns:6.2} ns/op | tiered {tiered_ns:6.2} ns/op \
         (delta {:+.2})",
        tiered_ns - arena_ns
    );
}

/// Row 2 — `JSON.NUMINCRBY` shape: 8 value bytes + u24 version, in place.
fn bench_scalar_patch(n: usize, label: &str) {
    let mut r = rig(n);
    let patch_at = 8 + KEY_LEN; // first value byte
    let arena_ns = best_sweep(
        || {
            let mut sum = 0u64;
            for &a in &r.arena_addrs {
                let buf = r.arena.bytes_mut(a, RECORD_LEN);
                buf[patch_at..patch_at + 8]
                    .copy_from_slice(&0x4242_4242_4242_4242u64.to_le_bytes());
                let version = u32::from_le_bytes([buf[5], buf[6], buf[7], 0]).wrapping_add(1);
                buf[5..8].copy_from_slice(&version.to_le_bytes()[..3]);
                sum += u64::from(buf[5]);
            }
            sum
        },
        n,
    );
    let tiered_ns = best_sweep(
        || {
            let mut sum = 0u64;
            for &a in &r.logical_addrs {
                let buf = r.space.bytes_mut(a, RECORD_LEN);
                buf[patch_at..patch_at + 8]
                    .copy_from_slice(&0x4242_4242_4242_4242u64.to_le_bytes());
                let version = u32::from_le_bytes([buf[5], buf[6], buf[7], 0]).wrapping_add(1);
                buf[5..8].copy_from_slice(&version.to_le_bytes()[..3]);
                sum += u64::from(buf[5]);
            }
            sum
        },
        n,
    );
    println!(
        "scalar patch      {label:<22} arena {arena_ns:6.2} ns/op | tiered {tiered_ns:6.2} ns/op \
         (delta {:+.2})",
        tiered_ns - arena_ns
    );
}

/// Row 3 — size-changing SET: arena alloc-copy-free + index repoint vs
/// tiered copy-to-tail (`TieredTable::update`, the real S05/S06 entry),
/// ring drains amortized into the sweep exactly like arena free-list
/// reuse is.
fn bench_relocate(n: usize, label: &str) {
    use inf_store::{Index, MemoryMode};
    // Arena side: alternate value sizes across class boundaries so every
    // op reallocates (the M3 realloc lane).
    let small = [b'v'; VAL_LEN];
    let large = [b'V'; VAL_LEN + 40];
    let small_len = RECORD_LEN;
    let large_len = RECORD_LEN + 40;
    let mut arena = Arena::new(ArenaConfig::default());
    let mut index: Index<MemoryMode> = Index::with_capacity(n * 2);
    let mut entries: Vec<(u64, ArenaAddr, usize)> = Vec::with_capacity(n);
    for i in 0..n {
        let key = key_of(i);
        let hash = KeyHasher::default().hash(&key);
        let addr = arena.alloc(small_len).expect("arena budget");
        write_record_image(arena.bytes_mut(addr, small_len), &key, &small, 1);
        index.insert(hash, addr);
        entries.push((hash, addr, small_len));
    }
    let arena_ns = best_sweep(
        || {
            let mut sum = 0u64;
            for (i, entry) in entries.iter_mut().enumerate() {
                let key = key_of(i);
                let (value, new_len): (&[u8], usize) =
                    if entry.2 == small_len { (&large, large_len) } else { (&small, small_len) };
                let new_addr = arena.alloc(new_len).expect("arena budget");
                write_record_image(arena.bytes_mut(new_addr, new_len), &key, value, 2);
                index.replace(entry.0, entry.1, new_addr);
                arena.free(entry.1, entry.2);
                *entry = (entry.0, new_addr, new_len);
                sum += new_len as u64;
            }
            sum
        },
        n,
    );
    // Tiered side: the real routed entry; every op copies to the tail.
    // Ring sized so drains stay rare; drain cost is part of the sweep.
    let ring = (n * large_len * 8).next_power_of_two();
    let mut table = TieredTable::new(
        AddressSpaceConfig {
            reserve_bytes: ring,
            page_bytes: 1 << 20,
            life_origin: LogicalAddr::ZERO,
        },
        DemotionConfig::for_budget(ring as u64, 1 << 20),
        n * 2,
        KeyHasher::default(),
    )
    .expect("reservation");
    let mut tiered: Vec<(u64, LogicalAddr, usize, u32)> = Vec::with_capacity(n);
    for i in 0..n {
        let key = key_of(i);
        let hash = KeyHasher::default().hash(&key);
        let addr = table.insert(&key, &small, hash).expect("fits");
        tiered.push((hash, addr, small_len, 0));
    }
    let tiered_ns = best_sweep(
        || {
            let mut sum = 0u64;
            for (i, entry) in tiered.iter_mut().enumerate() {
                let key = key_of(i);
                let value: &[u8] = if entry.2 == small_len { &large } else { &small };
                let placed = match table.update(&key, value, entry.0, entry.1, entry.2, entry.3) {
                    Ok(placed) => placed,
                    Err(_) => {
                        // Ring window full: release everything below the
                        // tail (the drain S07 later paces) and retry.
                        let tail = table.space().tail();
                        table.space_mut().advance_ro_boundary(tail);
                        table.space_mut().advance_flushed(tail);
                        table.space_mut().advance_head(tail);
                        table
                            .update(&key, value, entry.0, entry.1, entry.2, entry.3)
                            .expect("fits after drain")
                    }
                };
                let new_len = 8 + KEY_LEN + value.len();
                *entry = (entry.0, placed, new_len, entry.3.wrapping_add(1) & 0xFF_FFFF);
                sum += new_len as u64;
            }
            sum
        },
        n,
    );
    println!(
        "relocate          {label:<22} arena {arena_ns:6.2} ns/op | tiered {tiered_ns:6.2} ns/op \
         (delta {:+.2})",
        tiered_ns - arena_ns
    );
}

fn main() {
    // Pair-assert the bench-local record image against a real table write
    // before trusting any row (the layout-twin check).
    let mut probe = TieredTable::new(
        AddressSpaceConfig {
            reserve_bytes: 1 << 16,
            page_bytes: 1 << 12,
            life_origin: LogicalAddr::ZERO,
        },
        DemotionConfig::for_budget(1 << 16, 1 << 12),
        64,
        KeyHasher::default(),
    )
    .expect("reservation");
    let key = key_of(7);
    let value = [b'v'; VAL_LEN];
    let addr = probe.insert(&key, &value, KeyHasher::default().hash(&key)).expect("fits");
    let mut image = vec![0u8; RECORD_LEN];
    write_record_image(&mut image, &key, &value, 0);
    assert_eq!(probe.record_bytes(addr, RECORD_LEN), &image[..], "layout twin drifted");

    println!("--- M4-S05 mutation budget bench (within 1% of the M3 arena path) ---");
    for (n, label) in [(32 << 10, "32K records (cache-hot)"), (1 << 20, "1M records (miss-bound)")]
    {
        bench_in_place(n, label);
        bench_scalar_patch(n, label);
        bench_relocate(n, label);
    }
}
