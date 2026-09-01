//! M0-S15 AC: the command × edge-case matrix replied byte-identical to real
//! Redis (allowlisted introspection payloads excepted, per the AC).
//!
//! Spawns a throwaway `redis-server` (no persistence) as the oracle and the
//! in-process executor as the candidate, runs the scripted matrix on both,
//! and diffs raw reply bytes per the case's `Check` mode. Skips (with a loud
//! marker) when `redis-server` is not installed.
//!
//! Oracle pinning (M1-S14): when `INF_COMPAT_ORACLE_ADDR=host:port` is set,
//! the harness connects to that server instead of spawning one — CI runs the
//! pinned `redis:8.0.5` container (started with `--enable-debug-command yes`,
//! no persistence) so the oracle version can never drift with the runner's
//! apt archive. The local dev path (spawn from PATH) is unchanged.
//!
//! The real-node lane (`INFINITYD_BIN`, review 2026-08-30 F-L19-09) runs
//! the same matrix against a spawned multi-cell `infinityd` in
//! `tests/node_diff.rs`; the process plumbing and the compare loop are
//! shared via `compat::harness`.

use compat::candidate::Candidate;
use compat::harness::{oracle, run_matrix};
use compat::matrix::MATRIX;

#[test]
fn matrix_replies_match_redis() {
    let Some((_guard, mut oracle)) = oracle() else {
        eprintln!("SKIPPED: redis-server not installed — compat AC stays evidence-pending");
        return;
    };
    let mut candidate = Candidate::new();
    let report = run_matrix(MATRIX, &mut oracle, &[], |wire, _frames| candidate.execute_wire(wire));

    println!(
        "compat-diff v1: {} byte-compared cases, {} documented deviations, {} failures",
        report.compared,
        report.skipped,
        report.failures.len()
    );
    assert!(
        report.failures.is_empty(),
        "{} mismatches vs real Redis:\n{}",
        report.failures.len(),
        report.failures.join("\n")
    );
}
