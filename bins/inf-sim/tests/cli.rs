//! The simulator's usage refusals: a flag that names another scenario's leg
//! exits 2 before any run, with the line that says whose leg it is.

use std::process::Command;

/// A scenario of the shared runner, one with its own runner, and one whose
/// leg flag dispatches early: a refusal must stop each before it runs.
const OFF_LEG: [&[&str]; 3] = [
    &["--scenario", "m1-cache"],
    &["--scenario", "m2-durable"],
    &["--scenario", "m4-extent-window", "--window-leg", "burst"],
];

/// Runs the binary on each of [`OFF_LEG`] with `flags` and reads the
/// refusal: exit 2, `line` on stderr, nothing on stdout.
fn refused_off_leg(flags: &[&str], line: &str) {
    for scenario in OFF_LEG {
        let output = Command::new(env!("CARGO_BIN_EXE_inf-sim"))
            .args(scenario)
            .args(flags)
            .output()
            .expect("the simulator binary runs");
        assert_eq!(output.status.code(), Some(2), "{scenario:?}: {output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(line), "{scenario:?}: refusal: {stderr}");
        assert!(output.stdout.is_empty(), "{scenario:?}: a run started: {output:?}");
    }
}

/// The accept-broken plant is the accept-retry oracle's leg, `m0-smoke`'s
/// alone. Off it the binary refuses before any runner starts.
#[test]
fn accept_broken_is_refused_off_m0_smoke() {
    refused_off_leg(&["--plant", "accept-broken"], "--plant accept-broken is m0-smoke's");
}

/// The park-at-once leg is the wake oracle's, `m2-fill-tick`'s and
/// `m2-group-hold`'s alone. Off them the binary refuses before any runner
/// starts, an early-dispatched leg included, which would otherwise run with
/// the flag ignored.
#[test]
fn park_at_once_is_refused_off_the_hold_scenarios() {
    refused_off_leg(&["--park-at-once"], "--park-at-once is m2-fill-tick's and m2-group-hold's");
}
