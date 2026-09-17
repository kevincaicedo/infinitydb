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
//! - `m2-*`, `m3-document`, `m2-combined`, `m4-tiered`, `m2-ns-create-window`, `m2-ns-ddl-race` —
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

pub mod backfill;
pub mod bootstorm;
pub mod coldstorm;
pub mod combined;
pub mod diskfull;
mod document;
pub mod durable;
pub mod harness;
mod lift;
#[allow(unsafe_code)]
pub mod net;
pub mod nscreate;
pub mod nsddl;
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

/// Every scenario name the `inf-sim` binary accepts (F-L19-03, review
/// 2026-08-30): the registry `scripts/sim-smoke.sh` runs once per merge
/// and `tests/lanes.rs` checks against the CLI dispatch, so a scenario
/// cannot be born unrun. Add a scenario here, in `main.rs`, and in the
/// smoke script together.
pub const SCENARIOS: &[&str] = &[
    "m0-smoke",
    "m0-adversarial",
    "m0-surface",
    "m0-fabric-fairness",
    "m0-admission",
    "m1-cache",
    "m2-durable",
    "m2-clean-stop",
    "m2-device-budget",
    "m2-mode-transition",
    "m2-reorder-window",
    "m2-fill-tick",
    "m2-group-hold",
    "m2-fua-pending",
    "m2-ckpt-refused",
    "m2-recycle",
    "m2-combined",
    "m2-ns-create-window",
    "m2-ns-ddl-race",
    "m3-document",
    "boot-storm",
    "m4-steel",
    "m4-pressure",
    "m4-cold",
    "m4-recovery",
    "m4-diskfull",
    "m4-tiered",
    "m45-backfill",
    "m45-sidecar",
];

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
