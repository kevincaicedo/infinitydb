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

/// Runs the binary with `args` and reads its usage refusal: exit 2, `line`
/// on stderr, nothing on stdout.
fn refused(args: &[&str], line: &str) {
    let output =
        Command::new(env!("CARGO_BIN_EXE_inf-sim")).args(args).output().expect("the binary runs");
    assert_eq!(output.status.code(), Some(2), "{args:?}: {output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(line), "{args:?}: refusal: {stderr}");
    assert!(output.stdout.is_empty(), "{args:?}: a run started: {output:?}");
}

/// `--cold-leg` is `m4-cold`'s alone. Off it the binary refuses before any
/// runner starts, an early-dispatched leg included.
#[test]
fn a_cold_leg_is_refused_off_m4_cold() {
    refused_off_leg(&["--cold-leg", "dense"], "--cold-leg is m4-cold's");
}

/// An unknown `--cold-leg` is refused with the names of the legs.
#[test]
fn an_unknown_cold_leg_is_refused_with_the_legs() {
    let legs = "the legs are storm|cancel-all|held|deep-queue|stride|dense|errors|identity|stop";
    refused(&["--scenario", "m4-cold", "--cold-leg", "swarm"], legs);
}

/// `--cold-identity-start` is the identity leg's: refused with no leg or
/// another leg, and at or above the issuer's 2^55 pairs; the identity leg
/// refuses to run without it.
#[test]
fn the_identity_start_is_the_identity_legs_alone() {
    let not_identity = "--cold-identity-start is the identity leg's";
    refused(&["--scenario", "m4-cold", "--cold-identity-start", "7"], not_identity);
    let dense = ["--scenario", "m4-cold", "--cold-leg", "dense", "--cold-identity-start", "7"];
    refused(&dense, not_identity);
    let bare = ["--scenario", "m4-cold", "--cold-leg", "identity"];
    refused(&bare, "--cold-leg identity needs --cold-identity-start N");
    let above = ["--scenario", "m4-cold", "--cold-leg", "identity", "--cold-identity-start"];
    refused(&[&above[..], &["0x80000000000000"]].concat(), "a pair position below 2^55");
}

/// `--ops` sizes the storm alone: every other leg's shape is fixed.
#[test]
fn ops_is_refused_on_a_fixed_cold_leg() {
    let args = ["--scenario", "m4-cold", "--cold-leg", "dense", "--ops", "10"];
    refused(&args, "--ops sizes the storm; the dense leg's shape is fixed");
}
