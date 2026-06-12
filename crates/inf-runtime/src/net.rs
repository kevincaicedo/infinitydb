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
/// the dev tier runs unpinned).
pub fn pin_current_thread(core: usize) {
    #[cfg(target_os = "linux")]
    // SAFETY: sched_setaffinity on self with a properly built cpu_set_t;
    // failure just leaves the thread unpinned.
    unsafe {
        let mut set: libc::cpu_set_t = core::mem::zeroed();
        libc::CPU_SET(core, &mut set);
        libc::sched_setaffinity(0, size_of::<libc::cpu_set_t>(), &raw const set);
    }
    #[cfg(not(target_os = "linux"))]
    let _ = core;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_listeners_share_a_port() {
        let a = listen_reuseport(0).expect("first");
        let port = bound_port(&a).expect("port");
        let b = listen_reuseport(port).expect("second on same port");
        assert_eq!(bound_port(&b).expect("port"), port);
    }
}
