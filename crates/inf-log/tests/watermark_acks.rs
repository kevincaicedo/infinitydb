//! M2-S06 integration: `always` acks gate on the durability watermark.
//! The oracle — **no response leaves the cell before the watermark covers
//! its LSN** — is asserted inside every ack future (support harness) and
//! swept here across seeded random completion schedules; the wake-storm AC
//! is proven at the executor level (bounded drain under the EXECUTE
//! budget). The full sim-tier oracle (power cuts, 10k seeds) binds at
//! M2-S18/S19 on the sim disk.

mod support;

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

use inf_alloc::BufferPool;
use inf_foundation::time::{Nanos, VirtualClock};
use inf_log::{FsyncClass, SegmentConfig, StagingConfig};
use inf_runtime::{CellExecutor, CellLoop, LoopConfig, WatermarkGate};
use support::{DurablePlane, IoMode, ScriptedDriver, test_dir};

/// Deterministic xorshift64* (L7: no ambient randomness).
struct Rng(u64);

impl Rng {
    fn below(&mut self, bound: u64) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D) % bound
    }
}

fn run_schedule(seed: u64, dir: &Path) {
    let mut rng = Rng(seed);
    let mut driver = ScriptedDriver::new(IoMode::Real);
    // Random per-op completion delays (0–3 iterations): writes and fsyncs
    // land out of lockstep, seal syncs race linked syncs across fds.
    for _ in 0..8192 {
        driver.delays.push_back(rng.below(4) as u32);
    }
    let clock = Rc::new(VirtualClock::new(Nanos::ZERO));
    let mut lp =
        CellLoop::new(driver, Rc::clone(&clock), BufferPool::new(4, 1024), LoopConfig::default());
    // Small segments: rotation + seal fsyncs interleave with linked ones.
    let mut plane = DurablePlane::new(
        dir,
        StagingConfig { capacity_bytes: 16 << 10 },
        SegmentConfig { segment_bytes: 16 << 10, ..Default::default() },
    );
    plane.jobs_per_iter = 1 + rng.below(24) as usize;
    let mut always = 0;
    for _ in 0..30 {
        let n = 10 + rng.below(60);
        if rng.below(3) < 2 {
            plane.push_jobs(n, FsyncClass::Always);
            always += n;
        } else {
            plane.push_jobs(n, FsyncClass::Everysec);
        }
    }

    let mut idle = 0;
    let mut iters = 0u64;
    while idle <= 4 {
        lp.run_iteration(&mut plane).expect("iteration");
        assert!(plane.writes_this_iter <= 1);
        clock.advance(Nanos::from_millis(1 + rng.below(40)));
        iters += 1;
        assert!(iters < 200_000, "schedule {seed:#x} failed to quiesce");
        let quiesced = plane.jobs.is_empty()
            && plane.staging.is_empty()
            && plane.in_flight.is_none()
            && plane.commit.pending_fsyncs() == 0;
        idle = if quiesced { idle + 1 } else { 0 };
    }

    assert!(!plane.cell_failed, "schedule {seed:#x}: {:?}", plane.io_errors);
    let acks = plane.acks.borrow();
    // Zero oracle violations (each future asserted watermark ≥ its LSN);
    // completeness + FIFO-by-LSN on top.
    assert_eq!(acks.len() as u64, always, "schedule {seed:#x}: every always write acked");
    assert!(
        acks.windows(2).all(|w| w[0].0 < w[1].0),
        "schedule {seed:#x}: acks must wake FIFO by LSN"
    );
}

#[test]
fn no_ack_before_covering_fsync_across_random_schedules() {
    for seed in [0xC0FFEE, 0xDEAD_BEEF, 0x5EED_0001, 0x5EED_0002, 0x5EED_0003] {
        let dir = test_dir(&format!("sched-{seed:x}"));
        run_schedule(seed, &dir);
        std::fs::remove_dir_all(&dir).ok();
    }
}

/// S06 wake-storm AC (mechanism tier): one fsync completion releasing 50k
/// gated futures is O(N) ready-queue appends drained across iterations
/// under the EXECUTE budget — never one unbounded burst. The wall-clock
/// p99.9 < 2 ms row binds on the reference box (S22).
#[test]
fn wake_storm_50k_drains_under_the_execute_budget() {
    const WAITERS: u64 = 50_000;
    const BUDGET: usize = 1024;
    let mut executor = CellExecutor::new(4096);
    let gate = WatermarkGate::new();
    let order = Rc::new(RefCell::new(Vec::with_capacity(WAITERS as usize)));
    for lsn in 1..=WAITERS {
        let gate = gate.clone();
        let order = Rc::clone(&order);
        executor.spawn_local(async move {
            gate.waiter(lsn).await;
            order.borrow_mut().push(lsn);
        });
    }
    // Futures suspend on first poll (spawn queues them once).
    while executor.run_ready(BUDGET) == BUDGET {}
    executor.run_ready(BUDGET);
    assert_eq!(gate.waiting(), WAITERS as usize);

    // ONE fsync completion advances the watermark over all of them.
    assert_eq!(gate.advance(WAITERS), WAITERS as usize);

    let mut slices = 0;
    let mut polled_total = 0;
    loop {
        let polled = executor.run_ready(BUDGET);
        if polled == 0 {
            break;
        }
        assert!(polled <= BUDGET, "EXECUTE budget violated by a wake storm");
        polled_total += polled;
        slices += 1;
    }
    assert_eq!(polled_total, WAITERS as usize, "every gated future resumed");
    assert_eq!(slices, WAITERS.div_ceil(BUDGET as u64) as usize, "drain spread across slices");
    assert_eq!(executor.live_tasks(), 0, "no leaked tasks after the storm");
    // FIFO by LSN: the gate wakes in BTreeMap (LSN) order and the ready
    // queue preserves it.
    let order = order.borrow();
    assert!(order.windows(2).all(|w| w[0] < w[1]), "wake order must be FIFO by LSN");
}
