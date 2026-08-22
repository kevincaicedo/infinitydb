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
        fill: Default::default(),
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
        self.poke_in(SegmentId(0), offset, bytes);
    }

    fn poke_in(&self, segment: SegmentId, offset: u32, bytes: &[u8]) {
        let mut file =
            self.fs.open_write(&self.log_dir.join(segment_file_name(segment))).expect("open");
        file.write_at(u64::from(offset), bytes).expect("write");
    }

    /// Append one v2 frame (`SET key value`) with `stamp` at the current
    /// cursor; returns its base offset.
    fn v2_frame(&mut self, key: &[u8], value: &[u8], stamp: FrameStamp) -> u32 {
        let at = self.offset;
        let len = self.v2_frame_in(SegmentId(0), at, key, value, stamp);
        self.offset += len;
        at
    }

    /// Place one v2 frame at `offset` of `segment` (which must exist);
    /// returns its length.
    fn v2_frame_in(
        &self,
        segment: SegmentId,
        offset: u32,
        key: &[u8],
        value: &[u8],
        stamp: FrameStamp,
    ) -> u32 {
        let mut b = FrameBuilder::new();
        b.append(&RecordView::StringPostImage { ns: NS, key, value });
        let first = Lsn::new(segment, offset + FRAME_HEADER_LEN as u32);
        let bytes = b.finalize(first, stamp, FrameLayout::Packed).to_vec();
        self.poke_in(segment, offset, &bytes);
        u32::try_from(bytes.len()).expect("fits u32")
    }

    /// Preallocate `segment` (zeros, the rotor's shape) so frames can be
    /// placed in it.
    fn prealloc(&self, segment: SegmentId) {
        let file = self
            .fs
            .create_segment(&self.log_dir.join(segment_file_name(segment)), 1 << 16)
            .expect("prealloc");
        drop(file);
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
fn epoch_regression_at_a_frame_boundary_ends_replay_as_stale_residue() {
    // ADR-0031 D5 (as amended 2026-08-21): a durably-whole frame from a
    // discarded life resurfacing exactly at the new life's data end must
    // never replay — its lower epoch ends replay there, and the audit
    // classifies it by epoch as **stale residue**, never a hole: no
    // truncation of this life's data, no attestation check against a
    // discarded life's watermark (a residue frame attesting past the data
    // end used to refuse an honest image), resume at the data end.
    let fs = MemFs::new();
    let mut log = HandLog::new(&fs);
    log.v2_frame(b"live", b"1", stamp(5, 1, 0));
    let residue_at = log.v2_frame(b"ghost", b"stale", stamp(2, 9, 0));
    // A second residue frame attesting the first — past this life's data
    // end — is the discarded life's watermark, not evidence about ours.
    log.v2_frame(b"ghost2", b"stale", stamp(2, 10, u64::from(residue_at)));

    let mut ks = fresh_keyspace();
    let (rotor, stats) = recover(&fs, &mut ks).expect("epoch residue ends replay, never refuses");
    assert_eq!(get(&mut ks, b"live").as_deref(), Some(&b"1"[..]));
    assert_eq!(get(&mut ks, b"ghost"), None, "discarded-life records never replay");
    assert_eq!(stats.epoch_residue_stops, 1);
    assert_eq!(stats.stale_residue_slacks, 1, "the residue is classified by epoch");
    assert_eq!(stats.beyond_frames_discarded, 0, "not a hole: nothing of this life discarded");
    assert_eq!(stats.torn_truncated_at, None, "not a torn tail");
    assert_eq!(rotor.active_written(), residue_at, "resume at this life's data end");
    assert_eq!(rotor.resume_epoch(), 6, "resume tops the live epoch, not just the residue");
}

#[test]
fn stale_residue_beyond_a_gap_is_not_a_hole_when_a_later_life_lies_beyond() {
    // The shape the `m2-mode-transition` sweep found (ADR-0031 D5 as
    // amended, the non-local half): life 1's torn tail at `gap` left a
    // validating life-1 frame beyond a zero gap; recovery truncated the
    // pointer at `gap` (bytes never rewritten); life 2 rotated onto the
    // next segment before writing anything into segment 0 (the ADR-0086
    // D4 class-upgrade rotation), so the residue sealed in. Locally the
    // prefix and the residue share a life; segment 1's epoch-2 frames
    // prove a recovery already resumed past it — replay must continue
    // there instead of refusing "covered data was lost".
    let fs = MemFs::new();
    let mut log = HandLog::new(&fs);
    let gap = {
        log.v2_frame(b"a", b"1", stamp(1, 1, 0));
        log.v2_frame(b"b", b"2", stamp(1, 2, 0));
        log.offset
    };
    // The torn frame (zeros) then the survivor of life 1 beyond it.
    let residue_at = gap + 63;
    log.v2_frame_in(SegmentId(0), residue_at, b"ghost", b"stale", stamp(1, 4, u64::from(gap)));
    // Life 2 resumed at `gap` and wrote segment 1; its frames attest
    // coverage up to segment 1 (past segment 0's data end in LSN order).
    log.prealloc(SegmentId(1));
    let first = Lsn::new(SegmentId(1), 0);
    let len = log.v2_frame_in(SegmentId(1), 0, b"c", b"3", stamp(2, 1, u64::from(gap)));
    let second_covered = first.advance(len).to_u64();
    log.v2_frame_in(SegmentId(1), len, b"d", b"4", stamp(2, 2, second_covered));

    let mut ks = fresh_keyspace();
    let (rotor, stats) = recover(&fs, &mut ks).expect("stale residue never refuses the live log");
    assert_eq!(get(&mut ks, b"a").as_deref(), Some(&b"1"[..]));
    assert_eq!(get(&mut ks, b"b").as_deref(), Some(&b"2"[..]));
    assert_eq!(get(&mut ks, b"ghost"), None, "the discarded life's frame never replays");
    assert_eq!(get(&mut ks, b"c").as_deref(), Some(&b"3"[..]), "life 2 replays");
    assert_eq!(get(&mut ks, b"d").as_deref(), Some(&b"4"[..]));
    assert_eq!(stats.segments, 2);
    assert_eq!(stats.stale_residue_slacks, 1);
    assert_eq!(stats.sealed_slack_remnants, 1, "segment 0's residue is a remnant");
    assert_eq!(stats.beyond_frames_discarded, 0);
    assert_eq!(stats.torn_truncated_at, None);
    assert_eq!(rotor.active_segment(), SegmentId(1));
    assert_eq!(rotor.resume_epoch(), 3);
}

#[test]
fn stale_residue_inside_a_sealed_segment_does_not_stop_the_live_log() {
    // The sibling shape: life 2 wrote one short frame at the resume point
    // and rotated; the discarded life's frame right behind it regresses
    // the epoch mid-segment. Replay ends that segment there (ADR-0031 D5),
    // the audit classifies the residue by epoch, and segment 1 — the live
    // log — replays instead of being written off as residue.
    let fs = MemFs::new();
    let mut log = HandLog::new(&fs);
    log.v2_frame(b"a", b"1", stamp(1, 1, 0));
    let resume = log.offset;
    // Life 2's short frame at the resume point, then life 1's survivor.
    let short = log.v2_frame(b"b", b"2", stamp(2, 1, u64::from(resume)));
    let ghost_at = log.offset;
    log.v2_frame(b"ghost", b"stale", stamp(1, 3, u64::from(short)));
    log.prealloc(SegmentId(1));
    log.v2_frame_in(SegmentId(1), 0, b"c", b"3", stamp(2, 2, u64::from(ghost_at)));

    let mut ks = fresh_keyspace();
    let (rotor, stats) = recover(&fs, &mut ks).expect("the live log replays past the residue");
    assert_eq!(get(&mut ks, b"a").as_deref(), Some(&b"1"[..]));
    assert_eq!(get(&mut ks, b"b").as_deref(), Some(&b"2"[..]));
    assert_eq!(get(&mut ks, b"ghost"), None);
    assert_eq!(get(&mut ks, b"c").as_deref(), Some(&b"3"[..]));
    assert_eq!(stats.epoch_residue_stops, 1);
    assert_eq!(stats.stale_residue_slacks, 1);
    assert_eq!(stats.torn_truncated_at, None);
    assert_eq!(rotor.active_segment(), SegmentId(1));
    assert_eq!(rotor.resume_epoch(), 3);
}

#[test]
fn a_hole_of_the_current_life_with_later_attestation_still_refuses() {
    // The lying-device shape the amendment must keep refusing: life 1
    // lost a covered frame at `gap` (zeros) while its own later frame
    // survived and attests coverage past the gap — same epoch on both
    // sides of the gap, so it is a hole, and the attestation proves the
    // loss (ADR-0031 D4).
    let fs = MemFs::new();
    let mut log = HandLog::new(&fs);
    log.v2_frame(b"a", b"1", stamp(1, 1, 0));
    let gap = log.offset;
    let beyond = gap + 63;
    log.v2_frame_in(SegmentId(0), beyond, b"b", b"2", stamp(1, 3, u64::from(beyond)));

    let mut ks = fresh_keyspace();
    let err = recover(&fs, &mut ks).expect_err("covered data lost is fail-stop");
    assert!(err.to_string().contains("covered data was lost"), "{err}");
}
