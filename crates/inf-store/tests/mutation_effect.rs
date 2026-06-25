use inf_foundation::time::Nanos;
use inf_store::{
    CellStore, ExpireCond, MutationEffect, MutationSink, SetCond, SetExpire, SetOptions,
    SetOutcome, StoreConfig,
};

fn ms(value: u64) -> Nanos {
    Nanos(value * 1_000_000)
}

#[derive(Default)]
struct Recorder {
    effects: Vec<OwnedEffect>,
}

impl MutationSink for Recorder {
    fn push(&mut self, effect: MutationEffect<'_>) {
        self.effects.push(OwnedEffect::from(effect));
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
enum OwnedEffect {
    StringPostImage { key: Vec<u8>, value: Vec<u8>, expire_at_ms: Option<u64>, raw: bool },
    Delete { key: Vec<u8> },
    ExpireAt { key: Vec<u8>, expire_at_ms: Option<u64> },
}

impl From<MutationEffect<'_>> for OwnedEffect {
    fn from(effect: MutationEffect<'_>) -> OwnedEffect {
        match effect {
            MutationEffect::StringPostImage { key, value, expire_at_ms, raw } => {
                OwnedEffect::StringPostImage {
                    key: key.to_vec(),
                    value: value.to_vec(),
                    expire_at_ms,
                    raw,
                }
            }
            MutationEffect::Delete { key } => OwnedEffect::Delete { key: key.to_vec() },
            MutationEffect::ExpireAt { key, expire_at_ms } => {
                OwnedEffect::ExpireAt { key: key.to_vec(), expire_at_ms }
            }
        }
    }
}

#[test]
fn set_effect_reports_only_applied_post_image() {
    let mut store = CellStore::new(StoreConfig::default());
    let mut recorder = Recorder::default();
    let now = ms(10);
    let opts = SetOptions { expire: SetExpire::At(ms(50)), ..Default::default() };

    let outcome = store.set_with_effect(b"k", b"v", opts, now, &mut recorder).unwrap();
    assert_eq!(outcome, SetOutcome::Applied { old: None });
    assert_eq!(
        recorder.effects,
        vec![OwnedEffect::StringPostImage {
            key: b"k".to_vec(),
            value: b"v".to_vec(),
            expire_at_ms: Some(50),
            raw: false,
        }]
    );

    let opts = SetOptions { cond: SetCond::IfAbsent, ..Default::default() };
    let outcome = store.set_with_effect(b"k", b"ignored", opts, now, &mut recorder).unwrap();
    assert_eq!(outcome, SetOutcome::Skipped { old: None });
    assert_eq!(recorder.effects.len(), 1, "skipped writes must not emit effects");
}

#[test]
fn delete_effect_reports_only_existing_key() {
    let mut store = CellStore::new(StoreConfig::default());
    let mut recorder = Recorder::default();
    let now = ms(1);

    store.set(b"k", b"v", SetOptions::default(), now).unwrap();
    assert!(store.del_with_effect(b"k", now, &mut recorder));
    assert!(!store.del_with_effect(b"k", now, &mut recorder));

    assert_eq!(recorder.effects, vec![OwnedEffect::Delete { key: b"k".to_vec() }]);
}

#[test]
fn getdel_effect_reports_value_and_delete_only_when_present() {
    let mut store = CellStore::new(StoreConfig::default());
    let mut recorder = Recorder::default();
    let now = ms(1);

    store.set(b"k", b"v", SetOptions::default(), now).unwrap();
    assert_eq!(store.getdel_with_effect(b"k", now, &mut recorder), Some(b"v".to_vec()));
    assert_eq!(store.getdel_with_effect(b"k", now, &mut recorder), None);

    assert_eq!(recorder.effects, vec![OwnedEffect::Delete { key: b"k".to_vec() }]);
}

#[test]
fn expire_effect_reports_ttl_update_persist_and_delete() {
    let mut store = CellStore::new(StoreConfig::default());
    let mut recorder = Recorder::default();
    let now = ms(10);

    store.set(b"k", b"v", SetOptions::default(), now).unwrap();
    assert!(store.expire_with_effect(b"k", Some(ms(20)), ExpireCond::Always, now, &mut recorder));
    assert!(store.expire_with_effect(b"k", None, ExpireCond::Always, now, &mut recorder));
    assert!(store.expire_with_effect(b"k", Some(ms(5)), ExpireCond::Always, now, &mut recorder));

    assert_eq!(
        recorder.effects,
        vec![
            OwnedEffect::ExpireAt { key: b"k".to_vec(), expire_at_ms: Some(20) },
            OwnedEffect::ExpireAt { key: b"k".to_vec(), expire_at_ms: None },
            OwnedEffect::Delete { key: b"k".to_vec() },
        ]
    );
}

#[test]
fn getex_effect_reports_ttl_update_persist_and_delete() {
    let mut store = CellStore::new(StoreConfig::default());
    let mut recorder = Recorder::default();
    let now = ms(10);

    store.set(b"k", b"v", SetOptions::default(), now).unwrap();
    assert_eq!(
        store.get_ex_with_effect(b"k", inf_store::TtlUpdate::Keep, now, &mut recorder),
        Some(b"v".to_vec())
    );
    assert!(recorder.effects.is_empty(), "plain GETEX must not emit an effect");

    assert_eq!(
        store.get_ex_with_effect(b"k", inf_store::TtlUpdate::At(ms(20)), now, &mut recorder),
        Some(b"v".to_vec())
    );
    assert_eq!(
        store.get_ex_with_effect(b"k", inf_store::TtlUpdate::Persist, now, &mut recorder),
        Some(b"v".to_vec())
    );
    assert_eq!(
        store.get_ex_with_effect(b"k", inf_store::TtlUpdate::At(ms(5)), now, &mut recorder),
        Some(b"v".to_vec())
    );

    assert_eq!(
        recorder.effects,
        vec![
            OwnedEffect::ExpireAt { key: b"k".to_vec(), expire_at_ms: Some(20) },
            OwnedEffect::ExpireAt { key: b"k".to_vec(), expire_at_ms: None },
            OwnedEffect::Delete { key: b"k".to_vec() },
        ]
    );
}
