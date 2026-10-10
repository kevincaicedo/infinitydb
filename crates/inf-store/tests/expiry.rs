#![allow(
    clippy::disallowed_types,
    reason = "test target: std containers in test code, outside cell code (ADR-0163 D2)"
)]
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
//!   The campaign reports the virtual time it actually advanced
//!   (`expiry-campaign: sim_seconds=…`), which is the nightly fleet's
//!   sim-seconds credit for the run (review 2026-08-30, F-L19-02: the
//!   workflow used to echo the constant itself; a budget gate whose pass
//!   value is a literal in the file that asserts it is not a gate).

use std::collections::HashMap;

use inf_foundation::time::Nanos;
use inf_store::InternalDeadline::{At, BeforeOrigin};
use inf_store::limits::{
    EXPIRY_DRAIN_SLICES_MAX, EXPIRY_SWEEP_SLOTS_PER_SLICE, IDX_ALIAS_GROUP_MAX,
};
use inf_store::{
    COLLISION_KEY_PREFIX, CellStore, EvictionPolicy, ExpireCond, ExpiryAudit, ExpiryBudget,
    Keyspace, MAX_EXPIRE_MS, NsId, NsMode, NsSpec, SetCond, SetExpire, SetOptions, SetOutcome,
    StoreConfig, TtlUpdate, WheelNodesMax,
};

fn ms(v: u64) -> Nanos {
    Nanos(v * 1_000_000)
}

fn set_with_ttl(store: &mut CellStore, key: &[u8], deadline_ms: u64, now: Nanos) {
    let opts = SetOptions { expire: SetExpire::At(ms(deadline_ms)), ..Default::default() };
    store.set(key, b"v", opts, now).expect("set");
}

/// The campaign's virtual clock: the largest `now` handed to the store,
/// asserted monotone. What it reports at the end is measured from the
/// walk the test performed — never a constant the harness writes for it.
struct VirtualClock {
    now: Nanos,
}

impl VirtualClock {
    fn advance(&mut self, to: Nanos) -> Nanos {
        assert!(to >= self.now, "virtual time never runs backwards: {to:?} < {:?}", self.now);
        self.now = to;
        to
    }

    fn sim_seconds(&self) -> f64 {
        self.now.0 as f64 / 1e9
    }
}

/// Tick until the wheel catches up to `now` and the sweep settles;
/// returns total reaped (wheel fires and sweep visits).
fn drain(store: &mut CellStore, now: Nanos) -> u64 {
    let mut reaped = 0;
    loop {
        let stats = store.expire_tick(
            now,
            ExpiryBudget { max_fires: 1024, max_steps: 1 << 20, max_sweep_slots: 1 << 20 },
        );
        reaped += stats.reaped + stats.swept;
        if stats.lag_ms == 0 && store.expiry_settled(now) {
            return reaped;
        }
    }
}

/// ADR-0008 A1 O3's drain at the store tier: time frozen at `now`, every
/// budget unbounded, until the wheel has caught up and the sweep is idle
/// or completed a pass that began at `now`. Returns the records reaped.
fn drain_settled(store: &mut CellStore, now: Nanos) -> u64 {
    let mut reaped = 0;
    for _ in 0..EXPIRY_DRAIN_SLICES_MAX {
        let stats = store.expire_tick(now, ExpiryBudget::UNBOUNDED);
        reaped += stats.reaped + stats.swept;
        if store.expiry_settled(now) {
            return reaped;
        }
    }
    panic!("the expiry drain never settled at {now:?}");
}

fn small_cap(nodes: usize) -> StoreConfig {
    StoreConfig {
        wheel_nodes_max: WheelNodesMax::new(nodes).expect("under the width bound"),
        ..StoreConfig::default()
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

    // Halfway: exactly the due half is gone — via the wheel alone. A
    // deadline is due from the millisecond after it (F-L05-05).
    let reaped = drain(&mut store, ms(600));
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

/// I7 at one quiescent point: when the sweep owes nothing, every record
/// with a deadline has a node. Returns whether the check ran (the sweep
/// was idle) over at least one record with a deadline — the engagement
/// a caller counts.
fn assert_scheduled_when_idle(audit: &ExpiryAudit, at: &str) -> bool {
    assert_eq!(audit.unscheduled, 0, "{at}: records with a deadline and no node, sweep idle");
    audit.sweep_idle && audit.ttl_live > 0
}

/// The reference model under churn (ADR-0008 A1 O2): a `HashMap` key →
/// deadline, independent of the store, driven through every deadline
/// transition — set, clear, extend, shorten, persist, delete-then-set —
/// with budgeted slices between. After every op the wheel holds no more
/// live nodes than records with a deadline, and I7 holds whenever the
/// sweep is idle; after a drain with no reads, the store's `len()` (the
/// index's live count — it reads no record, so it reaps nothing) equals
/// the model's live count.
fn churn_against_model(cfg: StoreConfig, ops: usize, mut x: u64) {
    let mut store = CellStore::new(cfg);
    // key → deadline_ms (None = no TTL)
    let mut model: HashMap<Vec<u8>, Option<u64>> = HashMap::new();
    let mut rand = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let mut now_ms: u64 = 10;
    let mut idle_checks = 0u64;
    for op in 0..ops {
        now_ms += rand() % 20;
        let now = ms(now_ms);
        // Purge the model of expired entries before mutating against it
        // (alive through the deadline millisecond).
        model.retain(|_, deadline| deadline.is_none_or(|d| d >= now_ms));
        let key = format!("key:{}", rand() % 256).into_bytes();
        churn_op(&mut store, &mut model, &key, rand(), rand(), now, op);
        let (armed, ttl_live) = (store.wheel_armed(), store.stats().ttl_live);
        assert!(armed <= ttl_live, "op {op}: armed {armed} > ttl_live {ttl_live}");
        let audit = store.expiry_audit();
        idle_checks += u64::from(assert_scheduled_when_idle(&audit, &format!("op {op}")));
    }
    assert!(idle_checks > 0, "engagement: I7 never checked with the sweep idle");
    // Catch up fully: visible state must equal the model exactly.
    now_ms += 1;
    drain_settled(&mut store, ms(now_ms));
    assert_scheduled_when_idle(&store.expiry_audit(), "after the drain");
    model.retain(|_, deadline| deadline.is_none_or(|d| d >= now_ms));
    assert_eq!(store.len(), model.len(), "live census after the drain, before any read");
    let final_now = ms(now_ms);
    for (key, _) in model {
        assert!(store.get(&key, final_now).is_some(), "model key missing: {key:?}");
    }
}

fn churn_op(
    store: &mut CellStore,
    model: &mut HashMap<Vec<u8>, Option<u64>>,
    key: &[u8],
    pick: u64,
    roll: u64,
    now: Nanos,
    op: usize,
) {
    let now_ms = now.0 / 1_000_000;
    match pick % 9 {
        0 => {
            store.set(key, b"v", SetOptions::default(), now).expect("set");
            model.insert(key.to_vec(), None);
        }
        1 => {
            let deadline = now_ms + 1 + roll % 5_000;
            set_with_ttl(store, key, deadline, now);
            model.insert(key.to_vec(), Some(deadline));
        }
        2 => {
            let got = store.del(key, now);
            assert_eq!(got, model.remove(key).is_some(), "op {op}: DEL disagreed");
        }
        3 | 7 => {
            // 3 extends or moves a deadline; 7 shortens it hard.
            let span = if pick % 9 == 3 { 2_000 } else { 50 };
            let deadline = now_ms + 1 + roll % span;
            let got = store.expire(key, Some(At(ms(deadline))), ExpireCond::Always, now);
            let want = model.contains_key(key);
            assert_eq!(got, want, "op {op}: EXPIRE disagreed");
            if want {
                model.insert(key.to_vec(), Some(deadline));
            }
        }
        5 => {
            let got = store.expire(key, None, ExpireCond::Always, now);
            let want = matches!(model.get(key), Some(Some(_)));
            assert_eq!(got, want, "op {op}: PERSIST disagreed");
            if want {
                model.insert(key.to_vec(), None);
            }
        }
        6 => {
            store.del(key, now);
            let deadline = now_ms + 1 + roll % 3_000;
            set_with_ttl(store, key, deadline, now);
            model.insert(key.to_vec(), Some(deadline));
        }
        _ => {
            // A budget-bounded slice at a random moment.
            let budget = ExpiryBudget { max_fires: 32, max_steps: 512, max_sweep_slots: 64 };
            store.expire_tick(now, budget);
        }
    }
}

/// ADR-0111 A1: a deadline before the clock's origin is expired at every
/// reading of the clock, so every write that receives one leaves no key —
/// at `now` inside the origin's millisecond too, where a deadline clamped
/// onto the origin read as live. NX/XX and `GET` keep their meaning.
#[test]
fn pre_origin_deadlines_leave_no_key_at_any_now() {
    for now in [Nanos(1), Nanos(999_999), ms(5_000)] {
        let mut store = CellStore::new(StoreConfig::default());
        let before = SetOptions { expire: SetExpire::BeforeOrigin, ..Default::default() };
        let applied = store.set(b"fresh", b"v", before, now).expect("set");
        assert_eq!(applied, SetOutcome::Applied { old: None }, "{now}");
        assert_eq!(store.get_str(b"fresh", now), Ok(None), "SET at {now}");
        store.set(b"over", b"old", SetOptions::default(), now).expect("set");
        let get_old = SetOptions { get_old: true, ..before };
        let replaced = store.set(b"over", b"v", get_old, now).expect("set");
        assert_eq!(replaced, SetOutcome::Applied { old: Some(b"old".to_vec()) }, "{now}");
        assert_eq!(store.get_str(b"over", now), Ok(None), "SET .. GET at {now}");
        let xx = SetOptions { cond: SetCond::IfPresent, ..before };
        assert_eq!(store.set(b"over", b"v", xx, now), Ok(SetOutcome::Skipped { old: None }));
        store.set(b"getex", b"v", SetOptions::default(), now).expect("set");
        let read = store.get_ex(b"getex", TtlUpdate::BeforeOrigin, now);
        assert_eq!(read.as_deref(), Some(b"v".as_slice()), "GETEX answers first");
        assert_eq!(store.get_str(b"getex", now), Ok(None), "GETEX at {now}");
        // LT against a live deadline at `now`'s own millisecond (the
        // origin itself inside the first one) applies and deletes.
        let at_now = Nanos::from_millis(now.as_millis());
        let live = SetOptions { expire: SetExpire::At(at_now), ..Default::default() };
        store.set(b"lt", b"v", live, now).expect("set");
        assert_eq!(store.get_str(b"lt", now), Ok(Some(b"v".as_slice())), "live at {now}");
        let lt = store.expire(b"lt", Some(BeforeOrigin), ExpireCond::IfLess, now);
        assert!(lt, "LT applies at {now}");
        assert_eq!(store.get_str(b"lt", now), Ok(None), "EXPIRE LT at {now}");
        assert_eq!(store.len(), 0, "no record survives at {now}");
    }
}

#[test]
fn wheel_matches_reference_model_under_churn() {
    let ops: usize = if cfg!(miri) { 2_000 } else { 60_000 };
    churn_against_model(StoreConfig::default(), ops, 0xD15E_A5ED_C0FF_EE00);
}

/// O2's small-cap run: at 16 nodes most placements are refused, so most
/// records with a deadline are swept rather than scheduled — the drain
/// must still leave none past its deadline.
#[test]
fn a_small_node_budget_matches_the_reference_model_under_churn() {
    let ops: usize = if cfg!(miri) { 2_000 } else { 30_000 };
    churn_against_model(small_cap(16), ops, 0x5EED_0CA9_0016);
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
            stats.fired + stats.swept <= u64::from(budget.max_fires),
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
    // Seedable for the nightly DST fleet (M1-S15): distinct INF_DST_SEED
    // values are genuinely distinct 48 h campaigns, so the fleet's
    // sim-seconds budget is real coverage, not the same run repeated.
    let seed: u64 = std::env::var("INF_DST_SEED")
        .ok()
        .and_then(|v| {
            let v = v.trim();
            v.strip_prefix("0x")
                .map_or_else(|| v.parse().ok(), |hex| u64::from_str_radix(hex, 16).ok())
        })
        .filter(|&s| s != 0)
        .unwrap_or(0x48_4F_55_52);
    let mut x = seed;
    let mut rand = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let mut clock = VirtualClock { now: t0 };
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
    // `bucket·width` — the last instant where exactly the buckets below are
    // due (a deadline is due from the millisecond AFTER it, F-L05-05, so
    // the boundary key `bucket·width` is not yet due there; under the old
    // inclusive predicate the sample sat at `− 1`, and the full 10M
    // campaign once caught the harness off by one at exactly this edge).
    let mut bucket = 0usize;
    while bucket < BUCKETS {
        bucket = (bucket + 1 + (rand() % 24) as usize).min(BUCKETS);
        // The last stride overshoots 48 h (BUCKETS × width > HOURS_48_MS by
        // ~2 s), which the measured clock exposed: the campaign stops at
        // `HOURS_48_MS + 2`, the first instant every deadline (≤ 48 h + 1)
        // is due — so the reported sim-seconds are the campaign's actual
        // span, not an artefact of the bucket arithmetic.
        let now_ms = (bucket as u64 * bucket_width).min(HOURS_48_MS + 2);
        drain(&mut store, clock.advance(ms(now_ms)));
        let expected_gone: u64 = due_by_bucket[..bucket].iter().sum();
        assert_eq!(
            store.len() as u64,
            keys - expected_gone,
            "census diverged at bucket {bucket} (t={now_ms}ms)"
        );
    }
    drain(&mut store, clock.advance(ms(HOURS_48_MS + 2)));
    assert_eq!(store.len(), 0, "every TTL fired");
    let stats = store.stats();
    assert_eq!(stats.expired_active, keys);
    assert_eq!(stats.wheel_fallback, 0, "pool never overflowed");
    // The fleet's evidence line, printed only once every oracle above held:
    // the nightly asserts this exact line per seed (`--nocapture`), so a
    // renamed or filtered-out test cannot earn the credit (F-L19-01/02).
    println!(
        "expiry-campaign: sim_seconds={:.6} keys={keys} seed={seed:#x} mode={}",
        clock.sim_seconds(),
        if full { "full" } else { "ci" }
    );
}

/// M1-S04 AC 1 as ADR-0008 A1 restates it: one 16 B node per key hash
/// with a deadline plus one 4 B membership entry at a load in
/// (7/16, 7/8] — at most 26 B per scheduled key — with the node pool's
/// capacity within 2× its peak under growth. Attribution-verified: the
/// resident (`wheel_bytes`) and in-use (`wheel_live_bytes`) domains are
/// read back and split into nodes, pool slack and membership.
#[test]
fn wheel_memory_is_one_node_and_one_membership_entry_per_ttl_key() {
    let keys: u64 = if cfg!(miri) { 500 } else { 100_000 };
    let mut store = CellStore::new(StoreConfig::default());
    let t0 = ms(1);
    let empty = store.report();
    assert_eq!(empty.wheel_bytes, empty.wheel_live_bytes, "no pool before the first node");
    for i in 0..keys {
        let key = format!("ttl:{i}");
        set_with_ttl(&mut store, key.as_bytes(), 1_000_000 + i, t0);
    }
    let report = store.report();
    let audit = store.expiry_audit();
    assert_eq!(audit.armed, keys, "one live node per key");
    let nodes = keys * 16;
    let membership = report.wheel_live_bytes - empty.wheel_live_bytes - nodes;
    let pool_slack = report.wheel_bytes - report.wheel_live_bytes;
    assert!(membership <= keys * 10, "membership {membership} B for {keys} keys");
    assert!(report.wheel_live_bytes - empty.wheel_live_bytes <= keys * 26, "over 26 B per key");
    assert!(
        nodes + pool_slack <= 2 * nodes,
        "pool capacity over 2× its peak: {pool_slack} B slack"
    );
}

// ---- F-L05-03 (review 2026-08-30, batch 57): the expiry slice rotates ----

const SLICE: ExpiryBudget =
    ExpiryBudget { max_fires: 64, max_steps: 4096, max_sweep_slots: EXPIRY_SWEEP_SLOTS_PER_SLICE };

fn set_ttl_in(store: &mut CellStore, prefix: &str, n: u32, deadline_ms: u64) {
    for i in 0..n {
        set_with_ttl(store, format!("{prefix}{i}").as_bytes(), deadline_ms, ms(1));
    }
}

fn sessions_ns() -> NsSpec {
    NsSpec {
        id: NsId(16),
        name: b"sessions".to_vec(),
        mode: NsMode::Memory,
        fsync: None,
        policy: Some(EvictionPolicy::NoEviction),
        maxmemory: None,
        tier: None,
    }
}

/// The lane's reproduction plan, executed: db0 carries more due entries
/// than one slice's fire budget, db1 a handful long past due. Pre-fix the
/// slice walked `db0..db15` then named from the top every time, so db1
/// never saw a fire while db0 had work; the cursor now starts each slice
/// at the first store the previous one left unserved.
#[test]
fn a_busy_db0_does_not_starve_the_other_namespaces_wheels() {
    let mut ks = Keyspace::new(StoreConfig::default());
    set_ttl_in(ks.db_mut(0), "hot:", 2_000, 50);
    set_ttl_in(ks.db_mut(1), "cold:", 10, 50);
    for _ in 0..20 {
        ks.expire_tick(ms(1_000), SLICE);
    }
    assert_eq!(ks.db_mut(1).len(), 0, "db1's expired keys were never actively reaped");
    // The rotation is fair both ways: the storm keeps draining.
    let hot = ks.db_mut(0).len();
    assert!(hot <= 2_000 - 64 * 18, "db0 lost its turns to the rotation: {hot} left");
}

/// The lane's failure scenario: a named memory namespace sits last in the
/// chain, so a db0 storm kept its wheel from ever ticking — its expired
/// records accumulated while `DBSIZE` and `INFO keyspace` kept counting
/// them.
#[test]
fn a_named_namespace_last_in_the_chain_still_gets_its_wheel_ticks() {
    let mut ks = Keyspace::new(StoreConfig::default());
    ks.ns_create(sessions_ns()).expect("create");
    set_ttl_in(ks.ns_store_mut(NsId(16)).expect("registered"), "s:", 10, 10);
    set_ttl_in(ks.db_mut(0), "hot:", 2_000, 50);
    for _ in 0..20 {
        ks.expire_tick(ms(1_000), SLICE);
    }
    let left = ks.ns_store(NsId(16)).expect("materialized").len();
    assert_eq!(left, 0, "the sessions namespace's expired keys were never reaped");
}

/// The metric half: `lag_ms` folds every store's wheel, served or not, so
/// the plane's debt escalation and the operator see a starved store.
/// Pre-fix the fold covered only the stores the slice reached — db1's
/// older debt was invisible behind db0's.
#[test]
fn the_debt_metric_sees_a_store_the_slice_never_reached() {
    let mut ks = Keyspace::new(StoreConfig::default());
    set_ttl_in(ks.db_mut(0), "hot:", 2_000, 50);
    set_ttl_in(ks.db_mut(1), "cold:", 10, 10);
    let now = ms(1_000);
    let first = ks.expire_tick(now, SLICE);
    assert!(first.reaped == 64, "db0 alone spends the slice: {first:?}");
    let db0_only = 1_000 - 50;
    let db1 = ks.db(1).expect("materialized").expiry_lag_ms(now);
    assert!(db1 > db0_only, "db1's wheel trails further than db0's: {db1}");
    assert_eq!(first.lag_ms, db1, "the slice reported db0's debt, not the worst store's");
    assert_eq!(ks.expiry_lag_ms(now), db1, "the INFO fold disagrees with the slice");
    // An idle wheel is not debt: nothing armed reads 0, whatever its cursor.
    let mut idle = CellStore::new(StoreConfig::default());
    idle.set(b"k", b"v", SetOptions::default(), ms(1)).expect("set");
    assert_eq!(idle.expiry_lag_ms(ms(1_000_000)), 0);
}

/// F-L05-05 (review 2026-08-30, batch 58): the deadline millisecond still
/// serves the key. Redis's read path is `now > when`, so at `now == when`
/// `GET` answers the value and `PTTL` answers 0; the key is gone from the
/// next millisecond on. One predicate serves reads, scans and the wheel.
#[test]
fn the_deadline_millisecond_still_serves_the_key() {
    let mut store = CellStore::new(StoreConfig::default());
    set_with_ttl(&mut store, b"k", 100, ms(1));
    assert_eq!(store.get(b"k", ms(100)), Some(b"v".as_slice()), "served at the deadline ms");
    assert_eq!(store.ttl(b"k", ms(100)), inf_store::Ttl::Ms(0), "PTTL 0 at the deadline ms");
    assert!(store.exists(b"k", ms(100)));
    assert_eq!(store.get(b"k", ms(101)), None, "gone one millisecond later");
    assert_eq!(store.ttl(b"k", ms(101)), inf_store::Ttl::Missing);
}

/// F-L05-05: active expiry fires at the first millisecond the record
/// reads as expired — never at the deadline itself (a fire there would
/// fail validation and strand the entry as a stale drop, leaving the
/// record to lazy expiry alone).
#[test]
fn active_expiry_fires_at_the_first_expired_millisecond() {
    let mut store = CellStore::new(StoreConfig::default());
    set_with_ttl(&mut store, b"k", 100, ms(1));
    let budget = ExpiryBudget { max_fires: 1024, max_steps: 1 << 20, max_sweep_slots: 1 << 20 };
    let at_deadline = store.expire_tick(ms(100), budget);
    assert_eq!(at_deadline.reaped, 0, "not reaped at the deadline ms");
    assert_eq!(at_deadline.stale, 0, "and not dropped as stale either");
    assert_eq!(store.len(), 1);
    let after = store.expire_tick(ms(101), budget);
    assert_eq!(after.reaped, 1, "reaped by the wheel one millisecond later");
    assert_eq!(after.stale, 0);
    assert_eq!(store.len(), 0);
    assert_eq!(store.stats().expired_active, 1, "active, not lazy");
}

// ---- ADR-0008 A1: one wheel node per key hash; a refused key is swept ----

/// One hostile sequence on one or two keys (ADR-0008 A1's adversarial
/// list): `step(store, i, now)` runs the sequence's `i`-th touch.
struct HostileLeg {
    name: &'static str,
    keys: u64,
    step: fn(&mut CellStore, u64, Nanos),
}

const LEG_TOUCHES: u64 = 1_000;

fn set_plain(store: &mut CellStore, key: &[u8], now: Nanos) {
    store.set(key, b"v", SetOptions::default(), now).expect("set");
}

fn expire_at(store: &mut CellStore, key: &[u8], deadline_ms: u64, now: Nanos) {
    assert!(
        store.expire(key, Some(At(ms(deadline_ms))), ExpireCond::Always, now),
        "expire applied"
    );
}

fn persist(store: &mut CellStore, key: &[u8], now: Nanos) {
    assert!(store.expire(key, None, ExpireCond::Always, now), "persist applied");
}

/// The legs' drain instant: past the largest deadline a record can carry.
const LEGS_DRAIN_MS: u64 = MAX_EXPIRE_MS + 1;

/// Before every leg's earliest remaining deadline (10 000 ms): a drain here
/// cascades the key's node toward tier 0 but fires nothing.
const LEGS_MID_DRAIN_MS: u64 = 9_999;

fn hostile_legs() -> [HostileLeg; 13] {
    [
        // A death with no rewrite after it: a node the death left behind
        // fires with no member (`wheel_stale`). `b` keeps a deadline live
        // for the engagement checks.
        HostileLeg {
            name: "SET EX then DEL",
            keys: 2,
            step: |s, i, now| {
                if i == 0 {
                    set_with_ttl(s, b"b", 20_000, now);
                }
                set_with_ttl(s, b"a", 10_000 + i, now);
                assert!(s.del(b"a", now), "DEL applied");
            },
        },
        HostileLeg {
            name: "EXPIRE extend",
            keys: 1,
            step: |s, i, now| {
                if i == 0 {
                    set_plain(s, b"k", now);
                }
                expire_at(s, b"k", 10_000 + i, now);
            },
        },
        HostileLeg {
            name: "SET EX extend",
            keys: 1,
            step: |s, i, now| set_with_ttl(s, b"k", 10_000 + i, now),
        },
        HostileLeg {
            name: "GETEX EX extend",
            keys: 1,
            step: |s, i, now| {
                if i == 0 {
                    set_plain(s, b"k", now);
                }
                assert!(s.get_ex(b"k", TtlUpdate::At(ms(10_000 + i)), now).is_some());
            },
        },
        HostileLeg {
            name: "SET then EXPIRE",
            keys: 1,
            step: |s, i, now| {
                set_plain(s, b"k", now);
                expire_at(s, b"k", 10_000 + i, now);
            },
        },
        HostileLeg {
            name: "PERSIST then EXPIRE",
            keys: 1,
            step: |s, i, now| {
                if i == 0 {
                    set_with_ttl(s, b"k", 10_000, now);
                }
                assert!(s.expire(b"k", None, ExpireCond::Always, now), "persist applied");
                expire_at(s, b"k", 10_000 + i, now);
            },
        },
        HostileLeg {
            name: "DEL then SET EX",
            keys: 1,
            step: |s, i, now| {
                s.del(b"k", now);
                set_with_ttl(s, b"k", 10_000 + i, now);
            },
        },
        HostileLeg {
            name: "RENAME ping-pong",
            keys: 1,
            step: |s, i, now| {
                if i == 0 {
                    set_with_ttl(s, b"a", 10_000, now);
                }
                assert!(s.rename(b"a", b"b", now).expect("rename"));
                assert!(s.rename(b"b", b"a", now).expect("rename"));
            },
        },
        HostileLeg {
            name: "COPY REPLACE",
            keys: 2,
            step: |s, i, now| {
                if i == 0 {
                    set_plain(s, b"a", now);
                }
                expire_at(s, b"a", 10_000 + i, now);
                s.copy(b"a", b"b", true, now).expect("copy");
            },
        },
        HostileLeg {
            name: "replay SET then EXPIREAT",
            keys: 1,
            step: |s, i, now| {
                s.replay_set(b"k", b"v", now).expect("replay set");
                s.replay_expire_at(b"k", At(ms(10_000 + i)), now);
            },
        },
        HostileLeg {
            name: "strictly decreasing PEXPIREAT",
            keys: 1,
            step: |s, i, now| {
                if i == 0 {
                    set_plain(s, b"k", now);
                }
                expire_at(s, b"k", 10_000_000 - i * 1_000, now);
            },
        },
        // The integer maximum files on the overflow list: each touch clears
        // the previous near deadline from its tier slot, files there, and
        // moves out to a near deadline (a successor copy or an overflow
        // tombstone).
        HostileLeg {
            name: "PERSIST, then PEXPIREAT max, then near",
            keys: 1,
            step: |s, i, now| {
                if i == 0 {
                    set_plain(s, b"k", now);
                } else {
                    persist(s, b"k", now);
                }
                expire_at(s, b"k", MAX_EXPIRE_MS, now);
                expire_at(s, b"k", 10_000 + i, now);
            },
        },
        // Filed on the overflow list and cleared there: an overflow
        // tombstone each touch, kept at the list's tail.
        HostileLeg {
            name: "PEXPIREAT max, then PERSIST",
            keys: 1,
            step: |s, i, now| {
                if i == 0 {
                    set_plain(s, b"k", now);
                }
                expire_at(s, b"k", MAX_EXPIRE_MS, now);
                persist(s, b"k", now);
                if i + 1 == LEG_TOUCHES {
                    expire_at(s, b"k", MAX_EXPIRE_MS, now);
                }
            },
        },
    ]
}

/// O1 after every touch: one live node per key (`armed ≤ keys`, `armed ≤
/// ttl_live`), and no wheel list holding more than one tombstone. Rule 4
/// keeps a list's tombstone at its tail, so a list holds at most one per
/// list epoch; no tick runs during a leg, so every list is in one epoch.
/// That per-list premise is what `WHEEL_TOMBSTONES_MAX` is derived from,
/// and unlike the total it is a bound one leg can cross.
fn assert_one_node_per_key(store: &CellStore, leg: &HostileLeg, touch: u64) {
    let audit = store.expiry_audit();
    let name = leg.name;
    assert!(
        audit.armed <= audit.ttl_live,
        "{name}: touch {touch}: armed {} > ttl_live {}",
        audit.armed,
        audit.ttl_live
    );
    assert!(
        audit.armed <= leg.keys,
        "{name}: touch {touch}: armed {} for {} keys",
        audit.armed,
        leg.keys
    );
    assert!(
        audit.list_tombstones_max <= 1,
        "{name}: touch {touch}: {} tombstones in one wheel list",
        audit.list_tombstones_max
    );
}

/// The idle sweep leaves no record with a deadline unscheduled (I7) and
/// no node late (I4), and no membership entry is orphaned; `at` must see
/// a deadline live (engagement).
fn assert_schedule_sound(audit: &ExpiryAudit, name: &str, at: &str) {
    assert_eq!(audit.orphans, 0, "{name}: {at}: orphaned membership entries");
    assert!(
        assert_scheduled_when_idle(audit, &format!("{name}: {at}")),
        "{name}: {at}: engagement: the sweep owes nothing with a deadline live"
    );
    assert_eq!(audit.late, 0, "{name}: {at}: a node filed after its group's deadline");
}

/// O1 on every hostile leg: however a key's deadline is rewritten, every
/// touch keeps one node per key and one tombstone per list at most; the
/// schedule is sound after the touches and again after a drain that
/// cascades the key's node but fires nothing; and once the wheel passes
/// the largest deadline, every fire found a member (`wheel_stale == 0`),
/// no node or tombstone is left and the census is empty.
#[test]
fn one_key_keeps_one_wheel_node_under_every_ttl_rewrite() {
    for leg in hostile_legs() {
        let mut store = CellStore::new(StoreConfig::default());
        let name = leg.name;
        for i in 0..LEG_TOUCHES {
            (leg.step)(&mut store, i, ms(1));
            assert_one_node_per_key(&store, &leg, i);
        }
        assert_schedule_sound(&store.expiry_audit(), name, "after the touches");
        drain_settled(&mut store, ms(LEGS_MID_DRAIN_MS));
        assert_schedule_sound(&store.expiry_audit(), name, "after a drain");
        drain_settled(&mut store, ms(LEGS_DRAIN_MS));
        let audit = store.expiry_audit();
        assert_eq!(store.len(), 0, "{name}: a key outlived its deadline");
        assert_eq!(audit.armed, 0, "{name}: nodes left after every deadline passed");
        assert_eq!(audit.tombstones, 0, "{name}: tombstones left past the leg's last instant");
        assert_eq!(store.stats().wheel_stale, 0, "{name}: a fire found no member");
        assert_eq!(audit.ttl_live, 0, "{name}: the census kept a record that died");
    }
}

/// The state table's last row: `FLUSH*` resets the schedule with the record
/// table. A store flushed with every kind of schedule state live — nodes
/// at the cap, a tombstone, refused records and a sweep pass under way —
/// holds no node, no tombstone, no census and an idle sweep, and keys
/// written after it expire actively with no fire finding a hash that has
/// no member (`wheel_stale`).
#[test]
fn flush_resets_the_schedule_with_the_table() {
    let cfg = StoreConfig { initial_keys: 1_024, ..small_cap(16) };
    let mut store = CellStore::new(cfg);
    for i in 0..48 {
        set_with_ttl(&mut store, format!("old:{i}").as_bytes(), 1_000 + i, ms(0));
    }
    // The first key filed is its list's tail: clearing it leaves a tombstone.
    persist(&mut store, b"old:0", ms(0));
    let slice = ExpiryBudget { max_fires: u32::MAX, max_steps: u32::MAX, max_sweep_slots: 16 };
    store.expire_tick(ms(1), slice);
    let before = store.expiry_audit();
    assert!(before.armed > 0 && before.tombstones > 0, "engagement: {before:?}");
    assert_eq!(before.ttl_live, 47, "engagement: the census holds every deadline");
    assert!(store.sweep_pass_slots().is_some(), "engagement: a sweep pass is under way");

    store.flush(ms(5));
    let after = store.expiry_audit();
    assert_eq!(store.len(), 0);
    assert_eq!((after.armed, after.tombstones, after.ttl_live, after.orphans), (0, 0, 0, 0));
    assert!(after.sweep_idle, "nothing is owed once no record remains");

    for i in 0..8 {
        set_with_ttl(&mut store, format!("new:{i}").as_bytes(), 10 + i, ms(5));
    }
    let stale = store.stats().wheel_stale;
    drain_settled(&mut store, ms(2_000));
    assert_eq!(store.len(), 0, "keys written after the flush expire without a read");
    assert_eq!(store.stats().wheel_stale, stale, "a node outlived the flush and fired");
    assert_eq!(store.expiry_audit().ttl_live, 0);
}

/// Rule 3's crossing at the node budget: a refused placement is swept,
/// never left to lazy expiry. `wheel_nodes_max` 64 with 0, 1, cap − 1,
/// cap, cap + 1 and 200 keys, drained with `expire_tick` alone (no read).
#[test]
fn refused_keys_expire_actively_at_the_node_cap() {
    for keys in [0u64, 1, 63, 64, 65, 200] {
        let mut store = CellStore::new(small_cap(64));
        for i in 0..keys {
            set_with_ttl(&mut store, format!("cap:{i}").as_bytes(), 2 + i % 100, ms(1));
        }
        let refused = keys.saturating_sub(64);
        assert_eq!(store.stats().wheel_fallback, refused, "{keys} keys: refusals");
        // I7's crossing: a refusal leaves the sweep owed, never idle.
        let audit = store.expiry_audit();
        assert_eq!(
            audit.sweep_idle,
            refused == 0,
            "{keys} keys: the sweep owed exactly on refusal"
        );
        assert_scheduled_when_idle(&audit, &format!("{keys} keys"));
        drain_settled(&mut store, ms(200));
        let stats = store.stats();
        assert_eq!(store.len(), 0, "{keys} keys: {} refused keys retained", store.len());
        assert_eq!(stats.expired_active, keys, "{keys} keys: reaped actively");
        assert_eq!(stats.expired_lazy, 0, "{keys} keys: no read ever ran");
        assert_eq!(stats.expired_swept > 0, refused > 0, "{keys} keys: the sweep reaped");
    }
}

/// One member of the alias group `tag` under the suite's collision-oracle
/// hasher: the 32-byte hashed head is a function of `tag` alone.
fn alias_key(tag: u64, member: u8) -> [u8; 48] {
    let mut out = [0u8; 48];
    out[..16].copy_from_slice(COLLISION_KEY_PREFIX);
    out[16..24].copy_from_slice(&tag.wrapping_mul(0x9E37_79B9_7F4A_7C15).to_le_bytes());
    out[24..32].copy_from_slice(&tag.to_le_bytes());
    out[32..].copy_from_slice(&[member; 16]);
    out
}

/// Rule 4: a node is a fact about its key *hash*, so a colliding twin's
/// death must ask the group before removing it. `b` keeps the node, which
/// fires early at `a`'s instant, re-files at `b`'s, and reaps `b` there.
#[test]
fn a_colliding_hash_keeps_its_node_when_its_twin_dies() {
    let (a, b) = (alias_key(7, 0), alias_key(7, 1));
    let mut store = CellStore::new(StoreConfig::default());
    set_with_ttl(&mut store, &a, 100, ms(1));
    set_with_ttl(&mut store, &b, 200, ms(1));
    assert!(store.del(&a, ms(1)));
    let audit = store.expiry_audit();
    assert!(assert_scheduled_when_idle(&audit, "after a's DEL"), "b keeps the node, sweep idle");
    drain_settled(&mut store, ms(150));
    assert_eq!(store.len(), 1, "b is live until its own deadline");
    drain_settled(&mut store, ms(250));
    assert_eq!(store.len(), 0, "b expired without a read");
    let audit = store.expiry_audit();
    assert_eq!(audit.armed, 0);
    assert_eq!(audit.orphans, 0);
    assert_eq!(store.stats().expired_lazy, 0);
}

/// Rule 5 and I10: one node schedules two colliding keys with the same
/// `PXAT`; its fire must reap both and must never file anything into the
/// tier-0 slot it is draining (a node filed there is lost, and the second
/// key is left to lazy expiry).
#[test]
fn colliding_keys_with_one_deadline_both_expire_actively() {
    let (a, b) = (alias_key(9, 0), alias_key(9, 1));
    let mut store = CellStore::new(StoreConfig::default());
    set_with_ttl(&mut store, &a, 300, ms(1));
    set_with_ttl(&mut store, &b, 300, ms(1));
    drain_settled(&mut store, ms(305));
    let audit = store.expiry_audit();
    assert_eq!(store.len(), 0, "both colliding keys expired without a read");
    assert_eq!(audit.armed, 0);
    assert_eq!(audit.tombstones, 0);
    assert_eq!(audit.orphans, 0, "no membership entry outlived its node");
}

/// Rule 4's `Over` crossing: a group of nine with deadlines exceeds
/// `IDX_ALIAS_GROUP_MAX`, so the death of a tenth cannot answer "does
/// another member keep the node" — the node is removed and the sweep
/// owed, never left to lazy expiry.
#[test]
fn a_ttl_alias_group_of_nine_owes_the_sweep() {
    let mut store = CellStore::new(StoreConfig::default());
    for member in 0..10u8 {
        set_with_ttl(&mut store, &alias_key(11, member), 400, ms(1));
    }
    assert!(store.del(&alias_key(11, 0), ms(1)));
    assert!(store.stats().expiry_alias_over >= 1, "the walk over nine members is Over");
    drain_settled(&mut store, ms(500));
    let stats = store.stats();
    assert_eq!(store.len(), 0, "every member expired without a read");
    assert!(stats.expired_swept > 0, "the sweep reaped the group");
    assert_eq!(stats.expired_lazy, 0);
}

/// Rule 6's pass-scoped idle rule (I11): a write refused while a pass is
/// under way may land behind the cursor, so it must keep that pass from
/// going idle — not only the sweep's own refusals. Setup: 16 far keys
/// fill the pool, 32 refused keys expire before the pass visits them (so
/// the sweep refuses nothing of its own), the pass stops mid-table, and
/// 64 refused writes follow.
#[test]
fn a_write_refused_behind_the_sweep_cursor_is_reaped() {
    let cfg = StoreConfig { initial_keys: 1_024, ..small_cap(16) };
    let mut store = CellStore::new(cfg);
    for i in 0..16 {
        set_with_ttl(&mut store, format!("far:{i}").as_bytes(), 1_000_000, ms(0));
    }
    for i in 0..32 {
        set_with_ttl(&mut store, format!("wave:{i}").as_bytes(), 1, ms(0));
    }
    let capacity = store.index_capacity();
    let slice = ExpiryBudget { max_fires: u32::MAX, max_steps: u32::MAX, max_sweep_slots: 16 };
    let (begin, cursor) = (0..capacity)
        .find_map(|_| {
            store.expire_tick(ms(2), slice);
            let (begin, cursor) = store.sweep_pass_slots()?;
            let walked = cursor.wrapping_sub(begin) % capacity;
            (walked >= capacity / 2).then_some((begin, cursor))
        })
        .expect("a sweep pass is under way and reaches mid-table");
    let walked = cursor.wrapping_sub(begin) % capacity;
    let mut behind = 0;
    for i in 0..64 {
        let key = format!("late:{i}");
        set_with_ttl(&mut store, key.as_bytes(), 10, ms(2));
        let slot = store.key_index_slot(key.as_bytes()).expect("slotted");
        behind += u32::from(slot.wrapping_sub(begin) % capacity < walked);
    }
    assert!(behind > 0, "engagement: no refused write landed behind the cursor");
    drain_settled(&mut store, ms(20));
    assert_scheduled_when_idle(&store.expiry_audit(), "after the drain");
    assert_eq!(store.len(), 16, "{} expired writes outlived the drain", store.len() - 16);
}

/// O3's drain against a pass that was already under way: an expired write
/// refused behind its cursor at the drain's own instant was not visited,
/// so that pass must not settle the drain, though it began at that
/// instant. Only a pass that saw no refusal but its own vouches for every
/// record present when it ends. Same setup as the test above, with the
/// late writes already expired and the drain frozen at their instant.
#[test]
fn a_pass_dirtied_by_a_refused_write_does_not_settle_the_drain() {
    let cfg = StoreConfig { initial_keys: 1_024, ..small_cap(16) };
    let mut store = CellStore::new(cfg);
    let now = ms(2);
    for i in 0..16 {
        set_with_ttl(&mut store, format!("far:{i}").as_bytes(), 1_000_000, ms(0));
    }
    for i in 0..32 {
        set_with_ttl(&mut store, format!("wave:{i}").as_bytes(), 1, ms(0));
    }
    let capacity = store.index_capacity();
    let slice = ExpiryBudget { max_fires: u32::MAX, max_steps: u32::MAX, max_sweep_slots: 16 };
    let (begin, cursor) = (0..capacity)
        .find_map(|_| {
            store.expire_tick(now, slice);
            let (begin, cursor) = store.sweep_pass_slots()?;
            let walked = cursor.wrapping_sub(begin) % capacity;
            (walked >= capacity / 2).then_some((begin, cursor))
        })
        .expect("a sweep pass is under way and reaches mid-table");
    let walked = cursor.wrapping_sub(begin) % capacity;
    let mut behind = 0;
    for i in 0..64 {
        let key = format!("late:{i}");
        set_with_ttl(&mut store, key.as_bytes(), 1, now);
        let slot = store.key_index_slot(key.as_bytes()).expect("slotted");
        behind += u32::from(slot.wrapping_sub(begin) % capacity < walked);
    }
    assert!(behind > 0, "engagement: no expired write landed behind the cursor");
    drain_settled(&mut store, now);
    assert_eq!(store.len(), 16, "{} expired writes outlived the drain", store.len() - 16);
}

/// Rule 6 and O3's drain: an index rebuild moves records across the sweep
/// cursor, so a pass it voided vouches for nothing — it must not settle a
/// drain frozen at the instant it began. Setup: one record swept (the node
/// budget is 1), a pass begun at `now`, and plain writes at the same
/// instant that grow the index under it.
#[test]
fn a_voided_sweep_pass_does_not_settle_the_drain() {
    let mut store = CellStore::new(small_cap(1));
    let now = ms(10);
    set_with_ttl(&mut store, b"scheduled", 1_000_000, now);
    set_with_ttl(&mut store, b"swept", 1_000_000, now);
    let capacity = store.index_capacity();
    let slice = ExpiryBudget { max_fires: u32::MAX, max_steps: u32::MAX, max_sweep_slots: 16 };
    store.expire_tick(now, slice);
    assert!(store.sweep_pass_slots().is_some(), "a pass began at the frozen instant");
    for i in 0..capacity {
        if store.index_capacity() != capacity {
            break;
        }
        set_plain(&mut store, format!("grow:{i}").as_bytes(), now);
    }
    assert!(store.index_capacity() > capacity, "engagement: the index grew under the pass");
    for _ in 0..capacity {
        if store.sweep_pass_slots().is_none() {
            break;
        }
        store.expire_tick(now, slice);
    }
    assert_eq!(store.stats().sweep_passes_voided, 1, "engagement: the rebuild voided the pass");
    assert!(!store.expiry_settled(now), "a voided pass settled the drain at the instant it began");
    drain_settled(&mut store, now);
}

/// M1-S05's shared fire budget, in the unit each store's wheel budgets: a
/// fire. A fire whose group walk crosses ADR-0139 D9's member bound reaps
/// nothing, yet it is the most expensive fire (rule 5), so it must spend
/// the keyspace slice's budget — or every store holding such groups gets
/// the full budget again and one slice runs stores × `max_fires` walks.
#[test]
fn over_fires_spend_the_shared_keyspace_fire_budget() {
    let mut ks = Keyspace::new(StoreConfig::default());
    for db in 0..2u64 {
        for group in 0..3u64 {
            for member in 0..=IDX_ALIAS_GROUP_MAX as u8 {
                let key = alias_key(100 + db * 10 + group, member);
                set_with_ttl(ks.db_mut(db as usize), &key, 50, ms(1));
            }
        }
    }
    let budget = ExpiryBudget { max_fires: 2, max_steps: 4096, max_sweep_slots: 0 };
    ks.expire_tick(ms(1_000), budget);
    let fires: u64 =
        (0..2).map(|db| ks.db(db).expect("materialized").stats().expiry_alias_over).sum();
    assert!(fires > 0, "engagement: no fire crossed the group bound");
    assert!(fires <= u64::from(budget.max_fires), "{fires} fires in a slice budgeted 2");
}

/// A deadline behind an advanced wheel cursor (a writer whose clock reads
/// earlier than the last slice's) files at the cursor, so the next slice
/// reaps it — not a wheel revolution later, and never lazy-only.
#[test]
fn a_deadline_behind_the_wheel_cursor_is_reaped_by_the_next_slice() {
    let mut store = CellStore::new(StoreConfig::default());
    store.set(b"anchor", b"v", SetOptions::default(), ms(1)).expect("set");
    drain_settled(&mut store, ms(5_000));
    for (key, deadline_ms) in [(&b"a"[..], 4_900u64), (b"b", 5_000), (b"c", 4_999)] {
        set_with_ttl(&mut store, key, deadline_ms, ms(4_800));
    }
    let audit = store.expiry_audit();
    assert!(assert_scheduled_when_idle(&audit, "behind the cursor"), "placed, not swept");
    let next = store.expire_tick(ms(5_001), ExpiryBudget::UNBOUNDED);
    assert_eq!(next.reaped, 3, "reaped by the next slice");
    assert_eq!(store.len(), 1);
    assert_eq!(store.stats().expired_lazy, 0);
}
