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
/// still runs, unanswered (ADR-0123 A2): the key reads 2. A `Closed` that
/// dropped the connection with its queue would never write the key.
#[test]
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
/// run and answered, then the connection closes (ADR-0196 A1). An end of
/// stream that closed the slot in the same pass would leave PARSE to
/// release the entry unparsed and RESPOND to send nothing.
#[test]
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
/// over still run when the stall ends (ADR-0196 D2). A `Closed` that
/// removed the connection would leave the entry a vacant slot at PARSE.
#[test]
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

/// With the loop's remote-first ordering on, a pump woken by `Closed` runs
/// before that iteration's PARSE. The client's reply fails to send (the peer
/// is gone with its receive still armed), so its slot closes with its parser,
/// stamped at that reap; PARSE takes the inbox once more; then a `SET` the
/// receive returned and the `Closed` are reaped together. `Closed` stamps the
/// parser anew (ADR-0196 D2), so the pump's run-out, polled before PARSE,
/// waits for PARSE's take and the `SET` runs. A `Closed` that kept the older
/// stamp would end the parser at once and leave PARSE to release the entry.
#[test]
fn closed_restamps_the_parser_a_remote_first_pump_waits_on() {
    let mut node = Node::remote_first(1);
    node.accept(0, CLIENT);
    node.accept(0, READER);
    node.fail_sends(0, CLIENT);
    node.hold_closes(0, CLIENT);
    node.send(0, CLIENT, &frame(&[b"PING"]));
    node.rounds(3);
    let closing = |node: &Node| {
        let mut closing = 0;
        node.cells[0].plane.shared.conns.borrow_mut().for_each_mut(|conn| {
            closing +=
                usize::from(matches!(conn.slot, ConnSlot::Closing { by: ClosedBy::Peer { .. } }));
        });
        closing
    };
    assert_eq!(closing(&node), 1, "engagement: the failed send closes the slot with its parser");
    let waits_before = node.cells[0].plane.shared.node.reader_take_waits.get();
    node.send(0, CLIENT, &frame(&[b"SET", b"restamped", b"ran"]));
    node.deliver_closed(0, CLIENT);
    node.rounds(4);
    assert!(node.closed(0).contains(&CLIENT), "engagement: the held Closed was reaped");
    let waits_after = node.cells[0].plane.shared.node.reader_take_waits.get();
    assert_eq!(
        read_back(&mut node, 0, b"restamped"),
        b"$3\r\nran\r\n",
        "closed-stamp-kept: the SET reaped with the Closed was released unparsed"
    );
    assert!(waits_after > waits_before, "the run-out polled before PARSE waited for its take");
}

// ---- PARSE's own replies leave in the order the connection owes them ----

/// A frame whose bulk length is not a number: a protocol error.
const MALFORMED: &[u8] = b"*1\r\n$x\r\n";
/// The protocol error's reply, Redis's text, after which the slot closes.
const PROTOCOL_ERROR: &str = "-ERR Protocol error: invalid bulk length\r\n";

/// A reading of the client's slot on `cell`, while it holds its descriptor.
fn client_slot<R>(node: &Node, cell: usize, read: impl Fn(&Conn) -> R) -> R {
    fd_slot(node, cell, CLIENT, read)
}

/// A reading of `fd`'s slot on `cell`, while it holds its descriptor.
fn fd_slot<R>(node: &Node, cell: usize, fd: RawFd, read: impl Fn(&Conn) -> R) -> R {
    let mut seen = None;
    node.cells[cell].plane.shared.conns.borrow_mut().for_each_mut(|conn| {
        let held = match &conn.slot {
            ConnSlot::Active { link, .. } | ConnSlot::Flushing { link, .. } => Some(link.fd),
            ConnSlot::Closing { .. } | ConnSlot::Draining { .. } => None,
        };
        if held == Some(fd) {
            seen = Some(read(conn));
        }
    });
    seen.expect("the slot holds its descriptor")
}

/// Whether the plane's mark moved the slot to a flush.
fn marked(conn: &Conn) -> bool {
    matches!(conn.slot, ConnSlot::Flushing { .. })
}

/// That PARSE met the protocol error while the client's pump owed a reply:
/// the slot is marked to close after its flush and its pump is still active.
fn assert_error_met_owed(node: &Node, cell: usize) {
    assert!(client_slot(node, cell, marked), "engagement: the error parsed");
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
/// close. A PARSE that wrote the error into the output at once would send
/// it first.
#[test]
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
/// the error, then the close. An error written into the output at once
/// would leave while the `+OK` is held.
#[test]
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
    assert!(client_slot(&node, 0, marked), "engagement: the error parsed");
    assert!(client_slot(&node, 0, |conn| conn.pump_parked), "engagement: nothing was owed");
    node.rounds(4);
    assert!(node.closed(0).contains(&CLIENT), "the protocol error closes the connection");
    assert_eq!(written_text(&node, 0), PROTOCOL_ERROR, "the error alone, then the close");
}

/// The error owed behind a `SET` cell 1 holds, both in one buffer, then the
/// client's socket is reset before either reply is sent: nothing is sent,
/// and the `SET` still runs. An error written into the output at once
/// would be sent before the reset.
#[test]
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
/// PARSE refuses the `GET`, so the client reads `a`, then the refusal. A
/// refusal written into the output at once would leave first. The pump
/// runs the `ECHO` inside PARSE's pass, so the slot's pump is idle again by
/// the pass's end; engagement reads the route's input
/// instead: the slot bound to a namespace, which defers every command, and
/// the plane still loading after the pass.
#[test]
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

// ---- a half-closed or marked slot's count and receive (ADR-0196 A1.2, A1.3) ----

/// A client of the subscriber's cell that publishes to it.
const PUBLISHER: RawFd = 63;
/// A second subscriber, the marked control.
const MARKED: RawFd = 64;

/// `PUBLISH channel payload` from the publisher, run until its reply
/// arrives: the receiver count's frame.
fn publish(node: &mut Node, channel: &[u8], payload: &[u8]) -> Vec<u8> {
    let before = node.written(0, PUBLISHER).len();
    node.send(0, PUBLISHER, &frame(&[b"PUBLISH", channel, payload]));
    node.rounds(4);
    node.written(0, PUBLISHER)[before..].to_vec()
}

/// The slot's state and, flushing, its stream.
fn stream_of(conn: &Conn) -> &'static str {
    match &conn.slot {
        ConnSlot::Active { .. } => "active",
        ConnSlot::Flushing { stream: PeerStream::Open, .. } => "open",
        ConnSlot::Flushing { stream: PeerStream::Ended { by: ClosedBy::Peer { .. } }, .. } => {
            "peer"
        }
        ConnSlot::Flushing { stream: PeerStream::Ended { by: ClosedBy::Plane }, .. } => "ended",
        ConnSlot::Closing { .. } | ConnSlot::Draining { .. } => "closed",
    }
}

/// The bytes staged in the slot's `out`.
fn out_len(conn: &Conn) -> usize {
    match &conn.slot {
        ConnSlot::Active { link, .. } | ConnSlot::Flushing { link, .. } => link.out.len(),
        ConnSlot::Closing { .. } | ConnSlot::Draining { .. } => 0,
    }
}

/// A subscriber counts as a receiver until the server reads its end of
/// stream (ADR-0196 A1.2). (a) A subscriber's end of stream is reaped while
/// a send of it is in flight, and PARSE's next take lets its run-out end the
/// parser (`Flushing` `Ended`): a `PUBLISH` then counts it 0, and nothing is
/// written to it. (b) A subscriber marked by `QUIT` with its `+OK` unsent is
/// counted 1, the control, then 0 once its end of stream is read. Each
/// closes once its sends complete.
#[test]
fn half_closed_subscriber_not_counted() {
    let mut node = Node::new(1);
    for fd in [CLIENT, MARKED] {
        node.accept(0, fd);
        node.send(0, fd, &frame(&[b"SUBSCRIBE", b"ch"]));
    }
    node.accept(0, PUBLISHER);
    node.rounds(4);
    for fd in [CLIENT, MARKED] {
        node.hold_sends(0, fd, true);
    }
    assert_eq!(publish(&mut node, b"ch", b"m1"), b":2\r\n", "engagement: both subscribers count");
    // (a) the end of stream, then the run-out.
    node.eof(0, CLIENT);
    node.rounds(2);
    let stream = client_slot(&node, 0, stream_of);
    assert_eq!(stream, "ended", "VACUOUS run-out-unreached: the slot is {stream}, not ended");
    let staged = client_slot(&node, 0, out_len);
    // (b) the control: marked, its stream still open.
    node.send(0, MARKED, &frame(&[b"QUIT"]));
    node.rounds(2);
    assert_eq!(fd_slot(&node, 0, MARKED, stream_of), "open", "engagement: the QUIT marked it");
    let marked_staged = fd_slot(&node, 0, MARKED, out_len);
    assert_eq!(
        publish(&mut node, b"ch", b"m2"),
        b":1\r\n",
        "count-after-eof: the half-closed subscriber counted, or the marked one did not"
    );
    assert_eq!(client_slot(&node, 0, out_len), staged, "a message written after the end of stream");
    assert_eq!(fd_slot(&node, 0, MARKED, out_len), marked_staged, "a message written after QUIT");
    node.eof(0, MARKED);
    node.rounds(2);
    assert_eq!(fd_slot(&node, 0, MARKED, stream_of), "ended", "the marked stream ended");
    assert_eq!(
        publish(&mut node, b"ch", b"m3"),
        b":0\r\n",
        "count-after-eof: the marked subscriber counted after its end of stream"
    );
    for fd in [CLIENT, MARKED] {
        node.hold_sends(0, fd, false);
    }
    node.rounds(6);
    for fd in [CLIENT, MARKED] {
        assert!(node.closed(0).contains(&fd), "a flushed subscriber closes");
        let text = String::from_utf8_lossy(&node.written(0, fd)).into_owned();
        assert!(text.contains("m1") && !text.contains("m2") && !text.contains("m3"), "{text:?}");
    }
}

/// The mark of an active slot whose receive is disarmed, as the high water
/// leaves it, pushes one `RecvArm` (ADR-0196 A1.3), so the marked slot reads
/// its end of stream; the end of stream then ends its stream.
#[test]
fn mark_rearms_a_disarmed_receive() {
    let mut node = Node::new(1);
    node.accept(0, CLIENT);
    let arms = node.recv_arms(0, CLIENT);
    node.cells[0].plane.shared.conns.borrow_mut().for_each_mut(|conn| {
        if let ConnSlot::Active { recv, .. } = &mut conn.slot {
            *recv = RecvArm::Disarmed;
        }
    });
    node.hold_sends(0, CLIENT, true);
    node.send(0, CLIENT, &frame(&[b"QUIT"]));
    node.rounds(2);
    assert_eq!(client_slot(&node, 0, stream_of), "open", "engagement: the QUIT marked the slot");
    assert_eq!(
        node.recv_arms(0, CLIENT),
        arms + 1,
        "no-rearm-at-mark: the mark left a disarmed receive unarmed"
    );
    node.eof(0, CLIENT);
    node.rounds(2);
    assert_eq!(client_slot(&node, 0, stream_of), "ended", "the end of stream ends the stream");
    node.hold_sends(0, CLIENT, false);
    node.rounds(4);
    assert_eq!(written_text(&node, 0), "+OK\r\n", "the QUIT's +OK, then the close");
    assert!(node.closed(0).contains(&CLIENT), "the marked slot closes once its +OK is sent");
}

/// A RESP3 client subscribed to a channel cell 1 owns sends `PUBLISH` to it
/// and a malformed frame in one buffer. The publisher's own message rides
/// its publish's reply (ADR-0101 D4), though the slot is flushing when it
/// arrives, then the protocol error leaves after both, then the close.
#[test]
fn own_frame_rides_reply_after_protocol_error() {
    let mut node = Node::new(2);
    node.accept(0, CLIENT);
    let channel = node.key_on(1, "own");
    node.send(0, CLIENT, &frame(&[b"HELLO", b"3"]));
    node.send(0, CLIENT, &frame(&[b"SUBSCRIBE", &channel]));
    node.rounds(8);
    let confirmed = written_text(&node, 0);
    assert!(confirmed.ends_with(":1\r\n"), "engagement: the subscription confirmed: {confirmed:?}");
    let mut bytes = frame(&[b"PUBLISH", &channel, b"m"]);
    bytes.extend_from_slice(MALFORMED);
    node.send(0, CLIENT, &bytes);
    node.rounds(12);
    assert!(node.closed(0).contains(&CLIENT), "the protocol error closes the connection");
    let name = String::from_utf8_lossy(&channel).into_owned();
    let length = name.len();
    let message = format!(">3\r\n$7\r\nmessage\r\n${length}\r\n{name}\r\n$1\r\nm\r\n");
    let tail = written_text(&node, 0)[confirmed.len()..].to_string();
    assert!(tail.contains(&message), "own-frame-dropped: the own message is missing: {tail:?}");
    let owed = format!(":1\r\n{message}{PROTOCOL_ERROR}");
    assert_eq!(tail, owed, "reply-order: the count, the own message, then the error");
}

/// An inline `QUIT` marks the slot at once, so a `SET` in the next buffer,
/// reaped while the `+OK` is still unsent, is released unparsed (Redis
/// discards what follows `QUIT`). A mark kept as a flag beside a live parser
/// would run it.
#[test]
fn a_set_after_quit_does_not_run() {
    let mut node = Node::new(1);
    node.accept(0, CLIENT);
    node.accept(0, READER);
    node.hold_sends(0, CLIENT, true);
    node.send(0, CLIENT, &frame(&[b"QUIT"]));
    node.rounds(2);
    node.send(0, CLIENT, &frame(&[b"SET", b"after", b"quit"]));
    node.rounds(2);
    assert!(node.written(0, CLIENT).is_empty(), "engagement: the +OK is still unsent");
    assert_eq!(
        read_back(&mut node, 0, b"after"),
        b"$-1\r\n",
        "post-quit-ran: a SET after QUIT ran"
    );
    node.hold_sends(0, CLIENT, false);
    node.rounds(4);
    assert!(node.closed(0).contains(&CLIENT), "the QUIT closes the connection");
    assert_eq!(written_text(&node, 0), "+OK\r\n", "the QUIT's +OK alone, then the close");
}

/// A connection whose pump is parked is closed by a reset: `Closed` wakes
/// the pump, so its draining slot retires and frees its `conn` task, and
/// the cell holds neither once quiet (O1). A pump left parked at `Closed`
/// keeps its task and its slot for good, and after `C` such closes every
/// accept is refused.
#[test]
fn a_closed_connection_frees_its_conn_task() {
    let mut node = Node::new(1);
    node.accept(0, CLIENT);
    let owners = |node: &Node| node.cells[0].plane.conn_owners(node.cells[0].reactor.executor());
    assert_eq!(owners(&node).held, 1, "engagement: the connection's pump is alive");
    assert!(client_slot(&node, 0, |conn| conn.pump_parked), "engagement: its pump is parked");
    node.reset(0, CLIENT);
    node.rounds(4);
    assert!(node.closed(0).contains(&CLIENT), "engagement: the Closed was reaped");
    let after = owners(&node);
    assert_eq!(
        (after.held, after.slots),
        (0, 0),
        "conn-task-kept: a closed connection's task or slot outlived its close"
    );
}
