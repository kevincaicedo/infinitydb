//! A connection's descriptor is closed once. A close pushed for a slot
//! already closing reaches `close(2)` again after the first has returned the
//! number, which the next accept or file open may already hold. The fixture
//! drives the real plane through the loop with a driver that records what it
//! is handed and completes nothing it is not told to: a `Close` stays in
//! flight until a test delivers its `Closed`.

use std::cell::RefCell;

use super::*;
use inf_alloc::{BufferPool, LeaseKind};
use inf_fabric::{Mesh, MeshConfig};
use inf_foundation::time::VirtualClock;
use inf_runtime::{BackendDriver, Capabilities, CellLoop, LoopConfig, SubmitStats, Wait};
use inf_store::StoreConfig;

/// The one accepted descriptor of every case.
const FD: RawFd = 41;

/// A completion the next reap delivers.
enum Delivery {
    Accepted(RawFd),
    /// A zero-length receive: the peer's EOF.
    Eof(CompletionToken),
}

#[derive(Default)]
struct Script {
    deliver: Vec<Delivery>,
    /// The receive arm the plane pushed for each descriptor.
    armed: Vec<(RawFd, CompletionToken)>,
    /// Every `Close` the plane pushed, in order.
    closes: Vec<RawFd>,
}

#[derive(Default)]
struct RecordingDriver {
    script: RefCell<Script>,
}

impl BackendDriver for RecordingDriver {
    fn push(&mut self, op: IoOp) {
        let script = self.script.get_mut();
        match op {
            IoOp::RecvArm { fd, token } => script.armed.push((fd, token)),
            IoOp::Close { fd, .. } => script.closes.push(fd),
            _ => {}
        }
    }

    fn submit_and_reap(
        &mut self,
        pool: &mut BufferPool,
        _: Wait,
        out: &mut Vec<Completion>,
    ) -> std::io::Result<usize> {
        for delivery in self.script.get_mut().deliver.drain(..) {
            out.push(match delivery {
                Delivery::Accepted(fd) => Completion {
                    token: CompletionToken::new(TokenClass::Accept, 0, 0),
                    result: CompletionResult::Accepted { fd },
                },
                Delivery::Eof(token) => Completion {
                    token,
                    result: CompletionResult::Recv {
                        buf: pool
                            .try_lease(LeaseKind::Recv)
                            .expect("the fixture pool has a buffer"),
                        len: 0,
                    },
                },
            });
        }
        Ok(out.len())
    }

    fn register_pool(&mut self, _: &mut BufferPool) -> std::io::Result<()> {
        Ok(())
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            backend: "recording",
            multishot_accept: true,
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

/// One cell with one admitted connection: its client id and its receive
/// token.
struct Rig {
    reactor: CellLoop<RecordingDriver, VirtualClock>,
    plane: ServerPlane,
    id: u64,
    recv: CompletionToken,
}

impl Rig {
    fn admitted() -> Rig {
        let fabric = Mesh::new(1, MeshConfig::default()).into_iter().next().expect("one cell");
        let (mut plane, serve_mark, timer_set, executor) = ServerPlane::new(
            CellId(0),
            1,
            -1,
            KeyspaceSeed::Uncataloged { cfg: StoreConfig::default(), specs: Vec::new() },
            fabric,
            Rc::new(NodeInfo::try_default().expect("fixture cache allocation")),
            NoopObserver,
            false,
        )
        .expect("the server's timer owners fit their cap");
        let mut reactor = CellLoop::new(
            RecordingDriver::default(),
            VirtualClock::default(),
            BufferPool::new(4, 4096),
            LoopConfig::default(),
            serve_mark,
            timer_set,
            executor,
        );
        reactor.driver().script.borrow_mut().deliver.push(Delivery::Accepted(FD));
        for _ in 0..2 {
            reactor.run_iteration(&mut plane).expect("the recording driver never fails");
        }
        let ids: Vec<u64> = plane.shared.node.clients.borrow().iter().map(|(id, _)| id).collect();
        assert_eq!(ids.len(), 1, "the accept registers one client");
        let armed = reactor.driver().script.borrow().armed.clone();
        assert_eq!(armed.len(), 1, "the accept arms one receive");
        assert_eq!(armed[0].0, FD);
        Rig { reactor, plane, id: ids[0], recv: armed[0].1 }
    }

    fn deliver(&self, delivery: Delivery) {
        self.reactor.driver().script.borrow_mut().deliver.push(delivery);
    }

    /// `CLIENT KILL ID`'s mark, which the next MAINTAIN sweeps.
    fn kill(&self) {
        let marked = self.plane.shared.node.clients.borrow_mut().request_kill(self.id);
        assert!(marked, "the id resolves until `Closed`");
    }

    /// Ops a plane pushes reach the driver at the next iteration's submit.
    fn run(&mut self, iterations: usize) {
        for _ in 0..iterations {
            self.reactor.run_iteration(&mut self.plane).expect("the recording driver never fails");
        }
    }

    fn closes(&self) -> Vec<RawFd> {
        self.reactor.driver().script.borrow().closes.clone()
    }
}

/// Control: two marks of one id before one sweep are one mark (the
/// registry's flag), so the sweep closes the connection once.
#[test]
fn two_kills_of_one_id_before_one_sweep_close_it_once() {
    let mut rig = Rig::admitted();
    rig.kill();
    rig.kill();
    rig.run(2);
    assert_eq!(rig.closes(), [FD], "one sweep closes a killed connection once");
}

/// A second kill of one id, swept while its first close is in flight, pushes
/// no second `Close`.
#[test]
fn a_kill_swept_while_its_close_is_in_flight_closes_nothing_again() {
    let mut rig = Rig::admitted();
    rig.kill();
    rig.run(2);
    assert_eq!(rig.closes(), [FD], "the first sweep closes the connection");
    rig.kill();
    rig.run(2);
    assert_eq!(rig.closes(), [FD], "a connection already closing was closed twice");
}

/// A kill swept after the peer's EOF closed the connection, before its
/// `Closed`, pushes no second `Close`.
#[test]
fn a_kill_after_the_eof_close_before_closed_closes_nothing_again() {
    let mut rig = Rig::admitted();
    rig.deliver(Delivery::Eof(rig.recv));
    rig.run(2);
    assert_eq!(rig.closes(), [FD], "the EOF closes the connection");
    rig.kill();
    rig.run(2);
    assert_eq!(rig.closes(), [FD], "a connection already closing was closed twice");
}

// ---- a close runs the bytes the client handed over (ADR-0196, ADR-0123 A2) ----

use super::wire_rig::{Rig as Node, frame};
use inf_foundation::time::Clock;

/// The client whose close each test drives, and a second client of the
/// same cell that reads the store back over the wire.
const CLIENT: RawFd = 60;
const READER: RawFd = 61;

/// `GET key` from the reader, run until its reply arrives.
fn read_back(node: &mut Node, cell: usize, key: &[u8]) -> Vec<u8> {
    let before = node.written(cell, READER).len();
    node.send(cell, READER, &frame(&[b"GET", key]));
    node.rounds(4);
    node.written(cell, READER)[before..].to_vec()
}

/// A client's `DEL` of 32 keys cell 1 owns fills its pump's window of
/// remote legs while cell 1 is frozen, so the `SET` and the `INCR` behind
/// it stay queued; then the client's socket is reset (the peer is gone)
/// and its `Closed` is reaped before cell 1 answers. Every queued command
/// still runs, unanswered (ADR-0123 A2): the key reads 2. At HEAD `Closed`
/// drops the connection with its queue, so the key is never written.
#[test]
#[ignore = "Closed drops a connection with its queue, so commands the client handed over \
            before a reset never run; red until a draining slot keeps its queue and its pump \
            runs every queued command unanswered before the slot vacates"]
fn a_reset_connection_runs_its_queued_commands() {
    let mut node = Node::new(2);
    node.accept(0, CLIENT);
    node.accept(0, READER);
    node.freeze(1, true);
    let held = node.keys_on(1, "held", REMOTE_WINDOW);
    let key = node.key_on(0, "queued");
    let mut argv: Vec<&[u8]> = vec![b"DEL"];
    argv.extend(held.iter().map(Vec::as_slice));
    let mut bytes = frame(&argv);
    bytes.extend(frame(&[b"SET", &key, b"1"]));
    bytes.extend(frame(&[b"INCR", &key]));
    node.send(0, CLIENT, &bytes);
    node.rounds(2);
    let mut queued = 0;
    node.cells[0].plane.shared.conns.borrow_mut().for_each_mut(|conn| queued += conn.queue.len());
    assert_eq!(queued, 2, "engagement: the SET and the INCR are queued behind the held DEL");
    node.reset(0, CLIENT);
    node.rounds(2);
    assert!(node.closed(0).contains(&CLIENT), "engagement: the reset's Closed was reaped");
    node.freeze(1, false);
    node.rounds(8);
    assert_eq!(
        read_back(&mut node, 0, &key),
        b"$1\r\n2\r\n",
        "the SET and the INCR queued before the reset did not both run"
    );
    assert!(node.written(0, CLIENT).is_empty(), "a reset client is answered nothing");
}

/// A `SET` and the peer's end of stream reach the cell in one completion
/// pass, before PARSE runs. The command the client sent before its FIN is
/// run and answered, then the connection closes (ADR-0196 A1). At HEAD the
/// end of stream marks the slot closing in the same pass, so PARSE
/// releases the entry unparsed and RESPOND sends nothing.
#[test]
#[ignore = "an end of stream reaped beside its connection's last bytes closes the slot first, so \
            PARSE releases the bytes unparsed and nothing is answered; red until the end of \
            stream keeps the parser, answers what was read before it, and closes after the replies"]
fn half_close_answers_replies() {
    let mut node = Node::new(1);
    node.accept(0, CLIENT);
    node.accept(0, READER);
    node.send(0, CLIENT, &frame(&[b"SET", b"half", b"closed"]));
    node.eof(0, CLIENT);
    node.rounds(4);
    assert!(node.closed(0).contains(&CLIENT), "engagement: the half-closed client was closed");
    assert_eq!(
        node.written(0, CLIENT),
        b"+OK\r\n",
        "the SET read before the end of stream was not answered"
    );
    assert_eq!(read_back(&mut node, 0, b"half"), b"$6\r\nclosed\r\n", "the SET did not run");
}

/// A client's `SET` is reaped while another client's `DEBUG SLEEP` stalls
/// the cell, so its inbox entry waits for the stall's end; the client is
/// reset and its `Closed` dispatched during the stall. The bytes it handed
/// over still run when the stall ends (ADR-0196 D2). At HEAD `Closed`
/// removes the connection, and the entry meets a vacant slot at PARSE.
#[test]
#[ignore = "a stalled cell keeps a connection's received bytes in its inbox past the connection's \
            Closed, and PARSE then releases them at the vacant slot; red until a draining slot \
            keeps its parser and parses the entries reaped before its close"]
fn a_stalled_entry_runs_after_its_connection_closed() {
    const SLEEPER: RawFd = 62;
    let mut node = Node::new(1);
    node.accept(0, CLIENT);
    node.accept(0, SLEEPER);
    node.accept(0, READER);
    node.send(0, SLEEPER, &frame(&[b"DEBUG", b"SLEEP", b"1"]));
    node.rounds(1);
    let stalled = node.cells[0].plane.shared.stall_until.get();
    assert!(stalled > node.clock.now(), "engagement: DEBUG SLEEP stalls the cell");
    node.send(0, CLIENT, &frame(&[b"SET", b"stalled", b"ran"]));
    node.rounds(1);
    assert_eq!(node.cells[0].plane.inbox.len(), 1, "engagement: the SET waits in the inbox");
    node.reset(0, CLIENT);
    node.rounds(2);
    assert!(node.closed(0).contains(&CLIENT), "engagement: the Closed came during the stall");
    assert!(node.cells[0].plane.shared.stall_until.get() > node.clock.now(), "still stalled");
    node.clock.advance(Nanos::from_secs(2));
    node.rounds(2);
    assert_eq!(
        read_back(&mut node, 0, b"stalled"),
        b"$3\r\nran\r\n",
        "the SET reaped before the reset did not run after the stall"
    );
}

// ---- PARSE's own replies leave in the order the connection owes them ----

/// A frame whose bulk length is not a number: a protocol error.
const MALFORMED: &[u8] = b"*1\r\n$x\r\n";
/// The protocol error's reply, Redis's text, after which the slot closes.
const PROTOCOL_ERROR: &str = "-ERR Protocol error: invalid bulk length\r\n";

/// A reading of the client's slot on `cell`.
fn client_slot<R>(node: &Node, cell: usize, read: impl Fn(&Conn) -> R) -> R {
    let mut seen = None;
    node.cells[cell].plane.shared.conns.borrow_mut().for_each_mut(|conn| {
        if conn.fd == CLIENT {
            seen = Some(read(conn));
        }
    });
    seen.expect("the client's slot is occupied")
}

/// That PARSE met the protocol error while the client's pump owed a reply:
/// the slot is marked to close after its flush and its pump is still active.
fn assert_error_met_owed(node: &Node, cell: usize) {
    assert!(client_slot(node, cell, |conn| conn.close_after_flush), "engagement: the error parsed");
    assert!(
        client_slot(node, cell, |conn| !conn.pump_parked),
        "engagement: the pump owed a reply when PARSE met the error"
    );
}

/// The client's bytes on `cell` as text, for an order the message prints.
fn written_text(node: &Node, cell: usize) -> String {
    String::from_utf8_lossy(&node.written(cell, CLIENT)).into_owned()
}

/// A `SET` of a key cell 1 owns, then a malformed frame, in one buffer;
/// cell 1 is frozen while PARSE runs, so the `SET`'s reply is owed when PARSE
/// meets the error. The client reads the `+OK`, then the error, then the
/// close. At HEAD PARSE writes the error into the output at once, so it
/// leaves first.
#[test]
#[ignore = "PARSE writes a protocol error into the connection's output at once, ahead of the \
            replies its pump still owes; red until PARSE queues its own replies behind them"]
fn protocol_error_answers_after_deferred() {
    let mut node = Node::new(2);
    node.accept(0, CLIENT);
    let key = node.key_on(1, "owed");
    let mut bytes = frame(&[b"SET", &key, b"v"]);
    bytes.extend_from_slice(MALFORMED);
    node.freeze(1, true);
    node.send(0, CLIENT, &bytes);
    node.rounds(2);
    assert_error_met_owed(&node, 0);
    node.freeze(1, false);
    node.rounds(8);
    assert!(node.closed(0).contains(&CLIENT), "the protocol error closes the connection");
    let owed = format!("+OK\r\n{PROTOCOL_ERROR}");
    assert_eq!(written_text(&node, 0), owed, "reply-order: the error left before the owed +OK");
}

/// A `SET` of a key cell 1 owns while cell 1 holds its reply, then, in a
/// later buffer, a malformed frame: the held `+OK` once cell 1 answers, then
/// the error, then the close. At HEAD the error leaves while the `+OK` is
/// held.
#[test]
#[ignore = "PARSE writes a protocol error into the connection's output at once, ahead of the \
            replies its pump still owes; red until PARSE queues its own replies behind them"]
fn protocol_error_in_a_later_buffer_answers_after_a_held_reply() {
    let mut node = Node::new(2);
    node.accept(0, CLIENT);
    let key = node.key_on(1, "held");
    node.freeze(1, true);
    node.send(0, CLIENT, &frame(&[b"SET", &key, b"v"]));
    node.rounds(2);
    assert!(node.written(0, CLIENT).is_empty(), "engagement: cell 1 holds the SET's reply");
    node.send(0, CLIENT, MALFORMED);
    node.rounds(2);
    assert_error_met_owed(&node, 0);
    node.freeze(1, false);
    node.rounds(8);
    assert!(node.closed(0).contains(&CLIENT), "the protocol error closes the connection");
    let owed = format!("+OK\r\n{PROTOCOL_ERROR}");
    assert_eq!(written_text(&node, 0), owed, "reply-order: the error left before the held +OK");
}

/// Control: a malformed frame with nothing owed is answered at once, then
/// the connection closes.
#[test]
fn protocol_error_with_nothing_owed_answers_at_once() {
    let mut node = Node::new(2);
    node.accept(0, CLIENT);
    node.send(0, CLIENT, MALFORMED);
    node.rounds(1);
    assert!(client_slot(&node, 0, |conn| conn.close_after_flush), "engagement: the error parsed");
    assert!(client_slot(&node, 0, |conn| conn.pump_parked), "engagement: nothing was owed");
    node.rounds(4);
    assert!(node.closed(0).contains(&CLIENT), "the protocol error closes the connection");
    assert_eq!(written_text(&node, 0), PROTOCOL_ERROR, "the error alone, then the close");
}

/// The error owed behind a `SET` cell 1 holds, both in one buffer, then the
/// client's socket is reset before either reply is sent: nothing is sent,
/// and the `SET` still runs. At HEAD the error is sent at once, before the
/// reset.
#[test]
#[ignore = "PARSE writes a protocol error into the connection's output at once, ahead of the \
            replies its pump still owes; red until PARSE queues its own replies behind them"]
fn a_reset_before_the_owed_replies_leave_sends_nothing() {
    let mut node = Node::new(2);
    node.accept(0, CLIENT);
    node.accept(0, READER);
    let key = node.key_on(1, "reset");
    let mut bytes = frame(&[b"SET", &key, b"v"]);
    bytes.extend_from_slice(MALFORMED);
    node.freeze(1, true);
    node.send(0, CLIENT, &bytes);
    node.rounds(2);
    assert_error_met_owed(&node, 0);
    node.reset(0, CLIENT);
    node.rounds(2);
    assert!(node.closed(0).contains(&CLIENT), "engagement: the reset's Closed was reaped");
    node.freeze(1, false);
    node.rounds(8);
    assert_eq!(read_back(&mut node, 0, &key), b"$1\r\nv\r\n", "the SET before the error ran");
    assert_eq!(written_text(&node, 0), "", "a client reset before its replies leave reads none");
}

/// A plane loading its dataset and a namespace-bound connection: `ECHO a`,
/// which runs while loading and rides the pump, then `GET k`, which is
/// refused while loading, in one buffer. The `ECHO`'s reply is owed when
/// PARSE refuses the `GET`, so the client reads `a`, then the refusal. At
/// HEAD PARSE writes the refusal into the output at once, so it leaves
/// first. The pump runs the `ECHO` inside PARSE's pass, so the slot's pump
/// is idle again by the pass's end; engagement reads the route's input
/// instead: the slot bound to a namespace, which defers every command, and
/// the plane still loading after the pass.
#[test]
#[ignore = "PARSE writes a loading refusal into the connection's output at once, ahead of the \
            replies its pump still owes; red until PARSE queues its own replies behind them"]
fn loading_answers_after_deferred() {
    let namespace = inf_store::NsSpec {
        id: NsId(inf_store::FIRST_NAMED_NS_ID),
        name: b"loading".to_vec(),
        mode: NsMode::Memory,
        fsync: None,
        policy: None,
        maxmemory: None,
        tier: None,
    };
    let mut node = Node::with_namespaces(1, &[namespace]);
    node.accept(0, CLIENT);
    node.send(0, CLIENT, &frame(&[b"INF.NS", b"USE", b"loading"]));
    node.rounds(4);
    assert_eq!(written_text(&node, 0), "+OK\r\n", "engagement: the client's USE was answered");
    assert!(
        client_slot(&node, 0, |conn| conn.cx.ns.named().is_some()),
        "engagement: the slot is namespace-bound, so every command it sends defers"
    );
    node.cells[0].plane.shared.loading.set(true);
    let mut bytes = frame(&[b"ECHO", b"a"]);
    bytes.extend(frame(&[b"GET", b"k"]));
    node.send(0, CLIENT, &bytes);
    node.rounds(4);
    assert!(node.cells[0].plane.shared.loading.get(), "engagement: the plane is still loading");
    let owed = "+OK\r\n$1\r\na\r\n-LOADING Redis is loading the dataset in memory\r\n";
    assert_eq!(written_text(&node, 0), owed, "reply-order: the refusal left before the owed reply");
}
