//! The reactor loop (master plan §5.1): the 10 steps, typed budgets, and the
//! always-on iteration histogram. Tail protection is structural — every step
//! is bounded, so one connection's 4096-command pipeline cannot starve its
//! neighbours (the Vortex 966 ms lesson, M0-S07).
//!
//! Step map (numbers from §5.1):
//! ```text
//!  9+1  SUBMIT+REAP   one driver entry per iteration: flush queued ops,
//!                     harvest completions (Poll while spinning, Park idle)
//!   1   dispatch      plane.on_completion per completion; timers fire
//!   2   FABRIC-IN     plane.fabric_in (bounded drain)
//!  3+4  PARSE+EXECUTE plane.parse_execute under the foreground budget,
//!                     then executor.run_ready for resumed futures
//!   5   MAINTAIN      plane.maintain under the maintenance budget
//!   6   LOG           plane.seal_log (no-op at M0)
//!   7   RESPOND       plane.respond (queues Send ops)
//!   8   FABRIC-OUT    plane.fabric_out (publish + doorbells)
//!  10   IDLE          spin → park decision for the next iteration
//! ```
//! Ops queued anywhere in steps 1–8 ride the single submit at the top of the
//! next iteration (L3: one backend entry per iteration).
//!
//! The iteration histogram records **active** wall time (post-park to end of
//! FABRIC-OUT) in microseconds — `loop_iter_p999_us` is the frozen tripwire.

use std::io;
use std::time::Duration;

use inf_alloc::BufferPool;
use inf_foundation::LogHistogram;
use inf_foundation::time::{Clock, Nanos};

use crate::driver::{BackendDriver, Completion, IoOp, Wait};
use crate::executor::CellExecutor;
use crate::sched::{GroupClass, GroupScheduler};
use crate::timer::TimerWheel;

/// Loop tuning. `Default` is the M0 reference shape.
#[derive(Copy, Clone, Debug)]
pub struct LoopConfig {
    /// Busy iterations with `Wait::Poll` after work goes quiet, before
    /// parking (adaptive idle, §5.1 step 10).
    pub spin_iters: u32,
    /// Max resumed futures polled per iteration (EXECUTE slice bound).
    pub exec_budget: usize,
    /// Park ceiling when no timer is armed; `None` parks indefinitely.
    pub park_default: Option<Duration>,
}

impl Default for LoopConfig {
    fn default() -> LoopConfig {
        LoopConfig {
            spin_iters: 64,
            exec_budget: 1024,
            park_default: Some(Duration::from_millis(100)),
        }
    }
}

/// Per-iteration result (also the simulator's stepping observable).
#[derive(Copy, Clone, Debug, Default)]
pub struct IterStats {
    /// Completions dispatched this iteration.
    pub reaped: usize,
    /// Resumed futures polled.
    pub polled: usize,
    /// Commands the plane reported executing.
    pub commands: u64,
    /// Fabric messages the plane reported moving (in + out).
    pub fabric_msgs: u64,
    /// Ops submitted at the top of this iteration.
    pub submitted: u64,
    /// Whether the iteration began by parking (idle).
    pub parked: bool,
    /// Active wall time (excludes park).
    pub active: Nanos,
}

/// What the loop hands each plane step: op queueing, buffers, executor,
/// timers, budgets, and work accounting. Ops pushed here are flushed in the
/// next single submit — `LoopCx` never performs a syscall.
pub struct LoopCx<'a> {
    pub now: Nanos,
    pub pool: &'a mut BufferPool,
    pub executor: &'a mut CellExecutor,
    pub timers: &'a mut TimerWheel,
    ops: &'a mut Vec<IoOp>,
    sched: &'a mut GroupScheduler,
    commands: u64,
    fabric_msgs: u64,
}

impl LoopCx<'_> {
    /// Queue an op for the next submit (step 9). No syscall.
    pub fn push(&mut self, op: IoOp) {
        self.ops.push(op);
    }

    /// Remaining budget for `class` this iteration.
    pub fn budget(&self, class: GroupClass) -> u32 {
        self.sched.budget(class)
    }

    /// Charge `class` for work done; also feeds `cmds_per_iter` when the
    /// class is foreground.
    pub fn charge(&mut self, class: GroupClass, units: u32) {
        self.sched.charge(class, units);
        if class == GroupClass::Foreground {
            self.commands += u64::from(units);
        }
    }

    /// Report fabric messages moved (feeds `fabric_msgs_per_batch`).
    pub fn note_fabric(&mut self, msgs: u64) {
        self.fabric_msgs += msgs;
    }
}

impl core::fmt::Debug for LoopCx<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "LoopCx {{ now: {}, queued_ops: {} }}", self.now, self.ops.len())
    }
}

/// One cell's data plane, driven by [`CellLoop::run_iteration`]. Implemented
/// by `inf-server`'s cell (real plane) and by test/sim planes. Default
/// no-ops let M0 planes implement only what exists (e.g. `seal_log` is M2).
pub trait CellPlane {
    /// Step 1: one reaped completion (accept/recv/send/close result).
    fn on_completion(&mut self, cx: &mut LoopCx<'_>, c: Completion);
    /// A timer armed via `cx.timers` fired with `key`.
    fn on_timer(&mut self, cx: &mut LoopCx<'_>, key: u64) {
        let _ = (cx, key);
    }
    /// Step 2: drain inbound fabric rings (bounded).
    fn fabric_in(&mut self, cx: &mut LoopCx<'_>) {
        let _ = cx;
    }
    /// Steps 3+4: parse frames and execute commands under
    /// `cx.budget(Foreground)`, charging what was used.
    fn parse_execute(&mut self, cx: &mut LoopCx<'_>);
    /// Step 5: maintenance slices under `cx.budget(Maintenance)` (M0: stats
    /// flush only).
    fn maintain(&mut self, cx: &mut LoopCx<'_>) {
        let _ = cx;
    }
    /// Step 6: seal the iteration's log batch (M0: no-op, M2: writev+fsync).
    fn seal_log(&mut self, cx: &mut LoopCx<'_>) {
        let _ = cx;
    }
    /// Step 7: flush response buffers as Send ops.
    fn respond(&mut self, cx: &mut LoopCx<'_>);
    /// Step 8: publish outbound fabric batches. Return `true` if more work
    /// is pending (keeps the loop in Poll instead of parking).
    fn fabric_out(&mut self, cx: &mut LoopCx<'_>) -> bool {
        let _ = cx;
        false
    }
    /// Called when the loop is about to park. The plane may publish its
    /// "parked" flag here (doorbell wakeups, M0-R1) and must return `true`
    /// if a final check found pending work — the loop then polls instead of
    /// parking. Default: park unconditionally.
    fn before_park(&mut self) -> bool {
        false
    }
}

/// The cell reactor. Owns the driver, buffer pool, executor, timers,
/// scheduler, and the always-on iteration histogram. `run_iteration` is a
/// single step — `inf-sim` drives it deterministically; `infinityd` wraps it
/// in `loop {}`.
pub struct CellLoop<D: BackendDriver, C: Clock> {
    driver: D,
    clock: C,
    pool: BufferPool,
    executor: CellExecutor,
    timers: TimerWheel,
    sched: GroupScheduler,
    config: LoopConfig,
    ops: Vec<IoOp>,
    completions: Vec<Completion>,
    iter_hist_us: LogHistogram,
    spin_left: u32,
    iterations: u64,
    submits: u64,
    sqes_total: u64,
    cqes_total: u64,
    commands_total: u64,
    fabric_total: u64,
}

impl<D: BackendDriver, C: Clock> CellLoop<D, C> {
    pub fn new(driver: D, clock: C, pool: BufferPool, config: LoopConfig) -> CellLoop<D, C> {
        CellLoop {
            driver,
            clock,
            pool,
            executor: CellExecutor::new(1024),
            timers: TimerWheel::new(),
            sched: GroupScheduler::m0_default(),
            config,
            ops: Vec::with_capacity(256),
            completions: Vec::with_capacity(256),
            iter_hist_us: LogHistogram::new(),
            spin_left: 0,
            iterations: 0,
            submits: 0,
            sqes_total: 0,
            cqes_total: 0,
            commands_total: 0,
            fabric_total: 0,
        }
    }

    /// One loop iteration. See module docs for the step map.
    ///
    /// # Errors
    /// Backend-fatal errors only (the ring/queue itself failed); per-op
    /// errors arrive as completions.
    pub fn run_iteration(&mut self, plane: &mut impl CellPlane) -> io::Result<IterStats> {
        // ---- steps 9 (prev iteration's ops) + 1 (reap): ONE driver entry.
        // `before_park` runs only when spin is exhausted: the plane
        // publishes its parked flag and vetoes the park if a final doorbell
        // check finds work (the lost-wakeup handshake, M0-R1).
        let parked = self.spin_left == 0 && !plane.before_park();
        let wait = if parked { Wait::Park { timeout: self.park_timeout() } } else { Wait::Poll };
        let submitted = self.ops.len() as u64;
        for op in self.ops.drain(..) {
            self.driver.push(op);
        }
        self.completions.clear();
        let reaped = self.driver.submit_and_reap(&mut self.pool, wait, &mut self.completions)?;
        // Count REAL backend work (driver stats), not plane-pushed ops: the
        // driver adds provided-buffer restocks, multishot re-arms, and
        // short-write resubmits — exactly the syscall amortization L3 gates.
        #[cfg(not(feature = "no-tripwires"))]
        {
            let stats = self.driver.submit_stats();
            self.submits += stats.syscalls.max(1);
            self.sqes_total += stats.sqes;
            self.cqes_total += reaped as u64;
        }

        // Active time starts after the (possible) park.
        let start = self.clock.now();
        self.sched.refill();

        let commands;
        let fabric_msgs;
        let polled;
        {
            let mut cx = LoopCx {
                now: start,
                pool: &mut self.pool,
                executor: &mut self.executor,
                timers: &mut self.timers,
                ops: &mut self.ops,
                sched: &mut self.sched,
                commands: 0,
                fabric_msgs: 0,
            };

            // ---- step 1: dispatch completions, then due timers.
            for c in self.completions.drain(..) {
                plane.on_completion(&mut cx, c);
            }
            // Timers: collect-then-dispatch keeps `cx` exclusive.
            let mut due = Vec::new();
            cx.timers.advance(start, |key| due.push(key));
            for key in due {
                plane.on_timer(&mut cx, key);
            }

            // ---- step 2: FABRIC-IN.
            plane.fabric_in(&mut cx);

            // ---- steps 3+4: PARSE + EXECUTE (foreground budget).
            plane.parse_execute(&mut cx);
            polled = cx.executor.run_ready(self.config.exec_budget);

            // ---- step 5: MAINTAIN (maintenance budget).
            plane.maintain(&mut cx);

            // ---- step 6: LOG.
            plane.seal_log(&mut cx);

            // ---- step 7: RESPOND.
            plane.respond(&mut cx);

            // ---- step 8: FABRIC-OUT.
            let fabric_pending = plane.fabric_out(&mut cx);

            commands = cx.commands;
            fabric_msgs = cx.fabric_msgs;

            // ---- step 10: IDLE policy for the next iteration.
            let had_work = reaped > 0
                || polled > 0
                || commands > 0
                || fabric_msgs > 0
                || fabric_pending
                || !cx.ops.is_empty();
            if had_work {
                self.spin_left = self.config.spin_iters;
            } else {
                self.spin_left = self.spin_left.saturating_sub(1);
            }
        }

        let active = self.clock.now().saturating_sub(start);
        // The always-on tripwires (§16). `no-tripwires` exists ONLY for the
        // M0-S19 A/B artifact proving they cost nothing — never ship it.
        #[cfg(not(feature = "no-tripwires"))]
        {
            self.iter_hist_us.record(active.as_micros());
            self.iterations += 1;
            self.commands_total += commands;
            self.fabric_total += fabric_msgs;
        }

        Ok(IterStats { reaped, polled, commands, fabric_msgs, submitted, parked, active })
    }

    fn park_timeout(&self) -> Option<Duration> {
        let to_next_timer = self
            .timers
            .next_deadline()
            .map(|deadline| Duration::from_nanos(deadline.saturating_sub(self.clock.now()).0));
        match (to_next_timer, self.config.park_default) {
            (Some(t), Some(d)) => Some(t.min(d)),
            (Some(t), None) => Some(t),
            (None, d) => d,
        }
    }

    /// Always-on iteration histogram (µs). `loop_iter_p999_us` =
    /// `.percentile(99.9)` — the §6 gate reads this.
    pub fn iteration_histogram(&self) -> &LogHistogram {
        &self.iter_hist_us
    }

    /// Raw lifetime counters `(submits, sqes, cqes, iterations, commands,
    /// fabric_msgs)` — the scrape side computes *windowed* deltas from two
    /// snapshots so idle iterations don't dilute under-load ratios (the
    /// lifetime ratios in [`tripwires`](Self::tripwires) include parks).
    pub fn counters(&self) -> [u64; 6] {
        [
            self.submits,
            self.sqes_total,
            self.cqes_total,
            self.iterations,
            self.commands_total,
            self.fabric_total,
        ]
    }

    /// Frozen tripwire snapshot for the control-thread scrape (M0-S19):
    /// `(sqes_per_submit, cqes_per_reap, cmds_per_iter, fabric_msgs_per_batch,
    /// loop_iter_p999_us)`, ratios ×1000 to stay integral.
    pub fn tripwires(&self) -> [(&'static str, u64); 5] {
        use inf_foundation::tripwire as tw;
        let per = |total: u64, n: u64| (total * 1000).checked_div(n).unwrap_or(0);
        [
            (tw::SQES_PER_SUBMIT, per(self.sqes_total, self.submits)),
            (tw::CQES_PER_REAP, per(self.cqes_total, self.submits)),
            (tw::CMDS_PER_ITER, per(self.commands_total, self.iterations)),
            (tw::FABRIC_MSGS_PER_BATCH, per(self.fabric_total, self.iterations)),
            (tw::LOOP_ITER_P999_US, self.iter_hist_us.percentile(99.9)),
        ]
    }

    pub fn driver(&self) -> &D {
        &self.driver
    }

    pub fn pool(&self) -> &BufferPool {
        &self.pool
    }

    pub fn executor(&self) -> &CellExecutor {
        &self.executor
    }
}

impl<D: BackendDriver, C: Clock> core::fmt::Debug for CellLoop<D, C> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "CellLoop {{ iterations: {}, live_tasks: {}, armed_timers: {} }}",
            self.iterations,
            self.executor.live_tasks(),
            self.timers.armed()
        )
    }
}
