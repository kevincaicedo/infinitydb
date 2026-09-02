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
//! single thread form a real total order).

pub mod backfill;
pub mod bootstorm;
pub mod coldstorm;
pub mod combined;
pub mod diskfull;
mod document;
pub mod durable;
pub mod harness;
pub mod net;
pub mod nscreate;
pub mod nsddl;
pub mod pressure;
pub mod recovery;
pub mod resp;
pub mod sidecar;
pub mod steel;
pub mod tiered;

pub use backfill::{BackfillReport, BackfillScenario, run_backfill_scenario};
pub use bootstorm::{BootStormReport, BootStormScenario, run_boot_storm_scenario};
pub use coldstorm::{ColdStormReport, ColdStormScenario, run_cold_storm_scenario};
pub use combined::{CombinedReport, CombinedScenario, run_combined_scenario};
pub use diskfull::{DiskfullReport, DiskfullScenario, run_diskfull_scenario};
pub use durable::{DurableReport, DurableScenario, run_durable_scenario};
pub use harness::{Scenario, SimReport, run_scenario};
pub use nscreate::{NsCreateWindowReport, run_ns_create_window_scenario};
pub use nsddl::{NsDdlRaceReport, run_ns_ddl_race_scenario};
pub use pressure::{PressureReport, PressureScenario, run_pressure_scenario};
pub use recovery::{RecoveryReport, RecoveryScenario, run_recovery_scenario};
pub use sidecar::{SidecarReport, SidecarScenario, run_sidecar_scenario};
pub use steel::{SteelReport, SteelScenario, run_steel_scenario};
pub use tiered::{TieredNodeReport, TieredScenario, run_tiered_scenario};

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
