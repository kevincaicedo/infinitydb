//! Bounds owned by the simulator. Every crossing names its behaviour here.

use inf_foundation::bounded::{Cap, CapFill};

/// Escaped bytes of one printed violation text, counted after control
/// characters are escaped. Owner: the driver's one line writer
/// (`driver::write_text`), which every single-run violation line, every
/// sweep results and stderr line and the panic hook's line go through.
/// Crossing: the escaped text is cut between two characters and outside an
/// escape, and ends ` … (<b> bytes not printed)`, `<b>` in escaped bytes.
pub const VIOLATION_TEXT_BYTES_MAX: usize = 2_048;

/// Violation entries a single run prints in raise order. Owner: the
/// single-seed driver, through `Violations::printed_lines`. Crossing: a
/// later entry is printed only as the first of its check, and one line
/// counts the rest by check. The run's list keeps every entry.
pub const VIOLATIONS_PRINTED_FIRST: usize = 5;

/// Bytes of one report's readings, the values beyond its hashes and its
/// verdict that a twin compares. Owner: the twin, whose const assertion
/// holds every report's readings type to it, so a sweep holds one run, two
/// hashes, a verdict and these bytes at a time. Crossing: none — a larger
/// readings type does not build.
pub const READINGS_BYTES_MAX: usize = 64;

/// Failing seeds a sweep shard run with `--out` copies to stderr. Owner:
/// the sweep driver. Crossing: the results file keeps every seed's line, and
/// one stderr line after the shard's last seed counts the rest and names
/// that file. A shard run without `--out` has no results file, so its
/// stderr is its record and takes every failing seed, each line cut by
/// `VIOLATION_TEXT_BYTES_MAX`.
pub const FAILING_SEEDS_PRINTED_MAX: u64 = 5;

/// Cold reads the steel thread's executor holds at once (ADR-0177 D2: no
/// task without a class): its one class, a `GET` whose tier read may
/// suspend. Owner: `steel::run`, which makes one at a time. Crossing: a
/// reserve past it panics the run, a harness defect.
pub const STEEL_GETS: Cap = Cap::entries::<8>("steel-gets", CapFill::Serving);

/// Writers the pressure leg's executor holds: its one class, a writer
/// storm, eight of which run for the leg's life. Owner:
/// `pressure::run_pressure_scenario`. Crossing: as [`STEEL_GETS`].
pub const PRESSURE_WRITERS: Cap = Cap::entries::<64>("pressure-writers", CapFill::Serving);

/// The pressure leg's wait nodes, under its stall gate: one for each slot of
/// [`PRESSURE_WRITERS`], each writer parking at most one wait (ADR-0177 D5).
/// Owner: `pressure::build_world`. Crossing: a wait finds no node, which the
/// leg's writer count never reaches.
pub const PRESSURE_WAIT_NODES: Cap =
    Cap::entries::<{ PRESSURE_WRITERS.entries_max() }>("pressure-wait-nodes", CapFill::Serving);

/// Cold `GET`s the cold storm's executor holds at once: its one class.
/// Owner: `coldstorm::run`, whose rounds keep at most this many in
/// flight. Crossing: as [`STEEL_GETS`].
pub const COLDSTORM_GETS: Cap = Cap::entries::<256>("coldstorm-gets", CapFill::Serving);
