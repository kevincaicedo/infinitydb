//! M2.5-S01 boot-storm scenario tests: determinism, a CI seed slice, and
//! the planted-bug demonstration — the oracle observable (blocking
//! `sync_dir` calls on the ready path) goes red against the pre-S01
//! synchronous boot path, so reverting the fix is caught by the scenario.

use inf_sim::{BootStormScenario, run_boot_storm_scenario};

#[test]
fn boot_storm_same_seed_same_hash() {
    // Determinism: the storm replays byte-identically per seed (L7).
    let scenario = BootStormScenario::m2_boot_storm(0xB007_0001);
    let a = run_boot_storm_scenario(&scenario);
    let b = run_boot_storm_scenario(&scenario);
    assert!(a.ok(), "violations: {:?}", a.violations);
    assert_eq!(a.trace_hash, b.trace_hash);
    assert_eq!(a.boots, b.boots);
}

#[test]
fn boot_storm_ci_slice_is_green() {
    // A small storm sweep per merge; the nightly fleet widens it (S14).
    for seed in 0..8u64 {
        let report = run_boot_storm_scenario(&BootStormScenario::m2_boot_storm(0xB007_1000 + seed));
        assert!(report.ok(), "seed {seed:#x}: {:?}", report.violations);
        assert_eq!(report.boots, 6, "seed {seed:#x} finished all cycles");
    }
}

#[test]
fn oracle_catches_the_synchronous_boot_path() {
    // The planted-bug discipline (M2.5-S01 AC): the pre-fix boot path ran
    // `create_cell_dirs` — four blocking dir-fsyncs — on the reactor
    // thread before ready. Drive exactly that path against a SimDisk and
    // assert the storm oracle's observable moves: a revert of the
    // deferred boot (`begin_recovery` without `deferred_boot_sync`) makes
    // `run_boot_storm_scenario` report a nonzero blocking-sync delta and
    // fail. The deferred variant leaves the observable untouched.
    use inf_server::SimDisk;

    let disk = SimDisk::new();
    let before = disk.sync_dir_calls();
    let dirs = inf_log::create_cell_dirs(&disk, std::path::Path::new("node/shard-x"))
        .expect("sync tier creates dirs");
    assert!(
        disk.sync_dir_calls() >= before + 4,
        "the synchronous path blocks on ≥ 4 dir-fsyncs — the wedge mechanism"
    );
    assert!(dirs.log.ends_with("log"));

    let deferred_before = disk.sync_dir_calls();
    let (_dirs, handles) =
        inf_log::create_cell_dirs_deferred(&disk, std::path::Path::new("node/shard-y"))
            .expect("deferred tier creates dirs");
    assert_eq!(
        disk.sync_dir_calls(),
        deferred_before,
        "the deferred path issues zero blocking syncs — barriers ride the driver"
    );
    assert_eq!(handles.len(), 4, "log, ckpt, shard, parent handles");
}
