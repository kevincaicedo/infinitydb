//! The single-seed driver's plants and controls: a twin whose state hash
//! differs, a twin whose readings differ (a stub, and the durable family's
//! own report), and twins that agree. A single run prints its lines, so each
//! case runs this test binary again on itself in a child process and reads
//! the child's output.

use std::cell::Cell;
use std::process::{Command, Output};

use super::*;
use crate::{DurableReport, WakeReport};

/// A report whose every field the plant sets.
#[derive(Clone, Debug)]
struct Stub {
    trace_hash: u64,
    state_hash: u64,
    violations: Violations,
    readings: u64,
}

impl Stub {
    fn clean(seed: u64) -> Stub {
        Stub { trace_hash: seed, state_hash: !seed, violations: Violations::new(), readings: 0 }
    }
}

impl SeedReport for Stub {
    type Readings = u64;

    fn trace_hash(&self) -> u64 {
        self.trace_hash
    }
    fn state_hash(&self) -> u64 {
        self.state_hash
    }
    fn verdict(&self) -> Verdict {
        Verdict::judge(self.violations.is_empty(), false, &self.violations)
    }
    fn violations(&self) -> &Violations {
        &self.violations
    }
    fn trace(&self) -> Option<&[u8]> {
        None
    }
    fn sim_seconds(&self) -> Option<SimSeconds> {
        Some(SimSeconds { seconds: 0.5, published: 1, delivered: 1 })
    }
    fn readings(&self) -> u64 {
        self.readings
    }
}

/// Two durable reports that differ in their wake readings alone: the
/// durable family's own report type, as its runs return it.
fn durable_twins() -> impl FnMut(u64) -> DurableReport {
    let calls = Cell::new(0u64);
    move |seed: u64| {
        calls.set(calls.get() + 1);
        DurableReport {
            trace_hash: seed,
            state_hash: !seed,
            wake: WakeReport { held_parks: 2 + calls.get(), ..WakeReport::default() },
            ..DurableReport::default()
        }
    }
}

/// Runs this test binary again on `test` alone, with `env` set, and returns
/// its output.
fn child(test: &str, env: &[(&str, &str)]) -> Output {
    Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture", "--test-threads=1"])
        .envs(env.iter().copied())
        .output()
        .unwrap()
}

const SINGLE: &str = "INF_SIM_DRIVER_SINGLE_CHILD";
const SINGLE_TEST: &str = "driver::tests::a_single_run_reads_its_twin";

/// A run of clean stubs that `plant` changes on each call, the call's
/// number beside it.
fn stub(plant: fn(&mut Stub, u64)) -> impl FnMut(u64) -> Stub {
    let mut calls = 0u64;
    move |seed: u64| {
        calls += 1;
        let mut report = Stub::clean(seed);
        plant(&mut report, calls);
        report
    }
}

/// The child's run for each leg the parent names.
fn single_leg(leg: &str) -> Exit {
    match leg {
        "state" => run_single(7, Verify::Twin, stub(|r, call| r.state_hash ^= call), |_| Ok(())),
        "readings" => run_single(7, Verify::Twin, stub(|r, call| r.readings = call), |_| Ok(())),
        "agree" => run_single(7, Verify::Twin, stub(|_, _| {}), |_| Ok(())),
        "durable" => run_single(7, Verify::Twin, durable_twins(), |_| Ok(())),
        "violated" => {
            let run_one = stub(|r, _| r.violations.push("planted: a key lost".to_owned()));
            run_single(7, Verify::Twin, run_one, |_| Ok(()))
        }
        _ => Exit::Usage,
    }
}

/// A twin whose state hash differs exits 1 with the divergence line and no
/// agreement line; one whose readings differ, a stub's or the durable
/// family's wake readings, is a divergence too; twins that agree print the
/// agreement line and exit by the first run's verdict.
#[test]
fn a_single_run_reads_its_twin() {
    if let Some(leg) = std::env::var_os(SINGLE) {
        std::process::exit(single_leg(leg.to_str().unwrap()).code());
    }
    let run = |leg: &str| {
        let output = child(SINGLE_TEST, &[(SINGLE, leg)]);
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        (output.status.code(), stdout, stderr)
    };
    let (code, stdout, stderr) = run("state");
    assert_eq!(code, Some(1), "{stdout}{stderr}");
    assert!(stderr.contains("DETERMINISM VIOLATION — state_hash differs"), "{stderr}");
    assert!(!stdout.contains("determinism: agreed"), "{stdout}");

    let (code, stdout, stderr) = run("readings");
    assert_eq!(code, Some(1), "{stdout}{stderr}");
    assert!(stderr.contains("DETERMINISM VIOLATION — readings differ (1 vs 2)"), "{stderr}");
    assert!(!stdout.contains("determinism: agreed"), "{stdout}");

    let (code, stdout, stderr) = run("durable");
    assert_eq!(code, Some(1), "{stdout}{stderr}");
    assert!(
        stderr.contains(
            "DETERMINISM VIOLATION — readings differ (WakeReport { parks: 0, held_parks: 3"
        ),
        "{stderr}"
    );

    let (code, stdout, stderr) = run("agree");
    assert_eq!(code, Some(0), "{stdout}{stderr}");
    assert!(
        stdout.contains(
            "inf-sim: determinism: agreed (state_hash=0xfffffffffffffff8, \
             trace_hash=0x0000000000000007)"
        ),
        "{stdout}"
    );

    let (code, stdout, stderr) = run("violated");
    assert_eq!(code, Some(1), "{stdout}{stderr}");
    assert!(stderr.contains("inf-sim: ORACLE VIOLATION: planted: a key lost"), "{stderr}");
    assert!(stdout.contains("inf-sim: determinism: agreed"), "the twin reproduced it: {stdout}");
}
