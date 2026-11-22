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
    let held = rig.cells[0].plane.shared.gate.borrow().holders();
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
    let held = rig.cells[0].plane.shared.gate.borrow().holders();
    assert!(held >= 1, "engagement: the DEL sent no leg to the frozen owner");
    let window = crate::limits::GATE_UNITS_PER_CONN_TASK as usize;
    assert!(
        held <= window,
        "the gate holds {held} waiters for one connection's DEL of {KEYS} remote keys, past its \
         window of {window}"
    );
    assert!(rig.written(0, fd(0)).is_empty(), "no reply before the owner answers");
}

/// A node's fabric gate holds at most the key's index field, 2^24 holders (ADR-0177 D4): `H`
/// at the field builds, one past it is `Layout`, before anything is built.
#[test]
fn the_gates_holders_stop_at_the_keys_index_field() {
    let caps = crate::limits::plane_task_caps(1, 2).expect("one slot a cell");
    let units = crate::limits::gate_units(&caps);
    let credits = inf_runtime::limits::GATE_HOLDERS_MAX - units;
    let at = crate::limits::gate_holders(&caps, 2, credits).expect("H = 2^24");
    let at = at.expect("a gate at two cells");
    assert_eq!(at.entries_max(), inf_runtime::limits::GATE_HOLDERS_MAX);
    let past = crate::limits::gate_holders(&caps, 2, credits + 1);
    assert!(matches!(past, Err(CapError::Layout)), "H = 2^24 + 1 is refused");
}

/// A node whose `H` passes the key's index field builds no plane: `PlaneBuildError::Gate`
/// from the plane's build, before its executor allocates a slot. At two cells `C` = 2^19 + 1
/// connection slots of 32 units pass 2^24 alone.
#[test]
fn a_node_whose_holders_pass_the_field_builds_no_plane() {
    let cells = inf_foundation::CellCount::new(2).expect("two cells");
    let capacity = crate::config::BootCapacity::new((1 << 20) + 2, cells).expect("C fits");
    let config = crate::config::ConfigStore::default().with_boot_capacity(capacity);
    let (node, admission) = NodeInfo::try_new(config).expect("fixture cache allocation");
    let fabric = inf_fabric::Mesh::new(2, inf_fabric::MeshConfig::default());
    let fabric = fabric.into_iter().nth(1).expect("cell 1");
    let cfg = inf_store::StoreConfig::default();
    let seed = KeyspaceSeed::Uncataloged { cfg, specs: Vec::new() };
    let built: Result<(ServerPlane, _, _, _), _> = ServerPlane::new(
        CellId(1),
        2,
        -1,
        seed,
        fabric,
        Rc::new(node),
        admission,
        NoopObserver,
        false,
    );
    let refused = built.err().expect("H past the field refuses the plane");
    assert_eq!(refused, PlaneBuildError::Gate(CapError::Layout));
}

/// Runs `rig` a round at a time until `done` holds, at most `ROUNDS_MAX` rounds; a wait that
/// runs out is the test's engagement unmet: `VACUOUS <what>`.
fn until(rig: &mut Rig, what: &str, done: impl Fn(&Rig) -> bool) {
    for _ in 0..ROUNDS_MAX {
        if done(rig) {
            return;
        }
        rig.rounds(1);
    }
    assert!(done(rig), "VACUOUS {what}: not reached in {ROUNDS_MAX} rounds");
}

/// A pattern subscriber accepted on cell 0, its subscription confirmed: its close owes cell 1
/// a delta.
fn subscribed(rig: &mut Rig, subscriber: RawFd) {
    rig.accept(0, subscriber);
    rig.send(0, subscriber, &frame(&[b"PSUBSCRIBE", b"admit:*"]));
    until(rig, "subscribed", |rig| {
        rig.written(0, subscriber).starts_with(b"*3\r\n$10\r\npsubscribe")
    });
}

/// Cell 1 frozen and `subscriber` reset: its slot drains on cell 0, its delta's ack withheld,
/// until cell 0's sweep reads that one draining slot.
fn drain(rig: &mut Rig, subscriber: RawFd) {
    rig.freeze(1, true);
    rig.reset(0, subscriber);
    until(rig, "slot-undrained: one draining slot on cell 0", |rig| {
        rig.cells[0].plane.shared.node.draining_connections.get() == 1
    });
}

/// Whether cell 0 admitted `fd`: its receive armed, nothing sent.
fn admitted(rig: &Rig, fd: RawFd) -> bool {
    rig.recv_arms(0, fd) > 0 && rig.written(0, fd).is_empty()
}

/// What cell 0 sent on `fd`, as text.
fn sent(rig: &Rig, fd: RawFd) -> String {
    String::from_utf8_lossy(&rig.written(0, fd)).into_owned()
}

/// ADR-0123 A1's accept: a client is admitted while the cell's active clients are under the
/// hot share and its occupied slots under the boot share `C`, so a draining slot counts against
/// `C` alone. Boot `maxclients` 8 on two cells (`C` = 4), the share lowered to 2 by `CONFIG
/// SET maxclients 4`: a probe and a pattern subscriber on cell 0, the subscriber reset while
/// cell 1 withholds its delta's ack, so its slot drains. The next client is admitted (active 1,
/// occupied 2), as Redis admits a client under its `maxclients`; the one after is refused at
/// the share (active 2); once the slot vacates, the next is refused too (active 2). Engagement,
/// each else `VACUOUS`: cell 0's own knob reads the lowered share (the accept reads the knob,
/// which the cell's MAINTAIN refreshes, not the config store), and its sweep reads one draining
/// slot. The `CONFIG SET` answers once every cell has, so it is read before cell 1 freezes.
#[test]
#[ignore = "red until the accept compares the active share and the occupied slots apart"]
fn a_draining_slot_counts_against_the_boot_share_alone() {
    let mut rig = Rig::with_maxclients(2, 8);
    let (probe, subscriber) = (fd(0), fd(1));
    rig.accept(0, probe);
    subscribed(&mut rig, subscriber);
    rig.send(0, probe, &frame(&[b"CONFIG", b"SET", b"maxclients", b"4"]));
    until(&mut rig, "config-set-unanswered", |rig| rig.written(0, probe) == b"+OK\r\n");
    until(&mut rig, "share-unlowered: cell 0's knob at the lowered share", |rig| {
        rig.cells[0].plane.shared.knobs.get().maxclients_share == 2
    });
    drain(&mut rig, subscriber);
    rig.offer(0, fd(2));
    assert!(
        admitted(&rig, fd(2)),
        "a client under the lowered share of 2, beside one active client and one draining slot, \
         read {:?}: A1 and Redis admit it",
        sent(&rig, fd(2))
    );
    rig.offer(0, fd(3));
    assert_eq!(rig.written(0, fd(3)), MAXCLIENTS_REFUSAL, "active 2 is the lowered share");
    rig.freeze(1, false);
    until(&mut rig, "slot-unvacated", |rig| rig.cells[0].plane.connections() == 2);
    rig.offer(0, fd(4));
    assert_eq!(rig.written(0, fd(4)), MAXCLIENTS_REFUSAL, "active 2 once the slot vacates");
}

/// The control at the boot value: three clients and a pattern subscriber fill cell 0's share of
/// 4 (`C`), and the subscriber is reset while cell 1 withholds its ack, so its slot drains. The
/// next client is refused, the draining slot holding its place (occupied 4 = `C`: today's
/// refusal, which A1 keeps); once the slot vacates, the next is admitted.
#[test]
fn a_draining_slot_holds_its_place_at_the_boot_share() {
    let mut rig = Rig::with_maxclients(2, 8);
    for n in 0..3 {
        rig.accept(0, fd(n));
    }
    let subscriber = fd(3);
    subscribed(&mut rig, subscriber);
    drain(&mut rig, subscriber);
    rig.offer(0, fd(4));
    assert_eq!(
        rig.written(0, fd(4)),
        MAXCLIENTS_REFUSAL,
        "three active clients and one draining slot occupy the boot share of 4"
    );
    rig.freeze(1, false);
    until(&mut rig, "slot-unvacated", |rig| rig.cells[0].plane.connections() == 3);
    rig.offer(0, fd(5));
    assert!(admitted(&rig, fd(5)), "the vacated slot admits: read {:?}", sent(&rig, fd(5)));
}
