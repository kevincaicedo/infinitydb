//! M0-S15 AC: the command × edge-case matrix replied byte-identical to real
//! Redis (allowlisted introspection payloads excepted, per the AC).
//!
//! Spawns a throwaway `redis-server` (no persistence) as the oracle and the
//! in-process executor as the candidate, runs the scripted matrix on both,
//! and diffs raw reply bytes per the case's `Check` mode. Missing or invalid
//! Redis fails the test (F-L19-11).
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
    let (_guard, mut oracle) = oracle();
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

/// F-L19-10's falsifier at the harness: a candidate that enumerates one
/// key short, answers `KEYS` one key short, or draws a key the oracle
/// does not hold is caught by the set, walk and membership checks — the
/// three cases that were `SkipDiff` before this batch.
#[test]
fn a_lying_enumeration_is_caught_by_the_set_checks() {
    use compat::harness::{parse_bulk_array, parse_scan};
    use compat::matrix::{Case, Check};
    let (_guard, mut oracle) = oracle();
    // Seed both engines with the same keys through the honest prefix.
    let matrix = [
        Case { argv: &["SET", "s:1", "v"], check: Check::ByteExact },
        Case { argv: &["SET", "s:2", "v"], check: Check::ByteExact },
        Case { argv: &["SET", "s:3", "v"], check: Check::ByteExact },
        Case { argv: &["KEYS", "s:*"], check: Check::SetEqual },
        Case { argv: &["SCAN", "0"], check: Check::ScanWalk },
        Case { argv: &["RANDOMKEY"], check: Check::MemberOfKeys },
        Case { argv: &["FLUSHALL"], check: Check::ByteExact },
    ];
    let mut candidate = Candidate::new();
    let honest =
        run_matrix(&matrix, &mut oracle, &[], |wire, _frames| candidate.execute_wire(wire));
    assert!(honest.failures.is_empty(), "honest candidate failed: {:?}", honest.failures);

    // Drop `s:3` from every enumeration and draw a key the oracle lacks.
    let mut candidate = Candidate::new();
    let lying = run_matrix(&matrix, &mut oracle, &[], |wire, _frames| {
        let reply = candidate.execute_wire(wire);
        if wire.starts_with(b"*2\r\n$4\r\nKEYS") {
            let keys: Vec<Vec<u8>> = parse_bulk_array(&reply)
                .expect("array")
                .into_iter()
                .filter(|k| k != b"s:3")
                .collect();
            return encode_array(&keys);
        }
        if wire.starts_with(b"*2\r\n$4\r\nSCAN") {
            let (cursor, page) = parse_scan(&reply).expect("scan page");
            let page: Vec<Vec<u8>> = page.into_iter().filter(|k| k != b"s:3").collect();
            let mut out = b"*2\r\n".to_vec();
            out.extend(format!("${}\r\n", cursor.len()).into_bytes());
            out.extend(cursor);
            out.extend(b"\r\n");
            out.extend(encode_array(&page));
            return out;
        }
        if wire.starts_with(b"*1\r\n$9\r\nRANDOMKEY") {
            return b"$8\r\nnot:live\r\n".to_vec();
        }
        reply
    });
    let text = lying.failures.join("\n");
    for needle in ["array sets differ", "enumerate different sets", "not a live key"] {
        assert!(text.contains(needle), "the lying candidate passed {needle:?}:\n{text}");
    }
}

fn encode_array(items: &[Vec<u8>]) -> Vec<u8> {
    let mut out = format!("*{}\r\n", items.len()).into_bytes();
    for item in items {
        out.extend(format!("${}\r\n", item.len()).into_bytes());
        out.extend(item);
        out.extend(b"\r\n");
    }
    out
}
