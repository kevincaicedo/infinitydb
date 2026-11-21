#![allow(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    reason = "test-only: filesystem fixtures outside cell code (ADR-0144 D5)"
)]
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
use inf_foundation::CellId;
use inf_foundation::bounded::{Cap, CapCensus, CapFill};
use inf_foundation::time::{Nanos, VirtualClock};
use inf_log::{FsyncClass, LogGeometry, SegmentConfig, StagingConfig};
use inf_runtime::{CellLoop, ExecutorLayout, LoopConfig, TaskAdmission, WaitNodes, WatermarkGate};
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
    let (census, serve_mark) = CapCensus::new(CellId(0));
    let (timer_set, everysec_owner) = support::everysec_timers(&census);
    let mut lp = CellLoop::new(
        driver,
        Rc::clone(&clock),
        BufferPool::new(4, 1024),
        LoopConfig::default(),
        serve_mark,
        timer_set,
        support::loop_executor(&census),
    );
    // Small segments: rotation + seal fsyncs interleave with linked ones.
    let staging = StagingConfig::with_capacity(16 << 10);
    let segment = SegmentConfig { segment_bytes: 16 << 10, ..Default::default() };
    let mut plane = DurablePlane::new(
        dir,
        LogGeometry::new(staging, segment).expect("geometry"),
        everysec_owner,
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
            && plane.commit.frames_unwritten() == 0
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

/// S06 wake-storm AC (mechanism tier, ADR-0177 A2): one fsync completion
/// covers 50,000 gated futures. `advance` wakes none; the gate wakes them a
/// budget at a time, as the durable cell's LOG steps call it, each turn one
/// `wake_covered` and one EXECUTE pass at the same 1,024, so the storm leaves
/// over 49 turns, in LSN order, no call past its budget and no turn's pass
/// past EXECUTE's. The wall-clock p99.9 < 2 ms row binds on the reference box
/// (S22).
#[test]
fn wake_storm_50k_drains_a_budget_a_turn() {
    const WAITERS: u64 = 50_000;
    /// A turn's EXECUTE pass and its wakes: the loop's default budget, which
    /// the durable cell's wake budget may not pass.
    const BUDGET: usize = 1024;
    /// The turns the storm takes: one a budget of waits.
    const TURNS: usize = WAITERS.div_ceil(BUDGET as u64) as usize;
    /// The storm's waiters, all alive at once.
    const STORM: Cap = Cap::entries::<{ WAITERS as u32 }>("storm-waiters", CapFill::Serving);
    /// The gate's pool: a wait node for each waiter (ADR-0177 D5).
    const STORM_NODES: Cap = Cap::entries::<{ WAITERS as u32 }>("storm-nodes", CapFill::Serving);
    async fn storm_waiter(gate: WatermarkGate, lsn: u64, order: Rc<RefCell<Vec<u64>>>) {
        gate.wait_for(lsn).await;
        order.borrow_mut().push(lsn);
    }
    let (census, _mark) = CapCensus::new(CellId(0));
    let mut layout = ExecutorLayout::new(&census);
    let class = layout.class(&storm_waiter, STORM).expect("the storm's class");
    let mut executor = layout.build().expect("no class refuses");
    let pool = WaitNodes::build(STORM_NODES, &census).expect("the storm's pool");
    let gate = WatermarkGate::build(pool.seat);
    let order = Rc::new(RefCell::new(Vec::with_capacity(WAITERS as usize)));
    for lsn in 1..=WAITERS {
        let gate = gate.clone();
        let order = Rc::clone(&order);
        let TaskAdmission::Granted(permit) = executor.reserve(class) else {
            panic!("the storm's class holds every waiter");
        };
        permit.spawn_local(move || storm_waiter(gate, lsn, order));
    }
    // Futures suspend on first poll (spawn queues them once).
    while executor.run_ready(BUDGET) == BUDGET {}
    executor.run_ready(BUDGET);
    assert_eq!(gate.waiting(), WAITERS as usize);

    // ONE fsync completion advances the watermark over all of them.
    gate.advance(WAITERS);
    assert_eq!(executor.run_ready(BUDGET), 0, "advance itself woke a wait");
    assert!(gate.owes(), "the advance owes the storm's wakes");

    let mut turns = 0;
    let mut polled_total = 0;
    // One turn more than the storm needs: a gate still owing after it fails below.
    for _ in 0..=TURNS {
        if !gate.owes() {
            break;
        }
        let woken = gate.wake_covered(BUDGET);
        assert!(woken <= BUDGET, "a call woke {woken}, past its budget");
        let polled = executor.run_ready(BUDGET);
        assert_eq!(polled, woken, "turn {turns}: the pass runs the batch it woke");
        polled_total += polled;
        turns += usize::from(woken > 0);
    }
    assert!(!gate.owes(), "wakes still owed after {TURNS} turns");
    assert_eq!(turns, TURNS, "the storm leaves a budget a turn");
    assert_eq!(polled_total, WAITERS as usize, "every gated future resumed");
    assert_eq!(gate.waiting(), 0, "no wait left on the gate");
    assert_eq!(executor.live_tasks(), 0, "no leaked tasks after the storm");
    // FIFO by LSN: the gate wakes in its heap's (LSN) order and the ready
    // queue preserves it.
    let order = order.borrow();
    assert!(order.windows(2).all(|w| w[0] < w[1]), "wake order must be FIFO by LSN");
}
