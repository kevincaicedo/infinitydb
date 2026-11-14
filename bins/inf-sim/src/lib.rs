#![allow(
    clippy::disallowed_methods,
    reason = "sim-harness: fixture files outside the simulated cells (ADR-0144 D5)"
)]
#![allow(
    clippy::disallowed_types,
    reason = "sim-harness: the simulator's models and harness, not cell code (ADR-0163 D2)"
)]
//! `inf-sim` — the deterministic simulator skeleton (M0-S20, master plan
//! §17.1): the whole node — N cells, fabric, wire, store, command plane —
//! runs single-threaded with injected time and entropy. Same seed ⇒
//! byte-identical event traces; every failure is a replayable seed.
//!
//! Composition: the **real** `ServerPlane`/`CellLoop` (no sim forks of the
//! data plane) over [`SimDriver`], a `BackendDriver` whose "network" is
//! in-memory per-cell byte queues with seeded chunking (random recv split
//! points exercise the parser's resumability on every run). Simulated
//! clients live in the harness; a shared [`oracle`] observes every apply
//! point and replays it against a single-store model — replies must match
//! byte-for-byte (the single-key linearizability oracle: apply points on a
//! single thread form a real total order) — and, since ADR-0129, against an
//! independent model written from Redis's documented semantics
//! (`harness::shadow`), the check the replay cannot be: the replay runs the
//! product's own `execute` on the product's own store.
//!
//! # Device model per scenario (F-L04-06)
//!
//! The write-vs-fsync reorder window (ADR-0087 D7) exists only on a
//! [`inf_log::fs::sim::StallConfig`] whose plain-write base or bandwidth
//! is non-zero; `SimDisk::new()` is the instant, submission-ordered
//! device. Every **driver-tier** durable boot goes through
//! [`durable::boot`], which refuses a closed window:
//!
//! - scenarios — device
//! - ---|---
//! - `m2-*`, `m3-document`, `m2-combined`, `m4-tiered`, `m2-ns-create-window`,
//!   `m2-ns-drop-window`, `m2-ns-ddl-race` —
//!   the m2 reference stall device (`durable::m2_stall_config`, writes 8 µs + tail)
//! - `m45-backfill`, `m45-sidecar`, `boot-storm` — `StallConfig::write_reorder()` — writes off the
//!   timeline, fsyncs instant
//! - `m4-steel`, `m4-cold`, `m4-pressure`, `m4-diskfull`, `m4-recovery` — **order-preserving by
//!   construction**: the flush pipeline runs the blocking `SegmentFile` tier (a `write_at` returns
//!   landed); the driver is used for tier reads only, so no write-vs-fsync window exists to open

// §17.3 as amended (ADR-0121, batch 44): the simulator's backend driver
// (`net`) and its steel-thread tier reader (`steel`) execute driver ops
// over `StableBytes` payloads — the two named `allow`s (SAFETY.md); the
// library root, where the unsafe lives, carries the deny (the binary's
// `forbid` governed a root with none of it).
#![deny(unsafe_code)]
// ADR-0144 D1: a production `match` names every variant of its enum.
#![cfg_attr(
    not(test),
    deny(clippy::wildcard_enum_match_arm, clippy::match_wildcard_for_single_variants)
)]

pub mod backfill;
pub mod bootstorm;
mod census;
pub mod coldstorm;
pub mod combined;
pub mod diskfull;
mod document;
pub mod driver;
pub mod durable;
pub mod extent_window;
pub mod harness;
mod lift;
pub mod limits;
#[allow(unsafe_code)]
pub mod net;
pub mod nscreate;
pub mod nsddl;
pub mod nsdrop;
pub mod nslifetimes;
pub mod pressure;
pub mod recovery;
pub mod resp;
pub mod sidecar;
pub mod state;
#[allow(unsafe_code)]
pub mod steel;
pub mod tiered;
pub mod txmodel;

pub use backfill::{BackfillReport, BackfillScenario, run_backfill_scenario};
pub use bootstorm::{BootStormReport, BootStormScenario, run_boot_storm_scenario};
pub use coldstorm::{ColdLeg, ColdStormReport, ColdStormScenario, run_cold_storm_scenario};
pub use combined::{CombinedReport, CombinedScenario, run_combined_scenario};
pub use diskfull::{DiskfullReport, DiskfullScenario, run_diskfull_scenario};
pub use durable::{
    DurableReport, DurableScenario, EpochMarksLeg, LedgerWedgeLeg, TierBudgetLeg, WakeLeg,
    WakeReport, run_durable_scenario,
};
pub use extent_window::{
    ExtentWindowLeg, ExtentWindowReport, ExtentWindowScenario, run_extent_window_scenario,
};
pub use harness::{PubTarget, Scenario, SimReport, run_scenario};
pub use nscreate::{NsCreateWindowReport, run_ns_create_window_scenario};
pub use nsddl::{NsDdlRaceReport, run_ns_ddl_race_scenario};
pub use nsdrop::{DropCut, NsDropWindowReport, run_ns_drop_window_scenario};
pub use nslifetimes::{LifetimesLeg, NsLifetimesReport, run_ns_lifetimes_scenario};
pub use pressure::{PressureLeg, PressureReport, PressureScenario, run_pressure_scenario};
pub use recovery::{
    RecoveryReport, RecoveryScenario, SealCensus, SeedClass, SpecVariant, run_recovery_scenario,
};
pub use sidecar::{SidecarReport, SidecarScenario, run_sidecar_scenario};
pub use steel::{SteelReport, SteelScenario, run_steel_scenario};
pub use tiered::{TieredNodeReport, TieredScenario, run_tiered_scenario};

/// Every scenario the `inf-sim` binary accepts, one row each: the registry
/// `scripts/sim-smoke.sh` runs once per merge and `tests/lanes.rs` checks
/// against the CLI dispatch, so a scenario cannot be born unrun. Add a
/// scenario here, in `main.rs`, and in the smoke script together.
///
/// The sweep column says whether the scenario takes `--sweep`; the binary
/// refuses it on every other row before any run. The leg column names the
/// leg flag a legged scenario takes and no other row takes, and whether the
/// flag's absence is refused or runs the leg the row names.
pub const SCENARIOS: &[ScenarioRow] = &[
    ScenarioRow::single("m0-smoke"),
    ScenarioRow::single("m0-adversarial"),
    ScenarioRow::single("m0-surface"),
    ScenarioRow::single("m0-fabric-fairness"),
    ScenarioRow::single("m0-admission"),
    ScenarioRow::single("m1-cache"),
    ScenarioRow::sweeps("m2-durable"),
    ScenarioRow::sweeps("m2-clean-stop"),
    ScenarioRow::sweeps("m2-device-budget"),
    ScenarioRow::sweeps("m2-mode-transition"),
    ScenarioRow::sweeps("m2-reorder-window"),
    ScenarioRow::sweeps("m2-fill-tick"),
    ScenarioRow::sweeps("m2-fill-dense"),
    ScenarioRow::sweeps("m2-group-hold"),
    ScenarioRow::sweeps("m2-fua-pending"),
    ScenarioRow::sweeps("m2-ledger-wedge").with_leg(Leg::LedgerWedge),
    ScenarioRow::sweeps("m2-stop-tick"),
    ScenarioRow::sweeps("m2-ckpt-refused"),
    ScenarioRow::sweeps("m2-recycle"),
    ScenarioRow::sweeps("m2-combined"),
    ScenarioRow::sweeps("m2-ns-create-window"),
    ScenarioRow::sweeps("m2-ns-drop-window"),
    ScenarioRow::sweeps("m2-ns-ddl-race"),
    ScenarioRow::sweeps("m3-document"),
    ScenarioRow::single("boot-storm"),
    ScenarioRow::single("m4-steel"),
    ScenarioRow::single("m4-pressure").with_default_leg(LegValue::Pressure(PressureLeg::Budget)),
    ScenarioRow::single("m4-cold").with_default_leg(LegValue::Cold(ColdLeg::Storm)),
    ScenarioRow::sweeps("m4-recovery"),
    ScenarioRow::single("m4-diskfull"),
    ScenarioRow::sweeps("m4-tiered"),
    ScenarioRow::sweeps("m4-ns-lifetimes")
        .with_default_leg(LegValue::Lifetimes(LifetimesLeg::Full)),
    ScenarioRow::sweeps("m4-tier-budget").with_leg(Leg::TierBudget),
    ScenarioRow::sweeps("m4-epoch-marks").with_leg(Leg::EpochMarks),
    ScenarioRow::single("m4-extent-window").with_leg(Leg::ExtentWindow),
    ScenarioRow::single("m45-backfill"),
    ScenarioRow::single("m45-sidecar"),
    ScenarioRow::single("m1-pub-cross").with_default_leg(LegValue::PubTarget(PubTarget::Next)),
];

/// One registry row: a scenario's name and its two columns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScenarioRow {
    pub name: &'static str,
    pub sweeps: Sweeps,
    pub leg: LegCell,
}

/// The sweep column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sweeps {
    /// `--sweep N` runs N seeds through the sweep driver, each twice.
    Yes,
    /// One seed a run; `--sweep` is refused.
    No,
}

/// The leg column: whether a row takes a leg flag, and what the flag's
/// absence means there. Three outcomes, so an enum (a required leg and a
/// defaulted one differ only when the flag is absent).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LegCell {
    /// The row takes no leg flag.
    NoLeg,
    /// The row runs the leg its flag names, and refuses to run without it.
    Required(Leg),
    /// The row runs the leg its flag names, or this one when the flag is
    /// absent.
    Defaulted(LegValue),
}

impl LegCell {
    /// The leg kind whose flag the row takes, if it takes one.
    #[must_use]
    pub const fn kind(self) -> Option<Leg> {
        match self {
            LegCell::NoLeg => None,
            LegCell::Required(leg) => Some(leg),
            LegCell::Defaulted(default) => Some(default.leg()),
        }
    }
}

impl ScenarioRow {
    const fn sweeps(name: &'static str) -> ScenarioRow {
        ScenarioRow { name, sweeps: Sweeps::Yes, leg: LegCell::NoLeg }
    }

    const fn single(name: &'static str) -> ScenarioRow {
        ScenarioRow { name, sweeps: Sweeps::No, leg: LegCell::NoLeg }
    }

    const fn with_leg(self, leg: Leg) -> ScenarioRow {
        ScenarioRow { leg: LegCell::Required(leg), ..self }
    }

    const fn with_default_leg(self, default: LegValue) -> ScenarioRow {
        ScenarioRow { leg: LegCell::Defaulted(default), ..self }
    }

    /// The row whose name is `name`.
    #[must_use]
    pub fn named(name: &str) -> Option<&'static ScenarioRow> {
        SCENARIOS.iter().find(|row| row.name == name)
    }

    /// The leg this row runs, out of the leg flags the command line gave:
    /// the last one given when the row has a leg, its default when it has
    /// one and none was given, else none. A missing required leg, or a leg
    /// flag that is another row's, is the refusal's text.
    pub fn leg_of(&self, given: &[LegValue]) -> Result<Option<LegValue>, String> {
        let own = self.leg.kind();
        if let Some(other) = given.iter().find(|value| Some(value.leg()) != own) {
            let owner = SCENARIOS
                .iter()
                .find(|row| row.leg.kind() == Some(other.leg()))
                .map_or("no scenario", |row| row.name);
            let flag = other.leg().flag();
            return Err(match own {
                Some(own) => format!("{flag} is {owner}'s; {}'s leg is {}", self.name, own.flag()),
                None => format!("{flag} is {owner}'s; {} has no leg", self.name),
            });
        }
        match (self.leg, given.last()) {
            (LegCell::NoLeg, _) => Ok(None),
            (LegCell::Required(_) | LegCell::Defaulted(_), Some(value)) => Ok(Some(*value)),
            (LegCell::Defaulted(default), None) => Ok(Some(default)),
            (LegCell::Required(leg), None) => {
                Err(format!("{} needs {} {}", self.name, leg.flag(), leg.names().join("|")))
            }
        }
    }
}

/// The leg column's kinds: the eight legged scenarios' leg flags.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Leg {
    TierBudget,
    EpochMarks,
    LedgerWedge,
    ExtentWindow,
    Cold,
    Lifetimes,
    Pressure,
    PubTarget,
}

impl Leg {
    /// Every leg kind, in the order the help text lists their flags.
    pub const ALL: [Leg; 8] = [
        Leg::TierBudget,
        Leg::EpochMarks,
        Leg::ExtentWindow,
        Leg::LedgerWedge,
        Leg::Cold,
        Leg::Lifetimes,
        Leg::Pressure,
        Leg::PubTarget,
    ];

    /// The leg kind whose flag `flag` is.
    #[must_use]
    pub fn of_flag(flag: &str) -> Option<Leg> {
        Leg::ALL.into_iter().find(|leg| leg.flag() == flag)
    }

    /// Parses one of the leg's names; any other text is the refusal's.
    pub fn parse(self, name: &str) -> Result<LegValue, String> {
        let value = match self {
            Leg::TierBudget => TierBudgetLeg::parse(name).map(LegValue::TierBudget),
            Leg::EpochMarks => EpochMarksLeg::parse(name).map(LegValue::EpochMarks),
            Leg::LedgerWedge => LedgerWedgeLeg::parse(name).map(LegValue::LedgerWedge),
            Leg::ExtentWindow => ExtentWindowLeg::parse(name).map(LegValue::ExtentWindow),
            Leg::Cold => ColdLeg::parse(name).map(LegValue::Cold),
            Leg::Lifetimes => LifetimesLeg::parse(name).map(LegValue::Lifetimes),
            Leg::Pressure => PressureLeg::parse(name).map(LegValue::Pressure),
            Leg::PubTarget => PubTarget::parse(name).map(LegValue::PubTarget),
        };
        value.ok_or_else(|| {
            format!("unknown {} {name}; the legs are {}", self.flag(), self.names().join("|"))
        })
    }

    /// The flag that names the leg.
    #[must_use]
    pub const fn flag(self) -> &'static str {
        match self {
            Leg::TierBudget => "--tier-leg",
            Leg::EpochMarks => "--marks-leg",
            Leg::LedgerWedge => "--wedge-leg",
            Leg::ExtentWindow => "--window-leg",
            Leg::Cold => "--cold-leg",
            Leg::Lifetimes => "--lifetimes-leg",
            Leg::Pressure => "--pressure-leg",
            Leg::PubTarget => "--pub-target",
        }
    }

    /// The leg's names, in its enum's order: the first is the one a
    /// one-seed sweep of the row runs.
    #[must_use]
    pub fn names(self) -> Vec<&'static str> {
        match self {
            Leg::TierBudget => TierBudgetLeg::ALL.iter().map(|leg| leg.name()).collect(),
            Leg::EpochMarks => EpochMarksLeg::ALL.iter().map(|leg| leg.name()).collect(),
            Leg::LedgerWedge => LedgerWedgeLeg::ALL.iter().map(|leg| leg.name()).collect(),
            Leg::ExtentWindow => ExtentWindowLeg::ALL.iter().map(|leg| leg.name()).collect(),
            Leg::Cold => ColdLeg::ALL.iter().map(|(_, name)| *name).collect(),
            Leg::Lifetimes => LifetimesLeg::NAMES.to_vec(),
            Leg::Pressure => PressureLeg::ALL.iter().map(|leg| leg.name()).collect(),
            Leg::PubTarget => PubTarget::ALL.iter().map(|target| target.name()).collect(),
        }
    }
}

/// A leg flag's value, parsed: the leg kind and its typed leg, which the
/// scenario's constructor takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LegValue {
    TierBudget(TierBudgetLeg),
    EpochMarks(EpochMarksLeg),
    LedgerWedge(LedgerWedgeLeg),
    ExtentWindow(ExtentWindowLeg),
    Cold(ColdLeg),
    Lifetimes(LifetimesLeg),
    Pressure(PressureLeg),
    PubTarget(PubTarget),
}

impl LegValue {
    #[must_use]
    pub const fn leg(self) -> Leg {
        match self {
            LegValue::TierBudget(_) => Leg::TierBudget,
            LegValue::EpochMarks(_) => Leg::EpochMarks,
            LegValue::LedgerWedge(_) => Leg::LedgerWedge,
            LegValue::ExtentWindow(_) => Leg::ExtentWindow,
            LegValue::Cold(_) => Leg::Cold,
            LegValue::Lifetimes(_) => Leg::Lifetimes,
            LegValue::Pressure(_) => Leg::Pressure,
            LegValue::PubTarget(_) => Leg::PubTarget,
        }
    }

    /// `m4-cold`'s leg, when this value is one.
    #[must_use]
    pub const fn cold(self) -> Option<ColdLeg> {
        match self {
            LegValue::Cold(leg) => Some(leg),
            LegValue::TierBudget(_)
            | LegValue::EpochMarks(_)
            | LegValue::LedgerWedge(_)
            | LegValue::ExtentWindow(_)
            | LegValue::Lifetimes(_)
            | LegValue::Pressure(_)
            | LegValue::PubTarget(_) => None,
        }
    }

    /// `m1-pub-cross`'s leg, when this value is one.
    #[must_use]
    pub const fn pub_target(self) -> Option<PubTarget> {
        match self {
            LegValue::PubTarget(target) => Some(target),
            LegValue::TierBudget(_)
            | LegValue::EpochMarks(_)
            | LegValue::LedgerWedge(_)
            | LegValue::ExtentWindow(_)
            | LegValue::Cold(_)
            | LegValue::Lifetimes(_)
            | LegValue::Pressure(_) => None,
        }
    }
}

/// The registry's columns: every name on one row, every legged row's leg
/// kind on that row alone and in the kinds the flag parse reads.
#[cfg(test)]
mod registry {
    use super::{ColdLeg, Leg, LegValue, LifetimesLeg, SCENARIOS, ScenarioRow};

    #[test]
    fn every_name_and_every_leg_kind_is_on_one_row() {
        for row in SCENARIOS {
            let same = SCENARIOS.iter().filter(|other| other.name == row.name).count();
            assert_eq!(same, 1, "{} is on {same} rows", row.name);
            let Some(leg) = row.leg.kind() else { continue };
            let owners = SCENARIOS.iter().filter(|other| other.leg.kind() == Some(leg)).count();
            assert_eq!(owners, 1, "{} is the leg of {owners} rows", leg.flag());
            assert_eq!(Leg::of_flag(leg.flag()), Some(leg), "{} is not parsed", leg.flag());
            let first = leg.names()[0];
            assert_eq!(leg.parse(first).map(|value| value.leg()), Ok(leg));
        }
        assert!(Leg::TierBudget.parse("no-such-leg").unwrap_err().contains("the legs are"));
        assert_eq!(ScenarioRow::named("no-such-scenario"), None);
    }

    #[test]
    fn a_leg_is_its_row_alone() {
        let budget = ScenarioRow::named("m4-tier-budget").unwrap();
        let durable = ScenarioRow::named("m2-durable").unwrap();
        let tier = Leg::TierBudget.parse("two-8mb").unwrap();
        let marks = Leg::EpochMarks.parse("wedge").unwrap();
        assert_eq!(budget.leg_of(&[tier]), Ok(Some(tier)));
        assert_eq!(durable.leg_of(&[]), Ok(None));
        assert!(budget.leg_of(&[]).unwrap_err().contains("m4-tier-budget needs --tier-leg"));
        let refusal = durable.leg_of(&[tier]).unwrap_err();
        assert!(refusal.contains("--tier-leg is m4-tier-budget's; m2-durable has no leg"));
        let refusal = budget.leg_of(&[tier, marks]).unwrap_err();
        assert!(
            refusal.contains("--marks-leg is m4-epoch-marks's; m4-tier-budget's leg is --tier-leg"),
            "{refusal}"
        );
    }

    /// A defaulted leg runs its default when its flag is absent, the named
    /// leg when given, and is another row's leg everywhere else.
    #[test]
    fn a_defaulted_leg_runs_its_default_when_unnamed() {
        let cold = ScenarioRow::named("m4-cold").unwrap();
        let durable = ScenarioRow::named("m2-durable").unwrap();
        let dense = Leg::Cold.parse("dense").unwrap();
        assert_eq!(cold.leg_of(&[]), Ok(Some(LegValue::Cold(ColdLeg::Storm))));
        assert_eq!(cold.leg_of(&[dense]), Ok(Some(dense)));
        let refusal = durable.leg_of(&[dense]).unwrap_err();
        assert!(refusal.contains("--cold-leg is m4-cold's; m2-durable has no leg"), "{refusal}");
        let lifetimes = ScenarioRow::named("m4-ns-lifetimes").unwrap();
        let control = Leg::Lifetimes.parse("control").unwrap();
        assert_eq!(lifetimes.leg_of(&[]), Ok(Some(LegValue::Lifetimes(LifetimesLeg::Full))));
        assert_eq!(lifetimes.leg_of(&[control]), Ok(Some(control)));
        let refusal = lifetimes.leg_of(&[dense]).unwrap_err();
        assert!(
            refusal.contains("--cold-leg is m4-cold's; m4-ns-lifetimes's leg is --lifetimes-leg"),
            "{refusal}"
        );
    }
}

/// ADR-0107: the simulator's tests arm fault points and build forced
/// collisions; without the `dst` feature both are compiled to no-ops and
/// every scenario test would pass vacuously. This test turns a plain
/// `cargo test -p inf-sim` red (`cargo test --workspace` unifies the
/// features through the store/server dev-dependencies; `--features dst`
/// is the explicit form CI uses).
#[cfg(test)]
mod dst_build {
    // The constants are the point: a runtime-visible red under a plain
    // `cargo test -p inf-sim` (a `const` block would fail the *build* of
    // every workspace-wide command instead — ADR-0107 chose the test).
    #[allow(clippy::assertions_on_constants)]
    #[test]
    fn fault_points_and_collision_oracle_are_compiled_in() {
        assert!(
            inf_foundation::fault::COMPILED_IN,
            "inf-sim tests need the fault registry: run with `--features dst` (ADR-0107)"
        );
        assert!(
            inf_foundation::COLLISION_ORACLE,
            "inf-sim tests need the collision oracle: run with `--features dst` (ADR-0107)"
        );
    }
}
