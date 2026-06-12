//! M1-E2 acceptance shapes: the TTL wheel + budgeted expiry slices.
//!
//! - Active expiry works with ZERO reads (the wheel, not the lazy path).
//! - Wheel-vs-model oracle under random op/tick interleavings: after any
//!   caught-up slice, visible state equals the reference model exactly.
//! - Storm protection (M1-S05): a same-millisecond mass expiry drains in
//!   bounded slices, never one giant pause, and the `expiry_debt` lag hits 0.
//! - Virtual-time DST shape (M1-S04 AC): deadlines across 48 simulated
//!   hours, every expiry fires exactly at catch-up, none early, none missed.
//!   `INF_DST_FULL=1` runs the full 10M-key campaign (CI default: 200k).

use std::collections::HashMap;

use inf_foundation::time::Nanos;
use inf_store::{CellStore, ExpireCond, ExpiryBudget, SetExpire, SetOptions, StoreConfig};

fn ms(v: u64) -> Nanos {
    Nanos(v * 1_000_000)
}

fn set_with_ttl(store: &mut CellStore, key: &[u8], deadline_ms: u64, now: Nanos) {
    let opts = SetOptions { expire: SetExpire::At(ms(deadline_ms)), ..Default::default() };
    store.set(key, b"v", opts, now).expect("set");
}

/// Tick until the wheel catches up to `now`; returns total reaped.
fn drain(store: &mut CellStore, now: Nanos) -> u64 {
    let mut reaped = 0;
    loop {
        let stats = store.expire_tick(now, ExpiryBudget { max_fires: 1024, max_steps: 1 << 20 });
        reaped += stats.reaped;
        if stats.lag_ms == 0 {
            return reaped;
        }
    }
}

#[test]
fn active_wheel_reaps_without_any_reads() {
    let mut store = CellStore::new(StoreConfig::default());
    let t0 = ms(1);
    for i in 0..1_000u32 {
        let key = format!("k:{i}");
        // Deadlines spread over [100, 1100) ms.
        set_with_ttl(&mut store, key.as_bytes(), 100 + u64::from(i), t0);
    }
    store.set(b"immortal", b"v", SetOptions::default(), t0).expect("set");
    assert_eq!(store.len(), 1001);

    // Before any deadline: a caught-up tick reaps nothing (never early).
    assert_eq!(drain(&mut store, ms(99)), 0);
    assert_eq!(store.len(), 1001);

    // Halfway: exactly the due half is gone — via the wheel alone.
    let reaped = drain(&mut store, ms(599));
    assert_eq!(reaped, 500, "deadlines 100..=599");
    assert_eq!(store.len(), 501);

    // Far side: everything TTL'd is gone, the immortal key survives.
    drain(&mut store, ms(10_000));
    assert_eq!(store.len(), 1);
    let stats = store.stats();
    assert_eq!(stats.expired_active, 1000);
    assert_eq!(stats.expired_lazy, 0, "no read ever ran");
    assert_eq!(stats.ttl_live, 0);
}

#[test]
fn wheel_matches_reference_model_under_churn() {
    let ops: usize = if cfg!(miri) { 2_000 } else { 60_000 };
    let mut store = CellStore::new(StoreConfig::default());
    // key → deadline_ms (None = no TTL)
    let mut model: HashMap<Vec<u8>, Option<u64>> = HashMap::new();
    let mut x: u64 = 0xD15E_A5ED_C0FF_EE00;
    let mut rand = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let mut now_ms: u64 = 10;
    for op in 0..ops {
        now_ms += rand() % 20;
        let now = ms(now_ms);
        // Purge the model of expired entries before mutating against it.
        model.retain(|_, deadline| deadline.is_none_or(|d| d > now_ms));
        let key = format!("key:{}", rand() % 256).into_bytes();
        match rand() % 5 {
            0 => {
                store.set(&key, b"v", SetOptions::default(), now).expect("set");
                model.insert(key, None);
            }
            1 => {
                let deadline = now_ms + 1 + rand() % 5_000;
                set_with_ttl(&mut store, &key, deadline, now);
                model.insert(key, Some(deadline));
            }
            2 => {
                let got = store.del(&key, now);
                let want = model.remove(&key).is_some();
                assert_eq!(got, want, "op {op}: DEL disagreed");
            }
            3 => {
                let deadline = now_ms + 1 + rand() % 2_000;
                let got = store.expire(&key, Some(ms(deadline)), ExpireCond::Always, now);
                let want = model.contains_key(&key);
                assert_eq!(got, want, "op {op}: EXPIRE disagreed");
                if want {
                    model.insert(key, Some(deadline));
                }
            }
            _ => {
                // A budget-bounded slice at a random moment.
                store.expire_tick(now, ExpiryBudget { max_fires: 32, max_steps: 512 });
            }
        }
    }
    // Catch up fully: visible state must equal the model exactly.
    now_ms += 1;
    drain(&mut store, ms(now_ms));
    model.retain(|_, deadline| deadline.is_none_or(|d| d > now_ms));
    assert_eq!(store.len(), model.len(), "live census after catch-up");
    let final_now = ms(now_ms);
    for (key, _) in model {
        assert!(store.get(&key, final_now).is_some(), "model key missing: {key:?}");
    }
}

#[test]
fn same_second_storm_drains_in_bounded_slices() {
    let keys: u64 = if cfg!(miri) { 2_000 } else { 100_000 };
    let mut store = CellStore::new(StoreConfig::default());
    let t0 = ms(1);
    for i in 0..keys {
        let key = format!("storm:{i}");
        set_with_ttl(&mut store, key.as_bytes(), 1_000, t0); // same millisecond
    }
    // Live foreground traffic continues against non-TTL keys.
    store.set(b"fg", b"v", SetOptions::default(), t0).expect("set");

    let after = ms(1_500);
    let budget = ExpiryBudget::default(); // 64 fires per slice
    let mut slices = 0u64;
    let mut total = 0u64;
    loop {
        let stats = store.expire_tick(after, budget);
        assert!(
            stats.reaped + stats.stale <= u64::from(budget.max_fires),
            "slice exceeded its fire budget"
        );
        total += stats.reaped;
        slices += 1;
        // Foreground stays serviceable mid-storm (the no-cliff property in
        // miniature: each slice is small, reads run between slices).
        assert!(store.get(b"fg", after).is_some());
        if stats.lag_ms == 0 && stats.reaped == 0 {
            break;
        }
        assert!(slices < keys, "storm failed to drain");
    }
    assert_eq!(total, keys, "every storm key reaped");
    assert!(slices >= keys / u64::from(budget.max_fires), "drained in bounded slices");
    assert_eq!(store.len(), 1);
}

#[test]
fn dst_virtual_time_48h_campaign() {
    let full = std::env::var("INF_DST_FULL").is_ok_and(|v| v == "1");
    let keys: u64 = if cfg!(miri) {
        1_000
    } else if full {
        10_000_000
    } else {
        200_000
    };
    const HOURS_48_MS: u64 = 48 * 3600 * 1000;
    let mut store = CellStore::new(StoreConfig::default());
    let t0 = ms(1);
    let mut x: u64 = 0x48_4F_55_52;
    let mut rand = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    // Deadlines uniform across 48 h; sorted census via bucket counts.
    const BUCKETS: usize = 1 << 12;
    let bucket_width = HOURS_48_MS / BUCKETS as u64 + 1;
    let mut due_by_bucket = [0u64; BUCKETS];
    for i in 0..keys {
        let deadline = 2 + rand() % HOURS_48_MS;
        let key = format!("dst:{i}");
        set_with_ttl(&mut store, key.as_bytes(), deadline, t0);
        due_by_bucket[(deadline / bucket_width) as usize] += 1;
    }
    assert_eq!(store.stats().ttl_live, keys);

    // Walk virtual time in random bucket strides; sample each census at
    // `bucket·width − 1` — the last instant where exactly the buckets below
    // are due (expiry is INCLUSIVE at the deadline millisecond, so sampling
    // ON a boundary would under-count by the boundary keys; the full 10M
    // campaign caught exactly that harness off-by-one).
    let mut bucket = 0usize;
    while bucket < BUCKETS {
        bucket = (bucket + 1 + (rand() % 24) as usize).min(BUCKETS);
        let now_ms = bucket as u64 * bucket_width - 1;
        drain(&mut store, ms(now_ms));
        let expected_gone: u64 = due_by_bucket[..bucket].iter().sum();
        assert_eq!(
            store.len() as u64,
            keys - expected_gone,
            "census diverged at bucket {bucket} (t={now_ms}ms)"
        );
    }
    drain(&mut store, ms(HOURS_48_MS + 2));
    assert_eq!(store.len(), 0, "every TTL fired");
    let stats = store.stats();
    assert_eq!(stats.expired_active, keys);
    assert_eq!(stats.wheel_fallback, 0, "pool never overflowed");
}

#[test]
fn wheel_memory_stays_within_sixteen_bytes_per_ttl_key() {
    let keys: u64 = if cfg!(miri) { 500 } else { 100_000 };
    let mut store = CellStore::new(StoreConfig::default());
    let t0 = ms(1);
    let baseline = store.report().wheel_bytes;
    for i in 0..keys {
        let key = format!("ttl:{i}");
        set_with_ttl(&mut store, key.as_bytes(), 1_000_000 + i, t0);
    }
    let report = store.report();
    // 16 B/entry exactly, plus Vec growth slack (< 2×) and the fixed slot
    // table — the M1-S04 attribution AC shape at dev tier.
    let pool = report.wheel_bytes - baseline;
    assert!(pool >= keys * 16, "pool under-reports: {pool}");
    assert!(pool <= keys * 16 * 2, "wheel pool exceeds 16 B/key + growth slack: {pool}");
}
