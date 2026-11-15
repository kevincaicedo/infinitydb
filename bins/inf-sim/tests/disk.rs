//! M2-S18 driver-tier tests (ADR-0020 D7): `SimDriver` executes
//! `IoOp::LogWrite` / `IoOp::Fdatasync` against the sim disk — a write
//! completes `LogWritten` but is NOT durable, the fsync completes
//! `Synced` and is the barrier, a linked sync after a failed write is
//! `ECANCELED` (the uring chain contract), and a dead disk completes
//! ops with `EIO`. A disk's barrier hold keeps each barrier it names
//! whole until it lifts, read from the completions the driver posts.

use std::path::Path;
use std::rc::Rc;

use inf_alloc::BufferPool;
use inf_foundation::FileOffset;
use inf_foundation::time::{Clock, Nanos, VirtualClock};
use inf_log::fs::sim::{SimDiskConfig, SimFile, StallConfig};
use inf_log::fs::{SegmentFile, SegmentFs};
use inf_runtime::{
    BackendDriver, Completion, CompletionResult, CompletionToken, IoOp, StableBytes, TokenClass,
    Wait, WriteBarrier,
};
use inf_server::SimDisk;
use inf_sim::net::{CellNet, HeldBarriers, Plant, SimDriver};

const DIR: &str = "shard-0/log";

fn reap(driver: &mut SimDriver, pool: &mut BufferPool) -> Vec<Completion> {
    let mut out = Vec::new();
    driver.submit_and_reap(pool, Wait::Poll, &mut out).expect("submit");
    out
}

fn token(class: TokenClass, slot: u32) -> CompletionToken {
    CompletionToken::new(class, slot, 1)
}

/// The payload for a LogWrite: leaked so it outlives the op (the test
/// stands in for the plane's FrameLease custody).
fn stable(bytes: &'static [u8]) -> StableBytes {
    // SAFETY: 'static bytes are live, stable, and unmodified forever —
    // trivially past the op's terminal completion.
    unsafe { StableBytes::new(bytes) }
}

/// The handles ride along: a dropped handle is a closed fd to the sim
/// (F-L01-02), exactly as the plane's custody rules require.
fn disk_with_segment() -> (SimDisk, [SimFile; 2], i32, i32) {
    let disk = SimDisk::new();
    let (handles, fd, dir_fd) = segment_on(&disk);
    (disk, handles, fd, dir_fd)
}

/// One segment on `disk`, its name committed, and its directory open: the
/// segment's handle first, then the directory's, and their fds.
fn segment_on(disk: &SimDisk) -> ([SimFile; 2], i32, i32) {
    disk.create_dir_all(Path::new(DIR)).expect("dirs");
    let seg = disk.create_segment(&Path::new(DIR).join("seg-000000.ilog"), 4096).expect("create");
    disk.sync_dir(Path::new(DIR)).expect("commit the name");
    let fd = seg.raw_fd().expect("sim fd");
    let dir = disk.open_dir(Path::new(DIR)).expect("dir handle");
    let dir_fd = dir.raw_fd().expect("dir fd");
    ([seg, dir], fd, dir_fd)
}

#[test]
fn log_write_is_buffered_until_the_linked_sync() {
    let (disk, _handles, fd, _dir_fd) = disk_with_segment();
    let net = CellNet::new(0, 7, Plant::None);
    let mut driver = SimDriver::with_disk(net, disk.clone());
    let mut pool = BufferPool::new(8, 512);

    // Write + linked fsync: completions in submission order.
    driver.push(IoOp::LogWrite {
        fd,
        offset: FileOffset::from_u32_bytes(0),
        data: stable(b"frame-one"),
        token: token(TokenClass::LogWrite, 1),
        barrier: WriteBarrier::LinkedFsync { fsync_token: token(TokenClass::Fsync, 2) },
    });
    let done = reap(&mut driver, &mut pool);
    assert_eq!(done.len(), 2);
    assert!(matches!(done[0].result, CompletionResult::LogWritten));
    assert!(matches!(done[1].result, CompletionResult::Synced));

    // A second write WITHOUT a sync: page-cache only.
    driver.push(IoOp::LogWrite {
        fd,
        offset: FileOffset::from_u32_bytes(512),
        data: stable(b"frame-two-unsynced"),
        token: token(TokenClass::LogWrite, 3),
        barrier: WriteBarrier::None,
    });
    let done = reap(&mut driver, &mut pool);
    assert_eq!(done.len(), 1);
    assert!(matches!(done[0].result, CompletionResult::LogWritten));

    // Sweep cuts: the synced frame is law; the un-synced one must both
    // survive somewhere and vanish somewhere.
    let mut lost = 0u32;
    let mut kept = 0u32;
    for seed in 0..24u64 {
        let probe = SimDisk::new();
        probe.create_dir_all(Path::new(DIR)).expect("dirs");
        let seg =
            probe.create_segment(&Path::new(DIR).join("seg-000000.ilog"), 4096).expect("create");
        probe.sync_dir(Path::new(DIR)).expect("commit");
        let pfd = seg.raw_fd().expect("fd");
        probe.driver_write_at(pfd, FileOffset::from_u32_bytes(0), b"frame-one").expect("write");
        probe.driver_fdatasync(pfd).expect("sync");
        probe
            .driver_write_at(pfd, FileOffset::from_u32_bytes(512), b"frame-two-unsynced")
            .expect("write");
        probe.power_cut(seed);
        let bytes = probe.contents(&Path::new(DIR).join("seg-000000.ilog")).expect("named");
        assert_eq!(&bytes[..9], b"frame-one", "seed {seed}: synced bytes survive");
        if bytes.len() >= 512 + 18 && bytes[512..512 + 18] == *b"frame-two-unsynced" {
            kept += 1;
        } else {
            lost += 1;
        }
    }
    assert!(lost > 0 && kept > 0, "un-synced driver write must be volatile ({lost}/{kept})");
}

/// The uring chain contract: a failed write cancels its linked sync;
/// standalone fsyncs on a dead disk fail with EIO.
#[test]
fn failed_write_cancels_the_linked_sync() {
    let (disk, _handles, fd, dir_fd) = disk_with_segment();
    let net = CellNet::new(0, 7, Plant::None);
    let mut driver = SimDriver::with_disk(net, disk.clone());
    let mut pool = BufferPool::new(8, 512);

    disk.cut_after_ops(0); // dead: every op fails from here
    driver.push(IoOp::LogWrite {
        fd,
        offset: FileOffset::from_u32_bytes(0),
        data: stable(b"never-lands"),
        token: token(TokenClass::LogWrite, 1),
        barrier: WriteBarrier::LinkedFsync { fsync_token: token(TokenClass::Fsync, 2) },
    });
    driver.push(IoOp::Fdatasync { fd: dir_fd, token: token(TokenClass::ManifestSync, 3) });
    let done = reap(&mut driver, &mut pool);
    assert_eq!(done.len(), 3);
    assert!(
        matches!(done[0].result, CompletionResult::Error { errno, .. } if errno == libc::EIO),
        "failed write is EIO"
    );
    assert!(
        matches!(done[1].result, CompletionResult::Error { errno, .. } if errno == libc::ECANCELED),
        "linked sync after a failed write is ECANCELED (no sync past a failed write)"
    );
    assert!(
        matches!(done[2].result, CompletionResult::Error { errno, .. } if errno == libc::EIO),
        "standalone fsync on a dead disk is EIO"
    );
}

/// F-L04-06, the model: a plain write followed by an independently issued
/// fdatasync. On the instant device the write is always covered — the
/// window is closed; on `StallConfig::write_reorder()` the sync completes
/// first and the write rides the cut, lost on some seed.
#[test]
fn the_reorder_window_is_closed_on_the_instant_device_and_open_on_write_reorder() {
    let lost_over = |stall: Option<StallConfig>| -> u32 {
        let mut lost = 0u32;
        for seed in 0..24u64 {
            let disk = match &stall {
                Some(cfg) => SimDisk::with_stall(SimDiskConfig::default(), cfg.clone(), seed),
                None => SimDisk::new(),
            };
            disk.create_dir_all(Path::new(DIR)).expect("dirs");
            let seg =
                disk.create_segment(&Path::new(DIR).join("seg-000000.ilog"), 4096).expect("create");
            disk.sync_dir(Path::new(DIR)).expect("commit");
            let fd = seg.raw_fd().expect("fd");
            let clock = Rc::new(VirtualClock::new(Nanos(1_000)));
            let net = CellNet::new(0, 7, Plant::None);
            let mut driver = SimDriver::with_disk_stall(net, disk.clone(), Rc::clone(&clock));
            let mut pool = BufferPool::new(8, 512);
            driver.push(IoOp::LogWrite {
                fd,
                offset: FileOffset::from_u32_bytes(0),
                data: stable(&[0x5A; 4096]),
                token: token(TokenClass::LogWrite, 1),
                barrier: WriteBarrier::None,
            });
            driver.push(IoOp::Fdatasync { fd, token: token(TokenClass::Fsync, 2) });
            let mut done = reap(&mut driver, &mut pool);
            // Let every deferred op land.
            clock.advance(Nanos(1_000_000));
            done.extend(reap(&mut driver, &mut pool));
            assert_eq!(done.len(), 2, "seed {seed}: both ops complete");
            disk.power_cut(seed);
            let bytes = disk.contents(&Path::new(DIR).join("seg-000000.ilog")).expect("named");
            if bytes.len() < 4096 || bytes[..4096].iter().any(|b| *b != 0x5A) {
                lost += 1;
            }
        }
        lost
    };
    assert_eq!(lost_over(None), 0, "the instant device orders the write before the later sync");
    assert!(
        lost_over(Some(StallConfig::write_reorder())) > 0,
        "write_reorder(): no seed let the write land after the later-issued sync"
    );
    assert!(!SimDisk::new().write_reorder_armed());
    assert!(
        SimDisk::with_stall(SimDiskConfig::default(), StallConfig::write_reorder(), 1)
            .write_reorder_armed()
    );
}

/// F-L04-06, the harness: every driver-tier durable scenario table row
/// builds a disk with the window open (the `boot` gate refuses the rest;
/// `corpus.rs` drives the backfill / sidecar / boot-storm runners
/// through it).
#[test]
fn every_driver_tier_scenario_arms_the_reorder_window() {
    use inf_sim::durable::build_disk;
    use inf_sim::{DurableScenario, TieredScenario};
    let rows: Vec<(&str, DurableScenario)> = vec![
        ("m2-durable", DurableScenario::m2_durable(1)),
        ("m2-clean-stop", DurableScenario::m2_clean_stop(1)),
        ("m2-device-budget", DurableScenario::m2_device_budget(1)),
        ("m2-mode-transition", DurableScenario::m2_mode_transition(1)),
        ("m2-reorder-window", DurableScenario::m2_reorder_window(1)),
        ("m2-fill-tick", DurableScenario::m2_fill_tick(1)),
        ("m2-group-hold", DurableScenario::m2_group_hold(1)),
        ("m2-fua-pending", DurableScenario::m2_fua_pending(1)),
        ("m2-ckpt-refused", DurableScenario::m2_ckpt_refused(1)),
        ("m2-recycle", DurableScenario::m2_recycle(1)),
        ("m3-document", DurableScenario::m3_document(1)),
    ];
    for (name, scenario) in &rows {
        assert!(
            build_disk(scenario.seed, scenario.stall.as_ref()).write_reorder_armed(),
            "{name}: the reorder window is closed"
        );
    }
    let tiered = TieredScenario::m4_tiered(1);
    assert!(build_disk(tiered.seed, tiered.stall.as_ref()).write_reorder_armed(), "m4-tiered");
}

// The token slots of the hold's ops, by the op they ride.
const LINKED: u32 = 1;
const LINKED_SYNC: u32 = 2;
const PLAIN: u32 = 3;
const THROUGH: u32 = 4;
const SYNC: u32 = 5;
const SEGMENT_SYNC: u32 = 6;
const DIR_SYNC: u32 = 7;

/// The step of the grid a stall run reads its completions on.
const GRID: Nanos = Nanos(1_000);
/// The grid steps a stall run waits for its completions: 10 ms of virtual
/// time, past every service time its stall model draws (at most 1 ms here).
const GRID_STEPS_MAX: u32 = 10_000;
/// The completions the three barrier ops owe: the linked write's and its
/// sync's, the write-through write's and the standalone sync's.
const BARRIER_COMPLETIONS: usize = 4;

/// What a completion posted, as the device contract names it.
#[derive(Debug, PartialEq, Eq)]
enum Posted {
    Written,
    Synced,
    Errno(i32),
    Other,
}

fn is_segment(name: &Path) -> bool {
    name.extension().is_some_and(|ext| ext == "ilog")
}

fn write(fd: i32, at: u32, slot: u32, barrier: WriteBarrier) -> IoOp {
    let offset = FileOffset::from_u32_bytes(at);
    let token = token(TokenClass::LogWrite, slot);
    IoOp::LogWrite { fd, offset, data: stable(b"frame"), token, barrier }
}

/// The three barrier ops: a write with its linked sync, a write-through
/// write and a standalone sync.
fn barriers(fd: i32) -> [IoOp; 3] {
    let fsync_token = token(TokenClass::Fsync, LINKED_SYNC);
    [
        write(fd, 0, LINKED, WriteBarrier::LinkedFsync { fsync_token }),
        write(fd, 1024, THROUGH, WriteBarrier::WriteThrough),
        IoOp::Fdatasync { fd, token: token(TokenClass::Fsync, SYNC) },
    ]
}

/// The entry of four ops: the barrier ops with a plain write among them.
fn four(fd: i32) -> Vec<IoOp> {
    let [linked, through, sync] = barriers(fd);
    vec![linked, write(fd, 512, PLAIN, WriteBarrier::None), through, sync]
}

/// One driver entry carrying `ops`: what it posted, in order.
fn entry(driver: &mut SimDriver, pool: &mut BufferPool, ops: Vec<IoOp>) -> Vec<(u32, Posted)> {
    for op in ops {
        driver.push(op);
    }
    let posted = |result| match result {
        CompletionResult::LogWritten => Posted::Written,
        CompletionResult::Synced => Posted::Synced,
        CompletionResult::Error { errno, .. } => Posted::Errno(errno),
        _ => Posted::Other,
    };
    let done = reap(driver, pool);
    done.into_iter().map(|done| (done.token.slot(), posted(done.result))).collect()
}

/// Every op the device took, every class together.
fn observed_ops(driver: &SimDriver) -> u64 {
    driver.observed_io().ops.iter().sum()
}

/// A fresh driver over `disk`, with the pool its entries reap into.
fn driver_on(disk: &SimDisk) -> (SimDriver, BufferPool) {
    let driver = SimDriver::with_disk(CellNet::new(0, 7, Plant::None), disk.clone());
    (driver, BufferPool::new(8, 512))
}

/// `hold_barriers` keeps every barrier whole and lets a plain write
/// pass; at the lift the held ops post in submission order.
fn every_barrier_is_held(disk: &SimDisk, fd: i32) {
    let (mut driver, mut pool) = driver_on(disk);
    disk.hold_barriers(Some(is_segment));
    let posted = entry(&mut driver, &mut pool, four(fd));
    let plain = vec![(PLAIN, Posted::Written)];
    assert_eq!(posted, plain, "barrier-completed-while-held: the hold kept a barrier short");
    let kinds = HeldBarriers { standalone: 1, linked: 1, write_through: 1 };
    assert_eq!(driver.held_barriers(), kinds, "held-by-kind: the hold keeps each barrier");
    assert_eq!(driver.held_syncs(), 3, "held-by-kind: the total is the kinds' sum");
    assert_eq!(observed_ops(&driver), 1, "held-observed: a held op reached the device");
    disk.hold_barriers(None);
    let posted = entry(&mut driver, &mut pool, Vec::new());
    let order = vec![
        (LINKED, Posted::Written),
        (LINKED_SYNC, Posted::Synced),
        (THROUGH, Posted::Written),
        (SYNC, Posted::Synced),
    ];
    assert_eq!(posted, order, "held-order: the lift posts the held ops in submission order");
    assert_eq!(driver.held_barriers(), HeldBarriers::default(), "held-order: none left held");
    assert_eq!(observed_ops(&driver), 5, "held-order: each held op observed at its lift");
}

/// `hold_syncs` keeps the standalone sync alone: the frame barriers pass.
fn standalone_alone_is_held(disk: &SimDisk, fd: i32) {
    let (mut driver, mut pool) = driver_on(disk);
    disk.hold_syncs(Some(is_segment));
    let posted = entry(&mut driver, &mut pool, four(fd));
    let passed = vec![
        (LINKED, Posted::Written),
        (LINKED_SYNC, Posted::Synced),
        (PLAIN, Posted::Written),
        (THROUGH, Posted::Written),
    ];
    assert_eq!(posted, passed, "standalone-scope-widened: a sync hold kept a frame barrier");
    let kinds = HeldBarriers { standalone: 1, ..HeldBarriers::default() };
    assert_eq!(driver.held_barriers(), kinds, "standalone-scope-widened: held by kind");
    disk.hold_syncs(None);
    let posted = entry(&mut driver, &mut pool, Vec::new());
    assert_eq!(posted, vec![(SYNC, Posted::Synced)], "held-order: the sync at its lift");
}

/// The ops two entries leave held post at the lift in submission order
/// across the entries, the earlier entry's first, and ahead of the lifting
/// entry's own op.
fn held_entries_release_in_order(disk: &SimDisk, fd: i32) {
    let (mut driver, mut pool) = driver_on(disk);
    disk.hold_barriers(Some(is_segment));
    let [linked, through, sync] = barriers(fd);
    let posted = entry(&mut driver, &mut pool, vec![linked]);
    assert!(posted.is_empty(), "held-entry-order: the first entry's op posted {posted:?}");
    let posted = entry(&mut driver, &mut pool, vec![through, sync]);
    assert!(posted.is_empty(), "held-entry-order: the second entry's ops posted {posted:?}");
    disk.hold_barriers(None);
    let posted = entry(&mut driver, &mut pool, vec![write(fd, 512, PLAIN, WriteBarrier::None)]);
    let order = vec![
        (LINKED, Posted::Written),
        (LINKED_SYNC, Posted::Synced),
        (THROUGH, Posted::Written),
        (SYNC, Posted::Synced),
        (PLAIN, Posted::Written),
    ];
    assert_eq!(posted, order, "held-entry-order: the lift posts each entry's held ops in turn");
}

/// Re-arming a narrower hold, `hold_syncs` over `hold_barriers`, releases
/// the held writes at the next entry: each held op is read by its own kind
/// against the hold armed now, so the standalone sync alone stays held.
fn a_narrowed_rearm_releases_the_held_writes(disk: &SimDisk, fd: i32) {
    let (mut driver, mut pool) = driver_on(disk);
    disk.hold_barriers(Some(is_segment));
    let posted = entry(&mut driver, &mut pool, four(fd));
    assert_eq!(posted, vec![(PLAIN, Posted::Written)], "narrowed-rearm: the broad hold's premise");
    disk.hold_syncs(Some(is_segment));
    let posted = entry(&mut driver, &mut pool, Vec::new());
    let writes =
        vec![(LINKED, Posted::Written), (LINKED_SYNC, Posted::Synced), (THROUGH, Posted::Written)];
    assert_eq!(posted, writes, "narrowed-rearm-held: a sync hold kept a held write");
    let kinds = HeldBarriers { standalone: 1, ..HeldBarriers::default() };
    assert_eq!(driver.held_barriers(), kinds, "narrowed-rearm-held: the sync alone held");
    disk.hold_syncs(None);
    let posted = entry(&mut driver, &mut pool, Vec::new());
    assert_eq!(posted, vec![(SYNC, Posted::Synced)], "narrowed-rearm-held: the sync at its lift");
}

/// A hold keeps the barriers on the files its predicate names alone: every
/// barrier on a file it does not name, or on a directory, posts at once,
/// while a sync on the named segment in the same entry is held.
fn a_barrier_on_an_unnamed_fd_passes(disk: &SimDisk, fd: i32, dir_fd: i32) {
    let manifest = disk.create_segment(&Path::new(DIR).join("MANIFEST-000001"), 4096);
    let manifest = manifest.expect("create");
    let manifest_fd = manifest.raw_fd().expect("sim fd");
    let (mut driver, mut pool) = driver_on(disk);
    disk.hold_barriers(Some(is_segment));
    let mut ops = Vec::from(barriers(manifest_fd));
    ops.push(IoOp::Fdatasync { fd: dir_fd, token: token(TokenClass::Fsync, DIR_SYNC) });
    ops.push(IoOp::Fdatasync { fd, token: token(TokenClass::Fsync, SEGMENT_SYNC) });
    let posted = entry(&mut driver, &mut pool, ops);
    let passed = vec![
        (LINKED, Posted::Written),
        (LINKED_SYNC, Posted::Synced),
        (THROUGH, Posted::Written),
        (SYNC, Posted::Synced),
        (DIR_SYNC, Posted::Synced),
    ];
    assert_eq!(posted, passed, "unnamed-barrier-held: a hold kept a barrier it does not name");
    let kinds = HeldBarriers { standalone: 1, ..HeldBarriers::default() };
    assert_eq!(driver.held_barriers(), kinds, "unnamed-barrier-held: the segment's sync held");
    disk.hold_barriers(None);
    let posted = entry(&mut driver, &mut pool, Vec::new());
    let lifted = vec![(SEGMENT_SYNC, Posted::Synced)];
    assert_eq!(posted, lifted, "unnamed-barrier-held: the segment's sync at its lift");
}

/// Every completion a stall run posts from the entry carrying `ops` on: the
/// instant of the reap that posted it and its token's slot. The clock steps
/// on the grid a reap at a time, so a completion reads at the first grid
/// instant at or past its due time; the run reads completions alone, never
/// the device's schedule.
fn posted_on_the_grid(
    driver: &mut SimDriver,
    pool: &mut BufferPool,
    clock: &VirtualClock,
    mut ops: Vec<IoOp>,
) -> Vec<(Nanos, u32)> {
    let mut posted = Vec::new();
    for _ in 0..=GRID_STEPS_MAX {
        let now = clock.now();
        let done = entry(driver, pool, std::mem::take(&mut ops));
        posted.extend(done.into_iter().map(|(slot, _)| (now, slot)));
        if posted.len() >= BARRIER_COMPLETIONS {
            break;
        }
        clock.advance(GRID);
    }
    assert_eq!(posted.len(), BARRIER_COMPLETIONS, "held-draw-early: unposted after 10 ms");
    posted
}

/// Under the stall model a held op draws nothing while held: nothing posts
/// before the lift, and from the lift on its completions post at the grid
/// instants of an unarmed run that submits the same ops at the lift on the
/// same device.
fn a_held_op_draws_at_its_lift() {
    const STALL_SEED: u64 = 0x5eed;
    let stall = StallConfig {
        base_ns: 40_000,
        tail_permille: 500,
        tail_mult: 4,
        through_base_ns: 20_000,
        write_base_ns: 8_000,
        ..StallConfig::instant()
    };
    let lift = Nanos(1_000_000);
    let run = |armed: bool| {
        let disk = SimDisk::with_stall(SimDiskConfig::default(), stall.clone(), STALL_SEED);
        let (_handles, fd, _dir_fd) = segment_on(&disk);
        let clock = Rc::new(VirtualClock::new(Nanos(1_000)));
        let net = CellNet::new(0, 7, Plant::None);
        let mut driver = SimDriver::with_disk_stall(net, disk.clone(), Rc::clone(&clock));
        let mut pool = BufferPool::new(8, 512);
        if !armed {
            clock.set(lift);
            return posted_on_the_grid(&mut driver, &mut pool, &clock, barriers(fd).into());
        }
        disk.hold_barriers(Some(is_segment));
        let posted = entry(&mut driver, &mut pool, barriers(fd).into());
        assert!(posted.is_empty(), "held-draw-early: a held op posted at submission {posted:?}");
        clock.set(Nanos(lift.0 - 1));
        let posted = entry(&mut driver, &mut pool, Vec::new());
        assert!(posted.is_empty(), "held-draw-early: a held op posted before its lift {posted:?}");
        clock.set(lift);
        disk.hold_barriers(None);
        posted_on_the_grid(&mut driver, &mut pool, &clock, Vec::new())
    };
    let unarmed = run(false);
    let deferred = unarmed.iter().all(|&(at, _)| at > lift);
    assert!(deferred, "held-draw-early: the stall model defers each barrier {unarmed:?}");
    assert_eq!(run(true), unarmed, "held-draw-early: a held op drew before its lift");
}

/// A held op whose fd closes in the hold names no file, so the next
/// entry submits it at once and the closed descriptor answers: the
/// write `EBADF`, its linked sync `ECANCELED`.
fn a_closed_hold_answers_at_once() {
    let (disk, [segment, _dir], fd, _dir_fd) = disk_with_segment();
    let (mut driver, mut pool) = driver_on(&disk);
    disk.hold_barriers(Some(is_segment));
    let [linked, _, _] = barriers(fd);
    let posted = entry(&mut driver, &mut pool, vec![linked]);
    assert!(posted.is_empty(), "closed-hold-answer: the held write posted {posted:?}");
    drop(segment);
    let posted = entry(&mut driver, &mut pool, Vec::new());
    let answer =
        vec![(LINKED, Posted::Errno(libc::EBADF)), (LINKED_SYNC, Posted::Errno(libc::ECANCELED))];
    assert_eq!(posted, answer, "closed-hold-answer: the closed descriptor's answer");
    assert_eq!(driver.held_syncs(), 0, "closed-hold-answer: nothing left held");
    disk.hold_barriers(None);
}

/// The device contract of a hold, read from the completions the driver
/// posts: each scope keeps its barriers whole, on the files it names,
/// observed and drawn only at the lift, in submission order across
/// entries; a narrower re-arm releases what it no longer holds; a plain
/// write always passes.
#[test]
fn a_barrier_hold_keeps_every_barrier_until_it_lifts() {
    let (disk, _handles, fd, dir_fd) = disk_with_segment();
    every_barrier_is_held(&disk, fd);
    standalone_alone_is_held(&disk, fd);
    held_entries_release_in_order(&disk, fd);
    a_narrowed_rearm_releases_the_held_writes(&disk, fd);
    a_barrier_on_an_unnamed_fd_passes(&disk, fd, dir_fd);
    a_held_op_draws_at_its_lift();
    a_closed_hold_answers_at_once();
}
