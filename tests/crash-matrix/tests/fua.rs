//! M4.5-S34 crash rows (`m4.toml`, ADR-0086 D8): power cuts around
//! write-through (FUA-class) frames on `Direct` segments, on the sim disk
//! (lose/tear/reorder physics — the MemFs tier's process-KILL physics
//! cannot express "a later write is durable, an earlier one is not").
//!
//! - `fua_in_flight` — the cut lands with a write-through frame issued
//!   but not executed: every frame whose ticket completed recovers
//!   exactly; the in-flight frame and everything after are absent; the
//!   reopened tail reads its pre-zeroed fact from the file and resumes at
//!   the aligned boundary.
//! - `seal_flush_x_fua` — a segment rotation's seal FLUSH is in flight
//!   (never completes) while a write-through frame in the new segment
//!   completes: the done-prefix rule keeps that frame **un-acked** (the
//!   ledger watermark never passes the seal), and recovery admits either
//!   outcome for it while every acked key recovers exactly — the
//!   ack-before-durable bug the prefix rule (ADR-0086 D2.5) forbids is
//!   the thing this row would catch.
//!
//! M4.5-S35 rows (ADR-0087 D6/D7):
//!
//! - `seal_flush_x_fua_plain_tail` — the shape ADR-0086 recorded as a
//!   limitation, made deterministic: segment N ends in barrier-less
//!   `everysec` frames (volatile), rotates (seal FLUSH pending, never
//!   executes), a write-through frame lands durable in N + 1, then the
//!   cut. On the seeds whose cut eats N's tail, recovery finds a hole in
//!   a *sealed* segment with a validating frame beyond it; the stamp
//!   evidence proves it un-covered and boot truncates at the hole instead
//!   of refusing (the old unconditional sealed-slack refusal).
//! - `pipeline_later_durable_earlier_torn` — K = 4 write-through frames
//!   in flight; only the third executes before the cut. Nothing in the
//!   four was acked (the done-prefix), replay ends at the first frame's
//!   base, the third is discarded as an un-attested beyond-frame.
//!
//! Both rows drive the rotor + staging ring + commit ledger exactly as
//! the plane does (the `inf-log/tests/support` choreography), executing
//! each op against the disk by hand so the cut can land at a chosen
//! point. Seeds vary the cut's sector coin; the per-key oracle is the
//! sweep's: acked ⇒ exact, un-acked ⇒ absent or exact.

use std::collections::BTreeMap;
use std::path::Path;

use crash_matrix::{CELL, NS, anchor, config, fresh_keyspace, load_matrix, now};
use inf_foundation::rng::{Entropy, SplitMix64};
use inf_foundation::time::Nanos;
use inf_log::fs::sim::SimDisk;
use inf_log::fs::{SegmentFs, SegmentIoMode};
use inf_log::{
    FRAME_ALIGN, FsyncClass, GroupCommit, MutationEffect, SegmentConfig, SegmentRotor,
    StagingConfig, StagingRing, ZERO_FILL_SLICE_BYTES, create_cell_dirs,
};
use inf_log::{FrameId, FramePlan, Lsn};
use inf_server::open_cell_log;

const SEGMENT_BYTES: u32 = 64 << 10;

/// One op executed against the disk: what the plane would push.
enum Issued {
    /// Write-through frame: durable at execution.
    Through { id: FrameId, ticket: inf_log::FsyncTicket, offset: u64, bytes: Vec<u8> },
    /// Plain write + the linked FLUSH-class sync (executed together).
    Linked { id: FrameId, ticket: inf_log::FsyncTicket, offset: u64, bytes: Vec<u8> },
    /// Plain write, no barrier (the due accumulates behind an in-flight
    /// FLUSH-class entry — the ADR-0022 D3 discipline).
    Plain { id: FrameId, offset: u64, bytes: Vec<u8> },
}

struct Rig {
    disk: SimDisk,
    rotor: SegmentRotor<SimDisk>,
    ring: StagingRing,
    commit: GroupCommit<inf_log::fs::sim::SimFile>,
    /// key → (value, acked)
    model: BTreeMap<Vec<u8>, (Vec<u8>, bool)>,
    /// Frames queued: (exclusive end, keys in the frame).
    frame_keys: Vec<(u64, Vec<Vec<u8>>)>,
    seq: u32,
}

fn direct_cfg() -> SegmentConfig {
    SegmentConfig {
        segment_bytes: SEGMENT_BYTES,
        io_mode: SegmentIoMode::Direct,
        ..Default::default()
    }
}

impl Rig {
    /// A fresh Direct cell driven to its first pre-zeroed segment: the
    /// next segment zero-fills (unpaced, the active is sparse), the
    /// barrier lands, and the upgrade rotation runs on the first frame.
    fn new() -> Rig {
        let disk = SimDisk::new();
        let dirs = create_cell_dirs(&disk, Path::new("data/shard-0")).expect("dirs");
        let rotor = SegmentRotor::create_fresh_deferred(disk.clone(), dirs.log, direct_cfg())
            .expect("rotor");
        let mut rig = Rig {
            disk,
            rotor,
            ring: StagingRing::new(StagingConfig { capacity_bytes: 16 << 10, frames_in_flight: 4 }),
            commit: GroupCommit::new(),
            model: BTreeMap::new(),
            frame_keys: Vec::new(),
            seq: 0,
        };
        rig.prepare_next();
        rig
    }

    /// MAINTAIN's job: preallocate the next segment and pre-zero it to
    /// ready (dir names made durable directly — the dir barrier is not
    /// under test here).
    fn prepare_next(&mut self) {
        let (_, barrier) = self.rotor.maintain_deferred(0).expect("maintain");
        drop(barrier);
        self.disk.sync_dir(Path::new("data/shard-0/log")).expect("names");
        while let Some(slice) = self.rotor.next_zero_slice(ZERO_FILL_SLICE_BYTES) {
            let zeros = vec![0u8; slice.len as usize];
            self.disk.driver_write_at(slice.fd, slice.offset, &zeros).expect("zero");
            self.rotor.note_zero_slice_written();
        }
        let fd = self.rotor.take_zero_fill_barrier().expect("barrier owed");
        self.disk.driver_fdatasync(fd).expect("barrier");
        self.rotor.note_zero_fill_synced();
    }

    /// Stage `n` always-class SETs and seal them into one frame; returns
    /// the op to execute and, when rotation happened, the deferred seal's
    /// `(fd, ticket)` — registered in the ledger **before** the frame's
    /// own ticket, as the plane does.
    fn seal_frame(&mut self, n: u32) -> (Issued, Option<(i32, inf_log::FsyncTicket)>) {
        self.seal_frame_class(n, FsyncClass::Always)
    }

    /// [`seal_frame`](Self::seal_frame) for a chosen durability class.
    /// `Everysec` records are modeled as never acked-durable (the oracle
    /// admits either outcome for them; the rows cut well inside the loss
    /// window).
    fn seal_frame_class(
        &mut self,
        n: u32,
        class: FsyncClass,
    ) -> (Issued, Option<(i32, inf_log::FsyncTicket)>) {
        let mut keys = Vec::new();
        for _ in 0..n {
            self.seq += 1;
            let key = format!("k:{}", self.seq % 7).into_bytes();
            let value = format!("v:{}", self.seq).into_bytes();
            let effect = MutationEffect::StringSet { ns: NS, key: &key, value: &value };
            self.ring.stage(&effect).expect("fits");
            self.commit.note_staged(class);
            self.model.insert(key.clone(), (value, false));
            keys.push(key);
        }
        let frame_len = self.ring.pending_frame_len();
        // The plane's order (ADR-0087 D3): plan before the seal. These
        // rows execute every frame before sealing the next, so the
        // pipeline is always drained and `Wait` is unreachable.
        let rotation_due = self.rotor.rotation_due(frame_len);
        let write_through_ok = self.rotor.next_frame_write_through_ok(frame_len);
        let plan = self.commit.frame_plan(write_through_ok, rotation_due);
        assert_ne!(plan, FramePlan::Wait, "drained rig never waits");
        let (slot, handoff) = self.rotor.begin_frame_deferred(frame_len, 0).expect("reserve");
        assert_eq!(handoff.is_some(), rotation_due);
        assert_eq!(slot.write_through_ok(), write_through_ok);
        let seal = handoff.map(|h| {
            let fd = h.raw_fd().expect("sim fd");
            (fd, self.commit.register_seal_fsync(h, Nanos::ZERO))
        });
        let end = slot.base().advance(slot.len());
        let covered = self.commit.watermark().map_or(0, |l| l.to_u64());
        let lease = self.ring.seal(slot.first_record_lsn(), covered, slot.layout());
        let id = self.commit.note_frame_queued(end, slot.len());
        self.frame_keys.push((end.to_u64(), keys));
        let offset = u64::from(slot.base().offset);
        let bytes = self.ring.leased_frame(&lease).to_vec();
        let issued = match plan {
            FramePlan::WriteThrough => Issued::Through {
                id,
                ticket: self.commit.register_write_through(Nanos::ZERO),
                offset,
                bytes,
            },
            FramePlan::LinkedFsync => Issued::Linked {
                id,
                ticket: self.commit.register_linked_fsync(Nanos::ZERO),
                offset,
                bytes,
            },
            FramePlan::Plain => Issued::Plain { id, offset, bytes },
            FramePlan::Wait => unreachable!("asserted above"),
        };
        self.rotor.commit_frame_queued(slot);
        // The plane releases the lease at LogWritten; here the bytes are
        // copied out, so the lease can go now.
        self.ring.release(lease);
        (issued, seal)
    }

    /// Execute a deferred seal: the FLUSH lands and its ticket completes.
    fn execute_seal(&mut self, seal: (i32, inf_log::FsyncTicket)) {
        self.disk.driver_fdatasync(seal.0).expect("seal");
        self.complete(seal.1);
    }

    fn fd(&self) -> i32 {
        self.rotor.active_raw_fd().expect("sim fd")
    }

    /// Execute an issued op against the disk and account its completion.
    fn execute(&mut self, issued: Issued) {
        let fd = self.fd();
        match issued {
            Issued::Through { id, ticket, offset, bytes } => {
                self.disk.driver_write_through(fd, offset, &bytes).expect("through");
                self.commit.note_frame_written(id);
                self.complete(ticket);
            }
            Issued::Linked { id, ticket, offset, bytes } => {
                self.disk.driver_write_at(fd, offset, &bytes).expect("write");
                self.commit.note_frame_written(id);
                self.disk.driver_fdatasync(fd).expect("fsync");
                self.complete(ticket);
            }
            Issued::Plain { id, offset, bytes } => {
                self.disk.driver_write_at(fd, offset, &bytes).expect("write");
                self.commit.note_frame_written(id);
            }
        }
    }

    fn complete(&mut self, ticket: inf_log::FsyncTicket) {
        if let Some(end) = self.commit.on_fsync_complete(ticket, Nanos::from_micros(300)) {
            let watermark = end.to_u64();
            for (frame_end, keys) in &self.frame_keys {
                if *frame_end <= watermark {
                    for key in keys {
                        if let Some(entry) = self.model.get_mut(key) {
                            entry.1 = true;
                        }
                    }
                }
            }
        }
    }
}

/// Recover the cell from the cut image and check the per-key oracle.
fn recover_and_audit(
    disk: &SimDisk,
    model: &BTreeMap<Vec<u8>, (Vec<u8>, bool)>,
    context: &str,
) -> inf_server::RecoverStats {
    let mut ks = fresh_keyspace(FsyncClass::Always);
    let mut cfg = config(SEGMENT_BYTES);
    cfg.segment.io_mode = SegmentIoMode::Direct;
    let (rotor, stats, _) =
        open_cell_log(disk.clone(), &mut ks, CELL, &cfg, anchor(), now()).expect("recovers");
    assert_eq!(rotor.active_io_mode(), SegmentIoMode::Direct);
    assert!(
        rotor.active_written().is_multiple_of(FRAME_ALIGN),
        "{context}: resume cursor is aligned"
    );
    let store = ks.ns_store_mut(NS).expect("ns");
    for (key, (value, acked)) in model {
        let got = store.get(key, now()).map(<[u8]>::to_vec);
        if *acked {
            assert_eq!(
                got.as_deref(),
                Some(value.as_slice()),
                "{context}: ACKED key {:?} must recover exactly (stats {stats:?})",
                String::from_utf8_lossy(key)
            );
        } else if let Some(got) = got {
            // Un-acked: absent, or exactly one of the values written to
            // it (the newest durable one). The model holds the newest.
            assert!(
                got.starts_with(b"v:"),
                "{context}: un-acked key {:?} holds foreign bytes",
                String::from_utf8_lossy(key)
            );
        }
    }
    stats
}

/// `fua_in_flight`: frames 1..=k execute write-through; frame k+1 is
/// issued but the cut lands before the device saw it.
#[test]
fn fua_in_flight_loses_only_the_unacked_frame() {
    for seed in 0..24u64 {
        let mut rng = SplitMix64::new(seed ^ 0xF0A);
        let mut rig = Rig::new();
        // The first frame is the upgrade rotation's (segment 0 → 1): its
        // seal lands first, then the write-through frame.
        let (first, seal) = rig.seal_frame(2);
        rig.execute_seal(seal.expect("upgrade rotation"));
        assert!(matches!(first, Issued::Through { .. }), "pre-zeroed segment: write-through");
        rig.execute(first);
        rig.prepare_next();
        let done = 2 + rng.next_below(4);
        for _ in 0..done {
            let (op, h) = rig.seal_frame(1 + rng.next_below(3) as u32);
            assert!(h.is_none(), "64 KiB segment holds these frames");
            rig.execute(op);
        }
        let acked_before = rig.model.values().filter(|(_, a)| *a).count();
        assert!(acked_before > 0);
        // In flight: sealed, registered, never executed.
        let (_in_flight, h) = rig.seal_frame(2);
        assert!(h.is_none());
        let model = rig.model.clone();
        let disk = rig.disk.clone();
        drop(rig);
        disk.power_cut(seed);
        recover_and_audit(&disk, &model, &format!("fua_in_flight seed {seed}"));
    }
}

/// `seal_flush_x_fua`: the active segment fills, rotation hands off a
/// seal FLUSH that never executes, and a write-through frame in the new
/// segment completes — un-acked by the done-prefix rule — then the cut.
#[test]
fn seal_flush_in_flight_keeps_later_fua_frames_unacked() {
    for seed in 0..24u64 {
        let mut rig = Rig::new();
        let (first, seal) = rig.seal_frame(1);
        rig.execute_seal(seal.expect("upgrade rotation"));
        rig.execute(first);
        rig.prepare_next();
        // Fill segment 1 with 4 KiB frames until rotation is due.
        let mut crossed = false;
        for _ in 0..64 {
            let (op, seal) = rig.seal_frame(1);
            if seal.is_some() {
                // Rotation onto segment 2: the seal of segment 1 is in
                // the ledger ahead of this frame's ticket but never
                // executes — the FLUSH unit is "busy". The frame lands
                // write-through in segment 2 and completes — behind the
                // seal, so nothing it carries is acked.
                let acked_before = rig.model.values().filter(|(_, a)| *a).count();
                assert!(matches!(op, Issued::Through { .. }));
                rig.execute(op);
                let acked_after = rig.model.values().filter(|(_, a)| *a).count();
                assert_eq!(acked_after, acked_before, "the done-prefix holds acks behind the seal");
                crossed = true;
                break;
            }
            rig.execute(op);
        }
        assert!(crossed, "segment 1 rotated within 64 frames");
        let model = rig.model.clone();
        let disk = rig.disk.clone();
        drop(rig);
        disk.power_cut(seed ^ 0x5EA1);
        recover_and_audit(&disk, &model, &format!("seal_flush_x_fua seed {seed}"));
    }
}

/// The rows are declared in the matrix (self-policing).
#[test]
fn s34_rows_are_carried_here() {
    let def = load_matrix(&Path::new(env!("CARGO_MANIFEST_DIR")).join("m4.toml"));
    for expect in [
        "fua-in-flight",
        "seal-flush-x-fua",
        "seal-flush-x-fua-plain-tail",
        "pipeline-later-durable-earlier-torn",
    ] {
        assert!(
            def.rows.iter().any(|r| r.test == "fua.rs" && r.expect == expect),
            "the {expect} row is declared"
        );
    }
}

/// `seal_flush_x_fua_plain_tail` (ADR-0087 D6): segment 1 fills with
/// write-through frames, ends in plain `everysec` frames (volatile), and
/// rotates; the seal FLUSH never executes while a write-through frame in
/// segment 2 does. Seeds whose cut eats the plain tail leave a hole in a
/// sealed segment with durable data beyond — boot must truncate there,
/// never refuse, and every acked key must recover exactly.
#[test]
fn plain_tail_behind_a_pending_seal_truncates_instead_of_refusing() {
    let mut holes = 0u32;
    for seed in 0..24u64 {
        let mut rig = Rig::new();
        let (first, seal) = rig.seal_frame(1);
        rig.execute_seal(seal.expect("upgrade rotation"));
        rig.execute(first);
        rig.prepare_next();
        // Write-through frames until exactly two 4 KiB slots remain.
        while rig.rotor.active_written() + 3 * 4096 <= SEGMENT_BYTES {
            let (op, seal) = rig.seal_frame(1);
            assert!(seal.is_none());
            rig.execute(op);
        }
        let watermark_before = rig.commit.watermark();
        // Two plain everysec frames close the segment (no sync due ⇒ no
        // barrier; the bytes sit in the volatile layer).
        for _ in 0..2 {
            let (op, seal) = rig.seal_frame_class(1, FsyncClass::Everysec);
            assert!(seal.is_none(), "still inside segment 1");
            assert!(matches!(op, Issued::Plain { .. }), "no sync due: plain");
            rig.execute(op);
        }
        // The next always frame rotates: the seal of segment 1 is pending
        // (never executes); the frame goes write-through in segment 2
        // (the pending seal covers the gap — the prefix rule) and lands.
        let (op, seal) = rig.seal_frame(1);
        assert!(seal.is_some(), "rotation onto segment 2");
        assert!(matches!(op, Issued::Through { .. }));
        rig.execute(op);
        assert_eq!(
            rig.commit.watermark(),
            watermark_before,
            "the pending seal holds the watermark"
        );
        let model = rig.model.clone();
        let disk = rig.disk.clone();
        drop(rig);
        disk.power_cut(seed ^ 0x91A1);
        let stats =
            recover_and_audit(&disk, &model, &format!("seal_flush_x_fua_plain_tail seed {seed}"));
        if let Some(at) = stats.torn_truncated_at
            && at.segment.0 == 1
        {
            // The cut ate (some of) the plain tail: the hole sits in the
            // sealed segment 1, the segment-2 frame was discarded as an
            // un-attested beyond-frame, and boot continued.
            assert!(stats.beyond_frames_discarded >= 1, "seed {seed}: the later frame is counted");
            assert!(stats.torn_segments_removed >= 1, "seed {seed}: segment 2 removed");
            holes += 1;
        }
    }
    assert!(holes > 0, "some seed must cut the plain tail, or the row proves nothing");
}

/// `pipeline_later_durable_earlier_torn` (ADR-0087 D7): four write-through
/// frames in flight, the third lands, the cut eats the rest. Nothing in
/// the four was acked; replay ends at the first frame's base and the third
/// is an un-attested beyond-frame, discarded.
#[test]
fn later_frame_durable_earlier_torn_acks_nothing_and_truncates_at_the_first() {
    for seed in 0..24u64 {
        let mut rng = SplitMix64::new(seed ^ 0x4E4);
        let mut rig = Rig::new();
        let (first, seal) = rig.seal_frame(1);
        rig.execute_seal(seal.expect("upgrade rotation"));
        rig.execute(first);
        rig.prepare_next();
        let warm = 1 + rng.next_below(3);
        for _ in 0..warm {
            let (op, _) = rig.seal_frame(1 + rng.next_below(2) as u32);
            rig.execute(op);
        }
        let watermark_before = rig.commit.watermark();
        let first_base = Lsn::new(rig.rotor.active_segment(), rig.rotor.active_written());
        // Four frames sealed back to back: each extends the durable
        // prefix through the pending write-through tickets before it, so
        // every one is write-through (the prefix rule with K in flight).
        let mut issued = Vec::new();
        for _ in 0..4 {
            let (op, seal) = rig.seal_frame(1 + rng.next_below(3) as u32);
            assert!(seal.is_none(), "64 KiB segment holds four frames");
            assert!(matches!(op, Issued::Through { .. }), "pipelined write-through");
            issued.push(op);
        }
        assert_eq!(rig.commit.write_through_in_flight(), 4);
        // Only the third lands (and completes its ticket) before the cut.
        let third = issued.remove(2);
        rig.execute(third);
        assert_eq!(
            rig.commit.watermark(),
            watermark_before,
            "a later frame completing first acks nothing"
        );
        assert_eq!(rig.commit.write_through_in_flight(), 3);
        let model = rig.model.clone();
        let disk = rig.disk.clone();
        drop(rig);
        disk.power_cut(seed ^ 0x7E4A);
        let stats = recover_and_audit(
            &disk,
            &model,
            &format!("pipeline_later_durable_earlier_torn seed {seed}"),
        );
        assert_eq!(stats.torn_truncated_at, Some(first_base), "seed {seed}: resume at frame 1");
        assert_eq!(stats.beyond_frames_discarded, 1, "seed {seed}: the third frame, discarded");
    }
}
