//! `timeout` judges a connection only when the server owes it nothing
//! (ADR-0193 D1): a client whose command is held at a frozen peer or by a
//! stalled cell is answered first, and its idle clock runs from its reply
//! (a reply sent is activity, D3) and from the stall's end (D2). Each test
//! boots a quiet durable node, sets `timeout 1` over the wire, and reads
//! its clients' bytes and the server's closes from the net with the node's
//! virtual clock: no code shared with the sweep.
//!
//! The window a close must fall in is the idle oracle's: from the step the
//! client reads its last reply, `[timeout - step_ns_max - 1 ms, timeout +
//! 4 * step_ns_max + 1 ms]`. Each test checks its engagement: the hold
//! outlasted `timeout` while the client was owed a reply, else `VACUOUS
//! hold-under-timeout`.

use inf_foundation::time::{Clock, Nanos};
use inf_runtime::RawFd;
use inf_store::SlotRouter;

use super::park::{Quiet, STEPS_MAX};
use super::{DurableScenario, MiniClient, encode};
use crate::resp::reply_len;

/// The `timeout` every test sets, 1 s.
const TIMEOUT: Nanos = Nanos(1_000_000_000);

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
/// against the idle window after its last reply.
fn judge_reaped(quiet: &mut Quiet, watched: &mut [(&str, &mut Watched)]) {
    let lower = Nanos(TIMEOUT.0 - quiet.step_ns_max - 1_000_000);
    let upper = Nanos(TIMEOUT.0 + 4 * quiet.step_ns_max + 1_000_000);
    let last = watched.iter().filter_map(|(_, client)| client.replied_at).max();
    let until = Nanos(last.expect("every client was answered").0 + upper.0 + 1);
    while quiet.clock.now() < until && watched.iter().any(|(_, client)| client.closed_at.is_none())
    {
        step(quiet, watched);
    }
    for (who, client) in watched.iter() {
        let replied = client.replied_at.expect("answered");
        let Some(closed) = client.closed_at else {
            panic!(
                "reaped-after-window: {who} was still open {} ms after its reply",
                upper.0 / 1_000_000
            );
        };
        let idle = Nanos(closed.0 - replied.0);
        assert!(
            idle >= lower,
            "reaped-before-window: {who} closed {} us after its reply, under {} us",
            idle.0 / 1_000,
            lower.0 / 1_000
        );
        assert!(
            idle <= upper,
            "reaped-after-window: {who} closed {} us after its reply, past {} us",
            idle.0 / 1_000,
            upper.0 / 1_000
        );
    }
}

/// `CONFIG SET timeout 1` on every cell (the command scatters), from a
/// client that then stays silent.
fn set_timeout(quiet: &mut Quiet) {
    let mut setup = MiniClient::connect(&mut quiet.node, 0);
    let reply = quiet.call(&mut setup, &[b"CONFIG", b"SET", b"timeout", b"1"], "CONFIG SET");
    assert_eq!(reply, b"+OK\r\n", "CONFIG SET timeout 1");
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

/// A `GET` of a key cell 1 owns while cell 1 is frozen for twice the
/// timeout: the client waits on a reply the server owes it, so no sweep
/// closes it; it reads the reply once cell 1 runs again, and is reaped a
/// timeout after it.
#[test]
#[ignore = "timeout judges a connection by its receive clock alone, so a client waiting on a \
            reply held at a frozen peer is closed before the reply; red until a connection the \
            server owes a reply is never judged and a reply sent is activity"]
fn timeout_spares_a_command_held_at_a_frozen_peer() {
    let mut quiet = Quiet::boot(2, DurableScenario::SPIN_ITERS, 0);
    set_timeout(&mut quiet);
    let mut client = Watched::connect(&mut quiet, 0);
    quiet.node.frozen = Some((1, u64::MAX));
    let sent = quiet.clock.now();
    client.send(&mut quiet, &[b"GET", &key_on(2, 1)]);
    step_until(&mut quiet, Nanos(sent.0 + 2 * TIMEOUT.0), &mut [("the held client", &mut client)]);
    assert_hold_engaged(Nanos(quiet.clock.now().0 - sent.0), &client, "the held client");
    quiet.node.frozen = None;
    step_until_answered(&mut quiet, &mut [("the held client", &mut client)]);
    assert_eq!(client.replies, [b"$-1\r\n".to_vec()], "the held GET's reply");
    judge_reaped(&mut quiet, &mut [("the held client", &mut client)]);
}

/// `DEBUG SLEEP 3` under `timeout 1`. During the sleep a third client sends a
/// 160 KiB `SET`, more than the cell's pool of 128 buffers of 1 KiB, so the
/// inbox holds the pool; then a second client, answered 0.8 s before, sends
/// `PING`, which waits in the net as the kernel holds a paused receive. Every
/// request of the stalled cell is answered after the stall, and each client
/// is reaped a timeout after its reply (ADR-0193 D2, D5). Engagement: the
/// second client's bytes are still in the net when the stall ends, else
/// `VACUOUS pool-not-dry`.
#[test]
#[ignore = "timeout judges a connection by its receive clock while its cell is stalled, so the \
            sweep closes clients whose requests the stall holds unread; red until a connection \
            is judged from the later of its clock and the stall's end"]
fn timeout_spares_the_replies_of_a_stalled_cell() {
    const BIG_BYTES: usize = 160 * 1024;
    let mut quiet = Quiet::boot(1, DurableScenario::SPIN_ITERS, 0);
    set_timeout(&mut quiet);
    let mut second = Watched::connect(&mut quiet, 0);
    second.send(&mut quiet, &[b"PING"]);
    step_until_answered(&mut quiet, &mut [("the second client", &mut second)]);
    let answered = second.replied_at.expect("PONG");
    let mut issuer = Watched::connect(&mut quiet, 0);
    issuer.send(&mut quiet, &[b"DEBUG", b"SLEEP", b"3"]);
    let mut third = Watched::connect(&mut quiet, 0);
    let mut clients = [
        ("the issuer", &mut issuer),
        ("the second client", &mut second),
        ("the third client", &mut third),
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
    judge_reaped(&mut quiet, &mut clients);
}
