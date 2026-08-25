//! M4.5-S36 `.ick` container v3 crash rows (ADR-0088 D3/D8): the aligned,
//! direct-written checkpoint under the recovery driver's unchanged rules —
//! "a file named `*.ick` is footer-complete; the MANIFEST is the only
//! authority; framing damage in a named unit is corruption (§8.4)",
//! re-asserted now that blocks carry padding.
//!
//! - `v3-named-unit-loads` — the canary: a published v3 unit boots
//!   through `open_cell_log`, every image loads, the tail replays over it.
//! - `v3-mid-section-orphan` — the cut lands inside a section of a
//!   direct `.ick.new` (an unaligned truncation point: a torn block): the
//!   old unit stays authoritative and loads; the orphan is GC'd.
//! - `v3-footer-before-fdatasync` — the cut lands between the footer
//!   write and its completion fdatasync: the `.ick.new` is footer-complete
//!   on disk but no MANIFEST names it — boot replays the log alone and
//!   collects the orphan (a complete-looking file is still nothing
//!   without its name).
//! - `v3-named-unit-damaged-padding` — a non-zero byte inside a *named*
//!   unit's block padding fail-stops typed (`Padding`): the writer wrote
//!   every padding byte as zero; anything else is damage the reader must
//!   not step over.

use std::path::{Path, PathBuf};

use inf_foundation::time::Nanos;
use inf_log::ckpt::{ICK_BLOCK_ALIGN, SyncIckWriter, ick_file_name, ick_staging_file_name};
use inf_log::fs::mem::MemFs;
use inf_log::fs::{SegmentFile, SegmentFs};
use inf_log::{
    CkptConfig, Manifest, MutationEffect, NsId, RecordView, SegmentConfig, SegmentRotor,
    StagingConfig, StagingRing, create_cell_dirs, segment_file_name, write_manifest,
};
use inf_server::{DurableConfig, open_cell_log};
use inf_store::{FsyncClass, Keyspace, NsMode, NsSpec, StoreConfig, WallAnchor};

const NS: NsId = NsId(16);
const CELL: u16 = 0;
const SHARD: &str = "data/shard-0";
const KEYS: u64 = 300;

fn now() -> Nanos {
    Nanos::from_millis(1)
}

fn anchor() -> WallAnchor {
    WallAnchor { internal_ms: 0, unix_ms: 1_750_000_000_000 }
}

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

fn fresh_keyspace() -> Keyspace {
    let mut ks = Keyspace::new(StoreConfig::default());
    ks.ns_create(NsSpec {
        id: NS,
        name: b"ledger".to_vec(),
        mode: NsMode::Durable,
        fsync: Some(FsyncClass::Always),
        policy: None,
        maxmemory: None,
        tier: None,
    })
    .expect("ns");
    ks
}

fn key_of(i: u64) -> Vec<u8> {
    format!("k:{i:05}").into_bytes()
}

fn value_of(i: u64, generation: u8) -> Vec<u8> {
    let mut v = vec![b'a' + (i % 26) as u8; 40 + (i as usize % 50)];
    v.push(generation);
    v
}

fn get(ks: &mut Keyspace, key: &[u8]) -> Option<Vec<u8>> {
    ks.ns_store_mut(NS).expect("ns store").get(key, now()).map(<[u8]>::to_vec)
}

/// A shard with one published **v3** checkpoint (every key at its first
/// generation) and a tail of `tail_ops` overwrites (second generation)
/// beyond its begin marker. Returns the published unit's bytes.
fn build_shard(fs: &MemFs, tail_ops: u64) -> Vec<u8> {
    let config = cfg();
    let dirs = create_cell_dirs(fs, Path::new(SHARD)).expect("dirs");
    let mut rotor =
        SegmentRotor::create_fresh(fs.clone(), dirs.log.clone(), config.segment).expect("rotor");
    let mut ring = StagingRing::new(config.staging);
    ring.stage(&MutationEffect::CkptBegin { ckpt_id: 1 }).expect("stage");
    for i in 0..tail_ops {
        let key = key_of(i);
        let value = value_of(i, 2);
        ring.stage(&MutationEffect::StringSet { ns: NS, key: &key, value: &value }).expect("stage");
    }
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

    let ckpt_dir = Path::new(SHARD).join("ckpt");
    let mut w = SyncIckWriter::create_v3(
        fs.clone(),
        &ckpt_dir,
        &CkptConfig { section_bytes: 2048, ..Default::default() },
        CELL,
        1,
        begin_lsn,
        &[NS.0],
    )
    .expect("create v3");
    for i in 0..KEYS {
        let key = key_of(i);
        let value = value_of(i, 1);
        w.append(&RecordView::StringPostImage { ns: NS, key: &key, value: &value }).expect("image");
    }
    let summary = w.finish().expect("publish");
    assert!(summary.sections >= 4, "several aligned sections");
    assert_eq!(summary.bytes % ICK_BLOCK_ALIGN as u64, 0, "v3 ends on a boundary");
    write_manifest(
        fs,
        Path::new(SHARD),
        &Manifest { ckpt_id: 1, begin_lsn, segments: vec![begin_lsn.segment], tiers: vec![] },
    )
    .expect("manifest");
    fs.contents(&ckpt_dir.join(ick_file_name(1))).expect("published bytes")
}

#[test]
fn v3_named_unit_loads_and_the_tail_replays_over_it() {
    let fs = MemFs::new();
    let published = build_shard(&fs, 25);
    assert_eq!(published.len() % ICK_BLOCK_ALIGN, 0);
    let mut ks = fresh_keyspace();
    let (_rotor, stats, manifest) =
        open_cell_log(fs.clone(), &mut ks, CELL, &cfg(), anchor(), now()).expect("boot");
    assert!(manifest.is_some(), "the MANIFEST named the v3 unit");
    assert!(stats.records_applied >= 25, "the tail replayed");
    for i in 0..KEYS {
        let want = value_of(i, if i < 25 { 2 } else { 1 });
        assert_eq!(get(&mut ks, &key_of(i)), Some(want), "key {i}");
    }
}

#[test]
fn v3_mid_section_orphan_is_collected_and_the_old_unit_loads() {
    let fs = MemFs::new();
    let published = build_shard(&fs, 10);
    // A second checkpoint that never finished: torn inside its second
    // block, at an unaligned offset (the `O_DIRECT` write was cut).
    let cut_at = ICK_BLOCK_ALIGN + 777;
    let orphan_path = Path::new(SHARD).join("ckpt").join(ick_staging_file_name(2));
    let mut orphan = fs.create_meta_direct(&orphan_path).expect("orphan");
    // The sim/mem tiers take unaligned *test* writes through the
    // SegmentFile seam (the driver seam is the aligned one).
    orphan.write_at(0, &published[..cut_at]).expect("write");
    drop(orphan);

    let mut ks = fresh_keyspace();
    let (_rotor, stats, manifest) =
        open_cell_log(fs.clone(), &mut ks, CELL, &cfg(), anchor(), now()).expect("boot");
    assert_eq!(manifest.map(|m| m.ckpt_id), Some(1), "the old unit is authoritative");
    assert!(stats.stale_files_removed >= 1, "the orphan is boot-GC'd");
    let names = fs.list_dir(&Path::new(SHARD).join("ckpt")).expect("dir");
    assert!(!names.iter().any(|n| n.ends_with(".ick.new")), "no orphan survives boot");
    assert_eq!(get(&mut ks, &key_of(KEYS - 1)), Some(value_of(KEYS - 1, 1)));
    assert_eq!(get(&mut ks, &key_of(0)), Some(value_of(0, 2)), "the tail replayed");
}

#[test]
fn v3_footer_complete_orphan_without_a_manifest_never_loads() {
    // The cut between the footer write and its fdatasync (or the rename):
    // the staging file looks complete, nothing names it.
    let fs = MemFs::new();
    let published = build_shard(&fs, 0);
    let ckpt_dir = Path::new(SHARD).join("ckpt");
    fs.remove_file(&Path::new(SHARD).join("MANIFEST")).expect("unpublish");
    fs.remove_file(&ckpt_dir.join(ick_file_name(1))).expect("unpublish");
    let mut orphan =
        fs.create_meta_direct(&ckpt_dir.join(ick_staging_file_name(1))).expect("orphan");
    orphan.write_at(0, &published).expect("write");
    drop(orphan);

    let mut ks = fresh_keyspace();
    let (_rotor, stats, manifest) =
        open_cell_log(fs.clone(), &mut ks, CELL, &cfg(), anchor(), now()).expect("boot");
    assert!(manifest.is_none(), "nothing was published");
    assert!(stats.stale_files_removed >= 1, "the complete-looking orphan is collected");
    // Only the log's own records exist: the checkpoint's generation-1
    // images were never loaded (the log holds only the begin marker).
    assert_eq!(get(&mut ks, &key_of(0)), None, "an unnamed unit contributes nothing");
    let names = fs.list_dir(&ckpt_dir).expect("dir");
    assert!(!names.iter().any(|n| n.ends_with(".ick.new")));
}

#[test]
fn v3_named_unit_with_damaged_padding_fail_stops() {
    let fs = MemFs::new();
    let published = build_shard(&fs, 5);
    // The header block's padding (after the header CRC, before the first
    // section boundary) is zero by construction; flip one byte of it.
    let header_len = 8 + 2 + 2 + 8 + 8 + 4 + 4 + 4;
    assert_eq!(published[header_len], 0, "header padding is zero");
    assert_eq!(published[ICK_BLOCK_ALIGN - 1], 0, "header padding is zero to the boundary");
    let mut damaged = published.clone();
    damaged[ICK_BLOCK_ALIGN - 1] = 0xA5;
    let path = Path::new(SHARD).join("ckpt").join(ick_file_name(1));
    fs.remove_file(&path).expect("replace");
    let mut f = fs.create_meta_direct(&path).expect("replace");
    f.write_at(0, &damaged).expect("write");
    drop(f);

    let mut ks = fresh_keyspace();
    let err = open_cell_log(fs.clone(), &mut ks, CELL, &cfg(), anchor(), now())
        .expect_err("a named unit with non-zero padding is corruption");
    let text = err.to_string();
    assert!(text.contains("Padding"), "typed refusal names the padding: {text}");
}
