//! The determinism twin at the binary, and the sweep arguments' refusals,
//! read off the scenario registry. A single run under the flag prints the
//! twin's agreement line; a row outside the sweep column refuses `--sweep`;
//! a sweep-capable row refuses a bad seed count or shard at the parse, the
//! flag beside `--sweep`, and `--shard` or `--out` without it. No test here
//! runs a sweep: the release build sweeps every sweep-capable row.

use std::process::Command;

use inf_sim::{SCENARIOS, ScenarioRow, Sweeps};

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

/// A row's leg flag and first leg name: a row whose leg is required cannot
/// run without them, and one whose leg has a default runs the named leg.
fn leg_args(row: &ScenarioRow) -> Vec<&'static str> {
    row.leg.kind().map(|leg| vec![leg.flag(), leg.names()[0]]).unwrap_or_default()
}

/// The command line `--scenario <row> [its first leg] <extra>`.
fn row_args<'a>(row: &'static ScenarioRow, extra: &[&'a str]) -> Vec<&'a str> {
    let mut args = vec!["--scenario", row.name];
    args.extend(leg_args(row));
    args.extend(extra);
    args
}

fn rows(sweeps: Sweeps) -> Vec<&'static ScenarioRow> {
    let rows: Vec<_> = SCENARIOS.iter().filter(|row| row.sweeps == sweeps).collect();
    assert!(!rows.is_empty(), "the registry has no {sweeps:?} row");
    rows
}

/// Every sweep-capable row's arguments are checked where they are parsed:
/// a zero seed count, a shard outside its count, and `--shard` or `--out`
/// without `--sweep` each exit 2 with their own message before any run
/// starts. Each row runs with its first leg, so a leg column that named the
/// wrong flag would read its leg refusal here instead.
#[test]
fn a_bad_sweep_argument_is_refused_at_the_parse() {
    let unused = std::env::temp_dir().join("inf-sim-out-never-created");
    let unused = unused.to_str().unwrap();
    for row in rows(Sweeps::Yes) {
        for (extra, message) in [
            (&["--sweep", "0"][..], "a sweep runs at least 1 seed"),
            (&["--sweep", "2", "--shard", "2/2"], "I is a shard index, below K"),
            (&["--sweep", "2", "--shard", "0/0"], "K is the shard count, at least 1"),
            (&["--shard", "0/2"], "--shard needs --sweep"),
            (&["--out", unused], "--out needs --sweep"),
        ] {
            assert_refused(&row_args(row, extra), message);
        }
    }
}

/// A sweep runs every seed twice, so `--verify-determinism` beside
/// `--sweep` is a usage error on every sweep-capable row: its absence would
/// read as once.
#[test]
fn a_sweep_refuses_the_flag() {
    for row in rows(Sweeps::Yes) {
        assert_refused(
            &row_args(row, &["--sweep", "1", "--verify-determinism"]),
            "--verify-determinism is a single run's: a sweep runs every seed twice",
        );
    }
}

/// Every row outside the sweep column refuses `--sweep` before any run, and
/// the message names the column.
#[test]
fn a_scenario_that_cannot_sweep_refuses_a_sweep() {
    for row in rows(Sweeps::No) {
        let refusal =
            format!("--sweep is not {}'s: the scenarios that sweep are m2-durable", row.name);
        assert_refused(&row_args(row, &["--sweep", "1"]), &refusal);
    }
}

/// `--list-sweeps` prints the sweep column, one line a row: its name and,
/// for a legged row, its leg flag and first leg.
#[test]
fn the_sweep_listing_is_the_sweep_column() {
    let output = Command::new(env!("CARGO_BIN_EXE_inf-sim")).arg("--list-sweeps").output().unwrap();
    assert!(output.status.success(), "{output:?}");
    let listed = String::from_utf8_lossy(&output.stdout);
    let column: Vec<String> =
        rows(Sweeps::Yes).into_iter().map(|row| row_args(row, &[])[1..].join(" ")).collect();
    assert_eq!(listed.lines().collect::<Vec<_>>(), column);
}
