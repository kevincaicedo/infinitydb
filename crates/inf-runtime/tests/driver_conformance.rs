#![allow(
    clippy::disallowed_methods,
    reason = "test target: harness deadlines and timings, not cell code"
)]
//! `BackendDriver` conformance over real loopback TCP. Generic over the
//! driver so the same suite runs on kqueue (macOS dev tier) and io_uring
//! (Linux CI, `--features uring`) — M0-S04/S05 AC.
//!
//! Includes the buffer-lifecycle storm: random accept/recv/send/close cycles
//! must end with the pool fully reconciled (every lease provably returned).
//! Cycle count scales via `INF_LIFECYCLE_CYCLES` (the 1M-cycle AC run);
//! default is CI-sized.

// No backend ⇒ no suite: Linux runs need `--features uring` (kqueue is the
// macOS dev tier). An empty test crate here is correct — the CI matrix has a
// dedicated uring leg, and a panicking `make_driver` stub would not compile
// (`!` does not implement `BackendDriver`).
#![cfg(any(target_os = "macos", all(target_os = "linux", feature = "uring")))]

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::os::fd::IntoRawFd;
use std::time::{Duration, Instant};

use inf_alloc::{BufferPool, LeaseKind};
use inf_runtime::{
    BackendDriver, Completion, CompletionResult, CompletionToken, IoOp, TokenClass, Wait,
};

#[cfg(target_os = "macos")]
fn make_driver() -> impl BackendDriver {
    inf_runtime::KqueueDriver::new().expect("kqueue")
}

#[cfg(all(target_os = "linux", feature = "uring"))]
#[path = "support/uring_env.rs"]
mod uring_env;

#[cfg(all(target_os = "linux", feature = "uring"))]
fn make_driver() -> impl BackendDriver {
    make_ring(uring_env::config())
}

/// The file's one ring constructor: `config`'s selections, checked engaged.
#[cfg(all(target_os = "linux", feature = "uring"))]
fn make_ring(config: inf_runtime::UringConfig) -> inf_runtime::UringDriver {
    let driver = inf_runtime::UringDriver::new(256, config).expect("io_uring");
    uring_env::assert_engaged(&driver.capabilities());
    driver
}

struct Rig {
    driver: Box<dyn DriverObj>,
    pool: BufferPool,
    out: Vec<Completion>,
    #[allow(dead_code)] // kept alive: the driver owns this fd for the rig's lifetime
    listener_fd: i32,
    port: u16,
}

/// Object-safe shim so the test rig can hold any driver (tests are not hot
/// paths; `dyn` is fine here).
trait DriverObj {
    fn push(&mut self, op: IoOp);
    fn submit_and_reap(
        &mut self,
        pool: &mut BufferPool,
        wait: Wait,
        out: &mut Vec<Completion>,
    ) -> std::io::Result<usize>;
    #[cfg(all(target_os = "linux", feature = "uring"))]
    fn caps(&self) -> inf_runtime::Capabilities;
}

impl<D: BackendDriver> DriverObj for D {
    fn push(&mut self, op: IoOp) {
        BackendDriver::push(self, op);
    }
    fn submit_and_reap(
        &mut self,
        pool: &mut BufferPool,
        wait: Wait,
        out: &mut Vec<Completion>,
    ) -> std::io::Result<usize> {
        BackendDriver::submit_and_reap(self, pool, wait, out)
    }
    #[cfg(all(target_os = "linux", feature = "uring"))]
    fn caps(&self) -> inf_runtime::Capabilities {
        BackendDriver::capabilities(self)
    }
}

const ACCEPT_TOKEN: u32 = 1;

impl Rig {
    fn new(pool_buffers: usize) -> Rig {
        Rig::over(Box::new(make_driver()), pool_buffers)
    }

    /// A rig over a ring built from `config` by the file's `make_ring`.
    #[cfg(all(target_os = "linux", feature = "uring"))]
    fn with_ring(pool_buffers: usize, config: inf_runtime::UringConfig) -> Rig {
        Rig::over(Box::new(make_ring(config)), pool_buffers)
    }

    fn over(driver: Box<dyn DriverObj>, pool_buffers: usize) -> Rig {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let listener_fd = listener.into_raw_fd(); // driver owns it now
        let mut rig = Rig {
            driver,
            pool: BufferPool::new(pool_buffers, 4096),
            out: Vec::new(),
            listener_fd,
            port,
        };
        rig.driver.push(IoOp::AcceptArm {
            listener: listener_fd,
            token: CompletionToken::new(TokenClass::Accept, ACCEPT_TOKEN, 0),
        });
        rig
    }

    fn connect(&self) -> TcpStream {
        let stream = TcpStream::connect(("127.0.0.1", self.port)).expect("connect");
        stream.set_read_timeout(Some(Duration::from_secs(5))).expect("timeout");
        stream
    }

    /// Pump the driver until `pred` matches a completion or time runs out;
    /// returns all completions harvested in the meantime.
    fn pump_until(&mut self, pred: impl Fn(&Completion) -> bool) -> Vec<Completion> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut seen = Vec::new();
        loop {
            self.out.clear();
            self.driver
                .submit_and_reap(
                    &mut self.pool,
                    Wait::Park { timeout: Some(Duration::from_millis(20)) },
                    &mut self.out,
                )
                .expect("submit_and_reap");
            let hit = self.out.iter().any(&pred);
            seen.append(&mut self.out);
            if hit {
                return seen;
            }
            assert!(Instant::now() < deadline, "timed out; saw: {seen:?}");
        }
    }

    /// One enter: what a single `submit_and_reap` posted, in its order.
    fn reap_once(&mut self) -> Vec<Completion> {
        self.out.clear();
        self.driver
            .submit_and_reap(
                &mut self.pool,
                Wait::Park { timeout: Some(Duration::from_millis(20)) },
                &mut self.out,
            )
            .expect("submit_and_reap");
        std::mem::take(&mut self.out)
    }

    fn accept_one(&mut self, client: &TcpStream) -> i32 {
        let _ = client;
        let seen = self.pump_until(|c| matches!(c.result, CompletionResult::Accepted { .. }));
        seen.iter()
            .find_map(|c| match c.result {
                CompletionResult::Accepted { fd } => Some(fd),
                _ => None,
            })
            .expect("accepted")
    }
}

fn recv_token(slot: u32) -> CompletionToken {
    CompletionToken::new(TokenClass::Recv, slot, 0)
}

fn send_token(slot: u32) -> CompletionToken {
    CompletionToken::new(TokenClass::Send, slot, 0)
}

#[test]
fn accept_recv_echo_send_close_roundtrip() {
    let mut rig = Rig::new(8);
    let mut client = rig.connect();
    let conn = rig.accept_one(&client);

    rig.driver.push(IoOp::RecvArm { fd: conn, token: recv_token(7) });
    client.write_all(b"hello vortex lessons").expect("client write");

    let seen = rig.pump_until(|c| matches!(c.result, CompletionResult::Recv { .. }));
    let (buf, len) = seen
        .iter()
        .find_map(|c| match c.result {
            CompletionResult::Recv { buf, len } => {
                assert_eq!(c.token.slot(), 7, "recv completion carries the arm token");
                Some((buf, len))
            }
            _ => None,
        })
        .expect("recv");
    assert_eq!(&rig.pool.bytes(buf)[..len as usize], b"hello vortex lessons");

    // Echo it back on a send-leased buffer.
    let reply = rig.pool.try_lease(LeaseKind::Send).expect("send lease");
    rig.pool.bytes_mut(reply)[..len as usize].copy_from_slice(b"hello vortex lessons");
    rig.pool.release(buf); // recv buffer back to the pool
    rig.driver.push(IoOp::Send { fd: conn, buf: reply, len, token: send_token(7) });

    let seen = rig.pump_until(|c| matches!(c.result, CompletionResult::Sent { .. }));
    let sent_buf = seen
        .iter()
        .find_map(|c| match c.result {
            CompletionResult::Sent { buf } => Some(buf),
            _ => None,
        })
        .expect("sent");
    rig.pool.release(sent_buf);

    let mut echoed = vec![0u8; len as usize];
    client.read_exact(&mut echoed).expect("client read");
    assert_eq!(&echoed, b"hello vortex lessons");

    rig.driver.push(IoOp::Close { fd: conn, token: CompletionToken::new(TokenClass::Close, 7, 0) });
    rig.pump_until(|c| matches!(c.result, CompletionResult::Closed));
    assert_eq!(rig.pool.reconcile(), Ok(()), "all buffers returned");
}

#[test]
fn peer_close_delivers_eof_recv() {
    let mut rig = Rig::new(4);
    let client = rig.connect();
    let conn = rig.accept_one(&client);
    rig.driver.push(IoOp::RecvArm { fd: conn, token: recv_token(1) });
    drop(client); // peer closes

    let seen = rig.pump_until(|c| matches!(c.result, CompletionResult::Recv { len: 0, .. }));
    let buf = seen
        .iter()
        .find_map(|c| match c.result {
            CompletionResult::Recv { buf, len: 0 } => Some(buf),
            _ => None,
        })
        .expect("EOF recv");
    rig.pool.release(buf);
    assert_eq!(rig.pool.reconcile(), Ok(()));
}

#[test]
fn pool_exhaustion_pauses_and_resumes_recv() {
    // One buffer: the first recv leases it; while the consumer holds it,
    // more data arrives ⇒ RecvDropped (pause). Releasing the buffer must
    // auto-resume delivery — the backpressure seam (M0-S09 consumes this).
    let mut rig = Rig::new(1);
    let mut client = rig.connect();
    let conn = rig.accept_one(&client);
    rig.driver.push(IoOp::RecvArm { fd: conn, token: recv_token(2) });

    client.write_all(b"first").expect("write 1");
    let seen = rig.pump_until(|c| matches!(c.result, CompletionResult::Recv { .. }));
    let held = seen
        .iter()
        .find_map(|c| match c.result {
            CompletionResult::Recv { buf, .. } => Some(buf),
            _ => None,
        })
        .expect("first recv");

    client.write_all(b"second").expect("write 2");
    rig.pump_until(|c| matches!(c.result, CompletionResult::RecvDropped));

    // Pool still dry: no data can flow. Release ⇒ resume.
    rig.pool.release(held);
    let seen = rig.pump_until(|c| matches!(c.result, CompletionResult::Recv { .. }));
    let (buf, len) = seen
        .iter()
        .find_map(|c| match c.result {
            CompletionResult::Recv { buf, len } => Some((buf, len)),
            _ => None,
        })
        .expect("resumed recv");
    assert_eq!(&rig.pool.bytes(buf)[..len as usize], b"second");
    rig.pool.release(buf);
    assert_eq!(rig.pool.reconcile(), Ok(()));
}

#[test]
fn close_cancels_blocked_sends_and_returns_buffers() {
    let mut rig = Rig::new(8);
    let client = rig.connect();
    let conn = rig.accept_one(&client);

    // Shrink the kernel send buffer, then flood it without the client
    // reading until a send actually blocks (EAGAIN ⇒ queued in the driver).
    set_small_sndbuf(conn);
    let mut queued_any = false;
    for i in 0..64u32 {
        let buf = rig.pool.try_lease(LeaseKind::Send).expect("lease");
        let payload = rig.pool.buf_size() as u32;
        rig.pool.bytes_mut(buf).fill(0xAB);
        rig.driver.push(IoOp::Send { fd: conn, buf, len: payload, token: send_token(i) });
        rig.out.clear();
        rig.driver.submit_and_reap(&mut rig.pool, Wait::Poll, &mut rig.out).expect("submit");
        let mut done = false;
        for c in rig.out.drain(..) {
            match c.result {
                CompletionResult::Sent { buf } => {
                    rig.pool.release(buf);
                    done = true;
                }
                CompletionResult::Error { buf: Some(buf), .. } => {
                    rig.pool.release(buf);
                    done = true;
                }
                _ => {}
            }
        }
        if !done {
            queued_any = true; // this send is now blocked in the driver
            break;
        }
    }
    assert!(queued_any, "could not block a send; cancel path unexercised");

    rig.driver.push(IoOp::Close { fd: conn, token: CompletionToken::new(TokenClass::Close, 9, 0) });
    let seen = rig.pump_until(|c| matches!(c.result, CompletionResult::Closed));
    for c in &seen {
        match c.result {
            CompletionResult::Error { errno, buf: Some(buf) } => {
                assert_eq!(errno, libc::ECANCELED, "blocked sends cancel on close");
                rig.pool.release(buf);
            }
            // A send that raced ahead of the cancel still terminates the
            // lease — either terminal returns the buffer, that is the proof.
            CompletionResult::Sent { buf } => rig.pool.release(buf),
            _ => {}
        }
    }
    assert_eq!(rig.pool.reconcile(), Ok(()), "cancelled sends returned every buffer");
    drop(client);
}

#[test]
fn buffer_lifecycle_storm_reconciles_to_zero() {
    // M0-S04 AC shape: N random accept/recv/send/close cycles; the pool's
    // lease accounting must reconcile exactly. 1M cycles via
    // INF_LIFECYCLE_CYCLES=1000000 (release build) for the gate artifact.
    let cycles: u32 =
        std::env::var("INF_LIFECYCLE_CYCLES").ok().and_then(|v| v.parse().ok()).unwrap_or(500);

    let mut rig = Rig::new(16);
    let mut rng: u64 = 0x9E3779B97F4A7C15;
    let mut next_rand = move || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };

    for cycle in 0..cycles {
        let mut client = rig.connect();
        let conn = rig.accept_one(&client);
        rig.driver.push(IoOp::RecvArm { fd: conn, token: recv_token(cycle & 0xFF_FFFF) });

        let payload_len = (next_rand() % 1024 + 1) as usize;
        let payload: Vec<u8> = (0..payload_len).map(|i| (i as u8) ^ (cycle as u8)).collect();
        client.write_all(&payload).expect("storm write");

        // Collect until the full payload arrived (may split across recvs).
        let mut got = Vec::new();
        while got.len() < payload_len {
            let seen = rig.pump_until(|c| matches!(c.result, CompletionResult::Recv { .. }));
            for c in seen {
                if let CompletionResult::Recv { buf, len } = c.result {
                    got.extend_from_slice(&rig.pool.bytes(buf)[..len as usize]);
                    rig.pool.release(buf);
                }
            }
        }
        assert_eq!(got, payload, "cycle {cycle}: payload corrupted");

        // Echo roughly half the time to mix send lifecycles in.
        if next_rand().is_multiple_of(2) {
            let buf = rig.pool.try_lease(LeaseKind::Send).expect("storm lease");
            let n = payload_len.min(rig.pool.buf_size()) as u32;
            rig.pool.bytes_mut(buf)[..n as usize].copy_from_slice(&payload[..n as usize]);
            rig.driver.push(IoOp::Send {
                fd: conn,
                buf,
                len: n,
                token: send_token(cycle & 0xFF_FFFF),
            });
            let seen = rig.pump_until(|c| matches!(c.result, CompletionResult::Sent { .. }));
            for c in seen {
                if let CompletionResult::Sent { buf } = c.result {
                    rig.pool.release(buf);
                }
            }
            let mut echoed = vec![0u8; n as usize];
            client.read_exact(&mut echoed).expect("storm echo read");
        }

        rig.driver.push(IoOp::Close {
            fd: conn,
            token: CompletionToken::new(TokenClass::Close, cycle & 0xFF_FFFF, 0),
        });
        rig.pump_until(|c| matches!(c.result, CompletionResult::Closed));

        assert_eq!(
            rig.pool.leased(),
            0,
            "cycle {cycle}: leases outstanding after terminal completions"
        );
    }

    assert_eq!(rig.pool.reconcile(), Ok(()), "storm must reconcile to zero");
    let (recv_leases, send_leases) = rig.pool.lease_counts();
    assert!(recv_leases >= u64::from(cycles), "every cycle leased at least one recv buffer");
    assert!(send_leases > 0, "storm exercised the send lifecycle");
}

/// F-L11-08: a send blocked on `EAGAIN` arms the write filter; when a
/// *later* `Send` on the same fd drains the whole queue on the submit
/// path, the filter must go with it. A level-triggered filter left armed
/// fires on every wait — the reactor can never park again on that fd.
#[test]
fn write_filter_disarms_after_a_late_send_drains_the_queue() {
    let mut rig = Rig::new(8);
    let mut client = rig.connect();
    let conn = rig.accept_one(&client);

    // Block a send: small send buffer, the client not reading.
    set_small_sndbuf(conn);
    let mut blocked = None;
    for i in 0..64u32 {
        let buf = rig.pool.try_lease(LeaseKind::Send).expect("lease");
        let payload = rig.pool.buf_size() as u32;
        rig.pool.bytes_mut(buf).fill(0xAB);
        rig.driver.push(IoOp::Send { fd: conn, buf, len: payload, token: send_token(i) });
        rig.out.clear();
        rig.driver.submit_and_reap(&mut rig.pool, Wait::Poll, &mut rig.out).expect("submit");
        let mut done = false;
        for c in rig.out.drain(..) {
            if let CompletionResult::Sent { buf } | CompletionResult::Error { buf: Some(buf), .. } =
                c.result
            {
                rig.pool.release(buf);
                done = true;
            }
        }
        if !done {
            blocked = Some(i);
            break;
        }
    }
    let blocked = blocked.expect("could not block a send; the write filter was never armed");

    // The regime: room appears before the driver's next wait — the peer
    // drains, the kernel buffer grows — and the *next* `Send` on the fd
    // is what empties the queue, on the submit path.
    set_sndbuf(conn, 1 << 20);
    drain_client(&mut client);
    let late = rig.pool.try_lease(LeaseKind::Send).expect("lease");
    rig.pool.bytes_mut(late)[..4].copy_from_slice(b"late");
    rig.driver.push(IoOp::Send { fd: conn, buf: late, len: 4, token: send_token(1000) });
    rig.out.clear();
    rig.driver.submit_and_reap(&mut rig.pool, Wait::Poll, &mut rig.out).expect("submit");
    let mut sent: Vec<u32> = Vec::new();
    for c in rig.out.drain(..) {
        if let CompletionResult::Sent { buf } = c.result {
            rig.pool.release(buf);
            sent.push(c.token.slot());
        }
    }
    sent.sort_unstable();
    assert_eq!(sent, vec![blocked, 1000], "regime: the late send drained the queue synchronously");
    drain_client(&mut client);

    // Nothing is pending on the driver: a park must last its timeout and
    // produce nothing. Three in a row — an orphaned filter returns at once
    // every time.
    let started = Instant::now();
    for round in 0..3 {
        rig.out.clear();
        let produced = rig
            .driver
            .submit_and_reap(
                &mut rig.pool,
                Wait::Park { timeout: Some(Duration::from_millis(100)) },
                &mut rig.out,
            )
            .expect("park");
        assert_eq!(produced, 0, "round {round}: a spurious completion {:?}", rig.out);
    }
    let waited = started.elapsed();
    assert!(
        waited >= Duration::from_millis(200),
        "three 100 ms parks returned after {waited:?}: the write filter is still armed"
    );
    assert_eq!(rig.pool.reconcile(), Ok(()));
    drop(client);
}

/// Lane L11 N19 (batch 70, macOS): a `Close` on a socket whose kernel
/// receive queue still holds bytes the consumer never read. The stop
/// path drops post-mark input by design (ADR-0124 D2) and the peer is
/// owed a FIN, not a reset; XNU sends RST for unread input at close, so
/// the driver discards it first (`shutdown(SHUT_RD)`). The regime is
/// input already delivered: bytes a peer keeps sending after the FIN
/// are reset by TCP itself on every kernel, so the pipeline fits the
/// receive window (64 KiB) and settles before the close. io_uring's tier
/// is not measured here: its multishot recv drains the socket into the
/// pool ahead of any close, and Linux ignores `SHUT_RD` for this.
#[cfg(target_os = "macos")]
#[test]
fn close_with_unread_input_is_a_fin_not_a_reset() {
    let mut rig = Rig::new(4);
    let mut client = rig.connect();
    let conn = rig.accept_one(&client);
    client.set_read_timeout(Some(Duration::from_secs(5))).expect("timeout");
    client.write_all(&vec![0x2A; 64 << 10]).expect("pipeline");
    rig.driver.push(IoOp::RecvArm { fd: conn, token: recv_token(0) });
    let got = rig.pump_until(|c| matches!(c.result, CompletionResult::Recv { .. }));
    std::thread::sleep(Duration::from_millis(50)); // the pipeline has landed
    for c in got {
        if let CompletionResult::Recv { buf, .. } = c.result {
            rig.pool.release(buf);
        }
    }
    rig.driver.push(IoOp::Close { fd: conn, token: CompletionToken::new(TokenClass::Close, 0, 0) });
    let closed = rig.pump_until(|c| matches!(c.result, CompletionResult::Closed));
    assert!(closed.iter().any(|c| matches!(c.result, CompletionResult::Closed)));
    let mut rest = Vec::new();
    let read = client.read_to_end(&mut rest);
    assert!(read.is_ok(), "the close was a reset, not a FIN: {read:?}");
    assert_eq!(rig.pool.reconcile(), Ok(()));
}

/// What one reap posted for the connection under test, in the reap's order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Event {
    Data(u32),
    End,
    Dropped,
    Closed,
}

/// `reaped`'s completions for the receive `recv` and the close `close` as
/// events, each `Recv`'s buffer returned to the pool; another token's
/// completion is skipped. A receive or close that fails is no event of
/// these tests' contracts, so it fails the test.
fn events_of(
    pool: &mut BufferPool,
    reaped: Vec<Completion>,
    recv: CompletionToken,
    close: CompletionToken,
) -> Vec<Event> {
    let mut events = Vec::new();
    for c in reaped {
        if c.token != recv && c.token != close {
            continue;
        }
        match c.result {
            CompletionResult::Recv { buf, len } => {
                pool.release(buf);
                events.push(if len == 0 { Event::End } else { Event::Data(len) });
            }
            CompletionResult::RecvDropped => events.push(Event::Dropped),
            CompletionResult::Closed => events.push(Event::Closed),
            other => panic!("an unexpected completion for {:?}: {other:?}", c.token),
        }
    }
    events
}

/// Whether a test of the multishot receive reads it on this ring. A ring
/// the environment degrades (the kernel matrix's degraded leg, whose mode
/// `make_ring` checked engaged) takes the oneshot receive, and the test
/// reads the same order there; a probed ring the kernel gave no multishot
/// receive cannot be judged, and a skip that exits 0 is no instrument.
#[cfg(all(target_os = "linux", feature = "uring"))]
fn multishot_receive(rig: &Rig) -> bool {
    let caps = rig.driver.caps();
    if caps.multishot_recv {
        return true;
    }
    assert!(
        uring_env::degraded_selected(),
        "VACUOUS multishot-unsupported: a probed ring without multishot receive: {caps:?}"
    );
    false
}

/// The peer's bytes come before its end of stream (ADR-0196 D1): the
/// client writes 64 B and shuts its write side before the driver's next
/// enter, so a multishot receive posts the data and the zero-length `Recv`
/// in one reap, and the driver's order inside one reap is the subject. A
/// zero-length `Recv` ahead of the bytes is `eof-before-data`. A kernel
/// that split them across reaps reached no shared reap: the order is still
/// read, then the run is `VACUOUS`, never green. The oneshot receive posts
/// one completion an arm and re-arms on the next submit, so it has no
/// shared reap to reach: its order alone is read.
#[cfg(all(target_os = "linux", feature = "uring"))]
#[test]
fn multishot_recv_posts_data_before_eof() {
    const ENTERS_MAX: usize = 8;
    let mut rig = Rig::new(8);
    let mut client = rig.connect();
    let conn = rig.accept_one(&client);
    let multishot = multishot_receive(&rig);
    let recv = recv_token(3);
    let close = CompletionToken::new(TokenClass::Close, 3, 0);
    rig.driver.push(IoOp::RecvArm { fd: conn, token: recv });
    client.write_all(&[0x5A; 64]).expect("client write");
    client.shutdown(Shutdown::Write).expect("client FIN");

    let mut seen: Vec<(usize, Event)> = Vec::new();
    for enter in 0..ENTERS_MAX {
        let reaped = rig.reap_once();
        let events = events_of(&mut rig.pool, reaped, recv, close);
        seen.extend(events.into_iter().map(|event| (enter, event)));
        if seen.iter().any(|&(_, event)| event == Event::End) {
            break;
        }
    }
    let end = seen
        .iter()
        .position(|&(_, event)| event == Event::End)
        .unwrap_or_else(|| panic!("no end of stream in {ENTERS_MAX} enters: {seen:?}"));
    let before: u32 = seen[..end]
        .iter()
        .map(|&(_, event)| if let Event::Data(len) = event { len } else { 0 })
        .sum();
    assert_eq!(
        before, 64,
        "eof-before-data: {before} of the 64 bytes written before the FIN came first: {seen:?}"
    );
    assert!(
        seen[..end].iter().all(|&(_, event)| matches!(event, Event::Data(_))),
        "only the bytes precede the end of stream: {seen:?}"
    );
    if multishot {
        let (data_enter, end_enter) = (seen[end - 1].0, seen[end].0);
        assert_eq!(
            data_enter, end_enter,
            "VACUOUS single-reap-unreached: the kernel posted the bytes in enter {data_enter} and \
             the end of stream in enter {end_enter}"
        );
    }

    rig.driver.push(IoOp::Close { fd: conn, token: close });
    rig.pump_until(|c| matches!(c.result, CompletionResult::Closed));
    assert_eq!(rig.pool.reconcile(), Ok(()), "every buffer returned");
}

/// The events of `recv` and `close` from the next enter until `Closed`,
/// then two enters more, each tagged with its enter: a `Recv` the driver
/// posts after `Closed` is read wherever it lands.
#[cfg(all(target_os = "linux", feature = "uring"))]
fn events_through_closed(
    rig: &mut Rig,
    recv: CompletionToken,
    close: CompletionToken,
) -> Vec<(usize, Event)> {
    const ENTERS_AFTER_CLOSED: usize = 2;
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut seen: Vec<(usize, Event)> = Vec::new();
    let mut enter = 0;
    while !seen.iter().any(|&(_, event)| event == Event::Closed) {
        assert!(Instant::now() < deadline, "no Closed in 5 s: {seen:?}");
        let reaped = rig.reap_once();
        let events = events_of(&mut rig.pool, reaped, recv, close);
        seen.extend(events.into_iter().map(|event| (enter, event)));
        enter += 1;
    }
    for _ in 0..ENTERS_AFTER_CLOSED {
        let reaped = rig.reap_once();
        let events = events_of(&mut rig.pool, reaped, recv, close);
        seen.extend(events.into_iter().map(|event| (enter, event)));
        enter += 1;
    }
    seen
}

/// The close-order oracle (ADR-0196 D1): the driver queues the fd's
/// cancels before the close and posts `Closed` after them, so no `Recv`
/// with bytes follows `Closed` (`recv-after-closed`). Engagement: the
/// client's 64 bytes posted before `Closed`, else
/// `recv-before-close-unreached`, `VACUOUS`, never green.
#[cfg(all(target_os = "linux", feature = "uring"))]
fn judge_close_order(seen: &[(usize, Event)]) {
    let closed = seen.iter().position(|&(_, event)| event == Event::Closed).expect("Closed");
    let after: Vec<&(usize, Event)> =
        seen[closed..].iter().filter(|(_, event)| matches!(event, Event::Data(_))).collect();
    assert!(after.is_empty(), "recv-after-closed: bytes posted after Closed: {seen:?}");
    assert!(
        seen[..closed].iter().any(|&(_, event)| event == Event::Data(64)),
        "VACUOUS recv-before-close-unreached: no 64-byte Recv before Closed: {seen:?}"
    );
}

/// A close ends the receive, where the kernel posts completions between
/// enters: a ring set up without deferred task work
/// (`SetupFlags::NoDeferTaskrun`, a boot selection). The receive is armed
/// and submitted before the client writes, so the bytes' `Recv` is in the
/// ring as the write returns, and the `Close` rides the next enter. This
/// order is this regime's alone: under deferred task work, with the
/// receive submitted before the bytes arrive, the kernel posts them at the
/// next enter's task work, after the cancel the `Close` queues there ended
/// the receive, and that enter reaps `Closed` alone. The ring's mode is
/// still the environment's, so the degraded leg reads the oneshot receive.
#[cfg(all(target_os = "linux", feature = "uring"))]
#[test]
fn close_posts_no_recv_after_closed() {
    use inf_runtime::SetupFlags;
    let config = uring_env::config().with_setup(SetupFlags::NoDeferTaskrun);
    let mut rig = Rig::with_ring(8, config);
    assert!(!rig.driver.caps().defer_taskrun, "the no-DEFER_TASKRUN tier");
    let mut client = rig.connect();
    let conn = rig.accept_one(&client);
    multishot_receive(&rig);
    let recv = recv_token(4);
    let close = CompletionToken::new(TokenClass::Close, 4, 0);
    rig.driver.push(IoOp::RecvArm { fd: conn, token: recv });
    let armed = rig.reap_once();
    let early = events_of(&mut rig.pool, armed, recv, close);
    assert!(early.is_empty(), "nothing is posted before the client writes: {early:?}");

    client.write_all(&[0x5A; 64]).expect("client write");
    rig.driver.push(IoOp::Close { fd: conn, token: close });
    let seen = events_through_closed(&mut rig, recv, close);
    judge_close_order(&seen);
    assert_eq!(rig.pool.reconcile(), Ok(()), "every buffer returned");
    drop(client);
}

/// The end of stream ends the receive, on every backend (ADR-0196 A1.3): no
/// `Recv` follows the zero-length one for the token while the fd stays
/// open (`recv-after-eof`), and the half-closed peer still reads what the
/// server sends it, then its end of stream at the `Close`.
#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "red on kqueue: a zero read leaves the level-triggered filter armed"
)]
fn eof_ends_the_receive() {
    const ENTERS_OPEN: usize = 8;
    let mut rig = Rig::new(8);
    let mut client = rig.connect();
    let conn = rig.accept_one(&client);
    let recv = recv_token(6);
    let close = CompletionToken::new(TokenClass::Close, 6, 0);
    rig.driver.push(IoOp::RecvArm { fd: conn, token: recv });
    client.write_all(&[0x5A; 64]).expect("client write");
    client.shutdown(Shutdown::Write).expect("client FIN");

    let reaped = rig.pump_until(|c| {
        c.token == recv && matches!(c.result, CompletionResult::Recv { len: 0, .. })
    });
    let mut seen = events_of(&mut rig.pool, reaped, recv, close);
    let end = seen.iter().position(|&event| event == Event::End).expect("the end of stream");
    let bytes: u32 =
        seen[..end].iter().map(|&event| if let Event::Data(len) = event { len } else { 0 }).sum();
    assert_eq!(bytes, 64, "the bytes before the end of stream: {seen:?}");
    for _ in 0..ENTERS_OPEN {
        let reaped = rig.reap_once();
        seen.extend(events_of(&mut rig.pool, reaped, recv, close));
    }
    assert_eq!(
        seen.len(),
        end + 1,
        "recv-after-eof: a receive event after the zero-length Recv, the fd open: {seen:?}"
    );

    let reply = rig.pool.try_lease(LeaseKind::Send).expect("send lease");
    rig.pool.bytes_mut(reply)[..64].fill(0xA5);
    rig.driver.push(IoOp::Send { fd: conn, buf: reply, len: 64, token: send_token(6) });
    let reaped = rig.pump_until(|c| matches!(c.result, CompletionResult::Sent { .. }));
    for c in reaped {
        match c.result {
            CompletionResult::Sent { buf } => rig.pool.release(buf),
            _ => seen.extend(events_of(&mut rig.pool, vec![c], recv, close)),
        }
    }
    let mut echoed = [0u8; 64];
    client.read_exact(&mut echoed).expect("the half-closed client reads the reply");
    assert_eq!(echoed, [0xA5; 64]);

    rig.driver.push(IoOp::Close { fd: conn, token: close });
    let reaped = rig.pump_until(|c| matches!(c.result, CompletionResult::Closed));
    seen.extend(events_of(&mut rig.pool, reaped, recv, close));
    assert_eq!(seen.len(), end + 2, "recv-after-eof: the events to Closed: {seen:?}");
    let mut rest = Vec::new();
    let read = client.read_to_end(&mut rest).expect("the client reads its end of stream");
    assert_eq!(read, 0, "nothing after the reply, then the end of stream: {rest:?}");
    assert_eq!(rig.pool.reconcile(), Ok(()), "every buffer returned");
}

/// Reads whatever the peer has already delivered and stops at the first
/// empty poll (the driver is not running, so nothing more can arrive).
fn drain_client(client: &mut TcpStream) {
    client.set_nonblocking(true).expect("nonblocking");
    let mut scratch = [0u8; 1 << 16];
    let mut idle_polls = 0;
    while idle_polls < 3 {
        match client.read(&mut scratch) {
            Ok(0) => break,
            Ok(_) => idle_polls = 0,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                idle_polls += 1;
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => panic!("drain: {e}"),
        }
    }
    client.set_nonblocking(false).expect("blocking");
}

fn set_small_sndbuf(fd: i32) {
    set_sndbuf(fd, 4096);
}

fn set_sndbuf(fd: i32, size: libc::c_int) {
    // SAFETY: setsockopt with a valid int pointer on a live socket.
    unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_SNDBUF,
            (&raw const size).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
    }
}

/// The `sqes_per_submit` tripwire's canary, in tree (ADR-0209 D5): 16
/// closes of fresh sockets, each one SQE with nothing in flight on its fd,
/// read at the driver and through a cell loop under each submit policy.
/// The driver leg reads the loop's two inputs (`SubmitStats`); the loop leg
/// reads the loop's own fold. A restock of P provided buffers may ride the
/// first call: batched reads (P + 16) / 2 >= 8 SQEs an enter, per-SQE
/// (P + 16) / (P + 18) < 1.
#[cfg(all(target_os = "linux", feature = "uring"))]
mod submit_policy {
    use std::net::UdpSocket;
    use std::os::fd::IntoRawFd;

    use inf_alloc::BufferPool;
    use inf_foundation::CellId;
    use inf_foundation::bounded::{Cap, CapCensus, CapFill};
    use inf_foundation::time::StdClock;
    use inf_runtime::{
        BackendDriver, CellLoop, CellPlane, Completion, CompletionResult, CompletionToken, IoOp,
        LoopConfig, LoopCx, SubmitPolicy, SubmitStats, TimerSet, TokenClass, Wait,
    };

    use super::{make_ring, uring_env};

    const CLOSES: u32 = 16;
    const OWNERS: Cap = Cap::entries::<1>("submit-policy-owners", CapFill::Serving);
    /// The loop leg's hang bound: the closes complete within a few
    /// iterations, so this bound ends a hang and decides nothing.
    const ITERATIONS_MAX: u32 = 10_000;

    fn closes() -> Vec<IoOp> {
        (0..CLOSES)
            .map(|slot| {
                let fd = UdpSocket::bind("127.0.0.1:0").expect("socket").into_raw_fd();
                IoOp::Close { fd, token: CompletionToken::new(TokenClass::Close, slot, 0) }
            })
            .collect()
    }

    /// The driver leg: the stats of the call that carries the closes, after
    /// a warm-up call (`submit_and_reap` resets them at each call).
    fn measured_call(policy: SubmitPolicy) -> SubmitStats {
        let mut driver = make_ring(uring_env::config().with_submit(policy));
        let mut pool = BufferPool::new(16, 4096);
        let mut out = Vec::new();
        driver.submit_and_reap(&mut pool, Wait::Poll, &mut out).expect("warm-up");
        for op in closes() {
            driver.push(op);
        }
        out.clear();
        driver.submit_and_reap(&mut pool, Wait::Poll, &mut out).expect("the measured call");
        driver.submit_stats()
    }

    #[test]
    fn a_batched_ring_enters_once_for_every_queued_sqe() {
        let stats = measured_call(SubmitPolicy::Batched);
        assert!(stats.sqes >= u64::from(CLOSES), "every close rode the call: {stats:?}");
        assert_eq!(stats.syscalls, 1, "one enter for the call's SQEs: {stats:?}");
    }

    #[test]
    fn a_per_sqe_ring_enters_before_every_chain() {
        let stats = measured_call(SubmitPolicy::PerSqe);
        assert!(stats.sqes >= u64::from(CLOSES), "every close rode the call: {stats:?}");
        // Every queued chain is a single SQE (a close, or a restock), so the
        // call enters once for each and once more for its reap.
        assert_eq!(stats.syscalls, stats.sqes + 1, "an enter before every SQE: {stats:?}");
    }

    /// Pushes the closes once, in its first PARSE, and counts their `Closed`.
    struct ClosePlane {
        pending: Vec<IoOp>,
        closed: u32,
    }

    impl CellPlane for ClosePlane {
        fn on_completion(&mut self, _cx: &mut LoopCx<'_>, c: Completion) {
            if matches!(c.result, CompletionResult::Closed) {
                self.closed += 1;
            }
        }
        fn parse_execute(&mut self, cx: &mut LoopCx<'_>) {
            for op in self.pending.drain(..) {
                cx.push(op);
            }
        }
        fn respond(&mut self, _cx: &mut LoopCx<'_>) {}
    }

    /// The loop leg: `tripwires()`'s `sqes_per_submit` (x1000) once every
    /// close completed.
    fn sqes_per_submit(policy: SubmitPolicy) -> u64 {
        let (census, serve_mark) = CapCensus::new(CellId(0));
        let timers = TimerSet::build(OWNERS, &census).expect("the leg's timer set");
        let driver = make_ring(uring_env::config().with_submit(policy));
        let pool = BufferPool::new(16, 4096);
        let config = LoopConfig::default();
        let mut cell = CellLoop::new(driver, StdClock::new(), pool, config, serve_mark, timers);
        let mut plane = ClosePlane { pending: closes(), closed: 0 };
        let mut iterations = 0;
        while plane.closed < CLOSES {
            assert!(iterations < ITERATIONS_MAX, "{} of {CLOSES} closes completed", plane.closed);
            cell.run_iteration(&mut plane).expect("an iteration");
            iterations += 1;
        }
        let (name, value) = cell.tripwires()[0];
        assert_eq!(name, inf_foundation::tripwire::SQES_PER_SUBMIT);
        value
    }

    #[test]
    #[cfg(not(feature = "no-tripwires"))]
    fn the_tripwire_reads_a_batched_loop_at_eight_sqes_an_enter_or_more() {
        let ratio = sqes_per_submit(SubmitPolicy::Batched);
        assert!(ratio >= 8_000, "sqes_per_submit x1000 = {ratio}: batching is not exercised");
    }

    #[test]
    #[cfg(not(feature = "no-tripwires"))]
    fn the_tripwire_trips_on_a_per_sqe_loop() {
        let ratio = sqes_per_submit(SubmitPolicy::PerSqe);
        assert!(ratio < 1_000, "sqes_per_submit x1000 = {ratio}: the canary did not trip");
    }
}
