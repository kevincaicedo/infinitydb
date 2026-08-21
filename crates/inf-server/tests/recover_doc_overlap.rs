//! M3-S17 fuzzy-overlap boot test (ADR-0043 R1/R2): a checkpoint may
//! contain a newer document image than an early tail delta, or omit a key
//! whose later delete it observed. Recovery counts both skips and reaches
//! the same canonical state/version as live execution.
#![cfg(feature = "doc")]

use std::path::{Path, PathBuf};

use inf_doc::apply::{ApplyOp, Number};
use inf_doc::path::compile;
use inf_doc::{JsonParser, encode_apply_op};
use inf_foundation::time::Nanos;
use inf_log::ckpt::SyncIckWriter;
use inf_log::fs::mem::MemFs;
use inf_log::{
    CkptConfig, DocLineage, FsyncClass, Manifest, MutationEffect, NsId, RecordView, SegmentConfig,
    SegmentId, StagingConfig, StagingRing, create_cell_dirs, write_manifest,
};
use inf_server::{DurableConfig, open_cell_log};
use inf_store::{Keyspace, NsMode, NsSpec, StoreConfig, WallAnchor};

const NS: NsId = NsId(16);
const NOW: Nanos = Nanos::from_millis(1);
const ANCHOR: WallAnchor = WallAnchor { internal_ms: 0, unix_ms: 0 };
const LINEAGE_1: DocLineage = DocLineage::FIRST;
const LINEAGE_2: DocLineage = match DocLineage::new(2) {
    Some(lineage) => lineage,
    None => unreachable!(),
};

fn config() -> DurableConfig {
    DurableConfig {
        data_dir: PathBuf::from("data"),
        staging: StagingConfig::default(),
        segment: SegmentConfig { segment_bytes: 1 << 16, ..Default::default() },
        ckpt: CkptConfig::default(),
        recover: Default::default(),
        flush_bound: 1,
        fua_p50_us_probed: 0,
        device: Default::default(),
    }
}

fn keyspace() -> Keyspace {
    let mut ks = Keyspace::new(StoreConfig::default());
    ks.ns_create(NsSpec {
        id: NS,
        name: b"docs".to_vec(),
        mode: NsMode::Durable,
        fsync: Some(FsyncClass::Always),
        policy: None,
        maxmemory: None,
        tier: None,
    })
    .expect("namespace");
    ks
}

#[test]
fn fuzzy_checkpoint_overlap_counts_stale_and_missing_deltas() {
    let fs = MemFs::new();
    let cfg = config();
    let dirs = create_cell_dirs(&fs, Path::new("data/shard-0")).expect("dirs");
    let mut rotor =
        inf_log::SegmentRotor::create_fresh(fs.clone(), dirs.log, cfg.segment).expect("rotor");
    let mut ring = StagingRing::new(cfg.staging);

    let marker = ring.stage(&MutationEffect::CkptBegin { ckpt_id: 1 }).expect("marker");
    let lease = ring.flush_into(&mut rotor, 0).expect("flush").expect("marker frame");
    let begin_lsn = lease.lsn_of(marker);
    ring.release(lease);

    // The fuzzy walker saw key `a` after its first delta (version 2), and
    // saw key `b` after a later delete, so `b` is absent from the image.
    let checkpoint_a = JsonParser::new().parse(br#"{"n":2}"#).expect("fixture");
    let mut writer =
        SyncIckWriter::create(fs.clone(), &dirs.ckpt, &cfg.ckpt, 0, 1, begin_lsn, &[NS.0])
            .expect("ick");
    writer
        .append(&RecordView::DocFull {
            ns: NS,
            key: b"a",
            lineage: LINEAGE_1,
            version: 2,
            idoc: &checkpoint_a,
        })
        .expect("doc full");
    // Key `c` was deleted and recreated after an old tail delta. The
    // walker captured the new incarnation, so replay must skip that old
    // delta even when its u24 version could collide.
    let checkpoint_c = JsonParser::new().parse(br#"{"other":true}"#).expect("fixture");
    writer
        .append(&RecordView::DocFull {
            ns: NS,
            key: b"c",
            lineage: LINEAGE_2,
            version: 1,
            idoc: &checkpoint_c,
        })
        .expect("recreated doc full");
    writer.finish().expect("publish ick");

    let program = compile(b"$.n").expect("path");
    let mut operand = Vec::new();
    let opcode = encode_apply_op(&ApplyOp::NumIncrBy(Number::I64(1)), &mut operand) as u8;
    let stale = MutationEffect::DocDelta {
        ns: NS,
        key: b"a",
        lineage: LINEAGE_1,
        base_version: 1,
        match_count: 1,
        post_len: checkpoint_a.len() as u32,
        opcode,
        program: program.as_bytes(),
        operand: &operand,
    };
    let apply = MutationEffect::DocDelta {
        ns: NS,
        key: b"a",
        lineage: LINEAGE_1,
        base_version: 2,
        match_count: 1,
        post_len: checkpoint_a.len() as u32,
        opcode,
        program: program.as_bytes(),
        operand: &operand,
    };
    let missing = MutationEffect::DocDelta {
        ns: NS,
        key: b"b",
        lineage: LINEAGE_1,
        base_version: 1,
        match_count: 1,
        post_len: checkpoint_a.len() as u32,
        opcode,
        program: program.as_bytes(),
        operand: &operand,
    };
    for effect in [stale, apply, missing, MutationEffect::Delete { ns: NS, key: b"b" }] {
        ring.stage(&effect).expect("tail record");
    }
    let old_c = MutationEffect::DocDelta {
        ns: NS,
        key: b"c",
        lineage: LINEAGE_1,
        base_version: 4,
        match_count: 1,
        post_len: checkpoint_a.len() as u32,
        opcode,
        program: program.as_bytes(),
        operand: &operand,
    };
    ring.stage(&old_c).expect("old-incarnation delta");
    ring.stage(&MutationEffect::Delete { ns: NS, key: b"c" }).expect("recreate delete");
    ring.stage(&MutationEffect::DocFull {
        ns: NS,
        key: b"c",
        lineage: LINEAGE_2,
        version: 1,
        idoc: &checkpoint_c,
    })
    .expect("recreate full");
    let lease = ring.flush_into(&mut rotor, 0).expect("flush").expect("tail frame");
    ring.release(lease);

    write_manifest(
        &fs,
        Path::new("data/shard-0"),
        &Manifest { ckpt_id: 1, begin_lsn, segments: vec![SegmentId(0)], tiers: Vec::new() },
    )
    .expect("manifest");
    drop(rotor);

    let mut recovered = keyspace();
    let (_rotor, stats, _manifest) =
        open_cell_log(fs, &mut recovered, 0, &cfg, ANCHOR, NOW).expect("boot recovery");
    assert_eq!(stats.doc_deltas_skipped_stale, 2);
    assert_eq!(stats.doc_deltas_skipped_missing, 1);

    let store = recovered.ns_store_mut(NS).expect("namespace");
    let read = store.json_get(b"a", NOW).unwrap().unwrap();
    assert_eq!(read.version, 3);
    let want = JsonParser::new().parse(br#"{"n":3}"#).expect("fixture");
    assert_eq!(store.json_freeze(b"a", NOW).unwrap().unwrap(), want);
    assert!(store.json_get(b"b", NOW).unwrap().is_none());
    assert_eq!(store.json_freeze(b"c", NOW).unwrap().unwrap(), checkpoint_c);
}
