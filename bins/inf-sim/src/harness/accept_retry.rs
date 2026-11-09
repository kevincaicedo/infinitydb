//! The accept-retry oracle, on `m0-smoke --plant accept-broken`. The plant
//! parks each cell's accept arm as a broken listener, which by the driver
//! contract only an `AcceptArm` resumes, never a `Close`: the plane's
//! accept-retry timer is the queued clients' one way in. Each park is judged
//! at the first `AcceptArm` the cell's driver receives after it. That op
//! comes no earlier than the retry's deadline and less than two scheduler
//! steps past it: the step whose turn the timer is delivered in, and the
//! next, whose driver entry submits the op. The deadline is derived here
//! from the park's time and the window by the timer set's rounding rule
//! (ADR-0154 D3): the requested instant in whole milliseconds rounded up,
//! and no earlier than the millisecond after the park's. The oracle reads
//! the driver's park and its `AcceptArm` count, the sim clock, the
//! scenario's step bound and the window in `inf_server::limits`; it never
//! reads the timer set, the plane's owner, the deadline the plane armed or
//! its error counter.

use crate::driver::Violations;
use core::cell::RefCell;
use core::fmt;
use std::rc::Rc;

use inf_foundation::time::Nanos;

use crate::net::CellNet;

/// What the oracle read over one run (node sums; times in ns).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RetryReport {
    /// Broken-class parks seen, one at most a cell (the plant fires once).
    pub parks: u64,
    /// Parks resumed by an `AcceptArm` inside their bound.
    pub retried: u64,
    /// Parks whose first `AcceptArm` came before their deadline.
    pub early: u64,
    /// Parks whose first `AcceptArm` came past their bound.
    pub late: u64,
    /// Parks that ended with no `AcceptArm`: the leg's premise broke.
    pub by_close: u64,
    /// Parks still open when the run ended.
    pub open: u64,
    /// From each judged park to its first `AcceptArm`; `None` until a park
    /// is judged.
    pub latency_ns_min: Option<u64>,
    pub latency_ns_max: Option<u64>,
    /// From each judged park's deadline to its first `AcceptArm`, negative
    /// when early; `None` until a park is judged.
    pub past_deadline_ns_min: Option<i64>,
    pub past_deadline_ns_max: Option<i64>,
    /// The window, and how far past its deadline a park's first `AcceptArm`
    /// may come: two scheduler steps' exclusive bound.
    pub window_ns: u64,
    pub delivery_ns_max: u64,
}

/// One cell's open park: when it was seen, its retry's deadline, and the
/// driver's `AcceptArm` count then.
#[derive(Copy, Clone, Debug)]
struct OpenPark {
    since: Nanos,
    deadline: Nanos,
    arms: u64,
}

/// The oracle's state for one run: one slot a cell.
pub(super) struct RetryWatch {
    window: Nanos,
    delivery_max: Nanos,
    open: Vec<Option<OpenPark>>,
    report: RetryReport,
}

impl RetryWatch {
    /// `step_ns_bound` is the scenario's exclusive bound of one scheduler
    /// step's advance ([`super::Scenario::step_ns_bound`]).
    pub(super) fn new(cells: usize, step_ns_bound: u64) -> RetryWatch {
        let window = inf_server::limits::ACCEPT_RETRY;
        let step = Nanos(step_ns_bound);
        let delivery_max = step.saturating_add(step);
        RetryWatch {
            window,
            delivery_max,
            open: vec![None; cells],
            report: RetryReport {
                window_ns: window.0,
                delivery_ns_max: delivery_max.0,
                ..RetryReport::default()
            },
        }
    }

    /// Read after every scheduler step's iterations, at the step's time.
    pub(super) fn sample(&mut self, now: Nanos, nets: &[Rc<RefCell<CellNet>>]) {
        for (slot, net) in self.open.iter_mut().zip(nets) {
            let net = net.borrow();
            let parked = parked_broken(&net);
            let arms = net.accept_arms();
            match *slot {
                None if parked => {
                    let deadline = retry_deadline(now, self.window);
                    *slot = Some(OpenPark { since: now, deadline, arms });
                    self.report.parks += 1;
                }
                None => {}
                Some(park) if arms > park.arms => {
                    *slot = None;
                    self.report.judge(park, now, self.delivery_max);
                }
                Some(_) if !parked => {
                    *slot = None;
                    self.report.by_close += 1;
                }
                Some(_) => {}
            }
        }
    }

    /// The run's verdict, after its last step, pushed onto the run's list.
    pub(super) fn finish(mut self, violations: &mut Violations) -> RetryReport {
        self.report.open = self.open.iter().map(|slot| u64::from(slot.is_some())).sum();
        self.report.verdict(violations);
        self.report
    }
}

impl RetryReport {
    /// One park's first `AcceptArm`, at `arm`: early before the park's
    /// deadline, late past it by more than `delivery_max`, else retried.
    fn judge(&mut self, park: OpenPark, arm: Nanos, delivery_max: Nanos) {
        let latency = arm.saturating_sub(park.since).0;
        self.latency_ns_min = least(self.latency_ns_min, latency);
        self.latency_ns_max = greatest(self.latency_ns_max, latency);
        let past = past_deadline_ns(arm, park.deadline);
        self.past_deadline_ns_min = least(self.past_deadline_ns_min, past);
        self.past_deadline_ns_max = greatest(self.past_deadline_ns_max, past);
        if arm < park.deadline {
            self.early += 1;
        } else if arm > park.deadline.saturating_add(delivery_max) {
            self.late += 1;
        } else {
            self.retried += 1;
        }
    }

    /// The violation lines of a finished run, each with its own token.
    fn verdict(&self, lines: &mut Violations) {
        if self.parks == 0 {
            lines.push(
                "VACUOUS accept-retry-unengaged: the leg's plant parked no accept arm".to_owned(),
            );
        }
        if self.open > 0 {
            lines.push(format!(
                "accept-retry-missing: {} of {} broken-parked accept arms saw no AcceptArm by \
                 the run's end (the plane's retry timer re-arms them)",
                self.open, self.parks
            ));
        }
        if self.by_close > 0 {
            lines.push(format!(
                "accept-retry-by-close: {} broken-parked accept arms resumed with no AcceptArm",
                self.by_close
            ));
        }
        if self.early > 0 {
            lines.push(format!(
                "accept-retry-early: {} accept arms re-armed before their retry's deadline \
                 (the earliest {} ns before it)",
                self.early,
                shown(self.past_deadline_ns_min.map(i64::unsigned_abs))
            ));
        }
        if self.late > 0 {
            lines.push(format!(
                "accept-retry-late: {} accept arms re-armed more than {} ns past their \
                 retry's deadline (the latest {} ns past it)",
                self.late,
                self.delivery_ns_max,
                shown(self.past_deadline_ns_max)
            ));
        }
    }
}

impl fmt::Display for RetryReport {
    /// The leg's report line: the counts, the extremes (`none` when no park
    /// was judged) and the bound.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "parks={} retried={} early={} late={} by_close={} open={} latency_ns=[{}, {}] \
             past_deadline_ns=[{}, {}] window_ns={} delivery_ns_max={}",
            self.parks,
            self.retried,
            self.early,
            self.late,
            self.by_close,
            self.open,
            shown(self.latency_ns_min),
            shown(self.latency_ns_max),
            shown(self.past_deadline_ns_min),
            shown(self.past_deadline_ns_max),
            self.window_ns,
            self.delivery_ns_max
        )
    }
}

/// A running minimum taking `value`.
fn least<T: Ord + Copy>(running: Option<T>, value: T) -> Option<T> {
    Some(running.map_or(value, |seen| seen.min(value)))
}

/// A running maximum taking `value`.
fn greatest<T: Ord + Copy>(running: Option<T>, value: T) -> Option<T> {
    Some(running.map_or(value, |seen| seen.max(value)))
}

/// A reading as printed: its value, or `none` when it is absent.
fn shown<T: fmt::Display>(reading: Option<T>) -> String {
    reading.map_or_else(|| "none".to_owned(), |value| value.to_string())
}

/// The retry's deadline for a park at `park`, by the timer set's rounding
/// rule (ADR-0154 D3), written here from the rule: `park + window` in whole
/// milliseconds rounded up, raised to the millisecond after `park`'s.
fn retry_deadline(park: Nanos, window: Nanos) -> Nanos {
    let millisecond = Nanos::from_millis(1).0;
    let requested_ms = park.saturating_add(window).0.div_ceil(millisecond);
    let earliest_ms = park.as_millis().saturating_add(1);
    Nanos(requested_ms.max(earliest_ms).saturating_mul(millisecond))
}

/// `arm − deadline` in signed ns, saturated at `i64`'s range (a report
/// figure; the verdict compares the instants themselves).
fn past_deadline_ns(arm: Nanos, deadline: Nanos) -> i64 {
    if arm >= deadline {
        i64::try_from(arm.0 - deadline.0).unwrap_or(i64::MAX)
    } else {
        i64::try_from(deadline.0 - arm.0).map_or(i64::MIN, |early| -early)
    }
}

/// The driver's park, as the oracle reads it.
#[cfg(not(inf_canary_accept_retry_park_unread))]
fn parked_broken(net: &CellNet) -> bool {
    net.accept_parked() == Some(crate::net::AcceptPark::Broken)
}

/// Canary: the park is never read, so the leg judges nothing.
#[cfg(inf_canary_accept_retry_park_unread)]
fn parked_broken(_net: &CellNet) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1_000_000;

    #[test]
    fn the_deadline_rounds_up_to_a_millisecond_after_the_parks() {
        let window = Nanos::from_millis(100);
        // The leg's parks, early in the run's first millisecond.
        assert_eq!(retry_deadline(Nanos(1_195), window), Nanos(101 * MS));
        assert_eq!(retry_deadline(Nanos(MS - 1), window), Nanos(101 * MS));
        // A requested instant on a millisecond is that millisecond.
        assert_eq!(retry_deadline(Nanos(MS), window), Nanos(101 * MS));
        assert_eq!(retry_deadline(Nanos(MS + 1), window), Nanos(102 * MS));
        // A window below the rounding: the millisecond after the park's.
        assert_eq!(retry_deadline(Nanos(MS), Nanos::ZERO), Nanos(2 * MS));
        assert_eq!(retry_deadline(Nanos(MS + MS / 2), Nanos::ZERO), Nanos(2 * MS));
        assert_eq!(retry_deadline(Nanos(MS + MS / 2), Nanos(MS)), Nanos(3 * MS));
    }

    #[test]
    fn a_first_arm_is_judged_against_its_parks_deadline_and_bound() {
        let park = OpenPark { since: Nanos(1_000), deadline: Nanos(101 * MS), arms: 1 };
        let delivery_max = Nanos(32_000);
        let mut report = RetryReport::default();
        report.judge(park, Nanos(101 * MS - 1), delivery_max);
        assert_eq!(report.early, 1);
        report.judge(park, Nanos(101 * MS), delivery_max);
        report.judge(park, Nanos(101 * MS + 32_000), delivery_max);
        assert_eq!(report.retried, 2);
        report.judge(park, Nanos(101 * MS + 32_001), delivery_max);
        assert_eq!(report.late, 1);
        assert_eq!(report.past_deadline_ns_min, Some(-1));
        assert_eq!(report.past_deadline_ns_max, Some(32_001));
        assert_eq!(report.latency_ns_min, Some(101 * MS - 1 - 1_000));
    }

    /// The verdict's lines, as the run's list receives them.
    fn verdict_of(report: &RetryReport) -> Violations {
        let mut lines = Violations::new();
        report.verdict(&mut lines);
        lines
    }

    #[test]
    fn each_clause_of_the_verdict_carries_its_own_token() {
        let unengaged = RetryReport::default();
        assert_eq!(verdict_of(&unengaged).len(), 1);
        assert!(verdict_of(&unengaged)[0].starts_with("VACUOUS accept-retry-unengaged"));
        let green = RetryReport { parks: 3, retried: 3, ..RetryReport::default() };
        assert!(verdict_of(&green).is_empty());
        for (report, token) in [
            (RetryReport { parks: 1, open: 1, ..RetryReport::default() }, "accept-retry-missing"),
            (
                RetryReport { parks: 1, by_close: 1, ..RetryReport::default() },
                "accept-retry-by-close",
            ),
            (RetryReport { parks: 1, early: 1, ..RetryReport::default() }, "accept-retry-early"),
            (RetryReport { parks: 1, late: 1, ..RetryReport::default() }, "accept-retry-late"),
        ] {
            let lines = verdict_of(&report);
            assert_eq!(lines.len(), 1, "{token}: {lines:?}");
            assert!(lines[0].starts_with(token), "{token}: {lines:?}");
        }
    }
}
