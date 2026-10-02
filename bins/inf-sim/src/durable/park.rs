//! The loop-tier park instrument: what a cell's reactor decides in the
//! iterations after the cell publishes a checkpoint, read on the shipped
//! plane over the sim driver.
//!
//! - **Estimator and scope:** `IterStats::parked` of one cell's reactor
//!   iteration, kept by [`Node::iter_stats`]. It is the reactor's own
//!   decision (spin exhausted, no ready task, no `before_park` veto), made
//!   before the driver call. The sim driver ignores the wait, so a park is
//!   this flag and never elapsed virtual time.
//! - **Resolution:** one reactor iteration. The harness runs each cell
//!   once per scheduler step, and the publishing iteration is the step in
//!   which the cell's board slot moved.
//! - **Spread, and the control leg:** none. Time is virtual and the
//!   scheduler seeded, so two runs of one seed read the same flags (the
//!   control test asserts it). The control is the same publication with no
//!   waiter, on the same binary: its next iteration parks.
//! - **Liveness, and its planted red:** `parks_read`, the parked iterations
//!   a run read. The planted red is the harness's own `spin_iters`: while
//!   the loop still spins, the same oracle reads no park.

use std::path::PathBuf;
use std::rc::Rc;

use inf_foundation::rng::SplitMix64;
use inf_foundation::time::{Nanos, VirtualClock};
use inf_server::{SimDisk, StallConfig};

use super::{DurableScenario, MiniClient, Node, TraceObserver, boot, build_disk};

const SEED: u64 = 0xC0FFEE;

/// Scheduler steps a wait may take before the test calls it stalled.
const STEPS_MAX: u32 = 20_000;

/// A quiet durable node: recovered, no writers, one client per test.
struct Quiet {
    node: Node,
    rng: SplitMix64,
    clock: Rc<VirtualClock>,
    disk: SimDisk,
    step_ns_max: u64,
    /// Iterations of cell 0 the instrument read, and how many parked.
    iterations_read: u64,
    parks_read: u64,
}

impl Quiet {
    fn boot(cells: u16, spin_iters: u32) -> Quiet {
        let clock = Rc::new(VirtualClock::new(Nanos(1)));
        let disk = build_disk(SEED, Some(&StallConfig::write_reorder()));
        let scenario = DurableScenario { cells, spin_iters, ..DurableScenario::m2_durable(SEED) };
        let observer = TraceObserver::default();
        let node = boot(&scenario, PathBuf::from("node"), &disk, &clock, &observer).expect("boot");
        let mut quiet = Quiet {
            node,
            rng: SplitMix64::new(SEED),
            clock,
            disk,
            step_ns_max: scenario.step_ns_max,
            iterations_read: 0,
            parks_read: 0,
        };
        for _ in 0..STEPS_MAX {
            if quiet.node.ready() {
                return quiet;
            }
            quiet.step();
        }
        panic!("the node did not recover in {STEPS_MAX} steps");
    }

    /// One scheduler step; reads cell 0's park decision for it.
    fn step(&mut self) -> bool {
        self.node.step(&mut self.rng, &self.clock, &self.disk, self.step_ns_max).expect("step");
        let parked = self.node.iter_stats(0).parked;
        self.iterations_read += 1;
        self.parks_read += u64::from(parked);
        parked
    }

    fn published(&self, cell: u16) -> u64 {
        self.node.control.ckpt_board().slot(cell).published()
    }

    /// Steps until cell 0's slot moves: the step that returns is cell 0's
    /// publishing iteration. Returns the waiters registered on cell 0 when
    /// that iteration began.
    fn step_to_own_publication(&mut self) -> usize {
        let before = self.published(0);
        for _ in 0..STEPS_MAX {
            let waiters = self.node.plane(0).ckpt_waiters_for_sim();
            self.step();
            if self.published(0) != before {
                return waiters;
            }
        }
        panic!("cell 0 published no checkpoint in {STEPS_MAX} steps");
    }
}

/// What the instrument read around one own publication of cell 0.
#[derive(Debug, PartialEq, Eq)]
struct ParkRead {
    /// Waiters on cell 0 when its publishing iteration began.
    waiters_at_publication: usize,
    /// `parked` of the iteration after the publishing one.
    parked_after: bool,
    parks_read: u64,
    iterations_read: u64,
}

/// The control leg's run: a 1-cell node, `INF.CKPT` with no `WAIT`, read
/// through the iteration after the publishing one.
fn publication_with_no_waiter(spin_iters: u32) -> ParkRead {
    let mut quiet = Quiet::boot(1, spin_iters);
    let mut client = MiniClient::connect(&mut quiet.node, 0);
    let Quiet { node, rng, clock, disk, step_ns_max, .. } = &mut quiet;
    let reply = client.call(node, rng, clock, disk, *step_ns_max, &[b"INF.CKPT"]).expect("call");
    assert_eq!(reply.as_deref(), Some(b"+OK\r\n".as_slice()), "INF.CKPT");
    let waiters_at_publication = quiet.step_to_own_publication();
    let parked_after = quiet.step();
    ParkRead {
        waiters_at_publication,
        parked_after,
        parks_read: quiet.parks_read,
        iterations_read: quiet.iterations_read,
    }
}

/// The control leg: with `spin_iters: 0` and no waiter, the iteration
/// after the publishing one parks, so the instrument can read a park. Two
/// runs of the seed read the same flags (no spread).
#[test]
fn a_publication_with_no_waiter_parks_the_next_iteration() {
    let read = publication_with_no_waiter(0);
    assert_eq!(read.waiters_at_publication, 0, "the control leg has no waiter");
    assert!(read.parked_after, "the iteration after the publication did not park: {read:?}");
    assert!(read.parks_read > 0, "liveness: the instrument read no park in {read:?}");
    assert_eq!(read, publication_with_no_waiter(0), "one seed, one sequence of flags");
}

/// The instrument's planted red: at the harness's own `spin_iters` the
/// loop is still spinning after the publication, and the control leg's
/// oracle reads no park. A harness that dropped the parameter, or one that
/// answered a constant flag, fails here or in the control leg.
#[test]
fn canary_a_spinning_loop_reads_no_park_after_the_publication() {
    let read = publication_with_no_waiter(DurableScenario::SPIN_ITERS);
    assert_eq!(read.waiters_at_publication, 0);
    assert!(!read.parked_after, "the planted red must read no park: {read:?}");
    assert!(read.iterations_read > 0);
}
