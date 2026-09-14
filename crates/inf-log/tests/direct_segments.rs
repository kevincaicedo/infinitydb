//! M4.5-S34 (ADR-0086): frame format v3, `Direct` segments, the
//! zero-fill state machine, the class-upgrade / not-ready rotations, the
//! `SimDisk` write-through model, and the std tier's pre-zeroing fact.
//! Every test states its goal and method in its first sentence.

use std::path::{Path, PathBuf};

use inf_log::fs::mem::MemFs;
use inf_log::fs::sim::SimDisk;
use inf_log::fs::{SegmentFile, SegmentFs, SegmentIoMode, StdSegmentFs};
use inf_log::{
    DEFAULT_MAX_FRAME_LEN, FRAME_ALIGN, FRAME_HEADER_LEN, FRAME_MAGIC, FRAME_MAGIC_V3,
    FrameBuilder, FrameIter, FrameLayout, FrameStamp, LogError, Lsn, MutationEffect, NsId, ReadEnd,
    ReaderConfig, RecordView, RegionScan, SegmentConfig, SegmentId, SegmentReader, SegmentRotor,
    StagingConfig, StagingRing, ZERO_FILL_SLICE_BYTES, align_up_frame, decode_frame, scan_region,
    scan_region_evidence,
};

fn stamp(seq: u64) -> FrameStamp {
    FrameStamp { epoch: 1, seq, covered_lsn: 0 }
}

fn record(n: u8) -> RecordView<'static> {
    // A few hundred bytes so several frames fit one 16 KiB segment but
    // every frame is far below one 4 KiB block.
    static VALUE: [u8; 200] = [0x5A; 200];
    RecordView::StringPostImage { ns: NsId(u32::from(n) + 1), key: b"key", value: &VALUE }
}

fn direct_cfg(segment_bytes: u32) -> SegmentConfig {
    SegmentConfig { segment_bytes, io_mode: SegmentIoMode::Direct, ..Default::default() }
}

// ---- frame format v3 --------------------------------------------------------

/// A v3 frame is the v2 layout under a new magic whose successor is the
/// next 4 KiB boundary; the builder zero-pads to it and the decoder
/// reports both lengths.
#[test]
fn v3_frame_is_v2_with_an_aligned_successor() {
    let mut packed = FrameBuilder::new();
    let mut aligned = FrameBuilder::new();
    packed.append(&record(1));
    aligned.append(&record(1));
    let first = Lsn::new(SegmentId(0), FRAME_HEADER_LEN as u32);
    let v2 = packed.finalize(first, stamp(1), FrameLayout::Packed).to_vec();
    let sealed = aligned.finalize(first, stamp(1), FrameLayout::Aligned);
    assert_eq!(sealed.as_ptr() as usize % FRAME_ALIGN as usize, 0, "sealed frame is aligned");
    let v3 = sealed.to_vec();

    assert_eq!(&v2[0..4], &FRAME_MAGIC);
    assert_eq!(&v3[0..4], &FRAME_MAGIC_V3);
    assert_eq!(v3.len() as u32, FRAME_ALIGN, "one block for a ~250-byte frame");
    let body_end = v2.len() - 4;
    assert_eq!(&v3[4..body_end], &v2[4..body_end], "header and body identical past the magic");
    assert_ne!(&v3[body_end..v2.len()], &v2[body_end..], "the CRC covers the magic");
    assert!(v3[v2.len()..].iter().all(|&b| b == 0), "padding is zeroed");

    let (frame, consumed) = decode_frame(&v3, DEFAULT_MAX_FRAME_LEN).expect("decodes");
    assert_eq!(frame.layout(), FrameLayout::Aligned);
    assert_eq!(consumed as u32, frame.frame_len());
    assert_eq!(frame.frame_len() as usize, v2.len());
    assert_eq!(frame.padded_len(), FRAME_ALIGN);
    assert_eq!(align_up_frame(4096), 4096);
    assert_eq!(align_up_frame(4097), 8192);
}

/// `FrameIter` walks a mixed v2/v3 image by each frame's own successor
/// rule: v3 frames jump to the boundary, v2 frames pack, and an image
/// ending inside a v3 frame's padding ends the walk cleanly.
#[test]
fn frame_iter_follows_each_frames_successor_rule() {
    let mut image = Vec::new();
    let mut lsns = Vec::new();
    let layouts =
        [FrameLayout::Aligned, FrameLayout::Packed, FrameLayout::Packed, FrameLayout::Aligned];
    for (seq, layout) in (1u64..).zip(layouts) {
        let mut b = FrameBuilder::new();
        b.append(&record(seq as u8));
        let first = Lsn::new(SegmentId(0), image.len() as u32 + FRAME_HEADER_LEN as u32);
        lsns.push(first);
        image.extend_from_slice(b.finalize(first, stamp(seq), layout));
    }
    // Chop the last frame's padding: only its frame bytes survive.
    let last_frame_len = {
        let (frame, _) = decode_frame(&image[image.len() - 4096..], DEFAULT_MAX_FRAME_LEN)
            .expect("last frame decodes");
        frame.frame_len() as usize
    };
    image.truncate(image.len() - 4096 + last_frame_len + 7);

    let walked: Vec<Lsn> = FrameIter::new(&image, DEFAULT_MAX_FRAME_LEN)
        .map(|r| r.expect("valid frames").1.first_lsn())
        .collect();
    assert_eq!(walked, lsns);
}

/// The segment reader skips v3 padding even when the padding extends past
/// its read window (an 8-byte window forces the compaction path), and
/// reports the aligned boundary as the data end.
#[test]
fn reader_skips_padding_across_window_boundaries() {
    let fs = MemFs::new();
    let dir = Path::new("log");
    fs.create_dir_all(dir).expect("dir");
    let mut file = fs.create_segment(&dir.join("seg-000000.ilog"), 64 << 10).expect("seg");
    let mut image = Vec::new();
    for seq in 1..=5u64 {
        let mut b = FrameBuilder::new();
        b.append(&record(seq as u8));
        let first = Lsn::new(SegmentId(0), image.len() as u32 + FRAME_HEADER_LEN as u32);
        image.extend_from_slice(b.finalize(first, stamp(seq), FrameLayout::Aligned));
    }
    file.write_at(0, &image).expect("write");
    for chunk in [8usize, 100, 4096, 1 << 20] {
        let cfg = ReaderConfig { chunk_bytes: chunk, max_frame_len: DEFAULT_MAX_FRAME_LEN };
        let mut reader = SegmentReader::open(&fs, dir, SegmentId(0), cfg).expect("open");
        let mut seen = 0;
        while let Some(frame) = reader.next_frame().expect("valid") {
            seen += 1;
            assert_eq!(frame.first_lsn().offset % FRAME_ALIGN, FRAME_HEADER_LEN as u32);
        }
        assert_eq!(seen, 5, "window {chunk}");
        assert_eq!(reader.read_end(), Some(ReadEnd::ZeroTail { at: 5 * FRAME_ALIGN }));
    }
}

/// The tail scanner counts a v3 frame beyond the data end once and skips
/// its padded extent — padding is the frame's own write, never evidence.
#[test]
fn tail_scan_skips_v3_padding() {
    let fs = MemFs::new();
    let dir = Path::new("log");
    fs.create_dir_all(dir).expect("dir");
    let mut file = fs.create_segment(&dir.join("seg-000003.ilog"), 64 << 10).expect("seg");
    let mut b = FrameBuilder::new();
    b.append(&record(1));
    let at = 2 * FRAME_ALIGN;
    let first = Lsn::new(SegmentId(3), at + FRAME_HEADER_LEN as u32);
    file.write_at(u64::from(at), b.finalize(first, stamp(7), FrameLayout::Aligned)).expect("write");
    let cfg = ReaderConfig::default();
    assert_eq!(
        scan_region(&fs, dir, SegmentId(3), 0, cfg).expect("scan"),
        RegionScan::ValidFrame { offset: at }
    );
    let evidence = scan_region_evidence(&fs, dir, SegmentId(3), 0, cfg).expect("scan");
    assert_eq!(evidence.valid_frames, 1);
    assert_eq!(evidence.max_epoch, 1);
    assert_eq!(
        scan_region(&fs, dir, SegmentId(3), at + FRAME_ALIGN, cfg).expect("scan"),
        RegionScan::AllZero
    );
}

// ---- Direct rotor on the in-memory tier ----------------------------------

/// On a tier that is born allocated (`MemFs`), a `Direct` rotor is
/// pre-zeroed from creation: slots are aligned, padded, write-through
/// eligible, and the synchronous drain writes v3 frames a reader walks.
#[test]
fn direct_rotor_on_memfs_writes_aligned_write_through_frames() {
    let fs = MemFs::new();
    let dir = PathBuf::from("log");
    fs.create_dir_all(&dir).expect("dir");
    let mut rotor =
        SegmentRotor::create_fresh(fs.clone(), dir.clone(), direct_cfg(16 << 10)).expect("fresh");
    assert_eq!(rotor.active_io_mode(), SegmentIoMode::Direct);
    assert!(rotor.active_write_through(), "MemFs is fully allocated at birth");
    let mut ring = StagingRing::new(StagingConfig::with_capacity(8 << 10));
    let mut unpadded_total = 0u64;
    for i in 0..6u8 {
        let value = [i; 100];
        let effect = MutationEffect::StringSet { ns: NsId(1), key: b"k", value: &value };
        unpadded_total += (FRAME_HEADER_LEN + effect.encoded_len() + 4) as u64;
        ring.stage(&effect).expect("fits");
        let lease = ring.flush_into(&mut rotor, 0).expect("flush").expect("frame");
        assert_eq!(lease.frame_len(), FRAME_ALIGN, "one block per small frame");
        assert_eq!(lease.first_record_lsn().offset % FRAME_ALIGN, FRAME_HEADER_LEN as u32);
        ring.release(lease);
    }
    // 4 frames per 16 KiB segment: one rotation happened.
    assert_eq!(rotor.stats().rotations, 1);
    assert_eq!(rotor.active_written(), 2 * FRAME_ALIGN);
    assert_eq!(ring.stats().padding_bytes, 6 * u64::from(FRAME_ALIGN) - unpadded_total);
    let slot = rotor.begin_frame(500, 0).expect("reserve");
    assert_eq!(slot.layout(), FrameLayout::Aligned);
    assert_eq!(slot.len(), FRAME_ALIGN);
    assert!(slot.write_through_ok());
    drop(slot);

    let mut reader =
        SegmentReader::open(&fs, &dir, SegmentId(0), ReaderConfig::default()).expect("open");
    let mut n = 0;
    while reader.next_frame().expect("valid").is_some() {
        n += 1;
    }
    assert_eq!(n, 4);
    assert_eq!(reader.read_end(), Some(ReadEnd::FileEnd { at: 16 << 10 }));
}

/// A frame above `fua_max_frame_bytes` keeps the FLUSH class even on a
/// pre-zeroed direct segment — the probed crossover is per frame.
#[test]
fn frames_above_fua_max_are_not_write_through() {
    let fs = MemFs::new();
    let dir = PathBuf::from("log");
    fs.create_dir_all(&dir).expect("dir");
    let cfg = SegmentConfig { fua_max_frame_bytes: 8192, ..direct_cfg(64 << 10) };
    let mut rotor = SegmentRotor::create_fresh(fs, dir, cfg).expect("fresh");
    let small = rotor.begin_frame(8000, 0).expect("reserve");
    assert!(small.write_through_ok());
    rotor.commit_frame_queued(small);
    let large = rotor.begin_frame(8193, 0).expect("reserve");
    assert_eq!(large.len(), 12288);
    assert!(!large.write_through_ok(), "12 KiB padded frame exceeds the 8 KiB probe bound");
    rotor.commit_frame_queued(large);
}

// ---- zero-fill state machine on the sim disk ------------------------------

/// Drives one zero-fill to completion the way the plane does: slices in
/// order, one in flight, then the barrier — and the next segment becomes
/// ready.
fn zero_fill_to_ready(rotor: &mut SegmentRotor<SimDisk>, disk: &SimDisk, segment_bytes: u32) {
    let mut filled = 0;
    while let Some(slice) = rotor.next_zero_slice(ZERO_FILL_SLICE_BYTES) {
        assert_eq!(slice.offset, u64::from(filled));
        assert!(slice.len > 0 && slice.len.is_multiple_of(FRAME_ALIGN));
        let zeros = vec![0u8; slice.len as usize];
        disk.driver_write_at(slice.fd, slice.offset, &zeros).expect("zero write");
        assert!(rotor.next_zero_slice(ZERO_FILL_SLICE_BYTES).is_none(), "one slice in flight");
        rotor.note_zero_slice_written();
        filled += slice.len;
    }
    assert_eq!(filled, segment_bytes);
    let fd = rotor.take_zero_fill_barrier().expect("barrier owed");
    assert!(rotor.take_zero_fill_barrier().is_none(), "barrier issued once");
    disk.driver_fdatasync(fd).expect("barrier");
    rotor.note_zero_fill_synced();
}

/// On the sim disk a fresh `Direct` cell starts un-zeroed (FLUSH class),
/// MAINTAIN zero-fills the next segment through the driver, and the first
/// frame after the barrier rotates onto it — the class-upgrade rotation.
#[test]
fn zero_fill_then_class_upgrade_rotation() {
    let disk = SimDisk::new();
    let dir = PathBuf::from("log");
    disk.create_dir_all(&dir).expect("dir");
    let segment_bytes = 16 << 10;
    let mut rotor =
        SegmentRotor::create_fresh_deferred(disk.clone(), dir.clone(), direct_cfg(segment_bytes))
            .expect("fresh");
    assert!(!rotor.active_write_through(), "segment 0 is sparse: FLUSH class");
    let slot = rotor.begin_frame_deferred(300, 0).expect("reserve").0;
    assert_eq!(slot.layout(), FrameLayout::Aligned, "aligned frames even before pre-zeroing");
    assert!(!slot.write_through_ok());
    rotor.commit_frame_queued(slot);

    let (report, barrier) = rotor.maintain_deferred(0).expect("maintain");
    assert_eq!(report.preallocated, Some(SegmentId(1)));
    assert!(barrier.is_some(), "the prealloc dir barrier rides the driver");
    assert!(rotor.next_zero_filling());
    assert!(rotor.next_zero_slice(ZERO_FILL_SLICE_BYTES).is_some());
    // Sizes: 16 KiB segment ⇒ one slice.
    rotor.note_zero_slice_written();
    assert!(rotor.next_zero_slice(ZERO_FILL_SLICE_BYTES).is_none(), "filled");
    let fd = rotor.take_zero_fill_barrier().expect("owed");
    disk.driver_fdatasync(fd).expect("sync");
    rotor.note_zero_fill_synced();
    assert!(!rotor.next_zero_filling());
    assert_eq!(rotor.stats().zero_fill_bytes, u64::from(segment_bytes));

    // The next frame upgrades: a deferred seal of segment 0 comes back
    // and the frame lands write-through eligible in segment 1 at 0.
    let (slot, handoff) = rotor.begin_frame_deferred(300, 0).expect("reserve");
    let handoff = handoff.expect("upgrade rotation seals segment 0");
    assert_eq!(handoff.segment(), SegmentId(0));
    assert_eq!(handoff.end_offset(), FRAME_ALIGN);
    assert_eq!(slot.base(), Lsn::new(SegmentId(1), 0));
    assert!(slot.write_through_ok());
    assert!(rotor.active_write_through());
    assert_eq!(rotor.stats().rotations_upgrade, 1);
    assert_eq!(rotor.stats().rotations_unzeroed, 0);
    rotor.commit_frame_queued(slot);
    drop(handoff);
}

/// When the active segment fills while the next one is still zero-filling:
/// a slice in flight makes the frame wait (`NextNotReady`); once no slice
/// is in flight the rotor takes the segment anyway and runs it FLUSH-class
/// (`rotations_unzeroed`) — never a blocking fill, never a silent FUA on
/// unwritten extents.
#[test]
fn not_ready_rotation_waits_for_the_slice_then_degrades_loudly() {
    let disk = SimDisk::new();
    let dir = PathBuf::from("log");
    disk.create_dir_all(&dir).expect("dir");
    let segment_bytes = 16 << 10;
    let mut rotor =
        SegmentRotor::create_fresh_deferred(disk.clone(), dir.clone(), direct_cfg(segment_bytes))
            .expect("fresh");
    rotor.maintain_deferred(0).expect("maintain");
    // Fill segment 0 with four 4 KiB frames.
    for _ in 0..4 {
        let slot = rotor.begin_frame_deferred(300, 0).expect("reserve").0;
        rotor.commit_frame_queued(slot);
    }
    // Zero slice in flight on segment 1: the fifth frame must wait.
    assert!(rotor.next_zero_slice(ZERO_FILL_SLICE_BYTES).is_some());
    assert!(matches!(
        rotor.begin_frame_deferred(300, 0),
        Err(LogError::NextNotReady { segment: SegmentId(1) })
    ));
    // The slice lands but the barrier has not been issued: rotate anyway.
    rotor.note_zero_slice_written();
    let (slot, handoff) = rotor.begin_frame_deferred(300, 0).expect("rotates");
    assert!(handoff.is_some());
    assert_eq!(slot.base().segment, SegmentId(1));
    assert_eq!(slot.layout(), FrameLayout::Aligned);
    assert!(!slot.write_through_ok(), "un-zeroed segment: FLUSH class");
    assert!(!rotor.active_write_through());
    assert_eq!(rotor.stats().rotations_unzeroed, 1);
    assert!(rotor.take_zero_fill_barrier().is_none(), "the abandoned fill owes nothing");
    rotor.commit_frame_queued(slot);

    // The next prealloc starts a fresh fill; once ready, the next frame
    // upgrades again.
    rotor.maintain_deferred(0).expect("maintain");
    zero_fill_to_ready(&mut rotor, &disk, segment_bytes);
    let (_, handoff) = rotor.begin_frame_deferred(300, 0).expect("reserve");
    assert!(handoff.is_some(), "upgrade rotation onto the pre-zeroed segment 2");
    assert!(rotor.active_write_through());
    assert_eq!(rotor.stats().rotations_upgrade, 1);
}

/// A reopened tail is pre-zeroed only if the file says so: a cut that lost
/// the zero-fill barrier leaves a short segment, which reopens FLUSH-class;
/// a synced fill reopens write-through. The fact is read, never remembered.
#[test]
fn reopen_reads_the_prezeroed_fact_from_the_file() {
    let disk = SimDisk::new();
    let dir = PathBuf::from("log");
    disk.create_dir_all(&dir).expect("dir");
    let segment_bytes = 16 << 10;
    let scan = |disk: &SimDisk| inf_log::scan_log_dir(disk, &dir).expect("scan");
    {
        let mut rotor = SegmentRotor::create_fresh_deferred(
            disk.clone(),
            dir.clone(),
            direct_cfg(segment_bytes),
        )
        .expect("fresh");
        rotor.maintain_deferred(0).expect("maintain");
        disk.sync_dir(&dir).expect("names durable");
        // Zero slice written, barrier never issued — then the cut.
        rotor.next_zero_slice(ZERO_FILL_SLICE_BYTES).expect("slice");
        rotor.note_zero_slice_written();
    }
    disk.power_cut(0xBAD);
    let next = dir.join("seg-000001.ilog");
    let file = disk.open_segment_append(&next, SegmentIoMode::Direct).expect("open");
    let survived = file.fully_allocated().expect("fact");
    // Whether the un-synced zero slice survived is the seed's business;
    // either way the reopened rotor reports exactly what the file holds.
    let rotor = SegmentRotor::open_existing(
        disk.clone(),
        dir.clone(),
        direct_cfg(segment_bytes),
        &scan(&disk),
        0,
    )
    .expect("reopen");
    assert_eq!(rotor.active_segment(), SegmentId(1));
    assert_eq!(rotor.active_write_through(), survived);
    drop(rotor);

    // Fill it properly this time: the reopen sees a pre-zeroed tail.
    let mut file = disk.open_segment_append(&next, SegmentIoMode::Direct).expect("open");
    let zeros = vec![0u8; segment_bytes as usize];
    file.write_at(0, &zeros).expect("zeros");
    file.sync_data().expect("barrier");
    let scanned = scan(&disk);
    let rotor =
        SegmentRotor::open_existing(disk.clone(), dir, direct_cfg(segment_bytes), &scanned, 0)
            .expect("reopen");
    assert!(rotor.active_write_through());
}

/// F-L04-01 at the rotor: the whole zero-fill written, its barrier lost to
/// the cut. On every seed where the top sector survived the segment has
/// its full length with holes under it (32 sectors at a 1/2 coin: an
/// intact image is a 2⁻³² event), and the reopened rotor must run FLUSH
/// class. The old sim read the length alone and reopened write-through.
#[test]
fn reopen_after_a_torn_zero_fill_runs_flush_class() {
    let dir = PathBuf::from("log");
    let segment_bytes = 16 << 10;
    let mut full_length_seeds = 0;
    for seed in 0..32u64 {
        let disk = SimDisk::new();
        disk.create_dir_all(&dir).expect("dir");
        {
            let mut rotor = SegmentRotor::create_fresh_deferred(
                disk.clone(),
                dir.clone(),
                direct_cfg(segment_bytes),
            )
            .expect("fresh");
            rotor.maintain_deferred(0).expect("maintain");
            disk.sync_dir(&dir).expect("names durable");
            // Every slice written, the barrier owed but never issued.
            while let Some(slice) = rotor.next_zero_slice(ZERO_FILL_SLICE_BYTES) {
                let zeros = vec![0u8; slice.len as usize];
                disk.driver_write_at(slice.fd, slice.offset, &zeros).expect("zero write");
                rotor.note_zero_slice_written();
            }
            assert!(rotor.take_zero_fill_barrier().is_some(), "barrier owed");
        }
        disk.power_cut(seed);
        let next = dir.join("seg-000001.ilog");
        let len = disk.contents(&next).expect("name survives").len();
        if len != segment_bytes as usize {
            continue;
        }
        full_length_seeds += 1;
        let scanned = inf_log::scan_log_dir(&disk, &dir).expect("scan");
        let rotor = SegmentRotor::open_existing(
            disk.clone(),
            dir.clone(),
            direct_cfg(segment_bytes),
            &scanned,
            0,
        )
        .expect("reopen");
        assert_eq!(rotor.active_segment(), SegmentId(1));
        assert!(
            !rotor.active_write_through(),
            "seed {seed}: a full-length torn fill reopened write-through"
        );
    }
    assert!(full_length_seeds > 0, "no seed kept the top sector — nothing tested");
}

// ---- SimDisk write-through model ------------------------------------------

/// Zero-fills `[0, len)` through the driver and commits the mapping with
/// the barrier — the ADR-0086 D4 discipline a write-through relies on.
fn commit_extent(disk: &SimDisk, fd: i32, len: usize) {
    disk.driver_write_at(fd, 0, &vec![0u8; len]).expect("zero-fill");
    disk.driver_fdatasync(fd).expect("barrier");
}

/// A write-through onto a committed extent lands durably at completion,
/// supersedes the pending writes it overlaps (a later FUA-acknowledged
/// write cannot be resurrected over), and leaves later plain writes to
/// the cut's coin.
#[test]
fn sim_write_through_is_durable_and_supersedes_overlaps() {
    let disk = SimDisk::new();
    let dir = Path::new("d");
    disk.create_dir_all(dir).expect("dir");
    let path = dir.join("f");
    let file = disk.create_segment(&path, 0).expect("create");
    disk.sync_dir(dir).expect("name");
    let fd = file.raw_fd().expect("sim fd");
    commit_extent(&disk, fd, 14);
    // Pending plain write over [0, 8), then a write-through over [4, 12).
    disk.driver_write_at(fd, 0, &[1u8; 8]).expect("plain");
    disk.driver_write_through(fd, 4, &[2u8; 8]).expect("through");
    // Later plain write over [10, 14).
    disk.driver_write_at(fd, 10, &[3u8; 4]).expect("plain");
    assert_eq!(disk.contents(&path).expect("os view"), {
        let mut v = vec![1u8; 4];
        v.extend_from_slice(&[2; 6]);
        v.extend_from_slice(&[3; 4]);
        v
    });
    for seed in 0..64u64 {
        let d = disk.clone();
        // Clones share state: snapshot by re-running the ops on a fresh
        // disk per seed instead.
        drop(d);
        let disk = SimDisk::new();
        disk.create_dir_all(dir).expect("dir");
        let file = disk.create_segment(&path, 0).expect("create");
        disk.sync_dir(dir).expect("name");
        let fd = file.raw_fd().expect("sim fd");
        commit_extent(&disk, fd, 14);
        disk.driver_write_at(fd, 0, &[1u8; 8]).expect("plain");
        disk.driver_write_through(fd, 4, &[2u8; 8]).expect("through");
        disk.driver_write_at(fd, 10, &[3u8; 4]).expect("plain");
        disk.power_cut(seed);
        let image = disk.contents(&path).expect("survives");
        assert_eq!(&image[4..10], &[2u8; 6], "seed {seed}: through bytes never resurrected over");
        for (i, &b) in image[..4].iter().enumerate() {
            assert!(b == 0 || b == 1, "seed {seed}: byte {i} is the plain write or zero");
        }
        for (i, &b) in image[10..].iter().enumerate() {
            // Past the through's end the committed extent's zero shows
            // where the plain write was lost.
            let legal = if 10 + i < 12 { b == 2 || b == 3 } else { b == 0 || b == 3 };
            assert!(legal, "seed {seed}: byte {} is old-through, new-plain or zero", 10 + i);
        }
    }
}

/// `FsyncLies`-class behavior is the driver's business; the disk itself
/// must count a write-through onto a committed extent as durable with no
/// barrier of its own.
#[test]
fn sim_write_through_needs_no_fdatasync() {
    let disk = SimDisk::new();
    let dir = Path::new("d");
    disk.create_dir_all(dir).expect("dir");
    let path = dir.join("f");
    let file = disk.create_segment(&path, 0).expect("create");
    disk.sync_dir(dir).expect("name");
    let fd = file.raw_fd().expect("sim fd");
    commit_extent(&disk, fd, 512);
    disk.driver_write_through(fd, 0, b"durable").expect("through");
    disk.power_cut(7);
    assert_eq!(&disk.contents(&path).expect("survives")[..7], b"durable");
}

/// F-L04-01 / ADR-0119 D1: a write-through into a hole is data under a
/// mapping no barrier committed — it rides the cut's coin like a plain
/// write (some seeds keep it, some lose it), never "durable at
/// completion". The sim's old model kept it on every seed, which is why
/// a rotor that *assumed* pre-zeroing could never lose a frame here.
#[test]
fn sim_write_through_into_a_hole_rides_the_cut() {
    let dir = Path::new("d");
    let path = dir.join("f");
    let (mut kept, mut lost) = (0, 0);
    for seed in 0..64u64 {
        let disk = SimDisk::new();
        disk.create_dir_all(dir).expect("dir");
        let file = disk.create_segment(&path, 0).expect("create");
        disk.sync_dir(dir).expect("name");
        let fd = file.raw_fd().expect("sim fd");
        disk.driver_write_through(fd, 0, b"durable").expect("through");
        disk.power_cut(seed);
        match disk.contents(&path).expect("name survives").as_slice() {
            b"durable" => kept += 1,
            b"" => lost += 1,
            other => panic!("seed {seed}: torn to {other:?}"),
        }
    }
    assert!(kept > 0 && lost > 0, "the coin must fall both ways ({kept} kept, {lost} lost)");
}

/// F-L04-01: a torn zero-fill — full length because the top sector
/// survived, holes underneath — reads `fully_allocated() == false`, as
/// `st_blocks` does on the device. The old length-based rule read `true`
/// on every such seed and sent the rotor's FUA frames into holes. The
/// fill is a non-zero pattern so a lost sector is visible in the image.
#[test]
fn sim_torn_zero_fill_is_not_fully_allocated() {
    let dir = Path::new("d");
    let path = dir.join("seg");
    let target = 64usize << 10;
    let (mut full_length, mut torn_at_full_length) = (0, 0);
    for seed in 0..64u64 {
        let disk = SimDisk::new();
        disk.create_dir_all(dir).expect("dir");
        let file = disk.create_segment_direct(&path, target as u64).expect("create");
        disk.sync_dir(dir).expect("name");
        let fd = file.raw_fd().expect("sim fd");
        assert!(!file.fully_allocated().expect("fact"), "born sparse");
        for piece in 0..target / 4096 {
            disk.driver_write_at(fd, (piece * 4096) as u64, &[0xAB; 4096]).expect("fill");
        }
        assert!(file.fully_allocated().expect("fact"), "written through, barrier pending");
        drop(file);
        disk.power_cut(seed);
        let image = disk.contents(&path).expect("name survives");
        let hole = image.iter().any(|&b| b != 0xAB);
        let file = disk.open_segment_append(&path, SegmentIoMode::Direct).expect("open");
        let allocated = file.fully_allocated().expect("fact");
        assert_eq!(
            allocated,
            image.len() == target && !hole,
            "seed {seed}: len {} hole {hole}",
            image.len()
        );
        full_length += u64::from(image.len() == target);
        torn_at_full_length += u64::from(image.len() == target && hole);
    }
    assert!(full_length > 0, "no seed kept the top sector");
    assert!(torn_at_full_length > 0, "no seed tore under a full length — nothing tested");
}

// ---- std tier: the pre-zeroing fact -------------------------------------

/// On a real filesystem a direct segment is born sparse (`fully_allocated
/// == false`); writing every block of zeros makes it `true`. Skips when the
/// filesystem refuses `O_DIRECT` (the typed `Unsupported`, never a
/// silent fallback).
#[test]
fn std_direct_segment_reports_allocation_honestly() {
    let root = std::env::var_os("CARGO_TARGET_TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join(format!("inf-log-s34-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let fs = StdSegmentFs;
    fs.create_dir_all(&root).expect("dir");
    let path = root.join("seg-000000.ilog");
    let size: u64 = 256 << 10;
    let mut file = match fs.create_segment_direct(&path, size) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::Unsupported => {
            eprintln!("skipping: {err}");
            return;
        }
        Err(err) => panic!("create_segment_direct: {err}"),
    };
    assert_eq!(file.file_size().expect("size"), size);
    assert!(!file.fully_allocated().expect("fact"), "sparse at creation");
    // Aligned source buffer: O_DIRECT needs it (the ADR-0054 D2 shape).
    let mut raw = vec![0u8; (size as usize) + FRAME_ALIGN as usize];
    let at = raw.as_ptr().align_offset(FRAME_ALIGN as usize);
    let zeros = &mut raw[at..at + size as usize];
    file.write_at(0, zeros).expect("zero-fill");
    file.sync_data().expect("barrier");
    assert!(file.fully_allocated().expect("fact"), "every block written");
    // The reopen path sees the same fact.
    let reopened = fs.open_segment_append(&path, SegmentIoMode::Direct).expect("reopen");
    assert!(reopened.fully_allocated().expect("fact"));
    // A buffered sparse sibling stays `false` after a partial write.
    let sparse = fs.create_segment_unsynced(&root.join("seg-000001.ilog"), size).expect("sparse");
    assert!(!sparse.fully_allocated().expect("fact"));
    let _ = std::fs::remove_dir_all(&root);
}

/// While the active segment is pre-zeroed, the next segment's zero-fill is
/// paced against the active fill level (`2 × written + head start`): it
/// spreads across the segment's life instead of bursting at device speed
/// — the A/B's p99 lesson. An un-zeroed active segment fills unpaced.
#[test]
fn zero_fill_is_paced_by_the_active_segments_fill() {
    let disk = SimDisk::new();
    let dir = PathBuf::from("log");
    disk.create_dir_all(&dir).expect("dir");
    let segment_bytes: u32 = 64 << 20;
    // The pre-D9 prealloc (ADR-0090 D9 would wait for the pool after the
    // upgrade rotation; its own pacing test lives in `segment_prealloc_
    // wait.rs`).
    let cfg =
        SegmentConfig { prealloc: inf_log::PreallocPolicy::Immediate, ..direct_cfg(segment_bytes) };
    let mut rotor = SegmentRotor::create_fresh_deferred(disk.clone(), dir, cfg).expect("fresh");
    // Segment 0 is un-zeroed: segment 1 fills unpaced, end to end.
    rotor.maintain_deferred(0).expect("maintain");
    zero_fill_to_ready(&mut rotor, &disk, segment_bytes);
    let (slot, handoff) = rotor.begin_frame_deferred(300, 0).expect("upgrade");
    assert!(handoff.is_some());
    rotor.commit_frame_queued(slot);
    assert!(rotor.active_write_through());

    // Segment 2 fills paced: the head start first, then 2 B per byte the
    // active segment takes.
    rotor.maintain_deferred(0).expect("maintain");
    let mut issued = 0u32;
    while let Some(slice) = rotor.next_zero_slice(ZERO_FILL_SLICE_BYTES) {
        disk.driver_write_at(slice.fd, slice.offset, &vec![0u8; slice.len as usize]).expect("w");
        rotor.note_zero_slice_written();
        issued += slice.len;
    }
    // Slices issue while `cursor < 2 × written + head start`, so the fill
    // stops at the first slice boundary at or past that bound.
    let bound = |written: u32| {
        let allowed = 2 * written + inf_log::ZERO_FILL_HEAD_START;
        allowed.div_ceil(ZERO_FILL_SLICE_BYTES) * ZERO_FILL_SLICE_BYTES
    };
    assert_eq!(rotor.active_written(), FRAME_ALIGN, "the upgrade frame");
    assert_eq!(issued, bound(FRAME_ALIGN), "head start (+ one frame's worth) while nearly empty");
    // 4 MiB of frames ⇒ 8 MiB more of fill allowed.
    while rotor.active_written() < (4 << 20) + FRAME_ALIGN {
        let slot = rotor.begin_frame_deferred(FRAME_ALIGN - 44, 0).expect("reserve").0;
        rotor.commit_frame_queued(slot);
    }
    let mut total = issued;
    while let Some(slice) = rotor.next_zero_slice(ZERO_FILL_SLICE_BYTES) {
        disk.driver_write_at(slice.fd, slice.offset, &vec![0u8; slice.len as usize]).expect("w");
        rotor.note_zero_slice_written();
        total += slice.len;
    }
    assert_eq!(total, bound(rotor.active_written()), "2× gain over the log's own rate");
    assert!(rotor.next_zero_filling());
}

// ---- FLUSH → FUA on an existing log (ADR-0086 D4 as amended, 2026-08-21) ----

/// A packed (v2) tail reopened under a `Direct` rotor keeps packing —
/// `Buffered` at its exact end, no alignment slack — and upgrades at the
/// next rotation; the reader then walks v2 → v2 → v3 without a gap. The
/// review finding this pins: rounding the tail up to 4 KiB left a zero
/// word the reader took for end-of-log, and the frame beyond it (an
/// acked `always` record) was discarded as un-attested residue on the
/// following boot.
#[test]
fn packed_tail_reopens_buffered_under_a_direct_rotor_and_upgrades_at_rotation() {
    let disk = SimDisk::new();
    let dir = PathBuf::from("log");
    disk.create_dir_all(&dir).expect("dir");
    let segment_bytes = 16 << 10;
    let buffered_cfg =
        SegmentConfig { segment_bytes, io_mode: SegmentIoMode::Buffered, ..Default::default() };
    let frame = |seq: u64, first: Lsn, layout: FrameLayout| {
        let mut b = FrameBuilder::new();
        b.append(&record(seq as u8));
        b.finalize(first, stamp(seq), layout).to_vec()
    };
    let frame_len = |seq: u8| {
        let mut b = FrameBuilder::new();
        b.append(&record(seq));
        b.frame_len()
    };
    // Life 1 (FLUSH class): one packed frame; its end is not aligned.
    let packed_end = {
        let mut rotor =
            SegmentRotor::create_fresh(disk.clone(), dir.clone(), buffered_cfg).expect("fresh");
        let slot = rotor.begin_frame(frame_len(1), 0).expect("reserve");
        assert_eq!(slot.layout(), FrameLayout::Packed);
        let bytes = frame(1, slot.first_record_lsn(), FrameLayout::Packed);
        rotor.commit_frame(slot, &bytes).expect("commit");
        rotor.active_written()
    };
    assert!(!packed_end.is_multiple_of(FRAME_ALIGN), "a ~250-byte v2 frame ends unaligned");

    // Life 2 (FUA class configured): the tail reopens Buffered, at its end.
    let scan = inf_log::scan_log_dir(&disk, &dir).expect("scan");
    let mut rotor = SegmentRotor::open_existing(
        disk.clone(),
        dir.clone(),
        direct_cfg(segment_bytes),
        &scan,
        packed_end,
    )
    .expect("reopen");
    assert_eq!(rotor.active_io_mode(), SegmentIoMode::Buffered, "packed tail keeps packing");
    assert_eq!(rotor.active_written(), packed_end, "no alignment slack inserted");
    assert!(!rotor.active_write_through());
    assert_eq!(rotor.stats().reopened_packed_tails, 1);
    // The second frame lands packed, contiguous with the first.
    let len = frame_len(2);
    let (slot, handoff) = rotor.begin_frame_deferred(len, 0).expect("reserve");
    assert!(handoff.is_none());
    assert_eq!(slot.layout(), FrameLayout::Packed);
    assert_eq!(slot.base().offset, packed_end);
    let second = frame(2, slot.first_record_lsn(), FrameLayout::Packed);
    disk.driver_write_at(rotor.active_raw_fd().expect("fd"), u64::from(packed_end), &second)
        .expect("write");
    rotor.commit_frame_queued(slot);
    let second_end = rotor.active_written();
    // MAINTAIN pre-zeroes segment 1; the next frame is the upgrade rotation.
    rotor.maintain_deferred(0).expect("maintain");
    disk.sync_dir(&dir).expect("names");
    zero_fill_to_ready(&mut rotor, &disk, segment_bytes);
    let (slot, handoff) = rotor.begin_frame_deferred(len, 0).expect("reserve");
    let handoff = handoff.expect("upgrade rotation seals segment 0");
    assert_eq!(handoff.end_offset(), second_end, "segment 0 seals at its packed end");
    assert_eq!(slot.base(), Lsn::new(SegmentId(1), 0));
    assert_eq!(slot.layout(), FrameLayout::Aligned);
    assert!(slot.write_through_ok());
    assert_eq!(rotor.active_io_mode(), SegmentIoMode::Direct);
    assert_eq!(rotor.stats().rotations_upgrade, 1);
    let third = frame(3, slot.first_record_lsn(), FrameLayout::Aligned);
    disk.driver_write_through(rotor.active_raw_fd().expect("fd"), 0, &third).expect("through");
    rotor.commit_frame_queued(slot);
    drop(handoff);

    // Every frame is reachable: segment 0 reads two packed frames to a
    // clean end at `second_end`; segment 1 reads the aligned one.
    let mut reader =
        SegmentReader::open(&disk, &dir, SegmentId(0), ReaderConfig::default()).expect("open");
    let mut seen = 0;
    while reader.next_frame().expect("frame").is_some() {
        seen += 1;
    }
    assert_eq!(seen, 2);
    assert_eq!(reader.read_end(), Some(ReadEnd::ZeroTail { at: second_end }));
    let mut reader =
        SegmentReader::open(&disk, &dir, SegmentId(1), ReaderConfig::default()).expect("open");
    assert!(reader.next_frame().expect("frame").is_some());
    assert!(reader.next_frame().expect("frame").is_none());
    assert_eq!(reader.read_end(), Some(ReadEnd::ZeroTail { at: FRAME_ALIGN }));

    // An aligned tail (the v3 end) reopens Direct — the transition rule
    // binds on the tail's shape, not on the log's history.
    let scan = inf_log::scan_log_dir(&disk, &dir).expect("scan");
    let rotor = SegmentRotor::open_existing(
        disk.clone(),
        dir.clone(),
        direct_cfg(segment_bytes),
        &scan,
        FRAME_ALIGN,
    )
    .expect("reopen");
    assert_eq!(rotor.active_io_mode(), SegmentIoMode::Direct);
    assert_eq!(rotor.stats().reopened_packed_tails, 0);
}

// ---- F-L04-14 sibling: the sync tier's injecting points on a Direct segment ----

/// Goal: `torn_frame` and `log_append_short_write` inject on a `Direct`
/// segment at the sync tier (`commit_frame` — the staging ring's path):
/// the landed prefix is sector-granular, whole-block shaped, and the
/// torn frame never decodes. Method: `SimDisk` asserts the O_DIRECT
/// contract on the direct inode; before the fix both points wrote a
/// sub-block prefix and died in that assertion.
#[test]
fn sync_tier_torn_points_inject_on_a_direct_segment() {
    use inf_foundation::fault::{self, FaultSpec};
    fault::disarm_all();
    for (point, succeeds) in [("torn_frame", true), ("log_append_short_write", false)] {
        let disk = SimDisk::new();
        let dir = PathBuf::from("log");
        disk.create_dir_all(&dir).expect("dir");
        let mut rotor = SegmentRotor::create_fresh(disk.clone(), dir.clone(), direct_cfg(64 << 10))
            .expect("fresh");
        // A ~3 KiB record: the frame's CRC sits past every tear below, so
        // the torn block never decodes (a ~250-byte frame would survive
        // whole inside the first sector — honest, but vacuous here).
        let mut b = FrameBuilder::new();
        b.append(&RecordView::StringPostImage { ns: NsId(1), key: b"key", value: &[0x5A; 3000] });
        let slot = rotor.begin_frame(b.frame_len(), 0).expect("reserve");
        let frame = b.finalize(slot.first_record_lsn(), stamp(1), FrameLayout::Aligned).to_vec();
        assert_eq!(frame.len() as u32, FRAME_ALIGN, "one aligned block");
        fault::arm(point, FaultSpec::Nth(1));
        let outcome = rotor.commit_frame(slot, &frame);
        assert_eq!(fault::fired(point), 1, "{point} fired");
        fault::disarm_all();
        assert_eq!(outcome.is_ok(), succeeds, "{point}: {outcome:?}");
        let image = disk.contents(&dir.join("seg-000000.ilog")).expect("exists");
        let cut = if succeeds { 4096 * 2 / 3 / 512 * 512 } else { 2048 };
        assert_eq!(&image[..cut], &frame[..cut], "{point}: the sector-rounded prefix landed");
        assert!(image[cut..4096].iter().all(|&b| b == 0), "{point}: nothing beyond the tear");
        assert_eq!(
            FrameIter::new(&image, DEFAULT_MAX_FRAME_LEN).filter_map(Result::ok).count(),
            0,
            "{point}: the torn frame never decodes"
        );
    }
}

// ---- F-L04-07: `O_DIRECT` is a property of the open file description ----

/// F-L04-07: a `Direct` handle stays alignment-checked after the same
/// file is reopened `Buffered` (the ADR-0086 D7 packed-tail reopen).
/// Pre-fix the reopen cleared the *inode's* flag, so a misaligned
/// write-through on the still-open direct fd — `EINVAL` on a real
/// `O_DIRECT` fd — passed the simulator silently.
#[test]
#[should_panic(expected = "direct write offset 7 is not 4096-aligned")]
fn direct_handle_stays_alignment_checked_after_a_buffered_reopen() {
    let disk = SimDisk::new();
    let dir = PathBuf::from("log");
    disk.create_dir_all(&dir).expect("dir");
    let path = dir.join("seg-000000.ilog");
    let direct = disk.create_segment_direct(&path, 64 << 10).expect("create direct");
    let direct_fd = direct.raw_fd().expect("sim fd");
    let _buffered = disk.open_segment_append(&path, SegmentIoMode::Buffered).expect("reopen");
    disk.driver_write_through(direct_fd, 7, &[0u8; 13]).expect("misaligned on the direct fd");
}

/// F-L04-07, the converse: a `Buffered` handle is never alignment-checked
/// because the same file was later reopened `Direct` — pre-fix the
/// reopen armed the inode and a legal packed write on the buffered fd
/// died in the sim's assertion.
#[test]
fn buffered_handle_is_not_alignment_checked_after_a_direct_reopen() {
    let disk = SimDisk::new();
    let dir = PathBuf::from("log");
    disk.create_dir_all(&dir).expect("dir");
    let path = dir.join("seg-000000.ilog");
    let buffered = disk.create_segment(&path, 64 << 10).expect("create buffered");
    let buffered_fd = buffered.raw_fd().expect("sim fd");
    let _direct = disk.open_segment_append(&path, SegmentIoMode::Direct).expect("reopen");
    disk.driver_write_at(buffered_fd, 7, &[0x5A; 13]).expect("packed write on the buffered fd");
    let image = disk.contents(&path).expect("exists");
    assert_eq!(&image[7..20], &[0x5A; 13], "the packed write landed");
}

/// F-L04-07's mechanism: every open is its own file description with its
/// own fd (pre-fix the fd *was* the inode, so two handles shared one fd
/// and a mode could only live on the inode). Closing one handle closes
/// its fd (`EBADF`, F-L01-02) and leaves the other serving.
#[test]
fn each_open_is_its_own_file_description() {
    let disk = SimDisk::new();
    let dir = PathBuf::from("log");
    disk.create_dir_all(&dir).expect("dir");
    let path = dir.join("seg-000000.ilog");
    let first = disk.create_segment(&path, 64 << 10).expect("create");
    let second = disk.open_segment_append(&path, SegmentIoMode::Buffered).expect("reopen");
    let (a, b) = (first.raw_fd().expect("fd"), second.raw_fd().expect("fd"));
    assert_ne!(a, b, "two opens, two file descriptions");
    drop(first);
    let err = disk.driver_write_at(a, 0, &[1u8; 8]).expect_err("closed fd");
    assert_eq!(err.raw_os_error(), Some(libc::EBADF), "{err}");
    disk.driver_write_at(b, 0, &[2u8; 8]).expect("the live handle's fd serves");
    assert_eq!(&disk.contents(&path).expect("exists")[..8], &[2u8; 8]);
}
