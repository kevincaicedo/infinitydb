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
pub(super) const STEPS_MAX: u32 = 20_000;

/// A quiet durable node: recovered, no writers, one client per test.
pub(super) struct Quiet {
    pub(super) node: Node,
    rng: SplitMix64,
    pub(super) clock: Rc<VirtualClock>,
    disk: SimDisk,
    step_ns_max: u64,
    /// Iterations of cell 0 the instrument read, and how many parked.
    iterations_read: u64,
    parks_read: u64,
}

impl Quiet {
    pub(super) fn boot(cells: u16, spin_iters: u32) -> Quiet {
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
    pub(super) fn step(&mut self) -> bool {
        self.node.step(&mut self.rng, &self.clock, &self.disk, self.step_ns_max).expect("step");
        let parked = self.node.iter_stats(0).parked;
        self.iterations_read += 1;
        self.parks_read += u64::from(parked);
        parked
    }

    pub(super) fn published(&self, cell: u16) -> u64 {
        self.node.control.ckpt_board().slot(cell).published()
    }

    /// Steps until cell 0's slot moves: the step that returns is cell 0's
    /// publishing iteration. Returns the waiters registered on cell 0 when
    /// that iteration began.
    pub(super) fn step_to_own_publication(&mut self) -> usize {
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

/// ADR-0159 A1.4's own-slot term at the loop tier: with `spin_iters: 0` and
/// an `INF.CKPT WAIT` parked on the cell that publishes last, no iteration
/// parks from the one after the publishing iteration through the one whose
/// MAINTAIN wakes the waitlist. Without the term the cell parks on top of a
/// satisfied `WAIT` and answers it one park timeout late.
#[test]
fn an_own_publication_with_a_waiter_does_not_park_before_its_wake() {
    let mut quiet = Quiet::boot(1, 0);
    let mut client = MiniClient::connect(&mut quiet.node, 0);
    client.send(&mut quiet.node, &[b"INF.CKPT", b"WAIT"]);
    let waiters = quiet.step_to_own_publication();
    assert_eq!(waiters, 1, "VACUOUS: no WAIT was registered when cell 0 published");
    let mut window = Vec::new();
    while quiet.node.plane(0).ckpt_waiters_for_sim() > 0 {
        assert!(window.len() < 16, "the waitlist was never woken: {window:?}");
        window.push(quiet.step());
    }
    assert!(
        !window.contains(&true),
        "a parked iteration between the publication and its wake (parked flags, from the \
         iteration after the publishing one through the waking one): {window:?}"
    );
    let reply = (0..16).find_map(|_| {
        quiet.step();
        client.recv(&mut quiet.node)
    });
    assert_eq!(reply.as_deref(), Some(b"+OK\r\n".as_slice()), "the WAIT answers");
}

/// The guard ends (ADR-0159 A1.4): an all-cell `WAIT` on a 2-cell node
/// whose peer never publishes is not satisfied by this cell's own
/// publication. Once a sweep that began after the publication completed,
/// the cell parks with its waiter still registered: the own-slot term holds
/// for at most `2 * ceil(N / 64)` turns, and the wake's re-check takes one.
#[test]
fn a_waiter_the_own_publication_does_not_satisfy_lets_the_cell_park() {
    const CELLS: u16 = 2;
    let mut quiet = Quiet::boot(CELLS, 0);
    let mut client = MiniClient::connect(&mut quiet.node, 0);
    // The peer is never stepped again, so its slot stays at 0.
    quiet.node.frozen = Some((1, u64::MAX));
    client.send(&mut quiet.node, &[b"INF.CKPT", b"WAIT"]);
    let waiters = quiet.step_to_own_publication();
    assert_eq!(waiters, 1, "VACUOUS: no WAIT was registered when cell 0 published");
    let turns_max = 2 * usize::from(CELLS).div_ceil(64) + 2;
    let mut flags = Vec::new();
    while flags.last() != Some(&true) {
        assert!(flags.len() < turns_max, "the cell never parks after its publication: {flags:?}");
        flags.push(quiet.step());
    }
    // The wake's re-check, if it is still owed, runs and parks again.
    for _ in 0..4 {
        quiet.step();
    }
    assert_eq!(quiet.published(1), 0, "the peer published");
    assert_eq!(quiet.node.plane(0).ckpt_waiters_for_sim(), 1, "the WAIT is still parked");
    assert_eq!(client.recv(&mut quiet.node), None, "an unsatisfied WAIT answered");
}

/// Out of the guard's scope (ADR-0159 D4): a `WAIT` on cell 0 whose only
/// publisher is cell 1 leaves cell 0 free to park, and the wake follows
/// cell 0's next completed sweep. A guard that held the cell awake for any
/// registered waiter would read no park here.
#[test]
fn a_peers_publication_does_not_hold_the_waiting_cell_awake() {
    let mut quiet = Quiet::boot(2, 0);
    let mut client = MiniClient::connect(&mut quiet.node, 0);
    client.send(&mut quiet.node, &[b"INF.CKPT", b"CELL", b"1", b"WAIT"]);
    let mut parks_while_waiting = 0u32;
    let mut steps = 0;
    while quiet.published(1) == 0 {
        assert!(steps < STEPS_MAX, "cell 1 published no checkpoint");
        let waiting = quiet.node.plane(0).ckpt_waiters_for_sim() == 1;
        parks_while_waiting += u32::from(quiet.step() && waiting);
        steps += 1;
    }
    assert_eq!(quiet.published(0), 0, "cell 0 published: the WAIT targets cell 1 alone");
    assert!(parks_while_waiting > 0, "cell 0 never parked while its WAIT waited on cell 1");
    let reply = (0..16).find_map(|_| {
        quiet.step();
        client.recv(&mut quiet.node)
    });
    assert_eq!(reply.as_deref(), Some(b"+OK\r\n".as_slice()), "the WAIT answers");
}
