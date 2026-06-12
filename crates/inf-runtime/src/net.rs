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
