#![allow(
    clippy::disallowed_types,
    reason = "test target: std containers in test code, outside cell code (ADR-0163 D2)"
)]
#![allow(
    clippy::disallowed_methods,
    reason = "test target: harness deadlines and timings, not cell code"
)]
//! Reactor-loop structure (M0-S07): bounded steps, HoL fairness via budgets,
//! spin→park idle policy, timer-driven park timeouts, and the always-on
//! iteration histogram. Uses a null in-memory driver — backend behavior has
//! its own conformance suite.

use std::collections::VecDeque;
use std::io;
use std::rc::Rc;
use std::time::Duration;

use inf_alloc::BufferPool;
use inf_foundation::CellId;
use inf_foundation::bounded::{Cap, CapCensus, CapFill, CensusPhase};
use inf_foundation::time::{Nanos, StdClock, VirtualClock};
use inf_runtime::limits::PARK_CEILING_MAX;
use inf_runtime::{
    BackendDriver, Capabilities, CellLoop, CellPlane, Completion, GroupClass, IoOp, LoopConfig,
    LoopCx, ParkCeiling, ParkCeilingTooLong, Reserved, SubmitStats, TimerNotice, TimerOutcome,
    TimerOwner, TimerSet, Wait,
};

/// The runtime fixture's timer positions: one, which the timer test
/// reserves; the tests that arm no timer leave it unreserved, so it fills
/// while serving (a test host's cap).
const FIXTURE_OWNERS: Cap = Cap::entries::<1>("reactor-owners", CapFill::Serving);
/// The delivery test's positions: one more than a turn's allowance.
const COHORT_OWNERS: Cap = Cap::entries::<65>("delivery-owners", CapFill::Serving);

/// Driver that records how it was waited on and completes nothing.
#[derive(Debug, Default)]
struct NullDriver {
    waits: Vec<&'static str>,
    /// Each park's timeout, in the order the parks were asked for.
    park_timeouts: Vec<Option<Duration>>,
    pushed: usize,
}

impl BackendDriver for NullDriver {
    fn push(&mut self, _op: IoOp) {
        self.pushed += 1;
    }
    fn submit_and_reap(
        &mut self,
        _pool: &mut BufferPool,
        wait: Wait,
        _out: &mut Vec<Completion>,
    ) -> io::Result<usize> {
        self.waits.push(match wait {
            Wait::Poll => "poll",
            Wait::Park { timeout } => {
                self.park_timeouts.push(timeout);
                "park"
            }
        });
        Ok(0)
    }
    fn register_pool(&mut self, _pool: &mut BufferPool) -> io::Result<()> {
        Ok(())
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            backend: "null",
            multishot_accept: false,
            multishot_recv: false,
            provided_buffers: false,
            fixed_buffers: false,
            single_issuer: false,
            defer_taskrun: false,
            performance_tier: false,
        }
    }
    fn submit_stats(&self) -> SubmitStats {
        SubmitStats::default()
    }
}

fn test_loop(config: LoopConfig) -> CellLoop<NullDriver, StdClock> {
    test_loop_owning(config, 0).0
}

/// The fixture's loop with `owners` of its timer positions reserved, each
/// routed by its index, before the loop exists.
fn test_loop_owning(
    config: LoopConfig,
    owners: u64,
) -> (CellLoop<NullDriver, StdClock>, Vec<TimerOwner>) {
    let (census, serve_mark) = CapCensus::new(CellId(0));
    let (timers, owners) = timer_set(FIXTURE_OWNERS, &census, owners);
    let lp = CellLoop::new(
        NullDriver::default(),
        StdClock::new(),
        BufferPool::new(4, 1024),
        config,
        serve_mark,
        timers,
    );
    (lp, owners)
}

/// A set of `cap` positions on `census`, `owners` of them reserved.
fn timer_set(cap: Cap, census: &Rc<CapCensus>, owners: u64) -> (TimerSet, Vec<TimerOwner>) {
    let mut set = TimerSet::build(cap, census).expect("the fixture's timer set");
    let owners = (0..owners)
        .map(|route| match set.reserve(route) {
            Reserved::Owner(owner) => owner,
            other => panic!("a free position: {other:?}"),
        })
        .collect();
    (set, owners)
}

/// Two synthetic connections: A holds a deep pipeline, B sends one PING per
/// iteration. The plane honors the foreground budget, draining round-robin.
struct PipelinePlane {
    conn_a: VecDeque<u32>,
    b_pending: bool,
    b_served_iterations: Vec<bool>,
    a_done: u32,
}

impl PipelinePlane {
    fn new(pipeline_depth: u32) -> PipelinePlane {
        PipelinePlane {
            conn_a: (0..pipeline_depth).collect(),
            b_pending: false,
            b_served_iterations: Vec::new(),
            a_done: 0,
        }
    }
}

impl CellPlane for PipelinePlane {
    fn on_completion(&mut self, _cx: &mut LoopCx<'_>, _c: Completion) {}

    fn parse_execute(&mut self, cx: &mut LoopCx<'_>) {
        // B's PING arrives every iteration while A's pipeline drains.
        self.b_pending = true;
        let budget = cx.budget(GroupClass::Foreground);
        let mut used = 0;
        let mut b_served = false;
        // Round-robin: B first (it is one command), then A up to the budget.
        if self.b_pending && used < budget {
            self.b_pending = false;
            b_served = true;
            used += 1;
        }
        while used < budget && self.conn_a.pop_front().is_some() {
            self.a_done += 1;
            used += 1;
        }
        cx.charge(GroupClass::Foreground, used);
        self.b_served_iterations.push(b_served);
    }

    fn respond(&mut self, _cx: &mut LoopCx<'_>) {}

    fn fabric_out(&mut self, _cx: &mut LoopCx<'_>) -> bool {
        !self.conn_a.is_empty()
    }
}

#[test]
fn pipeline_cannot_starve_second_connection() {
    // The Vortex HoL test, structural form (M0-S07 AC): a 4096-command
    // pipeline on conn A drains across multiple bounded iterations, and conn
    // B is served in EVERY one of them. The protocol-level variant (real
    // RESP, p99 < 1 ms wall-clock) lands with inf-server integration.
    let mut lp = test_loop(LoopConfig::default());
    let mut plane = PipelinePlane::new(4096);
    let mut iterations = 0;
    while plane.a_done < 4096 {
        lp.run_iteration(&mut plane).expect("iteration");
        iterations += 1;
        assert!(iterations < 100, "budget must drain 4096 cmds in bounded slices");
    }
    assert!(iterations > 1, "the pipeline must NOT drain in one unbounded gulp");
    assert!(
        plane.b_served_iterations.iter().all(|&served| served),
        "B was starved in at least one iteration: {:?}",
        plane.b_served_iterations
    );
    // Histogram is always-on and saw every iteration.
    assert_eq!(lp.iteration_histogram().count(), iterations);
}

/// Plane that is busy for a fixed number of iterations, then idle.
struct BusyThenIdle {
    busy_left: u32,
}

impl CellPlane for BusyThenIdle {
    fn on_completion(&mut self, _cx: &mut LoopCx<'_>, _c: Completion) {}
    fn parse_execute(&mut self, cx: &mut LoopCx<'_>) {
        if self.busy_left > 0 {
            self.busy_left -= 1;
            cx.charge(GroupClass::Foreground, 1);
        }
    }
    fn respond(&mut self, _cx: &mut LoopCx<'_>) {}
}

/// Batch 61 (lane L11 `reactor.rs:239`): the first iteration polls — the
/// plane arms its accept in its first PARSE, which runs *after* the wait,
/// so a park there costs boot one full `park_default` for nothing.
#[test]
fn the_first_iteration_polls_before_anything_is_armed() {
    for spin_iters in [4, 0] {
        let mut lp = test_loop(LoopConfig { spin_iters, ..LoopConfig::default() });
        let mut plane = BusyThenIdle { busy_left: 0 };
        let first = lp.run_iteration(&mut plane).expect("iteration");
        assert!(!first.parked, "spin_iters {spin_iters}: boot parked: {:?}", lp.driver().waits);
    }
}

/// The cell's census serves from the loop's first iteration: every step
/// before it is assembly, and the mark is taken once (it is a token).
#[test]
fn the_first_iteration_takes_the_serve_mark() {
    let (census, serve_mark) = CapCensus::new(CellId(7));
    let (timers, _) = timer_set(FIXTURE_OWNERS, &census, 0);
    let mut lp = CellLoop::new(
        NullDriver::default(),
        StdClock::new(),
        BufferPool::new(4, 1024),
        LoopConfig::default(),
        serve_mark,
        timers,
    );
    assert_eq!(census.phase(), CensusPhase::Assembling, "constructing the loop is assembly");
    let mut plane = BusyThenIdle { busy_left: 1 };
    lp.run_iteration(&mut plane).expect("iteration");
    assert_eq!(census.phase(), CensusPhase::Serving);
    lp.run_iteration(&mut plane).expect("iteration");
    assert_eq!(census.phase(), CensusPhase::Serving);
    assert_eq!(census.cap_assembly_violations(), 0);
}

#[test]
fn idle_policy_spins_then_parks() {
    let spin = 4;
    let mut lp = test_loop(LoopConfig { spin_iters: spin, ..LoopConfig::default() });
    let mut plane = BusyThenIdle { busy_left: 3 };
    // Busy + spin-down + a few parked iterations.
    for _ in 0..(3 + spin + 3) {
        lp.run_iteration(&mut plane).expect("iteration");
    }
    let waits = &lp.driver().waits;
    // The first iteration polls (batch 61), work keeps it polling, then
    // `spin` more polls, then it parks forever.
    let tail: Vec<_> = waits.iter().rev().take(3).collect();
    assert!(tail.iter().all(|w| **w == "park"), "loop must park when idle: {waits:?}");
    assert!(
        waits.iter().filter(|w| **w == "poll").count() >= spin as usize,
        "loop must spin {spin} iterations before parking: {waits:?}"
    );
}

struct TimerPlane {
    owner: TimerOwner,
    fired: Vec<u64>,
    armed: bool,
}

impl CellPlane for TimerPlane {
    fn on_completion(&mut self, _cx: &mut LoopCx<'_>, _c: Completion) {}
    fn on_timer<'n>(&mut self, _cx: &mut LoopCx<'_>, notice: &'n TimerNotice) -> TimerOutcome<'n> {
        self.fired.push(notice.route());
        TimerOutcome::Complete
    }
    fn parse_execute(&mut self, cx: &mut LoopCx<'_>) {
        if !self.armed {
            self.armed = true;
            let deadline = cx.now + Nanos::from_millis(5);
            cx.timers().arm(&self.owner, deadline).expect("armed");
        }
    }
    fn respond(&mut self, _cx: &mut LoopCx<'_>) {}
}

#[test]
fn armed_timer_fires_through_the_loop() {
    let config = LoopConfig {
        spin_iters: 0,
        park_default: ParkCeiling::new(Duration::from_millis(1)).expect("within the maximum"),
        ..LoopConfig::default()
    };
    let (mut lp, owners) = test_loop_owning(config, 1);
    let owner = owners.into_iter().next().expect("an owner");
    let mut plane = TimerPlane { owner, fired: Vec::new(), armed: false };
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while plane.fired.is_empty() {
        lp.run_iteration(&mut plane).expect("iteration");
        assert!(std::time::Instant::now() < deadline, "timer never fired");
    }
    assert_eq!(plane.fired, vec![0], "the owner's route");
    assert_eq!(lp.armed_timers(), 0, "a completed arm leaves its owner idle");
}

#[test]
fn tripwires_report_the_frozen_names() {
    let mut lp = test_loop(LoopConfig::default());
    let mut plane = BusyThenIdle { busy_left: 5 };
    for _ in 0..10 {
        lp.run_iteration(&mut plane).expect("iteration");
    }
    let names: Vec<&str> = lp.tripwires().iter().map(|(n, _)| *n).collect();
    assert_eq!(
        names,
        vec![
            "sqes_per_submit",
            "cqes_per_reap",
            "cmds_per_iter",
            "fabric_msgs_per_batch",
            "loop_iter_p999_us"
        ]
    );
    let cmds_per_iter_x1000 = lp.tripwires()[2].1;
    assert!(cmds_per_iter_x1000 > 0, "5 commands over 10 iterations must register");
}

/// Records the order of plane phase calls; parse_execute spawns a parked
/// future (a pump awaiting a remote reply) whose waker fabric_in fires on
/// the next iteration — exactly the reply-wake path the remote-first lever
/// reorders around.
struct PhaseOrderPlane {
    calls: std::rc::Rc<std::cell::RefCell<Vec<&'static str>>>,
    waker: std::rc::Rc<std::cell::RefCell<Option<std::task::Waker>>>,
    done: std::rc::Rc<std::cell::Cell<bool>>,
    spawned: bool,
}

impl PhaseOrderPlane {
    fn new(spawned: bool) -> PhaseOrderPlane {
        PhaseOrderPlane {
            calls: std::rc::Rc::default(),
            waker: std::rc::Rc::default(),
            done: std::rc::Rc::default(),
            spawned,
        }
    }
}

/// Parks until `done` flips (the stored waker is the wake handle).
struct ParkedReply {
    waker: std::rc::Rc<std::cell::RefCell<Option<std::task::Waker>>>,
    done: std::rc::Rc<std::cell::Cell<bool>>,
}

impl Future for ParkedReply {
    type Output = ();
    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<()> {
        if self.done.get() {
            std::task::Poll::Ready(())
        } else {
            *self.waker.borrow_mut() = Some(cx.waker().clone());
            std::task::Poll::Pending
        }
    }
}

impl CellPlane for PhaseOrderPlane {
    fn on_completion(&mut self, _cx: &mut LoopCx<'_>, _c: Completion) {}
    fn fabric_in(&mut self, _cx: &mut LoopCx<'_>) {
        // The "reply arrived" moment: wake the parked pump.
        if let Some(waker) = self.waker.borrow_mut().take() {
            self.done.set(true);
            waker.wake();
        }
    }
    fn parse_execute(&mut self, cx: &mut LoopCx<'_>) {
        self.calls.borrow_mut().push("parse_execute");
        if !self.spawned {
            self.spawned = true;
            cx.executor.spawn_local(ParkedReply {
                waker: std::rc::Rc::clone(&self.waker),
                done: std::rc::Rc::clone(&self.done),
            });
        }
    }
    fn respond(&mut self, _cx: &mut LoopCx<'_>) {}
    fn fabric_out(&mut self, _cx: &mut LoopCx<'_>) -> bool {
        self.calls.borrow_mut().push("fabric_out");
        false
    }
}

#[test]
fn remote_first_execute_publishes_before_the_local_bulk() {
    // M2.5-S21 lever choreography: with the flag on, the iteration whose
    // FABRIC-IN wakes a parked pump runs an early FABRIC-OUT *before*
    // PARSE+EXECUTE (the hop overlaps local work); step 8 still runs.
    let mut lp = test_loop(LoopConfig { remote_first_execute: true, ..LoopConfig::default() });
    let mut plane = PhaseOrderPlane::new(false);
    let calls = std::rc::Rc::clone(&plane.calls);
    lp.run_iteration(&mut plane).expect("iteration"); // spawns + parks the pump
    calls.borrow_mut().clear();
    lp.run_iteration(&mut plane).expect("iteration"); // fabric_in wakes it
    assert_eq!(*calls.borrow(), vec!["fabric_out", "parse_execute", "fabric_out"]);
}

#[test]
fn remote_first_execute_early_flush_needs_a_resumed_future() {
    // No ready futures => no early FABRIC-OUT (the lever never adds an
    // unconditional flush); and with the lever off the reply-wake iteration
    // keeps the default single-flush ordering.
    for (lever, spawned) in [(false, false), (true, true)] {
        let mut lp = test_loop(LoopConfig { remote_first_execute: lever, ..LoopConfig::default() });
        let mut plane = PhaseOrderPlane::new(spawned);
        let calls = std::rc::Rc::clone(&plane.calls);
        lp.run_iteration(&mut plane).expect("iteration");
        calls.borrow_mut().clear();
        lp.run_iteration(&mut plane).expect("iteration");
        assert_eq!(
            *calls.borrow(),
            vec!["parse_execute", "fabric_out"],
            "lever={lever} spawned={spawned}"
        );
    }
}

/// F-L11-07: the wake lands in MAINTAIN — after `run_ready`, with no
/// completion, command, fabric message or queued op beside it (the shape
/// of every wake driven by another thread's state: `ddl_waiters`,
/// `ckpt_waiters`, `stall_waiters`). The test arms the wake itself, in
/// an otherwise idle iteration.
struct WakeInMaintain {
    waker: std::rc::Rc<std::cell::RefCell<Option<std::task::Waker>>>,
    done: std::rc::Rc<std::cell::Cell<bool>>,
    spawned: bool,
    wake_now: bool,
}

impl CellPlane for WakeInMaintain {
    fn on_completion(&mut self, _cx: &mut LoopCx<'_>, _c: Completion) {}
    fn parse_execute(&mut self, cx: &mut LoopCx<'_>) {
        if !self.spawned {
            self.spawned = true;
            cx.executor.spawn_local(ParkedReply {
                waker: std::rc::Rc::clone(&self.waker),
                done: std::rc::Rc::clone(&self.done),
            });
        }
    }
    fn maintain(&mut self, _cx: &mut LoopCx<'_>) {
        if !self.wake_now {
            return;
        }
        if let Some(waker) = self.waker.borrow_mut().take() {
            self.done.set(true);
            waker.wake();
        }
    }
    fn respond(&mut self, _cx: &mut LoopCx<'_>) {}
}

#[test]
fn a_task_woken_in_maintain_is_polled_before_any_park() {
    // spin_iters = 1 reaches the park decision through the idle count;
    // spin_iters = 0 parks every idle iteration — the ready queue alone
    // must veto it.
    for spin_iters in [1, 0] {
        let mut lp = test_loop(LoopConfig {
            spin_iters,
            // Ten seconds asked for, above the maximum: the loop takes the maximum.
            park_default: ParkCeiling::new(Duration::from_secs(10)).unwrap_or(ParkCeiling::MAX),
            ..LoopConfig::default()
        });
        let mut plane = WakeInMaintain {
            waker: std::rc::Rc::default(),
            done: std::rc::Rc::default(),
            spawned: false,
            wake_now: false,
        };
        let spawn = lp.run_iteration(&mut plane).expect("iteration");
        assert_eq!(spawn.polled, 1, "the task is spawned and parks");
        let idle = lp.run_iteration(&mut plane).expect("iteration");
        assert_eq!((idle.polled, idle.parked), (0, spin_iters == 0));
        // The parked iteration: nothing happens in it except the wake in
        // MAINTAIN (a board edge seen after run_ready).
        plane.wake_now = true;
        let wake = lp.run_iteration(&mut plane).expect("iteration");
        assert_eq!(
            (wake.polled, wake.parked),
            (0, true),
            "the wake lands in a parked, idle iteration"
        );
        // The runnable task is work: no park on top of it, and it runs now.
        let next = lp.run_iteration(&mut plane).expect("iteration");
        assert!(
            !next.parked,
            "spin_iters {spin_iters}: parked on top of a runnable task: {:?}",
            lp.driver().waits
        );
        assert_eq!(next.polled, 1, "the woken task runs on the next iteration");
        assert_eq!(lp.executor().live_tasks(), 0, "and completes");
    }
}

/// Arms `owners` timers one millisecond out in its first PARSE, then counts
/// the callbacks each iteration begins. It reports no work, so the loop's
/// idle policy alone decides each wait.
struct DueCohort {
    owners: Vec<TimerOwner>,
    armed: bool,
    begun: u64,
}

impl CellPlane for DueCohort {
    fn on_completion(&mut self, _cx: &mut LoopCx<'_>, _c: Completion) {}
    fn on_timer<'n>(&mut self, _cx: &mut LoopCx<'_>, _notice: &'n TimerNotice) -> TimerOutcome<'n> {
        self.begun += 1;
        TimerOutcome::Complete
    }
    fn parse_execute(&mut self, cx: &mut LoopCx<'_>) {
        if !self.armed {
            self.armed = true;
            let deadline = cx.now + Nanos::from_millis(1);
            let mut timers = cx.timers();
            for owner in &self.owners {
                timers.arm(owner, deadline).expect("armed");
            }
        }
    }
    fn respond(&mut self, _cx: &mut LoopCx<'_>) {}
}

/// Delivery and the park (ADR-0154 D3): 65 timers fall due at once. A turn
/// begins at most 64 callbacks, the 65th waits for the next turn's step 1,
/// and an iteration whose decision finds a timer due polls instead of
/// parking, at `spin_iters` 1, where the idle count reaches the decision,
/// and 0, where every idle iteration reaches it. The oracle is the virtual
/// clock, the callbacks counted per iteration and the waits the driver was
/// handed: iteration 1 arms all 65 and reports no work, the clock steps
/// 2 ms, iteration 2 begins 64 callbacks, iteration 3 one, and iteration 4,
/// with nothing armed, parks for `park_default`.
#[test]
fn sixty_five_due_timers_take_two_turns_and_no_park() {
    let park_default = Duration::from_millis(100);
    let mut readings = Vec::new();
    for spin_iters in [1, 0] {
        let clock = Rc::new(VirtualClock::new(Nanos::from_millis(10)));
        let (census, serve_mark) = CapCensus::new(CellId(0));
        let (timers, owners) = timer_set(COHORT_OWNERS, &census, 65);
        let ceiling = ParkCeiling::new(park_default).expect("within the maximum");
        let config = LoopConfig { spin_iters, park_default: ceiling, ..LoopConfig::default() };
        let mut lp = CellLoop::new(
            NullDriver::default(),
            Rc::clone(&clock),
            BufferPool::new(4, 1024),
            config,
            serve_mark,
            timers,
        );
        let mut plane = DueCohort { owners, armed: false, begun: 0 };
        let mut begun = Vec::new();
        for iteration in 1..=4 {
            let before = plane.begun;
            lp.run_iteration(&mut plane).expect("iteration");
            begun.push(plane.begun - before);
            if iteration == 1 {
                clock.advance(Nanos::from_millis(2));
            }
        }
        let driver = lp.driver();
        readings.push((spin_iters, begun, driver.waits.clone(), driver.park_timeouts.clone()));
    }
    for reading in &readings {
        println!("delivery {reading:?}");
    }
    for (spin_iters, begun, waits, park_timeouts) in readings {
        assert_eq!(
            (begun, waits, park_timeouts),
            (vec![0, 64, 1, 0], vec!["poll", "poll", "poll", "park"], vec![Some(park_default)]),
            "spin_iters {spin_iters}: callbacks begun per iteration, the waits, the park timeouts"
        );
    }
}

/// Where one iteration offers the cold host its delivery, and when the
/// task woken by this turn's replies polls: the fixture's two marks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OrderMark {
    Delivery,
    Poll,
}

/// The delivery-placement fixture: FABRIC-IN spawns one task, ready at
/// once, which records its poll; the plane records a mark where a cold
/// delivery can run, which at HEAD is the head of `parse_execute`.
struct DeliveryOrderPlane {
    trace: Rc<std::cell::RefCell<Vec<OrderMark>>>,
    spawned: bool,
}

impl CellPlane for DeliveryOrderPlane {
    fn on_completion(&mut self, _cx: &mut LoopCx<'_>, c: Completion) {
        panic!("the fixture issues no op: {c:?}");
    }
    fn fabric_in(&mut self, cx: &mut LoopCx<'_>) {
        assert!(!self.spawned, "the fixture admits one task");
        self.spawned = true;
        let trace = Rc::clone(&self.trace);
        cx.executor.spawn_local(async move {
            trace.borrow_mut().push(OrderMark::Poll);
        });
    }
    fn parse_execute(&mut self, _cx: &mut LoopCx<'_>) {
        self.trace.borrow_mut().push(OrderMark::Delivery);
    }
    fn respond(&mut self, _cx: &mut LoopCx<'_>) {}
}

/// One iteration of the fixture: its marks in order, after its controls
/// (one driver entry, one poll, nothing pushed, no buffer leased or staged).
fn delivery_order(remote_first_execute: bool) -> Vec<OrderMark> {
    let config =
        LoopConfig { spin_iters: 0, exec_budget: 1, remote_first_execute, ..LoopConfig::default() };
    let mut lp = test_loop(config);
    let trace = Rc::new(std::cell::RefCell::new(Vec::new()));
    let mut plane = DeliveryOrderPlane { trace: Rc::clone(&trace), spawned: false };
    let stats = lp.run_iteration(&mut plane).expect("iteration");
    assert_eq!(lp.driver().waits.len(), 1, "one driver entry");
    assert_eq!(lp.driver().pushed, 0, "nothing pushed");
    assert_eq!(stats.polled, 1, "one task poll");
    assert_eq!((lp.pool().leased(), lp.pool().staged()), (0, 0), "no buffer held");
    trace.take()
}

/// A turn's cold delivery runs before any task woken by its replies polls,
/// in both `remote_first_execute` modes, so a resumed task never runs ahead
/// of the delivery its turn owes (ADR-0181 R2). The default mode places the
/// mark first, the control; with `remote_first_execute` the loop polls the
/// woken task before `parse_execute`, the first place HEAD gives a delivery.
#[test]
#[ignore = "with remote_first_execute the loop polls tasks woken by the turn's replies before \
            parse_execute, the first place a turn can deliver cold reads, so a resumed task runs \
            ahead of the turn's delivery; red until the delivery runs before either poll"]
fn a_turns_cold_delivery_precedes_a_woken_tasks_poll_in_both_modes() {
    let marks = [OrderMark::Delivery, OrderMark::Poll];
    assert_eq!(delivery_order(false), marks, "the control: the default mode");
    assert_eq!(
        delivery_order(true),
        marks,
        "with remote_first_execute a woken task polled before the turn's delivery"
    );
}

/// A driver whose park, once the plane has exported a parked task's waker, wakes that waker from
/// another thread while it parks: a foreign wake pushed during the park, which rings no doorbell.
/// It records each park's timeout.
#[derive(Default)]
struct ForeignWakeDriver {
    park_timeouts: Vec<Option<Duration>>,
    waker: std::sync::Arc<std::sync::Mutex<Option<std::task::Waker>>>,
    /// The park, by its index, during which the foreign wake was pushed.
    woken_in: Option<usize>,
}

impl BackendDriver for ForeignWakeDriver {
    fn push(&mut self, _op: IoOp) {}
    fn submit_and_reap(
        &mut self,
        _pool: &mut BufferPool,
        wait: Wait,
        _out: &mut Vec<Completion>,
    ) -> io::Result<usize> {
        if let Wait::Park { timeout } = wait {
            self.park_timeouts.push(timeout);
            let exported = self.waker.lock().expect("the waker slot").take();
            if let Some(waker) = exported {
                std::thread::spawn(move || waker.wake()).join().expect("the waking thread");
                self.woken_in = Some(self.park_timeouts.len() - 1);
            }
        }
        Ok(0)
    }
    fn register_pool(&mut self, _pool: &mut BufferPool) -> io::Result<()> {
        Ok(())
    }
    fn capabilities(&self) -> Capabilities {
        NullDriver::default().capabilities()
    }
    fn submit_stats(&self) -> SubmitStats {
        SubmitStats::default()
    }
}

/// Spawns one task in its first PARSE that parks, puts a clone of its waker where the driver
/// reads it, and finishes on its next poll; it counts the task's polls.
struct ExportParked {
    waker: std::sync::Arc<std::sync::Mutex<Option<std::task::Waker>>>,
    polls: Rc<std::cell::Cell<u32>>,
    spawned: bool,
}

impl CellPlane for ExportParked {
    fn on_completion(&mut self, _cx: &mut LoopCx<'_>, _c: Completion) {}
    fn parse_execute(&mut self, cx: &mut LoopCx<'_>) {
        if self.spawned {
            return;
        }
        self.spawned = true;
        let slot = std::sync::Arc::clone(&self.waker);
        let polls = Rc::clone(&self.polls);
        cx.executor.spawn_local(core::future::poll_fn(move |cx| {
            polls.set(polls.get() + 1);
            if polls.get() > 1 {
                return std::task::Poll::Ready(());
            }
            *slot.lock().expect("the waker slot") = Some(cx.waker().clone());
            std::task::Poll::Pending
        }));
    }
    fn respond(&mut self, _cx: &mut LoopCx<'_>) {}
}

/// Every park a cell loop issues carries a timeout of at most its ceiling (ADR-0003 A1.2.7), and
/// a foreign wake pushed during a park, the only ready task, is drained and polled by the
/// iteration that park opens. A ceiling above `PARK_CEILING_MAX` is refused.
#[test]
fn an_idle_park_carries_its_ceiling() {
    let over = PARK_CEILING_MAX + Duration::from_nanos(1);
    assert_eq!(
        ParkCeiling::new(over),
        Err(ParkCeilingTooLong { asked: over }),
        "a ceiling one nanosecond above the maximum is refused"
    );
    assert_eq!(ParkCeiling::new(PARK_CEILING_MAX), Ok(ParkCeiling::MAX));
    let ceiling = ParkCeiling::new(Duration::from_millis(7)).expect("within the maximum");
    let driver = ForeignWakeDriver::default();
    let mut plane = ExportParked {
        waker: std::sync::Arc::clone(&driver.waker),
        polls: Rc::default(),
        spawned: false,
    };
    let (census, serve_mark) = CapCensus::new(CellId(0));
    let (timers, _) = timer_set(FIXTURE_OWNERS, &census, 0);
    let config = LoopConfig { spin_iters: 0, park_default: ceiling, ..LoopConfig::default() };
    let mut lp = CellLoop::new(
        driver,
        StdClock::new(),
        BufferPool::new(4, 1024),
        config,
        serve_mark,
        timers,
    );
    let first = lp.run_iteration(&mut plane).expect("iteration");
    assert_eq!((first.parked, first.polled), (false, 1), "the task is spawned and parks");
    let woken = lp.run_iteration(&mut plane).expect("iteration");
    assert_eq!(lp.driver().woken_in, Some(0), "the foreign wake was pushed during the first park");
    assert!(woken.parked, "the iteration opened with a park");
    assert_eq!(woken.polled, 1, "the park's own iteration drained the wake and polled the task");
    assert_eq!(plane.polls.get(), 2);
    assert_eq!(lp.executor().waker_foreign_ops(), 1, "one foreign wake, which consumed its waker");
    for _ in 0..3 {
        lp.run_iteration(&mut plane).expect("iteration");
    }
    let timeouts = &lp.driver().park_timeouts;
    assert!(timeouts.len() >= 3, "the idle loop parked: {timeouts:?}");
    assert!(
        timeouts.iter().all(|t| t.is_some_and(|t| t <= ceiling.get())),
        "every park carries a timeout of at most its ceiling: {timeouts:?}"
    );
}
