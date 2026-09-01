//! M2-S10 AC: dirty-under-checkpoint correctness (mechanism tier — the
//! sim power-cut corpus binds at S18/S19). Mutations race the checkpoint
//! walker across bounded slices, every mutation also rides the real log
//! pipeline (staging ring → frames → MemFs segments), and recovery is the
//! S13 shape: load the `.ick`, then replay the tail from `ckpt-begin`.
//! The recovered state must digest-equal the live state — the ADR-0016 D3
//! fuzzy invariant, exercised with heavy overlap (writes, overwrites,
//! deletes, and expiries against keys before, during, and after their
//! walk slot).

use std::collections::BTreeMap;
use std::path::Path;

use inf_foundation::rng::{Entropy, SplitMix64};
use inf_foundation::time::Nanos;
use inf_log::ckpt::{IckReaderConfig, SyncIckWriter, ick_file_name, read_ick};
use inf_log::fs::mem::MemFs;
use inf_log::{
    CkptConfig, Lsn, MutationEffect, ReaderConfig, RecordView, SegmentConfig, SegmentReader,
    SegmentRotor, StagingConfig, StagingRing, create_cell_dirs, scan_log_dir,
};
use inf_store::{
    FsyncClass, Keyspace, NsCatalog, NsId, NsMode, NsSpec, ReplayOutcome, StoreConfig, WallAnchor,
};

const NS: u32 = 16;
const ANCHOR: WallAnchor = WallAnchor { internal_ms: 0, unix_ms: 0 };
const NOW: Nanos = Nanos::from_millis(50_000);
const KEYSPACE: u64 = 160;

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
        dropped: Vec::new(),
    };
    ks.seed_catalog(&catalog).expect("seed");
    ks
}

fn key_bytes(i: u64) -> Vec<u8> {
    format!("k:{i:04}").into_bytes()
}

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

/// The mutation pipeline of the plane, in miniature: apply to the store
/// AND stage the same effects the S08 emission hook produces.
fn mutate(ks: &mut Keyspace, staging: &mut StagingRing, rng: &mut SplitMix64) {
    let ns = NsId(NS);
    let key = key_bytes(rng.next_u64() % KEYSPACE);
    let store = ks.ns_store_mut(ns).expect("ns registered");
    match rng.next_u64() % 10 {
        // Delete (also covers deleting keys the walker already emitted).
        0..=1 => {
            store.replay_del(&key, NOW);
            staging.stage(&MutationEffect::Delete { ns, key: &key }).expect("stage");
        }
        // Set with a deadline; some already expired at NOW.
        2..=3 => {
            let value = vec![b'x'; (rng.next_u64() % 48) as usize];
            let at_ms = if rng.next_u64().is_multiple_of(4) {
                1_000 // long past: dead-on-arrival everywhere
            } else {
                100_000 + rng.next_u64() % 1_000_000
            };
            store.replay_set(&key, &value, NOW).expect("set");
            store.replay_expire_at(&key, Nanos::from_millis(at_ms), NOW);
            staging
                .stage(&MutationEffect::StringSet { ns, key: &key, value: &value })
                .expect("stage");
            let at_unix_ms = ANCHOR.unix_from_internal(Nanos::from_millis(at_ms));
            staging.stage(&MutationEffect::ExpireAt { ns, at_unix_ms, key: &key }).expect("stage");
        }
        // Plain set / overwrite.
        _ => {
            let value = vec![b'v'; (rng.next_u64() % 64) as usize];
            store.replay_set(&key, &value, NOW).expect("set");
            staging
                .stage(&MutationEffect::StringSet { ns, key: &key, value: &value })
                .expect("stage");
        }
    }
}

fn flush(staging: &mut StagingRing, rotor: &mut SegmentRotor<MemFs>) -> Option<Lsn> {
    staging.flush_into(rotor, 0).expect("flush").map(|lease| {
        let first = lease.first_record_lsn();
        staging.release(lease);
        first
    })
}

#[test]
fn mutations_racing_the_walker_recover_from_the_tail() {
    for seed in 0..8u64 {
        run_seed(seed);
    }
}

fn run_seed(seed: u64) {
    let mut rng = SplitMix64::new(0xF0221 ^ seed);
    let mut ks = durable_keyspace();

    // The real log pipeline on the fault-injectable MemFs.
    let fs = MemFs::new();
    let dirs = create_cell_dirs(&fs, Path::new("/shard-0")).expect("dirs");
    let mut rotor = SegmentRotor::create_fresh(
        fs.clone(),
        dirs.log.clone(),
        SegmentConfig { segment_bytes: 32 << 10, ..Default::default() },
    )
    .expect("rotor");
    let mut staging = StagingRing::new(StagingConfig::default());

    // Pre-checkpoint history: writes that are ONLY covered by the walker
    // (their records precede ckpt-begin and tail replay never sees them).
    for _ in 0..300 {
        mutate(&mut ks, &mut staging, &mut rng);
        if rng.next_u64().is_multiple_of(4) {
            flush(&mut staging, &mut rotor);
        }
    }

    // ckpt-begin rides the ordinary ring; its LSN comes from the lease.
    let at = staging.stage(&MutationEffect::CkptBegin { ckpt_id: 1 }).expect("stage begin");
    let lease = staging.flush_into(&mut rotor, 0).expect("flush").expect("begin frame");
    let begin_lsn = lease.lsn_of(at);
    staging.release(lease);

    // Stream the checkpoint in bounded slices, mutations interleaved with
    // heavy overlap — the fuzzy shape.
    let ckpt_dir = dirs.ckpt.clone();
    let cfg = CkptConfig { section_bytes: 512, ..Default::default() };
    let mut writer =
        SyncIckWriter::create(fs.clone(), &ckpt_dir, &cfg, 0, 1, begin_lsn, &[NS]).expect("create");
    let mut cursor = 0u64;
    loop {
        // A few racing mutations between walk slices.
        for _ in 0..(rng.next_u64() % 6) {
            mutate(&mut ks, &mut staging, &mut rng);
        }
        if rng.next_u64().is_multiple_of(3) {
            flush(&mut staging, &mut rotor);
        }
        // One bounded walk slice.
        let mut staged: Vec<(Vec<u8>, Vec<u8>, Option<u64>)> = Vec::new();
        let store = ks.ns_store_mut(NsId(NS)).expect("ns registered");
        cursor = store.scan_post_images(cursor, 8, NOW, |key, value, expire| {
            staged.push((key.to_vec(), value.to_vec(), expire));
        });
        for (key, value, expire) in &staged {
            let ns = NsId(NS);
            writer.append(&RecordView::StringPostImage { ns, key, value }).expect("append");
            if let Some(ms) = expire {
                let at_unix_ms = ANCHOR.unix_from_internal(Nanos::from_millis(*ms));
                writer.append(&RecordView::ExpireAt { ns, at_unix_ms, key }).expect("append");
            }
        }
        if cursor == 0 {
            break;
        }
    }
    writer.finish().expect("finish");

    // Post-walk mutations: covered purely by the tail.
    for _ in 0..100 {
        mutate(&mut ks, &mut staging, &mut rng);
        if rng.next_u64().is_multiple_of(4) {
            flush(&mut staging, &mut rotor);
        }
    }
    flush(&mut staging, &mut rotor);

    // ---- Recovery (the S13 shape): .ick + tail replay from begin. ----
    let mut recovered = durable_keyspace();
    read_ick(&fs, &ckpt_dir.join(ick_file_name(1)), IckReaderConfig::default(), |view| {
        recovered.apply_record(&view, NOW, ANCHOR).expect("apply ick");
        Ok::<(), ()>(())
    })
    .expect("load ick");

    let scan = scan_log_dir(&fs, &dirs.log).expect("scan");
    let mut markers = 0u64;
    for &segment in scan.segments() {
        let mut reader =
            SegmentReader::open(&fs, &dirs.log, segment, ReaderConfig::default()).expect("open");
        reader
            .apply_frames(|frame| {
                for record in frame.records() {
                    let (lsn, record) = record.expect("valid record");
                    if lsn.to_u64() < begin_lsn.to_u64() {
                        continue; // covered by the checkpoint
                    }
                    match recovered.apply_record(&record, NOW, ANCHOR).expect("apply tail") {
                        ReplayOutcome::SkippedMarker => markers += 1,
                        ReplayOutcome::Applied => {}
                        other => panic!("unexpected outcome {other:?}"),
                    }
                }
                Ok::<(), std::convert::Infallible>(())
            })
            .expect("replay");
    }
    assert_eq!(markers, 1, "the begin marker replays as a counted skip (seed {seed})");
    assert_eq!(
        contents(&mut ks),
        contents(&mut recovered),
        "checkpoint + tail from begin must equal the live state (seed {seed})"
    );
}
