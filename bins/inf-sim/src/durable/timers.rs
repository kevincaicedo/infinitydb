//! The timers' DST rows: how many timers a cell holds armed while its
//! fill policy holds frame after frame.
//!
//! - **The armed peak.** After every scheduler step, each cell's
//!   `CellLoop::armed_timers()`; the run keeps the largest, cell scope. A
//!   cell's timers are bounded by its owner positions only if every hold
//!   episode leaves at most one timer behind: one per role.
//! - **Engagement.** The fill episodes that begin inside one hold window:
//!   each cell's `frame_waits_fill` (the product's episode count) read at
//!   every step and differenced over tumbling windows of the scenario's
//!   fill window; the run keeps the largest rise. A dense row that rose by
//!   fewer than 8 inside one window opened too few episodes to say
//!   anything about their timers.
//! - **The dense leg.** `m2-fill-tick` at steps of at most 50 µs, with 300
//!   closed-loop everysec writers whose 700-byte values reach their cell
//!   whole at each reap, on 4 MiB segments: they fill the 64 KiB target in
//!   about two steps, so an episode is a hold and a seal a LOG step or two
//!   apart and a 50 ms window holds hundreds of them. Its control is
//!   `m2-fill-tick` as it stands, one writer thinking up to 3 s, whose peak
//!   is the everysec timer and the few episodes one sparse writer opens.

use inf_foundation::time::Nanos;

use super::{DurableScenario, Node};

/// What the timers' rows read over a run's traffic phase.
#[derive(Clone, Copy, Debug, Default)]
pub struct TimersReport {
    /// The most timers any cell held armed after any scheduler step.
    pub armed_timers_max: u32,
    /// The most fill episodes one cell began inside one fill window.
    pub fill_episodes_window_max: u64,
}

/// The rows' per-cell state over one life of a node.
pub(super) struct TimersOracle {
    /// The scenario's fill window: the span the episodes are counted over.
    window: Nanos,
    /// Per cell: the window's start and the cell's episode count then.
    windows: Vec<Option<(Nanos, u64)>>,
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
            report.armed_timers_max = report.armed_timers_max.max(cell_loop.armed_timers());
            let Some(episodes) = plane.durable_stats().map(|stats| stats.frame_waits_fill) else {
                continue;
            };
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

impl DurableScenario {
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

    /// Episodes the dense leg must begin inside one window, else the row
    /// is vacuous.
    const ENGAGED_EPISODES_MIN: u64 = 8;
    /// The positions a server cell's timers fill: accept retry, everysec,
    /// fill hold, group hold.
    const SERVER_TIMER_ROLES: u32 = 4;

    /// The dense leg's population: episodes of a hold and a seal a LOG step
    /// or two apart. On the wheel each arms a fill timer that nothing
    /// cancels, so the timers armed at once grow with the episodes inside
    /// one window; one owner per role holds the peak at the role count.
    #[test]
    #[ignore = "red on the wheel: each fill hold episode arms a timer that lives its whole window"]
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
    }
}
