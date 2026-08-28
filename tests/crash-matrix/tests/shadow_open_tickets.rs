//! M4.5-S37 (ADR-0093 A8, review of 2026-08-28): the whole-node
//! open-ticket rows — the `m4-tiered` scenario's phase 6d — run
//! in-process on one shadow-arm seed: with the reconciler paused, every
//! plane path meets a ticket that is genuinely open (`GET`, two `DBSIZE`
//! drains in flight together, `SCAN`'s twin, a second `SET`, `DEL`'s
//! read-free forced resolution, the `Ticketed` refusal, the collision
//! verdict) and the injected twin-read failure
//! (`inf_server::fault::SHADOW_TWIN_READ_FAIL`, thread-local — a real
//! node cannot be armed from a test) is relayed by `DBSIZE` as the typed
//! error. The sweep runs this on every arm seed; this row is the
//! `cargo test` witness that the paths are exercised, not disclosed.

use inf_sim::tiered::{TieredScenario, run_tiered_scenario};

#[test]
fn open_ticket_rows_hold_every_plane_path_with_tickets_open() {
    // A shadow-arm seed (`seed % 4 != 3`).
    let report = run_tiered_scenario(&TieredScenario::m4_tiered(0x5EED_0001));
    assert!(report.ok(), "violations: {:?}", report.violations);
    assert!(report.shadow_arm && report.open_rows, "the arm and its open-ticket rows ran");
    assert!(report.open_tickets >= 4, "same-key tickets held open: {}", report.open_tickets);
    assert!(report.open_dbsize_drains >= 1 && report.open_dbsize_reads >= 4, "{report:?}");
    assert!(report.open_scan_twins >= 4, "SCAN twins: {}", report.open_scan_twins);
    assert!(report.open_retargeted >= 4, "retargets: {}", report.open_retargeted);
    assert!(report.open_forced_deletes >= 2, "forced deletes: {}", report.open_forced_deletes);
    assert!(report.open_ticketed_fallbacks >= 4, "{}", report.open_ticketed_fallbacks);
    assert!(report.open_collision_verdicts >= 4, "{}", report.open_collision_verdicts);
    assert!(
        report.open_read_fault_errors >= 1,
        "fault::SHADOW_TWIN_READ_FAIL fired and DBSIZE relayed the typed error: {}",
        report.open_read_fault_errors
    );
    assert!(report.open_settled_without_read >= 1, "{}", report.open_settled_without_read);
}
