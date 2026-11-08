use std::process::Command;

/// A single run under the flag runs the twin and prints its one agreement
/// line, which names both hashes.
#[test]
fn a_single_run_under_the_flag_prints_the_twin_agreement_line() {
    let output = Command::new(env!("CARGO_BIN_EXE_inf-sim"))
        .args([
            "--scenario",
            "m0-smoke",
            "--seed",
            "0xC0FFEE",
            "--commands",
            "100",
            "--verify-determinism",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("inf-sim: determinism: agreed (state_hash=0x"),
        "no agreement line: {stdout}"
    );
    assert!(stdout.contains(", trace_hash=0x"), "the trace hash is not named: {stdout}");
}

/// Exit 2 with `message` on stderr and nothing run. An exit 2 for another
/// reason (a missing leg, an unknown flag) is red, since the message is
/// read too.
fn assert_refused(args: &[&str], message: &str) {
    let output = Command::new(env!("CARGO_BIN_EXE_inf-sim")).args(args).output().unwrap();
    assert_eq!(output.status.code(), Some(2), "{args:?}: {output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(message), "{args:?}: refused for another reason: {stderr}");
    assert!(output.stdout.is_empty(), "{args:?}: a run started: {output:?}");
}

/// A sweep's arguments are checked where they are parsed: a zero seed
/// count, a shard outside its count, and `--shard` or `--out` without
/// `--sweep` each exit 2 with their own message before any run starts.
#[test]
fn a_bad_sweep_argument_is_refused_at_the_parse() {
    let unused = std::env::temp_dir().join("inf-sim-out-never-created");
    let unused = unused.to_str().unwrap();
    for (extra, message) in [
        (&["--sweep", "0"][..], "a sweep runs at least 1 seed"),
        (&["--sweep", "2", "--shard", "2/2"], "I is a shard index, below K"),
        (&["--sweep", "2", "--shard", "0/0"], "K is the shard count, at least 1"),
        (&["--shard", "0/2"], "--shard needs --sweep"),
        (&["--out", unused], "--out needs --sweep"),
    ] {
        let mut args = vec!["--scenario", "m2-durable"];
        args.extend(extra);
        assert_refused(&args, message);
    }
}

/// A sweep runs every seed twice, so `--verify-determinism` beside
/// `--sweep` is a usage error: its absence would read as once.
#[test]
fn a_sweep_refuses_the_flag() {
    assert_refused(
        &["--scenario", "m2-durable", "--sweep", "1", "--verify-determinism"],
        "--verify-determinism is a single run's: a sweep runs every seed twice",
    );
}

/// A scenario that cannot sweep refuses `--sweep` before any run, and the
/// message names the scenarios that can.
#[test]
#[ignore = "eight scenarios run one seed under --sweep N and exit 0, and the refusal names \
            three of the scenarios that sweep; red until --sweep is refused off the registry's \
            sweep column"]
fn a_scenario_that_cannot_sweep_refuses_a_sweep() {
    for scenario in [
        &["--scenario", "m4-steel"][..],
        &["--scenario", "m45-backfill"],
        &["--scenario", "m45-sidecar"],
        &["--scenario", "m4-diskfull"],
        &["--scenario", "m4-pressure"],
        &["--scenario", "m4-cold"],
        &["--scenario", "boot-storm"],
        &["--scenario", "m4-extent-window", "--window-leg", "burst"],
        &["--scenario", "m0-smoke"],
    ] {
        let mut args = scenario.to_vec();
        args.extend(["--sweep", "1"]);
        let refusal = format!("--sweep is not {}'s: the scenarios that sweep are", scenario[1]);
        assert_refused(&args, &refusal);
    }
}
