//! Review harness: Redis semantic conformance of `CellStore::get_range`.
//!
//! Goal and method: `GETRANGE` is declared `full` in the CI-enforced compat
//! matrix, so its behavior must be contract-equivalent to Redis. This test
//! encodes Redis 8.0.5's actual index rule — which has a pre-clamp early
//! return for a wholly-negative inverted range that InfinityDB's clamp-then-
//! compare form lacks — and drives `get_range` over the boundary directly.
//! The oracle was established by byte-diffing a live `redis-server 8.0.5`.

use inf_foundation::time::Nanos;
use inf_store::{CellStore, SetOptions, StoreConfig};

fn now() -> Nanos {
    Nanos(1_000_000)
}

fn store_with(value: &[u8]) -> CellStore {
    let mut store = CellStore::new(StoreConfig::default());
    store.set(b"s", value, SetOptions::default(), now()).expect("set");
    store
}

/// Redis `getrangeCommand` (t_string.c), transcribed. The first clause is the
/// one InfinityDB is missing: a range whose two indices are BOTH negative and
/// inverted returns empty *before* any clamping happens.
fn redis_get_range<'a>(value: &'a [u8], start: i64, end: i64) -> &'a [u8] {
    let n = value.len() as i64;
    if start < 0 && end < 0 && start > end {
        return b"";
    }
    let from = if start < 0 { (n + start).max(0) } else { start };
    let to = (if end < 0 { (n + end).max(0) } else { end }).min(n - 1);
    if n == 0 || from > to || from >= n {
        return b"";
    }
    &value[from as usize..=to as usize]
}

#[test]
fn rv_f_l00_06_getrange_wholly_negative_inverted_range_returns_data() {
    // The minimal witness: an 11-byte value with both indices below -len.
    let mut store = store_with(b"Hello World");
    let got = store.get_range(b"s", -100, -200, now()).expect("string");
    assert_eq!(
        got,
        redis_get_range(b"Hello World", -100, -200),
        "GETRANGE s -100 -200: InfinityDB returned {:?}, Redis 8.0.5 returns b\"\"",
        got
    );
}

#[test]
fn rv_f_l00_06_getrange_matches_redis_over_the_whole_index_grid() {
    // Sweep every index pair in [-14, 14] against the transcribed oracle for
    // several value lengths, so the report can state the exact divergent set.
    let mut divergences = Vec::new();
    for value in [b"".as_slice(), b"a", b"ab", b"Hello World"] {
        let mut store = store_with(value);
        for start in -14i64..=14 {
            for end in -14i64..=14 {
                let got = store.get_range(b"s", start, end, now()).expect("string").to_vec();
                let want = redis_get_range(value, start, end);
                if got != want {
                    divergences.push(format!(
                        "len={} start={start} end={end}: inf={:?} redis={:?}",
                        value.len(),
                        String::from_utf8_lossy(&got),
                        String::from_utf8_lossy(want)
                    ));
                }
            }
        }
    }
    assert!(
        divergences.is_empty(),
        "{} GETRANGE index pairs diverge from Redis 8.0.5:\n{}",
        divergences.len(),
        divergences.join("\n")
    );
}
