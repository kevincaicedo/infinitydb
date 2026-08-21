//! M2-S05 integration: the reactor LOG step end-to-end — one `LogWrite` per
//! iteration, linked/seal/standalone fsync policy, deferred seal, everysec
//! virtual-time behavior (L7), and the linked-fsync fault contract — driven
//! through the real `CellLoop` with the scripted deterministic driver.
//! (The uring flavor of the fault contract lives in
//! `inf-runtime/tests/uring_file.rs`.)

mod support;

use std::path::Path;
use std::rc::Rc;

use inf_alloc::BufferPool;
use inf_foundation::time::{Clock, Nanos, VirtualClock};
use inf_log::fs::StdSegmentFs;
use inf_log::{
    FsyncClass, Lsn, ReaderConfig, SegmentConfig, SegmentId, SegmentReader, StagingConfig,
    SyncReason, scan_log_dir,
};
use inf_runtime::{CellLoop, LoopConfig, TokenClass};
use support::{DurablePlane, IoMode, ScriptedDriver, job_key, job_value, test_dir};

type TestLoop = CellLoop<ScriptedDriver, Rc<VirtualClock>>;

fn cell(
    dir: &Path,
    driver: ScriptedDriver,
    segment_bytes: u32,
) -> (TestLoop, DurablePlane, Rc<VirtualClock>) {
    let clock = Rc::new(VirtualClock::new(Nanos::ZERO));
    let lp = CellLoop::new(
        driver,
        Rc::clone(&clock),
        BufferPool::new(4, 1024),
        LoopConfig {
            spin_iters: 4,
            exec_budget: 1024,
            park_default: Some(std::time::Duration::from_millis(1)),
            remote_first_execute: false,
        },
    );
    let plane = DurablePlane::new(
        dir,
        StagingConfig { capacity_bytes: 64 << 10 },
        SegmentConfig { segment_bytes, ..Default::default() },
    );
    (lp, plane, clock)
}

/// Drive iterations until the plane quiesces (or `max` iterations pass),
/// asserting the per-iteration write tripwire the whole way.
fn run_until_quiesced(
    lp: &mut TestLoop,
    plane: &mut DurablePlane,
    clock: &Rc<VirtualClock>,
    step: Nanos,
    max: u64,
) -> u64 {
    let mut iters = 0;
    let mut idle = 0;
    while iters < max {
        lp.run_iteration(plane).expect("iteration");
        assert!(plane.writes_this_iter <= 1, "L3 tripwire: >1 log write in one iteration");
        clock.advance(step);
        iters += 1;
        let quiesced = plane.jobs.is_empty()
            && plane.staging.is_empty()
            && plane.in_flight.is_none()
            && plane.commit.pending_fsyncs() == 0;
        if plane.cell_failed {
            break;
        }
        // A few extra turns so gated futures observe their wakes.
        idle = if quiesced { idle + 1 } else { 0 };
        if idle > 4 {
            break;
        }
    }
    assert!(iters < max, "cell failed to quiesce in {max} iterations");
    iters
}

/// Replay every segment, returning (lsn, key, value) triples in log order.
fn replay(dir: &Path) -> Vec<(Lsn, Vec<u8>, Vec<u8>)> {
    let fs = StdSegmentFs;
    let log_dir = dir.join("log");
    let scan = scan_log_dir(&fs, &log_dir).expect("scan");
    let mut out = Vec::new();
    for &id in scan.segments() {
        let mut reader =
            SegmentReader::open(&fs, &log_dir, id, ReaderConfig::default()).expect("open");
        reader
            .apply_frames(|frame| {
                for record in frame.records() {
                    let (lsn, view) = record.expect("valid record");
                    match view {
                        inf_log::RecordView::StringPostImage { key, value, .. } => {
                            out.push((lsn, key.to_vec(), value.to_vec()));
                        }
                        other => panic!("unexpected record {other:?}"),
                    }
                }
                Ok::<(), std::convert::Infallible>(())
            })
            .expect("replay");
    }
    out
}

fn assert_replay_matches(plane: &DurablePlane, dir: &Path) {
    let replayed = replay(dir);
    assert_eq!(replayed.len(), plane.staged_log.len(), "record count");
    for (i, ((lsn, key, value), (seq, staged_lsn))) in
        replayed.iter().zip(&plane.staged_log).enumerate()
    {
        if Some(*lsn) != *staged_lsn || key != &job_key(*seq) {
            let lo = i.saturating_sub(3);
            eprintln!("first divergence at index {i}:");
            for (j, (rl, rk, _)) in
                replayed.iter().enumerate().take((i + 3).min(replayed.len())).skip(lo)
            {
                eprintln!(
                    "  [{j}] replayed {rl} {:?} | staged seq {} at {:?}",
                    String::from_utf8_lossy(rk),
                    plane.staged_log[j].0,
                    plane.staged_log[j].1,
                );
            }
        }
        assert_eq!(Some(*lsn), *staged_lsn, "LSN identity at index {i} (seq {seq})");
        assert_eq!(key, &job_key(*seq), "key identity for seq {seq}");
        assert_eq!(value, &job_value(*seq), "value identity for seq {seq}");
    }
}

// ---- S05: one write per iteration + policy resolution -----------------------

#[test]
fn mixed_load_one_write_per_iteration_replays_identically() {
    let dir = test_dir("mixed");
    // Completion delays of one iteration exercise the lease backpressure
    // (can_seal false while a write is in flight).
    let mut driver = ScriptedDriver::new(IoMode::Real);
    driver.delays = std::iter::repeat_n(1, 4096).collect();
    let (mut lp, mut plane, clock) = cell(&dir, driver, 1 << 20);
    plane.jobs_per_iter = 16;
    for chunk in 0..40 {
        let class = if chunk % 2 == 0 { FsyncClass::Always } else { FsyncClass::Everysec };
        plane.push_jobs(50, class);
    }

    run_until_quiesced(&mut lp, &mut plane, &clock, Nanos::from_millis(20), 100_000);
    // Trailing everysec bytes are covered by the NEXT 1 s tick (that lag is
    // the everysec loss-window contract, not a bug): keep ticking until the
    // standalone fsync lands.
    let mut drain = 0;
    while plane.commit.pending_log_bytes() > 0 {
        lp.run_iteration(&mut plane).expect("iteration");
        clock.advance(Nanos::from_millis(20));
        drain += 1;
        assert!(drain < 200, "everysec never covered the tail");
    }

    assert!(!plane.cell_failed, "unexpected I/O errors: {:?}", plane.io_errors);
    assert_eq!(plane.staged_records(), 2000, "every job staged");
    // Every always record acked, in LSN order, never before its fsync (the
    // in-future oracle assert already enforced the watermark condition).
    let acks = plane.acks.borrow();
    assert_eq!(acks.len(), 1000, "every always write acked");
    assert!(acks.windows(2).all(|w| w[0].0 < w[1].0), "acks FIFO by LSN");
    drop(acks);
    // The watermark ends covering everything that was queued.
    assert_eq!(plane.commit.watermark(), plane.commit.queued_up_to());
    assert_eq!(plane.commit.pending_log_bytes(), 0);
    assert_replay_matches(&plane, &dir);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn everysec_only_load_never_touches_the_gate() {
    let dir = test_dir("es-gate");
    let (mut lp, mut plane, clock) = cell(&dir, ScriptedDriver::new(IoMode::Real), 1 << 20);
    plane.push_jobs(200, FsyncClass::Everysec);
    let mut iters = 0;
    while !(plane.jobs.is_empty() && plane.staging.is_empty() && plane.in_flight.is_none()) {
        lp.run_iteration(&mut plane).expect("iteration");
        // The fast path pays no gate registration for everysec (S06 AC).
        assert_eq!(plane.gate.waiting(), 0, "everysec must not register gate waiters");
        clock.advance(Nanos::from_millis(20));
        iters += 1;
        assert!(iters < 10_000);
    }
    assert!(plane.acks.borrow().is_empty());
    std::fs::remove_dir_all(&dir).ok();
}

// ---- S05: fsync-on-seal (deferred) ------------------------------------------

#[test]
fn every_seal_gets_its_fsync_and_segments_replay() {
    let dir = test_dir("seal");
    // Tiny segments force many rotations; everysec off isolates seal syncs.
    let (mut lp, mut plane, clock) = cell(&dir, ScriptedDriver::new(IoMode::Real), 8 << 10);
    plane.everysec = false;
    plane.jobs_per_iter = 32;
    plane.push_jobs(3000, FsyncClass::Everysec);

    run_until_quiesced(&mut lp, &mut plane, &clock, Nanos::from_millis(5), 100_000);

    assert!(!plane.cell_failed);
    let rotations = plane.rotor.stats().rotations;
    assert!(rotations >= 10, "test needs real rotation pressure, got {rotations}");
    let seal_syncs =
        plane.fsync_submits.iter().filter(|(r, _)| *r == SyncReason::Seal).count() as u64;
    assert_eq!(seal_syncs, rotations, "fsync-on-seal observed at EVERY seal");
    assert_eq!(plane.rotor.stats().inline_preallocs, 0, "MAINTAIN kept the next segment ready");
    // The watermark reached at least the last sealed segment's end.
    let last_sealed = plane.rotor.sealed().last().copied().expect("sealed segments");
    assert!(plane.commit.watermark().expect("watermark") >= Lsn::new(last_sealed, 0));
    assert_replay_matches(&plane, &dir);
    std::fs::remove_dir_all(&dir).ok();
}

// ---- S05: everysec on virtual time (DST-tier, L7) ---------------------------

fn everysec_span(hours: u64) {
    let dir = test_dir(&format!("es-{hours}h"));
    let (mut lp, mut plane, clock) = cell(&dir, ScriptedDriver::new(IoMode::Recorded), 1 << 30);
    plane.jobs_per_iter = 2;
    let step = Nanos::from_millis(200);
    let busy_iters = hours * 3600 * 1000 / step.as_millis();
    // Continuous everysec load for `hours`, then a fully idle hour.
    plane.push_jobs(busy_iters * 2, FsyncClass::Everysec);
    for _ in 0..busy_iters {
        lp.run_iteration(&mut plane).expect("iteration");
        clock.advance(step);
    }
    let busy_end = clock.now();
    let idle_iters = 3600 * 1000 / step.as_millis();
    plane.jobs.clear();
    for _ in 0..idle_iters {
        lp.run_iteration(&mut plane).expect("iteration");
        clock.advance(step);
    }

    assert!(!plane.cell_failed);
    let ticks: Vec<Nanos> = plane
        .fsync_submits
        .iter()
        .filter(|(r, _)| *r != SyncReason::Seal)
        .map(|(_, t)| *t)
        .collect();
    // Interval discipline: every timer-driven fsync fires ≥ 1 s after the
    // previous and within tick tolerance (one loop step) of its deadline.
    for pair in ticks.windows(2) {
        let gap = pair[1].saturating_sub(pair[0]);
        assert!(gap >= Nanos::from_secs(1), "everysec fired early: {gap}");
        assert!(
            gap <= Nanos::from_secs(1) + step + step,
            "everysec missed its tick tolerance: {gap}"
        );
    }
    // Policy ceiling: at most one timer-driven fsync per second overall.
    let elapsed_s = hours * 3600;
    assert!(ticks.len() as u64 <= elapsed_s + 2, "fsyncs/s over the everysec ceiling");
    assert!(ticks.len() as u64 >= elapsed_s * 4 / 5, "everysec starved: {} ticks", ticks.len());
    // Idle hour: at most one covering sync right after load stops, then
    // silence — idle ticks are free (counted, no I/O).
    let late: Vec<&Nanos> = ticks.iter().filter(|t| **t > busy_end + Nanos::from_secs(2)).collect();
    assert!(late.is_empty(), "everysec fsyncs during the idle hour: {late:?}");
    assert!(plane.commit.stats().idle_ticks >= 3500, "idle ticks not counted as free");
    std::fs::remove_dir_all(&dir).ok();
}

/// The full 24 h AC row (M2-S05): virtual time compresses the day to ~7 s
/// wall-clock (L7), so this runs per-PR, not nightly.
#[test]
fn everysec_fires_within_tolerance_24h_virtual() {
    everysec_span(24);
}

// ---- S05: linked-fsync fault contract (scripted tier) ------------------------

#[test]
fn failed_write_cancels_linked_fsync_and_freezes_watermark() {
    let dir = test_dir("failw");
    let mut driver = ScriptedDriver::new(IoMode::Real);
    driver.fail_next_write = Some(libc::EIO);
    let (mut lp, mut plane, clock) = cell(&dir, driver, 1 << 20);
    plane.push_jobs(4, FsyncClass::Always);

    for _ in 0..20 {
        lp.run_iteration(&mut plane).expect("iteration");
        clock.advance(Nanos::from_millis(20));
        if plane.cell_failed {
            break;
        }
    }

    assert!(plane.cell_failed, "write failure must fail-stop the cell");
    assert!(plane.io_errors.contains(&(TokenClass::LogWrite, libc::EIO)));
    assert!(
        plane.io_errors.contains(&(TokenClass::Fsync, libc::ECANCELED)),
        "no sync-past-failed-write: the linked fsync must be cancelled, got {:?}",
        plane.io_errors
    );
    assert_eq!(plane.commit.watermark(), None, "nothing durable was claimed");
    assert!(plane.acks.borrow().is_empty(), "zero acks for the failed batch");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn short_write_delivers_exactly_one_sync_after_full_coverage() {
    let dir = test_dir("short");
    let mut driver = ScriptedDriver::new(IoMode::Real);
    driver.short_next_write = Some(3);
    let (mut lp, mut plane, clock) = cell(&dir, driver, 1 << 20);
    plane.push_jobs(8, FsyncClass::Always);

    run_until_quiesced(&mut lp, &mut plane, &clock, Nanos::from_millis(20), 10_000);

    assert!(!plane.cell_failed, "a short write is retried, not an error: {:?}", plane.io_errors);
    assert_eq!(plane.acks.borrow().len(), 8, "all acks after the full write was covered");
    assert_eq!(plane.commit.watermark(), plane.commit.queued_up_to());
    assert_replay_matches(&plane, &dir);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn fsync_failure_freezes_watermark_and_emits_zero_acks() {
    let dir = test_dir("failf");
    let mut driver = ScriptedDriver::new(IoMode::Real);
    driver.fail_next_fsync = Some(libc::EIO);
    let (mut lp, mut plane, clock) = cell(&dir, driver, 1 << 20);
    plane.push_jobs(4, FsyncClass::Always);

    for _ in 0..20 {
        lp.run_iteration(&mut plane).expect("iteration");
        clock.advance(Nanos::from_millis(20));
        if plane.cell_failed {
            break;
        }
    }

    assert!(plane.cell_failed, "fsync failure is fatal (§8.4 fsyncgate)");
    assert!(plane.io_errors.contains(&(TokenClass::Fsync, libc::EIO)));
    assert_eq!(plane.commit.watermark(), None);
    assert!(plane.acks.borrow().is_empty(), "zero acks emitted for the failed sync's batch");
    std::fs::remove_dir_all(&dir).ok();
}

// ---- S05: the L3 syscall shape ----------------------------------------------

#[test]
fn log_ops_ride_the_single_iteration_submit() {
    let dir = test_dir("l3");
    let (mut lp, mut plane, clock) = cell(&dir, ScriptedDriver::new(IoMode::Real), 1 << 20);
    plane.push_jobs(300, FsyncClass::Always);
    let iters = run_until_quiesced(&mut lp, &mut plane, &clock, Nanos::from_millis(20), 10_000);
    // One driver entry per iteration, no extra syscalls for LOG: the driver
    // counted exactly one submit per run_iteration.
    let [submits, ..] = lp.counters();
    assert!(submits <= iters, "LOG writev+fsync must ride the existing single submit");
    assert!(!plane.cell_failed);
    assert_eq!(SegmentId(0), plane.rotor.active_segment());
    std::fs::remove_dir_all(&dir).ok();
}
