//! `m2-stop-tick`: a graceful stop over a barrier that already covers the log.
//!
//! The shape is `m2-clean-stop`'s with no `always` writer, six `everysec`
//! writers on two cells, `Buffered` segments (a held zero-fill barrier on a
//! `Direct` segment holds the FLUSH slot, so no standalone would issue), K = 1
//! or 3 by seed, and the stop checkpoint off on every cell: with it on, the
//! checkpoint's begin marker is a frame queued during the drain, which carries
//! a due away with it.
//!
//! The stop is requested at the first step at or past a step drawn in
//! `[200, 200 + ops_per_writer)` at which a writer is below its quota and a
//! cell's log holds bytes not yet durable, or every writer is done. At the
//! first step the harness says `finish_stop` (every cell `Quiet`) it holds
//! every fdatasync on a log segment (the driver holds only standalone
//! fdatasyncs: a frame's linked sync passes) and records which cells' logs
//! are dirty then. A cell clean at the arming drains through an idle final
//! sync and is not read for the reach. The hold lifts at the first step at
//! which every cell is `Quiet` or `Drained` and every cell dirty at the
//! arming has counted a covered tick since it, or 5 s of injected time after
//! the arming. A tick over a held standalone that covers every queued byte
//! owes nothing (ADR-0199 D2.2); a ledger that keeps a due for it never
//! drains, and the clean-stop liveness oracle reads the stop.
//!
//! The clauses, each by its own token:
//! - `stop-tick-unheld`: the driver held a log fdatasync while armed, the
//!   harness's own fact;
//! - `stop-tick-uncovered`: at least one cell's log was dirty at the arming,
//!   and each such cell's `everysec_covered_ticks` rose since. The counter
//!   counts by the due's own comparison, so it says the shape was reached,
//!   never that it was handled;
//! - the clean-stop liveness oracle, and the survival audit with every
//!   acknowledged op required (a clean stop loses none), unchanged. A boot
//!   after this stop may replay its tail (the stop checkpoint is off), so the
//!   clean-stop replay clause is not this shape's.

use super::*;

/// The latest the hold lifts after the arming, injected time: well past
/// the wheel's one-second tick, so every dirty cell meets a tick inside it.
const LIFT_AFTER: Nanos = Nanos::from_secs(5);

/// How long the hold stays engaged (the driver holding a log fdatasync)
/// before it lifts on that alone: no bound but `LIFT_AFTER`'s.
#[cfg(not(inf_canary_stop_tick_short_hold))]
const ENGAGED_FOR: Option<Nanos> = None;
/// Plant: the hold lifts 1 ms after it engages, before the wheel's tick
/// can meet a held barrier, so no dirty cell counts a covered tick.
#[cfg(inf_canary_stop_tick_short_hold)]
const ENGAGED_FOR: Option<Nanos> = Some(Nanos::from_millis(1));

/// The hold's predicate: every fdatasync on a log segment (the predicate
/// the ledger-wedge shape holds its seal with).
#[cfg(not(inf_canary_stop_tick_unheld))]
const HELD: Option<fn(&Path) -> bool> = Some(super::ledger_wedge::log_segment);
/// Plant: the arm never arms the hold, so the driver holds nothing.
#[cfg(inf_canary_stop_tick_unheld)]
const HELD: Option<fn(&Path) -> bool> = None;

impl DurableScenario {
    /// `m2-stop-tick` (the module docs).
    #[must_use]
    pub fn m2_stop_tick(seed: u64) -> DurableScenario {
        let mut scenario = DurableScenario::m2_clean_stop(seed);
        scenario.always_writers = 0;
        scenario.esec_writers = 6;
        scenario.io_mode = SegmentIoMode::Buffered;
        scenario.frames_in_flight = if seed % 2 == 1 { 3 } else { 1 };
        scenario.stop_tick = true;
        scenario
    }
}

/// The arming's reading.
struct Armed {
    step: u64,
    at: Nanos,
    /// Each cell's covered ticks at the arming, `None` for a cell whose log
    /// was clean then.
    covered_at: Vec<Option<u64>>,
}

/// The arm's state through one stop, and what it read.
pub(crate) struct StopTick {
    /// The first step at which the stop may be requested.
    request_from: u64,
    armed: Option<Armed>,
    held_max: usize,
    /// When the driver first held a log fdatasync after the arming.
    engaged_at: Option<Nanos>,
    /// Injected time from the arming to the lift.
    lifted_after: Option<Nanos>,
}

/// What the arm read, for the run's report line.
#[derive(Clone, Debug)]
pub struct StopTickReport {
    pub request_from: u64,
    pub armed_at: Option<u64>,
    /// The cells whose log was dirty at the arming.
    pub dirty_at_arming: Vec<usize>,
    /// Each cell's covered ticks since the arming (0 for a clean cell).
    pub covered_since: Vec<u64>,
    pub held_max: usize,
    pub lifted_after_ms: Option<u64>,
}

impl StopTick {
    /// The arm, its request step drawn from `rng` in
    /// `[200, 200 + ops_per_writer)`.
    pub(crate) fn new(scenario: &DurableScenario, rng: &mut SplitMix64) -> StopTick {
        let request_from = 200 + rng.next_below(scenario.ops_per_writer.max(1));
        StopTick { request_from, armed: None, held_max: 0, engaged_at: None, lifted_after: None }
    }

    /// Whether the stop is requested at `step`, given whether a writer is
    /// still below its quota.
    pub(crate) fn stop_due(&self, step: u64, below_quota: bool, node: &Node, cells: usize) -> bool {
        if step < self.request_from {
            return false;
        }
        if !below_quota {
            return true;
        }
        (0..cells).any(|cell| {
            node.plane(cell).durable_stats().is_some_and(|stats| stats.pending_log_bytes > 0)
        })
    }

    /// The first `finish_stop`: hold every log fdatasync and read which
    /// cells are dirty.
    pub(crate) fn arm(&mut self, step: u64, node: &Node, disk: &SimDisk, now: Nanos, cells: usize) {
        if self.armed.is_some() {
            return;
        }
        let covered_at = (0..cells)
            .map(|cell| {
                node.plane(cell)
                    .durable_stats()
                    .filter(|stats| stats.pending_log_bytes > 0)
                    .map(|stats| stats.everysec_covered_ticks)
            })
            .collect();
        disk.hold_syncs(HELD);
        self.armed = Some(Armed { step, at: now, covered_at });
    }

    /// One stop step's reading, after the step: the hold's engagement and
    /// its lift.
    pub(crate) fn observe(&mut self, node: &Node, disk: &SimDisk, now: Nanos, cells: usize) {
        let Some(armed) = &self.armed else { return };
        if self.lifted_after.is_some() {
            return;
        }
        let held: usize = (0..cells).map(|cell| node.cells[cell].0.driver().held_syncs()).sum();
        self.held_max = self.held_max.max(held);
        if held > 0 && self.engaged_at.is_none() {
            self.engaged_at = Some(now);
        }
        let engaged_out =
            ENGAGED_FOR.zip(self.engaged_at).is_some_and(|(bound, at)| now >= at + bound);
        let quiet = (0..cells).all(|cell| {
            matches!(
                node.plane(cell).stop_phase(),
                inf_server::StopPhase::Quiet | inf_server::StopPhase::Drained
            )
        });
        let covered = self.covered_since(node, cells);
        let reached =
            armed.covered_at.iter().zip(&covered).all(|(at, since)| at.is_none() || *since > 0);
        if (quiet && reached) || engaged_out || now >= armed.at + LIFT_AFTER {
            disk.hold_syncs(None);
            self.lifted_after = Some(now.saturating_sub(armed.at));
        }
    }

    /// Each cell's covered ticks since the arming (0 for a cell clean then,
    /// or before the arming).
    fn covered_since(&self, node: &Node, cells: usize) -> Vec<u64> {
        let Some(armed) = &self.armed else { return vec![0; cells] };
        (0..cells)
            .map(|cell| {
                let now = node.plane(cell).durable_stats().map_or(0, |s| s.everysec_covered_ticks);
                armed.covered_at[cell].map_or(0, |at| now.saturating_sub(at))
            })
            .collect()
    }

    /// The arm's readings, for the report and for a liveness verdict.
    pub(crate) fn report(&self, node: &Node, cells: usize) -> StopTickReport {
        StopTickReport {
            request_from: self.request_from,
            armed_at: self.armed.as_ref().map(|armed| armed.step),
            dirty_at_arming: self.armed.as_ref().map_or_else(Vec::new, |armed| {
                (0..cells).filter(|&cell| armed.covered_at[cell].is_some()).collect()
            }),
            covered_since: self.covered_since(node, cells),
            held_max: self.held_max,
            lifted_after_ms: self.lifted_after.map(|after| after.as_millis()),
        }
    }

    /// The vacuity clauses, in order, once every cell drained.
    pub(crate) fn judge(&self, seed: u64, node: &Node, cells: usize, report: &mut DurableReport) {
        let read = self.report(node, cells);
        report.stop_tick = Some(read.clone());
        let vacuous = if read.armed_at.is_none() {
            Some("stop-tick-unheld — the stop never reached finish_stop".to_owned())
        } else if read.held_max == 0 {
            Some("stop-tick-unheld — the driver held no log fdatasync while armed".to_owned())
        } else if read.dirty_at_arming.is_empty() {
            Some("stop-tick-uncovered — no cell's log was dirty at the arming".to_owned())
        } else {
            read.dirty_at_arming.iter().find(|&&cell| read.covered_since[cell] == 0).map(|cell| {
                format!(
                    "stop-tick-uncovered — cell {cell} counted no covered tick since the arming"
                )
            })
        };
        if let Some(vacuous) = vacuous {
            report.violations.push(format!("VACUOUS seed {seed:#x}: {vacuous}"));
        }
    }
}

impl StopTickReport {
    /// One line for a verdict or the run's report.
    #[must_use]
    pub fn describe(&self) -> String {
        format!(
            "stop tick: requested from step {}, armed at step {:?}, cells dirty at the arming \
             {:?}, covered ticks since {:?}, driver held at most {} fdatasync(s), lifted after \
             {:?} ms",
            self.request_from,
            self.armed_at,
            self.dirty_at_arming,
            self.covered_since,
            self.held_max,
            self.lifted_after_ms
        )
    }
}
