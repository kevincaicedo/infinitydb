//! M0-S14/S15 AC: CellStore vs an in-memory reference model — random op
//! sequences over a small hot keyspace with TTLs and a virtual clock must
//! agree on every reply, and memory accounting must reconcile to zero after
//! a full sweep.

use std::collections::HashMap;

use inf_foundation::time::Nanos;
use inf_store::{
    CellStore, ExpireCond, OpError, SetCond, SetExpire, SetOptions, SetOutcome, StoreConfig, Ttl,
};

/// Reference model: value + optional absolute expiry (ms).
#[derive(Default)]
struct Model {
    map: HashMap<Vec<u8>, (Vec<u8>, Option<u64>)>,
}

impl Model {
    fn live(&mut self, key: &[u8], now_ms: u64) -> Option<&(Vec<u8>, Option<u64>)> {
        if let Some((_, Some(at))) = self.map.get(key)
            && *at <= now_ms
        {
            self.map.remove(key);
        }
        self.map.get(key)
    }

    fn get(&mut self, key: &[u8], now_ms: u64) -> Option<Vec<u8>> {
        self.live(key, now_ms).map(|(v, _)| v.clone())
    }
}

fn ms(now_ms: u64) -> Nanos {
    Nanos(now_ms * 1_000_000)
}

#[test]
fn storm_matches_reference_model() {
    let ops: usize = if cfg!(miri) { 1_500 } else { 120_000 };
    let mut store = CellStore::new(StoreConfig::default());
    let mut model = Model::default();
    let mut now_ms: u64 = 1;

    let mut x: u64 = 0x0DDB_A115_EED7_25A5;
    let mut rand = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };

    for op in 0..ops {
        // Time moves forward in small jumps so TTLs really fire mid-storm.
        if rand() % 8 == 0 {
            now_ms += rand() % 50;
        }
        let now = ms(now_ms);
        let key = format!("k{}", rand() % 192).into_bytes();
        match rand() % 10 {
            0..=2 => {
                // SET with random cond/expire/get_old.
                let value = format!("v{}", rand() % 100_000).into_bytes();
                let cond = match rand() % 3 {
                    0 => SetCond::Always,
                    1 => SetCond::IfAbsent,
                    _ => SetCond::IfPresent,
                };
                let expire = match rand() % 4 {
                    0 => SetExpire::Keep,
                    1 => SetExpire::At(ms(now_ms + 1 + rand() % 100)),
                    _ => SetExpire::Clear,
                };
                let get_old = rand() % 2 == 0;
                let got = store
                    .set(&key, &value, SetOptions { cond, expire, get_old }, now)
                    .expect("set never OOMs unbudgeted");
                // Model.
                let existing = model.get(&key, now_ms);
                let applies = match cond {
                    SetCond::Always => true,
                    SetCond::IfAbsent => existing.is_none(),
                    SetCond::IfPresent => existing.is_some(),
                };
                let old = if get_old { existing.clone() } else { None };
                let want = if applies {
                    let at = match expire {
                        SetExpire::Clear => None,
                        SetExpire::Keep => model.live(&key, now_ms).and_then(|(_, at)| *at),
                        SetExpire::At(n) => Some(n.0 / 1_000_000),
                    };
                    model.map.insert(key.clone(), (value, at));
                    SetOutcome::Applied { old }
                } else {
                    SetOutcome::Skipped { old }
                };
                assert_eq!(got, want, "op {op}: SET disagreed for {key:?}");
            }
            3 => {
                let got = store.del(&key, now);
                let want = model.live(&key, now_ms).is_some();
                model.map.remove(&key);
                assert_eq!(got, want, "op {op}: DEL disagreed");
            }
            4 => {
                let got = store.get(&key, now).map(<[u8]>::to_vec);
                assert_eq!(got, model.get(&key, now_ms), "op {op}: GET disagreed");
            }
            5 => {
                let got = store.exists(&key, now);
                assert_eq!(got, model.get(&key, now_ms).is_some(), "op {op}: EXISTS");
            }
            6 => {
                let delta = (rand() % 2000) as i64 - 1000;
                let got = store.incr_by(&key, delta, now);
                let existing = model.get(&key, now_ms);
                let want: Result<i64, OpError> = match existing {
                    None => Ok(delta),
                    Some(v) => match std::str::from_utf8(&v).ok().and_then(|s| {
                        // Model-side string2ll: no leading zeros, no "-0".
                        if (s.len() > 1 && (s.starts_with('0') || s.starts_with("-0"))) || s == "-0"
                        {
                            None
                        } else {
                            s.parse::<i64>().ok()
                        }
                    }) {
                        None => Err(OpError::NotInt),
                        Some(cur) => cur.checked_add(delta).ok_or(OpError::Overflow),
                    },
                };
                assert_eq!(got, want, "op {op}: INCRBY disagreed for {key:?}");
                if let Ok(next) = got {
                    let at = model.live(&key, now_ms).and_then(|(_, at)| *at);
                    model.map.insert(key.clone(), (next.to_string().into_bytes(), at));
                }
            }
            7 => {
                let tail = format!("t{}", rand() % 100).into_bytes();
                let got = store.append(&key, &tail, now).expect("append fits");
                let mut want = model.get(&key, now_ms).unwrap_or_default();
                want.extend_from_slice(&tail);
                assert_eq!(got, want.len() as u64, "op {op}: APPEND length");
                let at = model.live(&key, now_ms).and_then(|(_, at)| *at);
                model.map.insert(key.clone(), (want, at));
            }
            8 => {
                // EXPIRE / PERSIST with random conditions.
                let (at, cond) = if rand() % 4 == 0 {
                    (None, ExpireCond::Always) // PERSIST
                } else {
                    let cond = match rand() % 5 {
                        0 => ExpireCond::IfNoExpiry,
                        1 => ExpireCond::IfHasExpiry,
                        2 => ExpireCond::IfGreater,
                        3 => ExpireCond::IfLess,
                        _ => ExpireCond::Always,
                    };
                    (Some(ms(now_ms + rand() % 200)), cond)
                };
                let got = store.expire(&key, at, cond, now);
                let want = {
                    let new_ms = at.map(|n| n.0 / 1_000_000);
                    match model.live(&key, now_ms) {
                        None => false,
                        Some((_, cur)) => {
                            let cur = *cur;
                            let applies = match cond {
                                ExpireCond::Always => true,
                                ExpireCond::IfNoExpiry => cur.is_none(),
                                ExpireCond::IfHasExpiry => cur.is_some(),
                                ExpireCond::IfGreater => {
                                    matches!((new_ms, cur), (Some(n), Some(c)) if n > c)
                                }
                                ExpireCond::IfLess => match (new_ms, cur) {
                                    (Some(n), Some(c)) => n < c,
                                    (Some(_), None) => true,
                                    (None, _) => false,
                                },
                            };
                            if !applies || (new_ms.is_none() && cur.is_none()) {
                                false
                            } else if let Some(n) = new_ms
                                && n <= now_ms
                            {
                                model.map.remove(&key);
                                true
                            } else {
                                let entry = model.map.get_mut(&key).expect("checked live above");
                                entry.1 = new_ms;
                                true
                            }
                        }
                    }
                };
                assert_eq!(got, want, "op {op}: EXPIRE disagreed for {key:?}");
            }
            _ => {
                let got = store.ttl(&key, now);
                let want = match model.live(&key, now_ms) {
                    None => Ttl::Missing,
                    Some((_, None)) => Ttl::NoExpiry,
                    Some((_, Some(at))) => Ttl::Ms(at.saturating_sub(now_ms)),
                };
                assert_eq!(got, want, "op {op}: TTL disagreed for {key:?}");
            }
        }
    }

    // Full sweep: read every model key (reaping expired), then counts match.
    let keys: Vec<Vec<u8>> = model.map.keys().cloned().collect();
    let now = ms(now_ms);
    for key in &keys {
        assert_eq!(
            store.get(key, now).map(<[u8]>::to_vec),
            model.get(key, now_ms),
            "final sweep: {key:?}"
        );
    }
    // Reap everything store-side (keys the model dropped may still hold
    // arena bytes until read — expire-on-read by design), then reconcile.
    for i in 0..192u64 {
        let key = format!("k{i}").into_bytes();
        store.del(&key, now);
        model.map.remove(&key);
    }
    assert!(model.map.is_empty());
    let report = store.report();
    assert_eq!(report.records_live_bytes, 0, "byte-exact zero after full delete");
    assert_eq!(report.live_records, 0);
    assert_eq!(store.len(), 0);
}

#[test]
fn get_many_matches_get_including_duplicate_expired_keys() {
    let mut store = CellStore::new(StoreConfig::default());
    store.set(b"live", b"v", SetOptions::default(), ms(1)).expect("set");
    store
        .set(
            b"dying",
            b"gone",
            SetOptions { expire: SetExpire::At(ms(5)), ..Default::default() },
            ms(1),
        )
        .expect("set");
    // The same expired key twice in one batch: the first occurrence reaps
    // the record mid-chunk; the second must NOT read the freed slot.
    let keys: [&[u8]; 5] = [b"dying", b"live", b"dying", b"missing", b"live"];
    let mut got: Vec<Option<Vec<u8>>> = vec![None; keys.len()];
    store.get_many(&keys, ms(10), |i, v| got[i] = v.map(<[u8]>::to_vec));
    assert_eq!(
        got,
        vec![None, Some(b"v".to_vec()), None, None, Some(b"v".to_vec())],
        "batched results match scalar semantics"
    );
    assert_eq!(store.len(), 1, "expired key reaped exactly once");
}

#[test]
fn getdel_and_type_basics() {
    let mut store = CellStore::new(StoreConfig::default());
    let now = Nanos(0);
    assert_eq!(store.getdel(b"nope", now), None);
    store.set(b"k", b"v1", SetOptions::default(), now).expect("set");
    assert_eq!(store.type_of(b"k", now), Some(inf_store::TypeTag::String));
    assert_eq!(store.getdel(b"k", now), Some(b"v1".to_vec()));
    assert_eq!(store.get(b"k", now), None);
    assert_eq!(store.report().records_live_bytes, 0);
}

#[test]
fn out_of_memory_is_an_error_not_a_panic() {
    let mut store = CellStore::new(StoreConfig {
        arena: inf_alloc::ArenaConfig { chunk_size: 64 << 10, max_resident: Some(64 << 10) },
        initial_keys: 16,
    });
    let now = Nanos(0);
    let value = vec![0xAB; 1 << 10];
    let mut wrote = 0;
    loop {
        let key = format!("fill:{wrote}").into_bytes();
        match store.set(&key, &value, SetOptions::default(), now) {
            Ok(_) => wrote += 1,
            Err(OpError::OutOfMemory) => break,
            Err(e) => panic!("unexpected error: {e:?}"),
        }
        assert!(wrote < 10_000, "budget never enforced");
    }
    assert!(wrote > 0, "some writes landed before the budget");
    // The store stays consistent after OOM: reads and deletes still work.
    assert!(store.get(b"fill:0", now).is_some());
    assert!(store.del(b"fill:0", now));
}
