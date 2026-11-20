//! The simulator's two drivers and the determinism twin they share.
//!
//! A single run goes through [`run_single`]: the scenario's path supplies
//! its run and the lines it prints, and the driver prints the run's
//! violations in their bounded form ([`Violations::printed_lines`]). Under
//! `--verify-determinism` it then runs the seed again and prints one line:
//! the twin's agreement or the divergence it found. A sweep goes through
//! [`run_sweep`], the simulator's only seed loop: every seed of the shard
//! runs twice, always, because in a sweep the twin is not a flag; each
//! seed's verdict line is written to the shard's results before the next
//! seed runs, and the manifest is written once, at the shard's end, after
//! the fold's shard-end clauses. The twin runs whatever the first run's
//! verdict was: a violation that does not reproduce is itself a divergence.
//! The twin is private to this module, so the two drivers are its only
//! callers.
//!
//! The twin compares everything a report reproduces: the trace hash, the
//! state hash, the verdict, the report's readings and, where a single run
//! keeps its full trace, the trace bytes. A sweep keeps no trace bytes, for
//! their size, so a shard holds one run, two hashes, a verdict and at most
//! [`READINGS_BYTES_MAX`] bytes of readings at a time.
//!
//! A panic is not caught: the drivers install a hook that names the running
//! seed on stderr and, under `--out`, as the last line of the shard's
//! results (`hook`), and the process dies as before.
//!
//! Every printed violation text goes through one line writer
//! ([`write_text`]), which escapes control characters and cuts the escaped
//! text at its bound, so a line stays one line of bounded length.

mod hook;
mod line;
mod reports;
mod violations;

use std::convert::Infallible;
use std::fmt;
use std::fs::File;
use std::io::Write as _;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};

pub use line::{one_line, write_text};
pub use violations::Violations;

use crate::limits::{FAILING_SEEDS_PRINTED_MAX, READINGS_BYTES_MAX};

/// What one run of a seed concluded, by its own oracles.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Every oracle held.
    Clean,
    /// The boot refused under the recovery taxonomy's fail-stop rules and
    /// every oracle held: a legal outcome, disclosed and never a violation.
    Refused,
    /// An oracle failed, or the run stalled: the first violation's text, or
    /// `stall` when the run raised none.
    Violated(String),
}

impl Verdict {
    /// The verdict of a report whose own `ok()` reads `ok`, whose boot
    /// refused legally when `refused`, and whose violations are
    /// `violations`.
    #[must_use]
    pub fn judge(ok: bool, refused: bool, violations: &Violations) -> Verdict {
        match (ok, refused) {
            (false, _) => Verdict::Violated(violations.first().unwrap_or("stall").to_owned()),
            (true, true) => Verdict::Refused,
            (true, false) => Verdict::Clean,
        }
    }

    fn is_violated(&self) -> bool {
        match self {
            Verdict::Violated(_) => true,
            Verdict::Clean | Verdict::Refused => false,
        }
    }
}

impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Verdict::Clean => f.write_str("ok"),
            Verdict::Refused => f.write_str("refused"),
            Verdict::Violated(first) => write!(f, "violation {}", one_line(first)),
        }
    }
}

/// What differs between a seed's first run and its twin: the first
/// difference found, in the order the variants are listed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Divergence {
    /// The trace hash, then the state hash: `state::verify`'s reason.
    Hashes(String),
    Verdict {
        first: Verdict,
        twin: Verdict,
    },
    /// The report's readings, each as its `Debug` text.
    Readings {
        first: String,
        twin: String,
    },
    /// The full traces, compared byte for byte where the run keeps them:
    /// their lengths.
    TraceBytes {
        first: usize,
        twin: usize,
    },
}

impl fmt::Display for Divergence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Divergence::Hashes(reason) => f.write_str(reason),
            Divergence::Verdict { first, twin } => {
                write!(f, "verdict differs (first run {first}, twin {twin})")
            }
            Divergence::Readings { first, twin } => {
                write!(f, "readings differ ({first} vs {twin})")
            }
            Divergence::TraceBytes { first, twin } => {
                write!(f, "traces differ ({first} vs {twin} bytes)")
            }
        }
    }
}

/// The virtual time a run covered and the pub/sub counts the nightly's
/// budget line carries beside it.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SimSeconds {
    pub seconds: f64,
    pub published: u64,
    pub delivered: u64,
}

/// One run of one seed, as the drivers and the twin read it. Every
/// scenario's report implements it; no item has a default body, so every
/// report states each one.
pub trait SeedReport {
    /// What the report reproduces beyond its hashes, its verdict and its
    /// trace, which the twin compares. A report that takes none states
    /// `()`. At most [`READINGS_BYTES_MAX`] bytes, so a sweep holds it
    /// beside the hashes without the whole report.
    type Readings: Copy + Eq + fmt::Debug;

    fn trace_hash(&self) -> u64;
    fn state_hash(&self) -> u64;
    fn verdict(&self) -> Verdict;
    /// Every violation the run's oracles raised, in raise order.
    fn violations(&self) -> &Violations;
    /// The full event trace, where the run keeps one.
    fn trace(&self) -> Option<&[u8]>;
    /// The virtual time the run covered, where it measures it.
    fn sim_seconds(&self) -> Option<SimSeconds>;
    fn readings(&self) -> Self::Readings;
}

/// `--verify-determinism` on a single run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verify {
    Off,
    Twin,
}

/// The process's exit status, which the binary's `main` passes on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    /// Every oracle held and every twin agreed.
    Clean,
    /// A violation, a divergence or a fired shard-end clause.
    Failed,
    /// A usage error or an output the run could not write: the harness's
    /// code 2.
    Usage,
}

impl Exit {
    #[must_use]
    pub fn code(self) -> i32 {
        match self {
            Exit::Clean => 0,
            Exit::Failed => 1,
            Exit::Usage => 2,
        }
    }

    fn of(verdict: &Verdict) -> Exit {
        if verdict.is_violated() { Exit::Failed } else { Exit::Clean }
    }
}

/// Runs one seed: the path's lines, then the run's violations in their
/// bounded form, then, under [`Verify::Twin`], the twin and its one line.
/// `print` returns the text of a usage refusal (an unwritable
/// `--trace-out`, for one), which ends the run with [`Exit::Usage`] before
/// any twin.
pub fn run_single<R: SeedReport>(
    seed: u64,
    verify: Verify,
    mut run_one: impl FnMut(u64) -> R,
    print: impl FnOnce(&R) -> Result<(), String>,
) -> Exit {
    hook::install();
    let _running = hook::Running::seed(seed);
    let first_lines = |report: &R| -> Result<(), String> {
        print(report)?;
        for line in report.violations().printed_lines() {
            eprintln!("{line}");
        }
        Ok(())
    };
    match verify {
        Verify::Off => {
            let report = run_one(seed);
            match first_lines(&report) {
                Ok(()) => Exit::of(&report.verdict()),
                Err(refusal) => usage(&refusal),
            }
        }
        Verify::Twin => match twice(&mut run_one, seed, Keep::Report, first_lines) {
            Err(refusal) => usage(&refusal),
            Ok(Twin::Agreed { verdict, trace_hash, state_hash }) => {
                println!(
                    "inf-sim: determinism: agreed (state_hash={state_hash:#018x}, \
                     trace_hash={trace_hash:#018x})"
                );
                Exit::of(&verdict)
            }
            Ok(Twin::Diverged { reason, .. }) => {
                eprintln!("inf-sim: DETERMINISM VIOLATION — {}", one_line(&reason.to_string()));
                Exit::Failed
            }
        },
    }
}

fn usage(refusal: &str) -> Exit {
    eprintln!("inf-sim: {refusal}");
    Exit::Usage
}

/// What the twin keeps of a seed's first run while the second runs.
#[derive(Clone, Copy, Debug)]
enum Keep {
    /// The hashes, the verdict and the readings: a sweep holds one run at
    /// a time.
    Hashes,
    /// The whole report, for its trace bytes.
    Report,
}

/// The twin's outcome. It exists only once the second run returned.
#[derive(Debug)]
enum Twin {
    Agreed { verdict: Verdict, trace_hash: u64, state_hash: u64 },
    Diverged { verdict: Verdict, reason: Divergence },
}

/// Runs `seed` twice and compares the runs; `on_first` sees the first run
/// before the second starts and may end the twin with its error. The second
/// run happens whatever the first's verdict was.
fn twice<R: SeedReport, E>(
    run_one: &mut impl FnMut(u64) -> R,
    seed: u64,
    keep: Keep,
    on_first: impl FnOnce(&R) -> Result<(), E>,
) -> Result<Twin, E> {
    const { assert!(size_of::<R::Readings>() <= READINGS_BYTES_MAX) };
    let first = run_one(seed);
    on_first(&first)?;
    let (trace_hash, state_hash) = (first.trace_hash(), first.state_hash());
    let (verdict, readings) = (first.verdict(), first.readings());
    let kept = match keep {
        Keep::Report => Some(first),
        Keep::Hashes => {
            drop(first);
            None
        }
    };
    let twin = run_one(seed);
    let hashes = crate::state::verify(trace_hash, state_hash, twin.trace_hash(), twin.state_hash());
    let reason = if let Err(reason) = hashes {
        Some(Divergence::Hashes(reason))
    } else if twin.verdict() != verdict {
        Some(Divergence::Verdict { first: verdict.clone(), twin: twin.verdict() })
    } else if twin.readings() != readings {
        Some(Divergence::Readings {
            first: format!("{readings:?}"),
            twin: format!("{:?}", twin.readings()),
        })
    } else {
        kept.as_ref().and_then(|first| traces_differ(first, &twin))
    };
    Ok(match reason {
        None => Twin::Agreed { verdict, trace_hash, state_hash },
        Some(reason) => Twin::Diverged { verdict, reason },
    })
}

/// The full traces, where a single run kept the first run's report.
fn traces_differ<R: SeedReport>(first: &R, twin: &R) -> Option<Divergence> {
    match (first.trace(), twin.trace()) {
        (Some(a), Some(b)) if a != b => {
            Some(Divergence::TraceBytes { first: a.len(), twin: b.len() })
        }
        (Some(_) | None, _) => None,
    }
}

/// `--shard I/K`: this process runs the seeds `base + j` with
/// `j ≡ I (mod K)`, K at least 1 and I below K.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shard {
    index: u64,
    count: NonZeroU64,
}

impl Shard {
    /// The one shard of an unsharded sweep.
    pub const WHOLE: Shard = Shard { index: 0, count: NonZeroU64::MIN };

    /// Parses `I/K`; a value outside the type is the refusal's text.
    pub fn parse(text: &str) -> Result<Shard, String> {
        let (index, count) =
            text.split_once('/').ok_or_else(|| format!("--shard wants I/K, got {text}"))?;
        let index: u64 = index.parse().map_err(|e| format!("--shard {text}: {e}"))?;
        let count: u64 = count.parse().map_err(|e| format!("--shard {text}: {e}"))?;
        let count = NonZeroU64::new(count)
            .ok_or_else(|| format!("--shard {text}: K is the shard count, at least 1"))?;
        if index >= count.get() {
            return Err(format!("--shard {text}: I is a shard index, below K"));
        }
        Ok(Shard { index, count })
    }

    #[must_use]
    pub fn index(self) -> u64 {
        self.index
    }

    #[must_use]
    pub fn count(self) -> u64 {
        self.count.get()
    }
}

/// Parses `--sweep N`: the seed count, at least 1.
pub fn parse_seed_count(text: &str) -> Result<NonZeroU64, String> {
    let seeds: u64 = text.parse().map_err(|e| format!("--sweep {text}: {e}"))?;
    NonZeroU64::new(seeds).ok_or_else(|| format!("--sweep {text}: a sweep runs at least 1 seed"))
}

/// A sweep's arguments, checked where they were parsed: `seeds` seeds from
/// `base_seed`, this process's shard of them, and the directory its results
/// and manifest go to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sweep {
    base_seed: u64,
    seeds: NonZeroU64,
    shard: Shard,
    out: Option<PathBuf>,
}

impl Sweep {
    #[must_use]
    pub fn new(base_seed: u64, seeds: NonZeroU64, shard: Shard, out: Option<PathBuf>) -> Sweep {
        Sweep { base_seed, seeds, shard, out }
    }

    /// The shard's seeds: `base + j` for every `j` below `seeds` with
    /// `j ≡ I (mod K)`, at most `ceil(seeds / K)` of them, so the K shards
    /// cover the request once.
    fn shard_seeds(&self) -> impl Iterator<Item = u64> + '_ {
        let count = self.shard.count();
        std::iter::successors(Some(self.shard.index()), move |j| j.checked_add(count))
            .take_while(|j| *j < self.seeds.get())
            .map(|j| self.base_seed.wrapping_add(j))
    }
}

/// A sweep-capable scenario's per-shard counters. The four items have no
/// default bodies, so every scenario states each one.
pub trait Fold {
    type Report: SeedReport;
    /// Folds one seed's first run.
    fn absorb(&mut self, report: &Self::Report);
    /// The shard's coverage, printed after the driver's counts on the
    /// shard's coverage line: disclosed, never assumed.
    fn coverage(&self) -> String;
    /// The manifest's fields after the head every sweep shares.
    fn manifest_tail(&self) -> String;
    /// Every check over the whole shard's fold, such as an arm's
    /// engagement: the text of each that fired. Each counts as one
    /// violation and is one `shard VIOLATION` line before the manifest; a
    /// shard-scope engagement check lives here and nowhere else.
    fn shard_end_clauses(&self) -> Vec<String>;
}

/// Runs this process's shard of `sweep`, each seed twice, and writes its
/// results and manifest under `--out`. The exit status is [`Exit::Failed`]
/// exactly when the manifest's `violations` is above zero.
pub fn run_sweep<F: Fold>(
    scenario: &str,
    sweep: &Sweep,
    mut fold: F,
    mut run_one: impl FnMut(u64) -> F::Report,
) -> Exit {
    hook::install();
    let files = match sweep.out.as_deref().map(|dir| ShardFiles::open(dir, sweep.shard)) {
        None => None,
        Some(Ok(files)) => Some(files),
        Some(Err(error)) => return usage(&error),
    };
    let lent = files.as_ref().map(|files| {
        hook::LentResults::lend(&files.results, files.results_path.display().to_string())
    });
    let mut tally = Tally::default();
    let mut copy = StderrCopy::of(files.as_ref());
    let mut sim_seconds: Option<SimSeconds> = None;
    for seed in sweep.shard_seeds() {
        let _running = hook::Running::seed(seed);
        let Ok(twin) = twice(&mut run_one, seed, Keep::Hashes, |first: &F::Report| {
            tally.seeds_run += 1;
            fold.absorb(first);
            if let Some(measured) = first.sim_seconds() {
                sim_seconds.get_or_insert_default().add(measured);
            }
            Ok::<(), Infallible>(())
        });
        let outcome = tally.seed(seed, &twin);
        if let Some(Err(error)) = files.as_ref().map(|files| files.write_line(&outcome.results)) {
            return usage(&error);
        }
        if copy.seed(&outcome.stderr) == Copied::Withheld
            && let Some(lent) = &lent
        {
            lent.withheld_one();
        }
    }
    copy.finish();
    let (index, count) = (sweep.shard.index(), sweep.shard.count());
    for clause in fold.shard_end_clauses() {
        tally.violations += 1;
        let clause = one_line(&clause);
        eprintln!("inf-sim: shard {index}/{count}: VIOLATION {clause}");
        let line = format!("shard VIOLATION {clause}");
        if let Some(Err(error)) = files.as_ref().map(|files| files.write_line(&line)) {
            return usage(&error);
        }
    }
    println!(
        "inf-sim: {scenario} sweep shard {index}/{count}: {} seeds ({} verified, {} diverged), \
         {} violations, {} legal taxonomy refusals; {}",
        tally.seeds_run,
        tally.seeds_verified,
        tally.seeds_diverged,
        tally.violations,
        tally.refused,
        fold.coverage()
    );
    // First runs only: the twin replays virtual time the shard already
    // counted, and the line is never a manifest field, which the nightly's
    // budget sum would count twice.
    if let Some(measured) = sim_seconds {
        println!(
            "inf-sim: sim_seconds={:.6} published={} delivered={}",
            measured.seconds, measured.published, measured.delivered
        );
    }
    if let Some(files) = &files {
        let manifest = tally.manifest(scenario, sweep, &fold.manifest_tail());
        if let Err(error) = files.write_manifest(&manifest) {
            return usage(&error);
        }
    }
    if tally.violations > 0 { Exit::Failed } else { Exit::Clean }
}

impl SimSeconds {
    fn add(&mut self, other: SimSeconds) {
        self.seconds += other.seconds;
        self.published += other.published;
        self.delivered += other.delivered;
    }
}

/// A shard's counts. `seeds_verified` is the liveness count: it rises only
/// when a twin returned, so a driver that stopped running the twin would
/// write it short of `seeds_run`.
#[derive(Default)]
struct Tally {
    seeds_run: u64,
    seeds_verified: u64,
    seeds_diverged: u64,
    violations: u64,
    refused: u64,
}

/// One seed's results line, and its stderr lines when it failed: a
/// violation's line, or a divergence's and the first run's violation.
struct SeedOutcome {
    results: String,
    stderr: Vec<String>,
}

impl Tally {
    /// Counts one seed's twin and returns its lines.
    fn seed(&mut self, seed: u64, twin: &Twin) -> SeedOutcome {
        self.seeds_verified += 1;
        let quiet = |results: String| SeedOutcome { results, stderr: Vec::new() };
        match twin {
            Twin::Agreed { verdict: Verdict::Clean, .. } => quiet(format!("{seed:#x} ok")),
            Twin::Agreed { verdict: Verdict::Refused, .. } => {
                self.refused += 1;
                quiet(format!("{seed:#x} refused (taxonomy fail-stop)"))
            }
            Twin::Agreed { verdict: Verdict::Violated(first), .. } => {
                self.violations += 1;
                let first = one_line(first);
                SeedOutcome {
                    stderr: vec![format!("inf-sim: seed {seed:#x}: {first}")],
                    results: format!("{seed:#x} VIOLATION {first}"),
                }
            }
            Twin::Diverged { verdict, reason } => {
                self.violations += 1;
                self.seeds_diverged += 1;
                let reason = one_line(&reason.to_string());
                let mut stderr = vec![format!("inf-sim: seed {seed:#x}: determinism: {reason}")];
                if let Verdict::Violated(first) = verdict {
                    stderr.push(format!("inf-sim: seed {seed:#x}: first run: {}", one_line(first)));
                }
                SeedOutcome {
                    stderr,
                    results: format!("{seed:#x} VIOLATION determinism: {reason}"),
                }
            }
        }
    }

    /// The manifest: the head every sweep shares, then the fold's tail.
    fn manifest(&self, scenario: &str, sweep: &Sweep, tail: &str) -> String {
        let mut manifest = format!(
            "scenario={scenario} base_seed={:#x} sweep={} shard={}/{} seeds_run={} \
             seeds_verified={} seeds_diverged={} violations={} refused={}",
            sweep.base_seed,
            sweep.seeds,
            sweep.shard.index(),
            sweep.shard.count(),
            self.seeds_run,
            self.seeds_verified,
            self.seeds_diverged,
            self.violations,
            self.refused
        );
        if !tail.is_empty() {
            manifest.push(' ');
            manifest.push_str(tail);
        }
        manifest.push('\n');
        manifest
    }
}

/// What a shard copies of its failing seeds to stderr. With `--out` the
/// results file is the shard's record of every seed, so stderr takes the
/// first [`FAILING_SEEDS_PRINTED_MAX`] failing seeds and one line counts the
/// rest and names that file. Without it stderr is the shard's record, so it
/// takes every failing seed, each line cut by the line writer, and no
/// crossing line.
enum StderrCopy {
    Bounded { results: String, copied: u64, withheld: u64 },
    Every,
}

impl StderrCopy {
    fn of(files: Option<&ShardFiles>) -> StderrCopy {
        match files {
            Some(files) => StderrCopy::Bounded {
                results: files.results_path.display().to_string(),
                copied: 0,
                withheld: 0,
            },
            None => StderrCopy::Every,
        }
    }

    fn seed(&mut self, lines: &[String]) -> Copied {
        if lines.is_empty() {
            return Copied::Passed;
        }
        match self {
            StderrCopy::Bounded { copied, withheld, .. }
                if *copied >= FAILING_SEEDS_PRINTED_MAX =>
            {
                *withheld += 1;
                return Copied::Withheld;
            }
            StderrCopy::Bounded { copied, .. } => *copied += 1,
            StderrCopy::Every => {}
        }
        for line in lines {
            eprintln!("{line}");
        }
        Copied::Printed
    }

    /// The crossing line, after the shard's last seed.
    fn finish(&self) {
        if let StderrCopy::Bounded { results, withheld, .. } = self
            && *withheld > 0
        {
            eprintln!(
                "inf-sim: {withheld} more failing seeds not printed; every seed's line is in \
                 {results}"
            );
        }
    }
}

/// What a shard did with one seed's stderr lines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Copied {
    /// The seed passed: it has none.
    Passed,
    Printed,
    /// Past the bound: the results file holds its line.
    Withheld,
}

/// The shard's two files under `--out`: the results, written one
/// unbuffered line at a time, and the manifest, written once.
struct ShardFiles {
    results: File,
    results_path: PathBuf,
    manifest_path: PathBuf,
}

impl ShardFiles {
    /// Creates the directory, removes the shard's old manifest and
    /// truncates its results, so a reused `--out` never shows an earlier
    /// run's verdict.
    fn open(dir: &Path, shard: Shard) -> Result<ShardFiles, String> {
        let index = shard.index();
        std::fs::create_dir_all(dir).map_err(|e| format!("--out {}: {e}", dir.display()))?;
        let manifest_path = dir.join(format!("manifest-shard-{index}.txt"));
        match std::fs::remove_file(&manifest_path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("--out {}: {e}", manifest_path.display())),
        }
        let results_path = dir.join(format!("results-shard-{index}.txt"));
        let results = File::create(&results_path)
            .map_err(|e| format!("--out {}: {e}", results_path.display()))?;
        Ok(ShardFiles { results, results_path, manifest_path })
    }

    fn write_line(&self, line: &str) -> Result<(), String> {
        (&self.results)
            .write_all(format!("{line}\n").as_bytes())
            .map_err(|e| format!("--out {}: {e}", self.results_path.display()))
    }

    fn write_manifest(&self, manifest: &str) -> Result<(), String> {
        std::fs::write(&self.manifest_path, manifest)
            .map_err(|e| format!("--out {}: {e}", self.manifest_path.display()))
    }
}

#[cfg(test)]
mod tests;
