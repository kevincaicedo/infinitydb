//! F-L03-03 (review of 2026-08-30; ADR-0028 A1): the recovery driver's
//! presize peek locates the `.ick` footer by the end-of-file probe even
//! when a durable namespace the header names contributed no entry —
//! the footer then lists fewer namespaces than the header, which used
//! to defeat the probe into the dependent hop chain on every boot.

use std::path::{Path, PathBuf};

use inf_foundation::time::Nanos;
use inf_log::ckpt::{SyncIckWriter, ick_file_name};
use inf_log::fs::mem::MemFs;
use inf_log::fs::{SegmentFile, SegmentFs};
use inf_log::{
    CkptConfig, Manifest, MutationEffect, NsId, RecordView, SegmentConfig, SegmentRotor,
    StagingConfig, StagingRing, create_cell_dirs, segment_file_name, write_manifest,
};
use inf_server::{DurableConfig, open_cell_log};
use inf_store::{FsyncClass, KeyHasher, Keyspace, NsMode, NsSpec, StoreConfig, WallAnchor};

const FULL: NsId = NsId(16);
const EMPTY: NsId = NsId(17);
const CELL: u16 = 0;
const SHARD: &str = "data/shard-0";

fn cfg() -> DurableConfig {
    DurableConfig {
        data_dir: PathBuf::from("data"),
        staging: StagingConfig::default(),
        segment: SegmentConfig { segment_bytes: 64 << 10, ..Default::default() },
        ckpt: CkptConfig::default(),
        recover: Default::default(),
        flush_bound: 1,
        fua_p50_us_probed: 0,
        device: Default::default(),
        fill: Default::default(),
        group: Default::default(),
    }
}

fn keyspace() -> Keyspace {
    let mut ks = Keyspace::new(StoreConfig::default());
    for (id, name) in [(FULL, &b"ledger"[..]), (EMPTY, &b"fresh"[..])] {
        ks.ns_create(NsSpec {
            id,
            name: name.to_vec(),
            mode: NsMode::Durable,
            fsync: Some(FsyncClass::Always),
            policy: None,
            maxmemory: None,
            tier: None,
        })
        .expect("ns");
    }
    ks
}

/// A shard whose published checkpoint names two durable namespaces in
/// its header and carries images for one — many sections, so the hop
/// chain would be long.
fn build_shard(fs: &MemFs, populated: &[NsId]) {
    let config = cfg();
    let dirs = create_cell_dirs(fs, Path::new(SHARD)).expect("dirs");
    let mut rotor =
        SegmentRotor::create_fresh(fs.clone(), dirs.log.clone(), config.segment).expect("rotor");
    let mut ring = StagingRing::new(config.staging);
    ring.stage(&MutationEffect::CkptBegin { ckpt_id: 1 }).expect("stage");
    rotor.maintain(0).expect("maintain");
    let slot = rotor.begin_frame(ring.pending_frame_len(), 0).expect("reserve");
    let begin_lsn = slot.first_record_lsn();
    let lease = ring.seal(begin_lsn, 0, slot.layout());
    let frame = ring.leased_frame(&lease).to_vec();
    rotor.commit_frame(slot, &frame).expect("commit");
    ring.release(lease);
    let mut file = fs
        .open_write(&dirs.log.join(segment_file_name(rotor.active_segment())))
        .expect("active segment");
    file.sync_data().expect("fdatasync");
    drop(file);

    let mut w = SyncIckWriter::create_v3(
        fs.clone(),
        &Path::new(SHARD).join("ckpt"),
        &CkptConfig { section_bytes: 512, ..Default::default() },
        CELL,
        1,
        begin_lsn,
        &[FULL.0, EMPTY.0],
    )
    .expect("create v3");
    for &ns in populated {
        for i in 0..200u32 {
            let key = format!("k:{i:04}").into_bytes();
            let value = vec![b'v'; 40];
            w.append(&RecordView::StringPostImage { ns, key: &key, value: &value }).expect("image");
        }
    }
    let summary = w.finish().expect("publish");
    assert!(summary.sections >= 8, "a long hop chain: {} sections", summary.sections);
    assert_eq!(summary.entries_per_ns.len(), populated.len(), "the footer names the populated set");
    write_manifest(
        fs,
        Path::new(SHARD),
        &Manifest {
            ckpt_id: 1,
            begin_lsn,
            segments: vec![begin_lsn.segment],
            tiers: vec![],
            key_hash_id: KeyHasher::default().identity(),
        },
    )
    .expect("manifest");
}

fn boot(fs: &MemFs) -> inf_server::RecoverStats {
    let mut ks = keyspace();
    let (_rotor, stats, manifest) = open_cell_log(
        fs.clone(),
        &mut ks,
        CELL,
        &cfg(),
        WallAnchor { internal_ms: 0, unix_ms: 1_750_000_000_000 },
        Nanos::from_millis(1),
    )
    .expect("boot");
    assert_eq!(manifest.map(|m| m.ckpt_id), Some(1));
    assert_eq!(stats.ckpt_records, 200, "the checkpoint loaded");
    assert_eq!(ks.ns_store_mut(FULL).expect("ns").len(), 200);
    stats
}

#[test]
fn an_empty_durable_namespace_does_not_defeat_the_footer_probe() {
    let fs = MemFs::new();
    build_shard(&fs, &[FULL]);
    let stats = boot(&fs);
    assert_eq!(
        stats.ckpt_footer_probe_hit,
        Some(true),
        "F-L03-03: the header names {{16, 17}}, the footer lists 16 alone — the probe must \
         still locate the footer instead of hopping every section"
    );
    let path = Path::new(SHARD).join("ckpt").join(ick_file_name(1));
    let sections = fs.contents(&path).expect("ick").len() / 4096;
    assert!(sections >= 8, "{sections} aligned blocks: the hop chain the probe replaces");
}
