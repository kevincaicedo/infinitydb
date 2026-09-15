#![allow(
    clippy::disallowed_methods,
    reason = "test target: harness deadlines and timings, not cell code"
)]
//! F-L11-01 at the driver: a `Close` whose fd number the kernel hands to
//! the next accepted connection before the closed connection's send
//! resolves. The driver's close/send side tables were keyed by the bare
//! fd, so (a) a short write reaped after the close resubmitted its
//! remainder on the *successor's* socket and (b) the successor's own
//! `Close` overwrote the predecessor's wait — one `Closed` for two
//! connections. Its own test binary: fd numbers must be reused
//! deterministically (no concurrent test opening descriptors), and the
//! driver is built on the no-`DEFER_TASKRUN` tier (a pre-6.1 kernel's),
//! where the kernel posts CQEs between enters — the only tier on which
//! a send's completion can land after its fd's close was queued.

#![cfg(all(target_os = "linux", feature = "uring"))]

use std::io::{ErrorKind, Read};
use std::net::{TcpListener, TcpStream};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd};
use std::time::{Duration, Instant};

use inf_alloc::{BufferPool, LeaseKind};
use inf_runtime::{
    BackendDriver, Completion, CompletionResult, CompletionToken, IoOp, TokenClass, UringDriver,
    Wait,
};

const ACCEPT_TOKEN: u32 = 1;
const PARK: Duration = Duration::from_millis(20);
/// Sends are one pool buffer. Loopback's MSS is 64 KiB, so a locked
/// receive window reopens only once half of it is read (SWS avoidance):
/// the opening read below frees ~16 KiB, the resumed send writes that
/// much and leaves a remainder far larger than the successor's locked
/// buffers — a misrouted remainder blocks there, the lane's step 6.
const BUF: usize = 64 * 1024;
const OPENING_READ: usize = 16 * 1024;

fn set_sockbuf(fd: i32, opt: libc::c_int, size: libc::c_int) {
    // SAFETY: setsockopt with a valid int pointer on a live socket.
    let rc = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            opt,
            (&raw const size).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    assert_eq!(rc, 0, "setsockopt");
}

/// A bare, unconnected IPv4 stream socket (`TcpStream::connect` would
/// allocate its descriptor at connect time).
fn unconnected_socket() -> i32 {
    // SAFETY: plain socket(2).
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
    assert!(fd >= 0, "socket");
    fd
}

fn connect(fd: i32, port: u16) -> TcpStream {
    let addr = libc::sockaddr_in {
        sin_family: libc::AF_INET as libc::sa_family_t,
        sin_port: port.to_be(),
        sin_addr: libc::in_addr { s_addr: u32::from_be_bytes([127, 0, 0, 1]).to_be() },
        sin_zero: [0; 8],
    };
    // SAFETY: connect(2) with a fully initialised sockaddr_in of the stated length.
    let rc = unsafe {
        libc::connect(
            fd,
            (&raw const addr).cast(),
            std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        )
    };
    assert_eq!(rc, 0, "connect: {}", std::io::Error::last_os_error());
    // SAFETY: `fd` is an owned, connected stream socket nothing else holds.
    unsafe { TcpStream::from_raw_fd(fd) }
}

fn pump(driver: &mut UringDriver, pool: &mut BufferPool, wait: Wait) -> Vec<Completion> {
    let mut out = Vec::new();
    driver.submit_and_reap(pool, wait, &mut out).expect("submit");
    out
}

fn pump_until(
    driver: &mut UringDriver,
    pool: &mut BufferPool,
    pred: impl Fn(&Completion) -> bool,
) -> Vec<Completion> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut seen = Vec::new();
    loop {
        let mut got = pump(driver, pool, Wait::Park { timeout: Some(PARK) });
        let hit = got.iter().any(&pred);
        seen.append(&mut got);
        if hit {
            return seen;
        }
        assert!(Instant::now() < deadline, "timed out; saw: {seen:?}");
    }
}

fn accepted_fd(seen: &[Completion]) -> i32 {
    seen.iter()
        .find_map(|c| match c.result {
            CompletionResult::Accepted { fd } => Some(fd),
            _ => None,
        })
        .expect("accepted")
}

fn send_token(slot: u32) -> CompletionToken {
    CompletionToken::new(TokenClass::Send, slot, 0)
}

fn close_token(slot: u32) -> CompletionToken {
    CompletionToken::new(TokenClass::Close, slot, 0)
}

/// Release every buffer a terminal send completion carries; returns the
/// tokens that terminated.
fn release_sends(pool: &mut BufferPool, seen: &[Completion]) -> Vec<(CompletionToken, bool)> {
    let mut done = Vec::new();
    for c in seen {
        match c.result {
            CompletionResult::Sent { buf } => {
                pool.release(buf);
                done.push((c.token, true));
            }
            CompletionResult::Error { buf: Some(buf), .. } => {
                pool.release(buf);
                done.push((c.token, false));
            }
            _ => {}
        }
    }
    done
}

/// Drain a peer socket to EOF (or the read timeout) and count the bytes.
fn drain(peer: &mut TcpStream, timeout: Duration) -> usize {
    peer.set_read_timeout(Some(timeout)).expect("timeout");
    let mut total = 0;
    let mut scratch = [0u8; 8192];
    loop {
        match peer.read(&mut scratch) {
            Ok(0) => return total,
            Ok(n) => total += n,
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                return total;
            }
            Err(e) if e.kind() == ErrorKind::ConnectionReset => return total,
            Err(e) => panic!("peer read: {e}"),
        }
    }
}

#[test]
fn a_reused_fd_number_keeps_both_closes_and_never_carries_the_predecessors_bytes() {
    // SAFETY: single-test binary, set before any thread or ring exists.
    unsafe { std::env::set_var("INF_URING_NO_DEFER_TASKRUN", "1") };
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    // Accepted sockets inherit the listener's (locked) send buffer.
    set_sockbuf(listener.as_raw_fd(), libc::SO_SNDBUF, 4096);
    let listener_fd = listener.into_raw_fd();
    let mut driver = UringDriver::new(256).expect("io_uring");
    assert!(!driver.capabilities().defer_taskrun, "the no-DEFER_TASKRUN tier");
    let mut pool = BufferPool::new(64, BUF);
    driver.push(IoOp::AcceptArm {
        listener: listener_fd,
        token: CompletionToken::new(TokenClass::Accept, ACCEPT_TOKEN, 0),
    });

    // ---- connection A: flood until a send blocks in the kernel.
    // Receive buffers are locked before connecting: autotuning would
    // otherwise open the window wide enough for the whole send.
    let client_a = unconnected_socket();
    set_sockbuf(client_a, libc::SO_RCVBUF, 16 * 1024);
    let mut peer_a = connect(client_a, port);
    let fd_a = accepted_fd(&pump_until(&mut driver, &mut pool, |c| {
        matches!(c.result, CompletionResult::Accepted { .. })
    }));
    let mut full_sends = 0usize;
    let mut blocked: Option<CompletionToken> = None;
    for i in 0..64u32 {
        let buf = pool.try_lease(LeaseKind::Send).expect("lease");
        pool.bytes_mut(buf).fill(0xAB);
        let token = send_token(i);
        driver.push(IoOp::Send { fd: fd_a, buf, len: BUF as u32, token });
        let seen = pump(&mut driver, &mut pool, Wait::Poll);
        let done = release_sends(&mut pool, &seen);
        if done.is_empty() {
            blocked = Some(token);
            break;
        }
        assert!(done.iter().all(|(_, ok)| *ok), "flood sends succeed: {seen:?}");
        full_sends += 1;
    }
    let blocked = blocked.expect("could not block a send; regime unreachable");

    // Open the window by one read: the blocked send resumes and writes a
    // PART of its buffer. On this tier the CQE lands at the next syscall
    // exit — before the close below is queued, exactly the reap order
    // the lane's step 3 needs (short write reaped in the close's pump).
    let mut opened = vec![0u8; OPENING_READ];
    peer_a.set_read_timeout(Some(Duration::from_millis(300))).expect("timeout");
    let mut opened_len = 0usize;
    while opened_len < OPENING_READ {
        match peer_a.read(&mut opened[opened_len..]) {
            Ok(0) => panic!("peer A saw EOF before the close"),
            Ok(n) => opened_len += n,
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => break,
            Err(e) => panic!("peer A read: {e}"),
        }
    }
    assert!(opened_len > 0, "peer A had nothing to read");
    assert!(opened[..opened_len].iter().all(|&b| b == 0xAB));
    std::thread::sleep(Duration::from_millis(50));

    // B's client socket is created BEFORE A closes, so the descriptor A
    // frees is the lowest free one when the kernel accepts B.
    let client_b = unconnected_socket();
    set_sockbuf(client_b, libc::SO_RCVBUF, 8 * 1024);

    // ---- close A with the short write unreaped.
    driver.push(IoOp::Close { fd: fd_a, token: close_token(100) });
    let close_pump = pump(&mut driver, &mut pool, Wait::Poll);
    let mut ended: Vec<(CompletionToken, bool)> = release_sends(&mut pool, &close_pump);
    let mut closed: Vec<CompletionToken> = close_pump
        .iter()
        .filter(|c| matches!(c.result, CompletionResult::Closed))
        .map(|c| c.token)
        .collect();

    // ---- connection B lands on A's fd number.
    let mut peer_b = connect(client_b, port);
    let accept_pump = pump_until(&mut driver, &mut pool, |c| {
        matches!(c.result, CompletionResult::Accepted { .. })
    });
    let fd_b = accepted_fd(&accept_pump);
    assert_eq!(fd_b, fd_a, "the kernel reuses the lowest free descriptor");
    ended.extend(release_sends(&mut pool, &accept_pump));
    closed.extend(
        accept_pump
            .iter()
            .filter(|c| matches!(c.result, CompletionResult::Closed))
            .map(|c| c.token),
    );

    // ---- close B; settle everything.
    driver.push(IoOp::Close { fd: fd_b, token: close_token(200) });
    let settle_deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < settle_deadline {
        let seen = pump(&mut driver, &mut pool, Wait::Park { timeout: Some(PARK) });
        ended.extend(release_sends(&mut pool, &seen));
        closed.extend(
            seen.iter().filter(|c| matches!(c.result, CompletionResult::Closed)).map(|c| c.token),
        );
    }

    // Regime oracle (tree-independent): A's peer saw the full sends plus
    // a strict part of the blocked one — the blocked send WAS short-written.
    let a_bytes = opened_len + drain(&mut peer_a, Duration::from_secs(1));
    let partial = a_bytes.checked_sub(full_sends * BUF).expect("no fewer than the full sends");
    assert!(
        partial > 0 && partial < BUF,
        "regime not reached: the blocked send was not short-written (partial {partial})"
    );

    // (b) One `Closed` per `Close`, both of them.
    let mut closed_slots: Vec<u32> = closed.iter().map(|t| t.slot()).collect();
    closed_slots.sort_unstable();
    assert_eq!(closed_slots, vec![100, 200], "both closes complete exactly once");

    // (a) The successor never receives the predecessor's bytes.
    let leaked = drain(&mut peer_b, Duration::from_secs(1));
    assert_eq!(leaked, 0, "connection B received {leaked} bytes of A's remainder");
    // The short-written send terminates as cancelled, never `Sent` after
    // its fd was closed.
    let fate = ended.iter().find(|(t, _)| *t == blocked).map(|(_, ok)| *ok);
    assert_eq!(fate, Some(false), "the closed connection's send ends cancelled, not Sent");
    assert_eq!(pool.reconcile(), Ok(()), "every send buffer returned");
}
