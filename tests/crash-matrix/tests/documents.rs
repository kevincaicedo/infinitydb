//! M3-S18 document crash rows. Each row feeds production idoc/path/delta
//! bytes through the real log frame and recovery implementations. The
//! tests vary only the cut location; there is no parallel replay model.

use std::path::{Path, PathBuf};

use crash_matrix::load_matrix;
use inf_doc::apply::{ApplyOp, Number};
use inf_doc::path::compile;
use inf_doc::{JsonParser, encode_apply_op};
use inf_foundation::KeyHasher;
use inf_foundation::time::Nanos;
use inf_log::ckpt::SyncIckWriter;
use inf_log::fs::mem::MemFs;
use inf_log::fs::sim::{SimDisk, SimDiskConfig};
use inf_log::fs::{SegmentFile, SegmentFs};
use inf_log::{
    CkptConfig, DocLineage, FRAME_HEADER_LEN, Lsn, Manifest, MutationEffect, NsId, RecordView,
    SegmentConfig, SegmentId, SegmentRotor, StagingConfig, StagingRing, create_cell_dirs,
    segment_file_name, write_manifest,
};
use inf_server::{DurableConfig, RecoverStats, open_cell_log};
use inf_store::{FsyncClass, Keyspace, NsMode, NsSpec, StoreConfig, WallAnchor};

const NS: NsId = NsId(16);
const CELL: u16 = 0;
const NOW: Nanos = Nanos::from_millis(1);
const ANCHOR: WallAnchor = WallAnchor { internal_ms: 0, unix_ms: 1_750_000_000_000 };
const SHARD_DIR: &str = "data/shard-0";
const LOG_DIR: &str = "data/shard-0/log";
const CKPT_DIR: &str = "data/shard-0/ckpt";

fn matrix_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("m3.toml")
}

fn config(segment_bytes: u32) -> DurableConfig {
    DurableConfig {
        data_dir: PathBuf::from("data"),
        staging: StagingConfig::default(),
        segment: SegmentConfig { segment_bytes, ..Default::default() },
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

struct Fixtures {
    v1: Vec<u8>,
    v2: Vec<u8>,
    v3: Vec<u8>,
    torn_full: Vec<u8>,
    program: inf_doc::path::PathProgram,
    operand: Vec<u8>,
    opcode: u8,
}

impl Fixtures {
    fn new() -> Fixtures {
        let mut parser = JsonParser::new();
        let v1 = parser.parse(br#"{"values":[1,1]}"#).expect("v1 fixture");
        let v2 = parser.parse(br#"{"values":[2,2]}"#).expect("v2 fixture");
        let v3 = parser.parse(br#"{"values":[3,3]}"#).expect("v3 fixture");
        let torn_json = format!(r#"{{"values":[9,9],"pad":"{}"}}"#, "x".repeat(512));
        let torn_full = parser.parse(torn_json.as_bytes()).expect("large full fixture");
        let program = compile(b"$.values[*]").expect("path fixture");
        let mut operand = Vec::new();
        let opcode = encode_apply_op(&ApplyOp::NumIncrBy(Number::I64(1)), &mut operand) as u8;
        Fixtures { v1, v2, v3, torn_full, program, operand, opcode }
    }

    fn full<'a>(&'a self, key: &'a [u8], version: u32, idoc: &'a [u8]) -> MutationEffect<'a> {
        MutationEffect::DocFull { ns: NS, key, lineage: DocLineage::FIRST, version, idoc }
    }

    fn delta<'a>(&'a self, key: &'a [u8], base_version: u32) -> MutationEffect<'a> {
        let post_len = match base_version {
            1 => self.v2.len(),
            2 => self.v3.len(),
            other => panic!("fixture has no post-image for base version {other}"),
        };
        MutationEffect::DocDelta {
            ns: NS,
            key,
            lineage: DocLineage::FIRST,
            base_version,
            match_count: 2,
            post_len: u32::try_from(post_len).expect("fixture length fits u32"),
            opcode: self.opcode,
            program: self.program.as_bytes(),
            operand: &self.operand,
        }
    }
}

#[derive(Copy, Clone)]
struct FrameReceipt {
    base: Lsn,
    end: Lsn,
    first_record: Lsn,
}

struct LogBuilder<F: SegmentFs + Clone> {
    fs: F,
    rotor: SegmentRotor<F>,
    ring: StagingRing,
    log_dir: PathBuf,
}

impl<F: SegmentFs + Clone> LogBuilder<F> {
    fn new(fs: F, cfg: &DurableConfig) -> LogBuilder<F> {
        let dirs = create_cell_dirs(&fs, Path::new(SHARD_DIR)).expect("cell directories");
        let rotor =
            SegmentRotor::create_fresh(fs.clone(), dirs.log.clone(), cfg.segment).expect("rotor");
        LogBuilder { fs, rotor, ring: StagingRing::new(cfg.staging), log_dir: dirs.log }
    }

    /// Appends exactly one frame. `covered_lsn` is the preceding fsync
    /// watermark stamped by the production frame format.
    fn frame(&mut self, effects: &[MutationEffect<'_>], covered_lsn: u64) -> FrameReceipt {
        assert!(!effects.is_empty(), "a crash-row frame is non-empty");
        let staged: Vec<_> =
            effects.iter().map(|effect| self.ring.stage(effect).expect("stage")).collect();
        self.rotor.maintain(0).expect("maintain");
        let slot = self.rotor.begin_frame(self.ring.pending_frame_len(), 0).expect("reserve");
        let base = slot.base();
        let lease = self.ring.seal(slot.first_record_lsn(), covered_lsn, slot.layout());
        let first_record = lease.lsn_of(staged[0]);
        let bytes = self.ring.leased_frame(&lease).to_vec();
        self.rotor.commit_frame(slot, &bytes).expect("commit frame");
        self.ring.release(lease);
        let end = Lsn::new(self.rotor.active_segment(), self.rotor.active_written());
        FrameReceipt { base, end, first_record }
    }

    fn sync(&self, segment: SegmentId) {
        let path = self.log_dir.join(segment_file_name(segment));
        self.fs.open_write(&path).expect("open segment").sync_data().expect("fdatasync");
    }

    fn active_segment(&self) -> SegmentId {
        self.rotor.active_segment()
    }
}

fn recover<F: SegmentFs + Clone + 'static>(fs: F, cfg: &DurableConfig) -> (Keyspace, RecoverStats) {
    let mut ks = keyspace();
    let (_rotor, stats, _manifest) =
        open_cell_log(fs, &mut ks, CELL, cfg, ANCHOR, NOW).expect("document recovery");
    (ks, stats)
}

fn assert_doc(ks: &mut Keyspace, key: &[u8], expected: &[u8]) {
    let actual = ks
        .ns_store_mut(NS)
        .expect("namespace")
        .json_freeze(key, NOW)
        .expect("document type")
        .expect("document present");
    assert_eq!(actual, expected, "recovered canonical document bytes");
}

fn assert_missing(ks: &mut Keyspace, key: &[u8]) {
    let actual =
        ks.ns_store_mut(NS).expect("namespace").json_freeze(key, NOW).expect("document type");
    assert!(actual.is_none(), "document must remain absent");
}

fn publish_checkpoint<F: SegmentFs + Clone>(
    fs: &F,
    cfg: &DurableConfig,
    begin: Lsn,
    active: SegmentId,
    records: &[RecordView<'_>],
) {
    let mut writer =
        SyncIckWriter::create(fs.clone(), Path::new(CKPT_DIR), &cfg.ckpt, CELL, 1, begin, &[NS.0])
            .expect("checkpoint create");
    for record in records {
        writer.append(record).expect("checkpoint record");
    }
    writer.finish().expect("checkpoint publish");
    let segments = (begin.segment.0..=active.0).map(SegmentId).collect();
    write_manifest(
        fs,
        Path::new(SHARD_DIR),
        &Manifest {
            ckpt_id: 1,
            begin_lsn: begin,
            segments,
            tiers: Vec::new(),
            key_hash_id: KeyHasher::default().identity(),
        },
    )
    .expect("manifest publish");
}

fn delta_before_fsync(seeds: u64) {
    let fixtures = Fixtures::new();
    let cfg = config(1 << 16);
    let mut old = 0u64;
    let mut new = 0u64;
    let mut torn = 0u64;
    for seed in 0..seeds {
        // Small sectors make the one pending delta frame genuinely
        // tearable instead of merely present/absent.
        let disk = SimDisk::with_config(SimDiskConfig { sector_bytes: 32 });
        let mut log = LogBuilder::new(disk.clone(), &cfg);
        let full = log.frame(&[fixtures.full(b"doc", 1, &fixtures.v1)], 0);
        log.sync(full.base.segment);
        let delta = log.frame(&[fixtures.delta(b"doc", 1)], full.end.to_u64());
        assert_eq!(delta.first_record.segment, delta.base.segment);
        drop(log);
        disk.power_cut(0xD0C0_1800 ^ seed);

        let (mut ks, stats) = recover(disk, &cfg);
        let actual = ks
            .ns_store_mut(NS)
            .expect("namespace")
            .json_freeze(b"doc", NOW)
            .expect("document")
            .expect("synced full survives");
        if actual == fixtures.v1 {
            old += 1;
        } else if actual == fixtures.v2 {
            new += 1;
        } else {
            panic!("seed {seed}: a torn command exposed partial document bytes");
        }
        torn += u64::from(stats.torn_truncated_at.is_some());
    }
    assert!(old > 0, "the seed slice must lose at least one un-fsynced delta");
    assert!(new > 0, "the seed slice must retain at least one complete delta");
    assert!(torn > 0, "the seed slice must exercise CRC tail truncation");
}

fn delta_before_covering_full() {
    let fixtures = Fixtures::new();
    let cfg = config(1 << 16);
    let fs = MemFs::new();
    let mut log = LogBuilder::new(fs.clone(), &cfg);
    let full = log.frame(&[fixtures.full(b"doc", 1, &fixtures.v1)], 0);
    log.frame(&[fixtures.delta(b"doc", 1)], full.end.to_u64());
    // Kill before the next cadence `DocFull`: the complete last delta is
    // sufficient and multi-match replay is one structural record.
    drop(log);

    let (mut ks, _stats) = recover(fs, &cfg);
    assert_doc(&mut ks, b"doc", &fixtures.v2);
}

fn torn_doc_full() {
    let fixtures = Fixtures::new();
    let cfg = config(1 << 16);
    let fs = MemFs::new();
    let mut log = LogBuilder::new(fs.clone(), &cfg);
    let full = log.frame(&[fixtures.full(b"doc", 1, &fixtures.v1)], 0);
    let delta = log.frame(&[fixtures.delta(b"doc", 1)], full.end.to_u64());
    let torn = log.frame(&[fixtures.full(b"doc", 3, &fixtures.torn_full)], delta.end.to_u64());
    let path = Path::new(LOG_DIR).join(segment_file_name(torn.base.segment));
    let bytes = fs.contents(&path).expect("segment contents");
    let at = usize::try_from(torn.base.offset).expect("offset") + FRAME_HEADER_LEN + 32;
    let flipped = [bytes[at] ^ 0xFF];
    fs.open_write(&path).expect("open segment").write_at(at as u64, &flipped).expect("tear");
    drop(log);

    let (mut ks, stats) = recover(fs, &cfg);
    assert_doc(&mut ks, b"doc", &fixtures.v2);
    assert_eq!(stats.torn_truncated_at, Some(torn.base));
}

fn checkpoint_before_segment_truncation() {
    let fixtures = Fixtures::new();
    let cfg = config(512);
    let fs = MemFs::new();
    let mut log = LogBuilder::new(fs.clone(), &cfg);
    let full = log.frame(&[fixtures.full(b"doc", 1, &fixtures.v1)], 0);
    let delta = log.frame(&[fixtures.delta(b"doc", 1)], full.end.to_u64());

    // Fill only with one overwritable key until the document's old log
    // segment is below the checkpoint floor. Its final image joins the
    // checkpoint, so the fixture remains a valid recovery unit.
    let filler = vec![b'x'; 220];
    for _ in 0..8 {
        if log.active_segment() > SegmentId(0) {
            break;
        }
        log.frame(
            &[MutationEffect::StringSet { ns: NS, key: b"filler", value: &filler }],
            delta.end.to_u64(),
        );
    }
    assert!(log.active_segment() > SegmentId(0), "fixture must rotate before checkpoint");
    let begin = log.frame(&[MutationEffect::CkptBegin { ckpt_id: 1 }], delta.end.to_u64());
    let tail = log.frame(&[fixtures.delta(b"doc", 2)], begin.end.to_u64());
    publish_checkpoint(
        &fs,
        &cfg,
        begin.first_record,
        tail.base.segment,
        &[
            RecordView::DocFull {
                ns: NS,
                key: b"doc",
                lineage: DocLineage::FIRST,
                version: 2,
                idoc: &fixtures.v2,
            },
            RecordView::StringPostImage { ns: NS, key: b"filler", value: &filler },
        ],
    );
    let stale_segment = Path::new(LOG_DIR).join(segment_file_name(SegmentId(0)));
    assert!(fs.contents(&stale_segment).is_some(), "cut lands before segment truncation");
    drop(log);

    let (mut ks, _stats) = recover(fs.clone(), &cfg);
    assert_doc(&mut ks, b"doc", &fixtures.v3);
    assert!(fs.contents(&stale_segment).is_none(), "boot GC removes checkpoint-covered segment");
}

fn fuzzy_overlap_lsn_classes() {
    let fixtures = Fixtures::new();
    let cfg = config(1 << 16);
    for prefix in 0..=4usize {
        let fs = MemFs::new();
        let mut log = LogBuilder::new(fs.clone(), &cfg);
        let begin = log.frame(&[MutationEffect::CkptBegin { ckpt_id: 1 }], 0);
        let events = [
            fixtures.delta(b"doc", 1),
            fixtures.delta(b"gone", 1),
            MutationEffect::Delete { ns: NS, key: b"gone" },
            fixtures.delta(b"doc", 2),
        ];
        let mut covered = begin.end.to_u64();
        for event in events.iter().take(prefix) {
            covered = log.frame(std::slice::from_ref(event), covered).end.to_u64();
        }
        publish_checkpoint(
            &fs,
            &cfg,
            begin.first_record,
            log.active_segment(),
            &[RecordView::DocFull {
                ns: NS,
                key: b"doc",
                lineage: DocLineage::FIRST,
                version: 2,
                idoc: &fixtures.v2,
            }],
        );
        drop(log);

        let (mut ks, stats) = recover(fs, &cfg);
        let expected = if prefix == events.len() { &fixtures.v3 } else { &fixtures.v2 };
        assert_doc(&mut ks, b"doc", expected);
        assert_missing(&mut ks, b"gone");
        assert_eq!(stats.doc_deltas_skipped_stale, u64::from(prefix >= 1));
        assert_eq!(stats.doc_deltas_skipped_missing, u64::from(prefix >= 2));
    }
}

#[test]
fn m3_document_crash_matrix() {
    let definition = load_matrix(&matrix_path());
    let expected = [
        "doc_delta_before_fsync",
        "doc_delta_before_covering_full",
        "torn_doc_full",
        "checkpoint_before_segment_truncation_with_delta_tail",
        "fuzzy_overlap_lsn_classes",
    ];
    assert_eq!(definition.rows.len(), expected.len(), "M3-S18 has exactly five named cut rows");

    for (row, expected_name) in definition.rows.iter().zip(expected) {
        assert_eq!(row.point, expected_name);
        assert_eq!(row.tier, "document");
        assert_eq!(row.policies, ["always"]);
        assert_eq!(row.test, "documents");
        match row.point.as_str() {
            "doc_delta_before_fsync" => delta_before_fsync(definition.seeds),
            "doc_delta_before_covering_full" => delta_before_covering_full(),
            "torn_doc_full" => torn_doc_full(),
            "checkpoint_before_segment_truncation_with_delta_tail" => {
                checkpoint_before_segment_truncation();
            }
            "fuzzy_overlap_lsn_classes" => fuzzy_overlap_lsn_classes(),
            other => panic!("unimplemented M3-S18 row {other}"),
        }
    }
}
