//! A test ring's selections, and the check that the ring engaged them
//! (ADR-0209 D2, D5). Each `inf-runtime` test target that builds a ring
//! includes this module once, by `#[path]`; it is no target of its own.
//!
//! [`config`] decodes the test's own environment where the ring is built,
//! so a ring's mode is always the variable's answer: the CI kernel-matrix
//! job's degraded leg sets `INF_URING_FORCE_DEGRADED`, and every test ring
//! runs degraded there. A test adjusts the setup chain or the submit policy
//! with the config's overrides; nothing overrides the mode.
//!
//! [`assert_engaged`] reads the variable itself, not through the decoder,
//! and checks the ring's capabilities against it, so a decoder that dropped
//! the variable is red on the degraded leg and one that always degraded is
//! red on the probed leg. The ring census in `scripts/check-node-spawn.sh`
//! requires the call beside every ring a test file builds.

use inf_runtime::{Capabilities, UringConfig};

/// The kernel release from which the driver gives multishot accept to a
/// probed ring (its version gate).
const MULTISHOT_ACCEPT_FROM: (u32, u32) = (5, 19);

/// The selections a test ring is built with: the decoder over the test's
/// environment, read once, where the ring is built.
#[allow(clippy::disallowed_methods, reason = "test-only: the test's environment, not cell code")]
pub fn config() -> UringConfig {
    UringConfig::from_lookup(std::env::var_os)
}

/// Whether the test's environment selects the degraded mode, read from the
/// variable itself, not through the decoder.
#[allow(clippy::disallowed_methods, reason = "test-only: the test's environment, not cell code")]
pub fn degraded_selected() -> bool {
    std::env::var_os("INF_URING_FORCE_DEGRADED").is_some()
}

/// Checks that `caps`, a test ring's, engaged the mode the environment
/// selects. With `INF_URING_FORCE_DEGRADED` present the ring is degraded:
/// no multishot accept, no multishot recv. Absent, a kernel at 5.19 or
/// later gives multishot accept.
pub fn assert_engaged(caps: &Capabilities) {
    if degraded_selected() {
        assert!(!caps.multishot_accept, "a degraded leg's ring accepts multishot: {caps:?}");
        assert!(!caps.multishot_recv, "a degraded leg's ring receives multishot: {caps:?}");
    } else if kernel_release() >= MULTISHOT_ACCEPT_FROM {
        assert!(caps.multishot_accept, "a probed ring here lacks multishot accept: {caps:?}");
    }
}

/// `major.minor` of the running kernel, from `/proc/sys/kernel/osrelease`.
#[allow(clippy::disallowed_methods, reason = "test-only: a kernel fact, not cell code")]
fn kernel_release() -> (u32, u32) {
    let text = std::fs::read_to_string("/proc/sys/kernel/osrelease").expect("the kernel release");
    let mut parts = text.trim().split(['.', '-']);
    let mut next = || parts.next().and_then(|p| p.parse::<u32>().ok()).expect("a release number");
    (next(), next())
}
