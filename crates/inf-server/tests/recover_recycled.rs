//! M4.5-S39b (ADR-0090 D2/D3/D5 as amended): recovery over the residue a
//! recycled segment carries — frames that decode at their own offset but
//! are stamped for another segment id. The crash rows of ADR-0090 D5 as
//! hand-built images on the `MemFs` seam under a `Direct` rotor: the
//! residue is a classified data end (never data), a proven-residue slack
//! (never a hole, never torn), an empty recycled next segment is a legal
//! tail, a torn this-life write over residue resumes at the data end, a
//! twice-recycled file classifies the same, and a **same-segment** frame
//! behind a hole still refuses through the new rule (the ADR-0031 D4
//! class is untouched). The sweep-scale rows live in `inf-sim
//! --scenario m2-recycle`.

use std::path::PathBuf;

use inf_foundation::time::Nanos;
use inf_log::fs::mem::MemFs;
use inf_log::fs::{SegmentFile, SegmentFs, SegmentIoMode};
use inf_log::{
    CkptConfig, FRAME_ALIGN, FRAME_HEADER_LEN, FrameBuilder, FrameLayout, FrameStamp, Lsn, NsId,
    RecordView, SegmentConfig, SegmentId, SegmentRotor, StagingConfig, create_cell_dirs,
    segment_file_name,
};
use inf_server::{DurableConfig, open_cell_log};
use inf_store::{FsyncClass, Keyspace, NsMode, NsSpec, StoreConfig, WallAnchor};

const NS: NsId = NsId(16);
const CELL: u16 = 0;
const SEGMENT_BYTES: u32 = 64 << 10;

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
        segment: SegmentConfig {
            segment_bytes: SEGMENT_BYTES,
            io_mode: SegmentIoMode::Direct,
            ..Default::default()
        },
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

fn get(ks: &mut Keyspace, key: &[u8]) -> Option<Vec<u8>> {
    ks.ns_store_mut(NS).expect("ns store").get(key, now()).map(<[u8]>::to_vec)
}

fn stamp(epoch: u32, seq: u64, covered_lsn: u64) -> FrameStamp {
    FrameStamp { epoch, seq, covered_lsn }
}

/// A fresh `Direct` cell dir with `seg-000000.ilog`; aligned (v3) frames
/// placed by hand at exact block offsets, stamped for whichever segment
/// the test says — the only way to author the residue a recycled file
/// carries without running a whole life.
struct HandLog {
    fs: MemFs,
    log_dir: PathBuf,
}

impl HandLog {
    fn new(fs: &MemFs) -> HandLog {
        let dirs = create_cell_dirs(fs, std::path::Path::new("data/shard-0")).expect("dirs");
        let rotor =
            SegmentRotor::create_fresh(fs.clone(), dirs.log.clone(), cfg().segment).expect("rotor");
        drop(rotor);
        HandLog { fs: fs.clone(), log_dir: dirs.log }
    }

    fn prealloc(&self, segment: SegmentId) {
        let file = self
            .fs
            .create_segment_direct(
                &self.log_dir.join(segment_file_name(segment)),
                u64::from(SEGMENT_BYTES),
            )
            .expect("prealloc");
        drop(file);
    }

    fn poke(&self, file_segment: SegmentId, offset: u32, bytes: &[u8]) {
        let mut file =
            self.fs.open_write(&self.log_dir.join(segment_file_name(file_segment))).expect("open");
        file.write_at(u64::from(offset), bytes).expect("write");
    }

    /// One aligned frame (`SET key value`) at block `block` of
    /// `file_segment`'s file, stamped as if written for `stamped_segment`
    /// at the same offset: `stamped == file` is this life's frame, any
    /// other id is recycled-life residue. Returns the frame's bytes.
    fn frame(
        &self,
        file_segment: SegmentId,
        stamped_segment: SegmentId,
        block: u32,
        key: &[u8],
        value: &[u8],
        stamp: FrameStamp,
    ) -> Vec<u8> {
        let offset = block * FRAME_ALIGN;
        let mut b = FrameBuilder::new();
        b.append(&RecordView::StringPostImage { ns: NS, key, value });
        let first = Lsn::new(stamped_segment, offset + FRAME_HEADER_LEN as u32);
        let bytes = b.finalize(first, stamp, FrameLayout::Aligned).to_vec();
        assert_eq!(bytes.len() as u32, FRAME_ALIGN, "one block per test frame");
        self.poke(file_segment, offset, &bytes);
        bytes
    }
}

fn recover(
    fs: &MemFs,
    ks: &mut Keyspace,
) -> std::io::Result<(SegmentRotor<MemFs>, inf_server::RecoverStats)> {
    open_cell_log(fs.clone(), ks, CELL, &cfg(), anchor(), now())
        .map(|(rotor, stats, _seed)| (rotor, stats))
}

const OLD: SegmentId = SegmentId(7);
const OLDER: SegmentId = SegmentId(3);

/// Row: this life's frames, then the previous life's — replay ends at the
/// first foreign frame, the slack is proven residue, nothing is torn,
/// the residue's records never replay, the next life's epoch tops the
/// residue's too.
#[test]
fn residue_behind_this_lifes_data_is_a_classified_end_never_data() {
    let fs = MemFs::new();
    let log = HandLog::new(&fs);
    let s0 = SegmentId(0);
    log.frame(s0, s0, 0, b"a", b"1", stamp(2, 1, 0));
    log.frame(s0, s0, 1, b"b", b"2", stamp(2, 2, 0));
    // Residue: seq-contiguous with the prefix and a higher epoch — the
    // hardest shape for a segment-blind reader (it would replay it).
    log.frame(s0, OLD, 2, b"ghost", b"x", stamp(2, 3, 0));
    log.frame(s0, OLD, 3, b"a", b"stale", stamp(3, 1, 0));

    let mut ks = fresh_keyspace();
    let (rotor, stats) = recover(&fs, &mut ks).expect("residue never refuses");
    assert_eq!(get(&mut ks, b"a").as_deref(), Some(&b"1"[..]), "residue never overwrote it");
    assert_eq!(get(&mut ks, b"b").as_deref(), Some(&b"2"[..]));
    assert_eq!(get(&mut ks, b"ghost"), None, "residue never replays");
    assert_eq!(stats.frames, 2);
    assert_eq!(stats.segment_residue_stops, 1);
    assert_eq!(stats.recycled_residue_slacks, 1);
    assert_eq!(stats.torn_truncated_at, None, "proven residue is not a torn tail");
    assert_eq!(stats.sealed_slack_remnants, 0);
    assert_eq!(stats.beyond_frames_discarded, 0, "not a hole");
    assert_eq!(stats.epoch_residue_stops, 0, "the segment rule fires before any epoch rule");
    assert_eq!(rotor.active_segment(), s0);
    assert_eq!(rotor.active_written(), 2 * FRAME_ALIGN, "resume at this life's data end");
    assert!(rotor.active_write_through(), "a recycled file is fully allocated");
    assert_eq!(rotor.resume_epoch(), 4, "tops the residue's epoch 3 as well as the prefix's 2");
}

/// Row: the rename survived the cut, nothing of this life was written —
/// an empty recycled next segment is a legal empty tail (resume at 0,
/// write-through from the first frame), not a torn trailing segment.
#[test]
fn an_empty_recycled_next_segment_is_a_legal_tail() {
    let fs = MemFs::new();
    let log = HandLog::new(&fs);
    let (s0, s1) = (SegmentId(0), SegmentId(1));
    log.frame(s0, s0, 0, b"a", b"1", stamp(1, 1, 0));
    log.prealloc(s1);
    for block in 0..4 {
        log.frame(s1, OLD, block, b"ghost", b"x", stamp(1, 10 + u64::from(block), 0));
    }

    let mut ks = fresh_keyspace();
    let (rotor, stats) = recover(&fs, &mut ks).expect("an empty recycled next never refuses");
    assert_eq!(get(&mut ks, b"a").as_deref(), Some(&b"1"[..]));
    assert_eq!(get(&mut ks, b"ghost"), None);
    assert_eq!(stats.segments, 2);
    assert_eq!(stats.segment_residue_stops, 1);
    assert_eq!(stats.recycled_residue_slacks, 1);
    assert_eq!(stats.torn_truncated_at, None);
    assert_eq!(stats.torn_segments_removed, 0, "kept, like a zero-filled next");
    assert_eq!(rotor.active_segment(), s1);
    assert_eq!(rotor.active_written(), 0);
    assert!(rotor.active_write_through());
    assert_eq!(rotor.resume_epoch(), 2);
}

/// Row: a sealed recycled segment in the middle of the live log — its
/// residue tail ends that segment's data; the next segment replays.
#[test]
fn a_sealed_recycled_segment_does_not_stop_the_live_log() {
    let fs = MemFs::new();
    let log = HandLog::new(&fs);
    let (s0, s1) = (SegmentId(0), SegmentId(1));
    log.frame(s0, s0, 0, b"a", b"1", stamp(1, 1, 0));
    log.frame(s0, OLD, 1, b"ghost", b"x", stamp(1, 9, 0));
    log.prealloc(s1);
    log.frame(s1, s1, 0, b"b", b"2", stamp(1, 2, 0));

    let mut ks = fresh_keyspace();
    let (rotor, stats) = recover(&fs, &mut ks).expect("replays past the sealed residue");
    assert_eq!(get(&mut ks, b"a").as_deref(), Some(&b"1"[..]));
    assert_eq!(get(&mut ks, b"b").as_deref(), Some(&b"2"[..]));
    assert_eq!(get(&mut ks, b"ghost"), None);
    assert_eq!(stats.segment_residue_stops, 1);
    assert_eq!(stats.recycled_residue_slacks, 1);
    assert_eq!(stats.sealed_slack_remnants, 0, "residue is not a remnant anomaly");
    assert_eq!(stats.torn_truncated_at, None);
    assert_eq!(rotor.active_segment(), s1);
    assert_eq!(rotor.active_written(), FRAME_ALIGN);
}

/// Row: a torn write of this life over residue — the partial frame is
/// garbage at the data end, the foreign frames behind it are residue;
/// resume at the data end, nothing refused, nothing of this life lost.
#[test]
fn a_torn_this_life_write_over_residue_resumes_at_the_data_end() {
    let fs = MemFs::new();
    let log = HandLog::new(&fs);
    let s0 = SegmentId(0);
    log.frame(s0, s0, 0, b"a", b"1", stamp(2, 1, 0));
    // Residue across blocks 1..3, then this life's torn frame over block 1.
    for block in 1..3 {
        log.frame(s0, OLD, block, b"ghost", b"x", stamp(1, 5 + u64::from(block), 0));
    }
    let torn = {
        let mut b = FrameBuilder::new();
        b.append(&RecordView::StringPostImage { ns: NS, key: b"b", value: b"2" });
        let first = Lsn::new(s0, FRAME_ALIGN + FRAME_HEADER_LEN as u32);
        b.finalize(first, stamp(2, 2, 0), FrameLayout::Aligned).to_vec()
    };
    // Thirty bytes of this life's frame land (a header with its length,
    // the body cut mid-way); the rest of the block is still the residue
    // frame's bytes — the CRC fails, the block is garbage at the data end.
    log.poke(s0, FRAME_ALIGN, &torn[..30]);

    let mut ks = fresh_keyspace();
    let (rotor, stats) = recover(&fs, &mut ks).expect("a torn write over residue never refuses");
    assert_eq!(get(&mut ks, b"a").as_deref(), Some(&b"1"[..]));
    assert_eq!(get(&mut ks, b"b"), None, "the torn frame was never covered, never acked");
    assert_eq!(get(&mut ks, b"ghost"), None);
    assert_eq!(stats.recycled_residue_slacks, 1);
    assert_eq!(stats.segment_residue_stops, 0, "the data end was a torn frame, not a foreign one");
    assert_eq!(stats.torn_truncated_at, None, "indistinguishable from residue — disclosed so");
    assert_eq!(rotor.active_written(), FRAME_ALIGN);
}

/// Row: a file recycled twice carries residue stamped with two previous
/// ids — both foreign, the same verdict.
#[test]
fn a_twice_recycled_file_classifies_the_same() {
    let fs = MemFs::new();
    let log = HandLog::new(&fs);
    let s0 = SegmentId(0);
    log.frame(s0, s0, 0, b"a", b"1", stamp(3, 1, 0));
    log.frame(s0, OLD, 1, b"ghost", b"x", stamp(2, 4, 0));
    log.frame(s0, OLDER, 2, b"ghost2", b"y", stamp(1, 8, 0));
    log.frame(s0, OLD, 3, b"ghost3", b"z", stamp(2, 5, 0));

    let mut ks = fresh_keyspace();
    let (rotor, stats) = recover(&fs, &mut ks).expect("two previous lives are still residue");
    assert_eq!(get(&mut ks, b"a").as_deref(), Some(&b"1"[..]));
    assert_eq!(get(&mut ks, b"ghost"), None);
    assert_eq!(get(&mut ks, b"ghost2"), None);
    assert_eq!(stats.segment_residue_stops, 1);
    assert_eq!(stats.recycled_residue_slacks, 1);
    assert_eq!(rotor.active_written(), FRAME_ALIGN);
    assert_eq!(rotor.resume_epoch(), 4);
}

/// M4.5-S39d: the per-phase accounting on the synchronous tier — the
/// audit counted the slack's bytes and its foreign frames, replay counted
/// this life's bytes and frames, and every duration is zero (no clock
/// here: `open_cell_log` has nothing to credit — the loop tier does).
#[test]
fn phase_accounting_counts_audit_bytes_and_foreign_frames_without_a_clock() {
    let fs = MemFs::new();
    let log = HandLog::new(&fs);
    let s0 = SegmentId(0);
    log.frame(s0, s0, 0, b"a", b"1", stamp(3, 1, 0));
    log.frame(s0, OLD, 1, b"ghost", b"x", stamp(2, 4, 0));
    log.frame(s0, OLDER, 2, b"ghost2", b"y", stamp(1, 8, 0));

    let mut ks = fresh_keyspace();
    let (_rotor, stats) = recover(&fs, &mut ks).expect("residue never refuses");
    let phases = stats.phases;
    assert_eq!(phases.ckpt_bytes, 0, "no checkpoint in this image");
    assert_eq!(phases.replay_frames, 1);
    assert_eq!(phases.replay_frames, stats.frames);
    assert!(phases.replay_bytes >= FRAME_ALIGN as u64, "this life's frame was read");
    assert_eq!(phases.audit_foreign_frames, 2, "both residue frames CRC-validated in the audit");
    assert_eq!(phases.audit_valid_frames, 0);
    assert!(
        phases.audit_bytes >= 2 * FRAME_ALIGN as u64,
        "the audit read the slack behind the data end: {phases:?}"
    );
    assert_eq!(phases.phase_ns(), [0; 5], "no clock on the synchronous tier");
    assert_eq!(phases.total_ns, 0);
    assert_eq!(phases.dominating(), None);
}

/// Refusal row: a **same-segment** validating frame behind a hole of this
/// life, attesting coverage past the data end, must still refuse through
/// the new rule — foreign frames around it change nothing (ADR-0031 D4).
#[test]
fn a_same_segment_frame_behind_a_hole_still_refuses_through_the_new_rule() {
    let fs = MemFs::new();
    let log = HandLog::new(&fs);
    let s0 = SegmentId(0);
    log.frame(s0, s0, 0, b"a", b"1", stamp(2, 1, 0));
    // Foreign residue where this life's second frame should be …
    log.frame(s0, OLD, 1, b"ghost", b"x", stamp(1, 9, 0));
    // … and this life's third frame, attesting coverage past the gap.
    let covered = Lsn::new(s0, 2 * FRAME_ALIGN).to_u64();
    log.frame(s0, s0, 2, b"c", b"3", stamp(2, 3, covered));

    let mut ks = fresh_keyspace();
    let err = recover(&fs, &mut ks).expect_err("covered data lost behind the residue");
    assert!(err.to_string().contains("covered data was lost"), "{err}");
}

/// The sibling: the same-segment frame behind the hole attests nothing —
/// the legal remainder of an un-covered reorder hole; truncated with the
/// torn tail as today, the foreign frames counted as nothing.
#[test]
fn an_unattested_same_segment_frame_behind_a_hole_truncates_as_today() {
    let fs = MemFs::new();
    let log = HandLog::new(&fs);
    let s0 = SegmentId(0);
    log.frame(s0, s0, 0, b"a", b"1", stamp(2, 1, 0));
    log.frame(s0, OLD, 1, b"ghost", b"x", stamp(1, 9, 0));
    log.frame(s0, s0, 2, b"c", b"3", stamp(2, 3, 0));

    let mut ks = fresh_keyspace();
    let (rotor, stats) = recover(&fs, &mut ks).expect("an un-covered hole truncates");
    assert_eq!(get(&mut ks, b"a").as_deref(), Some(&b"1"[..]));
    assert_eq!(get(&mut ks, b"c"), None, "beyond the hole: discarded");
    assert_eq!(get(&mut ks, b"ghost"), None);
    assert_eq!(stats.beyond_frames_discarded, 1);
    assert_eq!(stats.recycled_residue_slacks, 0, "a slack with this life's frame is a hole");
    assert_eq!(stats.torn_truncated_at, Some(Lsn::new(s0, FRAME_ALIGN)));
    assert_eq!(rotor.active_written(), FRAME_ALIGN);
}

/// Determinism of the rule: the same image recovers to the same resume
/// point and counters twice (the DST oracle's currency).
#[test]
fn recycled_residue_recovery_is_deterministic() {
    let fs = MemFs::new();
    let log = HandLog::new(&fs);
    let s0 = SegmentId(0);
    log.frame(s0, s0, 0, b"a", b"1", stamp(2, 1, 0));
    log.frame(s0, OLD, 1, b"ghost", b"x", stamp(1, 9, 0));
    let mut ks1 = fresh_keyspace();
    let (r1, st1) = recover(&fs, &mut ks1).expect("first");
    drop(r1);
    let mut ks2 = fresh_keyspace();
    let (r2, st2) = recover(&fs, &mut ks2).expect("second");
    assert_eq!(ks1.state_digest(now()), ks2.state_digest(now()));
    assert_eq!(st1.recycled_residue_slacks, st2.recycled_residue_slacks);
    assert_eq!(r2.active_written(), FRAME_ALIGN);
}
