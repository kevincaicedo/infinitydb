//! The simulator's single-seed driver and the determinism twin.
//!
//! A single run goes through [`run_single`]: the scenario's path supplies
//! its run and the lines it prints, and the driver prints the run's
//! violations in their bounded form ([`Violations::printed_lines`]). Under
//! `--verify-determinism` it then runs the seed again and prints one line:
//! the twin's agreement or the divergence it found. The twin runs whatever
//! the first run's verdict was: a violation that does not reproduce is
//! itself a divergence. The twin is private to this module, so the driver
//! is its only caller.
//!
//! The twin compares everything a report reproduces: the trace hash, the
//! state hash, the verdict, the report's readings and, where the run keeps
//! its full trace, the trace bytes.
//!
//! Every printed violation text goes through one line writer
//! ([`write_text`]), which escapes control characters and cuts the escaped
//! text at its bound, so a line stays one line of bounded length.

mod line;
mod reports;
mod violations;

use std::fmt;

pub use line::{one_line, write_text};
pub use violations::Violations;

use crate::limits::READINGS_BYTES_MAX;

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
    /// A violation or a divergence.
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
        Verify::Twin => match twice(&mut run_one, seed, first_lines) {
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

/// The twin's outcome. It exists only once the second run returned.
#[derive(Debug)]
enum Twin {
    Agreed { verdict: Verdict, trace_hash: u64, state_hash: u64 },
    Diverged { reason: Divergence },
}

/// Runs `seed` twice and compares the runs; `on_first` sees the first run
/// before the second starts and may end the twin with its error. The second
/// run happens whatever the first's verdict was.
fn twice<R: SeedReport, E>(
    run_one: &mut impl FnMut(u64) -> R,
    seed: u64,
    on_first: impl FnOnce(&R) -> Result<(), E>,
) -> Result<Twin, E> {
    const { assert!(size_of::<R::Readings>() <= READINGS_BYTES_MAX) };
    let first = run_one(seed);
    on_first(&first)?;
    let (trace_hash, state_hash) = (first.trace_hash(), first.state_hash());
    let (verdict, readings) = (first.verdict(), first.readings());
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
        traces_differ(&first, &twin)
    };
    Ok(match reason {
        None => Twin::Agreed { verdict, trace_hash, state_hash },
        Some(reason) => Twin::Diverged { reason },
    })
}

/// The full traces, where the report keeps one.
fn traces_differ<R: SeedReport>(first: &R, twin: &R) -> Option<Divergence> {
    match (first.trace(), twin.trace()) {
        (Some(a), Some(b)) if a != b => {
            Some(Divergence::TraceBytes { first: a.len(), twin: b.len() })
        }
        (Some(_) | None, _) => None,
    }
}

#[cfg(test)]
mod tests;
