//! M2-S10 AC: checkpoint a randomized cell state → load the `.ick` →
//! state digest equal (empty tail). The walk streams through
//! `CellStore::scan_post_images` (the resize-stable SCAN cursor) into
//! `inf-log`'s `SyncIckWriter` over the fault-injectable `MemFs`, and the
//! loader replays through the same `Keyspace::apply_record` upsert the
//! log tail uses (ADR-0016 D1 — one replay vocabulary).

use std::collections::BTreeMap;
use std::path::Path;

use inf_foundation::time::Nanos;
use inf_log::ckpt::{IckReaderConfig, SyncIckWriter, ick_file_name, read_ick};
use inf_log::fs::SegmentFs;
use inf_log::fs::mem::MemFs;
use inf_log::{CkptConfig, Lsn, RecordView, SegmentId};
use inf_store::{
    FsyncClass, Keyspace, NsCatalog, NsId, NsMode, NsSpec, ReplayOutcome, StoreConfig, WallAnchor,
};
use proptest::prelude::*;

const NS: u32 = 16;
/// Identity anchor: unix ms == internal ms (deadlines survive round-trip
/// byte-exactly).
const ANCHOR: WallAnchor = WallAnchor { internal_ms: 0, unix_ms: 0 };
const NOW: Nanos = Nanos::from_millis(10_000);

fn durable_keyspace() -> Keyspace {
    let mut ks = Keyspace::new(StoreConfig::default());
    let catalog = NsCatalog {
        next_id: NS + 1,
        entries: vec![NsSpec {
            id: NsId(NS),
            name: b"ledger".to_vec(),
            mode: NsMode::Durable,
            fsync: Some(FsyncClass::Everysec),
            policy: None,
            maxmemory: None,
            tier: None,
        }],
        index: Default::default(),
    };
    ks.seed_catalog(&catalog).expect("seed");
    ks
}

/// Full-content map of the durable namespace: `key → (value, expire_ms)`.
/// Walked with the same cursor the checkpoint uses; entries expired at
/// `NOW` never appear (reaped, not emitted).
fn contents(ks: &mut Keyspace) -> BTreeMap<Vec<u8>, (Vec<u8>, Option<u64>)> {
    let mut map = BTreeMap::new();
    let store = ks.ns_store_mut(NsId(NS)).expect("ns registered");
    let mut cursor = 0u64;
    loop {
        cursor = store.scan_post_images(cursor, 64, NOW, |key, value, expire| {
            map.insert(key.to_vec(), (value.to_vec(), expire));
        });
        if cursor == 0 {
            return map;
        }
    }
}

#[derive(Clone, Debug)]
enum Op {
    Set { key: u8, value: Vec<u8> },
    SetWithDeadline { key: u8, value: Vec<u8>, at_ms: u64 },
    Del { key: u8 },
}

fn key_bytes(key: u8) -> Vec<u8> {
    format!("k:{key:03}").into_bytes()
}

fn apply(ks: &mut Keyspace, op: &Op) {
    let store = ks.ns_store_mut(NsId(NS)).expect("ns registered");
    match op {
        Op::Set { key, value } => store.replay_set(&key_bytes(*key), value, NOW).expect("set"),
        Op::SetWithDeadline { key, value, at_ms } => {
            let key = key_bytes(*key);
            store.replay_set(&key, value, NOW).expect("set");
            store.replay_expire_at(&key, Nanos::from_millis(*at_ms), NOW);
        }
        Op::Del { key } => store.replay_del(&key_bytes(*key), NOW),
    }
}

fn op_strategy() -> impl Strategy<Value = Op> {
    let value = || proptest::collection::vec(any::<u8>(), 0..96);
    prop_oneof![
        (any::<u8>(), value()).prop_map(|(key, value)| Op::Set { key, value }),
        // Deadlines both sides of NOW: past ⇒ expired (absent from the
        // checkpoint AND from the recovered state), future ⇒ carried.
        (any::<u8>(), value(), 1_000u64..1_000_000)
            .prop_map(|(key, value, at_ms)| Op::SetWithDeadline { key, value, at_ms }),
        any::<u8>().prop_map(|key| Op::Del { key }),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, ..ProptestConfig::default() })]

    #[test]
    fn randomized_state_round_trips_through_ick(
        ops in proptest::collection::vec(op_strategy(), 0..300),
        section_bytes in 64u32..4096,
    ) {
        let mut ks = durable_keyspace();
        for op in &ops {
            apply(&mut ks, &op.clone());
        }

        // Checkpoint the (quiesced) state: walk in small bounded calls —
        // the multi-slice shape — into the .ick.
        let fs = MemFs::new();
        let dir = Path::new("/ckpt");
        fs.create_dir_all(dir).unwrap();
        let cfg = CkptConfig { section_bytes, ..Default::default() };
        let begin = Lsn::new(SegmentId(3), 128);
        let mut writer =
            SyncIckWriter::create(fs.clone(), dir, &cfg, 0, 1, begin, &[NS]).expect("create");
        {
            let store = ks.ns_store_mut(NsId(NS)).expect("ns registered");
            let mut cursor = 0u64;
            loop {
                let mut staged: Vec<(Vec<u8>, Vec<u8>, Option<u64>)> = Vec::new();
                cursor = store.scan_post_images(cursor, 8, NOW, |key, value, expire| {
                    staged.push((key.to_vec(), value.to_vec(), expire));
                });
                for (key, value, expire) in &staged {
                    let ns = NsId(NS);
                    writer
                        .append(&RecordView::StringPostImage { ns, key, value })
                        .expect("append");
                    if let Some(ms) = expire {
                        let at_unix_ms = ANCHOR.unix_from_internal(Nanos::from_millis(*ms));
                        writer
                            .append(&RecordView::ExpireAt { ns, at_unix_ms, key })
                            .expect("append");
                    }
                }
                if cursor == 0 {
                    break;
                }
            }
        }
        let summary = writer.finish().expect("finish");

        // Load into a fresh keyspace (empty tail) and compare contents.
        let mut recovered = durable_keyspace();
        let (info, audit) = read_ick(
            &fs,
            &dir.join(ick_file_name(1)),
            IckReaderConfig::default(),
            |view| {
                let outcome = recovered.apply_record(&view, NOW, ANCHOR).expect("apply");
                assert!(matches!(outcome, ReplayOutcome::Applied), "checkpoint records apply");
                Ok::<(), ()>(())
            },
        )
        .expect("load");
        prop_assert_eq!(info.begin_lsn, begin);
        prop_assert_eq!(&info.ns_ids, &vec![NS]);
        prop_assert_eq!(audit, summary, "loader audit reproduces the writer summary");
        prop_assert_eq!(contents(&mut ks), contents(&mut recovered));
    }
}
