//! Eviction-engine behavior oracles (M1-S06/S07): policy correctness, hot-key
//! protection, the maxmemory slack bound, and an LFU-vs-random hit-rate
//! mechanism check. Dev-tier evidence — the Redis hit-rate *parity* artifact
//! and the eviction-pressure p99.9 gate rows belong to the M1-S17 campaign
//! on the reference box (L10).

use inf_foundation::time::Nanos;
use inf_store::{
    EvictBudget, EvictionPolicy, Keyspace, NsId, NsMode, NsSpec, OpError, PressureConfig,
    SetExpire, SetOptions, StoreConfig,
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

// ---- M4-S27 (ADR-0068): named memory-namespace enforcement --------------------

fn memory_ns(id: u32, name: &[u8], policy: Option<EvictionPolicy>) -> NsSpec {
    NsSpec {
        id: NsId(id),
        name: name.to_vec(),
        mode: NsMode::Memory,
        fsync: None,
        policy,
        maxmemory: None,
        tier: None,
    }
}

fn ns_set(ks: &mut Keyspace, id: u32, key: &str, len: usize) {
    ks.ns_store_mut(NsId(id))
        .expect("registered")
        .set(key.as_bytes(), &vec![0u8; len], SetOptions::default(), NOW)
        .expect("set");
    ks.refresh_pressure();
}

/// Emulates the exec layer's namespace-scoped DENYOOM gate (ADR-0068 D4)
/// for one write into a budgeted named memory namespace.
fn ns_gated_set(ks: &mut Keyspace, id: u32, key: &str, len: usize) -> Result<(), OpError> {
    if let Some(verdict) = ks.ns_free_for_write(NsId(id), NOW) {
        verdict?;
    }
    ns_set(ks, id, key, len);
    Ok(())
}

/// The M1-S05/S07 write storm re-targeted at a named memory namespace
/// (ADR-0068 D2 budget leg): under a sustained gated storm the namespace
/// never exceeds its own budget + one-write slack, reclaims from its own
/// keys only, and the numbered dbs are never disturbed — with **no**
/// node-level `maxmemory` set at all.
#[test]
fn named_ns_storm_holds_its_own_budget_and_leaves_db0_alone() {
    let mut ks = fresh();
    for i in 0..100 {
        set(&mut ks, &format!("db0:{i}"), 128);
    }
    const VALUE: usize = 256;
    ks.ns_create(memory_ns(16, b"cache", Some(EvictionPolicy::AllKeysRandom))).expect("create");
    for i in 0..200 {
        ns_set(&mut ks, 16, &format!("seed:{i}"), VALUE);
    }
    let budget = ks.ns_store(NsId(16)).expect("live").used_bytes();
    ks.ns_set_memory(b"cache", Some(EvictionPolicy::AllKeysRandom), Some(budget)).expect("hot");
    let slack = (VALUE + 64) as u64;
    for i in 0..2_000 {
        ns_gated_set(&mut ks, 16, &format!("storm:{i}"), VALUE).unwrap_or_else(|e| {
            panic!(
                "step {i}: {e:?} used={} budget={budget}",
                ks.ns_store(NsId(16)).expect("live").used_bytes()
            )
        });
        let used = ks.ns_store(NsId(16)).expect("live").used_bytes();
        assert!(used <= budget + slack, "step {i}: used {used} exceeds budget {budget} + slack");
    }
    assert!(
        ks.ns_store(NsId(16)).expect("live").stats().evicted_keys >= 1_000,
        "the storm must have evicted heavily from its own keys"
    );
    for i in 0..100 {
        assert!(
            ks.db_mut(0).exists(format!("db0:{i}").as_bytes(), NOW),
            "db0:{i} was evicted for a namespace's own growth (the S20 collateral bug)"
        );
    }
}

/// The inheritance leg (ADR-0068 D2): a named memory store without its
/// own budget joins the global eviction hand, so pressure driven by its
/// growth resolves by reclaiming **its** bytes — before S27 this exact
/// shape evicted the numbered dbs dry and then answered OOM.
#[test]
fn inheriting_named_ns_joins_the_global_hand() {
    let mut ks = fresh();
    ks.ns_create(memory_ns(16, b"grower", None)).expect("create");
    pressure(&mut ks, EvictionPolicy::AllKeysRandom, 0);
    for i in 0..50 {
        set(&mut ks, &format!("db0:{i}"), 64);
    }
    for i in 0..400 {
        ns_set(&mut ks, 16, &format!("grow:{i}"), 256);
    }
    let limit = ks.used_bytes() * 3 / 4;
    pressure(&mut ks, EvictionPolicy::AllKeysRandom, limit);
    assert!(ks.over_limit());
    assert_eq!(
        ks.free_for_write(NOW),
        Ok(()),
        "the hand must reach the named store's bytes instead of answering OOM"
    );
    assert!(ks.used_bytes() <= limit);
    assert!(
        ks.ns_store(NsId(16)).expect("live").stats().evicted_keys > 0,
        "reclaim must come from the growing namespace"
    );
}

/// Durable named stores never evict, whatever the node policy does
/// (ADR-0015 D5, scoped by ADR-0068 D1): the policy push skips them, the
/// hand skips them, and every durable key survives global pressure.
#[test]
fn durable_named_store_never_joins_any_hand() {
    let mut ks = fresh();
    let durable = NsSpec {
        id: NsId(16),
        name: b"ledger".to_vec(),
        mode: NsMode::Durable,
        fsync: None,
        policy: None,
        maxmemory: None,
        tier: None,
    };
    ks.ns_create(durable).expect("create");
    for i in 0..100 {
        ns_set(&mut ks, 16, &format!("keep:{i}"), 256);
    }
    for i in 0..20 {
        set(&mut ks, &format!("db0:{i}"), 64);
    }
    let limit = ks.used_bytes() / 2;
    pressure(&mut ks, EvictionPolicy::AllKeysRandom, limit);
    assert_eq!(
        ks.ns_store(NsId(16)).expect("live").eviction_policy(),
        EvictionPolicy::NoEviction,
        "the policy push must not reach durable stores"
    );
    // The hand can only reach db0's few keys; the verdict is honest OOM —
    // never an eviction from the durable store.
    assert_eq!(ks.free_for_write(NOW), Err(OpError::OutOfMemory));
    for i in 0..100 {
        assert!(
            ks.ns_store_mut(NsId(16)).expect("live").exists(format!("keep:{i}").as_bytes(), NOW),
            "durable key keep:{i} was evicted"
        );
    }
    assert_eq!(ks.ns_store(NsId(16)).expect("live").stats().evicted_keys, 0);
}

/// The proactive half of the budget leg (M1-S03 shape): a hot-reloaded
/// per-namespace budget takes observable effect through MAINTAIN slices
/// alone — no writes arrive, and the store settles at its low watermark.
#[test]
fn maintain_drives_budgeted_ns_to_its_low_watermark() {
    let mut ks = fresh();
    ks.ns_create(memory_ns(16, b"cache", Some(EvictionPolicy::AllKeysLru))).expect("create");
    for i in 0..500 {
        ns_set(&mut ks, 16, &format!("fill:{i}"), 200);
    }
    let used = ks.ns_store(NsId(16)).expect("live").used_bytes();
    let live = ks.ns_store(NsId(16)).expect("live").report().records_live_bytes;
    let budget = used - live + live / 2;
    ks.ns_set_memory(b"cache", Some(EvictionPolicy::AllKeysLru), Some(budget)).expect("hot");
    assert!(ks.ns_over_limit(NsId(16)), "budget flag visible immediately after hot-reload");
    // Tick to fixpoint, as the plane's MAINTAIN loop does (it never stops
    // at the flag — the pass drives past it to the watermark, hysteresis).
    let mut slices = 0;
    loop {
        let before = ks.ns_store(NsId(16)).expect("live").used_bytes();
        ks.evict_tick(NOW, EvictBudget::default());
        slices += 1;
        assert!(slices < 10_000, "maintain must converge");
        if ks.ns_store(NsId(16)).expect("live").used_bytes() == before {
            break;
        }
    }
    assert!(!ks.ns_over_limit(NsId(16)), "the budget flag must have cleared");
    let settled = ks.ns_store(NsId(16)).expect("live").used_bytes();
    assert!(settled <= budget, "maintain must reach the namespace budget");
    assert!(
        settled <= budget - budget / 16 + 512,
        "and settle near the low watermark (hysteresis): {settled} vs budget {budget}"
    );
    assert_eq!(ks.db_mut(0).len(), 0, "nothing else was touched");
}

/// `CONFIG SET maxmemory-policy` pushes to inheriting named memory stores
/// only — an explicit per-namespace `EVICTION` beats inherited
/// (ADR-0068 D3).
#[test]
fn node_policy_push_reaches_inheriting_stores_only() {
    let mut ks = fresh();
    ks.ns_create(memory_ns(16, b"inheriting", None)).expect("create");
    ks.ns_create(memory_ns(17, b"explicit", Some(EvictionPolicy::AllKeysLfu))).expect("create");
    ns_set(&mut ks, 16, "k", 8);
    ns_set(&mut ks, 17, "k", 8);
    pressure(&mut ks, EvictionPolicy::AllKeysLru, 0);
    assert_eq!(ks.ns_store(NsId(16)).expect("live").eviction_policy(), EvictionPolicy::AllKeysLru);
    assert_eq!(ks.ns_store(NsId(17)).expect("live").eviction_policy(), EvictionPolicy::AllKeysLfu);
    pressure(&mut ks, EvictionPolicy::NoEviction, 0);
    assert_eq!(ks.ns_store(NsId(16)).expect("live").eviction_policy(), EvictionPolicy::NoEviction);
    assert_eq!(ks.ns_store(NsId(17)).expect("live").eviction_policy(), EvictionPolicy::AllKeysLfu);
}

/// The eviction-accounting oracle extended to named stores (the S27 test
/// obligation): after storms across db0, a budgeted namespace, and an
/// inheriting namespace, the aggregate counters are exactly the field-wise
/// per-store sums and `used_bytes` reconciles — accounting stays L5-exact
/// under every enforcement leg at once.
#[test]
fn eviction_accounting_reconciles_across_named_stores() {
    let mut ks = fresh();
    ks.ns_create(memory_ns(16, b"budgeted", Some(EvictionPolicy::AllKeysRandom))).expect("create");
    ks.ns_create(memory_ns(17, b"inheriting", None)).expect("create");
    pressure(&mut ks, EvictionPolicy::AllKeysRandom, 0);
    for i in 0..150 {
        set(&mut ks, &format!("db0:{i}"), 128);
        ns_set(&mut ks, 16, &format!("b:{i}"), 128);
        ns_set(&mut ks, 17, &format!("i:{i}"), 128);
    }
    let budget = ks.ns_store(NsId(16)).expect("live").used_bytes() / 2;
    ks.ns_set_memory(b"budgeted", Some(EvictionPolicy::AllKeysRandom), Some(budget)).expect("hot");
    let limit = ks.used_bytes() * 3 / 4;
    pressure(&mut ks, EvictionPolicy::AllKeysRandom, limit);
    for i in 0..300 {
        let _ = gated_set(&mut ks, &format!("storm:{i}"), 128);
        let _ = ns_gated_set(&mut ks, 16, &format!("bs:{i}"), 128);
    }
    let mut slices = 0;
    while (ks.over_limit() || ks.ns_over_limit(NsId(16))) && slices < 10_000 {
        ks.evict_tick(NOW, EvictBudget::default());
        slices += 1;
    }
    let by_hand: u64 = ks.dbs().map(|(_, s)| s.stats().evicted_keys).sum::<u64>()
        + [16, 17]
            .iter()
            .map(|id| ks.ns_store(NsId(*id)).expect("live").stats().evicted_keys)
            .sum::<u64>();
    assert_eq!(ks.stats().evicted_keys, by_hand, "aggregate counters are field-wise sums");
    assert!(by_hand > 0, "the storms must have evicted");
    let used_by_hand: u64 = ks.dbs().map(|(_, s)| s.used_bytes()).sum::<u64>()
        + [16, 17].iter().map(|id| ks.ns_store(NsId(*id)).expect("live").used_bytes()).sum::<u64>();
    assert_eq!(ks.used_bytes(), used_by_hand, "used_bytes reconciles (L5)");
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
