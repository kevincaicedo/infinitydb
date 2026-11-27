//! The census's `Stopping` mark has two owners, the cell's loop and its
//! plane, the two aggregates that own the cell's containers: each marks it
//! in its `Drop` before its fields drop, so a container drops after the
//! first of the two and the order between them is no one's. Each test
//! builds one cell, runs one iteration (the census serves), drops one of
//! the two, and reads the phase through its own clone of the census before
//! the other drops.

use super::*;
use inf_alloc::BufferPool;
use inf_fabric::{Mesh, MeshConfig};
use inf_foundation::bounded::CensusPhase;
use inf_foundation::time::VirtualClock;
use inf_runtime::{BackendDriver, Capabilities, CellLoop, LoopConfig, SubmitStats, Wait};
use inf_store::StoreConfig;

/// A driver that completes nothing: one iteration needs no I/O.
struct Idle;

impl BackendDriver for Idle {
    fn push(&mut self, _: IoOp) {}

    fn submit_and_reap(
        &mut self,
        _: &mut BufferPool,
        _: Wait,
        _: &mut Vec<Completion>,
    ) -> std::io::Result<usize> {
        Ok(0)
    }

    fn register_pool(&mut self, _: &mut BufferPool) -> std::io::Result<()> {
        Ok(())
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            backend: "idle",
            multishot_accept: false,
            multishot_recv: false,
            provided_buffers: false,
            fixed_buffers: false,
            single_issuer: true,
            defer_taskrun: false,
            performance_tier: false,
        }
    }

    fn submit_stats(&self) -> SubmitStats {
        SubmitStats::default()
    }
}

/// One serving cell: its loop, its plane and the test's own census handle.
fn serving_cell() -> (CellLoop<Idle, VirtualClock>, ServerPlane, Rc<CapCensus>) {
    let fabric = Mesh::new(1, MeshConfig::default()).into_iter().next().expect("one cell");
    let (fixture_node, admission) = NodeInfo::try_default().expect("fixture cache allocation");
    let (mut plane, serve_mark, timer_set, executor) = ServerPlane::new(
        CellId(0),
        1,
        -1,
        KeyspaceSeed::Uncataloged { cfg: StoreConfig::default(), specs: Vec::new() },
        fabric,
        Rc::new(fixture_node),
        admission,
        NoopObserver,
        false,
    )
    .expect("the server's timer owners fit their cap");
    let census = Rc::clone(serve_mark.census());
    let mut reactor = CellLoop::new(
        Idle,
        VirtualClock::default(),
        BufferPool::new(4, 4096),
        LoopConfig::default(),
        serve_mark,
        timer_set,
        executor,
    );
    reactor.run_iteration(&mut plane).expect("the idle driver never fails");
    assert_eq!(census.phase(), CensusPhase::Serving, "the first iteration took the mark");
    (reactor, plane, census)
}

#[test]
fn the_plane_dropped_first_marks_the_census_stopping() {
    let (reactor, plane, census) = serving_cell();
    drop(plane);
    assert_eq!(census.phase(), CensusPhase::Stopping, "the plane's drop marks before its fields");
    drop(reactor);
    assert_eq!(census.phase(), CensusPhase::Stopping);
}

#[test]
fn the_loop_dropped_first_marks_the_census_stopping() {
    let (reactor, plane, census) = serving_cell();
    drop(reactor);
    assert_eq!(census.phase(), CensusPhase::Stopping, "the loop's drop marks before its fields");
    drop(plane);
    assert_eq!(census.phase(), CensusPhase::Stopping);
}

// ---- the stop's mark ends every reader (ADR-0124 D2, ADR-0196 A1.1) ----

use super::wire_rig::{Rig as Node, frame};
use inf_foundation::time::Clock;

const SLEEPER: RawFd = 70;
const HALF: RawFd = 71;
const GONE: RawFd = 72;

/// During a `DEBUG SLEEP`, one client's `SET k1` and its end of stream are
/// reaped (`Flushing` `Peer`, its entry held by the stall) and another's
/// `SET k2` and a reset (`Draining` `Peer` at `Closed`, its entry held);
/// with `stop`, the stop is requested before the sleep ends. Then the sleep
/// ends.
fn held_readers(stop: bool) -> Node {
    let mut node = Node::new(1);
    for fd in [SLEEPER, HALF, GONE] {
        node.accept(0, fd);
    }
    node.send(0, SLEEPER, &frame(&[b"DEBUG", b"SLEEP", b"1"]));
    node.rounds(1);
    let stalled = node.cells[0].plane.shared.stall_until.get();
    assert!(stalled > node.clock.now(), "engagement: DEBUG SLEEP stalls the cell");
    node.send(0, HALF, &frame(&[b"SET", b"k1", b"v"]));
    node.eof(0, HALF);
    node.send(0, GONE, &frame(&[b"SET", b"k2", b"v"]));
    node.reset(0, GONE);
    node.rounds(2);
    assert_eq!(node.cells[0].plane.inbox.len(), 2, "engagement: both entries wait in the inbox");
    assert!(node.closed(0).contains(&GONE), "engagement: the reset's Closed came in the stall");
    if stop {
        node.cells[0].plane.request_stop();
        node.rounds(1);
    }
    node.clock.advance(Nanos::from_secs(2));
    node.rounds(8);
    node
}

/// Whether `key` holds a value on the node's one cell.
fn landed(node: &Node, key: &[u8]) -> bool {
    let now = node.clock.now();
    node.cells[0].plane.shared.store.borrow_mut().db_mut(0).exists(key, now)
}

/// The stop's one walk ends every parser (ADR-0196 A1.1): what is parsed
/// and queued runs, what is not yet parsed is released. Neither held `SET`
/// runs once the sleep ends, every slot vacates and the stop reaches
/// `Quiet`.
#[test]
fn stop_ends_every_reader() {
    let node = held_readers(true);
    for key in [&b"k1"[..], b"k2"] {
        assert!(!landed(&node, key), "set-after-stop: a held SET ran after the stop's mark");
    }
    assert_eq!(node.cells[0].plane.connections(), 0, "every slot vacates");
    assert_eq!(node.cells[0].plane.stop_phase(), StopPhase::Quiet, "the stop reaches Quiet");
}

/// The control: with no stop, both held `SET`s run once the sleep ends.
#[test]
fn every_held_reader_runs_with_no_stop() {
    let node = held_readers(false);
    for key in [&b"k1"[..], b"k2"] {
        assert!(landed(&node, key), "a held SET did not run with no stop");
    }
}

/// The stop's barrier counts a draining slot until its pump's 1→0 deltas
/// are acknowledged (ADR-0177 D2): a subscriber of a channel cell 1 owns is
/// reset while cell 1 is frozen, so its deltas wait, and cell 0's stop holds
/// at its connections until cell 1 answers, then reaches `Quiet`.
#[test]
fn the_stop_waits_for_a_draining_slots_acks() {
    let mut node = Node::new(2);
    node.accept(0, HALF);
    let channel = node.key_on(1, "drain");
    node.send(0, HALF, &frame(&[b"SUBSCRIBE", &channel]));
    node.rounds(8);
    assert!(!node.written(0, HALF).is_empty(), "engagement: the subscription confirmed");
    node.freeze(1, true);
    node.reset(0, HALF);
    node.rounds(4);
    assert!(node.closed(0).contains(&HALF), "engagement: the reset's Closed was reaped");
    assert!(
        node.cells[0].plane.shared.gate.borrow().holders() >= 1,
        "engagement: a delta is unanswered"
    );
    node.cells[0].plane.request_stop();
    node.rounds(4);
    assert_eq!(
        node.cells[0].plane.stop_phase(),
        StopPhase::Draining,
        "quiet-before-acks: the stop passed a draining slot whose deltas are unanswered"
    );
    node.freeze(1, false);
    node.rounds(8);
    assert_eq!(node.cells[0].plane.connections(), 0, "the draining slot vacates once answered");
    assert_eq!(node.cells[0].plane.stop_phase(), StopPhase::Quiet, "the stop reaches Quiet");
}
