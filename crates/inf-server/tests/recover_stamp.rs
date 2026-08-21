//! M2.5-S12 (ADR-0031) recovery policy over the v2 frame stamp, end to end
//! on the injected `MemFs` seam: mixed v1→v2 logs replay (the upgrade
//! shape); a v1 frame *after* a v2 frame refuses (no honest writer);
//! seq/attestation continuity violations between byte-adjacent validating
//! frames refuse; an epoch regression at a frame boundary ends replay as
//! discarded-life residue (never applied, never fatal); and every
//! recovery-for-append derives an epoch above everything it observed.
//! The beyond-the-data-end attestation taxonomy lives in
//! `recover_torn.rs`; scan mechanics in `inf-log/tests/tail_scan.rs`.

use std::path::PathBuf;

use inf_foundation::time::Nanos;
use inf_log::FrameLayout;
use inf_log::fs::mem::MemFs;
use inf_log::fs::{SegmentFile, SegmentFs};
use inf_log::{
    CkptConfig, FRAME_HEADER_LEN, FrameBuilder, FrameStamp, Lsn, NsId, RecordView, SegmentConfig,
    SegmentId, SegmentRotor, StagingConfig, create_cell_dirs, segment_file_name,
};
use inf_server::{DurableConfig, open_cell_log};
use inf_store::{FsyncClass, Keyspace, NsMode, NsSpec, StoreConfig, WallAnchor};

const NS: NsId = NsId(16);
const CELL: u16 = 0;

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
        segment: SegmentConfig { segment_bytes: 1 << 16, ..Default::default() },
        ckpt: CkptConfig::default(),
        recover: Default::default(),
        flush_bound: 1,
        fua_p50_us_probed: 0,
        device: Default::default(),
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

fn get(ks: &mut Keyspace, key: &[u8]) -> Option<Vec<u8>> {
    ks.ns_store_mut(NS).expect("ns store").get(key, now()).map(<[u8]>::to_vec)
}

/// A fresh cell dir with a preallocated `seg-000000.ilog`, frames placed
/// by hand at exact offsets — the only way to author v1 (legacy) and
/// adversarial stamp shapes no shipping writer emits.
struct HandLog {
    fs: MemFs,
    log_dir: PathBuf,
    offset: u32,
}

impl HandLog {
    fn new(fs: &MemFs) -> HandLog {
        let dirs = create_cell_dirs(fs, std::path::Path::new("data/shard-0")).expect("dirs");
        let rotor =
            SegmentRotor::create_fresh(fs.clone(), dirs.log.clone(), cfg().segment).expect("rotor");
        drop(rotor);
        HandLog { fs: fs.clone(), log_dir: dirs.log, offset: 0 }
    }

    fn poke(&self, offset: u32, bytes: &[u8]) {
        let mut file =
            self.fs.open_write(&self.log_dir.join(segment_file_name(SegmentId(0)))).expect("open");
        file.write_at(u64::from(offset), bytes).expect("write");
    }

    /// Append one v2 frame (`SET key value`) with `stamp` at the current
    /// cursor; returns its base offset.
    fn v2_frame(&mut self, key: &[u8], value: &[u8], stamp: FrameStamp) -> u32 {
        let mut b = FrameBuilder::new();
        b.append(&RecordView::StringPostImage { ns: NS, key, value });
        let first = Lsn::new(SegmentId(0), self.offset + FRAME_HEADER_LEN as u32);
        let bytes = b.finalize(first, stamp, FrameLayout::Packed).to_vec();
        let at = self.offset;
        self.poke(at, &bytes);
        self.offset += u32::try_from(bytes.len()).expect("fits u32");
        at
    }

    /// Append one legacy v1 frame (magic `IFR1`, 20-byte header, no stamp)
    /// at the current cursor; returns its base offset.
    fn v1_frame(&mut self, key: &[u8], value: &[u8]) -> u32 {
        let record = RecordView::StringPostImage { ns: NS, key, value };
        let mut body = Vec::new();
        record.encode_into(&mut body);
        let frame_len = u32::try_from(20 + body.len() + 4).expect("fits u32");
        let mut frame = Vec::with_capacity(frame_len as usize);
        frame.extend_from_slice(b"IFR1");
        frame.extend_from_slice(&frame_len.to_le_bytes());
        frame.extend_from_slice(&1u32.to_le_bytes());
        frame.extend_from_slice(&0u32.to_le_bytes());
        frame.extend_from_slice(&(self.offset + 20).to_le_bytes());
        frame.extend_from_slice(&body);
        let crc = inf_simd::crc32c(&frame);
        frame.extend_from_slice(&crc.to_le_bytes());
        let at = self.offset;
        self.poke(at, &frame);
        self.offset += frame_len;
        at
    }
}

fn recover(
    fs: &MemFs,
    ks: &mut Keyspace,
) -> std::io::Result<(SegmentRotor<MemFs>, inf_server::RecoverStats)> {
    open_cell_log(fs.clone(), ks, CELL, &cfg(), anchor(), now())
        .map(|(rotor, stats, _seed)| (rotor, stats))
}

fn stamp(epoch: u32, seq: u64, covered_lsn: u64) -> FrameStamp {
    FrameStamp { epoch, seq, covered_lsn }
}

#[test]
fn mixed_v1_then_v2_log_replays_and_resumes_above_the_observed_epoch() {
    // The upgrade shape (ADR-0031 D2): a log written by an alpha.1 binary
    // (v1 frames), resumed by this binary (v2 frames), replays whole; the
    // next life's epoch tops the v2 frames it saw.
    let fs = MemFs::new();
    let mut log = HandLog::new(&fs);
    log.v1_frame(b"a", b"1");
    log.v2_frame(b"b", b"2", stamp(4, 7, 0));
    log.v2_frame(b"c", b"3", stamp(4, 8, 0));

    let mut ks = fresh_keyspace();
    let (rotor, stats) = recover(&fs, &mut ks).expect("mixed-era log replays");
    assert_eq!(get(&mut ks, b"a").as_deref(), Some(&b"1"[..]));
    assert_eq!(get(&mut ks, b"b").as_deref(), Some(&b"2"[..]));
    assert_eq!(get(&mut ks, b"c").as_deref(), Some(&b"3"[..]));
    assert_eq!(stats.frames, 3);
    assert_eq!(rotor.resume_epoch(), 5, "resume tops the observed epoch 4");
}

#[test]
fn v1_frame_after_a_v2_frame_refuses_to_start() {
    // Append order makes a v1 frame after a v2 frame unreachable by any
    // honest writer (alpha.1 cannot read v2 logs — ADR-0031 D2).
    let fs = MemFs::new();
    let mut log = HandLog::new(&fs);
    log.v2_frame(b"a", b"1", stamp(1, 1, 0));
    log.v1_frame(b"b", b"2");

    let mut ks = fresh_keyspace();
    let err = recover(&fs, &mut ks).expect_err("v1-after-v2 is fail-stop");
    assert!(err.to_string().contains("format-v1 frame follows a v2 frame"), "{err}");
}

#[test]
fn seq_gap_between_adjacent_frames_refuses_to_start() {
    // Byte-adjacent validating frames with a seq hole: no honest writer
    // emits it (within a life, seq → offset is contiguous — ADR-0031 D3).
    let fs = MemFs::new();
    let mut log = HandLog::new(&fs);
    log.v2_frame(b"a", b"1", stamp(1, 1, 0));
    log.v2_frame(b"b", b"2", stamp(1, 3, 0));

    let mut ks = fresh_keyspace();
    let err = recover(&fs, &mut ks).expect_err("a prefix seq gap is fail-stop");
    let msg = err.to_string();
    assert!(msg.contains("seq 3 follows seq 1"), "{msg}");
    assert!(msg.contains("refusing to start"), "{msg}");
}

#[test]
fn epoch_step_without_seq_restart_refuses_to_start() {
    let fs = MemFs::new();
    let mut log = HandLog::new(&fs);
    log.v2_frame(b"a", b"1", stamp(1, 1, 0));
    log.v2_frame(b"b", b"2", stamp(2, 5, 0));

    let mut ks = fresh_keyspace();
    let err = recover(&fs, &mut ks).expect_err("an epoch step stamps seq 1");
    assert!(err.to_string().contains("epoch stepped 1 → 2 but seq is 5"), "{err}");
}

#[test]
fn attestation_regression_within_an_epoch_refuses_to_start() {
    // The watermark is monotone within a life; a covered_lsn regression
    // between adjacent frames is forged or corrupt evidence (ADR-0031 D3).
    let fs = MemFs::new();
    let mut log = HandLog::new(&fs);
    let first = log.v2_frame(b"a", b"1", stamp(1, 1, 0));
    let second_base_guess = log.offset;
    log.v2_frame(b"b", b"2", stamp(1, 2, u64::from(second_base_guess)));
    log.v2_frame(b"c", b"3", stamp(1, 3, u64::from(first)));

    let mut ks = fresh_keyspace();
    let err = recover(&fs, &mut ks).expect_err("attestation regression is fail-stop");
    assert!(err.to_string().contains("attestation regressed"), "{err}");
}

#[test]
fn epoch_regression_at_a_frame_boundary_truncates_as_residue() {
    // ADR-0031 D5: a durably-whole frame from a discarded life resurfacing
    // exactly at the new life's data end must never replay — its lower
    // epoch ends replay there and the torn-tail machinery discards it.
    let fs = MemFs::new();
    let mut log = HandLog::new(&fs);
    log.v2_frame(b"live", b"1", stamp(5, 1, 0));
    let residue_at = log.v2_frame(b"ghost", b"stale", stamp(2, 9, 0));

    let mut ks = fresh_keyspace();
    let (rotor, stats) = recover(&fs, &mut ks).expect("epoch residue truncates, never applies");
    assert_eq!(get(&mut ks, b"live").as_deref(), Some(&b"1"[..]));
    assert_eq!(get(&mut ks, b"ghost"), None, "discarded-life records never replay");
    assert_eq!(stats.epoch_residue_stops, 1);
    assert_eq!(stats.beyond_frames_discarded, 1, "the residue frame is audit-counted");
    assert_eq!(stats.torn_truncated_at, Some(Lsn::new(SegmentId(0), residue_at)));
    assert_eq!(rotor.resume_epoch(), 6, "resume tops the live epoch, not just the residue");
}
