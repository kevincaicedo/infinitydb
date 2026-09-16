//! Node-assembly helpers (M0): `SO_REUSEPORT` listeners — one acceptor per
//! cell, the kernel spreads connections (master plan §5.3) — and
//! best-effort thread pinning. Lives here so `infinityd`/`inf-sim` stay
//! `#![forbid(unsafe_code)]`; this crate owns the socket/thread FFI.

use std::io;
use std::net::TcpListener;
use std::os::fd::FromRawFd;

/// Binds a `SO_REUSEPORT` IPv4 listener on `port` (0.0.0.0). Every cell
/// binds the same port; the kernel hashes incoming connections across the
/// listeners.
///
/// # Errors
/// Propagates socket/bind/listen failures (port in use without reuseport,
/// privileged port, fd exhaustion).
pub fn listen_reuseport(port: u16) -> io::Result<TcpListener> {
    // SAFETY: plain socket(2) FFI; the fd is checked before use and owned by
    // the returned TcpListener (closed on drop or error paths below).
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the raw fd is fresh and owned exclusively by this listener.
    let listener = unsafe { TcpListener::from_raw_fd(fd) };
    let one: libc::c_int = 1;
    for opt in [libc::SO_REUSEADDR, libc::SO_REUSEPORT] {
        // SAFETY: setsockopt with a valid int pointer on the live socket.
        let rc = unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                opt,
                (&raw const one).cast(),
                size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    let addr = libc::sockaddr_in {
        sin_family: libc::AF_INET as libc::sa_family_t,
        sin_port: port.to_be(),
        sin_addr: libc::in_addr { s_addr: libc::INADDR_ANY.to_be() },
        sin_zero: [0; 8],
        #[cfg(target_os = "macos")]
        sin_len: 0,
    };
    // SAFETY: addr is a fully initialized sockaddr_in of the stated length.
    let rc = unsafe {
        libc::bind(fd, (&raw const addr).cast(), size_of::<libc::sockaddr_in>() as libc::socklen_t)
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: listen on the bound socket.
    if unsafe { libc::listen(fd, 1024) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(listener)
}

/// Sets TCP keepalive on an accepted socket the way Redis's
/// `anetKeepAlive` does (ADR-0123 D3): `SO_KEEPALIVE` with `secs` idle,
/// probes every `max(1, secs / 3)` seconds, three probes; `secs == 0`
/// clears `SO_KEEPALIVE`. Lives here so the plane stays
/// `forbid(unsafe_code)`.
///
/// # Errors
/// The first failing `setsockopt` (a non-TCP fd; callers ignore it like
/// `TCP_NODELAY`'s failure).
pub fn set_keepalive(fd: std::os::fd::RawFd, secs: u32) -> io::Result<()> {
    let set = |level: libc::c_int, opt: libc::c_int, value: libc::c_int| -> io::Result<()> {
        // SAFETY: setsockopt with a valid int pointer of the stated length
        // on a caller-owned fd; a bad fd fails with EBADF/ENOTSOCK.
        let rc = unsafe {
            libc::setsockopt(
                fd,
                level,
                opt,
                (&raw const value).cast(),
                size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        if rc == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
    };
    if secs == 0 {
        return set(libc::SOL_SOCKET, libc::SO_KEEPALIVE, 0);
    }
    let idle = libc::c_int::try_from(secs).unwrap_or(libc::c_int::MAX);
    set(libc::SOL_SOCKET, libc::SO_KEEPALIVE, 1)?;
    #[cfg(target_os = "linux")]
    {
        set(libc::IPPROTO_TCP, libc::TCP_KEEPIDLE, idle)?;
        set(libc::IPPROTO_TCP, libc::TCP_KEEPINTVL, (idle / 3).max(1))?;
        set(libc::IPPROTO_TCP, libc::TCP_KEEPCNT, 3)?;
    }
    #[cfg(target_os = "macos")]
    set(libc::IPPROTO_TCP, libc::TCP_KEEPALIVE, idle)?;
    Ok(())
}

/// This process's resident set in bytes: Linux from `/proc/self/status`
/// (`VmRSS`), macOS from `proc_pidinfo(PROC_PIDTASKINFO)`; `None` where
/// no reader exists (lane L11 N19, batch 70 — a gauge abstains, never
/// reports 0 for "could not read"). Tooling only, never the data plane.
pub fn process_rss_bytes() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        let line = status.lines().find(|l| l.starts_with("VmRSS:"))?;
        line.split_whitespace().nth(1)?.parse::<u64>().ok().map(|kb| kb * 1024)
    }
    #[cfg(target_os = "macos")]
    {
        let mut info = std::mem::MaybeUninit::<libc::proc_taskinfo>::uninit();
        let size = size_of::<libc::proc_taskinfo>() as libc::c_int;
        // SAFETY: proc_pidinfo writes at most `size` bytes into `info`, a
        // buffer of exactly that size, and returns the bytes it filled.
        let filled = unsafe {
            libc::proc_pidinfo(
                std::process::id() as libc::c_int,
                libc::PROC_PIDTASKINFO,
                0,
                info.as_mut_ptr().cast(),
                size,
            )
        };
        if filled != size {
            return None;
        }
        // SAFETY: the kernel filled every byte of the struct (checked above).
        let info = unsafe { info.assume_init() };
        Some(info.pti_resident_size)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        None
    }
}

/// Refuses a port another process already listens on (batch 59, review
/// 2026-08-30 lane L19 addendum): `SO_REUSEPORT` lets a second node of
/// the same uid *join* a running node's listener group, and the kernel
/// then splits new connections between two keyspaces — silently. A plain
/// bind (no reuseport) fails `EADDRINUSE` against any listener on the
/// port, so a node probes once, before its cells bind the group. Port 0
/// (kernel-assigned) needs no probe. The window between the probe's
/// close and the group's bind is a foreign-process race the probe cannot
/// close; it turns a silent split into a loud one everywhere else.
///
/// # Errors
/// `AddrInUse` (with the port named) when the port is owned; other bind
/// failures as they are.
pub fn probe_port_unowned(port: u16) -> io::Result<()> {
    if port == 0 {
        return Ok(());
    }
    match probe_addr_unowned(std::net::SocketAddrV4::new(std::net::Ipv4Addr::UNSPECIFIED, port)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AddrInUse => Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!("port {port} is already owned by another process (address in use)"),
        )),
        Err(e) => Err(e),
    }
}

/// Binds `addr` (`SO_REUSEADDR`, no reuseport, **no listen**) and closes
/// it: `Ok` when nothing listens there. A bound socket never accepts, so
/// a connection arriving inside the probe's window is refused, not
/// accepted-then-reset — and a copy of the socket inherited by a
/// concurrently spawned child (`posix_spawn` copies the fd table until
/// the child execs) cannot answer either. Batch 61: `TcpListener::bind`
/// listens, and the compat harness's listening probe lived on in a
/// sibling test's spawning child long enough to accept — and then reset —
/// the readiness `PING` on a just-reserved port.
///
/// # Errors
/// `AddrInUse` when a listener owns the address; other socket/bind
/// failures as they are.
pub fn probe_addr_unowned(addr: std::net::SocketAddrV4) -> io::Result<()> {
    // `SOCK_CLOEXEC` is Linux-only (batch 61 broke the macOS build with
    // it); the dev tier sets the flag after the fact, which leaves a
    // spawn-race window the probe's doc already declares foreign.
    #[cfg(target_os = "linux")]
    let kind = libc::SOCK_STREAM | libc::SOCK_CLOEXEC;
    #[cfg(not(target_os = "linux"))]
    let kind = libc::SOCK_STREAM;
    // SAFETY: plain socket(2) FFI; the fd is checked before use and owned
    // by `owned` (closed on every path below).
    let fd = unsafe { libc::socket(libc::AF_INET, kind, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the raw fd is fresh and owned exclusively here.
    let _owned = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) };
    #[cfg(not(target_os = "linux"))]
    // SAFETY: fcntl on the live fd owned above; a failure only leaves the
    // probe socket inheritable for the microseconds until `_owned` drops.
    unsafe {
        libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
    }
    let one: libc::c_int = 1;
    // SAFETY: setsockopt with a valid int pointer on the live socket.
    let rc = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_REUSEADDR,
            (&raw const one).cast(),
            size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    let sin = libc::sockaddr_in {
        sin_family: libc::AF_INET as libc::sa_family_t,
        sin_port: addr.port().to_be(),
        sin_addr: libc::in_addr { s_addr: u32::from(*addr.ip()).to_be() },
        sin_zero: [0; 8],
        #[cfg(target_os = "macos")]
        sin_len: 0,
    };
    // SAFETY: sin is a fully initialized sockaddr_in of the stated length.
    let rc = unsafe {
        libc::bind(fd, (&raw const sin).cast(), size_of::<libc::sockaddr_in>() as libc::socklen_t)
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// The port a listener actually bound (port 0 = kernel-assigned; tests).
///
/// # Errors
/// Propagates `getsockname` failure.
pub fn bound_port(listener: &TcpListener) -> io::Result<u16> {
    Ok(listener.local_addr()?.port())
}

/// Cross-thread reactor wake handle (M0-R1 doorbell wakeups): writing the
/// peer cell's eventfd posts a CQE into its ring, ending a park immediately
/// instead of at the park-timeout ceiling. Cloneable and idempotent — the
/// driver's watch drains the counter.
#[cfg(target_os = "linux")]
#[derive(Clone, Debug)]
pub struct LoopWaker {
    fd: std::sync::Arc<std::os::fd::OwnedFd>,
}

#[cfg(target_os = "linux")]
impl LoopWaker {
    /// Wakes the owning cell's reactor if it is (or is about to be) parked.
    pub fn wake(&self) {
        use std::os::fd::AsRawFd;
        let one: u64 = 1;
        // SAFETY: write(2) of 8 bytes from a live stack buffer to an owned
        // eventfd. Errors (EAGAIN = counter saturated) mean the peer is
        // already due to wake — safe to ignore.
        let _ = unsafe { libc::write(self.fd.as_raw_fd(), (&raw const one).cast(), 8) };
    }
}

/// Creates one cell's wake pair: the driver adopts the watch side
/// ([`crate::UringDriver::adopt_wake_fd`]); [`LoopWaker`] clones go to every
/// peer cell's fabric.
///
/// # Errors
/// Propagates `eventfd(2)`/`dup` failure (fd exhaustion).
#[cfg(target_os = "linux")]
pub fn wake_pair() -> io::Result<(std::os::fd::OwnedFd, LoopWaker)> {
    use std::os::fd::FromRawFd as _;
    // SAFETY: plain eventfd(2); the fd is validated then owned below.
    let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fresh fd, owned exclusively here.
    let owned = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) };
    let waker = LoopWaker { fd: std::sync::Arc::new(owned.try_clone()?) };
    Ok((owned, waker))
}

/// Pins the calling thread to `core` (Linux; best-effort no-op elsewhere —
/// the dev tier runs unpinned). A core past `CPU_SETSIZE` is refused
/// typed (batch 61): the value is CLI-supplied and `CPU_SET` would index
/// out of bounds.
///
/// # Errors
/// `InvalidInput` for a core the affinity mask cannot name; the
/// `sched_setaffinity` failure itself is best-effort and not reported.
pub fn pin_current_thread(core: usize) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        let cap = usize::try_from(libc::CPU_SETSIZE).unwrap_or(0);
        if core >= cap {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("core {core} is past CPU_SETSIZE ({cap}) — pin start too high"),
            ));
        }
        // SAFETY: sched_setaffinity on self with a properly built cpu_set_t
        // (`core < CPU_SETSIZE` checked above); failure just leaves the
        // thread unpinned.
        unsafe {
            let mut set: libc::cpu_set_t = core::mem::zeroed();
            libc::CPU_SET(core, &mut set);
            libc::sched_setaffinity(0, size_of::<libc::cpu_set_t>(), &raw const set);
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = core;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Batch 61: the owned-port probe must never *accept* — a connection
    /// arriving inside its window is refused, not accepted-then-reset
    /// (`TcpListener::bind` listens; a bound socket does not).
    #[test]
    fn the_port_probe_never_accepts_a_connection() {
        use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
        let port = bound_port(&listen_reuseport(0).expect("pick")).expect("port");
        // The picker's listener is dropped with the temporary above; the
        // port is free now.
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let accepted = std::sync::Arc::new(AtomicU32::new(0));
        let hammer = {
            let (stop, accepted) = (std::sync::Arc::clone(&stop), std::sync::Arc::clone(&accepted));
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                        accepted.fetch_add(1, Ordering::Relaxed);
                    }
                }
            })
        };
        for _ in 0..20_000 {
            probe_port_unowned(port).expect("free port");
        }
        stop.store(true, Ordering::Relaxed);
        hammer.join().expect("hammer");
        assert_eq!(accepted.load(Ordering::Relaxed), 0, "the probe accepted connections");
    }

    /// Batch 61 (lane L11 `net.rs:122-123`): a core past `CPU_SETSIZE`
    /// (CLI-supplied) is a typed refusal, not `CPU_SET`'s index panic.
    /// Linux only: pinning is a documented no-op elsewhere.
    #[cfg(target_os = "linux")]
    #[test]
    fn pin_refuses_a_core_past_cpu_setsize() {
        let err = pin_current_thread(usize::MAX).expect_err("out of range");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{err}");
        assert!(err.to_string().contains("CPU_SETSIZE"), "{err}");
    }

    /// Batch 59: a reuseport group already listening on the port is
    /// exactly what a second node would silently join; the probe refuses
    /// it, names the port, and leaves the group untouched.
    #[test]
    fn a_port_a_listener_group_owns_is_refused() {
        let group_head = listen_reuseport(0).expect("listen");
        let port = bound_port(&group_head).expect("port");
        let err = probe_port_unowned(port).expect_err("an owned port is refused");
        assert_eq!(err.kind(), io::ErrorKind::AddrInUse);
        assert!(err.to_string().contains(&port.to_string()), "{err}");
        // The probe took nothing: the group still accepts a joiner.
        let joiner = listen_reuseport(port).expect("own cells still join");
        drop((joiner, group_head));
        probe_port_unowned(0).expect("port 0 is never probed");
    }

    /// ADR-0123 D3: the keepalive quartet lands on a live TCP socket and
    /// `0` clears it (read back with `getsockopt`).
    #[test]
    #[cfg(target_os = "linux")]
    fn keepalive_is_set_and_cleared_on_a_tcp_socket() {
        use std::os::fd::AsRawFd;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let client = std::net::TcpStream::connect(listener.local_addr().expect("addr")).expect("c");
        let (server, _) = listener.accept().expect("accept");
        let fd = server.as_raw_fd();
        let get = |level: libc::c_int, opt: libc::c_int| -> libc::c_int {
            let mut v: libc::c_int = 0;
            let mut len = size_of::<libc::c_int>() as libc::socklen_t;
            // SAFETY: getsockopt into a valid int of the stated length.
            let rc = unsafe { libc::getsockopt(fd, level, opt, (&raw mut v).cast(), &raw mut len) };
            assert_eq!(rc, 0, "getsockopt {opt}");
            v
        };
        set_keepalive(fd, 60).expect("set");
        assert_eq!(get(libc::SOL_SOCKET, libc::SO_KEEPALIVE), 1);
        assert_eq!(get(libc::IPPROTO_TCP, libc::TCP_KEEPIDLE), 60);
        assert_eq!(get(libc::IPPROTO_TCP, libc::TCP_KEEPINTVL), 20);
        assert_eq!(get(libc::IPPROTO_TCP, libc::TCP_KEEPCNT), 3);
        set_keepalive(fd, 0).expect("clear");
        assert_eq!(get(libc::SOL_SOCKET, libc::SO_KEEPALIVE), 0);
        assert!(set_keepalive(-1, 60).is_err(), "a bad fd reports its error");
        drop(client);
    }

    #[test]
    fn two_listeners_share_a_port() {
        let a = listen_reuseport(0).expect("first");
        let port = bound_port(&a).expect("port");
        let b = listen_reuseport(port).expect("second on same port");
        assert_eq!(bound_port(&b).expect("port"), port);
    }
}
