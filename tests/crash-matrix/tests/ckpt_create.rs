//! A checkpoint's staging create refused before its file exists
//! (`ckpt_create_fail`, fired inside the simulated disk's create) in the
//! whole-node durable scenario, run in-process: the point's registry is
//! thread-local, so a real node cannot be armed from a test. The walk
//! aborts, the trigger retries after its backoff, and the power cut that
//! ends the run recovers every acknowledged write, which the scenario's
//! durability audit reads against the client's own ledger.

#[path = "../receipt.rs"]
mod receipt;

use inf_foundation::fault::{self, FaultSpec};
use inf_sim::{DurableScenario, run_durable_scenario};

#[test]
fn a_refused_staging_create_aborts_its_walk_and_a_cut_recovers() {
    for round in 0..crash_matrix::seed_count(2) {
        // Odd seeds run the write-through segment class, whose frames cross
        // the scenario's checkpoint interval several times a run.
        let seed = 0xC4EA_7E01 + 2 * round;
        fault::arm(inf_log::fault::CKPT_CREATE_FAIL, FaultSpec::Nth(1));
        let report = run_durable_scenario(&DurableScenario::m2_durable(seed));
        let fired = fault::fired(inf_log::fault::CKPT_CREATE_FAIL);
        fault::disarm_all();
        assert_eq!(fired, 1, "seed {seed:#x}: one staging create refused (the row is not vacuous)");
        assert!(report.ok(), "seed {seed:#x}: violations {:?}", report.violations);
        assert!(report.audited_keys > 0, "seed {seed:#x}: the cut's audit read no key");
    }
    receipt::verified("ckpt_create_fail", "walk-aborted-unit-kept");
}
