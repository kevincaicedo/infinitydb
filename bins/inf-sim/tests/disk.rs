//! M2-S18 driver-tier tests (ADR-0020 D7): `SimDriver` executes
//! `IoOp::LogWrite` / `IoOp::Fdatasync` against the sim disk — a write
//! completes `LogWritten` but is NOT durable, the fsync completes
//! `Synced` and is the barrier, a linked sync after a failed write is
//! `ECANCELED` (the uring chain contract), and a dead disk completes
//! ops with `EIO`.

use std::path::Path;

use inf_alloc::BufferPool;
use inf_log::fs::sim::SimFile;
use inf_log::fs::{SegmentFile, SegmentFs};
use inf_runtime::{
    BackendDriver, Completion, CompletionResult, CompletionToken, IoOp, StableBytes, TokenClass,
    Wait, WriteBarrier,
};
use inf_server::SimDisk;
use inf_sim::net::{CellNet, Plant, SimDriver};

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
    disk.create_dir_all(Path::new(DIR)).expect("dirs");
    let seg = disk.create_segment(&Path::new(DIR).join("seg-000000.ilog"), 4096).expect("create");
    disk.sync_dir(Path::new(DIR)).expect("commit the name");
    let fd = seg.raw_fd().expect("sim fd");
    let dir = disk.open_dir(Path::new(DIR)).expect("dir handle");
    let dir_fd = dir.raw_fd().expect("dir fd");
    (disk, [seg, dir], fd, dir_fd)
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
        offset: 0,
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
        offset: 512,
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
        probe.driver_write_at(pfd, 0, b"frame-one").expect("write");
        probe.driver_fdatasync(pfd).expect("sync");
        probe.driver_write_at(pfd, 512, b"frame-two-unsynced").expect("write");
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
        offset: 0,
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
    use inf_foundation::time::{Nanos, VirtualClock};
    use inf_log::fs::sim::{SimDiskConfig, StallConfig};
    use std::rc::Rc;

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
                offset: 0,
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
