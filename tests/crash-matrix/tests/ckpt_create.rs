//! A checkpoint's staging create refused before its file exists
//! (`ckpt_create_fail`, fired inside the simulated disk's create) in the
//! whole-node durable scenario, run in-process: the point's registry is
//! thread-local, so a real node cannot be armed from a test. The disk's
//! call log, read at the cut, shows the refused create left no file, its
//! cell renamed nothing onto its `MANIFEST` before its next walk's create,
//! and that later walk, begun by the automatic trigger (the scenario
//! requests no checkpoint), published; the power cut that ends the run
//! recovers every acknowledged write, which the scenario's durability audit
//! reads against the client's own ledger.

#[path = "../receipt.rs"]
mod receipt;

use inf_foundation::fault::{self, FaultSpec};
use inf_log::fs::sim::{CallResult, DiskCall, LoggedCall};
use inf_sim::{DurableScenario, run_durable_scenario};

#[test]
fn a_refused_staging_create_aborts_its_walk_and_a_cut_recovers() {
    let point = inf_log::fault::CKPT_CREATE_FAIL;
    for round in 0..crash_matrix::seed_count(2) {
        // Odd seeds run the write-through segment class, whose frames cross
        // the scenario's checkpoint interval several times a run.
        let seed = 0xC4EA_7E01 + 2 * round;
        fault::arm(point, FaultSpec::Nth(1));
        let report = run_durable_scenario(&DurableScenario::m2_durable(seed));
        let fired = fault::fired(point);
        fault::disarm_all();
        assert_eq!(fired, 1, "seed {seed:#x}: one staging create refused (the row is not vacuous)");
        assert!(report.ok(), "seed {seed:#x}: violations {:?}", report.violations);
        assert!(report.audited_keys > 0, "seed {seed:#x}: the cut's audit read no key");
        assert_eq!(report.disk_calls_unlogged, 0, "seed {seed:#x}: the call log is whole");
        a_later_walk_published(seed, &report.disk_calls);
    }
    receipt::verified("ckpt_create_fail", "walk-aborted-unit-kept");
}

/// The disk's call log at the cut, read for the verdict: the refused create
/// left no file; the next call its cell logged (by the mark it was made
/// under) is the create of another staging file, made, so no rename onto
/// the cell's `MANIFEST` came between (the refused walk aborted and a new
/// walk began); and a rename onto that `MANIFEST` follows it (the later
/// walk published).
fn a_later_walk_published(seed: u64, calls: &[LoggedCall]) {
    let injected = CallResult::Injected(inf_log::fault::CKPT_CREATE_FAIL);
    let Some(refused) = calls.iter().position(|call| call.result == injected) else {
        panic!("seed {seed:#x}: no refused create in the call log: {calls:#?}");
    };
    let refusal = &calls[refused];
    let made = DiskCall::CheckpointCreate { left_file: true };
    let none = DiskCall::CheckpointCreate { left_file: false };
    assert_eq!(refusal.call, none, "seed {seed:#x}: the refused create left a file");
    let Some(cell) = refusal.mark.map(|mark| mark.phase) else {
        panic!("seed {seed:#x}: the refusal came outside a cell's turn: {refusal:?}");
    };
    let mut later =
        calls[refused + 1..].iter().filter(|call| call.mark.is_some_and(|m| m.phase == cell));
    let Some(next) = later.next() else {
        panic!("seed {seed:#x}: {cell:?} logged nothing after its refusal: no later walk");
    };
    assert_eq!(next.call, made, "seed {seed:#x}: {cell:?}'s next call is no made create: {next:?}");
    assert_eq!(next.result, CallResult::Ok, "seed {seed:#x}: {next:?}");
    assert_ne!(next.path, refusal.path, "seed {seed:#x}: the refused walk's create was retried");
    let published =
        later.any(|call| call.call == DiskCall::ManifestRename && call.result == CallResult::Ok);
    assert!(published, "seed {seed:#x}: {cell:?}'s later walk never renamed its MANIFEST");
}
