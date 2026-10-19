//! The loop-tier park instrument: what a cell's reactor decides in the
//! iterations after the cell publishes a checkpoint, read on the shipped
//! plane over the sim driver.
//!
//! - **Estimator and scope:** `IterStats::parked` of one cell's reactor
//!   iteration, kept by [`Node::iter_stats`]: the cell a [`Quiet`] node
//!   watches, any cell of the node. It is the reactor's own decision (spin
//!   exhausted, no ready task, no `before_park` veto), made before the
//!   driver call, so it reads the state the iteration before it left. The
//!   sim driver ignores the wait, so a park is this flag and never elapsed
//!   virtual time.
//! - **Resolution:** one reactor iteration. The harness runs each cell
//!   once per scheduler step, and the publishing iteration is the step in
//!   which the cell's board slot moved. A step that skips the watched cell
//!   (frozen) reads nothing: no flag, no count.
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
    /// The cell whose reactor the instrument reads.
    watched: u16,
    /// Iterations of the watched cell the instrument read, and how many
    /// parked.
    iterations_read: u64,
    parks_read: u64,
}

impl Quiet {
    /// Boots a `cells`-cell node and watches cell `watched`.
    pub(super) fn boot(cells: u16, spin_iters: u32, watched: u16) -> Quiet {
        assert!(watched < cells, "the watched cell is on the node");
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
            watched,
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

    /// One scheduler step. Returns the park decision of the iteration the
    /// watched cell ran in it, `None` when the step skipped that cell
    /// (frozen): it ran no iteration, so nothing is read or counted.
    pub(super) fn step(&mut self) -> Option<bool> {
        self.node.step(&mut self.rng, &self.clock, &self.disk, self.step_ns_max).expect("step");
        let parked = self.node.iter_stats(usize::from(self.watched))?.parked;
        self.iterations_read += 1;
        self.parks_read += u64::from(parked);
        Some(parked)
    }

    /// One scheduler step in which the watched cell runs: its park decision.
    fn step_watched(&mut self) -> bool {
        self.step().expect("the watched cell is frozen: it ran no iteration to read")
    }

    pub(super) fn published(&self, cell: u16) -> u64 {
        self.node.control.ckpt_board().slot(cell).published()
    }

    /// Pumps parked on the watched cell's checkpoint waitlist.
    pub(super) fn waiters(&self) -> usize {
        self.node.plane(usize::from(self.watched)).ckpt_waiters_for_sim()
    }

    /// Steps until the watched cell's slot moves: the step that returns is
    /// its publishing iteration. Returns the waiters registered on the cell
    /// when that iteration began.
    fn step_to_own_publication(&mut self) -> usize {
        let before = self.published(self.watched);
        for _ in 0..STEPS_MAX {
            let waiters = self.waiters();
            self.step_watched();
            if self.published(self.watched) != before {
                return waiters;
            }
        }
        panic!("cell {} published no checkpoint in {STEPS_MAX} steps", self.watched);
    }
}

/// What the instrument read around one own publication of the watched cell.
#[derive(Debug, PartialEq, Eq)]
struct ParkRead {
    /// Waiters on the cell when its publishing iteration began.
    waiters_at_publication: usize,
    /// `parked` of the iteration after the publishing one.
    parked_after: bool,
    parks_read: u64,
    iterations_read: u64,
}

/// The control leg's run: a 1-cell node, `INF.CKPT` with no `WAIT`, read
/// through the iteration after the publishing one.
fn publication_with_no_waiter(spin_iters: u32) -> ParkRead {
    let mut quiet = Quiet::boot(1, spin_iters, 0);
    let mut client = MiniClient::connect(&mut quiet.node, 0);
    let Quiet { node, rng, clock, disk, step_ns_max, .. } = &mut quiet;
    let reply = client.call(node, rng, clock, disk, *step_ns_max, &[b"INF.CKPT"]).expect("call");
    assert_eq!(reply.as_deref(), Some(b"+OK\r\n".as_slice()), "INF.CKPT");
    let waiters_at_publication = quiet.step_to_own_publication();
    let parked_after = quiet.step_watched();
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

/// What the instrument read from an own publication through its wake.
#[derive(Debug)]
struct WakeWindow {
    /// `parked` of each iteration from the one after the publishing
    /// iteration through the one whose MAINTAIN woke the waitlist.
    flags: Vec<bool>,
    /// Parks the instrument read up to the publishing iteration: its
    /// liveness in this run.
    parks_before: u64,
}

/// An own publication of cell `waiter` with `argv` (a `WAIT` that this
/// publication satisfies) parked on that cell, on a `cells`-cell node at
/// `spin_iters: 0`. The cell first sits out `skipped_steps` scheduler
/// steps, which moves its sweep's phase at the publication. The window is
/// bounded by the guard's limit, `2 * ceil(N / 64)` turns, and the `WAIT`
/// then answers.
fn own_publication_window(
    cells: u16,
    waiter: u16,
    skipped_steps: u32,
    argv: &[&[u8]],
) -> WakeWindow {
    let mut quiet = Quiet::boot(cells, 0, waiter);
    quiet.node.frozen = Some((usize::from(waiter), u64::from(skipped_steps)));
    for _ in 0..skipped_steps {
        assert_eq!(quiet.step(), None, "a skipped cell runs no iteration");
    }
    let mut client = MiniClient::connect(&mut quiet.node, usize::from(waiter));
    client.send(&mut quiet.node, argv);
    let waiters = quiet.step_to_own_publication();
    assert_eq!(waiters, 1, "VACUOUS: no WAIT was registered when cell {waiter} published");
    let parks_before = quiet.parks_read;
    let turns_max = 2 * usize::from(cells).div_ceil(64);
    let mut flags = Vec::new();
    while quiet.waiters() > 0 {
        assert!(
            flags.len() < turns_max,
            "{cells} cells: the waitlist was not woken within {turns_max} turns: {flags:?}"
        );
        flags.push(quiet.step_watched());
    }
    let reply = (0..16).find_map(|_| {
        quiet.step();
        client.recv(&mut quiet.node)
    });
    assert_eq!(reply.as_deref(), Some(b"+OK\r\n".as_slice()), "the WAIT answers");
    WakeWindow { flags, parks_before }
}

/// The park guard's own-slot term at the loop tier (`interfaces-m2.md`,
/// "Cells never fold the whole board"): with `spin_iters: 0` and an
/// `INF.CKPT WAIT` parked on the cell that publishes last, no iteration
/// parks from the one after the publishing iteration through the one whose
/// MAINTAIN wakes the waitlist: one turn at 1 cell. Without the term the
/// cell parks on top of a satisfied `WAIT` and answers it one park timeout
/// late.
#[test]
fn an_own_publication_with_a_waiter_does_not_park_before_its_wake() {
    let read = own_publication_window(1, 0, 0, &[b"INF.CKPT", b"WAIT"]);
    assert!(read.parks_before > 0, "liveness: the instrument read no park in {read:?}");
    assert!(
        !read.flags.contains(&true),
        "a parked iteration between the publication and its wake (parked flags, from the \
         iteration after the publishing one through the waking one): {:?}",
        read.flags
    );
    assert_eq!(read.flags.len(), 1, "a 1-cell sweep wakes in one turn: {:?}", read.flags);
}

/// The same term at the shipped multi-cell topology, on a cell other than
/// cell 0: `INF.CKPT CELL 1 WAIT` on cell 1 at 2, 65 and 130 cells. Beyond
/// 64 cells a sweep takes `S = ceil(N / 64)` turns. The longest window is
/// the one in which the sweep in progress at the publication had already
/// read slot 1: it completes without the publication, the own-slot term
/// alone holds the cell at that completion, and the wake waits for the
/// next sweep, more than `S` turns in all. Each topology is run at up to
/// `S` sweep phases, until one reaches that window. A sweep that watched
/// another cell's slot (`inf_canary_ckpt_sweep_own_slot_zero`) parks at
/// every topology here, as the guard without the term does.
#[test]
fn an_own_publication_on_a_peer_cell_does_not_park_before_its_wake() {
    let mut parked = Vec::new();
    for cells in [2u16, 65, 130] {
        let sweep_turns = usize::from(cells).div_ceil(64);
        let mut held_by_the_own_slot_term = false;
        for skipped_steps in 0..sweep_turns {
            let read = own_publication_window(
                cells,
                1,
                skipped_steps as u32,
                &[b"INF.CKPT", b"CELL", b"1", b"WAIT"],
            );
            assert!(read.parks_before > 0, "liveness: the instrument read no park in {read:?}");
            if read.flags.contains(&true) {
                parked.push((cells, read.flags));
                break;
            }
            // At one turn per sweep the one flag is the own-slot term's.
            if sweep_turns == 1 || read.flags.len() > sweep_turns {
                held_by_the_own_slot_term = true;
                break;
            }
        }
        assert!(
            held_by_the_own_slot_term || parked.last().is_some_and(|(at, _)| *at == cells),
            "VACUOUS: at {cells} cells no phase left the own-slot term alone in holding the cell"
        );
    }
    assert!(
        parked.is_empty(),
        "a parked iteration between an own publication on cell 1 and its wake, as (cells, \
         parked flags from the iteration after the publishing one through the waking one): \
         {parked:?}"
    );
}

/// The park guard's cursor term (ADR-0159 A1.4) at the loop tier, beyond
/// 64 cells: a cell with a registered waiter does not park while its sweep
/// is part-way. The `WAIT` targets a peer that never publishes, so there
/// is no own publication and the cursor term is the only one that holds.
/// A sweep takes `S = ceil(N / 64)` turns: the cell parks after the turn
/// that completes one and stays unparked through the next `S - 1`, so its
/// parks are `S` turns apart: closer and it parked part-way, farther and
/// the guard outlived its sweep. The control leg is the same node before
/// the `WAIT`: with no waiter it parks turn after turn, and the same
/// oracle reads adjacent parks. Without the term
/// (`inf_canary_ckpt_park_guard_cursor_skipped`) the waiting cell does too.
#[test]
fn a_part_way_sweep_holds_a_waiting_cell_awake() {
    let mut parked_part_way = Vec::new();
    for cells in [65u16, 130] {
        let sweep_turns = usize::from(cells).div_ceil(64);
        let turns = 4 * sweep_turns;
        let mut quiet = Quiet::boot(cells, 0, 0);
        let idle: Vec<bool> = (0..turns).map(|_| quiet.step_watched()).collect();
        assert_eq!(quiet.waiters(), 0, "the control leg has no waiter");
        assert_eq!(
            closest_parks(&idle),
            Some(1),
            "the control leg: an idle cell with no waiter parks turn after turn: {idle:?}"
        );
        // The peer is never stepped again, so its slot stays at 0.
        quiet.node.frozen = Some((1, u64::MAX));
        let mut client = MiniClient::connect(&mut quiet.node, 0);
        client.send(&mut quiet.node, &[b"INF.CKPT", b"CELL", b"1", b"WAIT"]);
        // The command's own turns are work; the read starts once they end.
        for _ in 0..64 {
            quiet.step_watched();
        }
        let waiting: Vec<bool> = (0..turns).map(|_| quiet.step_watched()).collect();
        assert_eq!(quiet.waiters(), 1, "VACUOUS: the WAIT is not parked on cell 0");
        assert_eq!(quiet.published(0), 0, "VACUOUS: cell 0 published, so the own-slot term held");
        assert_eq!(quiet.published(1), 0, "the peer published");
        assert_eq!(client.recv(&mut quiet.node), None, "an unsatisfied WAIT answered");
        assert!(waiting.contains(&true), "the guard never ended at {cells} cells: {waiting:?}");
        if closest_parks(&waiting) != Some(sweep_turns) {
            parked_part_way.push((cells, waiting));
        }
    }
    assert!(
        parked_part_way.is_empty(),
        "a waiting cell parked while its sweep was part-way, or stayed awake past it: its parks \
         are not ceil(N / 64) turns apart, as (cells, parked flags): {parked_part_way:?}"
    );
}

/// The fewest turns between two parks in `flags`: 1 for adjacent parks,
/// `None` with fewer than two parks.
fn closest_parks(flags: &[bool]) -> Option<usize> {
    let parks: Vec<usize> = (0..flags.len()).filter(|&turn| flags[turn]).collect();
    parks.windows(2).map(|pair| pair[1] - pair[0]).min()
}

/// The guard ends: an all-cell `WAIT` on a 2-cell node whose peer never
/// publishes is not satisfied by this cell's own publication. The own-slot
/// term holds for one turn at 64 cells or fewer, the wake's re-check takes
/// the next, and then the cell parks with its waiter still registered.
#[test]
fn a_waiter_the_own_publication_does_not_satisfy_lets_the_cell_park() {
    let mut quiet = Quiet::boot(2, 0, 0);
    let mut client = MiniClient::connect(&mut quiet.node, 0);
    // The peer is never stepped again, so its slot stays at 0.
    quiet.node.frozen = Some((1, u64::MAX));
    client.send(&mut quiet.node, &[b"INF.CKPT", b"WAIT"]);
    let waiters = quiet.step_to_own_publication();
    assert_eq!(waiters, 1, "VACUOUS: no WAIT was registered when cell 0 published");
    let mut flags = Vec::new();
    while flags.last() != Some(&true) {
        assert!(flags.len() < 3, "the cell never parks after its publication: {flags:?}");
        flags.push(quiet.step_watched());
    }
    assert_eq!(flags, [false, false, true], "one guard turn, the re-check's turn, then a park");
    // Nothing owed is left: the cell stays parked on its unsatisfied WAIT.
    for _ in 0..4 {
        quiet.step();
    }
    assert_eq!(quiet.published(1), 0, "the peer published");
    assert_eq!(quiet.waiters(), 1, "the WAIT is still parked");
    assert_eq!(client.recv(&mut quiet.node), None, "an unsatisfied WAIT answered");
}

/// Out of the guard's scope (ADR-0159 D4): a `WAIT` on cell 0 whose only
/// publisher is cell 1 leaves cell 0 free to park, and the wake follows
/// cell 0's next completed sweep. A guard that held the cell awake for any
/// registered waiter would read no park here.
#[test]
fn a_peers_publication_does_not_hold_the_waiting_cell_awake() {
    let mut quiet = Quiet::boot(2, 0, 0);
    let mut client = MiniClient::connect(&mut quiet.node, 0);
    client.send(&mut quiet.node, &[b"INF.CKPT", b"CELL", b"1", b"WAIT"]);
    let mut parks_while_waiting = 0u32;
    let mut steps = 0;
    while quiet.published(1) == 0 {
        assert!(steps < STEPS_MAX, "cell 1 published no checkpoint");
        let waiting = quiet.waiters() == 1;
        parks_while_waiting += u32::from(quiet.step_watched() && waiting);
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
