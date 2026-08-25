//! M4.5-S06 recovery-gate re-proof (plan §7: a 10 GB node with 4
//! declared indexes boots < 15 s **on the sidecar path**; the rebuild
//! fallback is timed informationally beside it — ADR-0078).
//!
//! The real boot path end to end: `open_cell_log` over `StdSegmentFs` —
//! manifest → presize → `.ick` stream (images + tag-0x06 sidecar load
//! through the ascending append path) → tail replay under `CatchUp` →
//! converged/cell-Ready commit. Three shard variants, one corpus:
//!
//!   sidecar   images + 4 index sidecars; boot must load all 4 and walk
//!             nothing — the binding row.
//!   control   the same images, no sidecar sections — isolates the
//!             sidecar bytes' read+decode+append cost as a boot delta.
//!   rebuild   the control shard booted, then `idx_backfill_tick`
//!             driven to convergence — the fallback the sidecar exists
//!             to avoid, timed informationally.
//!
//! Dev tier: page-cache-warm by default (disclose); the reference-box
//! wall-clock row binds at S17 per the evidence rules.
//!
//! Run:  taskset -c 4 cargo bench -p inf-server --bench sidecar_recovery
//! Env:  INF_BENCH_DIR (default `target/`), INF_BENCH_DOCS (default
//!       200_000 ≈ 200 MB; the gate shape is 10_000_000 ≈ 10 GB),
//!       INF_BENCH_REPS (default 3).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Instant;

use inf_doc::JsonParser;
use inf_doc::path::compile;
use inf_foundation::time::Nanos;
use inf_log::ckpt::SyncIckWriter;
use inf_log::fs::StdSegmentFs;
use inf_log::{
    CkptConfig, DocLineage, IdxSidecarMeta, Manifest, MutationEffect, NsId, RecordView,
    SegmentConfig, SegmentRotor, StagingConfig, StagingRing, create_cell_dirs, segment_file_name,
    write_manifest,
};
use inf_server::{DurableConfig, open_cell_log};
use inf_store::{
    BackfillBudget, FsyncClass, INDEX_KEY_ENCODING_VERSION, IndexId, IndexKeyBuf, IndexKeyType,
    IndexScalar, IndexSpec, IndexState, Keyspace, NsCatalog, NsMode, NsSpec, StoreConfig,
    WallAnchor, index_key_encode,
};

const NS: NsId = NsId(16);
const CELL: u16 = 0;
/// Filler sized so one document record lands near the 1 KiB gate shape.
const PAD: usize = 900;

const INDEXES: &[(u32, &str, IndexKeyType)] = &[
    (1, "$.price", IndexKeyType::F64),
    (2, "$.tag", IndexKeyType::Utf8),
    (3, "$.qty", IndexKeyType::I64),
    (4, "$.ts", IndexKeyType::I64),
];

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn cfg(data_dir: PathBuf) -> DurableConfig {
    DurableConfig {
        data_dir,
        staging: StagingConfig::default(),
        segment: SegmentConfig { segment_bytes: 64 << 20, ..Default::default() },
        ckpt: CkptConfig::default(),
        recover: Default::default(),
        flush_bound: 1,
        fua_p50_us_probed: 0,
        device: Default::default(),
        fill: Default::default(),
        group: Default::default(),
    }
}

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
    template.export_catalog(17, 5, 5)
}

fn key_of(i: u64) -> Vec<u8> {
    format!("d:{i:08}").into_bytes()
}

/// One document's scalar row: (price, tag ordinal, qty, ts). ~100k
/// distinct values per index (disclosed — value cardinality shapes the
/// tree, not the load rate).
fn fields_of(i: u64) -> (u64, u64, i64, i64) {
    (i % 100_000, i % 99_991, (i % 100_003) as i64, (i % 100_019) as i64)
}

fn doc_of(i: u64, parser: &mut JsonParser, pad: &str) -> Vec<u8> {
    let (price, tag, qty, ts) = fields_of(i);
    let json =
        format!(r#"{{"price":{price}.5,"tag":"t{tag:06}","qty":{qty},"ts":{ts},"pad":"{pad}"}}"#);
    parser.parse(json.as_bytes()).expect("valid doc")
}

/// Builds a shard under `data_dir`: log (ckpt-begin + a small tail),
/// the published `.ick` (all images; sidecars iff `sidecars`), MANIFEST.
fn build_shard(data_dir: &Path, docs: u64, sidecars: bool) -> u64 {
    let fs = StdSegmentFs;
    let shard = data_dir.join(format!("shard-{CELL}"));
    std::fs::create_dir_all(&shard).expect("mkdir");
    let config = cfg(data_dir.to_path_buf());
    let dirs = create_cell_dirs(&fs, &shard).expect("dirs");
    let mut rotor =
        SegmentRotor::create_fresh(fs, dirs.log.clone(), config.segment).expect("rotor");
    let mut ring = StagingRing::new(config.staging);
    let mut parser = JsonParser::new();
    let pad = "x".repeat(PAD);
    ring.stage(&MutationEffect::CkptBegin { ckpt_id: 1 }).expect("stage");
    let tail: Vec<(Vec<u8>, Vec<u8>)> =
        (0..1000.min(docs)).map(|i| (key_of(i), doc_of(i + docs, &mut parser, &pad))).collect();
    for (key, idoc) in &tail {
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
    {
        use inf_log::fs::{SegmentFile, SegmentFs as _};
        let mut file = StdSegmentFs
            .open_write(&dirs.log.join(segment_file_name(rotor.active_segment())))
            .expect("segment");
        file.sync_data().expect("fdatasync");
    }

    let ckpt_dir = shard.join("ckpt");
    let mut w = SyncIckWriter::create_v2(
        StdSegmentFs,
        &ckpt_dir,
        &CkptConfig::default(),
        CELL,
        1,
        begin_lsn,
        &[NS.0],
    )
    .expect("create v2");
    let mut file_bytes = 0u64;
    for i in 0..docs {
        let key = key_of(i);
        let idoc = doc_of(i, &mut parser, &pad);
        file_bytes += idoc.len() as u64 + key.len() as u64 + 32;
        w.append(&RecordView::DocFull {
            ns: NS,
            key: &key,
            lineage: DocLineage::FIRST,
            version: 1,
            idoc: &idoc,
        })
        .expect("image");
    }
    if sidecars {
        let mut buf = IndexKeyBuf::new();
        for &(id, _, key_type) in INDEXES {
            // The entry stream, derived arithmetically (identical to the
            // eval result on these docs) and sorted into the writer canon.
            let mut entries: BTreeSet<(Vec<u8>, u64)> = BTreeSet::new();
            for i in 0..docs {
                let (price, tag, qty, ts) = fields_of(i);
                let tag_text = format!("t{tag:06}");
                let scalar = match id {
                    1 => IndexScalar::F64(price as f64 + 0.5),
                    2 => IndexScalar::Utf8(&tag_text),
                    3 => IndexScalar::I64(qty),
                    _ => IndexScalar::I64(ts),
                };
                index_key_encode(key_type, scalar, &mut buf).expect("encodable");
                let hash = inf_store::CellStore::hash_key(&key_of(i));
                entries.insert((buf.as_bytes().to_vec(), hash));
            }
            let meta = IdxSidecarMeta {
                ns: NS.0,
                index_id: id,
                generation: u64::from(id),
                key_encoding_version: INDEX_KEY_ENCODING_VERSION,
                fixed8: key_type.fixed8(),
            };
            for (ordinal, (key, entry_ref)) in entries.iter().enumerate() {
                w.append_idx_entry(&meta, ordinal as u64, key, *entry_ref).expect("entry");
            }
            w.append_idx_final(&meta, entries.len() as u64).expect("final");
        }
    }
    w.finish().expect("publish");
    write_manifest(
        &StdSegmentFs,
        &shard,
        &Manifest { ckpt_id: 1, begin_lsn, segments: vec![begin_lsn.segment], tiers: vec![] },
    )
    .expect("manifest");
    file_bytes
}

fn boot(data_dir: &Path) -> (Keyspace, f64) {
    let mut ks = Keyspace::new(StoreConfig::default());
    ks.seed_catalog(&catalog()).expect("seed");
    let t = Instant::now();
    open_cell_log(
        StdSegmentFs,
        &mut ks,
        CELL,
        &cfg(data_dir.to_path_buf()),
        WallAnchor { internal_ms: 0, unix_ms: 1_750_000_000_000 },
        Nanos::from_millis(1),
    )
    .expect("boot");
    (ks, t.elapsed().as_secs_f64())
}

fn main() {
    let docs = env_u64("INF_BENCH_DOCS", 200_000);
    let reps = env_u64("INF_BENCH_REPS", 3);
    let root = PathBuf::from(std::env::var("INF_BENCH_DIR").unwrap_or_else(|_| "target".into()))
        .join("sidecar-recovery");
    let _ = std::fs::remove_dir_all(&root);
    println!("sidecar_recovery: {docs} docs × ~1 KiB, 4 indexes, {reps} replicates");

    let with = root.join("with");
    std::fs::create_dir_all(&with).expect("mkdir");
    let bytes = build_shard(&with, docs, true);
    println!("  corpus ≈ {:.2} GB of record bytes", bytes as f64 / 1e9);
    for rep in 0..reps {
        let (ks, secs) = boot(&with);
        let info = ks.idx_sidecar_info();
        assert_eq!(info.loaded, 4, "the sidecar path must carry every index");
        assert_eq!(ks.idx_backfill_info().docs_scanned_total, 0, "loaded ⇒ no walk");
        println!(
            "  sidecar rep {rep}: boot {secs:.2}s — {} entries loaded \
             ({:.1} M entries/s if load-only) [gate < 15 s at the 10 GB shape]",
            info.entries_loaded,
            info.entries_loaded as f64 / secs / 1e6,
        );
    }

    let without = root.join("without");
    std::fs::create_dir_all(&without).expect("mkdir");
    build_shard(&without, docs, false);
    for rep in 0..reps {
        let (ks, secs) = boot(&without);
        assert_eq!(ks.idx_sidecar_info().loaded, 0);
        println!("  control rep {rep}: boot {secs:.2}s (no sidecar sections)");
    }

    // The rebuild fallback (informational): control boot + the S05
    // machine driven to convergence.
    for rep in 0..reps {
        let (mut ks, boot_secs) = boot(&without);
        let t = Instant::now();
        let budget = BackfillBudget { max_docs: 8192, max_steps: 1 << 20 };
        let mut now = Nanos::from_millis(2);
        loop {
            let stats = ks.idx_backfill_tick(now, budget);
            if stats.active == 0 {
                break;
            }
            now = Nanos(now.0 + 1);
        }
        let walk_secs = t.elapsed().as_secs_f64();
        println!(
            "  rebuild rep {rep}: boot {boot_secs:.2}s + walk {walk_secs:.2}s \
             = {:.2}s (the fallback the sidecar avoids)",
            boot_secs + walk_secs
        );
    }
    let _ = std::fs::remove_dir_all(&root);
}
