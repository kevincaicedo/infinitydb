//! Reactor-loop structure (M0-S07): bounded steps, HoL fairness via budgets,
//! spin→park idle policy, timer-driven park timeouts, and the always-on
//! iteration histogram. Uses a null in-memory driver — backend behavior has
//! its own conformance suite.

use std::collections::VecDeque;
use std::io;
use std::time::Duration;

use inf_alloc::BufferPool;
use inf_foundation::time::StdClock;
use inf_runtime::{
    BackendDriver, Capabilities, CellLoop, CellPlane, Completion, GroupClass, IoOp, LoopConfig,
    LoopCx, SubmitStats, Wait,
};

/// Driver that records how it was waited on and completes nothing.
#[derive(Debug, Default)]
struct NullDriver {
    waits: Vec<&'static str>,
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
            Wait::Park { .. } => "park",
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
    CellLoop::new(NullDriver::default(), StdClock::new(), BufferPool::new(4, 1024), config)
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
    // First iteration parks (nothing has happened yet), then work keeps it
    // polling, then `spin` more polls, then parks forever.
    let tail: Vec<_> = waits.iter().rev().take(3).collect();
    assert!(tail.iter().all(|w| **w == "park"), "loop must park when idle: {waits:?}");
    assert!(
        waits.iter().filter(|w| **w == "poll").count() >= spin as usize,
        "loop must spin {spin} iterations before parking: {waits:?}"
    );
}

struct TimerPlane {
    fired: Vec<u64>,
    armed: bool,
}

impl CellPlane for TimerPlane {
    fn on_completion(&mut self, _cx: &mut LoopCx<'_>, _c: Completion) {}
    fn on_timer(&mut self, _cx: &mut LoopCx<'_>, key: u64) {
        self.fired.push(key);
    }
    fn parse_execute(&mut self, cx: &mut LoopCx<'_>) {
        if !self.armed {
            self.armed = true;
            let deadline = cx.now + inf_foundation::time::Nanos::from_millis(5);
            cx.timers.insert(deadline, 99);
        }
    }
    fn respond(&mut self, _cx: &mut LoopCx<'_>) {}
}

#[test]
fn armed_timer_fires_through_the_loop() {
    let mut lp = test_loop(LoopConfig {
        spin_iters: 0,
        park_default: Some(Duration::from_millis(1)),
        ..LoopConfig::default()
    });
    let mut plane = TimerPlane { fired: Vec::new(), armed: false };
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while plane.fired.is_empty() {
        lp.run_iteration(&mut plane).expect("iteration");
        assert!(std::time::Instant::now() < deadline, "timer never fired");
    }
    assert_eq!(plane.fired, vec![99]);
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
