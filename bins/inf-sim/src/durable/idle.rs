//! `timeout` judges a connection only when the server owes it nothing
//! (ADR-0193 D1): a client whose command is held at a frozen peer, on a held
//! fsync or by a stalled cell is answered first, and its idle clock runs
//! from its reply (a reply sent is activity) and from the stall's end at the
//! earliest (D2). Each test boots a quiet durable node, sets `timeout` over
//! the wire, and reads its clients' bytes and the server's closes from the
//! net with the node's virtual clock: no code shared with the sweep.
//!
//! The window a close must fall in is the idle oracle's: from the step the
//! client reads its last reply, `[timeout - step_ns_max - 1 ms, timeout +
//! 4 * step_ns_max + 1 ms]`. A test whose hold outlasts `timeout` checks its
//! engagement, the hold outlasted `timeout` while the client was owed a
//! reply (else `VACUOUS hold-under-timeout`), and reads the sweep's
//! `idle_sweep_deferrals_cell` above 0 (the instrument's liveness: the sweep
//! visited the held client past its clock and kept it). A hold under
//! `timeout` is a control: no deferral, and the close in the window.

use std::path::Path;

use inf_foundation::time::{Clock, Nanos};
use inf_runtime::RawFd;
use inf_store::SlotRouter;

use super::park::{Quiet, STEPS_MAX};
use super::{DurableScenario, MiniClient, encode};
use crate::harness::STEP_NS_FLOOR;
use crate::resp::reply_len;

/// The `timeout` the tests judge under, 1 s.
const TIMEOUT: Nanos = Nanos(1_000_000_000);

/// Where a watched client's idle window opens.
#[derive(Clone, Copy)]
enum IdleFrom {
    /// The step that framed its last reply: the server stamped that reply's
    /// send at most one step before it.
    LastReply,
    /// A step the test read, at most `slack` after the server's clock for
    /// the client restarted.
    Read { at: Nanos, slack: Nanos },
}

/// A client the test watches over the net: the replies it read and when, and
/// when the server closed it.
struct Watched {
    cell: usize,
    fd: RawFd,
    rx: Vec<u8>,
    /// Replies the client is owed: every command it sent.
    owed: usize,
    replies: Vec<Vec<u8>>,
    /// The virtual time of the step that framed its last reply.
    replied_at: Option<Nanos>,
    closed_at: Option<Nanos>,
    idle_from: IdleFrom,
}

impl Watched {
    fn connect(quiet: &mut Quiet, cell: usize) -> Watched {
        let fd = quiet.node.nets[cell].borrow_mut().connect();
        Watched {
            cell,
            fd,
            rx: Vec::new(),
            owed: 0,
            replies: Vec::new(),
            replied_at: None,
            closed_at: None,
            idle_from: IdleFrom::LastReply,
        }
    }

    fn send(&mut self, quiet: &mut Quiet, argv: &[&[u8]]) {
        quiet.node.nets[self.cell].borrow_mut().client_send(self.fd, &encode(argv));
        self.owed += 1;
    }

    /// Reads what the step delivered. A close while a reply is owed is the
    /// oracle's red, `reply-owed-close`.
    fn observe(&mut self, quiet: &Quiet, who: &str) {
        let now = quiet.clock.now();
        let mut net = quiet.node.nets[self.cell].borrow_mut();
        self.rx.extend_from_slice(&net.client_recv(self.fd));
        while let Some(n) = reply_len(&self.rx) {
            self.replies.push(self.rx.drain(..n).collect());
            self.replied_at = Some(now);
        }
        if self.closed_at.is_none() && net.closed(self.fd) {
            self.closed_at = Some(now);
            assert!(
                self.replies.len() >= self.owed,
                "reply-owed-close: the server closed {who} at {} ms with {} of its {} replies \
                 read",
                now.as_millis(),
                self.replies.len(),
                self.owed
            );
        }
    }

    fn answered(&self) -> bool {
        self.replies.len() >= self.owed
    }

    /// The instant its idle window opens and how far before it the server's
    /// own restart may lie.
    fn window_from(&self, step_ns_max: u64) -> (Nanos, Nanos) {
        match self.idle_from {
            IdleFrom::LastReply => (self.replied_at.expect("answered"), Nanos(step_ns_max)),
            IdleFrom::Read { at, slack } => (at, slack),
        }
    }
}

/// One scheduler step, then each watched client's reading.
fn step(quiet: &mut Quiet, watched: &mut [(&str, &mut Watched)]) {
    quiet.step();
    for (who, client) in watched.iter_mut() {
        client.observe(quiet, who);
    }
}

/// Steps until the virtual clock reaches `until`.
fn step_until(quiet: &mut Quiet, until: Nanos, watched: &mut [(&str, &mut Watched)]) {
    for _ in 0..STEPS_MAX * 4 {
        if quiet.clock.now() >= until {
            return;
        }
        step(quiet, watched);
    }
    panic!("the clock did not reach {} ms", until.as_millis());
}

/// Steps until every watched client has its replies.
fn step_until_answered(quiet: &mut Quiet, watched: &mut [(&str, &mut Watched)]) {
    for _ in 0..STEPS_MAX * 4 {
        if watched.iter().all(|(_, client)| client.answered()) {
            return;
        }
        step(quiet, watched);
    }
    let left: Vec<&str> =
        watched.iter().filter(|(_, client)| !client.answered()).map(|(who, _)| *who).collect();
    panic!("unanswered after {} steps: {left:?}", STEPS_MAX * 4);
}

/// Steps until every watched client is closed, then judges each close
/// against the idle window after its idle clock last restarted.
fn judge_reaped(quiet: &mut Quiet, watched: &mut [(&str, &mut Watched)]) {
    let step_ns_max = quiet.step_ns_max;
    let upper = Nanos(TIMEOUT.0 + 4 * step_ns_max + 1_000_000);
    let last = watched.iter().map(|(_, client)| client.window_from(step_ns_max).0).max();
    let until = Nanos(last.expect("a watched client").0 + upper.0 + 1);
    while quiet.clock.now() < until && watched.iter().any(|(_, client)| client.closed_at.is_none())
    {
        step(quiet, watched);
    }
    for (who, client) in watched.iter() {
        let (from, slack) = client.window_from(step_ns_max);
        let lower = Nanos(TIMEOUT.0 - slack.0 - 1_000_000);
        let Some(closed) = client.closed_at else {
            panic!(
                "reaped-after-window: {who} was still open {} ms after its idle clock restarted",
                upper.0 / 1_000_000
            );
        };
        let idle = Nanos(closed.0.saturating_sub(from.0));
        assert!(
            idle >= lower,
            "reaped-before-window: {who} closed {} us after its idle clock restarted, under {} us",
            idle.0 / 1_000,
            lower.0 / 1_000
        );
        assert!(
            idle <= upper,
            "reaped-after-window: {who} closed {} us after its idle clock restarted, past {} us",
            idle.0 / 1_000,
            upper.0 / 1_000
        );
    }
}

/// `CONFIG SET timeout <secs>` on every cell (the command scatters), from a
/// client that then stays silent.
fn set_timeout(quiet: &mut Quiet, secs: &[u8]) {
    let mut setup = MiniClient::connect(&mut quiet.node, 0);
    let reply = quiet.call(&mut setup, &[b"CONFIG", b"SET", b"timeout", secs], "CONFIG SET");
    assert_eq!(reply, b"+OK\r\n", "CONFIG SET timeout");
}

/// A key the given cell of a `cells`-cell node owns.
fn key_on(cells: u16, owner: u16) -> Vec<u8> {
    let router = SlotRouter::new_contiguous(cells);
    (0..10_000)
        .map(|n| format!("idle:{n}").into_bytes())
        .find(|key| router.cell_of(SlotRouter::slot_of(key)).0 == owner)
        .expect("every owner is reachable")
}

/// The engagement clause: the hold outlasted `timeout` while `who` was owed
/// its reply.
fn assert_hold_engaged(held: Nanos, who: &Watched, name: &str) {
    assert!(
        held > TIMEOUT && !who.answered(),
        "VACUOUS hold-under-timeout: {name} was held {} ms, answered {}",
        held.as_millis(),
        who.answered()
    );
}

/// `INFO stats` `idle_sweep_deferrals_cell` of `cell` (ADR-0193 D4), read by
/// a fresh client once the watched ones are closed: the sweep's visits that
/// found a connection the server owes past `timeout` by its clock. A field
/// the section does not name is a failure, never a 0.
fn deferrals(quiet: &mut Quiet, cell: usize) -> u64 {
    let mut probe = MiniClient::connect(&mut quiet.node, cell);
    let stats = quiet.call(&mut probe, &[b"INFO", b"stats"], "INFO stats");
    String::from_utf8_lossy(&stats)
        .lines()
        .find_map(|line| line.strip_prefix("idle_sweep_deferrals_cell:"))
        .and_then(|value| value.trim().parse().ok())
        .expect("INFO stats names idle_sweep_deferrals_cell")
}

/// The liveness clause of a hold past `timeout`: the sweep counted the held
/// client as owed past its clock at least once.
fn assert_deferred(deferred: u64, name: &str) {
    assert!(deferred > 0, "deferrals-unread: the sweep counted no deferral while {name} was held");
}

/// How long the owner cell stays frozen once the `GET` is sent.
#[derive(Clone, Copy)]
enum PeerHold {
    /// Not frozen: the control, answered within a few steps.
    Unfrozen,
    /// One scheduler step under `timeout`: the reply comes as the request's
    /// age nears `timeout`.
    StepUnderTimeout,
    /// This many timeouts.
    Timeouts(u64),
}

/// A `GET` of a key cell 1 owns while cell 1 is frozen for `hold`: the
/// client waits on a reply the server owes it, so no sweep closes it; it
/// reads the reply once cell 1 runs again, and is reaped a timeout after it.
/// Returns the deferrals cell 0's sweep counted.
fn held_at_a_frozen_peer(hold: PeerHold) -> u64 {
    let mut quiet = Quiet::boot(2, DurableScenario::SPIN_ITERS, 0);
    set_timeout(&mut quiet, b"1");
    let held = match hold {
        PeerHold::Unfrozen => Nanos(0),
        PeerHold::StepUnderTimeout => Nanos(TIMEOUT.0 - quiet.step_ns_max),
        PeerHold::Timeouts(n) => Nanos(n * TIMEOUT.0),
    };
    let mut client = Watched::connect(&mut quiet, 0);
    quiet.node.frozen = Some((1, u64::MAX));
    let sent = quiet.clock.now();
    client.send(&mut quiet, &[b"GET", &key_on(2, 1)]);
    step_until(&mut quiet, Nanos(sent.0 + held.0), &mut [("the held client", &mut client)]);
    if held > TIMEOUT {
        assert_hold_engaged(Nanos(quiet.clock.now().0 - sent.0), &client, "the held client");
    }
    quiet.node.frozen = None;
    step_until_answered(&mut quiet, &mut [("the held client", &mut client)]);
    assert_eq!(client.replies, [b"$-1\r\n".to_vec()], "the held GET's reply");
    judge_reaped(&mut quiet, &mut [("the held client", &mut client)]);
    deferrals(&mut quiet, 0)
}

/// Cell 1 frozen for twice the timeout.
#[test]
fn timeout_spares_a_command_held_at_a_frozen_peer() {
    let deferred = held_at_a_frozen_peer(PeerHold::Timeouts(2));
    assert_deferred(deferred, "the held client");
}

/// Cell 1 frozen for ten timeouts: every MAINTAIN of the hold finds the
/// client owed and keeps it.
#[test]
fn timeout_spares_a_command_held_ten_timeouts_at_a_frozen_peer() {
    let deferred = held_at_a_frozen_peer(PeerHold::Timeouts(10));
    assert_deferred(deferred, "the held client");
}

/// Cell 1 frozen for one step under the timeout: the client's clock restarts
/// at its reply's send, so it is reaped a timeout after the reply, not a
/// timeout after its request (`reaped-before-window`); a hold under the
/// timeout is no deferral.
#[test]
fn timeout_restarts_at_a_reply_held_a_step_under_the_timeout() {
    let deferred = held_at_a_frozen_peer(PeerHold::StepUnderTimeout);
    assert_eq!(deferred, 0, "deferral-in-control: a hold under the timeout was counted as owed");
}

/// The control: cell 1 never frozen. The reply comes within steps, the
/// client is reaped in the window after it, and the sweep defers nothing.
#[test]
fn timeout_reaps_a_client_its_peer_answered_at_once() {
    let deferred = held_at_a_frozen_peer(PeerHold::Unfrozen);
    assert_eq!(deferred, 0, "deferral-in-control: an answered client was counted as owed");
}

/// Holds every barrier on every file.
fn any_file(_: &Path) -> bool {
    true
}

/// Steps until the device holds a write's barrier (a write with its linked
/// sync, or a write-through write: the barriers an `always` frame rides)
/// while `who` waits on its write, for at most half the timeout: the hold's
/// engagement on the device's side, else `VACUOUS frame-barrier-unheld`.
fn step_until_frame_held(quiet: &mut Quiet, who: &mut Watched, name: &str) {
    let until = Nanos(quiet.clock.now().0 + TIMEOUT.0 / 2);
    while quiet.node.driver(who.cell).held_barriers().frames() == 0 && quiet.clock.now() < until {
        step(quiet, &mut [(name, &mut *who)]);
    }
    let held = quiet.node.driver(who.cell).held_barriers();
    assert!(
        held.frames() > 0,
        "VACUOUS frame-barrier-unheld: no frame barrier was held while {name} waited ({held:?})"
    );
}

/// A 1-cell node under `timeout <secs>` with an `always` namespace, and a
/// writer that has `USE`d it: the setup client and the writer.
fn always_writer(secs: &[u8]) -> (Quiet, MiniClient, Watched) {
    let mut quiet = Quiet::boot(1, DurableScenario::SPIN_ITERS, 0);
    set_timeout(&mut quiet, secs);
    let mut setup = MiniClient::connect(&mut quiet.node, 0);
    let create: &[&[u8]] =
        &[b"INF.NS", b"CREATE", b"acks", b"MODE", b"durable", b"FSYNC", b"always"];
    assert_eq!(quiet.call(&mut setup, create, "the always CREATE"), b"+OK\r\n");
    let mut writer = Watched::connect(&mut quiet, 0);
    writer.send(&mut quiet, &[b"INF.NS", b"USE", b"acks"]);
    step_until_answered(&mut quiet, &mut [("the writer", &mut writer)]);
    (quiet, setup, writer)
}

/// A `SET` of an `always` namespace while the device holds every barrier for
/// twice the timeout, then 1,100 pipelined `SET`s behind it (the queue
/// passes its high water of 1,024 and the receive is disarmed): the client
/// is owed replies throughout, so no sweep closes it; once the hold lifts it
/// reads all 1,101 `+OK`s and is reaped a timeout after. Engagement: the
/// device held the write's frame barrier, else `VACUOUS
/// frame-barrier-unheld`; the hold outlasted the timeout; and the queue's
/// high water disarmed the receive, else `VACUOUS disarm-unengaged`.
#[test]
fn timeout_spares_an_always_write_held_on_its_fsync() {
    const PIPELINED: usize = 1_100;
    let (mut quiet, _setup, mut client) = always_writer(b"1");
    quiet.disk.hold_barriers(Some(any_file));
    let sent = quiet.clock.now();
    client.send(&mut quiet, &[b"SET", b"held", b"0"]);
    step_until_frame_held(&mut quiet, &mut client, "the writer");
    step_until(&mut quiet, Nanos(sent.0 + 2 * TIMEOUT.0), &mut [("the writer", &mut client)]);
    assert_hold_engaged(Nanos(quiet.clock.now().0 - sent.0), &client, "the writer");
    for n in 0..PIPELINED {
        let key = format!("held:{n}");
        client.send(&mut quiet, &[b"SET", key.as_bytes(), b"1"]);
    }
    let behind = quiet.clock.now();
    step_until(&mut quiet, Nanos(behind.0 + TIMEOUT.0 / 2), &mut [("the writer", &mut client)]);
    let disarmed = quiet.node.nets[0].borrow().recv_disarmed(client.fd);
    assert!(disarmed, "VACUOUS disarm-unengaged: the pipelined SETs left the receive armed");
    quiet.disk.hold_barriers(None);
    step_until_answered(&mut quiet, &mut [("the writer", &mut client)]);
    let oks = client.replies.iter().filter(|reply| reply.as_slice() == b"+OK\r\n").count();
    assert_eq!(oks, PIPELINED + 2, "the USE, the held SET and every pipelined SET answered +OK");
    judge_reaped(&mut quiet, &mut [("the writer", &mut client)]);
    assert_deferred(deferrals(&mut quiet, 0), "the writer");
}

/// `timeout 10`, an `always` `SET` held on the device's barriers, and
/// `timeout` lowered to 1 by another client 1.5 s into the hold, which
/// lifts 1 s later: the held writer is owed its reply, so the lowered
/// timeout does not judge it; it reads the `+OK` and is reaped a timeout
/// after. Engagement: the frame barrier was held, and the hold outlasted the
/// lowered timeout.
#[test]
fn timeout_lowered_while_a_write_is_held_judges_it_from_its_reply() {
    let (mut quiet, mut setup, mut client) = always_writer(b"10");
    quiet.disk.hold_barriers(Some(any_file));
    let sent = quiet.clock.now();
    client.send(&mut quiet, &[b"SET", b"held", b"0"]);
    step_until_frame_held(&mut quiet, &mut client, "the writer");
    step_until(&mut quiet, Nanos(sent.0 + 3 * TIMEOUT.0 / 2), &mut [("the writer", &mut client)]);
    let lowered = quiet.call(&mut setup, &[b"CONFIG", b"SET", b"timeout", b"1"], "the lowering");
    assert_eq!(lowered, b"+OK\r\n", "CONFIG SET timeout 1 while the write is held");
    step_until(&mut quiet, Nanos(sent.0 + 5 * TIMEOUT.0 / 2), &mut [("the writer", &mut client)]);
    assert_hold_engaged(Nanos(quiet.clock.now().0 - sent.0), &client, "the writer");
    quiet.disk.hold_barriers(None);
    step_until_answered(&mut quiet, &mut [("the writer", &mut client)]);
    assert_eq!(client.replies, [b"+OK\r\n".to_vec(), b"+OK\r\n".to_vec()], "the USE and the SET");
    judge_reaped(&mut quiet, &mut [("the writer", &mut client)]);
    assert_deferred(deferrals(&mut quiet, 0), "the writer");
}

/// `DEBUG SLEEP 3` under `timeout 1`. During the sleep a third client sends a
/// 160 KiB `SET`, more than the cell's pool of 128 buffers of 1 KiB, so the
/// inbox holds the pool; then a second client, answered 0.8 s before, sends
/// `PING`, which waits in the net as the kernel holds a paused receive. A
/// fourth client connected before the sleep sends nothing. Every request of
/// the stalled cell is answered after the stall, each client that sent one
/// is reaped a timeout after its reply, and the silent one a timeout after
/// the stall's end, not at it (ADR-0193 D2, D5). The stall's end lies at
/// most three steps before the issuer reads its `+OK` (the step that
/// reaches it, the send's submit, the read), so the silent client's window
/// opens there with three steps' slack. Engagement: the second client's
/// bytes are still in the net when the stall ends, else `VACUOUS
/// pool-not-dry`.
#[test]
fn timeout_spares_the_replies_of_a_stalled_cell() {
    const BIG_BYTES: usize = 160 * 1024;
    let mut quiet = Quiet::boot(1, DurableScenario::SPIN_ITERS, 0);
    set_timeout(&mut quiet, b"1");
    let mut second = Watched::connect(&mut quiet, 0);
    second.send(&mut quiet, &[b"PING"]);
    step_until_answered(&mut quiet, &mut [("the second client", &mut second)]);
    let answered = second.replied_at.expect("PONG");
    let mut silent = Watched::connect(&mut quiet, 0);
    let mut issuer = Watched::connect(&mut quiet, 0);
    issuer.send(&mut quiet, &[b"DEBUG", b"SLEEP", b"3"]);
    let mut third = Watched::connect(&mut quiet, 0);
    let mut clients = [
        ("the issuer", &mut issuer),
        ("the second client", &mut second),
        ("the third client", &mut third),
        ("the silent client", &mut silent),
    ];
    step_until(&mut quiet, Nanos(answered.0 + 400_000_000), &mut clients);
    let big = vec![b'v'; BIG_BYTES];
    clients[2].1.send(&mut quiet, &[b"SET", b"big", &big]);
    step_until(&mut quiet, Nanos(answered.0 + 800_000_000), &mut clients);
    clients[1].1.send(&mut quiet, &[b"PING"]);
    let stall_end = Nanos(answered.0 + 3 * TIMEOUT.0);
    step_until(&mut quiet, stall_end, &mut clients);
    let unread = quiet.node.nets[0].borrow().unread(clients[1].1.fd).unwrap_or(0);
    assert!(unread > 0, "VACUOUS pool-not-dry: the second client's PING was taken in the stall");
    step_until_answered(&mut quiet, &mut clients);
    assert_eq!(clients[0].1.replies.last().map(Vec::as_slice), Some(b"+OK\r\n".as_slice()));
    assert_eq!(clients[1].1.replies.last().map(Vec::as_slice), Some(b"+PONG\r\n".as_slice()));
    assert_eq!(clients[2].1.replies.last().map(Vec::as_slice), Some(b"+OK\r\n".as_slice()));
    let slack = Nanos(3 * (quiet.step_ns_max + STEP_NS_FLOOR));
    let at = clients[0].1.replied_at.expect("the issuer's +OK");
    clients[3].1.idle_from = IdleFrom::Read { at, slack };
    judge_reaped(&mut quiet, &mut clients);
    assert_deferred(deferrals(&mut quiet, 0), "the stall");
}
