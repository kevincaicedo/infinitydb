//! Eviction-engine behavior oracles (M1-S06/S07): policy correctness, hot-key
//! protection, the maxmemory slack bound, and an LFU-vs-random hit-rate
//! mechanism check. Dev-tier evidence — the Redis hit-rate *parity* artifact
//! and the eviction-pressure p99.9 gate rows belong to the M1-S17 campaign
//! on the reference box (L10).

use inf_foundation::time::Nanos;
use inf_store::{
    EvictBudget, EvictionPolicy, Keyspace, OpError, PressureConfig, SetExpire, SetOptions,
    StoreConfig,
};

const NOW: Nanos = Nanos(1_000_000_000); // 1 s

fn fresh() -> Keyspace {
    // Pre-sized index: growth steps are part of `used`, so a tight slack
    // assertion wants the table at steady-state capacity from the start.
    Keyspace::new(StoreConfig {
        evict_seed: 0xE71C_7E57,
        initial_keys: 4096,
        ..StoreConfig::default()
    })
}

/// A limit that budgets the RECORD bytes to `num/den` of their current
/// level while carrying the fixed overhead (index table, wheel slots, CMS)
/// unchanged — eviction can only reclaim records, so policy tests must
/// scale against the reclaimable component.
fn records_limit(ks: &Keyspace, num: u64, den: u64) -> u64 {
    let live = ks.report().records_live_bytes;
    ks.used_bytes() - live + live * num / den
}

fn set(ks: &mut Keyspace, key: &str, len: usize) {
    ks.db_mut(0).set(key.as_bytes(), &vec![0u8; len], SetOptions::default(), NOW).expect("set");
}

fn set_ttl(ks: &mut Keyspace, key: &str, len: usize, ttl_ms: u64) {
    let opts = SetOptions {
        expire: SetExpire::At(Nanos(NOW.0 + ttl_ms * 1_000_000)),
        ..SetOptions::default()
    };
    ks.db_mut(0).set(key.as_bytes(), &vec![0u8; len], opts, NOW).expect("set");
}

fn pressure(ks: &mut Keyspace, policy: EvictionPolicy, limit: u64) {
    ks.set_pressure(PressureConfig { limit_bytes: limit, policy, samples: 5 });
}

/// Emulates the exec layer's DENYOOM gate for one write: escalate inline,
/// then write (the M1-S07 shape).
fn gated_set(ks: &mut Keyspace, key: &str, len: usize) -> Result<(), OpError> {
    if ks.over_limit() {
        ks.free_for_write(NOW)?;
    }
    set(ks, key, len);
    ks.refresh_pressure();
    Ok(())
}

#[test]
fn clock_lru_protects_hot_keys() {
    let mut ks = fresh();
    pressure(&mut ks, EvictionPolicy::AllKeysLru, 0); // tracking on, no limit yet
    for i in 0..400 {
        set(&mut ks, &format!("key:{i}"), 64);
    }
    // Hot working set: repeated GETs keep their reference bits saturated.
    let hot: Vec<String> = (0..40).map(|i| format!("key:{i}")).collect();
    for _ in 0..4 {
        for key in &hot {
            assert!(ks.db_mut(0).get(key.as_bytes(), NOW).is_some());
        }
    }
    // Age everyone else to generation 0 (the hot set gets re-touched
    // between sweeps, exactly like live traffic).
    let limit = records_limit(&ks, 1, 2);
    pressure(&mut ks, EvictionPolicy::AllKeysLru, limit);
    let mut rounds = 0;
    while ks.over_limit() && rounds < 10_000 {
        ks.evict_tick(NOW, EvictBudget { max_evictions: 8 });
        for key in &hot {
            let _ = ks.db_mut(0).get(key.as_bytes(), NOW);
        }
        rounds += 1;
    }
    assert!(ks.used_bytes() <= limit, "pressure must resolve");
    let survivors = hot.iter().filter(|k| ks.db_mut(0).get(k.as_bytes(), NOW).is_some()).count();
    assert!(
        survivors >= hot.len() * 9 / 10,
        "CLOCK must protect the hot set: {survivors}/{} survived",
        hot.len()
    );
    assert!(ks.stats().evicted_keys > 0);
}

#[test]
fn lfu_protects_frequent_keys() {
    let mut ks = fresh();
    pressure(&mut ks, EvictionPolicy::AllKeysLfu, 0);
    for i in 0..400 {
        set(&mut ks, &format!("key:{i}"), 64);
    }
    let hot: Vec<String> = (0..40).map(|i| format!("key:{i}")).collect();
    for _ in 0..32 {
        for key in &hot {
            assert!(ks.db_mut(0).get(key.as_bytes(), NOW).is_some());
        }
    }
    let limit = records_limit(&ks, 1, 2);
    pressure(&mut ks, EvictionPolicy::AllKeysLfu, limit);
    let mut rounds = 0;
    while ks.over_limit() && rounds < 10_000 {
        ks.evict_tick(NOW, EvictBudget { max_evictions: 8 });
        rounds += 1;
    }
    assert!(ks.used_bytes() <= limit, "pressure must resolve");
    let survivors = hot.iter().filter(|k| ks.db_mut(0).get(k.as_bytes(), NOW).is_some()).count();
    assert!(
        survivors >= hot.len() * 8 / 10,
        "CMS must protect the frequent set: {survivors}/{} survived",
        hot.len()
    );
}

#[test]
fn volatile_ttl_evicts_nearest_deadline_first() {
    let mut ks = fresh();
    // Three TTL bands, far in the future (nothing actually expires).
    for i in 0..30 {
        set_ttl(&mut ks, &format!("near:{i}"), 64, 10_000);
        set_ttl(&mut ks, &format!("mid:{i}"), 64, 1_000_000);
        set_ttl(&mut ks, &format!("far:{i}"), 64, 100_000_000);
    }
    let limit = records_limit(&ks, 2, 3);
    pressure(&mut ks, EvictionPolicy::VolatileTtl, limit);
    let mut rounds = 0;
    while ks.over_limit() && rounds < 10_000 {
        ks.evict_tick(NOW, EvictBudget { max_evictions: 4 });
        rounds += 1;
    }
    let near_alive =
        (0..30).filter(|i| ks.db_mut(0).exists(format!("near:{i}").as_bytes(), NOW)).count();
    let far_alive =
        (0..30).filter(|i| ks.db_mut(0).exists(format!("far:{i}").as_bytes(), NOW)).count();
    assert!(
        near_alive < far_alive,
        "volatile-ttl must prefer near deadlines: near {near_alive} vs far {far_alive}"
    );
}

#[test]
fn volatile_random_never_touches_persistent_keys() {
    let mut ks = fresh();
    for i in 0..100 {
        set(&mut ks, &format!("keep:{i}"), 64);
        set_ttl(&mut ks, &format!("vol:{i}"), 64, 1_000_000);
    }
    let limit = records_limit(&ks, 3, 4);
    pressure(&mut ks, EvictionPolicy::VolatileRandom, limit);
    let mut rounds = 0;
    while ks.over_limit() && rounds < 10_000 {
        ks.evict_tick(NOW, EvictBudget { max_evictions: 4 });
        rounds += 1;
    }
    assert!(ks.used_bytes() <= limit);
    for i in 0..100 {
        assert!(
            ks.db_mut(0).exists(format!("keep:{i}").as_bytes(), NOW),
            "persistent key keep:{i} was evicted by a volatile policy"
        );
    }
    assert!(ks.stats().evicted_keys > 0);
}

/// The M1-S07 bound: under a sustained gated write storm, usage never
/// exceeds `limit + one-write slack` at any observation point (the inline
/// escalation frees before the write lands). Dev-tier stand-in for the
/// "RSS ≤ maxmemory + bounded slack" gate row.
#[test]
fn write_storm_holds_the_limit_with_bounded_slack() {
    let mut ks = fresh();
    const VALUE: usize = 256;
    for i in 0..200 {
        set(&mut ks, &format!("seed:{i}"), VALUE);
    }
    let limit = ks.used_bytes();
    pressure(&mut ks, EvictionPolicy::AllKeysRandom, limit);
    // One write's worth of slack: record + key + header, class-rounded up
    // (the index is pre-sized, so no growth step can join the bound).
    let slack = (VALUE + 64) as u64;
    for i in 0..2_000 {
        gated_set(&mut ks, &format!("storm:{i}"), VALUE)
            .unwrap_or_else(|e| panic!("step {i}: {e:?} used={} limit={limit}", ks.used_bytes()));
        assert!(
            ks.used_bytes() <= limit + slack,
            "step {i}: used {} exceeds limit {limit} + slack {slack}",
            ks.used_bytes()
        );
    }
    assert!(ks.stats().evicted_keys >= 1_000, "the storm must have evicted heavily");
}

/// Mechanism sanity for the M1-S06 hit-rate AC: on a hot/cold skewed trace,
/// allkeys-lfu must retain a materially better hit rate than allkeys-random
/// at the same memory. (The Redis-parity artifact is M1-S17.)
#[test]
fn lfu_beats_random_on_skewed_trace() {
    fn run(policy: EvictionPolicy) -> f64 {
        let mut ks = fresh();
        pressure(&mut ks, policy, 0);
        let keys: Vec<String> = (0..1_000).map(|i| format!("key:{i}")).collect();
        for key in &keys {
            set(&mut ks, key, 64);
        }
        let limit = records_limit(&ks, 1, 2);
        pressure(&mut ks, policy, limit);
        // Deterministic skew: 10% of keys take 90% of accesses.
        let mut x: u64 = 0x5EED_CAFE;
        let mut rand = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let (mut hits, mut total) = (0u64, 0u64);
        for step in 0..60_000u64 {
            let i = if rand() % 10 != 0 { rand() % 100 } else { 100 + rand() % 900 } as usize;
            let key = &keys[i];
            total += 1;
            if ks.db_mut(0).get(key.as_bytes(), NOW).is_some() {
                hits += 1;
            } else {
                let _ = gated_set(&mut ks, key, 64); // cache miss refill
            }
            if step % 64 == 0 {
                ks.evict_tick(NOW, EvictBudget { max_evictions: 16 });
            }
        }
        hits as f64 / total as f64
    }
    let lfu = run(EvictionPolicy::AllKeysLfu);
    let random = run(EvictionPolicy::AllKeysRandom);
    assert!(
        lfu > random + 0.03,
        "allkeys-lfu hit rate {lfu:.3} must beat allkeys-random {random:.3} on a skewed trace"
    );
}

/// Evicting a TTL'd key leaves its wheel entry stale-tolerant (M1-S04
/// interplay): the entry fires as a counted no-op, never a misfire.
#[test]
fn eviction_and_wheel_stay_consistent() {
    let mut ks = fresh();
    for i in 0..100 {
        set_ttl(&mut ks, &format!("vol:{i}"), 64, 5_000);
    }
    let limit = records_limit(&ks, 1, 2);
    pressure(&mut ks, EvictionPolicy::VolatileLru, limit);
    let mut rounds = 0;
    while ks.over_limit() && rounds < 10_000 {
        ks.evict_tick(NOW, EvictBudget { max_evictions: 8 });
        rounds += 1;
    }
    let evicted = ks.stats().evicted_keys;
    assert!(evicted > 0);
    // Advance past every deadline: surviving keys expire exactly once;
    // evicted keys' wheel entries resolve as stale no-ops.
    let later = Nanos(NOW.0 + 10_000 * 1_000_000);
    let mut guard = 0;
    loop {
        let s = ks.expire_tick(later, inf_store::ExpiryBudget::default());
        if s.lag_ms == 0 && s.reaped == 0 && s.stale == 0 {
            break;
        }
        guard += 1;
        assert!(guard < 100_000, "expiry must drain");
    }
    let stats = ks.stats();
    assert_eq!(stats.expired_active + stats.expired_lazy + evicted, 100, "census closes exactly");
    assert_eq!(ks.db_mut(0).len(), 0);
}
