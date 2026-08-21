//! M2-S04 ACs: the sequential reader yields byte-identical record
//! sequences to what the writer logged — across segment rotations, frame
//! boundaries, and the active tail — and ends with facts, not policy:
//! `ZeroTail`/`FileEnd` for clean ends, typed errors (torn bytes, CRC
//! mismatch, bad magic, misdirected LSN) for everything else (S14 builds
//! the recovery policy on those facts).

use std::path::{Path, PathBuf};

use inf_log::FrameLayout;
use inf_log::fs::mem::MemFs;
use inf_log::fs::{SegmentFile, SegmentFs};
use inf_log::{
    FRAME_HEADER_LEN, FrameBuilder, FrameDecodeError, FrameStamp, Lsn, MutationEffect, NsId,
    ReadEnd, ReadError, ReaderConfig, RecordView, SegmentConfig, SegmentId, SegmentReader,
    SegmentRotor, StagingConfig, StagingRing, create_cell_dirs, scan_log_dir, segment_file_name,
};
use proptest::prelude::*;

/// Canonical v2 stamp for hand-built test frames (epoch 1, covered 0 —
/// attests nothing). `seq` matters only where a test builds sequential
/// frames the recovery policy will walk; readers/scanners ignore it.
fn stamp(seq: u64) -> FrameStamp {
    FrameStamp { epoch: 1, seq, covered_lsn: 0 }
}

fn mem_rotor(fs: &MemFs, segment_bytes: u32) -> (SegmentRotor<MemFs>, PathBuf) {
    let dirs = create_cell_dirs(fs, &PathBuf::from("data/shard-0")).expect("dirs");
    let cfg = SegmentConfig { segment_bytes, ..Default::default() };
    let rotor = SegmentRotor::create_fresh(fs.clone(), dirs.log.clone(), cfg).expect("rotor");
    (rotor, dirs.log)
}

/// Replay every segment `0..=last`, returning `(lsn, encoded record)` in
/// log order plus the tail segment's end.
fn replay_all(
    fs: &MemFs,
    log_dir: &Path,
    last: u32,
    cfg: ReaderConfig,
) -> (Vec<(Lsn, Vec<u8>)>, ReadEnd) {
    let mut replayed = Vec::new();
    let mut tail_end = None;
    for id in 0..=last {
        let mut reader =
            SegmentReader::open(fs, log_dir, SegmentId(id), cfg).expect("open segment");
        let end = reader
            .apply_frames(|frame| {
                for record in frame.records() {
                    let (lsn, view) = record.expect("valid record");
                    let mut bytes = Vec::new();
                    view.encode_into(&mut bytes);
                    replayed.push((lsn, bytes));
                }
                Ok::<(), std::convert::Infallible>(())
            })
            .expect("replay");
        tail_end = Some(end);
    }
    (replayed, tail_end.expect("at least one segment"))
}

/// Deterministic xorshift64* (L7: no ambient randomness).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }
}

/// The AC volume test: 10⁶ random-length writes through the staging ring
/// and rotor, replayed byte-identically across every rotated segment.
/// Deterministic seed — a failure replays exactly (L7).
#[test]
fn million_random_writes_replay_byte_identically() {
    let fs = MemFs::new();
    let (mut rotor, log_dir) = mem_rotor(&fs, 1 << 20);
    let mut ring = StagingRing::new(StagingConfig::with_capacity(64 << 10));
    let mut rng = Rng(0x1AF1_D8A5_0DB5_EED1);

    const TOTAL: usize = 1_000_000;
    // Expected byte stream + per-record LSNs, kept as one contiguous
    // buffer (10⁶ tiny Vecs would dominate the test's own memory).
    let mut expected_bytes = Vec::with_capacity(TOTAL * 48);
    let mut expected_lsns = Vec::with_capacity(TOTAL);
    let mut pending: Vec<inf_log::StagedAt> = Vec::new();
    let mut key = [0u8; 16];
    let mut value = [0u8; 64];

    let mut written = 0usize;
    while written < TOTAL {
        let burst = 1 + rng.below(64) as usize;
        for _ in 0..burst.min(TOTAL - written) {
            let key_len = 1 + rng.below(16) as usize;
            let val_len = rng.below(64) as usize;
            key[..key_len].fill(b'a' + (rng.next() % 26) as u8);
            value[..val_len].fill((rng.next() & 0xFF) as u8);
            let effect = match rng.below(4) {
                0 | 1 => MutationEffect::StringSet {
                    ns: NsId(rng.below(64) as u32),
                    key: &key[..key_len],
                    value: &value[..val_len],
                },
                2 => MutationEffect::Delete { ns: NsId(7), key: &key[..key_len] },
                _ => MutationEffect::ExpireAt {
                    ns: NsId(3),
                    at_unix_ms: rng.next(),
                    key: &key[..key_len],
                },
            };
            let at = ring.stage(&effect).expect("burst sized within capacity");
            effect.record().encode_into(&mut expected_bytes);
            pending.push(at);
            written += 1;
        }
        rotor.maintain(0).expect("maintain");
        let lease = ring.flush_into(&mut rotor, 0).expect("flush").expect("frame");
        for at in pending.drain(..) {
            expected_lsns.push(lease.lsn_of(at));
        }
        ring.release(lease);
    }
    assert!(rotor.stats().rotations > 10, "the volume must cross many segments");

    // Replay and compare against the expected stream, record by record.
    let mut cursor = 0usize;
    let mut count = 0usize;
    let mut scratch = Vec::with_capacity(128);
    let last = rotor.active_segment().0;
    for id in 0..=last {
        let mut reader = SegmentReader::open(&fs, &log_dir, SegmentId(id), ReaderConfig::default())
            .expect("open segment");
        let end = reader
            .apply_frames(|frame| {
                for record in frame.records() {
                    let (lsn, view) = record.expect("valid record");
                    assert_eq!(lsn, expected_lsns[count], "record {count} LSN");
                    scratch.clear();
                    view.encode_into(&mut scratch);
                    assert_eq!(
                        &expected_bytes[cursor..cursor + scratch.len()],
                        &scratch[..],
                        "record {count} bytes"
                    );
                    cursor += scratch.len();
                    count += 1;
                }
                Ok::<(), std::convert::Infallible>(())
            })
            .expect("replay");
        if id == last {
            assert_eq!(end.at(), rotor.active_written());
        }
    }
    assert_eq!(count, TOTAL, "zero loss across {TOTAL} writes");
    assert_eq!(cursor, expected_bytes.len(), "byte stream fully consumed");
}

proptest! {
    /// Structural round-trip: random records, random per-iteration
    /// grouping, random (small) segment and reader-chunk sizes — the
    /// reader must reproduce the exact write sequence, whatever the
    /// frame/segment/window alignment.
    #[test]
    fn random_grouping_and_geometry_round_trip(
        records in prop::collection::vec(
            (prop::collection::vec(any::<u8>(), 1..24),
             prop::collection::vec(any::<u8>(), 0..80)),
            1..120,
        ),
        group in 1usize..16,
        segment_bytes in 512u32..8192,
        chunk in 64usize..2048,
    ) {
        let fs = MemFs::new();
        let (mut rotor, log_dir) = mem_rotor(&fs, segment_bytes.max(1024));
        let mut ring = StagingRing::new(StagingConfig::with_capacity(512));

        let mut expected: Vec<(Lsn, Vec<u8>)> = Vec::new();
        let mut pending = Vec::new();
        let mut staged = 0usize;
        for (key, value) in &records {
            let effect = MutationEffect::StringSet { ns: NsId(1), key, value };
            let mut bytes = Vec::new();
            effect.record().encode_into(&mut bytes);
            if !ring.would_fit(bytes.len()) {
                // Group boundary forced by capacity: drain first.
                rotor.maintain(0).expect("maintain");
                if let Some(lease) = ring.flush_into(&mut rotor, 0).expect("flush") {
                    for (at, b) in pending.drain(..) {
                        expected.push((lease.lsn_of(at), b));
                    }
                    ring.release(lease);
                }
                staged = 0;
            }
            let at = ring.stage(&effect).expect("fits after drain");
            pending.push((at, bytes));
            staged += 1;
            if staged == group {
                rotor.maintain(0).expect("maintain");
                if let Some(lease) = ring.flush_into(&mut rotor, 0).expect("flush") {
                    for (at, b) in pending.drain(..) {
                        expected.push((lease.lsn_of(at), b));
                    }
                    ring.release(lease);
                }
                staged = 0;
            }
        }
        rotor.maintain(0).expect("maintain");
        if let Some(lease) = ring.flush_into(&mut rotor, 0).expect("flush") {
            for (at, b) in pending.drain(..) {
                expected.push((lease.lsn_of(at), b));
            }
            ring.release(lease);
        }

        let cfg = ReaderConfig { chunk_bytes: chunk, ..ReaderConfig::default() };
        let (replayed, end) = replay_all(&fs, &log_dir, rotor.active_segment().0, cfg);
        prop_assert_eq!(&replayed, &expected);
        prop_assert_eq!(end.at(), rotor.active_written());
    }
}

/// Write one frame directly (no staging) at the rotor's cursor.
fn write_frame(rotor: &mut SegmentRotor<MemFs>, records: &[RecordView<'_>]) -> Lsn {
    let mut builder = FrameBuilder::new();
    for record in records {
        builder.append(record);
    }
    let slot = rotor.begin_frame(builder.frame_len(), 0).expect("reserve");
    let first = slot.first_record_lsn();
    let bytes = builder.finalize(first, stamp(1), FrameLayout::Packed);
    rotor.commit_frame(slot, bytes).expect("commit")
}

#[test]
fn tail_ends_are_facts_zero_tail_and_file_end() {
    // ZeroTail: an active segment with preallocated space left.
    let fs = MemFs::new();
    let (mut rotor, log_dir) = mem_rotor(&fs, 4096);
    write_frame(&mut rotor, &[RecordView::Delete { ns: NsId(1), key: b"a" }]);
    let mut reader =
        SegmentReader::open(&fs, &log_dir, SegmentId(0), ReaderConfig::default()).expect("open");
    let end = reader.apply_frames(|_| Ok::<(), std::convert::Infallible>(())).expect("replay");
    assert_eq!(end, ReadEnd::ZeroTail { at: rotor.active_written() });

    // FileEnd: a file that stops exactly on a frame boundary.
    let full = fs.contents(&log_dir.join(segment_file_name(SegmentId(0)))).expect("image");
    let exact_dir = PathBuf::from("exact");
    fs.create_dir_all(&exact_dir).expect("dir");
    let path = exact_dir.join(segment_file_name(SegmentId(0)));
    let written = rotor.active_written() as usize;
    let mut file = fs.create_segment(&path, written as u64).expect("create");
    file.write_at(0, &full[..written]).expect("write");
    let mut reader =
        SegmentReader::open(&fs, &exact_dir, SegmentId(0), ReaderConfig::default()).expect("open");
    let end = reader.apply_frames(|_| Ok::<(), std::convert::Infallible>(())).expect("replay");
    assert_eq!(end, ReadEnd::FileEnd { at: rotor.active_written() });
}

#[test]
fn torn_tail_surfaces_as_typed_truncation_facts() {
    let fs = MemFs::new();
    let (mut rotor, log_dir) = mem_rotor(&fs, 4096);
    let first = write_frame(&mut rotor, &[RecordView::Delete { ns: NsId(1), key: b"aaaa" }]);
    let torn_at = rotor.active_written();
    write_frame(&mut rotor, &[RecordView::Delete { ns: NsId(1), key: b"bbbb" }]);
    let full = fs.contents(&log_dir.join(segment_file_name(SegmentId(0)))).expect("image");

    // Power-cut shape 1: the file itself ends mid-frame (short file).
    let cut = torn_at as usize + 7;
    let dir = PathBuf::from("short");
    fs.create_dir_all(&dir).expect("dir");
    let mut file =
        fs.create_segment(&dir.join(segment_file_name(SegmentId(0))), cut as u64).expect("create");
    file.write_at(0, &full[..cut]).expect("write");
    let mut reader =
        SegmentReader::open(&fs, &dir, SegmentId(0), ReaderConfig::default()).expect("open");
    let good = reader.next_frame().expect("first frame intact").expect("frame");
    assert_eq!(good.first_lsn(), first.advance(FRAME_HEADER_LEN as u32));
    let err = reader.next_frame().expect_err("torn frame must not decode");
    match err {
        ReadError::Frame { offset, error: FrameDecodeError::Truncated { .. }, .. } => {
            assert_eq!(offset, torn_at);
        }
        other => panic!("expected Truncated, got {other}"),
    }

    // Power-cut shape 2: preallocated file, write torn mid-frame (partial
    // bytes then zeros) — CRC catches it at the same offset.
    let dir = PathBuf::from("padded");
    fs.create_dir_all(&dir).expect("dir");
    let mut file =
        fs.create_segment(&dir.join(segment_file_name(SegmentId(0))), 4096).expect("create");
    file.write_at(0, &full[..cut]).expect("write");
    let mut reader =
        SegmentReader::open(&fs, &dir, SegmentId(0), ReaderConfig::default()).expect("open");
    reader.next_frame().expect("first frame intact").expect("frame");
    let err = reader.next_frame().expect_err("torn frame must not decode");
    match err {
        ReadError::Frame { offset, error: FrameDecodeError::CrcMismatch { .. }, .. } => {
            assert_eq!(offset, torn_at);
        }
        other => panic!("expected CrcMismatch, got {other}"),
    }
}

#[test]
fn interior_corruption_is_a_typed_error_never_a_skip() {
    let fs = MemFs::new();
    let (mut rotor, log_dir) = mem_rotor(&fs, 8192);
    write_frame(&mut rotor, &[RecordView::Delete { ns: NsId(1), key: b"first" }]);
    let corrupt_at = rotor.active_written();
    write_frame(&mut rotor, &[RecordView::Delete { ns: NsId(1), key: b"second" }]);
    write_frame(&mut rotor, &[RecordView::Delete { ns: NsId(1), key: b"third" }]);

    let path = log_dir.join(segment_file_name(SegmentId(0)));
    let image = fs.contents(&path).expect("image");
    // Flip one byte in the middle frame's body.
    let mut file = fs.open_write(&path).expect("open");
    let victim = corrupt_at as usize + FRAME_HEADER_LEN + 2;
    file.write_at(victim as u64, &[image[victim] ^ 0x40]).expect("corrupt");

    let mut reader =
        SegmentReader::open(&fs, &log_dir, SegmentId(0), ReaderConfig::default()).expect("open");
    reader.next_frame().expect("first frame valid").expect("frame");
    let err = reader.next_frame().expect_err("corruption must surface");
    match err {
        ReadError::Frame { offset, error: FrameDecodeError::CrcMismatch { .. }, .. } => {
            assert_eq!(offset, corrupt_at, "error names the exact frame offset");
        }
        other => panic!("expected CrcMismatch, got {other}"),
    }
    // Terminal: the reader never resumes past an error into the valid
    // third frame (skipping interior corruption is S14's forbidden move).
    assert!(reader.next_frame().expect("fused").is_none());
    assert_eq!(reader.read_end(), None, "no clean end was reached");
}

#[test]
fn foreign_bytes_at_a_frame_boundary_are_bad_magic() {
    let fs = MemFs::new();
    let (mut rotor, log_dir) = mem_rotor(&fs, 4096);
    write_frame(&mut rotor, &[RecordView::Delete { ns: NsId(1), key: b"ok" }]);
    let garbage_at = rotor.active_written();
    let path = log_dir.join(segment_file_name(SegmentId(0)));
    let mut file = fs.open_write(&path).expect("open");
    file.write_at(u64::from(garbage_at), b"JUNK").expect("write");

    let mut reader =
        SegmentReader::open(&fs, &log_dir, SegmentId(0), ReaderConfig::default()).expect("open");
    reader.next_frame().expect("first frame valid").expect("frame");
    let err = reader.next_frame().expect_err("garbage must surface");
    assert!(matches!(
        err,
        ReadError::Frame { offset, error: FrameDecodeError::BadMagic { .. }, .. }
        if offset == garbage_at
    ));
}

#[test]
fn misdirected_frame_is_an_lsn_mismatch() {
    let fs = MemFs::new();
    let (mut rotor, log_dir) = mem_rotor(&fs, 4096);
    // A frame whose header claims a position other than where it lands.
    let mut builder = FrameBuilder::new();
    builder.append(&RecordView::Delete { ns: NsId(1), key: b"x" });
    let slot = rotor.begin_frame(builder.frame_len(), 0).expect("reserve");
    let lied = Lsn::new(SegmentId(9), 0x400);
    let bytes = builder.finalize(lied, stamp(1), FrameLayout::Packed);
    rotor.commit_frame(slot, bytes).expect("commit");

    let mut reader =
        SegmentReader::open(&fs, &log_dir, SegmentId(0), ReaderConfig::default()).expect("open");
    let err = reader.next_frame().expect_err("misdirected write must surface");
    match err {
        ReadError::LsnMismatch { stored, expected, offset, .. } => {
            assert_eq!(stored, lied);
            assert_eq!(expected, Lsn::new(SegmentId(0), FRAME_HEADER_LEN as u32));
            assert_eq!(offset, 0);
        }
        other => panic!("expected LsnMismatch, got {other}"),
    }
}

/// `ReadEnd::at` is the recovered tail offset: reopening the rotor there
/// (the S02 protocol) appends exactly after the last valid frame — the
/// S04 → S13 hand-off.
#[test]
fn read_end_feeds_open_existing_round_trip() {
    let fs = MemFs::new();
    let (mut rotor, log_dir) = mem_rotor(&fs, 4096);
    write_frame(&mut rotor, &[RecordView::Delete { ns: NsId(1), key: b"pre" }]);
    let cfg = SegmentConfig { segment_bytes: 4096, ..Default::default() };
    drop(rotor);

    let scan = scan_log_dir(&fs, &log_dir).expect("scan");
    let mut reader =
        SegmentReader::open(&fs, &log_dir, scan.tail().expect("tail"), ReaderConfig::default())
            .expect("open");
    let end = reader.apply_frames(|_| Ok::<(), std::convert::Infallible>(())).expect("replay");

    let mut rotor = SegmentRotor::open_existing(fs.clone(), log_dir.clone(), cfg, &scan, end.at())
        .expect("reopen");
    write_frame(&mut rotor, &[RecordView::Delete { ns: NsId(1), key: b"post" }]);

    let (replayed, _) =
        replay_all(&fs, &log_dir, rotor.active_segment().0, ReaderConfig::default());
    let keys: Vec<Vec<u8>> = replayed
        .iter()
        .map(|(_, bytes)| {
            let (view, _) = inf_log::decode_record(bytes).expect("decode");
            match view {
                RecordView::Delete { key, .. } => key.to_vec(),
                other => panic!("unexpected record {other:?}"),
            }
        })
        .collect();
    assert_eq!(keys, vec![b"pre".to_vec(), b"post".to_vec()]);
}
