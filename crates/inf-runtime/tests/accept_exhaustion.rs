#![allow(
    clippy::disallowed_methods,
    reason = "test target: harness deadlines and timings, not cell code"
)]
//! F-L11-02 / F-L11-06 at the driver: an `EMFILE` on the listener parks the
//! accept arm (one `Error`, then the driver PARKS instead of re-arming into
//! the same failure every submit), and the arm resumes on its own once a
//! `Close` returns a descriptor. Its own test binary because it lowers
//! `RLIMIT_NOFILE`, which is process-wide.

#![cfg(any(target_os = "macos", all(target_os = "linux", feature = "uring")))]

use std::net::{TcpListener, TcpStream};
use std::os::fd::IntoRawFd;
use std::time::{Duration, Instant};

use inf_alloc::BufferPool;
use inf_runtime::{
    AcceptFailure, BackendDriver, Completion, CompletionResult, CompletionToken, IoOp, TokenClass,
    Wait, classify_accept_errno,
};

#[cfg(target_os = "macos")]
fn make_driver() -> impl BackendDriver {
    inf_runtime::KqueueDriver::new().expect("kqueue")
}

#[cfg(all(target_os = "linux", feature = "uring"))]
fn make_driver() -> impl BackendDriver {
    inf_runtime::UringDriver::new(256).expect("io_uring")
}

const ACCEPT_TOKEN: u32 = 1;
const PARK: Duration = Duration::from_millis(20);

fn pump(
    driver: &mut impl BackendDriver,
    pool: &mut BufferPool,
    out: &mut Vec<Completion>,
) -> Vec<Completion> {
    out.clear();
    driver.submit_and_reap(pool, Wait::Park { timeout: Some(PARK) }, out).expect("submit");
    core::mem::take(out)
}

fn pump_until(
    driver: &mut impl BackendDriver,
    pool: &mut BufferPool,
    out: &mut Vec<Completion>,
    pred: impl Fn(&Completion) -> bool,
) -> Vec<Completion> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut seen = Vec::new();
    loop {
        let mut got = pump(driver, pool, out);
        let hit = got.iter().any(&pred);
        seen.append(&mut got);
        if hit {
            return seen;
        }
        assert!(Instant::now() < deadline, "timed out; saw: {seen:?}");
    }
}

fn is_accept_error(c: &Completion) -> Option<i32> {
    match c.result {
        CompletionResult::Error { errno, .. } if c.token.class() == TokenClass::Accept => {
            Some(errno)
        }
        _ => None,
    }
}

fn raw_client_socket() -> i32 {
    // SAFETY: plain socket(2).
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
    assert!(fd >= 0, "socket");
    fd
}

fn raw_connect(fd: i32, port: u16) {
    let addr = libc::sockaddr_in {
        sin_family: libc::AF_INET as libc::sa_family_t,
        sin_port: port.to_be(),
        sin_addr: libc::in_addr { s_addr: u32::from(std::net::Ipv4Addr::LOCALHOST).to_be() },
        sin_zero: [0; 8],
        #[cfg(target_os = "macos")]
        sin_len: 0,
    };
    // SAFETY: a fully initialised sockaddr_in of the stated length on an
    // owned blocking socket; the loopback listener completes the handshake
    // without an accept, so this returns without a new descriptor.
    let rc = unsafe {
        libc::connect(
            fd,
            (&raw const addr).cast::<libc::sockaddr>(),
            core::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        )
    };
    assert_eq!(rc, 0, "connect: {}", std::io::Error::last_os_error());
}

fn nofile() -> libc::rlimit {
    let mut lim = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: out-pointer to a stack rlimit.
    assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) }, 0);
    lim
}

fn set_nofile(cur: libc::rlim_t) {
    let lim = libc::rlimit { rlim_cur: cur, rlim_max: nofile().rlim_max };
    // SAFETY: pointer to a stack rlimit; lowering the soft limit is always
    // permitted, raising it back up to the hard limit too.
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &lim) }, 0, "setrlimit");
}

/// Allocate descriptors until two land consecutively: every hole below is
/// plugged, so the next allocation must exceed the returned fd. Returns
/// that fd and the fillers (kept open).
fn plug_holes() -> (i32, Vec<i32>) {
    let mut fillers = Vec::new();
    let mut last = -2;
    loop {
        // SAFETY: dup of stdin (fd 0), a plain descriptor allocation.
        let fd = unsafe { libc::dup(0) };
        assert!(fd >= 0, "dup");
        fillers.push(fd);
        if fd == last + 1 {
            return (fd, fillers);
        }
        last = fd;
    }
}

#[test]
fn table_agrees_on_every_backend() {
    for e in [libc::EAGAIN, libc::EINTR, libc::ECONNABORTED, libc::EPROTO, libc::EPERM] {
        assert_eq!(classify_accept_errno(e), AcceptFailure::Transient, "errno {e}");
    }
    for e in [libc::EMFILE, libc::ENFILE, libc::ENOBUFS, libc::ENOMEM] {
        assert_eq!(classify_accept_errno(e), AcceptFailure::Exhausted, "errno {e}");
    }
    for e in [libc::EBADF, libc::EINVAL, libc::ENOTSOCK, libc::EOPNOTSUPP, libc::EFAULT, 0, 9999] {
        assert_eq!(classify_accept_errno(e), AcceptFailure::Broken, "errno {e}");
    }
}

#[test]
fn emfile_parks_the_arm_and_a_close_resumes_it() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let listener_fd = listener.into_raw_fd();
    let mut driver = make_driver();
    let mut pool = BufferPool::new(8, 4096);
    let mut out = Vec::new();

    // Every client-side descriptor exists BEFORE the limit drops (the test
    // process is both peer and server), and the limit drops before the arm
    // goes up: io_uring captures `RLIMIT_NOFILE` when the accept SQE is
    // prepared, exactly as a server started under `prlimit` sees it. The
    // headroom is small and then MEASURED: C0 is accepted, the extras are
    // connected one by one until the first `EMFILE`.
    let c0 = raw_client_socket();
    let extras: Vec<i32> = (0..3).map(|_| raw_client_socket()).collect();
    let original = nofile();
    let (high, _fillers) = plug_holes();
    set_nofile(libc::rlim_t::try_from(high).expect("fd") + 4);
    driver.push(IoOp::AcceptArm {
        listener: listener_fd,
        token: CompletionToken::new(TokenClass::Accept, ACCEPT_TOKEN, 0),
    });
    raw_connect(c0, port);
    let seen = pump_until(&mut driver, &mut pool, &mut out, |c| {
        matches!(c.result, CompletionResult::Accepted { .. })
    });
    let conn0 = seen
        .iter()
        .find_map(|c| match c.result {
            CompletionResult::Accepted { fd } => Some(fd),
            _ => None,
        })
        .expect("c0 accepted");
    let mut accepted_extras = 0usize;
    let mut first_error = None;
    let mut connected_extras = 0usize;
    for &fd in &extras {
        raw_connect(fd, port);
        connected_extras += 1;
        let seen = pump_until(&mut driver, &mut pool, &mut out, |c| {
            matches!(c.result, CompletionResult::Accepted { .. }) || is_accept_error(c).is_some()
        });
        accepted_extras +=
            seen.iter().filter(|c| matches!(c.result, CompletionResult::Accepted { .. })).count();
        first_error = seen.iter().find_map(is_accept_error);
        if first_error.is_some() {
            break;
        }
    }
    assert_eq!(first_error, Some(libc::EMFILE), "the limit was reached: {accepted_extras} in");
    assert!(accepted_extras < extras.len(), "at least one client is queued at the limit");

    // Held at EMFILE: no further error, nothing accepted — the arm is
    // parked, so every pump parks for its full timeout and delivers
    // nothing. (Pre-fix: one `Error{EMFILE}` per pump and no park at all.)
    let started = Instant::now();
    let mut errors = Vec::new();
    let mut idle_pumps = 0u32;
    let mut accepted_under_limit = 0u32;
    for _ in 0..25 {
        let got = pump(&mut driver, &mut pool, &mut out);
        if got.is_empty() {
            idle_pumps += 1;
        }
        for c in &got {
            if let Some(errno) = is_accept_error(c) {
                errors.push(errno);
            }
            if matches!(c.result, CompletionResult::Accepted { .. }) {
                accepted_under_limit += 1;
            }
        }
    }
    let held = started.elapsed();
    assert_eq!(accepted_under_limit, 0, "nothing can be accepted at the limit");
    assert!(errors.is_empty(), "parked: no re-arm into the same failure, got {errors:?}");
    assert!(idle_pumps >= 24, "parked pumps deliver nothing: {idle_pumps} of 25 were idle");
    assert!(
        held >= PARK * 20,
        "a parked arm parks the driver for its timeout; 25 pumps took {held:?}"
    );

    // An fd returns through `Close`: the arm resumes on its own and one
    // queued client is let in (a second queued client may meet `EMFILE`
    // again — one more error, parked again; the plane's retry wheel is the
    // other resume path).
    driver.push(IoOp::Close { fd: conn0, token: CompletionToken::new(TokenClass::Close, 0, 0) });
    let seen = pump_until(&mut driver, &mut pool, &mut out, |c| {
        matches!(c.result, CompletionResult::Accepted { .. })
    });
    assert!(
        seen.iter().any(|c| matches!(c.result, CompletionResult::Closed)),
        "close completed: {seen:?}"
    );
    accepted_extras +=
        seen.iter().filter(|c| matches!(c.result, CompletionResult::Accepted { .. })).count();
    assert!(seen.iter().filter(|c| is_accept_error(c).is_some()).count() <= 1, "{seen:?}");

    // The limit lifts (descriptors freed outside the driver): the
    // consumer's re-arm lets every remaining client in. io_uring may
    // deliver one stale EMFILE captured under the old limit; re-arming
    // (the retry wheel) re-prepares under the lifted limit.
    set_nofile(original.rlim_cur);
    for &fd in &extras[connected_extras..] {
        raw_connect(fd, port);
    }
    driver.push(IoOp::AcceptArm {
        listener: listener_fd,
        token: CompletionToken::new(TokenClass::Accept, ACCEPT_TOKEN, 0),
    });
    while accepted_extras < extras.len() {
        let seen = pump_until(&mut driver, &mut pool, &mut out, |c| {
            matches!(c.result, CompletionResult::Accepted { .. }) || is_accept_error(c).is_some()
        });
        for c in &seen {
            if is_accept_error(c).is_some() {
                driver.push(IoOp::AcceptArm {
                    listener: listener_fd,
                    token: CompletionToken::new(TokenClass::Accept, ACCEPT_TOKEN, 0),
                });
            }
        }
        accepted_extras +=
            seen.iter().filter(|c| matches!(c.result, CompletionResult::Accepted { .. })).count();
    }
    assert_eq!(accepted_extras, extras.len());

    // `AcceptArm` on an armed listener is idempotent (two pushes, one
    // accept per client — a second multishot arm would double-deliver).
    // io_uring captured the OLD limit when the resumed arm was prepared,
    // so this client may meet one more `EMFILE`; the consumer's re-arm
    // (the plane's retry wheel) re-prepares under the lifted limit.
    for _ in 0..2 {
        driver.push(IoOp::AcceptArm {
            listener: listener_fd,
            token: CompletionToken::new(TokenClass::Accept, ACCEPT_TOKEN, 0),
        });
    }
    let _c_last = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    let mut stale_limit_errors = 0;
    let accepted = loop {
        let seen = pump_until(&mut driver, &mut pool, &mut out, |c| {
            matches!(c.result, CompletionResult::Accepted { .. }) || is_accept_error(c).is_some()
        });
        let accepted =
            seen.iter().filter(|c| matches!(c.result, CompletionResult::Accepted { .. })).count();
        if accepted > 0 {
            break accepted;
        }
        stale_limit_errors += 1;
        assert!(stale_limit_errors <= 1, "the re-prepared arm sees the lifted limit: {seen:?}");
        driver.push(IoOp::AcceptArm {
            listener: listener_fd,
            token: CompletionToken::new(TokenClass::Accept, ACCEPT_TOKEN, 0),
        });
    };
    assert_eq!(accepted, 1, "one arm, one accept");
    for _ in 0..3 {
        let got = pump(&mut driver, &mut pool, &mut out);
        assert!(got.is_empty(), "idle after resume: {got:?}");
    }
    // SAFETY: closing the raw client sockets this test opened.
    unsafe {
        libc::close(c0);
        for fd in extras {
            libc::close(fd);
        }
    }
}
