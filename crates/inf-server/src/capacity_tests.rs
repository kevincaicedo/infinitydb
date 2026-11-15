//! What a cell's tasks and gate hold while a peer withholds its replies. A
//! connection's work is one task, alive while its slot is occupied, and the
//! cell's long-lived pumps are made once at plane start (ADR-0177 D2); a
//! connection holds at most a window of gate holders (ADR-0177 D4). Each test
//! drives two cells of one node and freezes cell 1, so every fabric operation
//! cell 0 sends there stays unanswered.

use super::wire_rig::{Rig, frame};
use super::*;

/// The pumps plane start makes on a node of `cells` cells (ADR-0177 D2): one
/// `ns-apply` pump per peer, the `owner-pub` pump and the `gated` pump.
fn plane_start_tasks(cells: u16) -> usize {
    usize::from(cells - 1) + 2
}

/// Scheduler rounds a two-cell exchange may take before the test calls it
/// stalled.
const ROUNDS_MAX: usize = 64;

/// The descriptor of the `n`-th client.
fn fd(n: usize) -> RawFd {
    RawFd::try_from(100 + n).expect("a small descriptor")
}

/// 200 clients on cell 0 each subscribe to a pattern of their own, so each
/// close takes its pattern from one subscriber to none and owes cell 1 a
/// delta. Cell 1 is frozen before the closes: the deltas stay unanswered.
/// The tasks alive on cell 0 are then at most its occupied connection slots
/// and its plane-start pumps (O1); at HEAD every closed subscriber's delta
/// is a task of its own, alive after its connection left the slab.
#[test]
#[ignore = "a closed subscriber's deltas are a new task per close, alive after its connection \
            left the slab until the peer acknowledges them; red until a connection's one task \
            ships its deltas from a draining slot that stays occupied until the last ack"]
fn a_closed_subscriber_leaves_no_task_beside_its_slot() {
    const SUBSCRIBERS: usize = 200;
    let mut rig = Rig::new(2);
    for n in 0..SUBSCRIBERS {
        rig.accept(0, fd(n));
        let pattern = format!("cap:{n}:*");
        rig.send(0, fd(n), &frame(&[b"PSUBSCRIBE", pattern.as_bytes()]));
    }
    let confirmed = |rig: &Rig| {
        (0..SUBSCRIBERS).all(|n| rig.written(0, fd(n)).starts_with(b"*3\r\n$10\r\npsubscribe"))
    };
    let mut rounds = 0;
    while !confirmed(&rig) {
        assert!(rounds < ROUNDS_MAX, "the subscriptions did not confirm in {ROUNDS_MAX} rounds");
        rig.rounds(1);
        rounds += 1;
    }
    let before = rig.cells[0].reactor.executor().live_tasks();
    rig.freeze(1, true);
    for n in 0..SUBSCRIBERS {
        rig.eof(0, fd(n));
    }
    rig.rounds(8);
    assert_eq!(rig.closed(0).len(), SUBSCRIBERS, "engagement: every subscriber's close completed");
    let held = rig.cells[0].plane.shared.gate.pending();
    assert!(held >= SUBSCRIBERS, "engagement: {held} deltas are unanswered, not {SUBSCRIBERS}");
    let live = rig.cells[0].reactor.executor().live_tasks();
    let occupied = rig.cells[0].plane.connections();
    assert!(
        live <= occupied + plane_start_tasks(2),
        "{live} tasks alive on cell 0 for {occupied} occupied connection slots and {} plane-start \
         pumps ({before} before the closes), after {SUBSCRIBERS} subscribers closed with their \
         deltas unanswered",
        plane_start_tasks(2)
    );
}
