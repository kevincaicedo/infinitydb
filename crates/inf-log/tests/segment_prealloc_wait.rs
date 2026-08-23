//! M4.5-S39b, ADR-0090 D9 (A8): the bounded pool wait at the rotor — with
//! recycling on and the pool empty at rotation, the MAINTAIN prealloc
//! re-checks the pool each slice until the active segment reaches the
//! bound, then falls back to a fresh segment whose zero-fill is paced
//! from its own origin. Every eligibility rule, the once-per-generation
//! miss, the ENOSPC headroom and the pool-arrival-after-expiry case are
//! pinned here. Every test states its goal and method in its first
//! sentence.

use std::path::PathBuf;

use inf_foundation::fault::{self, FaultSpec};
use inf_log::fs::sim::SimDisk;
use inf_log::fs::{SegmentFile, SegmentFs, SegmentIoMode};
use inf_log::{
    FRAME_ALIGN, FrameBuilder, FrameLayout, FrameStamp, Lsn, NsId, PoolWaitBound, PreallocPolicy,
    RecordView, SealedDisposal, SegmentConfig, SegmentId, SegmentRotor, ZERO_FILL_HEAD_START,
    ZERO_FILL_SLICE_BYTES,
};

/// 64 KiB segments: the quarter bound is four aligned frames, the eighth
/// two — wide enough that a wait can end either way.
const SEGMENT_BYTES: u32 = 64 << 10;
const QUARTER: u32 = SEGMENT_BYTES / 4;

fn stamp(seq: u64) -> FrameStamp {
    FrameStamp { epoch: 1, seq, covered_lsn: 0 }
}

fn record() -> RecordView<'static> {
    static VALUE: [u8; 200] = [0x5A; 200];
    RecordView::StringPostImage { ns: NsId(1), key: b"key", value: &VALUE }
}

fn quarter() -> PreallocPolicy {
    PreallocPolicy::WaitForPool { bound: PoolWaitBound::Quarter }
}

fn cfg(prealloc: PreallocPolicy) -> SegmentConfig {
    SegmentConfig {
        segment_bytes: SEGMENT_BYTES,
        io_mode: SegmentIoMode::Direct,
        recycle_slots: 1,
        prealloc,
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
    fn new(prealloc: PreallocPolicy) -> Lab {
        let disk = SimDisk::new();
        let dir = PathBuf::from("log");
        disk.create_dir_all(&dir).expect("dir");
        let rotor = SegmentRotor::create_fresh_deferred(disk.clone(), dir.clone(), cfg(prealloc))
            .expect("fresh");
        Lab { disk, dir, rotor, seq: 0 }
    }

    /// One MAINTAIN slice the way the plane runs it: the prealloc (its
    /// barrier synced at once) and then the zero-fill as far as pacing
    /// allows. Returns whether a segment was preallocated.
    fn maintain(&mut self) -> bool {
        let (report, barrier) = self.rotor.maintain_deferred(0).expect("maintain");
        if let Some(barrier) = barrier {
            let fd = barrier.dir.raw_fd().expect("sim dir fd");
            self.disk.driver_fdatasync(fd).expect("dir barrier");
        }
        self.fill();
        report.preallocated.is_some()
    }

    /// Drive the zero-fill as far as the pacing allows right now.
    fn fill(&mut self) -> u32 {
        let mut issued = 0;
        while let Some(slice) = self.rotor.next_zero_slice(ZERO_FILL_SLICE_BYTES) {
            let zeros = vec![0u8; slice.len as usize];
            self.disk.driver_write_at(slice.fd, slice.offset, &zeros).expect("zero write");
            self.rotor.note_zero_slice_written();
            issued += slice.len;
        }
        if let Some(fd) = self.rotor.take_zero_fill_barrier() {
            self.disk.driver_fdatasync(fd).expect("barrier");
            self.rotor.note_zero_fill_synced();
        }
        issued
    }

    /// One aligned frame written write-through; returns whether it rotated.
    fn frame(&mut self) -> bool {
        let mut b = FrameBuilder::new();
        b.append(&record());
        let (slot, handoff) = self.rotor.begin_frame_deferred(b.frame_len(), 0).expect("reserve");
        self.seq += 1;
        let bytes = b.finalize(slot.first_record_lsn(), stamp(self.seq), FrameLayout::Aligned);
        let fd = self.rotor.active_raw_fd().expect("fd");
        self.disk.driver_write_through(fd, u64::from(slot.base().offset), bytes).expect("frame");
        self.rotor.commit_frame_queued(slot);
        handoff.is_some()
    }

    /// Frames until the active segment holds `bytes` (one frame per 4 KiB),
    /// the paced fill driven between frames as the plane's MAINTAIN does.
    fn write_to(&mut self, bytes: u32) {
        while self.rotor.active_written() < bytes {
            assert!(!self.frame(), "no rotation expected below {bytes}");
            self.fill();
        }
    }

    /// Fill the active segment until it rotates onto the ready next one,
    /// the paced fill driven between frames.
    fn rotate(&mut self) -> SegmentId {
        let before = self.rotor.active_segment();
        while !self.frame() {
            self.fill();
        }
        assert_ne!(self.rotor.active_segment(), before);
        self.rotor.active_segment()
    }

    /// Boot → seg 1 pre-zeroed → upgrade rotation → seg 2 (the first
    /// generation, immediate) → rotate onto it. Leaves the rotor with a
    /// pre-zeroed active seg 2 at one frame, `next` absent, seg 1 sealed
    /// pre-zeroed and nothing pooled — the state every wait starts from.
    fn warm(&mut self) {
        assert!(self.maintain(), "boot: seg 1 at once");
        assert_eq!(self.rotate(), SegmentId(1));
        assert!(self.maintain(), "first generation after the upgrade: seg 2 at once");
        assert_eq!(self.rotate(), SegmentId(2));
        assert!(self.rotor.active_write_through());
        assert_eq!(self.rotor.active_written(), FRAME_ALIGN, "the rotating frame");
        assert_eq!(self.rotor.next_ready(), None);
    }

    fn waits(&self) -> (u64, u64, u64) {
        let s = self.rotor.stats();
        (s.recycle_waits_started, s.recycle_waits_satisfied, s.recycle_waits_expired)
    }
}

/// Goal: a wait the pool feeds before the bound ends `satisfied` and the
/// recycled segment is the next one — no zero-fill, no miss. Method:
/// warm, maintain (the wait starts), write to half the bound, pool seg 1,
/// maintain.
#[test]
fn pool_arrival_before_the_bound_satisfies_the_wait_with_the_recycled_segment() {
    let mut lab = Lab::new(quarter());
    lab.warm();
    let zero_fill_before = lab.rotor.stats().zero_fill_bytes;
    let misses_before = lab.rotor.stats().recycle_misses;
    assert!(!lab.maintain(), "the pool is empty: the prealloc waits");
    assert_eq!(lab.waits(), (1, 0, 0));
    assert_eq!(lab.rotor.next_ready(), None);
    lab.write_to(QUARTER / 2);
    assert!(!lab.maintain(), "still waiting below the bound");
    assert_eq!(lab.waits(), (1, 0, 0), "one wait per generation, not one per slice");
    assert_eq!(lab.rotor.forget_sealed(SegmentId(1)), SealedDisposal::Recycled);
    assert!(lab.maintain(), "the pool fed the wait");
    assert_eq!(lab.waits(), (1, 1, 0));
    assert_eq!(lab.rotor.next_ready(), Some(SegmentId(3)));
    assert!(!lab.rotor.next_zero_filling(), "recycled: ready without a fill");
    let stats = lab.rotor.stats();
    assert_eq!(stats.segments_recycled, 1);
    assert_eq!(stats.recycle_misses, misses_before, "a satisfied wait is not a miss");
    assert_eq!(stats.zero_fill_bytes, zero_fill_before, "the second write was not paid");
    assert_eq!(stats.recycle_wait_active_bytes_max, u64::from(QUARTER / 2));
    assert_eq!(lab.rotate(), SegmentId(3));
    assert_eq!(lab.rotor.stats().rotations_unzeroed, 0);
}

/// Goal: a pool that stays empty expires the wait at the bound into
/// exactly one fresh fallback, counted as one miss however many slices
/// ran. Method: warm, maintain past the bound with many slices, count.
#[test]
fn an_empty_pool_expires_the_wait_into_exactly_one_fresh_fallback_at_the_bound() {
    let mut lab = Lab::new(quarter());
    lab.warm();
    let misses_before = lab.rotor.stats().recycle_misses;
    let preallocs_before = lab.rotor.stats().preallocs;
    let mut slices = 0;
    while lab.rotor.active_written() < QUARTER - FRAME_ALIGN {
        assert!(!lab.maintain(), "waiting");
        slices += 1;
        assert!(!lab.frame());
    }
    assert!(slices >= 2, "several slices ran while waiting: {slices}");
    assert!(!lab.maintain(), "one frame short of the bound: still waiting");
    assert!(!lab.frame());
    assert_eq!(lab.rotor.active_written(), QUARTER, "at the bound");
    assert!(lab.maintain(), "expired: the fresh fallback");
    assert_eq!(lab.waits(), (1, 0, 1));
    let stats = lab.rotor.stats();
    assert_eq!(stats.recycle_misses, misses_before + 1, "one miss per expired wait");
    assert_eq!(stats.preallocs, preallocs_before + 1, "exactly one fallback");
    assert_eq!(stats.recycle_wait_active_bytes_max, u64::from(QUARTER));
    assert_eq!(lab.rotor.next_ready(), Some(SegmentId(3)));
    assert!(!lab.maintain(), "a next exists: no second prealloc, no second wait");
    assert_eq!(lab.waits(), (1, 0, 1));
}

/// Goal: the fallback's zero-fill is paced from its own origin — the
/// 16 MiB head start is never exceeded as a burst and the fill completes
/// before the segment is needed (`rotations_unzeroed` stays 0). Method: a
/// 32 MiB segment (head start < segment) so the pacing is observable;
/// expire the wait, measure the slices allowed at the origin, then at
/// origin + 4 MiB, then drive the fill to completion.
#[test]
fn the_fallbacks_zero_fill_is_paced_from_its_origin_and_completes_before_rotation() {
    let segment_bytes: u32 = 32 << 20;
    let disk = SimDisk::new();
    let dir = PathBuf::from("log");
    disk.create_dir_all(&dir).expect("dir");
    let cfg = SegmentConfig { segment_bytes, ..cfg(quarter()) };
    let rotor = SegmentRotor::create_fresh_deferred(disk.clone(), dir.clone(), cfg).expect("fresh");
    let mut lab = Lab { disk, dir, rotor, seq: 0 };
    assert!(lab.maintain());
    assert_eq!(lab.rotate(), SegmentId(1));
    assert!(lab.maintain());
    assert_eq!(lab.rotate(), SegmentId(2));
    assert!(lab.rotor.active_write_through());
    assert!(!lab.maintain(), "waiting");
    while lab.rotor.active_written() < segment_bytes / 4 {
        assert!(!lab.frame());
    }
    let origin = lab.rotor.active_written();
    let (report, barrier) = lab.rotor.maintain_deferred(0).expect("maintain");
    assert_eq!(report.preallocated, Some(SegmentId(3)));
    let fd = barrier.expect("barrier").dir.raw_fd().expect("fd");
    lab.disk.driver_fdatasync(fd).expect("dir barrier");
    let bound = |since_origin: u32| {
        let allowed = 2 * since_origin + ZERO_FILL_HEAD_START;
        allowed.div_ceil(ZERO_FILL_SLICE_BYTES) * ZERO_FILL_SLICE_BYTES
    };
    let issued = lab.fill();
    assert_eq!(issued, bound(0), "the head start only — no catch-up burst at the origin");
    while lab.rotor.active_written() < origin + (4 << 20) {
        assert!(!lab.frame());
    }
    let total = issued + lab.fill();
    assert_eq!(total, bound(4 << 20), "2× gain from the origin, not from the segment start");
    // The fill completes by origin + segment/2 − 8 MiB, before the end.
    while lab.rotor.next_zero_filling() {
        assert!(!lab.frame());
        lab.fill();
    }
    assert!(lab.rotor.active_written() <= origin + segment_bytes / 2 - (8 << 20));
    assert_eq!(lab.rotate(), SegmentId(3));
    assert_eq!(lab.rotor.stats().rotations_unzeroed, 0);
    assert!(lab.rotor.active_write_through());
}

/// Goal: a first boot never waits — the first generation preallocates at
/// once (rotations = 0), and a fresh cell's convergence is not delayed.
/// Method: a fresh rotor, maintain once.
#[test]
fn the_first_generation_after_boot_preallocates_at_once() {
    let mut lab = Lab::new(quarter());
    assert!(lab.maintain(), "segment 0 active, rotations 0: no wait");
    assert_eq!(lab.waits(), (0, 0, 0));
    assert_eq!(lab.rotor.stats().recycle_misses, 1, "an ordinary empty-pool miss");
}

/// Goal: the generation right after the first rotation waits — and a
/// reopened pre-zeroed tail after a crash behaves like a fresh boot
/// (rotations 0 ⇒ immediate), then waits from its second generation.
/// Method: warm a rotor, scan and reopen the dir under the same config.
#[test]
fn a_recovered_prezeroed_tail_preallocates_at_once_then_waits_from_its_second_generation() {
    let mut lab = Lab::new(quarter());
    lab.warm();
    let tail_end = lab.rotor.active_written();
    let scan = inf_log::scan_log_dir(&lab.disk, &lab.dir).expect("scan");
    let rotor = SegmentRotor::open_existing(
        lab.disk.clone(),
        lab.dir.clone(),
        cfg(quarter()),
        &scan,
        tail_end,
    )
    .expect("reopen");
    let mut lab = Lab { disk: lab.disk.clone(), dir: lab.dir.clone(), rotor, seq: lab.seq };
    assert!(lab.rotor.active_write_through(), "the tail reads fully allocated");
    assert!(lab.maintain(), "rotations 0 this life: immediate");
    assert_eq!(lab.waits(), (0, 0, 0));
    assert_eq!(lab.rotate(), SegmentId(3));
    assert!(!lab.maintain(), "second generation of this life: waits");
    assert_eq!(lab.waits(), (1, 0, 0));
}

/// Goal: a packed tail reopened `Buffered` under a `Direct` rotor never
/// waits (the active segment is not pre-zeroed) — FLUSH→FUA convergence
/// is not delayed. Method: a Buffered life, reopen under the Direct
/// config with the wait, maintain.
#[test]
fn a_packed_tail_reopened_buffered_never_waits() {
    let disk = SimDisk::new();
    let dir = PathBuf::from("log");
    disk.create_dir_all(&dir).expect("dir");
    let buffered = SegmentConfig { io_mode: SegmentIoMode::Buffered, ..cfg(quarter()) };
    let packed_end = {
        let mut rotor =
            SegmentRotor::create_fresh(disk.clone(), dir.clone(), buffered).expect("fresh");
        let mut b = FrameBuilder::new();
        b.append(&record());
        let slot = rotor.begin_frame(b.frame_len(), 0).expect("reserve");
        let bytes = b.finalize(slot.first_record_lsn(), stamp(1), FrameLayout::Packed);
        rotor.commit_frame(slot, bytes).expect("commit");
        rotor.active_written()
    };
    assert!(!packed_end.is_multiple_of(FRAME_ALIGN));
    let scan = inf_log::scan_log_dir(&disk, &dir).expect("scan");
    let rotor =
        SegmentRotor::open_existing(disk.clone(), dir.clone(), cfg(quarter()), &scan, packed_end)
            .expect("reopen");
    let mut lab = Lab { disk, dir, rotor, seq: 1 };
    assert_eq!(lab.rotor.active_io_mode(), SegmentIoMode::Buffered);
    assert!(lab.maintain(), "not pre-zeroed: the next segment is created at once");
    assert_eq!(lab.waits(), (0, 0, 0));
    // The upgrade rotation seals the packed segment 0 (never a pool
    // candidate), so the generation after it cannot be fed either and
    // preallocates at once; the one after *that* waits.
    let mut b = FrameBuilder::new();
    b.append(&record());
    let (slot, handoff) = lab.rotor.begin_frame_deferred(b.frame_len(), 0).expect("reserve");
    assert!(handoff.is_some(), "upgrade rotation");
    assert_eq!(slot.base(), Lsn::new(SegmentId(1), 0));
    lab.rotor.commit_frame_queued(slot);
    assert!(lab.maintain(), "nothing pre-zeroed is sealed: immediate");
    assert_eq!(lab.waits(), (0, 0, 0));
    assert_eq!(lab.rotate(), SegmentId(2));
    assert!(!lab.maintain(), "seg 1 sealed pre-zeroed: this generation waits");
    assert_eq!(lab.waits(), (1, 0, 0));
}

/// Goal: a time-sealing rotor never waits — a time seal at low occupancy
/// would otherwise find no next segment and pay an inline prealloc.
/// Method: the quarter policy with `seal_after_ms` set, warm, maintain.
#[test]
fn time_sealing_rotors_never_wait() {
    let disk = SimDisk::new();
    let dir = PathBuf::from("log");
    disk.create_dir_all(&dir).expect("dir");
    let timed = SegmentConfig { seal_after_ms: Some(1_000), ..cfg(quarter()) };
    let rotor =
        SegmentRotor::create_fresh_deferred(disk.clone(), dir.clone(), timed).expect("fresh");
    let mut lab = Lab { disk, dir, rotor, seq: 0 };
    lab.warm();
    assert!(lab.maintain(), "time-sealed: immediate");
    assert_eq!(lab.waits(), (0, 0, 0));
    assert_eq!(lab.rotor.stats().inline_preallocs, 0);
}

/// Goal: `--recycle-wait off` is the pre-D9 rotor — no wait is ever
/// started and every generation preallocates at rotation. Method: the
/// Immediate policy through three generations.
#[test]
fn the_immediate_policy_never_waits() {
    let mut lab = Lab::new(PreallocPolicy::Immediate);
    lab.warm();
    assert!(lab.maintain(), "immediate");
    assert_eq!(lab.rotate(), SegmentId(3));
    assert!(lab.maintain(), "immediate");
    assert_eq!(lab.waits(), (0, 0, 0));
    assert_eq!(lab.rotor.stats().recycle_misses, 4, "every generation missed the empty pool");
}

/// Goal: a pool arrival after the fallback was created is kept for the
/// following generation — never a second `next`. Method: expire a wait,
/// pool seg 1, maintain (nothing), rotate, maintain (recycled, no wait).
#[test]
fn a_pool_arrival_after_expiry_serves_the_following_generation() {
    let mut lab = Lab::new(quarter());
    lab.warm();
    assert!(!lab.maintain());
    lab.write_to(QUARTER);
    assert!(lab.maintain(), "expired into seg 3, fresh");
    assert_eq!(lab.rotor.forget_sealed(SegmentId(1)), SealedDisposal::Recycled);
    assert!(!lab.maintain(), "a next exists: the pooled segment stays pooled");
    assert_eq!(lab.rotor.pooled(), vec![SegmentId(1)]);
    assert_eq!(lab.rotate(), SegmentId(3));
    assert!(lab.maintain(), "the pool is not empty: no wait, the rename");
    assert_eq!(lab.rotor.next_ready(), Some(SegmentId(4)));
    assert_eq!(lab.rotor.stats().segments_recycled, 1);
    assert_eq!(lab.waits(), (1, 0, 1), "no second wait for a generation the pool can serve");
}

/// Goal: ENOSPC on the fallback is discovered at the bound with the rest
/// of the segment as admission headroom, the retry never waits or misses
/// twice, and space returning clears the exhaustion. Method: arm the
/// prealloc fault from the fallback on, expire the wait, retry, disarm.
#[test]
fn enospc_on_the_fallback_is_found_at_the_bound_and_retries_without_a_second_wait() {
    fault::disarm_all();
    let mut lab = Lab::new(quarter());
    lab.warm();
    assert!(!lab.maintain());
    lab.write_to(QUARTER);
    fault::arm(inf_log::fault::PREALLOC_NO_SPACE, FaultSpec::FromNth(1));
    let (report, barrier) = lab.rotor.maintain_deferred(0).expect("NoSpace is not a hard error");
    assert!(report.prealloc_failed);
    assert!(barrier.is_none());
    assert!(lab.rotor.space_exhausted(), "surfaced with 3/4 of the segment still free");
    assert_eq!(lab.rotor.active_written(), QUARTER);
    let misses = lab.rotor.stats().recycle_misses;
    assert_eq!(lab.waits(), (1, 0, 1));
    // Retries: no new wait, no new miss, still exhausted.
    for _ in 0..3 {
        assert!(!lab.frame());
        let (report, _) = lab.rotor.maintain_deferred(0).expect("retry");
        assert!(report.prealloc_failed);
    }
    assert_eq!(lab.waits(), (1, 0, 1));
    assert_eq!(lab.rotor.stats().recycle_misses, misses, "the miss was counted once");
    assert_eq!(lab.rotor.stats().prealloc_failures, 4);
    fault::disarm_all();
    assert!(lab.maintain(), "space returned");
    assert!(!lab.rotor.space_exhausted());
    assert_eq!(lab.rotor.next_ready(), Some(SegmentId(3)));
    assert_eq!(lab.rotor.stats().recycle_misses, misses);
}

/// Goal: the eighth bound expires at `segment_bytes / 8`. Method: the
/// Eighth policy, write to one frame short of it, then to it.
#[test]
fn the_eighth_bound_expires_at_an_eighth() {
    let mut lab = Lab::new(PreallocPolicy::WaitForPool { bound: PoolWaitBound::Eighth });
    lab.warm();
    assert!(!lab.maintain());
    lab.write_to(SEGMENT_BYTES / 8 - FRAME_ALIGN);
    assert!(!lab.maintain(), "one frame short");
    lab.write_to(SEGMENT_BYTES / 8);
    assert!(lab.maintain(), "expired at an eighth");
    assert_eq!(lab.waits(), (1, 0, 1));
    assert_eq!(PoolWaitBound::Eighth.bytes(SEGMENT_BYTES), SEGMENT_BYTES / 8);
    assert_eq!(PoolWaitBound::Quarter.bytes(SEGMENT_BYTES), QUARTER);
}

/// Goal: the policy's spellings round-trip (`--recycle-wait`). Method:
/// parse and display each.
#[test]
fn policy_spellings_round_trip() {
    for text in ["off", "quarter", "eighth"] {
        let policy = PreallocPolicy::parse(text).expect("spelling");
        assert_eq!(policy.to_string(), text);
    }
    assert_eq!(PreallocPolicy::parse("immediate"), Some(PreallocPolicy::Immediate));
    assert_eq!(PreallocPolicy::parse("half"), None);
    assert_eq!(PreallocPolicy::DEFAULT, quarter());
}
