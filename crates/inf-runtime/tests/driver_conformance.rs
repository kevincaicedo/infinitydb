//! `BackendDriver` conformance over real loopback TCP. Generic over the
//! driver so the same suite runs on kqueue (macOS dev tier) and io_uring
//! (Linux CI, `--features uring`) — M0-S04/S05 AC.
//!
//! Includes the buffer-lifecycle storm: random accept/recv/send/close cycles
//! must end with the pool fully reconciled (every lease provably returned).
//! Cycle count scales via `INF_LIFECYCLE_CYCLES` (the 1M-cycle AC run);
//! default is CI-sized.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
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
fn make_driver() -> impl BackendDriver {
    inf_runtime::UringDriver::new(256).expect("io_uring")
}

#[cfg(not(any(target_os = "macos", all(target_os = "linux", feature = "uring"))))]
fn make_driver() -> impl BackendDriver {
    panic!("no backend for this target; Linux runs need --features uring")
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
}

const ACCEPT_TOKEN: u32 = 1;

impl Rig {
    fn new(pool_buffers: usize) -> Rig {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let listener_fd = listener.into_raw_fd(); // driver owns it now
        let mut rig = Rig {
            driver: Box::new(make_driver()),
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
        if let CompletionResult::Error { errno, buf: Some(buf) } = c.result {
            assert_eq!(errno, libc::ECANCELED, "blocked sends cancel on close");
            rig.pool.release(buf);
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

fn set_small_sndbuf(fd: i32) {
    let size: libc::c_int = 4096;
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
