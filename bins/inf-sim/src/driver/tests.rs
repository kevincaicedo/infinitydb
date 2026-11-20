//! The drivers' plants and controls: a twin whose state hash differs, a
//! violation both runs report, a twin whose readings differ (a stub, and the
//! durable family's own report), a shard-end clause that fires, a shard's
//! stderr bound with and without `--out`, and their controls. Each sweep
//! case reads the shard's own files, as the runner does; a case that reads
//! printed lines runs this test binary again on itself in a child process.

use std::cell::Cell;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
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

/// A fold that counts what it absorbed and fires the clause it was given.
#[derive(Default)]
struct StubFold {
    absorbed: u64,
    clause: Option<&'static str>,
}

impl Fold for StubFold {
    type Report = Stub;
    fn absorb(&mut self, _report: &Stub) {
        self.absorbed += 1;
    }
    fn coverage(&self) -> String {
        format!("{} absorbed", self.absorbed)
    }
    fn manifest_tail(&self) -> String {
        format!("absorbed={}", self.absorbed)
    }
    fn shard_end_clauses(&self) -> Vec<String> {
        self.clause.map(str::to_owned).into_iter().collect()
    }
}

/// The durable family's own report through the sweep driver.
#[derive(Default)]
struct DurableStubFold;

impl Fold for DurableStubFold {
    type Report = DurableReport;
    fn absorb(&mut self, _report: &DurableReport) {}
    fn coverage(&self) -> String {
        String::new()
    }
    fn manifest_tail(&self) -> String {
        String::new()
    }
    fn shard_end_clauses(&self) -> Vec<String> {
        Vec::new()
    }
}

/// A directory of the test's own under the system's temporary directory,
/// empty at the start.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("inf-sim-driver-{}-{name}", std::process::id()));
    remove(&dir);
    dir
}

/// Removes a directory [`scratch`] named, and nothing else.
fn remove(dir: &Path) {
    let leaf = dir.file_name().and_then(|leaf| leaf.to_str()).unwrap_or("");
    assert!(leaf.starts_with("inf-sim-driver-"), "not a scratch directory: {}", dir.display());
    let _ = std::fs::remove_dir_all(dir);
}

fn sweep_of(seeds: u64, out: Option<&Path>) -> Sweep {
    Sweep::new(0x100, NonZeroU64::new(seeds).unwrap(), Shard::WHOLE, out.map(Path::to_path_buf))
}

fn read(path: PathBuf) -> String {
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// A twin whose state hash differs is a violation that names its seed; the
/// shard goes on to the next seed, and `seeds_verified` counts twin runs.
#[test]
fn a_diverging_twin_is_a_violation_that_names_its_seed() {
    let out = scratch("diverged");
    let calls = Cell::new(0u64);
    let run_one = |seed: u64| {
        calls.set(calls.get() + 1);
        let mut report = Stub::clean(seed);
        // The first seed's second run differs in its state hash alone.
        if seed == 0x100 && calls.get() == 2 {
            report.state_hash ^= 1;
        }
        report
    };
    let exit = run_sweep("stub", &sweep_of(2, Some(&out)), StubFold::default(), run_one);
    assert_eq!(exit, Exit::Failed);
    let results = read(out.join("results-shard-0.txt"));
    let lines: Vec<&str> = results.lines().collect();
    assert_eq!(lines.len(), 2, "{results}");
    assert!(lines[0].starts_with("0x100 VIOLATION determinism: state_hash differs"), "{results}");
    assert_eq!(lines[1], "0x101 ok", "the next seed ran: {results}");
    let manifest = read(out.join("manifest-shard-0.txt"));
    assert!(
        manifest.contains(
            " seeds_run=2 seeds_verified=2 seeds_diverged=1 violations=1 refused=0 absorbed=2"
        ),
        "{manifest}"
    );
    assert_eq!(calls.get(), 2 * 2, "seeds_verified counts the twin runs");
    remove(&out);
}

/// A violation both runs report is the seed's results line, and the shard
/// goes on: no panic, no lost manifest.
#[test]
fn a_violation_the_twin_reproduces_is_written_and_the_shard_goes_on() {
    let out = scratch("violated");
    let run_one = |seed: u64| {
        let mut report = Stub::clean(seed);
        if seed == 0x100 {
            report.violations.push("planted: a key lost".to_owned());
            report.violations.push("second".to_owned());
        }
        report
    };
    let exit = run_sweep("stub", &sweep_of(2, Some(&out)), StubFold::default(), run_one);
    assert_eq!(exit, Exit::Failed);
    let results = read(out.join("results-shard-0.txt"));
    assert_eq!(results, "0x100 VIOLATION planted: a key lost\n0x101 ok\n");
    let manifest = read(out.join("manifest-shard-0.txt"));
    assert!(manifest.contains(" seeds_verified=2 seeds_diverged=0 violations=1 "), "{manifest}");
    remove(&out);
}

/// A violation the twin does not reproduce is a divergence: the verdict is
/// part of what the twin compares.
#[test]
fn a_violation_the_twin_does_not_reproduce_is_a_divergence() {
    let out = scratch("unreproduced");
    let calls = Cell::new(0u64);
    let run_one = |seed: u64| {
        calls.set(calls.get() + 1);
        let mut report = Stub::clean(seed);
        if calls.get() == 1 {
            report.violations.push("once".to_owned());
        }
        report
    };
    let exit = run_sweep("stub", &sweep_of(1, Some(&out)), StubFold::default(), run_one);
    assert_eq!(exit, Exit::Failed);
    let results = read(out.join("results-shard-0.txt"));
    assert!(results.starts_with("0x100 VIOLATION determinism: verdict differs"), "{results}");
    remove(&out);
}

/// Readings that differ while the hashes and the verdict agree are a
/// divergence in a sweep too, for a stub and for the durable family's own
/// report (its wake readings); equal readings agree.
#[test]
fn a_sweep_twin_compares_the_readings() {
    let out = scratch("readings");
    let run_one = stub(|report, call| report.readings = call);
    let exit = run_sweep("stub", &sweep_of(2, Some(&out)), StubFold::default(), run_one);
    assert_eq!(exit, Exit::Failed);
    let results = read(out.join("results-shard-0.txt"));
    let lines: Vec<&str> = results.lines().collect();
    assert_eq!(lines[0], "0x100 VIOLATION determinism: readings differ (1 vs 2)", "{results}");
    assert_eq!(
        lines[1], "0x101 VIOLATION determinism: readings differ (3 vs 4)",
        "the next seed ran"
    );
    let manifest = read(out.join("manifest-shard-0.txt"));
    assert!(manifest.contains(" seeds_diverged=2 violations=2 "), "{manifest}");

    let exit = run_sweep("durable", &sweep_of(1, Some(&out)), DurableStubFold, durable_twins());
    assert_eq!(exit, Exit::Failed);
    let results = read(out.join("results-shard-0.txt"));
    assert!(
        results.starts_with(
            "0x100 VIOLATION determinism: readings differ (WakeReport { parks: 0, held_parks: 3"
        ),
        "{results}"
    );
    let manifest = read(out.join("manifest-shard-0.txt"));
    assert!(manifest.contains(" seeds_diverged=1 violations=1 "), "{manifest}");

    let exit = run_sweep("stub", &sweep_of(2, Some(&out)), StubFold::default(), Stub::clean);
    assert_eq!(exit, Exit::Clean, "equal readings agree");
    remove(&out);
}

/// A fired shard-end clause is one violation in the manifest and one
/// `shard VIOLATION` line; the exit status follows the manifest. The
/// control: the same shard with the clause holding reads clean.
#[test]
fn a_fired_shard_end_clause_is_counted_before_the_manifest() {
    let out = scratch("clause");
    let fold = StubFold { clause: Some("ARM VACUOUS across the shard"), ..StubFold::default() };
    let exit = run_sweep("stub", &sweep_of(2, Some(&out)), fold, Stub::clean);
    assert_eq!(exit, Exit::Failed);
    let results = read(out.join("results-shard-0.txt"));
    assert_eq!(results.lines().last(), Some("shard VIOLATION ARM VACUOUS across the shard"));
    let manifest = read(out.join("manifest-shard-0.txt"));
    assert!(manifest.contains(" seeds_verified=2 seeds_diverged=0 violations=1 "), "{manifest}");

    let exit = run_sweep("stub", &sweep_of(2, Some(&out)), StubFold::default(), Stub::clean);
    assert_eq!(exit, Exit::Clean);
    assert_eq!(read(out.join("results-shard-0.txt")), "0x100 ok\n0x101 ok\n");
    let manifest = read(out.join("manifest-shard-0.txt"));
    assert!(manifest.contains(" violations=0 refused=0 absorbed=2\n"), "{manifest}");
    remove(&out);
}

/// A reused `--out` never shows an earlier run's verdict: the old manifest
/// goes before the first seed runs.
#[test]
fn a_reused_out_loses_the_earlier_manifest_before_the_first_seed() {
    let out = scratch("reused");
    std::fs::create_dir_all(&out).unwrap();
    std::fs::write(out.join("manifest-shard-0.txt"), "violations=0 stale\n").unwrap();
    std::fs::write(out.join("results-shard-0.txt"), "stale\n").unwrap();
    let manifest = out.join("manifest-shard-0.txt");
    let run_one = |seed: u64| {
        assert!(!manifest.exists(), "the old manifest is still there while seeds run");
        Stub::clean(seed)
    };
    let exit = run_sweep("stub", &sweep_of(1, Some(&out)), StubFold::default(), run_one);
    assert_eq!(exit, Exit::Clean);
    assert_eq!(read(out.join("results-shard-0.txt")), "0x100 ok\n");
    remove(&out);
}

/// The partition: shard `I/K` runs `base + j` for `j ≡ I (mod K)` below the
/// seed count, so the shards cover the request once.
#[test]
fn the_shards_cover_the_request_once() {
    let seeds = NonZeroU64::new(10).unwrap();
    let mut covered = Vec::new();
    for index in 0..4 {
        let shard = Shard::parse(&format!("{index}/4")).unwrap();
        covered.extend(Sweep::new(0x100, seeds, shard, None).shard_seeds());
    }
    covered.sort_unstable();
    assert_eq!(covered, (0x100..0x10a).collect::<Vec<u64>>());
    let one = Sweep::new(u64::MAX, NonZeroU64::new(2).unwrap(), Shard::WHOLE, None);
    assert_eq!(one.shard_seeds().collect::<Vec<_>>(), [u64::MAX, 0], "the base wraps");
}

/// The sweep arguments' refusals, at the parse: no zero seed count, no
/// shard count below 1, no index at or past the count.
#[test]
fn the_sweep_arguments_are_checked_where_they_are_parsed() {
    assert!(parse_seed_count("0").unwrap_err().contains("at least 1 seed"));
    assert!(parse_seed_count("-1").is_err());
    assert_eq!(parse_seed_count("12000").unwrap().get(), 12_000);
    assert!(Shard::parse("0/0").unwrap_err().contains("K is the shard count, at least 1"));
    assert!(Shard::parse("2/2").unwrap_err().contains("I is a shard index, below K"));
    assert!(Shard::parse("3").unwrap_err().contains("wants I/K"));
    assert!(Shard::parse("x/2").is_err());
    let shard = Shard::parse("7/8").unwrap();
    assert_eq!((shard.index(), shard.count()), (7, 8));
    assert_eq!(Shard::parse("0/1").unwrap(), Shard::WHOLE);
}

const STDERR: &str = "INF_SIM_DRIVER_STDERR_CHILD";
const STDERR_TEST: &str = "driver::tests::a_shard_bounds_its_stderr_only_with_a_results_file";

/// Seven failing seeds, the seventh's text 3,000 bytes, with or without a
/// results file.
fn seven_failing(out: Option<&Path>) -> Exit {
    let run_one = |seed: u64| {
        let mut report = Stub::clean(seed);
        let text = if seed == 0x106 { "x".repeat(3_000) } else { format!("seed {seed:#x} failed") };
        report.violations.push(text);
        report
    };
    run_sweep("stub", &sweep_of(7, out), StubFold::default(), run_one)
}

/// With a results file the shard copies its first five failing seeds to
/// stderr and one line counts the rest and names the file, which holds
/// every seed's line; without one stderr is the shard's record and takes
/// every failing seed, with no crossing line. Each long text is cut as the
/// line writer cuts it.
#[test]
fn a_shard_bounds_its_stderr_only_with_a_results_file() {
    if let Some(out) = std::env::var_os(STDERR) {
        let exit =
            if out.is_empty() { seven_failing(None) } else { seven_failing(Some(Path::new(&out))) };
        std::process::exit(exit.code());
    }
    let out = scratch("stderr");
    let cut = format!("{} … (952 bytes not printed)", "x".repeat(2_048));
    let with = child(STDERR_TEST, &[(STDERR, out.to_str().unwrap())]);
    assert_eq!(with.status.code(), Some(1), "{with:?}");
    let stderr = String::from_utf8_lossy(&with.stderr);
    let seeds: Vec<&str> = stderr.lines().filter(|l| l.starts_with("inf-sim: seed ")).collect();
    assert_eq!(seeds.len(), 5, "{stderr}");
    let results_path = out.join("results-shard-0.txt");
    let crossing = format!(
        "inf-sim: 2 more failing seeds not printed; every seed's line is in {}",
        results_path.display()
    );
    assert!(stderr.lines().any(|l| l == crossing), "{stderr}");
    let results = read(results_path);
    assert_eq!(results.lines().count(), 7, "{results}");
    assert_eq!(results.lines().last(), Some(format!("0x106 VIOLATION {cut}").as_str()));

    let without = child(STDERR_TEST, &[(STDERR, "")]);
    assert_eq!(without.status.code(), Some(1), "{without:?}");
    let stderr = String::from_utf8_lossy(&without.stderr);
    let seeds: Vec<&str> = stderr.lines().filter(|l| l.starts_with("inf-sim: seed ")).collect();
    assert_eq!(seeds.len(), 7, "every failing seed: {stderr}");
    assert_eq!(seeds[6], format!("inf-sim: seed 0x106: {cut}"));
    assert!(!stderr.contains("more failing seeds not printed"), "{stderr}");
    remove(&out);
}

const PANIC: &str = "INF_SIM_DRIVER_PANIC_CHILD";
const PANIC_TEST: &str = "driver::tests::a_panic_names_its_seed_in_the_results";

/// The planted panic's message: two lines, and past the line writer's bound
/// once escaped, a two-byte character astride the cut.
fn planted_message() -> String {
    // "planted\x0apanic on the second seed " is 36 escaped bytes, so the
    // 2,011 bytes after it end at 2,047 and the 'é' would end at 2,049.
    format!("planted\npanic on the second seed {}é tail", "x".repeat(2_011))
}

/// The child's shard: a panic on the second seed, or seven failing seeds
/// and a panic on the eighth.
fn panicking_shard(leg: &str, out: &Path) -> Exit {
    let after_seven = leg == "after-seven";
    let run_one = |seed: u64| {
        let panics_at = if after_seven { 0x107 } else { 0x101 };
        assert!(seed != panics_at, "{}", planted_message());
        let mut report = Stub::clean(seed);
        if after_seven {
            report.violations.push(format!("seed {seed:#x} failed"));
        }
        report
    };
    run_sweep(
        "stub",
        &sweep_of(if after_seven { 8 } else { 3 }, Some(out)),
        StubFold::default(),
        run_one,
    )
}

/// A panic is not caught, and the shard's results name its seed: the
/// hook's line is the file's last, one line however many the message has,
/// cut by the line writer between two characters; the child exits by the
/// panic (101), not an abort, so the hook itself did not panic. A shard that
/// had held failing seeds back from stderr also counts them on the hook's
/// stderr and names the results file.
#[test]
fn a_panic_names_its_seed_in_the_results() {
    if let (Some(leg), Some(out)) =
        (std::env::var_os(PANIC), std::env::var_os("INF_SIM_DRIVER_PANIC_OUT"))
    {
        let _ = panicking_shard(leg.to_str().unwrap(), Path::new(&out));
        unreachable!("the planted panic ends the shard");
    }
    let out = scratch("panic");
    let output = child(
        PANIC_TEST,
        &[(PANIC, "second"), ("INF_SIM_DRIVER_PANIC_OUT", out.to_str().unwrap())],
    );
    assert_eq!(output.status.code(), Some(101), "{output:?}");
    let text = format!(
        "planted\\x0apanic on the second seed {} … (7 bytes not printed)",
        "x".repeat(2_011)
    );
    let results = read(out.join("results-shard-0.txt"));
    assert_eq!(results, format!("0x100 ok\n0x101 VIOLATION panic: {text}\n"), "{output:?}");
    assert!(!out.join("manifest-shard-0.txt").exists(), "a dead shard writes no manifest");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(&format!("inf-sim: seed 0x101: panic: {text}")), "{stderr}");
    assert!(!stderr.contains("more failing seeds not printed"), "none was held back: {stderr}");

    let output = child(
        PANIC_TEST,
        &[(PANIC, "after-seven"), ("INF_SIM_DRIVER_PANIC_OUT", out.to_str().unwrap())],
    );
    assert_eq!(output.status.code(), Some(101), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    let seeds =
        stderr.lines().filter(|l| l.starts_with("inf-sim: seed ") && !l.contains("panic")).count();
    assert_eq!(seeds, 5, "{stderr}");
    assert!(stderr.contains("inf-sim: seed 0x107: panic: planted"), "{stderr}");
    let held = format!(
        "inf-sim: 2 more failing seeds not printed; every seed's line is in {}",
        out.join("results-shard-0.txt").display()
    );
    assert!(stderr.lines().any(|l| l == held), "{stderr}");
    let results = read(out.join("results-shard-0.txt"));
    assert_eq!(results.lines().count(), 8, "{results}");
    assert!(results.lines().last().unwrap().starts_with("0x107 VIOLATION panic: planted"));
    remove(&out);
}
