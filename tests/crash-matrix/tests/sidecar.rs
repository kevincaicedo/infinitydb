//! M4.5-S06 sidecar recovery-unit crash rows (ADR-0078; the ADR-0073 D6
//! damage taxonomy made executable at the recovery-driver tier):
//!
//! - `named-unit-loads` — the canary: a published sidecar-bearing unit
//!   boots through `open_cell_log`, loads every index (no walk), arms
//!   tail catch-up, and the loaded trees equal the from-scratch
//!   derivation over the recovered documents.
//! - `mid-write-orphan` — the cut mid-sidecar-write row: a `.ick.new`
//!   orphan truncated inside its 0x06 region sits beside the published
//!   unit. The old unit stays authoritative, boots, loads — and the
//!   orphan is GC'd. Without any published unit at all, the same orphan
//!   yields a clean boot with zero loads and `rebuilt` decisions (the
//!   S05 machine's territory).
//! - `named-unit-torn` — truncation anywhere inside a *published*
//!   sidecar-bearing unit fail-stops (the manifest named it; framing
//!   damage is corruption, §8.4) — a torn unit can never quietly serve.
//! - `damaged-body` — one flipped byte inside a 0x06 body: the boot
//!   proceeds (soft class), the damaged index rebuilds, its neighbor
//!   loads, and the damage is counted (L10).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use inf_doc::JsonParser;
use inf_doc::path::compile;
use inf_foundation::KeyHasher;
use inf_foundation::time::Nanos;
use inf_log::ckpt::{SyncIckWriter, ick_file_name, ick_staging_file_name};
use inf_log::fs::mem::MemFs;
use inf_log::fs::{SegmentFile, SegmentFs};
use inf_log::{
    CkptConfig, DocLineage, IdxSidecarMeta, Manifest, MutationEffect, NsId, RecordView,
    SegmentConfig, SegmentRotor, StagingConfig, StagingRing, create_cell_dirs, segment_file_name,
    write_manifest,
};
use inf_server::{DurableConfig, open_cell_log};
use inf_store::{
    FsyncClass, INDEX_KEY_ENCODING_VERSION, IndexId, IndexKeyBuf, IndexKeyType, IndexScalar,
    IndexSpec, IndexState, Keyspace, NsCatalog, NsMode, NsSpec, OrderedCursor, SidecarBootDecision,
    StoreConfig, WallAnchor, index_key_encode,
};

const NS: NsId = NsId(16);
const CELL: u16 = 0;
const SHARD: &str = "data/shard-0";
const DOCS: u64 = 40;

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

const INDEXES: &[(u32, &str, IndexKeyType)] =
    &[(1, "$.price", IndexKeyType::F64), (2, "$.tag", IndexKeyType::Utf8)];

/// The index-bearing catalog: declarations persisted `ready` pre-crash
/// (`seed_catalog` regresses them and keeps the `was_ready` hint).
fn catalog() -> NsCatalog {
    let mut template = Keyspace::new(StoreConfig::default());
    template
        .ns_create(NsSpec {
            id: NS,
            name: b"docs".to_vec(),
            mode: NsMode::Durable,
            fsync: Some(FsyncClass::Always),
            policy: None,
            maxmemory: None,
            tier: None,
        })
        .expect("ns");
    for &(id, path, key_type) in INDEXES {
        template
            .idx_create(IndexSpec {
                id: IndexId(id),
                generation: u64::from(id),
                ns: NS,
                name: format!("by-{id}").into_bytes(),
                program: compile(path.as_bytes()).expect("path").as_bytes().to_vec(),
                key_type,
                state: IndexState::Ready,
            })
            .expect("declare");
    }
    template.export_catalog(17, 3, 3)
}

fn booted_keyspace() -> Keyspace {
    let mut ks = Keyspace::new(StoreConfig::default());
    ks.seed_catalog(&catalog()).expect("seed");
    ks
}

fn doc_of(i: u64) -> Vec<u8> {
    let json = format!(r#"{{"price":{}.5,"tag":"t{}"}}"#, i % 23, i % 7);
    JsonParser::new().parse(json.as_bytes()).expect("valid doc")
}

fn key_of(i: u64) -> Vec<u8> {
    format!("d:{i:03}").into_bytes()
}

/// The `(typed key, hash)` entries one document contributes to `path`.
fn entries_of(idoc: &[u8], key: &[u8], path: &str, key_type: IndexKeyType) -> Vec<(Vec<u8>, u64)> {
    use inf_doc::path::{EvalLimits, eval, resolve};
    use inf_doc::{DocValue, TapeDoc};
    let program = compile(path.as_bytes()).expect("path");
    let tape = TapeDoc::from_validated_bytes(idoc);
    let root = DocValue::from(tape.root());
    let matches = eval(&program, root, &EvalLimits::default()).expect("small doc");
    let hash = inf_store::KeyHasher::default().hash(key);
    let mut buf = IndexKeyBuf::new();
    let mut out = Vec::new();
    for steps in matches.iter() {
        let Some(value) = resolve(root, steps) else { continue };
        let scalar = match value {
            DocValue::I64(v) => IndexScalar::I64(v),
            DocValue::F64(f) => IndexScalar::F64(f),
            DocValue::Str(s) => IndexScalar::Utf8(s.to_str()),
            _ => continue,
        };
        if index_key_encode(key_type, scalar, &mut buf).is_ok() {
            out.push((buf.as_bytes().to_vec(), hash));
        }
    }
    out
}

/// Builds the shard: a log whose one frame carries the ckpt-begin marker
/// plus `tail_ops` post-checkpoint overwrites (the catch-up window), a
/// published sidecar-bearing `ckpt-000001.ick`, and the MANIFEST naming
/// it. Returns the published ick's bytes for the damage rows.
fn build_shard(fs: &MemFs, tail_ops: u64) -> Vec<u8> {
    let config = cfg();
    let dirs = create_cell_dirs(fs, Path::new(SHARD)).expect("dirs");
    let mut rotor =
        SegmentRotor::create_fresh(fs.clone(), dirs.log.clone(), config.segment).expect("rotor");
    let mut ring = StagingRing::new(config.staging);
    ring.stage(&MutationEffect::CkptBegin { ckpt_id: 1 }).expect("stage");
    // Tail beyond the checkpoint: overwrites of the first keys with new
    // values (replayed under CatchUp on a sidecar boot).
    let tail_docs: Vec<(Vec<u8>, Vec<u8>)> =
        (0..tail_ops).map(|i| (key_of(i), doc_of(i + 1000))).collect();
    for (key, idoc) in &tail_docs {
        ring.stage(&MutationEffect::DocFull {
            ns: NS,
            key,
            lineage: DocLineage::FIRST,
            version: 2,
            idoc,
        })
        .expect("stage");
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

    // The published checkpoint: every document image + both indexes'
    // true entry streams (sorted — the writer canon), FINAL-closed.
    let ckpt_dir = Path::new(SHARD).join("ckpt");
    let mut w = SyncIckWriter::create_v2(
        fs.clone(),
        &ckpt_dir,
        &CkptConfig { section_bytes: 512, ..Default::default() },
        CELL,
        1,
        begin_lsn,
        &[NS.0],
    )
    .expect("create v2");
    let corpus: Vec<(Vec<u8>, Vec<u8>)> = (0..DOCS).map(|i| (key_of(i), doc_of(i))).collect();
    for (key, idoc) in &corpus {
        w.append(&RecordView::DocFull {
            ns: NS,
            key,
            lineage: DocLineage::FIRST,
            version: 1,
            idoc,
        })
        .expect("image");
    }
    for &(id, path, key_type) in INDEXES {
        let meta = IdxSidecarMeta {
            ns: NS.0,
            index_id: id,
            generation: u64::from(id),
            key_encoding_version: INDEX_KEY_ENCODING_VERSION,
            fixed8: key_type.fixed8(),
        };
        let mut entries: BTreeSet<(Vec<u8>, u64)> = BTreeSet::new();
        for (key, idoc) in &corpus {
            entries.extend(entries_of(idoc, key, path, key_type));
        }
        for (ordinal, (key, entry_ref)) in entries.iter().enumerate() {
            w.append_idx_entry(&meta, ordinal as u64, key, *entry_ref).expect("entry");
        }
        w.append_idx_final(&meta, entries.len() as u64).expect("final");
    }
    w.finish().expect("publish");
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
    fs.contents(&ckpt_dir.join(ick_file_name(1))).expect("published bytes")
}

fn tree_len(ks: &Keyspace, id: IndexId) -> u64 {
    ks.idx_tree(NS, id).map_or(0, |t| t.len())
}

/// The from-scratch derivation over the *recovered* store, compared to
/// the loaded tree — the equivalence oracle at this tier.
fn assert_tree_matches_store(ks: &Keyspace, id: u32, path: &str, key_type: IndexKeyType) {
    let store = ks.ns_store(NS).expect("recovered store");
    let mut truth: BTreeSet<(Vec<u8>, u64)> = BTreeSet::new();
    let mut cursor = 0u64;
    loop {
        cursor = store.digest_checkpoint_images(cursor, 64, now(), |key, image, _| {
            let inf_store::CheckpointImage::JsonDoc { idoc, .. } = image else { return };
            truth.extend(entries_of(idoc, key, path, key_type));
        });
        if cursor == 0 {
            break;
        }
    }
    let tree = ks.idx_tree(NS, IndexId(id)).expect("tree");
    let mut walk = OrderedCursor::from_start();
    let mut got: BTreeSet<(Vec<u8>, u64)> = BTreeSet::new();
    while let Some((key, entry_ref)) = tree.cursor_next(&mut walk) {
        got.insert((key.to_vec(), entry_ref));
    }
    assert_eq!(got, truth, "index {id}: loaded+caught-up tree ≠ store derivation");
}

/// Locates the first tag-0x06 block by hopping section headers.
fn find_idx_block(image: &[u8]) -> usize {
    let ns_count = u32::from_le_bytes(image[28..32].try_into().unwrap()) as usize;
    let mut at = 8 + 2 + 2 + 8 + 8 + 4 + ns_count * 4 + 4;
    loop {
        assert!(at < image.len(), "no sidecar section found");
        if image[at] == 6 {
            return at;
        }
        assert_ne!(image[at], 2, "footer reached before any sidecar section");
        let body_len = u32::from_le_bytes(image[at + 1..at + 5].try_into().unwrap()) as usize;
        at += 9 + body_len + 4;
    }
}

#[test]
fn named_sidecar_unit_boots_loads_and_catches_up() {
    let fs = MemFs::new();
    build_shard(&fs, 5);
    let mut ks = booted_keyspace();
    open_cell_log(fs, &mut ks, CELL, &cfg(), anchor(), now()).expect("boot");
    let info = ks.idx_sidecar_info();
    assert_eq!((info.loaded, info.rebuilt, info.damaged_sections), (2, 0, 0));
    for &(id, ..) in INDEXES {
        assert_eq!(
            ks.idx_registry().cell_state(IndexId(id)),
            Some(IndexState::Ready),
            "a loaded index commits cell-Ready at end of replay"
        );
        assert!(matches!(
            ks.idx_registry().sidecar_boot(IndexId(id)),
            Some(SidecarBootDecision::Loaded { .. })
        ));
    }
    // The tail overwrites (docs 0..5 with new values) were caught up
    // under CatchUp — the trees equal the post-tail store, not the
    // checkpoint-time one.
    for &(id, path, key_type) in INDEXES {
        assert_tree_matches_store(&ks, id, path, key_type);
    }
}

#[test]
fn mid_sidecar_write_orphan_is_harmless_and_collected() {
    let fs = MemFs::new();
    let published = build_shard(&fs, 3);
    // The cut-mid-sidecar-write shape: a staging orphan truncated inside
    // its 0x06 region (a next checkpoint that never finished).
    let cut_at = find_idx_block(&published) + 20;
    let orphan_path = Path::new(SHARD).join("ckpt").join(ick_staging_file_name(2));
    let mut orphan = fs.create_meta(&orphan_path).expect("orphan");
    orphan.write_at(0, &published[..cut_at]).expect("write");
    drop(orphan);

    let mut ks = booted_keyspace();
    let (_rotor, stats, _seed) =
        open_cell_log(fs.clone(), &mut ks, CELL, &cfg(), anchor(), now()).expect("boot");
    // The old unit stays authoritative and its sidecars load.
    assert_eq!(ks.idx_sidecar_info().loaded, 2, "the published unit is untouched by the orphan");
    assert!(stats.stale_files_removed >= 1, "the orphan is boot-GC'd");
    let names = fs.list_dir(&Path::new(SHARD).join("ckpt")).expect("dir");
    assert!(!names.iter().any(|n| n.ends_with(".ick.new")), "no orphan survives boot");
}

#[test]
fn mid_sidecar_write_without_any_published_unit_rebuilds() {
    // Nothing was ever published: only the log and the torn `.ick.new`.
    let fs = MemFs::new();
    let published = build_shard(&fs, 0);
    let ckpt_dir = Path::new(SHARD).join("ckpt");
    fs.remove_file(&Path::new(SHARD).join("MANIFEST")).expect("unpublish");
    fs.remove_file(&ckpt_dir.join(ick_file_name(1))).expect("unpublish");
    let cut_at = find_idx_block(&published) + 20;
    let mut orphan = fs.create_meta(&ckpt_dir.join(ick_staging_file_name(1))).expect("orphan");
    orphan.write_at(0, &published[..cut_at]).expect("write");
    drop(orphan);

    let mut ks = booted_keyspace();
    open_cell_log(fs, &mut ks, CELL, &cfg(), anchor(), now()).expect("boot");
    let info = ks.idx_sidecar_info();
    assert_eq!(info.loaded, 0, "an unpublished torn sidecar never loads");
    assert_eq!(info.rebuilt, 2, "every declaration records its rebuild decision");
    for &(id, ..) in INDEXES {
        assert_eq!(
            ks.idx_registry().sidecar_boot(IndexId(id)),
            Some(SidecarBootDecision::Rebuilt {
                reason: inf_store::SidecarRebuildReason::NoSidecar
            })
        );
        assert_eq!(tree_len(&ks, IndexId(id)), 0, "the S05 machine owns the rebuild");
    }
}

#[test]
fn truncated_published_sidecar_unit_fail_stops() {
    // The manifest named it — framing damage anywhere is corruption
    // (§8.4), sidecar sections included: a torn unit never quietly
    // serves. Sweep cut points across the file, denser near the tail
    // (where the sidecar sections and footer live).
    let fs0 = MemFs::new();
    let published = build_shard(&fs0, 2);
    let idx_at = find_idx_block(&published);
    let mut cuts: Vec<usize> = (1..published.len()).step_by(211).collect();
    cuts.extend((idx_at..published.len()).step_by(53));
    for cut in cuts {
        let fs = MemFs::new();
        build_shard(&fs, 2);
        let path = Path::new(SHARD).join("ckpt").join(ick_file_name(1));
        fs.remove_file(&path).expect("replace");
        let mut torn = fs.create_meta(&path).expect("torn");
        torn.write_at(0, &published[..cut]).expect("write");
        drop(torn);
        let mut ks = booted_keyspace();
        let err = open_cell_log(fs, &mut ks, CELL, &cfg(), anchor(), now());
        assert!(err.is_err(), "cut at {cut}/{} must fail-stop", published.len());
    }
}

#[test]
fn damaged_sidecar_body_rebuilds_one_index_and_boots() {
    let fs = MemFs::new();
    let published = build_shard(&fs, 2);
    // Flip one byte inside the first 0x06 body (past the 36-byte meta):
    // the section CRC catches it, the boot continues, that index
    // rebuilds, the neighbor loads (ADR-0073 D6 / ADR-0078 D4).
    let at = find_idx_block(&published) + 9 + 36 + 3;
    let path = Path::new(SHARD).join("ckpt").join(ick_file_name(1));
    let mut image = published.clone();
    image[at] ^= 0x40;
    fs.remove_file(&path).expect("replace");
    let mut damaged = fs.create_meta(&path).expect("damaged");
    damaged.write_at(0, &image).expect("write");
    drop(damaged);

    let mut ks = booted_keyspace();
    open_cell_log(fs, &mut ks, CELL, &cfg(), anchor(), now())
        .expect("body damage never refuses a boot (L2)");
    let info = ks.idx_sidecar_info();
    assert_eq!(info.damaged_sections, 1, "the damage is counted (L10)");
    assert_eq!(info.loaded + info.rebuilt, 2);
    assert!(info.rebuilt >= 1, "the damaged stream resolves as a rebuild");
    assert!(info.loaded >= 1, "one section's damage never spreads to the neighbor");
}
