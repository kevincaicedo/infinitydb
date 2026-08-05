//! M2-S17 crash-matrix runner (ADR-0020): executes every MemFs-tier row
//! of `m2.toml` — {fault point} × {fsync policy} × {workload} × seed —
//! as inject → crash (kill) → recover → verify. Node-tier rows are
//! counted for coverage and carried by their named test (fsyncgate).
//!
//! Per run: the point must fire (a vacuous row fails), the injection's
//! documented semantics must be observed, the recovered digest must
//! equal a reference replay of the surviving log, the typed recovery
//! outcome must match the row's `expect`, the surviving-state model must
//! match key-for-key, the per-policy ack contract must hold (`always`
//! loses zero acked writes), and recovery must be idempotent.
//!
//! Seeds per combination come from the definition (`seeds = N`); the
//! nightly expands via `CRASH_MATRIX_SEEDS`.

use std::path::Path;

use crash_matrix::{
    MatrixRow, assert_model, load_matrix, policy, recover, reference_replay, run_workload, workload,
};
use inf_log::fs::mem::MemFs;
use inf_store::FsyncClass;

fn matrix_path() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("m2.toml")
}

fn seed_count(default: u64) -> u64 {
    std::env::var("CRASH_MATRIX_SEEDS").ok().and_then(|value| value.parse().ok()).unwrap_or(default)
}

/// Every declared fault point must appear in the matrix (the §6 "every
/// named fault point covered" gate, self-policing like the CI inventory
/// check), and node-tier rows must name the test that carries them.
#[test]
fn every_declared_point_has_a_matrix_row() {
    let mut def = load_matrix(&matrix_path());
    // M4-S11 tier rows live in m4.toml (carried by tests/tier.rs) —
    // coverage spans both files, so a declared-but-rowless point fails
    // regardless of which milestone owns it.
    let m4 = load_matrix(&Path::new(env!("CARGO_MANIFEST_DIR")).join("m4.toml"));
    def.rows.extend(m4.rows);
    let declared: Vec<&str> =
        inf_log::fault::ALL.iter().chain(inf_server::fault::ALL).copied().collect();
    for point in declared {
        assert!(
            def.rows.iter().any(|row| row.point == point),
            "fault point {point:?} has no crash-matrix row (tests/crash-matrix/m2.toml + m4.toml)"
        );
    }
    for row in &def.rows {
        match row.tier.as_str() {
            "memfs" => {
                assert!(!row.policies.is_empty(), "row {:?}: no policies", row.point);
                assert!(!row.workloads.is_empty(), "row {:?}: no workloads", row.point);
            }
            "node" => assert!(
                !row.test.is_empty(),
                "node-tier row {:?} must name its carrying test",
                row.point
            ),
            other => panic!("row {:?}: unknown tier {other:?}", row.point),
        }
    }
}

#[test]
fn matrix_kill_and_recover() {
    let def = load_matrix(&matrix_path());
    let seeds = seed_count(def.seeds);
    let mut runs = 0u64;
    for (row_idx, row) in def.rows.iter().enumerate() {
        if row.tier != "memfs" {
            continue;
        }
        for policy_name in &row.policies {
            for workload_name in &row.workloads {
                for s in 0..seeds {
                    let seed = 0x0BAD_5EED ^ ((row_idx as u64) << 24) ^ (s << 1) ^ 1;
                    run_one(row, policy_name, workload_name, seed);
                    runs += 1;
                }
            }
        }
    }
    assert!(runs > 0, "the matrix executed no runs");
    eprintln!("crash matrix: {runs} kill-and-recover runs green ({seeds} seeds/combination)");
}

fn run_one(row: &MatrixRow, policy_name: &str, workload_name: &str, seed: u64) {
    let ctx = format!(
        "row {{point {}, policy {policy_name}, workload {workload_name}, seed {seed:#x}}}",
        row.point
    );
    let class = policy(policy_name);
    let wl = workload(workload_name);
    let fs = MemFs::new();
    let out = run_workload(&fs, &row.point, class, wl, seed);

    // The injection happened and looked like its documented semantics:
    // torn_frame succeeds silently (lying disk); every other point
    // surfaces a typed error at the site.
    assert!(out.fired >= 1, "{ctx}: the armed point never fired (vacuous run)");
    if row.point == "torn_frame" {
        assert!(out.torn_base.is_some(), "{ctx}: torn_frame fired without a torn frame");
        assert!(out.typed_error.is_none(), "{ctx}: torn_frame must lie silently");
    } else {
        assert!(
            out.typed_error.is_some(),
            "{ctx}: the point fired but no typed error was observed"
        );
    }

    // Reference before recovery: boot GC deletes below-floor segments
    // the reference replay still needs.
    let reference = reference_replay(&fs, class);
    let mut rec = recover(&fs, class, wl.segment_bytes);
    assert_eq!(
        rec.digest, reference,
        "{ctx}: recovery diverged from the reference replay of the surviving log"
    );

    match row.expect.as_str() {
        "torn-tail" => {
            let truncated = rec.stats.torn_truncated_at;
            assert!(truncated.is_some(), "{ctx}: expected a torn-tail truncation");
            if let Some(base) = out.torn_base {
                if base.offset == 0 {
                    // The lying frame rotated into a fresh segment:
                    // recovery truncates to the end of the last
                    // data-bearing segment and removes the torn one —
                    // the same resume point, expressed at the boundary.
                    let t = truncated.expect("asserted above");
                    assert!(
                        t.segment < base.segment && rec.stats.torn_segments_removed >= 1,
                        "{ctx}: boundary tear must truncate below {base} and remove the \
                         trailing segment (got {t}, removed {})",
                        rec.stats.torn_segments_removed
                    );
                } else {
                    assert_eq!(
                        truncated,
                        Some(base),
                        "{ctx}: truncation must land exactly at the lying frame's base"
                    );
                }
            }
        }
        "clean" => {
            assert_eq!(
                rec.stats.torn_truncated_at, None,
                "{ctx}: expected a clean end, recovery truncated"
            );
        }
        "unit-resolves" => {
            // Old xor new, never neither-when-one-was-committed, never
            // both-partial (§8.4): the recovered unit is the last fully
            // committed publication or the interrupted one (kill physics
            // keep a completed rename visible even when its dir barrier
            // failed) — and a failed *rename* always resolves old.
            let unit = rec.unit.map(|u| u.ckpt_id);
            assert!(
                unit == out.committed_ckpt || unit == out.attempted_ckpt,
                "{ctx}: recovered unit {unit:?} is neither committed {:?} nor attempted {:?}",
                out.committed_ckpt,
                out.attempted_ckpt
            );
            if row.point == "manifest_rename_fail" {
                assert_eq!(
                    unit, out.committed_ckpt,
                    "{ctx}: a failed rename must leave the old unit authoritative"
                );
            }
        }
        other => panic!("{ctx}: unknown expect {other:?}"),
    }

    // The durability oracle at kill tier: the surviving-state model
    // matches key-for-key, and the per-policy ack contract holds.
    assert_model(&mut rec, &out.model, &ctx);
    match class {
        FsyncClass::Always => assert_eq!(
            out.acked_lost, 0,
            "{ctx}: an `always` ack was lost — §8.2 violated at kill tier"
        ),
        FsyncClass::Everysec => assert!(
            // Bounded by one staged-but-unflushed batch (+ the torn frame):
            // the ≤ 1 s window is structural here; the ack-stream oracle
            // binds at S19.
            out.acked_lost <= 64,
            "{ctx}: everysec lost {} acked records — beyond one batch",
            out.acked_lost
        ),
    }

    // Recovery is idempotent: a second boot of the same directory agrees
    // and boot GC has nothing left to do.
    let rec2 = recover(&fs, class, wl.segment_bytes);
    assert_eq!(rec2.digest, rec.digest, "{ctx}: re-recovery digest diverged");
    assert_eq!(rec2.stats.stale_files_removed, 0, "{ctx}: boot GC is not idempotent");
}
