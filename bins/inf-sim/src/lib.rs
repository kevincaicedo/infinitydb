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

pub mod bootstorm;
pub mod combined;
mod document;
pub mod durable;
pub mod harness;
pub mod net;
pub mod resp;
pub mod steel;

pub use bootstorm::{BootStormReport, BootStormScenario, run_boot_storm_scenario};
pub use combined::{CombinedReport, CombinedScenario, run_combined_scenario};
pub use durable::{DurableReport, DurableScenario, run_durable_scenario};
pub use harness::{Scenario, SimReport, run_scenario};
pub use steel::{SteelReport, SteelScenario, run_steel_scenario};
