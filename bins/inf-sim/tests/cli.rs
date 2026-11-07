//! The simulator's usage refusals: a flag that names another scenario's leg
//! exits 2 before any run, with the line that says whose leg it is.

use std::process::Command;

/// The accept-broken plant is the accept-retry oracle's leg, `m0-smoke`'s
/// alone. Off it the binary refuses before any runner starts: on a scenario
/// of the shared runner, on one with its own runner, and on one whose leg
/// flag dispatches early.
#[test]
fn accept_broken_is_refused_off_m0_smoke() {
    let scenarios: [&[&str]; 3] = [
        &["--scenario", "m1-cache"],
        &["--scenario", "m2-durable"],
        &["--scenario", "m4-extent-window", "--window-leg", "burst"],
    ];
    for scenario in scenarios {
        let output = Command::new(env!("CARGO_BIN_EXE_inf-sim"))
            .args(scenario)
            .args(["--plant", "accept-broken"])
            .output()
            .expect("the simulator binary runs");
        assert_eq!(output.status.code(), Some(2), "{scenario:?}: {output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("--plant accept-broken is m0-smoke's"),
            "{scenario:?}: refusal: {stderr}"
        );
        assert!(output.stdout.is_empty(), "{scenario:?}: a run started: {output:?}");
    }
}
