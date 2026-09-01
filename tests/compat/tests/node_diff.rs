//! The real-node compat lanes (review 2026-08-30, F-L19-09 — Group 0
//! item 2): the candidate is a spawned multi-cell `infinityd` behind a
//! TCP socket, not the in-process single-cell `Keyspace`. Before this
//! lane every `full` declaration in the matrix was proven in a topology
//! where fan-out is a no-op — the exact configuration class (cells,
//! named namespaces, tier) where the review's proven Criticals lived.
//!
//! Two lanes:
//! - `node_matrix_replies_match_redis`: the whole scripted `MATRIX`,
//!   byte-diffed against the redis-server oracle, on a 4-cell durable
//!   node — every existing compat case now also proven where fan-out,
//!   the control plane and the durable root are real.
//! - `node_fanout_and_tier_match_redis_under_namespace`: a
//!   namespace-bound connection (`INF.NS USE` on a durable **tiered**
//!   namespace) driving the scatter/fan-out surface — SCAN/KEYS/
//!   DBSIZE/FLUSHDB/FLUSHALL/RANDOMKEY — with boundary-length keys and
//!   values across the 16,368 B cold window, set-equality where reply
//!   order is a documented deviation, byte-exact everywhere else.
//!   Honesty note: values ride the tiered write/read path but the lane
//!   does not force demotion — cold-read-after-demotion byte fidelity
//!   stays with the m4-tiered DST lane and the N1 e2e.
//!
//! Gating: `INFINITYD_BIN` names the binary (set by `just compat` and
//! CI); unset skips loudly. The redis oracle follows the diff.rs rules.

use std::collections::BTreeSet;
use std::io::Write;
use std::net::TcpStream;
use std::path::Path;

use compat::harness::{CaseOverride, Expect, infinityd, oracle, read_frames, run_matrix};
use compat::matrix::MATRIX;
use compat::resp::encode_command;

fn scratch_base() -> &'static Path {
    Path::new(env!("CARGO_TARGET_TMPDIR"))
}

/// The pinned node-topology divergences. Consulted only when the default
/// compare fails (so the mid-script `FLUSHALL` — before any durable
/// namespace exists — still byte-compares), and each pins exact bytes or
/// an exact shape: drift inside a deviation fails the lane.
const NODE_OVERRIDES: &[CaseOverride] = &[CaseOverride {
    // ADR-0015's recorded M2 cut: once the script has created a
    // durable namespace, node-wide FLUSHALL refuses typed.
    argv: &["FLUSHALL"],
    expect: Expect::CandidateExact(
        b"-ERR FLUSHALL on a node with durable namespaces is not yet supported (M2)\r\n",
    ),
    why: "FLUSHALL with durable namespaces refuses (ADR-0015 M2 cut)",
}];

#[test]
fn node_matrix_replies_match_redis() {
    let Some((_node_guard, mut node)) = infinityd(4, scratch_base()) else {
        eprintln!("SKIPPED: INFINITYD_BIN unset — real-node compat lane not run (F-L19-09)");
        return;
    };
    let Some((_oracle_guard, mut oracle)) = oracle() else {
        eprintln!("SKIPPED: redis-server not installed — compat AC stays evidence-pending");
        return;
    };
    let mut node_buf = Vec::new();
    let report = run_matrix(MATRIX, &mut oracle, NODE_OVERRIDES, |wire, frames| {
        node.write_all(wire).expect("node write");
        read_frames(&mut node, &mut node_buf, frames)
    });
    println!(
        "compat-diff node lane: {} byte-compared cases on a 4-cell durable node, \
         {} documented deviations, {} pinned node deviations, {} failures",
        report.compared,
        report.skipped,
        report.deviations.len(),
        report.failures.len()
    );
    for line in &report.deviations {
        println!("  node deviation: {line}");
    }
    assert!(
        report.failures.is_empty(),
        "{} real-node mismatches vs real Redis:\n{}",
        report.failures.len(),
        report.failures.join("\n")
    );
    // The pinned list is exact: a fixed divergence must retire its
    // override (a stale excuse is a lie), a new one must be filed. The
    // FLUSHALL deviation always fires; N4 (the cross-cell self-delivery
    // permutation) retired with ADR-0101 — its two overrides are gone,
    // so the self-subscribed PUBLISH cases byte-compare on every boot.
    assert_eq!(
        report.deviations.len(),
        1,
        "pinned node deviations drifted:\n{}",
        report.deviations.join("\n")
    );
}

/// Sends one command and reads one reply frame.
fn cmd(stream: &mut TcpStream, buf: &mut Vec<u8>, argv: &[&str]) -> Vec<u8> {
    let owned: Vec<String> = argv.iter().map(|s| (*s).to_string()).collect();
    stream.write_all(&encode_command(&owned)).expect("write");
    read_frames(stream, buf, 1)
}

/// Both engines must answer these exact bytes.
fn assert_pair(oracle_reply: &[u8], node_reply: &[u8], label: &str, failures: &mut Vec<String>) {
    if oracle_reply != node_reply {
        failures.push(format!(
            "{label}:\n  oracle    {:?}\n  candidate {:?}",
            String::from_utf8_lossy(&oracle_reply[..oracle_reply.len().min(120)]),
            String::from_utf8_lossy(&node_reply[..node_reply.len().min(120)]),
        ));
    }
}

/// Parses `*N` of `$len` bulks into the element list.
fn parse_bulk_array(reply: &[u8]) -> Option<Vec<Vec<u8>>> {
    let header_end = reply.windows(2).position(|w| w == b"\r\n")? + 2;
    let count: usize = std::str::from_utf8(reply.get(1..header_end - 2)?).ok()?.parse().ok()?;
    if reply.first() != Some(&b'*') {
        return None;
    }
    let mut at = header_end;
    let mut items = Vec::with_capacity(count);
    for _ in 0..count {
        let rest = reply.get(at..)?;
        if rest.first() != Some(&b'$') {
            return None;
        }
        let len_end = rest.windows(2).position(|w| w == b"\r\n")? + 2;
        let len: usize = std::str::from_utf8(rest.get(1..len_end - 2)?).ok()?.parse().ok()?;
        items.push(rest.get(len_end..len_end + len)?.to_vec());
        at += len_end + len + 2;
    }
    (at == reply.len()).then_some(items)
}

/// Parses a `SCAN` reply — `*2` of (cursor bulk, key array).
fn parse_scan(reply: &[u8]) -> Option<(Vec<u8>, Vec<Vec<u8>>)> {
    let rest = reply.strip_prefix(b"*2\r\n")?;
    if rest.first() != Some(&b'$') {
        return None;
    }
    let len_end = rest.windows(2).position(|w| w == b"\r\n")? + 2;
    let len: usize = std::str::from_utf8(rest.get(1..len_end - 2)?).ok()?.parse().ok()?;
    let cursor = rest.get(len_end..len_end + len)?.to_vec();
    let keys = parse_bulk_array(rest.get(len_end + len + 2..)?)?;
    Some((cursor, keys))
}

/// Full cursor walk: every page's keys, until the terminating `0`.
fn scan_all(stream: &mut TcpStream, buf: &mut Vec<u8>, label: &str) -> BTreeSet<Vec<u8>> {
    let mut cursor = b"0".to_vec();
    let mut keys = BTreeSet::new();
    for _ in 0..10_000 {
        let cursor_text = String::from_utf8(cursor).expect("ASCII cursor");
        let reply = cmd(stream, buf, &["SCAN", &cursor_text, "COUNT", "10"]);
        let (next, page) =
            parse_scan(&reply).unwrap_or_else(|| panic!("{label}: malformed SCAN reply"));
        keys.extend(page);
        if next == b"0" {
            return keys;
        }
        cursor = next;
    }
    panic!("{label}: SCAN never terminated");
}

/// One phase of the namespace lane: a deterministic key/value corpus on
/// one candidate namespace vs one oracle db — single-key string ops and
/// DBSIZE byte-exact, SCAN set-equality, cleanup via per-key DEL so both
/// engines end the phase empty. `keys_supported` gates KEYS/RANDOMKEY
/// (refused on tiered namespaces — the declared M4 string-family cut).
#[allow(clippy::too_many_lines)]
fn run_ns_phase(
    oracle: &mut TcpStream,
    ob: &mut Vec<u8>,
    node: &mut TcpStream,
    nb: &mut Vec<u8>,
    phase: &str,
    keys_supported: bool,
    failures: &mut Vec<String>,
) {
    // 48 short keys + 4 at the 255-byte MAX_KEY_LEN boundary; values
    // small, 17,000 B (over the 16,368 B cold window) and 65,536 B.
    let mut keys: Vec<String> = (0..48).map(|i| format!("k:{i:02}")).collect();
    for i in 0..4 {
        keys.push(format!("{}{i:02}", "K".repeat(253)));
    }
    let value_for = |i: usize, key: &str| match i % 3 {
        0 => format!("v:{key}"),
        1 => format!("m:{key}:").repeat(17_000 / (key.len() + 3) + 1)[..17_000].to_string(),
        _ => format!("b:{key}:").repeat(65_536 / (key.len() + 3) + 1)[..65_536].to_string(),
    };
    for (i, key) in keys.iter().enumerate() {
        let value = value_for(i, key);
        let argv = ["SET", key.as_str(), value.as_str()];
        let o = cmd(oracle, ob, &argv);
        let n = cmd(node, nb, &argv);
        assert_pair(&o, &n, &format!("{phase}: SET {key}"), failures);
    }
    // Every value read back byte-exact — cross-cell; over-window sizes
    // ride the tiered namespace's write/read path in the tier phase.
    for (i, key) in keys.iter().enumerate() {
        let o = cmd(oracle, ob, &["GET", key]);
        let n = cmd(node, nb, &["GET", key]);
        assert_pair(&o, &n, &format!("{phase}: GET {key} (size class {})", i % 3), failures);
    }
    // Single-key surface + the scattered aggregate, byte-exact.
    // (Cross-cell multi-key commands on named namespaces are the
    // declared M2 refusal — pinned below, not compared.)
    for argv in [
        &["DBSIZE"][..],
        &["STRLEN", "k:01"][..],
        &["EXISTS", "k:00"][..],
        &["TYPE", "k:00"][..],
        &["APPEND", "k:01", "-tail"][..],
        &["STRLEN", "k:01"][..],
        &["GET", "k:01"][..],
        &["GETRANGE", "k:02", "5", "-2"][..],
        &["TOUCH", "k:03"][..],
        &["DEL", "k:47"][..],
        &["DBSIZE"][..],
    ] {
        let o = cmd(oracle, ob, argv);
        let n = cmd(node, nb, argv);
        assert_pair(&o, &n, &format!("{phase}: {argv:?}"), failures);
    }
    // SCAN — full cursor walk on each engine must enumerate the same
    // set (the C1 shape: a fan-out serving one cell returns a quarter).
    let o_scan = scan_all(oracle, ob, "oracle");
    let n_scan = scan_all(node, nb, "candidate");
    assert_eq!(
        o_scan,
        n_scan,
        "{phase}: SCAN walks diverge (candidate missing: {:?}; extra: {:?})",
        o_scan.difference(&n_scan).collect::<Vec<_>>(),
        n_scan.difference(&o_scan).collect::<Vec<_>>()
    );
    if keys_supported {
        // KEYS * — order is the documented deviation, the SET must agree.
        let o_keys = parse_bulk_array(&cmd(oracle, ob, &["KEYS", "*"]))
            .expect("oracle KEYS reply")
            .into_iter()
            .collect::<BTreeSet<_>>();
        let n_keys = parse_bulk_array(&cmd(node, nb, &["KEYS", "*"]))
            .expect("candidate KEYS reply")
            .into_iter()
            .collect::<BTreeSet<_>>();
        assert_eq!(
            o_keys,
            n_keys,
            "{phase}: KEYS * key sets diverge (candidate missing: {:?}; extra: {:?})",
            o_keys.difference(&n_keys).collect::<Vec<_>>(),
            n_keys.difference(&o_keys).collect::<Vec<_>>()
        );
        assert_eq!(o_scan, o_keys, "{phase}: oracle SCAN vs KEYS disagree — harness bug");
        // RANDOMKEY — two-level random is the documented deviation; the
        // guarantee that survives it is membership.
        let random = cmd(node, nb, &["RANDOMKEY"]);
        let member = parse_bulk_array(&[b"*1\r\n", &random[..]].concat())
            .and_then(|mut v| v.pop())
            .unwrap_or_else(|| panic!("{phase}: candidate RANDOMKEY not a bulk: {random:?}"));
        assert!(n_keys.contains(&member), "{phase}: RANDOMKEY answered a non-resident key");
    }
    // Cleanup: per-key DEL byte-exact (FLUSHDB on a named namespace is
    // the declared M2 refusal), both engines end the phase empty.
    for key in keys.iter().filter(|k| *k != "k:47") {
        let o = cmd(oracle, ob, &["DEL", key]);
        let n = cmd(node, nb, &["DEL", key]);
        assert_pair(&o, &n, &format!("{phase}: DEL {key}"), failures);
    }
    let o = cmd(oracle, ob, &["DBSIZE"]);
    let n = cmd(node, nb, &["DBSIZE"]);
    assert_pair(&o, &n, &format!("{phase}: empty DBSIZE"), failures);
}

#[test]
fn node_fanout_and_tier_match_redis_under_namespace() {
    let Some((_node_guard, mut node)) = infinityd(4, scratch_base()) else {
        eprintln!("SKIPPED: INFINITYD_BIN unset — real-node compat lane not run (F-L19-09)");
        return;
    };
    let Some((_oracle_guard, mut oracle)) = oracle() else {
        eprintln!("SKIPPED: redis-server not installed — compat AC stays evidence-pending");
        return;
    };
    let (mut ob, mut nb) = (Vec::new(), Vec::new());
    let mut failures: Vec<String> = Vec::new();

    // Phase 1 — durable namespace, connection bound via INF.NS USE: the
    // exact configuration F-L19-09 proved no gate exercises. Oracle
    // stays on its plain db 0.
    for preamble in [
        &["INF.NS", "CREATE", "plain", "MODE", "durable", "FSYNC", "everysec"][..],
        &["INF.NS", "USE", "plain"][..],
    ] {
        let reply = cmd(&mut node, &mut nb, preamble);
        assert_eq!(reply, b"+OK\r\n", "preamble {preamble:?} failed");
    }
    run_ns_phase(&mut oracle, &mut ob, &mut node, &mut nb, "plain", true, &mut failures);

    // Phase 2 — durable **tiered** namespace (MEM-BUDGET), oracle on a
    // fresh db. KEYS/RANDOMKEY are the declared M4 tiered cut (pinned
    // below); over-window values ride the tiered write/read path.
    // Honesty note: demotion is not forced here — cold-read-after-
    // demotion byte fidelity stays with the m4-tiered DST lane.
    assert_eq!(cmd(&mut oracle, &mut ob, &["SELECT", "1"]), b"+OK\r\n");
    for preamble in [
        &[
            "INF.NS",
            "CREATE",
            "tier",
            "MODE",
            "durable",
            "MEM-BUDGET",
            "64mb",
            "DISK-BUDGET",
            "256mb",
        ][..],
        &["INF.NS", "USE", "tier"][..],
    ] {
        let reply = cmd(&mut node, &mut nb, preamble);
        assert_eq!(reply, b"+OK\r\n", "preamble {preamble:?} failed");
    }
    run_ns_phase(&mut oracle, &mut ob, &mut node, &mut nb, "tier", false, &mut failures);

    // The declared named-namespace cuts, pinned byte-exact so drift in a
    // refusal is caught (the candidate side only — the oracle has no
    // namespaces to compare against).
    for (argv, expected) in [
        (
            &["FLUSHDB"][..],
            &b"-ERR FLUSHDB on a named namespace is not yet supported (M2, ADR-0015)\r\n"[..],
        ),
        (
            // 16 distinct keys: the key hash is secret-seeded (ADR-0094),
            // so no fixed pair provably spans cells — but P(16 keys all
            // on one of 4 cells) ≈ 4⁻¹⁵ per boot, negligible.
            &[
                "EXISTS", "s:0", "s:1", "s:2", "s:3", "s:4", "s:5", "s:6", "s:7", "s:8", "s:9",
                "s:a", "s:b", "s:c", "s:d", "s:e", "s:f",
            ][..],
            &b"-ERR multi-key commands spanning cells are not yet supported in named namespaces (M2)\r\n"[..],
        ),
        (
            &["KEYS", "*"][..],
            &b"-ERR this command is not supported on tiered namespaces in M4 (string family only)\r\n"[..],
        ),
        (
            &["RANDOMKEY"][..],
            &b"-ERR this command is not supported on tiered namespaces in M4 (string family only)\r\n"[..],
        ),
    ] {
        let n = cmd(&mut node, &mut nb, argv);
        if n != expected {
            failures.push(format!(
                "pinned refusal drifted for {argv:?}:\n  expected  {:?}\n  candidate {:?}",
                String::from_utf8_lossy(expected),
                String::from_utf8_lossy(&n),
            ));
        }
    }

    println!(
        "compat-diff ns lane: 2 namespaces (durable + tiered) × 52 keys (4 at MAX_KEY_LEN), \
         values to 64 KiB, SCAN set-equality, 4 pinned refusals, {} failures",
        failures.len()
    );
    assert!(failures.is_empty(), "{} ns-lane mismatches:\n{}", failures.len(), failures.join("\n"));
}
