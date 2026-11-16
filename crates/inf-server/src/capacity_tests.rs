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
/// and its plane-start pumps (O1): each closed subscriber's deltas ship
/// from its own pump, whose draining slot stays occupied until the acks.
#[test]
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

/// One client on cell 0 sends a `DEL` of 100 keys cell 1 owns, and cell 1
/// is frozen: no leg is answered. A connection holds at most its window of
/// gate holders, so the gate holds at most 32 (ADR-0177 D4, the windowed
/// fan); at HEAD the pump checks its window before the whole command and
/// then sends a leg for every remote key.
#[test]
#[ignore = "a fan checks the connection's window of 32 remote legs once, before the command, \
            then sends a leg and takes a gate holder per remote key; red until each leg takes \
            one of the connection's 32 gate units and a sender with none settles its oldest leg"]
fn a_remote_del_holds_at_most_its_window_of_gate_holders() {
    const KEYS: usize = 100;
    let mut rig = Rig::new(2);
    rig.accept(0, fd(0));
    rig.freeze(1, true);
    let keys = rig.keys_on(1, "fan", KEYS);
    let mut argv: Vec<&[u8]> = vec![b"DEL"];
    argv.extend(keys.iter().map(Vec::as_slice));
    rig.send(0, fd(0), &frame(&argv));
    rig.rounds(4);
    let held = rig.cells[0].plane.shared.gate.pending();
    assert!(held >= 1, "engagement: the DEL sent no leg to the frozen owner");
    assert!(
        held <= REMOTE_WINDOW,
        "the gate holds {held} waiters for one connection's DEL of {KEYS} remote keys, past its \
         window of {REMOTE_WINDOW}"
    );
    assert!(rig.written(0, fd(0)).is_empty(), "no reply before the owner answers");
}
