//! Termination signals (ADR-0124 D1): `SIGTERM`/`SIGINT` set one
//! process-wide flag the assembly polls each iteration. The handler does
//! one async-signal-safe thing — an atomic store. `SA_RESETHAND` restores
//! the default disposition after the first delivery, so a second signal
//! terminates the process at once (the operator's escalation).
#![allow(unsafe_code)]

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_stop_signal(_signo: libc::c_int) {
    STOP.store(true, Ordering::Release);
}

/// Installs the handler for `SIGTERM` and `SIGINT` and returns the flag
/// it sets. Idempotent; the flag is process-wide.
///
/// # Errors
/// `sigaction` failing (EINVAL — never for these two signals).
pub fn install_stop_flag() -> io::Result<&'static AtomicBool> {
    for signo in [libc::SIGTERM, libc::SIGINT] {
        // SAFETY: a zeroed `sigaction` is a valid all-default value; the
        // handler is a plain `extern "C" fn(c_int)` stored through the
        // `sa_sigaction` field as libc expects on every supported target;
        // the old-action pointer is null (not requested). The handler
        // touches only a static atomic — async-signal-safe.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction =
                on_stop_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
            action.sa_flags = libc::SA_RESETHAND | libc::SA_RESTART;
            libc::sigemptyset(&raw mut action.sa_mask);
            if libc::sigaction(signo, &raw const action, std::ptr::null_mut()) != 0 {
                return Err(io::Error::last_os_error());
            }
        }
    }
    Ok(&STOP)
}

/// True once a termination signal was delivered.
#[must_use]
pub fn stop_requested() -> bool {
    STOP.load(Ordering::Acquire)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The handler stores the flag: raise `SIGTERM` at ourselves after
    /// installing (the test process, not a cell; the second delivery
    /// would be default — this test raises exactly once).
    #[test]
    fn sigterm_sets_the_flag_once_installed() {
        let flag = install_stop_flag().expect("sigaction");
        assert!(!flag.load(Ordering::Acquire));
        // SAFETY: `raise` with a valid signal number; the handler above is
        // installed for it and only stores an atomic.
        let rc = unsafe { libc::raise(libc::SIGTERM) };
        assert_eq!(rc, 0);
        assert!(stop_requested(), "the handler did not run");
    }
}
