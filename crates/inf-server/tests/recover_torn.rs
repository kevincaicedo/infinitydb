//! M2-S14 recovery policy over the tail taxonomy, end to end on the
//! injected `MemFs` seam: a torn *final* write recovers minus the torn
//! frame (truncate the tail pointer, never bytes) and the cell keeps
//! serving; interior corruption — a validating frame beyond the corrupt
//! region — refuses to start with a `LogCorruption` naming segment and
//! offset (§8.4). Scan mechanics live in `inf-log/tests/tail_scan.rs`;
//! crash-matrix rows over these paths bind at M2-S17.

use std::path::{Path, PathBuf};

use inf_foundation::time::Nanos;
use inf_log::FrameLayout;
use inf_log::ckpt::SyncIckWriter;
use inf_log::fs::mem::MemFs;
use inf_log::fs::{SegmentFile, SegmentFs};
use inf_log::{
    CkptConfig, FRAME_HEADER_LEN, FrameBuilder, FrameStamp, Lsn, Manifest, MutationEffect, NsId,
    RecordView, SegmentConfig, SegmentId, SegmentRotor, StagingConfig, StagingRing,
    create_cell_dirs, segment_file_name, write_manifest,
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

/// One flushed frame per call: `SET key value` records at known LSNs.
struct LogBuilder {
    fs: MemFs,
    rotor: SegmentRotor<MemFs>,
    ring: StagingRing,
    log_dir: PathBuf,
}

impl LogBuilder {
    fn new(fs: &MemFs, cfg: &DurableConfig) -> LogBuilder {
        let dirs = create_cell_dirs(fs, &cfg.data_dir.join(format!("shard-{CELL}"))).expect("dirs");
        let rotor =
            SegmentRotor::create_fresh(fs.clone(), dirs.log.clone(), cfg.segment).expect("rotor");
        LogBuilder { fs: fs.clone(), rotor, ring: StagingRing::new(cfg.staging), log_dir: dirs.log }
    }

    /// Stage `records` as one frame, flush it, and return the frame's
    /// (base offset, per-record LSNs). Frames attest like a live `always`
    /// plane (ADR-0031 D1): each stamps `covered_lsn` = its own base — the
    /// watermark of a group commit that fsynced every prior frame.
    fn frame(&mut self, records: &[MutationEffect<'_>]) -> (Lsn, Vec<Lsn>) {
        let staged: Vec<_> =
            records.iter().map(|effect| self.ring.stage(effect).expect("stage")).collect();
        self.rotor.maintain(0).expect("maintain");
        let slot = self.rotor.begin_frame(self.ring.pending_frame_len(), 0).expect("reserve");
        let covered = slot.base().to_u64();
        let lease = self.ring.seal(slot.first_record_lsn(), covered, slot.layout());
        let frame = self.ring.leased_frame(&lease).to_vec();
        self.rotor.commit_frame(slot, &frame).expect("commit");
        let lsns: Vec<Lsn> = staged.iter().map(|&at| lease.lsn_of(at)).collect();
        self.ring.release(lease);
        let first = lsns[0];
        (Lsn::new(first.segment, first.offset - FRAME_HEADER_LEN as u32), lsns)
    }

    fn set_frame(&mut self, key: &[u8], value: &[u8]) -> (Lsn, Vec<Lsn>) {
        self.frame(&[MutationEffect::StringSet { ns: NS, key, value }])
    }

    fn seg_path(&self, id: SegmentId) -> PathBuf {
        self.log_dir.join(segment_file_name(id))
    }

    fn poke(&self, id: SegmentId, offset: u32, bytes: &[u8]) {
        let mut file = self.fs.open_write(&self.seg_path(id)).expect("open");
        file.write_at(u64::from(offset), bytes).expect("poke");
    }
}

fn recover(
    fs: &MemFs,
    ks: &mut Keyspace,
) -> std::io::Result<(SegmentRotor<MemFs>, inf_server::RecoverStats)> {
    open_cell_log(fs.clone(), ks, CELL, &cfg(), anchor(), now())
        .map(|(rotor, stats, _seed)| (rotor, stats))
}

fn get(ks: &mut Keyspace, key: &[u8]) -> Option<Vec<u8>> {
    ks.ns_store_mut(NS).expect("ns store").get(key, now()).map(<[u8]>::to_vec)
}

#[test]
fn torn_final_frame_recovers_minus_the_torn_frame() {
    let fs = MemFs::new();
    let config = cfg();
    let mut log = LogBuilder::new(&fs, &config);
    log.set_frame(b"a", b"1");
    log.set_frame(b"b", b"2");
    let (torn_base, _) = log.set_frame(b"c", b"3");
    // Tear the final frame mid-body: header intact, CRC now wrong.
    log.poke(torn_base.segment, torn_base.offset + FRAME_HEADER_LEN as u32 + 2, &[0, 0, 0]);

    let mut ks = fresh_keyspace();
    let (rotor, stats) = recover(&fs, &mut ks).expect("torn tail must recover");
    assert_eq!(get(&mut ks, b"a").as_deref(), Some(&b"1"[..]));
    assert_eq!(get(&mut ks, b"b").as_deref(), Some(&b"2"[..]));
    assert_eq!(get(&mut ks, b"c"), None, "the torn frame's record is gone");
    assert_eq!(stats.torn_truncated_at, Some(torn_base), "truncated at the torn frame's base");
    assert_eq!(
        (rotor.active_segment(), rotor.active_written()),
        (torn_base.segment, torn_base.offset),
        "the rotor resumes exactly at the truncation point"
    );
}

#[test]
fn resume_over_remnants_then_recover_again_is_clean() {
    let fs = MemFs::new();
    let config = cfg();
    let mut log = LogBuilder::new(&fs, &config);
    log.set_frame(b"a", b"1");
    // A fat torn frame: its remnants extend far past whatever is written
    // over them next.
    let value = vec![0xEE; 900];
    let (torn_base, _) =
        log.frame(&[MutationEffect::StringSet { ns: NS, key: b"fat", value: &value }]);
    log.poke(torn_base.segment, torn_base.offset + 8, &[0xFF; 4]);

    // First boot: torn tail truncated.
    let mut ks = fresh_keyspace();
    let (mut rotor, stats) = recover(&fs, &mut ks).expect("torn tail recovers");
    assert!(stats.torn_truncated_at.is_some());

    // The cell keeps writing over the remnant region through the
    // recovered rotor (a smaller frame than the remnant), under the
    // recovery-derived life — the assembly wiring ADR-0031 D6 requires.
    let mut ring = StagingRing::new(config.staging);
    ring.set_frame_epoch(rotor.resume_epoch());
    let at = ring
        .stage(&MutationEffect::StringSet { ns: NS, key: b"post", value: b"torn" })
        .expect("stage");
    rotor.maintain(0).expect("maintain");
    let lease = ring.flush_into(&mut rotor, 0).expect("flush").expect("frame");
    let post_lsn = lease.lsn_of(at);
    ring.release(lease);
    assert_eq!(post_lsn.offset - FRAME_HEADER_LEN as u32, torn_base.offset);
    drop(rotor);

    // Second boot: the old remnant beyond the new frame classifies as a
    // torn tail again (never as data, never fail-stop), and every
    // surviving record is intact.
    let mut ks2 = fresh_keyspace();
    let (_rotor, stats2) = recover(&fs, &mut ks2).expect("remnants stay recoverable");
    assert_eq!(get(&mut ks2, b"a").as_deref(), Some(&b"1"[..]));
    assert_eq!(get(&mut ks2, b"post").as_deref(), Some(&b"torn"[..]));
    assert_eq!(get(&mut ks2, b"fat"), None);
    assert!(stats2.torn_truncated_at.is_some(), "remnant garbage classifies torn");
}

#[test]
fn interior_corruption_refuses_to_start_naming_segment_and_offset() {
    let fs = MemFs::new();
    let config = cfg();
    let mut log = LogBuilder::new(&fs, &config);
    let (first_base, _) = log.set_frame(b"a", b"1");
    log.set_frame(b"b", b"2");
    log.set_frame(b"c", b"3");
    // Corrupt the FIRST frame: valid frames follow it.
    log.poke(first_base.segment, first_base.offset + FRAME_HEADER_LEN as u32 + 1, &[0xAA]);

    let mut ks = fresh_keyspace();
    let err = recover(&fs, &mut ks).expect_err("interior corruption is fail-stop");
    let msg = err.to_string();
    assert!(msg.contains("seg-000000"), "names the segment: {msg}");
    assert!(msg.contains("refusing to start"), "explicit refusal: {msg}");
    assert!(msg.contains("validating frame follows"), "names the evidence: {msg}");
}

/// Twenty one-record frames across several 256-byte segments through the
/// synchronous tier (frames stamp `covered_lsn = 0` — they attest
/// nothing), then corrupt the `nth` frame of sealed segment 1. Returns
/// the corrupted frame's base and every frame base in order.
fn sealed_volume_with_a_corrupt_frame(
    fs: &MemFs,
    config: &DurableConfig,
    nth: usize,
) -> (Lsn, Vec<Lsn>) {
    let dirs = create_cell_dirs(fs, Path::new("data/shard-0")).expect("dirs");
    let mut rotor =
        SegmentRotor::create_fresh(fs.clone(), dirs.log.clone(), config.segment).expect("rotor");
    let mut ring = StagingRing::new(config.staging);
    let mut frame_bases: Vec<Lsn> = Vec::new();
    for i in 0..20u32 {
        let key = format!("k:{i}");
        let at = ring
            .stage(&MutationEffect::StringSet { ns: NS, key: key.as_bytes(), value: b"vvvvvvvv" })
            .expect("stage");
        rotor.maintain(0).expect("maintain");
        let lease = ring.flush_into(&mut rotor, 0).expect("flush").expect("frame");
        let lsn = lease.lsn_of(at);
        frame_bases.push(Lsn::new(lsn.segment, lsn.offset - FRAME_HEADER_LEN as u32));
        ring.release(lease);
    }
    assert!(rotor.active_segment().0 >= 2, "the volume must have rotated");
    drop(rotor);
    let victim =
        *frame_bases.iter().filter(|lsn| lsn.segment.0 == 1).nth(nth).expect("segment 1 frame");
    let mut file = fs.open_write(&dirs.log.join(segment_file_name(victim.segment))).expect("open");
    file.write_at(u64::from(victim.offset + FRAME_HEADER_LEN as u32), &[0x55]).expect("poke");
    (victim, frame_bases)
}

#[test]
fn unattested_hole_in_a_sealed_segment_truncates_and_discards_later_segments() {
    // ADR-0087 D6 (amends ADR-0031 D4): a validating frame beyond a hole
    // in a *sealed* segment is judged by the same stamp evidence as one
    // in the resume region. A hole exists only if the barrier covering
    // it never completed, so nothing at or past it was ever acked; with
    // no later frame attesting coverage past the hole, boot truncates
    // there, discards every later segment unreplayed, and resumes — the
    // availability refusal ADR-0086 recorded is gone.
    let fs = MemFs::new();
    let mut config = cfg();
    config.segment.segment_bytes = 256;
    let (victim, frame_bases) = sealed_volume_with_a_corrupt_frame(&fs, &config, 1);
    assert!(victim.offset > 0, "a mid-segment hole: the segment keeps its own prefix");

    let mut ks = fresh_keyspace();
    let (rotor, stats, _) = open_cell_log(fs.clone(), &mut ks, CELL, &config, anchor(), now())
        .expect("an un-attested hole in sealed slack truncates");
    let hole_index = frame_bases.iter().position(|b| *b == victim).expect("victim indexed");
    for (i, _) in frame_bases.iter().enumerate() {
        let key = format!("k:{i}");
        let present = get(&mut ks, key.as_bytes()).is_some();
        assert_eq!(present, i < hole_index, "record {i}: prefix replays, hole and after do not");
    }
    assert_eq!(stats.torn_truncated_at, Some(victim), "the tail pointer stops at the hole");
    assert!(stats.beyond_frames_discarded >= 1, "later frames counted, never silent");
    assert!(stats.torn_segments_removed >= 1, "every later segment removed");
    assert_eq!(rotor.active_segment(), victim.segment, "appends resume in the hole's segment");
    assert_eq!(rotor.append_cursor(), victim, "at the hole");

    // Second boot reaches the same verdict on the same bytes (the tail
    // pointer moved; the remnant frames beyond it are never rewritten —
    // the `resume_over_remnants` rule) and the same prefix.
    let mut ks2 = fresh_keyspace();
    let (_, stats2, _) =
        open_cell_log(fs.clone(), &mut ks2, CELL, &config, anchor(), now()).expect("idempotent");
    assert_eq!(stats2.torn_truncated_at, Some(victim));
    // Only the hole segment's own remnants remain to be discarded.
    let remnants_in_hole_segment = frame_bases
        .iter()
        .filter(|b| b.segment == victim.segment && b.offset > victim.offset)
        .count() as u64;
    assert_eq!(stats2.beyond_frames_discarded, remnants_in_hole_segment);
    assert!(
        stats.beyond_frames_discarded > remnants_in_hole_segment,
        "first boot saw later segments"
    );
    assert_eq!(stats2.torn_segments_removed, 0, "later segments are already gone");
    assert_eq!(get(&mut ks2, b"k:0").as_deref(), Some(&b"vvvvvvvv"[..]));
}

#[test]
fn unattested_hole_at_a_sealed_segments_first_frame_resumes_in_the_previous_segment() {
    // ADR-0087 D6: a hole at offset 0 keeps nothing of its own — the
    // previous data-bearing segment's end is the resume point and the
    // hole's segment is removed like any trailing one (the pristine
    // prealloc invariant, as for a torn tail).
    let fs = MemFs::new();
    let mut config = cfg();
    config.segment.segment_bytes = 256;
    let (victim, frame_bases) = sealed_volume_with_a_corrupt_frame(&fs, &config, 0);
    assert_eq!(victim.offset, 0);
    let previous_end = data_end_of(
        &fs,
        &config,
        frame_bases[..].iter().rev().find(|b| b.segment.0 == 0).expect("segment 0 frame"),
    );

    let mut ks = fresh_keyspace();
    let (rotor, stats, _) = open_cell_log(fs.clone(), &mut ks, CELL, &config, anchor(), now())
        .expect("truncates into the previous segment");
    assert_eq!(stats.torn_truncated_at, Some(previous_end));
    assert_eq!(rotor.active_segment(), SegmentId(0));
    assert_eq!(rotor.append_cursor(), previous_end);
    let hole_index = frame_bases.iter().position(|b| *b == victim).expect("victim indexed");
    for i in 0..frame_bases.len() {
        let key = format!("k:{i}");
        assert_eq!(get(&mut ks, key.as_bytes()).is_some(), i < hole_index, "record {i}");
    }
}

/// Exclusive end of the frame at `base` (its length field).
fn data_end_of(fs: &MemFs, config: &DurableConfig, base: &Lsn) -> Lsn {
    let path = config
        .data_dir
        .join(format!("shard-{CELL}"))
        .join("log")
        .join(segment_file_name(base.segment));
    let seg = fs.contents(&path).expect("segment bytes");
    let at = base.offset as usize;
    let frame_len = u32::from_le_bytes(seg[at + 4..at + 8].try_into().unwrap());
    Lsn::new(base.segment, base.offset + frame_len)
}

#[test]
fn attested_hole_in_a_sealed_segment_refuses_to_start() {
    // The other verdict of ADR-0087 D6: frames sealed by a live plane
    // stamp the watermark (`LogBuilder` attests its own base), so a frame
    // in a later segment attests coverage past the corrupted one — the
    // device lost covered data, and boot refuses naming segment 1.
    let fs = MemFs::new();
    let mut config = cfg();
    config.segment.segment_bytes = 256;
    let mut log = LogBuilder::new(&fs, &config);
    let mut bases = Vec::new();
    for i in 0..20u32 {
        let key = format!("k:{i}");
        let (base, _) = log.set_frame(key.as_bytes(), b"vvvvvvvv");
        bases.push(base);
    }
    assert!(log.rotor.active_segment().0 >= 2, "the volume must have rotated");
    let victim = *bases.iter().find(|b| b.segment.0 == 1).expect("segment 1 has a frame");
    log.poke(victim.segment, victim.offset + FRAME_HEADER_LEN as u32, &[0x55]);

    let mut ks = fresh_keyspace();
    let err = open_cell_log(fs.clone(), &mut ks, CELL, &config, anchor(), now())
        .map(|_| ())
        .expect_err("an attested hole in a sealed segment is fail-stop");
    let msg = err.to_string();
    assert!(msg.contains("seg-000001"), "names the segment: {msg}");
    assert!(msg.contains("attests fsync coverage"), "names the attestation evidence: {msg}");
}

#[test]
fn torn_tail_removes_the_trailing_preallocated_segment() {
    let fs = MemFs::new();
    let config = cfg();
    let mut log = LogBuilder::new(&fs, &config);
    log.set_frame(b"a", b"1");
    let (torn_base, _) = log.set_frame(b"b", b"2");
    // The rotor preallocated the next segment in MAINTAIN.
    log.rotor.maintain(0).expect("maintain");
    let next = log.rotor.next_ready().expect("next preallocated");
    log.poke(torn_base.segment, torn_base.offset + FRAME_HEADER_LEN as u32, &[0x77, 0x77]);
    let next_path = log.seg_path(next);
    assert!(fs.contents(&next_path).is_some(), "prealloc'd next exists on disk");

    let mut ks = fresh_keyspace();
    let (rotor, stats) = recover(&fs, &mut ks).expect("torn tail recovers");
    assert_eq!(stats.torn_segments_removed, 1, "the empty trailing segment is gone");
    assert!(fs.contents(&next_path).is_none(), "removed from disk");
    assert_eq!(rotor.active_segment(), torn_base.segment, "resume in the data segment");
    assert_eq!(get(&mut ks, b"a").as_deref(), Some(&b"1"[..]));
    assert_eq!(get(&mut ks, b"b"), None);
}

/// Byte after the last frame of segment 0, recomputed from a record LSN.
fn data_end(fs: &MemFs, log: &LogBuilder, lsn: Lsn) -> u32 {
    let base = lsn.offset - FRAME_HEADER_LEN as u32;
    let seg = fs.contents(&log.seg_path(lsn.segment)).expect("segment bytes");
    let frame_len =
        u32::from_le_bytes(seg[base as usize + 4..base as usize + 8].try_into().unwrap());
    base + frame_len
}

#[test]
fn attesting_survivor_after_a_zero_gap_refuses_to_start() {
    // ADR-0031 D4: a surviving frame whose stamp attests fsync coverage
    // past the data end proves the gap sat in covered territory — the
    // disk lost covered data, and boot must refuse with that evidence.
    let fs = MemFs::new();
    let config = cfg();
    let mut log = LogBuilder::new(&fs, &config);
    let (_, lsns) = log.set_frame(b"a", b"1");
    let end = data_end(&fs, &log, lsns[0]);
    // A validating frame written for exactly `end + 512`, past a zero
    // gap, attesting the gap region was covered when it was sealed.
    let survivor_at = end + 512;
    let mut b = FrameBuilder::new();
    b.append(&RecordView::StringPostImage { ns: NS, key: b"ghost", value: b"stale" });
    let stamp =
        FrameStamp { epoch: 1, seq: 9, covered_lsn: Lsn::new(SegmentId(0), survivor_at).to_u64() };
    let bytes = b
        .finalize(
            Lsn::new(SegmentId(0), survivor_at + FRAME_HEADER_LEN as u32),
            stamp,
            FrameLayout::Packed,
        )
        .to_vec();
    log.poke(SegmentId(0), survivor_at, &bytes);

    let mut ks = fresh_keyspace();
    let err = recover(&fs, &mut ks).expect_err("an attesting survivor beyond a gap is fail-stop");
    let msg = err.to_string();
    assert!(msg.contains("validating frame follows"), "{msg}");
    assert!(msg.contains("attests fsync coverage"), "names the attestation evidence: {msg}");
}

#[test]
fn unattested_survivor_after_a_zero_gap_truncates_and_is_counted() {
    // The retired ADR-0021 D3 refusal (M2.5-S12): the same shape with no
    // surviving attestation is exactly what a reorder hole in the
    // un-covered suffix leaves behind — nothing acked is lost, so boot
    // truncates at the data end, counts the discarded survivor, and the
    // resumed life's epoch tops the survivor's (it can never re-enter a
    // replay prefix).
    let fs = MemFs::new();
    let config = cfg();
    let mut log = LogBuilder::new(&fs, &config);
    let (_, lsns) = log.set_frame(b"a", b"1");
    let end = data_end(&fs, &log, lsns[0]);
    let survivor_at = end + 512;
    let mut b = FrameBuilder::new();
    b.append(&RecordView::StringPostImage { ns: NS, key: b"ghost", value: b"stale" });
    let stamp = FrameStamp { epoch: 3, seq: 9, covered_lsn: 0 };
    let bytes = b
        .finalize(
            Lsn::new(SegmentId(0), survivor_at + FRAME_HEADER_LEN as u32),
            stamp,
            FrameLayout::Packed,
        )
        .to_vec();
    log.poke(SegmentId(0), survivor_at, &bytes);

    let mut ks = fresh_keyspace();
    let (rotor, stats) = recover(&fs, &mut ks).expect("an un-attested survivor truncates");
    assert_eq!(get(&mut ks, b"a").as_deref(), Some(&b"1"[..]), "prefix data intact");
    assert_eq!(get(&mut ks, b"ghost"), None, "the survivor never replays");
    assert_eq!(stats.beyond_frames_discarded, 1, "counted, never silent");
    assert_eq!(stats.torn_truncated_at, Some(Lsn::new(SegmentId(0), end)));
    assert!(rotor.resume_epoch() > 3, "the resumed life tops every observed epoch");
}

#[test]
fn v1_survivor_after_a_zero_gap_still_refuses_to_start() {
    // A format-v1 frame beyond the data end attests nothing — the
    // conservative pre-ADR-0031 rule holds for mixed-era logs.
    let fs = MemFs::new();
    let config = cfg();
    let mut log = LogBuilder::new(&fs, &config);
    let (_, lsns) = log.set_frame(b"a", b"1");
    let end = data_end(&fs, &log, lsns[0]);
    let survivor_at = end + 512;
    let bytes = v1_frame_at(SegmentId(0), survivor_at, b"ghost", b"stale");
    log.poke(SegmentId(0), survivor_at, &bytes);

    let mut ks = fresh_keyspace();
    let err = recover(&fs, &mut ks).expect_err("a v1 survivor beyond a gap is fail-stop");
    let msg = err.to_string();
    assert!(msg.contains("validating frame follows"), "{msg}");
    assert!(msg.contains("cannot attest"), "names the v1 limitation: {msg}");
}

/// Hand-built format-v1 frame (magic `IFR1`, 20-byte header, no stamp) for
/// exactly (`segment`, `offset`) — the legacy writer no code path emits
/// anymore (ADR-0031 D2).
fn v1_frame_at(segment: SegmentId, offset: u32, key: &[u8], value: &[u8]) -> Vec<u8> {
    let record = RecordView::StringPostImage { ns: NS, key, value };
    let mut body = Vec::new();
    record.encode_into(&mut body);
    let frame_len = u32::try_from(20 + body.len() + 4).expect("fits u32");
    let mut frame = Vec::with_capacity(frame_len as usize);
    frame.extend_from_slice(b"IFR1");
    frame.extend_from_slice(&frame_len.to_le_bytes());
    frame.extend_from_slice(&1u32.to_le_bytes());
    frame.extend_from_slice(&segment.0.to_le_bytes());
    frame.extend_from_slice(&(offset + 20).to_le_bytes());
    frame.extend_from_slice(&body);
    let crc = inf_simd::crc32c(&frame);
    frame.extend_from_slice(&crc.to_le_bytes());
    frame
}

#[test]
fn garbage_in_sealed_slack_is_tolerated_and_counted() {
    // The legitimate end-state of an earlier torn-tail resume: the
    // truncated segment sealed with remnant garbage beyond its data end.
    // It must never fail-stop (that would poison every boot after a
    // benign torn tail) — but it is counted, never silent.
    let fs = MemFs::new();
    let mut config = cfg();
    config.segment.segment_bytes = 256;
    let dirs = create_cell_dirs(&fs, Path::new("data/shard-0")).expect("dirs");
    let mut rotor =
        SegmentRotor::create_fresh(fs.clone(), dirs.log.clone(), config.segment).expect("rotor");
    let mut ring = StagingRing::new(config.staging);
    let mut seg0_end = 0u32;
    for i in 0..12u32 {
        let key = format!("k:{i}");
        let at = ring
            .stage(&MutationEffect::StringSet { ns: NS, key: key.as_bytes(), value: b"vvvvvvvv" })
            .expect("stage");
        rotor.maintain(0).expect("maintain");
        let lease = ring.flush_into(&mut rotor, 0).expect("flush").expect("frame");
        let lsn = lease.lsn_of(at);
        if lsn.segment == SegmentId(0) {
            seg0_end = rotor.active_written();
        }
        ring.release(lease);
    }
    assert!(rotor.active_segment().0 >= 1, "must have rotated");
    drop(rotor);
    // Non-validating remnants in sealed seg 0's slack.
    let mut file = fs.open_write(&dirs.log.join(segment_file_name(SegmentId(0)))).expect("open");
    file.write_at(u64::from(seg0_end) + 3, b"\xBA\xDB\xAD").expect("poke");

    let mut ks = fresh_keyspace();
    let (_rotor, stats, _seed) = open_cell_log(fs.clone(), &mut ks, CELL, &config, anchor(), now())
        .expect("sealed-slack remnants must not fail the boot");
    assert_eq!(stats.sealed_slack_remnants, 1, "remnants counted, never silent");
    assert_eq!(stats.torn_truncated_at, None, "the tail itself is clean");
    assert_eq!(get(&mut ks, b"k:11").as_deref(), Some(&b"vvvvvvvv"[..]));
}

#[test]
fn torn_tail_below_the_manifest_begin_lsn_refuses_to_start() {
    let fs = MemFs::new();
    let config = cfg();
    let mut log = LogBuilder::new(&fs, &config);
    log.set_frame(b"a", b"1");
    let (_, begin_lsns) = log.frame(&[
        MutationEffect::CkptBegin { ckpt_id: 1 },
        MutationEffect::StringSet { ns: NS, key: b"b", value: b"2" },
    ]);
    let begin = begin_lsns[0];
    let torn_base = Lsn::new(begin.segment, begin.offset - FRAME_HEADER_LEN as u32);

    // Publish ckpt 1 at `begin` (state: {a=1}), then tear the frame that
    // *contains* the begin marker — fsync-covered bytes at publication.
    let ckpt_dir = Path::new("data/shard-0/ckpt");
    let mut w = SyncIckWriter::create(fs.clone(), ckpt_dir, &config.ckpt, CELL, 1, begin, &[NS.0])
        .expect("ick create");
    w.append(&RecordView::StringPostImage { ns: NS, key: b"a", value: b"1" }).expect("ick record");
    w.finish().expect("ick publish");
    write_manifest(
        &fs,
        Path::new("data/shard-0"),
        &Manifest {
            ckpt_id: 1,
            begin_lsn: begin,
            segments: vec![begin.segment],
            tiers: Vec::new(),
        },
    )
    .expect("manifest");
    log.poke(torn_base.segment, torn_base.offset + FRAME_HEADER_LEN as u32, &[0x99]);

    let mut ks = fresh_keyspace();
    let err = recover(&fs, &mut ks).expect_err("truncation below begin is lost covered state");
    assert!(err.to_string().contains("below the MANIFEST begin-LSN"), "{}", err.to_string());
}

#[test]
fn torn_tail_above_begin_recovers_checkpoint_plus_tail() {
    let fs = MemFs::new();
    let config = cfg();
    let mut log = LogBuilder::new(&fs, &config);
    log.set_frame(b"a", b"old-a");
    let (_, begin_lsns) = log.frame(&[
        MutationEffect::CkptBegin { ckpt_id: 1 },
        MutationEffect::StringSet { ns: NS, key: b"b", value: b"2" },
    ]);
    let begin = begin_lsns[0];
    let (torn_base, _) = log.set_frame(b"c", b"3");

    let ckpt_dir = Path::new("data/shard-0/ckpt");
    let mut w = SyncIckWriter::create(fs.clone(), ckpt_dir, &config.ckpt, CELL, 1, begin, &[NS.0])
        .expect("ick create");
    // Checkpoint state at begin: a's final image.
    w.append(&RecordView::StringPostImage { ns: NS, key: b"a", value: b"ckpt-a" })
        .expect("ick record");
    w.finish().expect("ick publish");
    write_manifest(
        &fs,
        Path::new("data/shard-0"),
        &Manifest {
            ckpt_id: 1,
            begin_lsn: begin,
            segments: vec![begin.segment],
            tiers: Vec::new(),
        },
    )
    .expect("manifest");
    // Tear the last frame — strictly above begin.
    log.poke(torn_base.segment, torn_base.offset + FRAME_HEADER_LEN as u32, &[0x99]);

    let mut ks = fresh_keyspace();
    let (_rotor, stats) = recover(&fs, &mut ks).expect("torn tail above begin recovers");
    assert_eq!(stats.torn_truncated_at, Some(torn_base));
    assert_eq!(get(&mut ks, b"a").as_deref(), Some(&b"ckpt-a"[..]), "checkpoint image wins");
    assert_eq!(get(&mut ks, b"b").as_deref(), Some(&b"2"[..]), "tail from begin replayed");
    assert_eq!(get(&mut ks, b"c"), None, "torn frame dropped");
    assert!(stats.records_pre_begin > 0, "pre-begin floor records skipped");
}
