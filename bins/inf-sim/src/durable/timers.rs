//! The timers' DST rows: how many timers a cell holds armed while its
//! fill policy holds frame after frame.
//!
//! - **The armed peak.** After every scheduler step, each cell's
//!   `CellLoop::armed_timers()`; the run keeps the largest, cell scope. A
//!   cell's timers are bounded by its roles (`TimerRole::COUNT`) only if
//!   every hold episode leaves at most one timer behind: its role's owner.
//! - **An issue cancels its episode's arm.** At every step where a cell's
//!   durable plane holds no open hold episode, the cell's armed timers:
//!   the run keeps the largest, which is the everysec owner's one arm, or
//!   none before the durable plane exists. A hold's arm left behind by the
//!   issue that ended its episode reads two until its window passes. The
//!   steps read are counted, so a run that read none is visible.
//! - **Engagement.** The fill episodes that begin inside one hold window:
//!   each cell's `frame_waits_fill` (the product's episode count) read at
//!   every step and differenced over tumbling windows of the scenario's
//!   fill window; the run keeps the largest rise. A dense row that rose by
//!   fewer than 8 inside one window opened too few episodes to say
//!   anything about their timers.
//! - **TM3, the wake oracle**, on a leg of `m2-fill-tick` and of
//!   `m2-group-hold` (`--park-at-once`: the scenario at spin 0, so every
//!   iteration after a hold decides with the hold in hand). A park is
//!   judged when the cell's iteration before it left records staged with
//!   no log-segment write or sync in flight on the device, and its own
//!   submit carried none: its timeout, the kept `Wait`'s, is at most the
//!   larger hold window + 1 ms, since a hold arms its window rounded up to
//!   whole milliseconds. A run that judged no park is `VACUOUS`. It reads
//!   the loop's decision and wait, one gauge (`records_staged`), the
//!   driver's ops and the scenario's windows: no timer state.
//! - **The dense leg** (`m2-fill-dense`, a `sim-smoke` row, whose run
//!   judges the three readings above). `m2-fill-tick` at steps of at most
//!   50 µs, with 300
//!   closed-loop everysec writers whose 700-byte values reach their cell
//!   whole at each reap, on 4 MiB segments: they fill the 64 KiB target in
//!   about two steps, so an episode is a hold and a seal a LOG step or two
//!   apart and a 50 ms window holds hundreds of them. Its control is
//!   `m2-fill-tick` as it stands, one writer thinking up to 3 s, whose peak
//!   is the everysec timer and the few episodes one sparse writer opens.

use std::time::Duration;

use inf_foundation::time::Nanos;
use inf_runtime::Wait;

use super::ledger_wedge::log_segment;
use super::{DurableScenario, Node};

/// Episodes the dense leg must begin inside one window, else the row is
/// vacuous.
const ENGAGED_EPISODES_MIN: u64 = 8;
/// The positions a server cell's timers fill, one per role.
const SERVER_TIMER_ROLES: u32 = inf_server::TimerRole::COUNT;
/// A durable cell's timers armed between hold episodes: its everysec
/// owner's (no accept fails on these legs).
const UNHELD_ARMED_MAX: u32 = 1;

/// What the timers' rows read over a run's traffic phase.
#[derive(Clone, Copy, Debug, Default)]
pub struct TimersReport {
    /// The most timers any cell held armed after any scheduler step.
    pub armed_timers_max: u32,
    /// The most fill episodes one cell began inside one fill window.
    pub fill_episodes_window_max: u64,
    /// The most timers a cell held armed at a step where its durable plane
    /// held no open hold episode.
    pub armed_unheld_max: u32,
    /// The steps and cells read with a durable plane and no open episode.
    pub unheld_reads: u64,
}

/// The rows' per-cell state over one life of a node.
pub(super) struct TimersOracle {
    /// The scenario's fill window: the span the episodes are counted over.
    window: Nanos,
    /// Per cell: the window's start and the cell's episode count then.
    windows: Vec<Option<(Nanos, u64)>>,
}

impl TimersReport {
    /// The dense leg's verdict over its run: engaged, the armed peak within
    /// the roles, and one armed timer between hold episodes.
    pub(super) fn dense_violations(&self, seed: u64) -> Vec<String> {
        let mut violations = Vec::new();
        if self.fill_episodes_window_max < ENGAGED_EPISODES_MIN {
            violations.push(format!(
                "TIMERS VACUOUS seed {seed:#x}: {} fill episodes inside one window, want at \
                 least {ENGAGED_EPISODES_MIN}",
                self.fill_episodes_window_max
            ));
        }
        if self.unheld_reads == 0 {
            violations.push(format!(
                "TIMERS VACUOUS seed {seed:#x}: no step read a cell between hold episodes"
            ));
        }
        if self.armed_timers_max > SERVER_TIMER_ROLES {
            violations.push(format!(
                "TIMERS VIOLATION seed {seed:#x}: armed-over-roles — {} timers armed on one cell \
                 at once, above its {SERVER_TIMER_ROLES} roles",
                self.armed_timers_max
            ));
        }
        if self.armed_unheld_max > UNHELD_ARMED_MAX {
            violations.push(format!(
                "TIMERS VIOLATION seed {seed:#x}: armed-unheld — {} timers armed on a cell with \
                 no hold episode open: an issue left its arm",
                self.armed_unheld_max
            ));
        }
        violations
    }

    /// The readings, for the run's report line.
    #[must_use]
    pub fn describe(&self) -> String {
        format!(
            "timers: armed_timers_max {} fill_episodes_window_max {} armed_unheld_max {} \
             unheld_reads {}",
            self.armed_timers_max,
            self.fill_episodes_window_max,
            self.armed_unheld_max,
            self.unheld_reads
        )
    }
}

impl TimersOracle {
    pub(super) fn new(scenario: &DurableScenario) -> TimersOracle {
        TimersOracle {
            window: scenario.fill.window,
            windows: vec![None; usize::from(scenario.cells)],
        }
    }

    /// Reads every cell after one scheduler step at injected time `now`.
    pub(super) fn sample(&mut self, node: &Node, now: Nanos, report: &mut TimersReport) {
        for (cell, window) in self.windows.iter_mut().enumerate() {
            let (cell_loop, plane) = &node.cells[cell];
            let armed = cell_loop.armed_timers();
            report.armed_timers_max = report.armed_timers_max.max(armed);
            let Some(stats) = plane.durable_stats() else { continue };
            if stats.fill_hold_open == 0 && stats.group_hold_open == 0 {
                report.armed_unheld_max = report.armed_unheld_max.max(armed);
                report.unheld_reads += 1;
            }
            let episodes = stats.frame_waits_fill;
            let (start, base) = match *window {
                Some((start, base)) if now.saturating_sub(start) < self.window => (start, base),
                Some(_) | None => (now, episodes),
            };
            *window = Some((start, base));
            report.fill_episodes_window_max =
                report.fill_episodes_window_max.max(episodes.saturating_sub(base));
        }
    }
}

/// Where TM3 engages: off on every scenario but its leg, whose value only
/// [`DurableScenario::park_at_once`] makes, and the loop reads its spin
/// through it, so TM3 armed on a loop that spins cannot be built.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WakeLeg {
    armed: bool,
}

impl WakeLeg {
    /// TM3 off, the scenario's own spin.
    pub const OFF: WakeLeg = WakeLeg { armed: false };

    /// The loop's spin: `spin_iters` off the leg, 0 on it.
    #[must_use]
    pub fn spin(self, spin_iters: u32) -> u32 {
        if self.armed { 0 } else { spin_iters }
    }
}

/// TM3's readings over one run, node sums.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WakeReport {
    /// Every park the loops asked for over the traffic phase.
    pub parks: u64,
    /// Judged parks.
    pub held_parks: u64,
    /// Judged parks whose timeout passed the bound.
    pub late_parks: u64,
    /// Parks after a staged iteration whose own submit carried a
    /// log-segment write or sync: not judged, as a completion wakes the
    /// loop.
    pub held_parks_submitting: u64,
    /// The longest judged timeout, 0 for none.
    pub timeout_ns_max: u64,
}

/// The state a cell's last iteration left, as TM3 reads it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Held {
    No,
    Staged,
}

/// TM3 over one run: armed on the leg alone.
pub(super) struct WakeOracle {
    /// The larger hold window + 1 ms; `None` off the leg.
    bound: Option<Nanos>,
    held: Vec<Held>,
}

impl WakeOracle {
    pub(super) fn new(scenario: &DurableScenario) -> WakeOracle {
        let window = scenario.fill.window.max(scenario.group.window);
        let bound = scenario.wake.armed.then(|| window + Nanos::from_millis(1));
        WakeOracle { bound, held: vec![Held::No; usize::from(scenario.cells)] }
    }

    /// Reads every cell after one scheduler step: a park judged against
    /// the state the cell's iteration before it left, then that state
    /// again from what this step left. A frozen cell reads nothing.
    pub(super) fn sample(&mut self, node: &Node, report: &mut WakeReport) {
        let Some(bound) = self.bound else { return };
        for (cell, held) in self.held.iter_mut().enumerate() {
            let (cell_loop, plane) = &node.cells[cell];
            let Some(stats) = node.iter_stats[cell] else { continue };
            let driver = cell_loop.driver();
            report.parks += u64::from(stats.parked);
            if stats.parked && *held == Held::Staged {
                if driver.submitted_named(log_segment) {
                    report.held_parks_submitting += 1;
                } else {
                    let timeout_ns = match driver.last_wait() {
                        Some(Wait::Park { timeout: Some(timeout) }) => duration_ns(timeout),
                        Some(Wait::Park { timeout: None } | Wait::Poll) | None => u64::MAX,
                    };
                    report.held_parks += 1;
                    report.late_parks += u64::from(timeout_ns > bound.0);
                    report.timeout_ns_max = report.timeout_ns_max.max(timeout_ns);
                }
            }
            *held = match plane.durable_stats() {
                None => Held::No,
                Some(_) if driver.io_in_flight_named(log_segment) => Held::No,
                Some(stats) if stats.records_staged > 0 => Held::Staged,
                Some(_) => Held::No,
            };
            // Vacuity plant: the oracle never reads a held state.
            #[cfg(inf_canary_wake_never_held)]
            {
                *held = Held::No;
            }
        }
    }

    /// The run's verdict: a late judged park, or none judged.
    pub(super) fn violations(&self, seed: u64, report: &WakeReport) -> Vec<String> {
        let Some(bound) = self.bound else { return Vec::new() };
        let mut violations = Vec::new();
        if report.late_parks > 0 {
            violations.push(format!(
                "HELD FRAME PARK OUTLIVES ITS WINDOW seed {seed:#x}: held-park-outlives-window — \
                 {} of {} judged park(s) passed {} ns ({report:?})",
                report.late_parks, report.held_parks, bound.0
            ));
        }
        if report.held_parks == 0 {
            violations.push(format!(
                "VACUOUS seed {seed:#x}: held-park-unengaged — the wake oracle judged nothing \
                 ({report:?})"
            ));
        }
        violations
    }
}

/// A timeout in ns; past `u64::MAX` it reads `u64::MAX`, which is late.
fn duration_ns(timeout: Duration) -> u64 {
    u64::try_from(timeout.as_nanos()).unwrap_or(u64::MAX)
}

impl DurableScenario {
    /// TM3's leg: the scenario as it stands, its loops at spin 0, the wake
    /// oracle armed. The CLI accepts it on `m2-fill-tick` and
    /// `m2-group-hold` only.
    #[must_use]
    pub fn park_at_once(mut self) -> DurableScenario {
        self.wake = WakeLeg { armed: true };
        self
    }

    /// `m2-fill-tick`'s dense leg: the scenario's 50 ms window and 64 KiB
    /// target, steps of at most 50 µs, and only closed-loop everysec
    /// writers, enough to fill the target in about two steps.
    #[must_use]
    pub fn m2_fill_tick_dense(seed: u64) -> DurableScenario {
        let mut scenario = DurableScenario::m2_fill_tick(seed);
        scenario.esec_writers = DENSE_WRITERS;
        scenario.esec_think_ns_max = 0;
        scenario.mem_writers = 0;
        scenario.ops_per_writer = DENSE_OPS_PER_WRITER;
        scenario.step_ns_max = 50_000;
        scenario.segment_bytes = DENSE_SEGMENT_BYTES;
        scenario.fill_dense = true;
        scenario
    }
}

/// The dense leg's everysec writers: about 60 of their records reach each
/// of the two cells a step, a writer's round trip being a few steps.
const DENSE_WRITERS: usize = 300;
/// Ops per dense writer: a closed-loop writer sends one per step at most.
const DENSE_OPS_PER_WRITER: u64 = 400;
/// The dense leg's segments, and so its staging ring: a segment holds
/// sixty-odd full frames, so neither a rotation nor a full ring stops the
/// writers inside a window.
const DENSE_SEGMENT_BYTES: u32 = 4 << 20;
/// The dense leg's everysec `SET` value, bytes: about 60 such records a
/// step stage about 42 KiB, so a frame held at one LOG step reaches the
/// 64 KiB target at the next; one command stays inside one 1 KiB receive
/// buffer, which the leg delivers whole.
pub(super) const DENSE_VALUE_BYTES: usize = 700;

#[cfg(test)]
mod tests {
    use super::super::run_durable_scenario;
    use super::*;

    const SEED: u64 = 0xC0FFEE;

    /// The dense leg's population: episodes of a hold and a seal a LOG step
    /// or two apart. A timer that outlived its episode would grow the timers
    /// armed at once with the episodes inside one window; one owner per
    /// role, its arm cancelled at the episode's issue, holds the peak at
    /// the role count.
    #[test]
    fn dense_fill_episodes_keep_a_cells_timers_within_its_roles() {
        let scenario = DurableScenario::m2_fill_tick_dense(SEED);
        let report = run_durable_scenario(&scenario);
        let timers = report.timers;
        println!("dense leg {timers:?} waits_fill {}", report.frame_waits_fill);
        assert!(
            timers.fill_episodes_window_max >= ENGAGED_EPISODES_MIN,
            "VACUOUS: {} fill episodes inside one window, want at least {ENGAGED_EPISODES_MIN}",
            timers.fill_episodes_window_max
        );
        assert!(report.ok(), "the leg's own oracles: {:?}", report.violations);
        assert!(
            timers.armed_timers_max <= SERVER_TIMER_ROLES,
            "{} timers armed on one cell at once, above its {SERVER_TIMER_ROLES} roles",
            timers.armed_timers_max
        );
        assert!(timers.unheld_reads > 0, "VACUOUS: no step read a cell between hold episodes");
        assert!(
            timers.armed_unheld_max <= UNHELD_ARMED_MAX,
            "{} timers armed on a cell with no hold episode open: an issue left its arm",
            timers.armed_unheld_max
        );
    }

    /// The control: `m2-fill-tick` as it stands, one writer thinking up to
    /// 3 s, holds the everysec timer and the few episodes one sparse writer
    /// opens in a window, so the dense leg's red is its density and not the
    /// instrument.
    #[test]
    fn a_sparse_fill_writer_keeps_a_cells_timers_within_its_roles() {
        let report = run_durable_scenario(&DurableScenario::m2_fill_tick(SEED));
        let timers = report.timers;
        println!("sparse control {timers:?} waits_fill {}", report.frame_waits_fill);
        assert!(report.frame_waits_fill > 0, "VACUOUS: the sparse writer opened no fill episode");
        assert!(report.ok(), "the scenario's own oracles: {:?}", report.violations);
        assert!(
            timers.armed_timers_max <= SERVER_TIMER_ROLES,
            "{} timers armed on one cell at once, above its {SERVER_TIMER_ROLES} roles",
            timers.armed_timers_max
        );
        assert!(timers.unheld_reads > 0, "VACUOUS: no step read a cell between hold episodes");
        assert!(
            timers.armed_unheld_max <= UNHELD_ARMED_MAX,
            "{} timers armed on a cell with no hold episode open: an issue left its arm",
            timers.armed_unheld_max
        );
    }
}
