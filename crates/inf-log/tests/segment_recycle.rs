//! M4.5-S39b (ADR-0090): segment recycling at the rotor — a covered,
//! pre-zeroed `Direct` segment is pooled at truncation and becomes the
//! next segment by rename; the pool is bounded; a pooled file is re-read
//! (never assumed) at reuse; the rename's directory entry rides the same
//! barrier a fresh prealloc's does; the residue a recycled file carries
//! reads as foreign-segment frames, never as data. Every test states its
//! goal and method in its first sentence.

use std::path::PathBuf;

use inf_log::fs::sim::SimDisk;
use inf_log::fs::{SegmentFile, SegmentFs, SegmentIoMode};
use inf_log::{
    FRAME_ALIGN, FrameBuilder, FrameLayout, FrameStamp, NsId, ReadError, ReaderConfig, RecordView,
    SealedDisposal, SegmentConfig, SegmentId, SegmentReader, SegmentRotor, ZERO_FILL_SLICE_BYTES,
    scan_region_evidence, segment_file_name,
};

const SEGMENT_BYTES: u32 = 16 << 10;

fn stamp(seq: u64) -> FrameStamp {
    FrameStamp { epoch: 1, seq, covered_lsn: 0 }
}

fn record(n: u8) -> RecordView<'static> {
    static VALUE: [u8; 200] = [0x5A; 200];
    RecordView::StringPostImage { ns: NsId(u32::from(n) + 1), key: b"key", value: &VALUE }
}

fn cfg(recycle_slots: u8) -> SegmentConfig {
    SegmentConfig {
        segment_bytes: SEGMENT_BYTES,
        io_mode: SegmentIoMode::Direct,
        recycle_slots,
        ..Default::default()
    }
}

struct Lab {
    disk: SimDisk,
    dir: PathBuf,
    rotor: SegmentRotor<SimDisk>,
    seq: u64,
}

impl Lab {
    fn new(recycle_slots: u8) -> Lab {
        let disk = SimDisk::new();
        let dir = PathBuf::from("log");
        disk.create_dir_all(&dir).expect("dir");
        let rotor =
            SegmentRotor::create_fresh_deferred(disk.clone(), dir.clone(), cfg(recycle_slots))
                .expect("fresh");
        Lab { disk, dir, rotor, seq: 0 }
    }

    /// MAINTAIN the way the plane does: prealloc (barrier synced through
    /// the driver at once), then the zero-fill to ready.
    fn maintain(&mut self) -> inf_log::MaintainReport {
        let (report, barrier) = self.rotor.maintain_deferred(0).expect("maintain");
        if let Some(barrier) = barrier {
            let fd = barrier.dir.raw_fd().expect("sim dir fd");
            self.disk.driver_fdatasync(fd).expect("dir barrier");
        }
        while let Some(slice) = self.rotor.next_zero_slice(ZERO_FILL_SLICE_BYTES) {
            let zeros = vec![0u8; slice.len as usize];
            self.disk.driver_write_at(slice.fd, slice.offset, &zeros).expect("zero write");
            self.rotor.note_zero_slice_written();
        }
        if let Some(fd) = self.rotor.take_zero_fill_barrier() {
            self.disk.driver_fdatasync(fd).expect("barrier");
            self.rotor.note_zero_fill_synced();
        }
        report
    }

    /// One frame written write-through at the reserved base; returns
    /// whether it rotated.
    fn frame(&mut self) -> bool {
        let mut b = FrameBuilder::new();
        b.append(&record(1));
        let (slot, handoff) = self.rotor.begin_frame_deferred(b.frame_len(), 0).expect("reserve");
        self.seq += 1;
        let bytes = b.finalize(slot.first_record_lsn(), stamp(self.seq), FrameLayout::Aligned);
        let fd = self.rotor.active_raw_fd().expect("fd");
        self.disk.driver_write_through(fd, u64::from(slot.base().offset), bytes).expect("frame");
        self.rotor.commit_frame_queued(slot);
        handoff.is_some()
    }

    /// Fill the active segment until it rotates onto the ready next one.
    fn rotate(&mut self) -> SegmentId {
        let before = self.rotor.active_segment();
        while !self.frame() {}
        assert_ne!(self.rotor.active_segment(), before);
        self.rotor.active_segment()
    }

    fn names(&self) -> Vec<String> {
        let mut names = self.disk.list_dir(&self.dir).expect("list");
        names.sort();
        names
    }
}

/// Goal: a covered pre-zeroed segment is pooled at truncation and renamed
/// into the next id by MAINTAIN — no zero-fill, the dir barrier returned.
/// Method: two rotations on a `Direct` sim rotor, forget both sealed
/// segments, maintain again.
#[test]
fn covered_prezeroed_segment_is_pooled_and_renamed_into_the_next_id() {
    let mut lab = Lab::new(1);
    lab.maintain(); // seg 1 pre-zeroed, ready
    assert_eq!(lab.rotate(), SegmentId(1), "class-upgrade rotation onto seg 1");
    lab.maintain(); // seg 2 pre-zeroed
    assert_eq!(lab.rotate(), SegmentId(2));
    let zero_fill_before = lab.rotor.stats().zero_fill_bytes;
    assert_eq!(zero_fill_before, 2 * u64::from(SEGMENT_BYTES), "two generations paid so far");

    // Segment 0 was born sparse (FLUSH class) — never a candidate.
    assert_eq!(
        lab.rotor.forget_sealed(SegmentId(0)),
        SealedDisposal::Unlink(lab.dir.join(segment_file_name(SegmentId(0))))
    );
    // Segment 1 was pre-zeroed when active — pooled.
    assert_eq!(lab.rotor.forget_sealed(SegmentId(1)), SealedDisposal::Recycled);
    assert_eq!(lab.rotor.pooled(), vec![SegmentId(1)]);
    assert_eq!(lab.rotor.recycle_pool_bytes(), u64::from(SEGMENT_BYTES));

    // MAINTAIN: the pooled file becomes seg 3 by rename; the barrier is
    // returned exactly as for a fresh prealloc (ADR-0090 D3 as amended).
    let (report, barrier) = lab.rotor.maintain_deferred(0).expect("maintain");
    assert_eq!(report.preallocated, Some(SegmentId(3)));
    assert!(barrier.is_some(), "the rename's dir entry rides the prealloc barrier");
    assert_eq!(lab.rotor.next_ready(), Some(SegmentId(3)), "ready at once: no fill");
    assert!(!lab.rotor.next_zero_filling());
    assert!(lab.rotor.next_zero_slice(ZERO_FILL_SLICE_BYTES).is_none());
    let stats = lab.rotor.stats();
    assert_eq!(stats.segments_recycled, 1);
    assert_eq!(stats.recycle_misses, 2, "the two first-generation preallocs found no pool");
    assert_eq!(stats.recycle_fallbacks, 0);
    assert_eq!(stats.zero_fill_bytes, zero_fill_before, "the second write was not paid");
    assert_eq!(lab.rotor.pooled(), Vec::<SegmentId>::new());
    assert_eq!(lab.rotor.recycle_pool_bytes(), 0);
    assert_eq!(
        lab.names(),
        vec!["seg-000000.ilog", "seg-000002.ilog", "seg-000003.ilog"],
        "seg 1's file now carries id 3; seg 0 awaits the caller's unlink"
    );
    // The next frame that rotates lands write-through on the recycled
    // segment: it is fully allocated by its previous life.
    assert_eq!(lab.rotate(), SegmentId(3));
    assert!(lab.rotor.active_write_through());
    assert_eq!(lab.rotor.stats().rotations_unzeroed, 0);
}

/// Goal: the pool is bounded at `recycle_slots`; the overflow is unlinked
/// as before, counted. Method: one slot, two pooled candidates.
#[test]
fn pool_is_bounded_and_the_overflow_is_unlinked() {
    let mut lab = Lab::new(1);
    lab.maintain();
    lab.rotate();
    lab.maintain();
    lab.rotate();
    lab.maintain();
    lab.rotate(); // sealed: 0 (sparse), 1, 2 (pre-zeroed)
    assert_eq!(lab.rotor.forget_sealed(SegmentId(1)), SealedDisposal::Recycled);
    assert_eq!(
        lab.rotor.forget_sealed(SegmentId(2)),
        SealedDisposal::Unlink(lab.dir.join(segment_file_name(SegmentId(2))))
    );
    assert_eq!(lab.rotor.stats().recycle_pool_full, 1);
    assert_eq!(lab.rotor.recycle_pool_bytes(), u64::from(SEGMENT_BYTES), "one slot held");
}

/// Goal: with recycling off (`--no-segment-recycle`), or under a
/// `Buffered` rotor, every covered segment is unlinked and MAINTAIN
/// counts no misses — the pre-S39b path byte-for-byte. Method: both
/// configurations through a rotation each.
#[test]
fn recycling_off_or_buffered_never_pools() {
    let mut lab = Lab::new(0);
    lab.maintain();
    lab.rotate();
    lab.maintain();
    lab.rotate();
    assert_eq!(
        lab.rotor.forget_sealed(SegmentId(1)),
        SealedDisposal::Unlink(lab.dir.join(segment_file_name(SegmentId(1))))
    );
    lab.maintain();
    let stats = lab.rotor.stats();
    assert_eq!((stats.segments_recycled, stats.recycle_misses, stats.recycle_fallbacks), (0, 0, 0));

    let disk = SimDisk::new();
    let dir = PathBuf::from("buffered");
    disk.create_dir_all(&dir).expect("dir");
    let buffered = SegmentConfig {
        segment_bytes: SEGMENT_BYTES,
        io_mode: SegmentIoMode::Buffered,
        recycle_slots: 1,
        ..Default::default()
    };
    let mut rotor =
        SegmentRotor::create_fresh_deferred(disk.clone(), dir.clone(), buffered).expect("fresh");
    rotor.maintain_deferred(0).expect("maintain");
    let mut b = FrameBuilder::new();
    b.append(&record(1));
    let mut rotated = false;
    while !rotated {
        let (slot, handoff) = rotor.begin_frame_deferred(b.frame_len(), 0).expect("reserve");
        rotor.commit_frame_queued(slot);
        rotated = handoff.is_some();
    }
    assert_eq!(
        rotor.forget_sealed(SegmentId(0)),
        SealedDisposal::Unlink(dir.join(segment_file_name(SegmentId(0))))
    );
    assert_eq!(rotor.stats().recycle_misses, 0, "a Buffered rotor never misses: it never tries");
}

/// Goal: a prealloc that finds the pool empty counts a miss and fills as
/// before. Method: recycling on, nothing pooled yet.
#[test]
fn an_empty_pool_is_a_counted_miss_that_zero_fills() {
    let mut lab = Lab::new(1);
    lab.maintain();
    assert_eq!(lab.rotor.stats().recycle_misses, 1);
    assert_eq!(lab.rotor.stats().zero_fill_bytes, u64::from(SEGMENT_BYTES));
}

/// Goal: a pooled file is re-read at reuse (ADR-0086 D4) — one that no
/// longer reads fully allocated falls back to the zero-fill and counts a
/// fallback, never a recycle. Method: truncate the pooled file behind the
/// rotor, then maintain.
#[test]
fn a_pooled_file_that_reads_sparse_falls_back_to_the_fill() {
    let mut lab = Lab::new(1);
    lab.maintain();
    lab.rotate();
    lab.maintain();
    lab.rotate();
    assert_eq!(lab.rotor.forget_sealed(SegmentId(1)), SealedDisposal::Recycled);
    let mut file =
        lab.disk.open_write(&lab.dir.join(segment_file_name(SegmentId(1)))).expect("open");
    file.truncate(u64::from(SEGMENT_BYTES / 2)).expect("shrink");
    drop(file);

    let (report, barrier) = lab.rotor.maintain_deferred(0).expect("maintain");
    assert_eq!(report.preallocated, Some(SegmentId(3)));
    assert!(barrier.is_some());
    assert!(lab.rotor.next_zero_filling(), "read sparse ⇒ fills like a fresh segment");
    let stats = lab.rotor.stats();
    assert_eq!(stats.segments_recycled, 0);
    assert_eq!(stats.recycle_fallbacks, 1);
    assert!(lab.names().contains(&"seg-000003.ilog".to_owned()), "renamed all the same");
}

/// Goal: a rename that fails falls back to a fresh prealloc, counted, with
/// the barrier still returned. Method: remove the pooled file behind the
/// rotor, then maintain.
#[test]
fn a_failed_rename_falls_back_to_a_fresh_prealloc() {
    let mut lab = Lab::new(1);
    lab.maintain();
    lab.rotate();
    lab.maintain();
    lab.rotate();
    assert_eq!(lab.rotor.forget_sealed(SegmentId(1)), SealedDisposal::Recycled);
    lab.disk.remove_file(&lab.dir.join(segment_file_name(SegmentId(1)))).expect("vanish");

    let (report, barrier) = lab.rotor.maintain_deferred(0).expect("maintain");
    assert_eq!(report.preallocated, Some(SegmentId(3)));
    assert!(barrier.is_some());
    assert_eq!(lab.rotor.stats().recycle_fallbacks, 1);
    assert_eq!(lab.rotor.stats().segments_recycled, 0);
    assert_eq!(lab.rotor.pooled(), Vec::<SegmentId>::new(), "the pooled entry is gone");
    assert!(lab.names().contains(&"seg-000003.ilog".to_owned()), "created fresh");
}

/// Goal: the residue a recycled file carries reads as foreign-segment
/// frames — the reader refuses it with the typed error at the same
/// offset, the slack scanner counts it apart from validating frames and
/// proves the region residue. Method: recycle seg 1 into seg 3 and read
/// seg 3 before anything of its new life is written.
#[test]
fn recycled_residue_reads_as_foreign_segment_frames_never_data() {
    let mut lab = Lab::new(1);
    lab.maintain();
    lab.rotate();
    lab.maintain();
    lab.rotate();
    assert_eq!(lab.rotor.forget_sealed(SegmentId(1)), SealedDisposal::Recycled);
    lab.maintain();
    assert_eq!(lab.rotor.next_ready(), Some(SegmentId(3)));

    let mut reader =
        SegmentReader::open(&lab.disk, &lab.dir, SegmentId(3), ReaderConfig::default())
            .expect("open");
    match reader.next_frame() {
        Err(ReadError::ForeignSegment { segment, offset, stored_segment }) => {
            assert_eq!(segment, SegmentId(3));
            assert_eq!(offset, 0);
            assert_eq!(stored_segment, SegmentId(1));
        }
        other => panic!("expected the foreign-segment refusal, got {other:?}"),
    }
    assert!(reader.next_frame().expect("terminal").is_none(), "the reader yields nothing after");

    let evidence =
        scan_region_evidence(&lab.disk, &lab.dir, SegmentId(3), 0, ReaderConfig::default())
            .expect("scan");
    assert_eq!(evidence.valid_frames, 0, "nothing self-locates in seg 3");
    assert!(evidence.foreign_frames > 1, "every previous-life frame is foreign");
    assert_eq!(evidence.max_foreign_epoch, 1);
    assert_eq!(evidence.max_covered_lsn, 0, "foreign frames attest nothing");
    assert!(evidence.is_recycled_residue());
    assert_eq!(
        u64::from(evidence.first_nonzero.expect("nonzero")),
        0,
        "residue starts at offset 0"
    );
    // A frame at a shifted offset is still garbage, never foreign: poke
    // a copy of the first residue frame three blocks later (its stored
    // offset says 0; the sim asserts direct writes stay block-aligned).
    let path = lab.dir.join(segment_file_name(SegmentId(3)));
    let image = lab.disk.contents(&path).expect("image");
    let shifted_at = u64::from(3 * FRAME_ALIGN);
    let mut file = lab.disk.open_write(&path).expect("open");
    file.write_at(shifted_at, &image[..FRAME_ALIGN as usize]).expect("poke");
    drop(file);
    let evidence =
        scan_region_evidence(&lab.disk, &lab.dir, SegmentId(3), 0, ReaderConfig::default())
            .expect("scan");
    assert_eq!(evidence.valid_frames, 0);
    assert!(evidence.is_recycled_residue(), "a shifted copy is garbage, not a frame");
}

/// Goal: a named fuzz-corpus seed for the foreign-segment shape (ADR-0090
/// D5: "the frame decoder target gains the foreign-segment shape") — a
/// self-located v3 frame followed by a CRC-valid frame stamped for
/// another segment at the same offset, the image random mutation almost
/// never reaches on its own. Method: writes `crates/inf-log/fuzz/corpora/
/// segment_read/foreign-segment-20260822` when `INF_WRITE_FUZZ_SEED=1`
/// (a no-op otherwise); the `segment_read` target asserts the shape.
#[test]
fn foreign_segment_fuzz_seed() {
    if std::env::var_os("INF_WRITE_FUZZ_SEED").is_none() {
        return;
    }
    let frame = |segment: SegmentId, block: u32, seq: u64| {
        let mut b = FrameBuilder::new();
        b.append(&record(1));
        let first =
            inf_log::Lsn::new(segment, block * FRAME_ALIGN + inf_log::FRAME_HEADER_LEN as u32);
        b.finalize(first, stamp(seq), FrameLayout::Aligned).to_vec()
    };
    let mut image = frame(SegmentId(0), 0, 1);
    image.extend(frame(SegmentId(7), 1, 9));
    image.extend(frame(SegmentId(3), 2, 4));
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/fuzz/corpora/segment_read");
    std::fs::create_dir_all(path).expect("corpora dir");
    std::fs::write(format!("{path}/foreign-segment-20260822"), &image).expect("seed");
}
