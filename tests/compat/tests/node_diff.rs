#![allow(
    clippy::disallowed_methods,
    reason = "test-only: filesystem fixtures outside cell code (ADR-0144 D5)"
)]
//! The real-node compat lanes: the candidate is a spawned multi-cell
//! `infinityd` behind a TCP socket, not the in-process single-cell
//! `Keyspace`. On a single cell fan-out is a no-op, so a `full`
//! declaration proven there says nothing about the configurations where
//! cross-cell behavior differs (several cells, named namespaces, tier);
//! these lanes prove the declarations on them.
//!
//! Two lanes:
//! - `node_matrix_replies_match_redis`: the whole scripted `MATRIX`
//!   against the redis-server oracle on a 4-cell durable node, each case
//!   compared as its `Check` mode says (a `SkipDiff` case is not
//!   compared; a case whose compare fails passes only on its pinned
//!   node override, `FLUSHALL` once a durable namespace exists, on the
//!   candidate's bytes alone), so every compared case also runs where
//!   fan-out, the control plane and the durable root are real.
//! - `node_fanout_and_tier_match_redis_under_namespace`: a connection
//!   bound by `INF.NS USE` to a durable namespace, then to a durable
//!   **tiered** one, driving the scatter/fan-out surface with
//!   boundary-length keys and values across the 16,368 B cold window:
//!   single-key replies, `DBSIZE` and `DEL` byte-exact, the `SCAN` walk
//!   compared as a set and, on the plain namespace, `KEYS *` as a set and
//!   `RANDOMKEY` by membership, `DEBUG OBJECT` on each resident key by
//!   its reply prefix on the candidate alone; `FLUSHDB`, a spanning
//!   `EXISTS`, and `KEYS`/`RANDOMKEY` on the tiered namespace are
//!   refusals pinned on the candidate alone.
//!   Honesty note: values ride the tiered write/read path but the lane
//!   does not force demotion — cold-read-after-demotion byte fidelity
//!   stays with the m4-tiered DST lane and the N1 e2e.
//!
//! Gating: `INFINITYD_BIN` names the binary (set by `just compat` and
//! CI); unset skips loudly. The redis oracle follows the diff.rs rules.

// nextest-stall-default: each tiered phase writes at most 52 keys, values up
// to 64 KiB (about 1.5 MiB in all), in a window of at least its 64 MiB
// MEM-BUDGET per cell; no write parks (ADR-0053 D4), so the stall bound is
// never read.

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::path::Path;

use compat::harness::{
    CaseOverride, Expect, OracleGuard, candidate, infinityd, infinityd_at, oracle, parse_int_reply,
    read_frames, run_matrix, spawn_infinityd_at,
};
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
        b"-ERR FLUSHALL on a node with durable namespaces is not supported\r\n",
    ),
    why: "FLUSHALL with durable namespaces refuses (ADR-0015 M2 cut)",
}];

#[test]
fn node_matrix_replies_match_redis() {
    let Some((_node_guard, mut node)) = infinityd(4, scratch_base()) else {
        eprintln!("SKIPPED: INFINITYD_BIN unset — real-node compat lane not run (F-L19-09)");
        return;
    };
    let (_oracle_guard, mut oracle) = oracle();
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
    if let Err(e) = stream.write_all(&encode_command(&owned)) {
        panic!("write {argv:?} to {:?} failed: {e}", stream.peer_addr());
    }
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
#[allow(
    clippy::too_many_lines,
    reason = "shape: one linear phase script; splitting would scatter the \
     invariants"
)]
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
        // L12-01 (ADR-0104): `DEBUG OBJECT` must agree with `GET` on every
        // key wherever it lives — before, it probed the connection's cell
        // and answered "no such key" for the keys of every other cell (the
        // reply's address field is engine-internal, so the matrix case is
        // `SkipDiff` and could not see it). The missing-key error is
        // byte-exact against the oracle.
        for key in n_keys.iter() {
            let key = std::str::from_utf8(key).expect("ascii keys");
            let reply = cmd(node, nb, &["DEBUG", "OBJECT", key]);
            assert!(
                reply.starts_with(b"+Value at:"),
                "{phase}: DEBUG OBJECT {key} disagrees with GET/KEYS: {:?}",
                String::from_utf8_lossy(&reply)
            );
        }
        let o = cmd(oracle, ob, &["DEBUG", "OBJECT", "never-set:l12-01"]);
        let n = cmd(node, nb, &["DEBUG", "OBJECT", "never-set:l12-01"]);
        assert_pair(&o, &n, &format!("{phase}: DEBUG OBJECT missing"), failures);
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
    let (_oracle_guard, mut oracle) = oracle();
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
        (&["FLUSHDB"][..], &b"-ERR FLUSHDB on a named namespace is not supported\r\n"[..]),
        (
            // 16 distinct keys: the key hash is secret-seeded (ADR-0094),
            // so no fixed pair provably spans cells — but P(16 keys all
            // on one of 4 cells) ≈ 4⁻¹⁵ per boot, negligible.
            &[
                "EXISTS", "s:0", "s:1", "s:2", "s:3", "s:4", "s:5", "s:6", "s:7", "s:8", "s:9",
                "s:a", "s:b", "s:c", "s:d", "s:e", "s:f",
            ][..],
            &b"-ERR multi-key commands spanning cells are not supported in named \
                 namespaces\r\n"[..],
        ),
        (
            &["KEYS", "*"][..],
            &b"-ERR this command is not supported on tiered namespaces in M4 (string family \
                 only)\r\n"[..],
        ),
        (
            &["RANDOMKEY"][..],
            &b"-ERR this command is not supported on tiered namespaces in M4 (string family \
                 only)\r\n"[..],
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

/// Batch 45 (review 2026-08-30, F-L15-04): values up to the record bound
/// round-trip byte-exact on both engines — 2 MiB (the size the finding
/// proved closed the connection) and `MAX_VAL_LEN` under a 255-byte key.
/// One byte past `proto-max-bulk-len` is the pinned deviation: Redis
/// (512 MiB default) answers `+OK`; the node refuses from the bulk
/// header, exactly as Redis does past *its* cap, and closes.
#[test]
fn large_values_match_redis_up_to_the_bulk_cap() {
    let Some((_node_guard, mut node)) = infinityd(4, scratch_base()) else {
        eprintln!("SKIPPED: INFINITYD_BIN unset — real-node compat lane not run (F-L19-09)");
        return;
    };
    let (_oracle_guard, mut oracle) = oracle();
    let mut nb = Vec::new();
    let mut ob = Vec::new();
    let mut failures = Vec::new();
    let key255 = "k".repeat(255);
    for (label, key, len) in [
        ("2 MiB", "big:2mib", 2usize << 20),
        ("MAX_VAL_LEN under a 255-byte key", key255.as_str(), (16usize << 20) - 1),
    ] {
        let value = "v".repeat(len);
        let o = cmd(&mut oracle, &mut ob, &["SET", key, &value]);
        let n = cmd(&mut node, &mut nb, &["SET", key, &value]);
        assert_pair(&o, &n, &format!("SET {label}"), &mut failures);
        let o = cmd(&mut oracle, &mut ob, &["GET", key]);
        let n = cmd(&mut node, &mut nb, &["GET", key]);
        assert_pair(&o, &n, &format!("GET {label}"), &mut failures);
        let bulk_len = format!("${len}\r\n").len() + len + 2;
        assert_eq!(o.len(), bulk_len, "GET {label}: the whole value came back");
    }
    assert!(
        failures.is_empty(),
        "{} large-value mismatches:\n{}",
        failures.len(),
        failures.join("\n")
    );

    let over = (16usize << 20) + 1;
    let o = cmd(&mut oracle, &mut ob, &["SET", "over", &"v".repeat(over)]);
    assert_eq!(o, b"+OK\r\n", "the oracle admits one byte past our default cap");
    // Header only: the node rejects from the declared length, before any payload.
    node.write_all(format!("*3\r\n$3\r\nSET\r\n$4\r\nover\r\n${over}\r\n").as_bytes())
        .expect("write");
    let n = read_frames(&mut node, &mut nb, 1);
    assert_eq!(
        n,
        format!("-ERR Protocol error: invalid bulk length: {over} exceeds limit 16777216\r\n")
            .into_bytes(),
        "{:?}",
        String::from_utf8_lossy(&n)
    );
    let mut rest = Vec::new();
    std::io::Read::read_to_end(&mut node, &mut rest).expect("read to close");
    assert!(rest.is_empty(), "the protocol error closes the connection");
    println!("compat-diff large-value lane: 2 MiB + MAX_VAL_LEN byte-exact, 16 MiB + 1 pinned");
}

/// Batch 45 (review 2026-08-30, F-L15-06) at the binary: a spawned 4-cell
/// `infinityd` (the memory board wired, `memory_scope:node`) names every
/// `INFO` field once, so a flat-map client sees one scope per name.
#[test]
fn info_names_every_field_once_on_a_real_node() {
    let Some((_node_guard, mut node)) = infinityd(4, scratch_base()) else {
        eprintln!("SKIPPED: INFINITYD_BIN unset — real-node compat lane not run (F-L19-09)");
        return;
    };
    let mut nb = Vec::new();
    assert_eq!(cmd(&mut node, &mut nb, &["SET", "k", "v"]), b"+OK\r\n");
    let reply = cmd(&mut node, &mut nb, &["INFO"]);
    let text = String::from_utf8_lossy(&reply);
    let body = text.split_once("\r\n").expect("bulk header").1;
    let mut seen = std::collections::BTreeMap::<&str, usize>::new();
    for line in body.lines().filter(|l| !l.is_empty() && !l.starts_with('#')) {
        let (name, _) = line.split_once(':').unwrap_or_else(|| panic!("no ':' in {line:?}"));
        *seen.entry(name).or_default() += 1;
    }
    let twice: Vec<&str> = seen.iter().filter(|(_, n)| **n > 1).map(|(k, _)| *k).collect();
    assert!(twice.is_empty(), "INFO names a field more than once: {twice:?}");
    assert!(body.contains("memory_scope:node\r\n"), "{body}");
    assert!(body.contains("tripwire_scope:cell\r\n"), "{body}");
    assert!(body.contains("used_memory_doc_resident:"), "{body}");
    assert!(body.contains("\r\ndoc_resident_bytes:"), "{body}");

    let memory = cmd(&mut node, &mut nb, &["INFO", "memory"]);
    let memory = String::from_utf8_lossy(&memory);
    let tripwires = cmd(&mut node, &mut nb, &["INFO", "tripwires"]);
    let tripwires = String::from_utf8_lossy(&tripwires);
    assert!(memory.contains("memory_scope:node\r\n"));
    assert!(tripwires.contains("tripwire_scope:cell\r\n"));
    for (node_name, cell_name) in [
        ("used_memory_doc_resident", "doc_resident_bytes"),
        ("used_memory_doc_scratch", "doc_scratch_bytes"),
        ("used_memory_doc_path_cache", "doc_path_cache_bytes"),
        ("used_memory_idx_tree", "idx_tree_bytes"),
        ("used_memory_idx_slack", "idx_slack_bytes"),
    ] {
        assert!(memory.contains(&format!("\r\n{node_name}:")), "{memory}");
        assert!(!memory.contains(&format!("\r\n{cell_name}:")), "{memory}");
        assert!(tripwires.contains(&format!("\r\n{cell_name}:")), "{tripwires}");
        assert!(!tripwires.contains(&format!("\r\n{node_name}:")), "{tripwires}");
    }
}

/// Batch 49 (review 2026-08-30, F-L15-07) at the binary: a spawned 4-cell
/// `infinityd`'s `# Tripwires` carries no process-wide gauge; `process_rss`
/// renders once, in `# Memory`, beside `memory_scope:node` and
/// `used_memory_rss`. Pre-fix every cell's `INFO tripwires` repeated the
/// whole process's VmRSS under its `tripwire_scope:cell` line.
#[test]
fn info_tripwires_is_wholly_cell_scope_on_a_real_node() {
    let Some((_node_guard, mut node)) = infinityd(4, scratch_base()) else {
        eprintln!("SKIPPED: INFINITYD_BIN unset — real-node compat lane not run (F-L19-09)");
        return;
    };
    let mut nb = Vec::new();
    let body = |reply: Vec<u8>| -> String {
        let text = String::from_utf8_lossy(&reply);
        text.split_once("\r\n").expect("bulk header").1.to_string()
    };
    let tripwires = body(cmd(&mut node, &mut nb, &["INFO", "tripwires"]));
    assert!(tripwires.contains("tripwire_scope:cell\r\n"), "{tripwires}");
    assert!(
        !tripwires.contains("process_rss:"),
        "a process-wide gauge inside the cell-scope section: {tripwires}"
    );
    let memory = body(cmd(&mut node, &mut nb, &["INFO", "memory"]));
    assert!(memory.contains("memory_scope:node\r\n"), "{memory}");
    let field = |name: &str| -> u64 {
        memory
            .lines()
            .find_map(|l| l.strip_prefix(name).and_then(|r| r.strip_prefix(':')))
            .unwrap_or_else(|| panic!("missing {name}: {memory}"))
            .parse()
            .expect("u64")
    };
    assert!(field("process_rss") > 0, "{memory}");
    assert_eq!(field("process_rss"), field("used_memory_rss"), "{memory}");
}

/// Batch 46 (review of 2026-08-30, F-L13-08): `QUIT` on a namespace-bound
/// connection — the pump path — answers `+OK` and closes, as Redis does.
/// Pre-fix the spawned binary answered `+OK` and held the socket open
/// until the client's read timeout.
#[test]
fn quit_closes_a_namespace_bound_connection_like_redis() {
    let Some((_node_guard, mut node)) = infinityd(1, scratch_base()) else {
        eprintln!("SKIPPED: INFINITYD_BIN unset — real-node compat lane not run (F-L19-09)");
        return;
    };
    let (_oracle_guard, mut oracle) = oracle();
    let (mut ob, mut nb) = (Vec::new(), Vec::new());
    for preamble in
        [&["INF.NS", "CREATE", "cache", "MODE", "memory"][..], &["INF.NS", "USE", "cache"][..]]
    {
        assert_eq!(cmd(&mut node, &mut nb, preamble), b"+OK\r\n", "preamble {preamble:?}");
    }
    let o = cmd(&mut oracle, &mut ob, &["QUIT"]);
    let n = cmd(&mut node, &mut nb, &["QUIT"]);
    assert_eq!(o, n, "QUIT reply differs from Redis");
    for (who, stream) in [("redis", &mut oracle), ("node", &mut node)] {
        stream.set_read_timeout(Some(std::time::Duration::from_secs(5))).expect("timeout");
        let mut rest = Vec::new();
        match stream.read_to_end(&mut rest) {
            Ok(_) => assert!(rest.is_empty(), "{who}: bytes after QUIT's +OK: {rest:?}"),
            Err(e) => panic!("{who}: the server never closed the connection after QUIT ({e})"),
        }
    }
}

/// `QUIT` answers `+OK` once the replies before it are sent, then the server
/// closes the connection, and nothing behind the `QUIT` runs, inline or
/// queued (ADR-0123 A2 rule 3). Each shape is one write with ten `SET`s
/// behind the `QUIT`, on a two-cell node and on Redis: `QUIT` alone and
/// behind ten `SET`s on a connection bound by `INF.NS USE`, where every
/// command rides the connection's queue; behind a `DEL` of keys both cells
/// own, which queues it too; and inline, where the parse stops at it. Redis
/// reads the same bytes on a plain connection. A `QUIT` that keeps the queue
/// behind it lands the `SET`s and answers them.
#[test]
fn nothing_behind_a_quit_runs_like_redis() {
    let Some((_node_guard, mut node)) = infinityd(2, scratch_base()) else {
        eprintln!("SKIPPED: INFINITYD_BIN unset — real-node compat lane not run");
        return;
    };
    let (_oracle_guard, oracle) = oracle();
    let mut nb = Vec::new();
    let create = ["INF.NS", "CREATE", "cache", "MODE", "memory"];
    assert_eq!(cmd(&mut node, &mut nb, &create), b"+OK\r\n", "INF.NS CREATE");
    let mut failures = Vec::new();
    for shape in QuitShape::ALL {
        let ns = shape.bound().then_some("cache");
        let redis = quit_shape(&oracle, None, shape);
        let ours = quit_shape(&node, ns, shape);
        let behind = (redis.landed_behind, ours.landed_behind);
        if redis.replies != ours.replies || redis.landed != ours.landed || behind != (0, 0) {
            failures.push(format!(
                "{shape:?}: redis {:?}, {} landed before the QUIT, {} behind it, {:?}; node {:?}, \
                 {} landed before, {} behind, {:?}",
                String::from_utf8_lossy(&redis.replies),
                redis.landed,
                redis.landed_behind,
                redis.end,
                String::from_utf8_lossy(&ours.replies),
                ours.landed,
                ours.landed_behind,
                ours.end,
            ));
        }
        for (who, reading) in [("redis", &redis), ("node", &ours)] {
            if reading.end == End::Open {
                failures.push(format!("{shape:?}: {who} never closed the connection"));
            }
        }
    }
    assert!(failures.is_empty(), "QUIT differs from Redis:\n{}", failures.join("\n"));
}

// ---- Batch 50 (review 2026-08-30): F-L15-03 / F-L15-05 / F-L15-10 ----------

/// The `# …` header lines of an INFO body.
fn info_headers(reply: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(reply);
    let body = text.split_once("\r\n").map_or("", |(_, b)| b);
    body.lines().filter(|l| l.starts_with('#')).map(str::to_string).collect()
}

/// Batch 50 (review 2026-08-30, F-L15-10): `INFO <unknown>` is an empty
/// body on both engines (byte-exact `$0`); an unknown name beside a known
/// one selects the known one alone; `all` beside an unknown name is
/// everything. Pre-fix the spawned node answered its whole body.
#[test]
fn info_unknown_section_is_empty_like_redis() {
    let Some((_node_guard, mut node)) = infinityd(2, scratch_base()) else {
        eprintln!("SKIPPED: INFINITYD_BIN unset — real-node compat lane not run (F-L19-09)");
        return;
    };
    let (_oracle_guard, mut oracle) = oracle();
    let (mut ob, mut nb) = (Vec::new(), Vec::new());
    let o = cmd(&mut oracle, &mut ob, &["INFO", "nosuchsection"]);
    let n = cmd(&mut node, &mut nb, &["INFO", "nosuchsection"]);
    assert_eq!(o, b"$0\r\n\r\n", "oracle: {:?}", String::from_utf8_lossy(&o));
    assert_eq!(n, o, "node: {:?}", String::from_utf8_lossy(&n));
    let o = cmd(&mut oracle, &mut ob, &["INFO", "server", "nosuchsection"]);
    let n = cmd(&mut node, &mut nb, &["INFO", "server", "nosuchsection"]);
    assert_eq!(info_headers(&o), vec!["# Server"]);
    assert_eq!(info_headers(&n), vec!["# Server"], "{:?}", String::from_utf8_lossy(&n));
    let everything = info_headers(&cmd(&mut node, &mut nb, &["INFO"]));
    let n = cmd(&mut node, &mut nb, &["INFO", "nosuchsection", "all"]);
    assert_eq!(info_headers(&n), everything, "{:?}", String::from_utf8_lossy(&n));
}

/// Batch 50 (review 2026-08-30, F-L15-03) at the binary: a spawned 4-cell
/// `infinityd`'s `INFO keyspace` counts what `DBSIZE` counts — the node —
/// under `keyspace_scope:node`, and reads the same count Redis reads for
/// the same keys. Pre-fix one cell's share (≈ ¼) with no scope line.
#[test]
fn info_keyspace_counts_the_whole_node_like_dbsize() {
    let Some((_node_guard, mut node)) = infinityd(4, scratch_base()) else {
        eprintln!("SKIPPED: INFINITYD_BIN unset — real-node compat lane not run (F-L19-09)");
        return;
    };
    let (_oracle_guard, mut oracle) = oracle();
    let (mut ob, mut nb) = (Vec::new(), Vec::new());
    for i in 0..400u32 {
        let key = format!("kf:{i}");
        for (who, stream, buf) in [("redis", &mut oracle, &mut ob), ("node", &mut node, &mut nb)] {
            assert_eq!(cmd(stream, buf, &["SET", &key, "v"]), b"+OK\r\n", "{who} SET {key}");
        }
    }
    let keys_of = |reply: &[u8]| -> u64 {
        let text = String::from_utf8_lossy(reply);
        text.lines()
            .find_map(|l| l.strip_prefix("db0:keys="))
            .and_then(|r| r.split(',').next())
            .unwrap_or_else(|| panic!("no db0 line: {text}"))
            .parse()
            .expect("u64")
    };
    assert_eq!(cmd(&mut oracle, &mut ob, &["DBSIZE"]), b":400\r\n");
    assert_eq!(cmd(&mut node, &mut nb, &["DBSIZE"]), b":400\r\n");
    assert_eq!(keys_of(&cmd(&mut oracle, &mut ob, &["INFO", "keyspace"])), 400);
    // Peers publish on their MAINTAIN cadence: poll, bounded.
    #[allow(clippy::disallowed_methods)] // test harness thread, not cell code
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let reply = cmd(&mut node, &mut nb, &["INFO", "keyspace"]);
        let text = String::from_utf8_lossy(&reply).to_string();
        let keys = keys_of(&reply);
        assert!(
            text.contains("keyspace_scope:"),
            "no scope line, db0:keys={keys} vs DBSIZE 400 (×{:.2}): {text}",
            keys as f64 / 400.0
        );
        if keys == 400 && text.contains("keyspace_scope:node\r\n") {
            break;
        }
        #[allow(clippy::disallowed_methods)] // test harness thread, not cell code
        let overdue = std::time::Instant::now() >= deadline;
        assert!(!overdue, "db0:keys={keys} vs DBSIZE 400: {text}");
        #[allow(clippy::disallowed_methods)] // test harness thread, not cell code
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// A second client to whatever `stream` is connected to.
fn sibling(stream: &TcpStream) -> TcpStream {
    let peer = stream.peer_addr().expect("peer addr");
    let s = TcpStream::connect(peer).expect("connect");
    s.set_read_timeout(Some(std::time::Duration::from_secs(5))).expect("timeout");
    s
}

/// Closed by the server — FIN, or RST when it closed with our PING still
/// unread (a refused accept never reads its socket).
fn assert_closed_or_reset(stream: &mut TcpStream, who: &str) {
    let mut rest = Vec::new();
    match stream.read_to_end(&mut rest) {
        Ok(_) => assert!(rest.is_empty(), "{who}: bytes after the refusal: {rest:?}"),
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => {}
        Err(e) => panic!("{who} did not close the refused connection ({e})"),
    }
}

/// Batch 50 (review 2026-08-30, F-L15-05 `maxclients`): past the bound
/// both engines answer `-ERR max number of clients reached` and close.
/// Redis: `maxclients 1` with one client held → the second is refused.
/// Node: `maxclients 4` on four cells is one slot per cell (ADR-0123) →
/// within eight attempts one connection lands on a full cell. Pre-fix
/// the node refused `CONFIG SET maxclients` as immutable.
#[test]
fn maxclients_refusal_matches_redis() {
    let Some((_node_guard, mut node)) = infinityd(4, scratch_base()) else {
        eprintln!("SKIPPED: INFINITYD_BIN unset — real-node compat lane not run (F-L19-09)");
        return;
    };
    let (_oracle_guard, mut oracle) = oracle();
    let (mut ob, mut nb) = (Vec::new(), Vec::new());
    assert_eq!(cmd(&mut oracle, &mut ob, &["CONFIG", "SET", "maxclients", "1"]), b"+OK\r\n");
    let mut o2 = sibling(&oracle);
    let mut b2 = Vec::new();
    let expected = cmd(&mut o2, &mut b2, &["PING"]);
    assert_eq!(expected, b"-ERR max number of clients reached\r\n");
    assert_closed_or_reset(&mut o2, "redis");
    assert_eq!(cmd(&mut oracle, &mut ob, &["CONFIG", "SET", "maxclients", "10000"]), b"+OK\r\n");

    assert_eq!(cmd(&mut node, &mut nb, &["CONFIG", "SET", "maxclients", "4"]), b"+OK\r\n");
    // No settle wait (batch 59): a peer applies the fan leg and its knobs
    // in one iteration (FABRIC-IN, then MAINTAIN) before it reaps another
    // accept, and the origin's knobs precede its `+OK` the same way. The
    // batch-57 "eight PONGs" was a port shared with another test's node.
    let mut held = Vec::new();
    let mut refused = None;
    for _ in 0..8 {
        let mut s = sibling(&node);
        let mut buf = Vec::new();
        let reply = cmd(&mut s, &mut buf, &["PING"]);
        if reply == b"+PONG\r\n" {
            held.push(s);
            continue;
        }
        assert_eq!(reply, expected, "node: {:?}", String::from_utf8_lossy(&reply));
        refused = Some(s);
        break;
    }
    let mut refused = refused.expect("node: no connection refused under maxclients 4 / 4 cells");
    assert_closed_or_reset(&mut refused, "node");
    assert!(held.len() <= 3, "node admitted {} beyond the holding client", held.len());
}

/// Batch 59 (lane L19 addendum, found behind the batch-57/58 flakes): a
/// second `infinityd` started on a port a running node owns must refuse
/// to start. Pre-fix it *joined* the first node's `SO_REUSEPORT` group —
/// the kernel then split new connections between two keyspaces with no
/// error anywhere (two `run_id`s behind one port). Client-reachable by
/// any operator who starts a node twice.
#[test]
fn a_second_node_on_an_owned_port_refuses_to_start() {
    let Some(bin) = candidate() else {
        eprintln!("SKIPPED: INFINITYD_BIN unset — real-node compat lane not run (F-L19-09)");
        return;
    };
    let Some((_first_guard, mut first)) = infinityd(4, scratch_base()) else {
        return;
    };
    let mut fb = Vec::new();
    assert_eq!(cmd(&mut first, &mut fb, &["PING"]), b"+PONG\r\n");
    let port = first.peer_addr().expect("peer addr").port();
    let mut second = spawn_infinityd_at(&bin, 4, port, scratch_base());
    // A refusal exits; a joiner serves — its log then says "listening".
    let status = second.wait_exit(std::time::Duration::from_secs(5)).unwrap_or_else(|| {
        panic!("a second node stayed up on the owned port {port}; its log:\n{}", second.log_text())
    });
    let log = second.log_text();
    assert_eq!(status.code(), Some(1), "{status}; log:\n{log}");
    assert!(log.contains(&format!("port {port} is already owned by another process")), "{log}");
    // The first node is untouched by the refused second.
    assert_eq!(cmd(&mut first, &mut fb, &["PING"]), b"+PONG\r\n");
}

/// Batch 61 (lane L19's batch-59 residual, seen as the batch-60 compat
/// flake): readiness must pair a test with the process it spawned. A
/// foreign node already answering on the port — a leftover measurement
/// node, an operator's — answers the readiness `PING` before the spawned
/// child even reaches its owned-port refusal, and pre-fix `infinityd_at`
/// handed the test that foreign node (the kept log was empty because the
/// child never served). Now `INFO server:process_id` must name the child.
#[test]
fn readiness_refuses_a_port_another_node_answers() {
    let Some(bin) = candidate() else {
        eprintln!("SKIPPED: INFINITYD_BIN unset — real-node compat lane not run (F-L19-09)");
        return;
    };
    let Some((foreign_guard, mut foreign)) = infinityd(1, scratch_base()) else {
        return;
    };
    let mut fb = Vec::new();
    assert_eq!(cmd(&mut foreign, &mut fb, &["PING"]), b"+PONG\r\n");
    let port = foreign.peer_addr().expect("peer addr").port();
    let foreign_pid = foreign_guard.pid();
    let paired = infinityd_at(&bin, 1, port, scratch_base());
    let err = match paired {
        Ok((_guard, _stream)) => {
            panic!("readiness paired the test with a node it did not spawn (pid {foreign_pid})")
        }
        Err(err) => err,
    };
    eprintln!("readiness refused: {err}");
    assert!(
        err.contains(&format!("pid {foreign_pid}")) || err.contains("exited before answering"),
        "the refusal names the foreign owner: {err}"
    );
    // The foreign node is untouched.
    assert_eq!(cmd(&mut foreign, &mut fb, &["PING"]), b"+PONG\r\n");
}

/// Batch 59: a test that fails while the node is up keeps the node's
/// scratch directory and prints the `infinityd.log` tail. Pre-fix the
/// guard removed the directory — and the log — on drop, so a node that
/// died mid-test left no evidence (batches 57/58 each lost one flake to
/// this). The falsifier panics while holding the guard, then reads the
/// log the way a reader of the failure would.
#[test]
fn a_failing_test_keeps_the_node_log() {
    let Some((guard, mut node)) = infinityd(1, scratch_base()) else {
        eprintln!("SKIPPED: INFINITYD_BIN unset — real-node compat lane not run (F-L19-09)");
        return;
    };
    let mut nb = Vec::new();
    assert_eq!(cmd(&mut node, &mut nb, &["PING"]), b"+PONG\r\n");
    // The scratch dir is named by the node's port — recoverable without
    // the guard, exactly as a reader of a real failure would find it.
    let port = node.peer_addr().expect("peer addr").port();
    let dir = std::fs::read_dir(scratch_base())
        .expect("scratch base")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| {
            p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                n.starts_with("inf-compat-node-") && n.ends_with(&format!("-{port}"))
            })
        })
        .expect("the node's scratch dir exists while it runs");
    let log = dir.join("infinityd.log");
    assert!(log.is_file(), "{} missing while the node runs", log.display());
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let _held = guard;
        panic!("expected: this harness falsifier fails on purpose while holding the node guard");
    }));
    assert!(outcome.is_err(), "the closure must have panicked");
    let text = std::fs::read(&log)
        .unwrap_or_else(|e| panic!("{} did not survive the failing test: {e}", log.display()));
    assert!(!text.is_empty(), "the kept log is empty");
    // What a real failure leaves for its reader, this test cleans up.
    if dir.is_dir() {
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }
}

/// Batch 50 (review 2026-08-30, F-L15-05 `timeout`): an idle client is
/// closed after `timeout` seconds on both engines (FIN, no bytes).
/// Pre-fix the node answered `+OK` and never closed.
#[test]
fn timeout_closes_an_idle_connection_like_redis() {
    let Some((_node_guard, mut node)) = infinityd(1, scratch_base()) else {
        eprintln!("SKIPPED: INFINITYD_BIN unset — real-node compat lane not run (F-L19-09)");
        return;
    };
    let (_oracle_guard, mut oracle) = oracle();
    let (mut ob, mut nb) = (Vec::new(), Vec::new());
    assert_eq!(cmd(&mut oracle, &mut ob, &["CONFIG", "SET", "timeout", "1"]), b"+OK\r\n");
    assert_eq!(cmd(&mut node, &mut nb, &["CONFIG", "SET", "timeout", "1"]), b"+OK\r\n");
    let mut idle_o = sibling(&oracle);
    let mut idle_n = sibling(&node);
    let (mut bo, mut bn) = (Vec::new(), Vec::new());
    assert_eq!(cmd(&mut idle_o, &mut bo, &["PING"]), b"+PONG\r\n");
    assert_eq!(cmd(&mut idle_n, &mut bn, &["PING"]), b"+PONG\r\n");
    #[allow(clippy::disallowed_methods)] // test harness thread, not cell code
    std::thread::sleep(std::time::Duration::from_millis(2500));
    // The admin connections idled through the deadline too: both reaped.
    for (who, stream) in [
        ("redis", &mut idle_o),
        ("node", &mut idle_n),
        ("redis admin", &mut oracle),
        ("node admin", &mut node),
    ] {
        let mut rest = Vec::new();
        match stream.read_to_end(&mut rest) {
            Ok(_) => assert!(rest.is_empty(), "{who}: bytes on an idle connection: {rest:?}"),
            Err(e) => panic!("{who}: the server never closed the idle connection ({e})"),
        }
    }
    // A pinned oracle (CI) outlives the test: restore its default.
    let mut fresh = sibling(&idle_o);
    let mut fb = Vec::new();
    assert_eq!(cmd(&mut fresh, &mut fb, &["CONFIG", "SET", "timeout", "0"]), b"+OK\r\n");
}

/// Batch 51 (review 2026-08-30, F-L15-02) at the binary: both engines
/// render a 40-hex `run_id` that does not move across `RANDOMKEY`, and
/// every cell of the spawned node answers the same one. Pre-fix the node
/// answered 32 digits from the RANDOMKEY cursor, per cell.
#[test]
fn run_id_is_40_hex_and_stable_like_redis() {
    let Some((_node_guard, mut node)) = infinityd(2, scratch_base()) else {
        eprintln!("SKIPPED: INFINITYD_BIN unset — real-node compat lane not run (F-L19-09)");
        return;
    };
    let (_oracle_guard, mut oracle) = oracle();
    fn field(reply: &[u8], name: &str) -> String {
        let text = String::from_utf8_lossy(reply);
        text.lines()
            .find_map(|l| l.strip_prefix(&format!("{name}:")))
            .unwrap_or_else(|| panic!("{name} missing: {text}"))
            .trim()
            .to_string()
    }
    let mut seen = std::collections::BTreeMap::new();
    for (who, stream) in [("oracle", &mut oracle), ("node", &mut node)] {
        let mut buf = Vec::new();
        let before = field(&cmd(stream, &mut buf, &["INFO", "server"]), "run_id");
        assert_eq!(before.len(), 40, "{who}: {before}");
        assert!(before.bytes().all(|b| b.is_ascii_hexdigit()), "{who}: {before}");
        cmd(stream, &mut buf, &["SET", "runid:k", "v"]);
        cmd(stream, &mut buf, &["RANDOMKEY"]);
        let after = field(&cmd(stream, &mut buf, &["INFO", "server"]), "run_id");
        assert_eq!(after, before, "{who}: run_id moved across RANDOMKEY");
        let replid = field(&cmd(stream, &mut buf, &["INFO", "replication"]), "master_replid");
        assert_eq!(replid.len(), 40, "{who}: {replid}");
        seen.insert(who, before);
    }
    // Every cell of the node: the same identity.
    for _ in 0..16 {
        let mut sibling = sibling(&node);
        let mut buf = Vec::new();
        let run_id = field(&cmd(&mut sibling, &mut buf, &["INFO", "server"]), "run_id");
        assert_eq!(&run_id, &seen["node"], "a cell answers a different run_id");
    }
}

/// Batch 51 (the id-0 kill gap) at the binary: a fresh connection's
/// `CLIENT ID` is ≥ 1 on both engines and `CLIENT KILL ID` from a sibling
/// reaches it (`:1`, then a close). Pre-fix the first connection on a
/// cell was id 0 and the kill was refused.
#[test]
fn client_id_is_positive_and_killable_like_redis() {
    let Some((_node_guard, node)) = infinityd(2, scratch_base()) else {
        eprintln!("SKIPPED: INFINITYD_BIN unset — real-node compat lane not run (F-L19-09)");
        return;
    };
    let (_oracle_guard, oracle) = oracle();
    for (who, mut first) in [("oracle", oracle), ("node", node)] {
        let mut buf = Vec::new();
        let reply = cmd(&mut first, &mut buf, &["CLIENT", "ID"]);
        let id: i64 =
            String::from_utf8_lossy(&reply).trim().trim_start_matches(':').parse().expect("int");
        assert!(id >= 1, "{who}: first client id {id}");
        // A second connection on the node may land on another cell: try
        // siblings until one reports the kill (ids are per cell — an
        // engine-internal counter, declared).
        let mut killed = false;
        for _ in 0..32 {
            let mut killer = sibling(&first);
            let mut kb = Vec::new();
            let r = cmd(&mut killer, &mut kb, &["CLIENT", "KILL", "ID", &id.to_string()]);
            assert!(r == b":1\r\n" || r == b":0\r\n", "{who}: {:?}", String::from_utf8_lossy(&r));
            if r == b":1\r\n" {
                killed = true;
                break;
            }
        }
        assert!(killed, "{who}: no sibling could kill client {id}");
        assert_closed_or_reset(&mut first, who);
    }
}

// ---- Batch 52 (review 2026-08-30): F-L13-04 + the L13 tiered-parse items ----

/// Batch 52 (review 2026-08-30, L13 style rows + F-L13-04): a connection
/// bound to a tiered namespace parses integers, `SET` options and `SCAN`
/// options exactly as Redis 8.0.5 does on its default database — the
/// tiered plane used `str::parse` (`+5`, `007` accepted), answered every
/// unknown `SET` option with the expiry refusal, and let a trailing lone
/// `COUNT` through. The two bound-error rows compare by class: Redis
/// names `proto-max-bulk-len`, the tiered namespace names its `BLOB-MAX`;
/// both refuse, neither creates the key, and the node stays up (pre-fix
/// the first one killed the cell thread).
#[test]
fn tiered_namespace_argument_errors_match_redis() {
    let Some((_node_guard, mut node)) = infinityd(1, scratch_base()) else {
        eprintln!("SKIPPED: INFINITYD_BIN unset — real-node compat lane not run (F-L19-09)");
        return;
    };
    let (_oracle_guard, mut oracle) = oracle();
    let (mut ob, mut nb) = (Vec::new(), Vec::new());
    for preamble in [
        &[
            "INF.NS",
            "CREATE",
            "hot",
            "MODE",
            "durable",
            "MEM-BUDGET",
            "64mb",
            "DISK-BUDGET",
            "256mb",
        ][..],
        &["INF.NS", "USE", "hot"][..],
    ] {
        assert_eq!(cmd(&mut node, &mut nb, preamble), b"+OK\r\n", "preamble {preamble:?}");
    }
    let mut failures = Vec::new();
    let exact: &[&[&str]] = &[
        &["SET", "n", "5"],
        &["SETRANGE", "n", "007", "x"],
        &["SETRANGE", "n", "+1", "x"],
        &["SETRANGE", "n", "abc", "x"],
        &["SETRANGE", "n", "-1", "x"],
        &["SETRANGE", "n", "9223372036854775807", ""],
        &["SETRANGE", "missing", "9223372036854775807", ""],
        &["EXISTS", "missing"],
        &["INCRBY", "n", "+5"],
        &["INCRBY", "n", "007"],
        &["DECRBY", "n", "-0"],
        &["GETRANGE", "n", "007", "1"],
        &["GETRANGE", "n", "0", "+1"],
        &["SET", "n", "v", "BOGUS"],
        &["SET", "n", "v", "NX", "XX"],
        &["SET", "n", "v", "NX", "NX"],
        &["SET", "g", "v", "GET", "GET"],
        &["SCAN", "0", "COUNT"],
        &["SCAN", "0", "COUNT", "007"],
        &["SCAN", "0", "COUNT", "0"],
        &["SCAN", "0", "COUNT", "10", "MATCH"],
        &["INCRBY", "n", "2"],
        &["GETRANGE", "n", "0", "-1"],
    ];
    for argv in exact {
        let o = cmd(&mut oracle, &mut ob, argv);
        let n = cmd(&mut node, &mut nb, argv);
        assert_pair(&o, &n, &argv.join(" "), &mut failures);
    }
    // Both refuse the post-image past their cap, neither creates the key.
    for offset in ["9223372036854775807", "4611686018427387904"] {
        let o = cmd(&mut oracle, &mut ob, &["SETRANGE", "missing", offset, "x"]);
        let n = cmd(&mut node, &mut nb, &["SETRANGE", "missing", offset, "x"]);
        if !(o.starts_with(b"-ERR ") && n.starts_with(b"-ERR ")) {
            failures.push(format!(
                "SETRANGE missing {offset} x: oracle {:?} node {:?}",
                String::from_utf8_lossy(&o),
                String::from_utf8_lossy(&n)
            ));
        }
        let o = cmd(&mut oracle, &mut ob, &["EXISTS", "missing"]);
        let n = cmd(&mut node, &mut nb, &["EXISTS", "missing"]);
        assert_pair(&o, &n, &format!("EXISTS missing after SETRANGE {offset}"), &mut failures);
    }
    assert!(
        failures.is_empty(),
        "tiered-namespace parse rows diverge from Redis:\n{}",
        failures.join("\n")
    );
}

/// Batch 58 (review 2026-08-30, F-L05-05): the deadline millisecond is
/// served. Redis's read path is `now > when` — at `now == when` `PTTL`
/// answers 0 and `GET` the value; the key is gone from the next
/// millisecond. Both servers get the same shape: N keys with staggered
/// `PXAT` deadlines, and a tight loop of pipelined `PTTL` + `GET` pairs
/// per key until `PTTL` goes negative. Loopback samples the deadline
/// millisecond tens of times per key, so "never a 0" is a semantic, not
/// a sampling, outcome — the pre-fix node stepped from `1` to `-2`.
#[test]
fn deadline_millisecond_read_matches_redis() {
    let Some((_node_guard, mut node)) = infinityd(1, scratch_base()) else {
        eprintln!("SKIPPED: INFINITYD_BIN unset — real-node compat lane not run (F-L19-09)");
        return;
    };
    let (_oracle_guard, mut oracle) = oracle();
    const KEYS: u64 = 8;
    #[allow(clippy::disallowed_methods)] // wall-clock deadlines on the test thread, not cell code
    fn sample(stream: &mut TcpStream, who: &str) -> (u64, BTreeSet<Vec<u8>>) {
        let mut buf = Vec::new();
        let unix_ms = || {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0)
        };
        let base = unix_ms() + 300;
        for i in 0..KEYS {
            let key = format!("dl:{i}");
            let at = (base + 60 * i).to_string();
            assert_eq!(cmd(stream, &mut buf, &["SET", &key, "v", "PXAT", &at]), b"+OK\r\n");
        }
        let mut zero_seen = 0u64;
        let mut gets_at_zero = BTreeSet::new();
        for i in 0..KEYS {
            let key = format!("dl:{i}");
            let deadline = base + 60 * i;
            while unix_ms() + 30 < deadline {
                #[allow(clippy::disallowed_methods)] // test harness thread, not cell code
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            let mut seen_zero_here = false;
            loop {
                let mut frame = encode_command(&["PTTL".to_string(), key.clone()]);
                frame.extend_from_slice(&encode_command(&["GET".to_string(), key.clone()]));
                stream.write_all(&frame).expect("write");
                let pttl_reply = read_frames(stream, &mut buf, 1);
                let get_reply = read_frames(stream, &mut buf, 1);
                let pttl = parse_int_reply(&pttl_reply)
                    .unwrap_or_else(|| panic!("{who}: PTTL reply {pttl_reply:?}"));
                if pttl == 0 && !seen_zero_here {
                    seen_zero_here = true;
                    gets_at_zero.insert(get_reply);
                }
                if pttl < 0 {
                    break;
                }
            }
            zero_seen += u64::from(seen_zero_here);
        }
        (zero_seen, gets_at_zero)
    }
    let (oracle_zero, oracle_gets) = sample(&mut oracle, "redis");
    assert!(oracle_zero > 0, "redis never sampled a deadline millisecond — box too loaded");
    assert_eq!(
        oracle_gets,
        BTreeSet::from([b"$1\r\nv\r\n".to_vec()]),
        "redis serves the value at PTTL 0"
    );
    let (node_zero, node_gets) = sample(&mut node, "node");
    assert!(
        node_zero > 0,
        "node: PTTL never answered 0 on {KEYS} deadline milliseconds (inclusive expiry)"
    );
    assert_eq!(node_gets, oracle_gets, "node serves the value at PTTL 0, as redis");
}

// ---- `timeout` and a client the server is still answering (ADR-0193) ----

/// The large value of the timeout shapes: 12 MiB, so a 256 KiB receive
/// buffer holds a small part of its reply.
const TIMEOUT_VALUE_BYTES: usize = 12 << 20;

/// Every byte of a `GET` of the large value: its header and payload.
fn large_reply_bytes() -> usize {
    format!("${TIMEOUT_VALUE_BYTES}\r\n").len() + TIMEOUT_VALUE_BYTES + 2
}

/// Sets `timeout 1` and stores the large value under `key`.
fn timeout_setup(server: &mut TcpStream, key: &str) {
    let mut buf = Vec::new();
    assert_eq!(cmd(server, &mut buf, &["CONFIG", "SET", "timeout", "1"]), b"+OK\r\n");
    let value = "v".repeat(TIMEOUT_VALUE_BYTES);
    assert_eq!(cmd(server, &mut buf, &["SET", key, &value]), b"+OK\r\n");
}

/// Restores a pinned oracle's `timeout` and drops the large value, from a
/// fresh connection (the setup's own was reaped).
fn timeout_teardown(server: &TcpStream, key: &str) {
    let mut fresh = sibling(server);
    let mut buf = Vec::new();
    assert_eq!(cmd(&mut fresh, &mut buf, &["CONFIG", "SET", "timeout", "0"]), b"+OK\r\n");
    cmd(&mut fresh, &mut buf, &["DEL", key]);
}

/// Sets the `SOL_SOCKET` option `option` of `socket` to `value`.
fn socket_option<T>(socket: &impl std::os::fd::AsFd, option: libc::c_int, value: &T) {
    use std::os::fd::AsRawFd;
    let len = libc::socklen_t::try_from(std::mem::size_of::<T>()).expect("a small option");
    // SAFETY: `socket` lends an open descriptor for the call's whole duration
    // (an `AsFd` borrow is of a live one), and `value` is a live, initialised
    // `T` of `len` bytes that setsockopt only reads; no pointer outlives it.
    let status = unsafe {
        libc::setsockopt(
            socket.as_fd().as_raw_fd(),
            libc::SOL_SOCKET,
            option,
            std::ptr::from_ref(value).cast(),
            len,
        )
    };
    assert_eq!(status, 0, "setsockopt {option}: {}", std::io::Error::last_os_error());
}

/// Sets the receive buffer of `socket` to `bytes`.
fn receive_buffer(socket: &impl std::os::fd::AsFd, bytes: libc::c_int) {
    socket_option(socket, libc::SO_RCVBUF, &bytes);
}

/// Closes `stream` with a reset: `SO_LINGER` 0, then the close.
fn reset(stream: TcpStream) {
    socket_option(&stream, libc::SO_LINGER, &libc::linger { l_onoff: 1, l_linger: 0 });
    drop(stream);
}

/// R6's shape: `GET` of the large value read at 1 MiB per 250 ms through a
/// 256 KiB receive buffer under `timeout 1`. The bytes read before the
/// server's close, and whether it closed within `timeout` + 2 s of the last.
fn slow_reader(server: &mut TcpStream, key: &str) -> (usize, bool) {
    use std::time::{Duration, Instant};
    timeout_setup(server, key);
    let mut reader = sibling(server);
    receive_buffer(&reader, 256 * 1024);
    reader.set_read_timeout(Some(Duration::from_secs(10))).expect("timeout");
    let wire = encode_command(&["GET".to_string(), key.to_string()]);
    reader.write_all(&wire).expect("write");
    let mut read = 0usize;
    let mut chunk = vec![0u8; 64 * 1024];
    let mut ended = false;
    while read < large_reply_bytes() && !ended {
        let mut period = 0usize;
        while period < (1 << 20) && read < large_reply_bytes() {
            match reader.read(&mut chunk) {
                Ok(0) => {
                    ended = true;
                    break;
                }
                Ok(n) => {
                    read += n;
                    period += n;
                }
                Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => {
                    ended = true;
                    break;
                }
                Err(e) => panic!("the slow reader's read failed after {read} bytes: {e}"),
            }
        }
        #[allow(clippy::disallowed_methods)] // test harness thread, not cell code
        std::thread::sleep(Duration::from_millis(250));
    }
    let last = Instant::now();
    let closed = ended || {
        let mut rest = Vec::new();
        reader.read_to_end(&mut rest).is_ok() && rest.is_empty()
    };
    let in_time = last.elapsed() <= Duration::from_secs(3);
    timeout_teardown(server, key);
    (read, closed && in_time)
}

/// A slow reader is answered whole on both engines, then closed: the node
/// counts a reply it sends as activity, so `timeout` judges the read's
/// gaps, never the request's age (ADR-0193 D3). Redis's largest gap between
/// two writes is 0.501 s here, the node's the reader's 0.25 s pace.
#[test]
#[ignore = "timeout judges a connection by its receive clock alone, so the node closes a slow \
            reader a timeout after its request with most of the reply unsent; red until a reply \
            sent is activity and a connection the server owes bytes is never judged"]
fn timeout_spares_a_slow_reader_like_redis() {
    let Some((_node_guard, mut node)) = infinityd(1, scratch_base()) else {
        eprintln!("SKIPPED: INFINITYD_BIN unset — real-node compat lane not run");
        return;
    };
    let (_oracle_guard, mut oracle) = oracle();
    let want = large_reply_bytes();
    let (oracle_read, oracle_closed) = slow_reader(&mut oracle, "timeout:slow");
    assert_eq!(oracle_read, want, "redis: the slow reader's bytes");
    assert!(oracle_closed, "redis: the slow reader was closed after its reply");
    let (node_read, node_closed) = slow_reader(&mut node, "timeout:slow");
    assert_eq!(node_read, want, "node: the slow reader's bytes, as redis");
    assert!(node_closed, "node: the slow reader was closed after its reply, as redis");
}

/// R7's and R8's shape: `GET` of the large value, with `QUIT` behind it when
/// `quit`, written once and never read. The seconds from the write until
/// `CLIENT LIST` no longer holds the client.
fn non_reader(server: &mut TcpStream, key: &str, quit: bool) -> f64 {
    use std::time::{Duration, Instant};
    timeout_setup(server, key);
    let mut client = sibling(server);
    let mut buf = Vec::new();
    let id = cmd(&mut client, &mut buf, &["CLIENT", "ID"]);
    let id = String::from_utf8_lossy(&id).trim_start_matches(':').trim_end().to_string();
    let mut wire = encode_command(&["GET".to_string(), key.to_string()]);
    if quit {
        wire.extend(encode_command(&["QUIT".to_string()]));
    }
    client.write_all(&wire).expect("write");
    let sent = Instant::now();
    let mut probe = sibling(server);
    let mut probe_buf = Vec::new();
    loop {
        let list = cmd(&mut probe, &mut probe_buf, &["CLIENT", "LIST"]);
        let listed =
            String::from_utf8_lossy(&list).lines().any(|l| l.contains(&format!("id={id} ")));
        if !listed {
            break;
        }
        assert!(sent.elapsed() < Duration::from_secs(10), "the non-reader was never closed");
        #[allow(clippy::disallowed_methods)] // test harness thread, not cell code
        std::thread::sleep(Duration::from_millis(50));
    }
    let gone = sent.elapsed().as_secs_f64();
    drop(client);
    timeout_teardown(server, key);
    gone
}

/// A client that never reads its large reply is reaped within `timeout` + 2 s
/// of its request on both engines (Redis: R7), at HEAD and after the change:
/// a reply the client never takes is no activity.
#[test]
fn timeout_reaps_a_non_reader_like_redis() {
    let Some((_node_guard, mut node)) = infinityd(1, scratch_base()) else {
        eprintln!("SKIPPED: INFINITYD_BIN unset — real-node compat lane not run");
        return;
    };
    let (_oracle_guard, mut oracle) = oracle();
    let redis = non_reader(&mut oracle, "timeout:non-reader", false);
    assert!(redis <= 3.0, "redis reaped the non-reader {redis:.2} s after its request");
    let ours = non_reader(&mut node, "timeout:non-reader", false);
    assert!(ours <= 3.0, "node reaped the non-reader {ours:.2} s after its request, past 3 s");
}

/// The same with `QUIT` behind the `GET`, the one way `timeout` reaches a
/// slot flushing its replies before a close: closed within `timeout` + 2 s
/// on both engines (Redis: R8), the node's `idle_disconnections` + 1.
#[test]
fn timeout_reaps_a_flushing_non_reader_like_redis() {
    let Some((_node_guard, mut node)) = infinityd(1, scratch_base()) else {
        eprintln!("SKIPPED: INFINITYD_BIN unset — real-node compat lane not run");
        return;
    };
    let (_oracle_guard, mut oracle) = oracle();
    let redis = non_reader(&mut oracle, "timeout:flushing", true);
    assert!(redis <= 3.0, "redis closed the flushing non-reader {redis:.2} s after its request");
    let idle = |server: &TcpStream| -> u64 {
        let mut probe = sibling(server);
        let mut buf = Vec::new();
        let stats = cmd(&mut probe, &mut buf, &["INFO", "stats"]);
        String::from_utf8_lossy(&stats)
            .lines()
            .find_map(|line| line.strip_prefix("idle_disconnections:"))
            .and_then(|value| value.trim().parse().ok())
            .expect("INFO stats names idle_disconnections")
    };
    let before = idle(&node);
    let ours = non_reader(&mut node, "timeout:flushing", true);
    assert!(ours <= 3.0, "node closed the flushing non-reader {ours:.2} s after its request");
    assert!(idle(&node) > before, "node: the close was not the timeout's");
}

// ---- A close runs what its client sent (ADR-0123 A2; ADR-0196 and its A1) ----

/// A read's hang bound: a stream a server still holds open past it was never
/// closed. It ends the wait; it decides nothing a server sent.
const CLOSE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(20);

/// How a read to the end of a stream ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum End {
    /// The server's FIN.
    Fin,
    /// The server's reset.
    Reset,
    /// Still open at [`CLOSE_DEADLINE`].
    Open,
}

/// Every byte `stream` reads until the server closes it, and how it closed.
fn read_until_closed(stream: &mut TcpStream) -> (Vec<u8>, End) {
    use std::io::ErrorKind;
    use std::time::{Duration, Instant};
    stream.set_read_timeout(Some(Duration::from_millis(500))).expect("read timeout");
    let begin = Instant::now();
    let mut delivered = Vec::new();
    let mut chunk = vec![0u8; 1 << 16];
    while begin.elapsed() < CLOSE_DEADLINE {
        match stream.read(&mut chunk) {
            Ok(0) => return (delivered, End::Fin),
            Ok(n) => delivered.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == ErrorKind::ConnectionReset => return (delivered, End::Reset),
            Err(e)
                if matches!(
                    e.kind(),
                    ErrorKind::Interrupted | ErrorKind::WouldBlock | ErrorKind::TimedOut
                ) => {}
            Err(e) => panic!("a read failed after {} bytes: {e}", delivered.len()),
        }
    }
    (delivered, End::Open)
}

/// One command's wire bytes.
fn wire(argv: &[&str]) -> Vec<u8> {
    let owned: Vec<String> = argv.iter().map(|part| (*part).to_string()).collect();
    encode_command(&owned)
}

/// The names `prefix:0` to `prefix:{count - 1}`.
fn names(prefix: &str, count: usize) -> Vec<String> {
    (0..count).map(|n| format!("{prefix}:{n}")).collect()
}

/// A `SET` of every name, pipelined.
fn sets(names: &[String]) -> Vec<u8> {
    names.iter().flat_map(|name| wire(&["SET", name, "1"])).collect()
}

/// `n` `+OK` replies.
fn oks(n: usize) -> Vec<u8> {
    b"+OK\r\n".repeat(n)
}

/// How many of `names` exist, counted by `EXISTS` on a fresh connection,
/// bound by `INF.NS USE` to `ns` first when given.
fn existing(server: &TcpStream, ns: Option<&str>, names: &[String]) -> usize {
    let mut reader = sibling(server);
    let mut buf = Vec::new();
    if let Some(ns) = ns {
        assert_eq!(cmd(&mut reader, &mut buf, &["INF.NS", "USE", ns]), b"+OK\r\n", "USE {ns}");
    }
    // A named namespace refuses a multi-key command whose keys span cells, so
    // a bound reader asks one key at a time.
    let batch_len = if ns.is_some() { 1 } else { 500 };
    let mut count = 0;
    for batch in names.chunks(batch_len) {
        let mut argv = vec!["EXISTS"];
        argv.extend(batch.iter().map(String::as_str));
        let reply = cmd(&mut reader, &mut buf, &argv);
        let n = parse_int_reply(&reply).and_then(|n| usize::try_from(n).ok());
        count += n.unwrap_or_else(|| panic!("EXISTS: {:?}", String::from_utf8_lossy(&reply)));
    }
    count
}

/// `stream`'s client id, as `CLIENT LIST` prints it.
fn client_id(stream: &mut TcpStream) -> String {
    let reply = cmd(stream, &mut Vec::new(), &["CLIENT", "ID"]);
    let id = parse_int_reply(&reply).expect("CLIENT ID is an integer");
    id.to_string()
}

/// Client `id`'s `CLIENT LIST` row, read on a fresh connection. A node's
/// cell lists its own clients, so the legs that read a row run one cell.
fn client_row(server: &TcpStream, id: &str) -> Option<String> {
    let mut probe = sibling(server);
    let list = cmd(&mut probe, &mut Vec::new(), &["CLIENT", "LIST"]);
    let marker = format!("id={id} ");
    String::from_utf8_lossy(&list).lines().find(|line| line.contains(&marker)).map(str::to_string)
}

/// Whether `CLIENT LIST` holds client `id`'s row.
fn listed(server: &TcpStream, id: &str) -> bool {
    client_row(server, id).is_some()
}

/// A fresh connection to `server`'s address whose receive buffer is `bytes`
/// from before its connect, as a client that holds a small buffer from its
/// start: one shrunk after the connect leaves less free space than a segment,
/// so a loopback sender stalls in zero-window probes. Elsewhere than Linux the
/// descriptor is inheritable; the compatibility lanes run on Linux.
fn sibling_with_receive_buffer(server: &TcpStream, bytes: libc::c_int) -> TcpStream {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    let std::net::SocketAddr::V4(peer) = server.peer_addr().expect("peer addr") else {
        panic!("both servers listen on IPv4 loopback");
    };
    #[cfg(target_os = "linux")]
    let kind = libc::SOCK_STREAM | libc::SOCK_CLOEXEC;
    #[cfg(not(target_os = "linux"))]
    let kind = libc::SOCK_STREAM;
    // SAFETY: socket(2) reads no memory of this process; its result is checked
    // before use.
    let fd = unsafe { libc::socket(libc::AF_INET, kind, 0) };
    assert!(fd >= 0, "socket: {}", std::io::Error::last_os_error());
    // SAFETY: `fd` is the open socket socket(2) just returned and nothing else
    // holds it, so the `OwnedFd` is its one owner and closes it once.
    let socket = unsafe { OwnedFd::from_raw_fd(fd) };
    receive_buffer(&socket, bytes);
    // SAFETY: every field of `sockaddr_in` is an integer or an array of them,
    // so all zero bytes are a valid value; the fields read are set below.
    let mut address: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    address.sin_family = libc::sa_family_t::try_from(libc::AF_INET).expect("AF_INET fits");
    address.sin_port = peer.port().to_be();
    address.sin_addr.s_addr = u32::from(*peer.ip()).to_be();
    let len = libc::socklen_t::try_from(std::mem::size_of_val(&address)).expect("a small struct");
    // SAFETY: `address` is a live, initialised `sockaddr_in` of `len` bytes that
    // connect(2) only reads during the call, and `socket` is open throughout.
    let status =
        unsafe { libc::connect(socket.as_raw_fd(), std::ptr::from_ref(&address).cast(), len) };
    assert_eq!(status, 0, "connect: {}", std::io::Error::last_os_error());
    let stream = TcpStream::from(socket);
    stream.set_read_timeout(Some(std::time::Duration::from_secs(5))).expect("timeout");
    stream
}

/// Polls `condition` every 20 ms until it holds; false when it still fails at
/// [`CLOSE_DEADLINE`], a hang bound: the server's step that makes it true
/// takes a few iterations. A leg orders its next step after the server's by
/// such a condition, never by a fixed sleep.
fn eventually(mut condition: impl FnMut() -> bool) -> bool {
    use std::time::{Duration, Instant};
    let begin = Instant::now();
    while !condition() {
        if begin.elapsed() >= CLOSE_DEADLINE {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    true
}

/// Whether client `id`'s row leaves `CLIENT LIST` within the hang bound.
fn unlisted_in_time(server: &TcpStream, id: &str) -> bool {
    eventually(|| !listed(server, id))
}

/// Sends `signal` (`-STOP`, `-CONT`) to process `pid` with `kill(1)`.
fn signal(pid: u32, signal: &str) {
    let status = std::process::Command::new("kill").args([signal, &pid.to_string()]).status();
    assert!(status.is_ok_and(|status| status.success()), "kill {signal} {pid}");
}

/// How a leg keeps a server from reading while its client writes, so the
/// client's bytes and its FIN wait in the kernel and reach the server
/// together once it reads again.
#[derive(Clone, Copy, Debug)]
enum HoldBy {
    /// The server reads as it pleases.
    Nothing,
    /// The process is stopped (`SIGSTOP`) until the FIN is written.
    Signal(u32),
    /// The client writes 0.2 s into a second connection's `DEBUG SLEEP 1`:
    /// Redis reads nothing while it sleeps, and a node's sleeping cell
    /// parses nothing (so a node holds the leg on one cell).
    Sleep,
}

/// A hold in force, released once the client's FIN is written.
enum Hold {
    Stopped(u32),
    Asleep(TcpStream),
}

impl Hold {
    fn begin(server: &TcpStream, by: HoldBy) -> Option<Hold> {
        match by {
            HoldBy::Nothing => None,
            HoldBy::Signal(pid) => {
                signal(pid, "-STOP");
                Some(Hold::Stopped(pid))
            }
            HoldBy::Sleep => {
                let mut sleeper = sibling(server);
                sleeper.write_all(&wire(&["DEBUG", "SLEEP", "1"])).expect("DEBUG SLEEP");
                std::thread::sleep(std::time::Duration::from_millis(200));
                Some(Hold::Asleep(sleeper))
            }
        }
    }

    fn release(self) {
        match self {
            Hold::Stopped(pid) => signal(pid, "-CONT"),
            Hold::Asleep(mut sleeper) => {
                let reply = read_frames(&mut sleeper, &mut Vec::new(), 1);
                assert_eq!(reply, b"+OK\r\n", "DEBUG SLEEP's +OK");
            }
        }
    }
}

/// How the oracle is held for the stopped shape: by its signal when this
/// test spawned it, else by `DEBUG SLEEP`, under which Redis reads nothing
/// either (a pinned oracle's process is not the test's), said on stdout.
fn oracle_hold(guard: &OracleGuard) -> HoldBy {
    match guard.pid() {
        Some(pid) => HoldBy::Signal(pid),
        None => {
            println!("the pinned oracle is held by DEBUG SLEEP, not stopped by a signal");
            HoldBy::Sleep
        }
    }
}

/// Writes `bytes`, then the client's FIN when `fin`, each bounded by
/// [`CLOSE_DEADLINE`]: a stopped server takes nothing, and the bound ends a
/// write its buffers cannot hold. A server that closes as it reads, at a
/// `QUIT` or a protocol error with input behind it unread, resets the
/// connection (ADR-0196 D6.2), and the reset can reach the client before its
/// last write or its shutdown: a reset, a broken pipe or no connection there
/// is the server's close, returned as `Some(End::Reset)`, and the caller
/// reads on.
fn write_then_fin(stream: &mut TcpStream, bytes: &[u8], fin: bool) -> Option<End> {
    use std::io::ErrorKind;
    let closed = |error: &std::io::Error| {
        matches!(
            error.kind(),
            ErrorKind::ConnectionReset | ErrorKind::BrokenPipe | ErrorKind::NotConnected
        )
    };
    stream.set_write_timeout(Some(CLOSE_DEADLINE)).expect("write timeout");
    if let Err(error) = stream.write_all(bytes) {
        assert!(closed(&error), "a write failed, bound {CLOSE_DEADLINE:?}: {error}");
        return Some(End::Reset);
    }
    if fin && let Err(error) = stream.shutdown(Shutdown::Write) {
        assert!(closed(&error), "the FIN failed: {error}");
        return Some(End::Reset);
    }
    None
}

/// Every byte until the server closes `stream`, ended as a write that met
/// the server's close says (`early`), else as the read says.
fn read_on(stream: &mut TcpStream, early: Option<End>) -> (Vec<u8>, End) {
    let (bytes, end) = read_until_closed(stream);
    (bytes, early.unwrap_or(end))
}

/// What one half-closed pipeline read: its replies, how its stream ended,
/// and how many of its names a fresh client finds set.
struct HalfClosed {
    replies: Vec<u8>,
    end: End,
    landed: usize,
}

/// `count` pipelined `SET`s of `prefix`'s names, the client's FIN, then a
/// read to the end, with the server held by `by` until the FIN is written.
fn half_closed_pipeline(server: &TcpStream, by: HoldBy, prefix: &str, count: usize) -> HalfClosed {
    let names = names(prefix, count);
    let mut client = sibling(server);
    let hold = Hold::begin(server, by);
    let early = write_then_fin(&mut client, &sets(&names), true);
    if let Some(hold) = hold {
        hold.release();
    }
    let (replies, end) = read_on(&mut client, early);
    HalfClosed { replies, end, landed: existing(server, None, &names) }
}

/// Redis's reading of a half-closed pipeline of `count` `SET`s, every
/// shape (ADR-0196's HC1–HC3): `count` `+OK`, then the FIN, every name set.
/// Each server is held to it, so the node reads what Redis reads.
fn check_half_closed(label: &str, count: usize, reading: &HalfClosed, failures: &mut Vec<String>) {
    let want = oks(count);
    if reading.replies != want || reading.end != End::Fin || reading.landed != count {
        failures.push(format!(
            "{label}: {} reply bytes of {} for {count} SETs, then {:?} (owed Fin); {} of {count} \
             names set",
            reading.replies.len(),
            want.len(),
            reading.end,
            reading.landed,
        ));
    }
}

/// A client that pipelines `SET`s and then ends its stream is answered for
/// every one, then the server closes the stream, on both servers: the end of
/// the stream marks the connection, and the commands read before it run and
/// are answered (ADR-0196 A1). 100 and 5,000 `SET`s of keys both cells own,
/// the server running, and the server stopped until the FIN is written, so
/// the bytes and the FIN reach it at one read.
#[test]
fn a_half_closed_pipeline_is_answered_like_redis() {
    let Some((node_guard, node)) = infinityd(2, scratch_base()) else {
        eprintln!("SKIPPED: INFINITYD_BIN unset — real-node compat lane not run");
        return;
    };
    let (oracle_guard, oracle) = oracle();
    let held = [(oracle_hold(&oracle_guard), HoldBy::Signal(node_guard.pid()))];
    let shapes = [(HoldBy::Nothing, HoldBy::Nothing)].into_iter().chain(held);
    let mut failures = Vec::new();
    for (redis_by, node_by) in shapes {
        for count in [100, 5_000] {
            let shape = if matches!(node_by, HoldBy::Nothing) { "running" } else { "stopped" };
            let prefix = format!("half-close-{shape}-{count}");
            let redis = half_closed_pipeline(&oracle, redis_by, &prefix, count);
            check_half_closed(&format!("redis {prefix}"), count, &redis, &mut failures);
            let ours = half_closed_pipeline(&node, node_by, &prefix, count);
            check_half_closed(&format!("node {prefix}"), count, &ours, &mut failures);
        }
    }
    assert!(failures.is_empty(), "a half-closed pipeline:\n{}", failures.join("\n"));
}

/// The same pipelines sent 0.2 s into another client's `DEBUG SLEEP 1`, the
/// FIN written before the sleep ends: each server reads the bytes and the FIN
/// only once it wakes, then runs and answers every `SET`. One cell, so the
/// node's sleeping cell is the client's.
#[test]
fn a_half_closed_pipeline_sent_during_a_stall_is_answered_like_redis() {
    let Some((_node_guard, node)) = infinityd(1, scratch_base()) else {
        eprintln!("SKIPPED: INFINITYD_BIN unset — real-node compat lane not run");
        return;
    };
    let (_oracle_guard, oracle) = oracle();
    let mut failures = Vec::new();
    for count in [100, 5_000] {
        let prefix = format!("half-close-stall-{count}");
        let redis = half_closed_pipeline(&oracle, HoldBy::Sleep, &prefix, count);
        check_half_closed(&format!("redis {prefix}"), count, &redis, &mut failures);
        let ours = half_closed_pipeline(&node, HoldBy::Sleep, &prefix, count);
        check_half_closed(&format!("node {prefix}"), count, &ours, &mut failures);
    }
    assert!(failures.is_empty(), "a pipeline half-closed in a stall:\n{}", failures.join("\n"));
}

/// `before`'s `SET`s, `QUIT`, `behind`'s `SET`s and the FIN in one write,
/// with the server held by `by` until the FIN is written; then a read to the
/// end and a count of the names set before and behind the `QUIT`.
fn quit_in_half_closed_pipeline(
    server: &TcpStream,
    by: HoldBy,
    before: &[String],
    behind: &[String],
) -> QuitReading {
    let mut bytes = sets(before);
    bytes.extend(wire(&["QUIT"]));
    bytes.extend(sets(behind));
    let mut client = sibling(server);
    let hold = Hold::begin(server, by);
    let early = write_then_fin(&mut client, &bytes, true);
    if let Some(hold) = hold {
        hold.release();
    }
    let (replies, end) = read_on(&mut client, early);
    let landed = existing(server, None, before);
    QuitReading { replies, end, landed, landed_behind: existing(server, None, behind) }
}

/// 1,500 `SET`s, `QUIT`, 2,000 `SET`s and the FIN in one write, in the
/// half-closed pipeline's three shapes (running, stopped until the FIN is
/// written, written during a `DEBUG SLEEP` on one cell): both servers answer
/// the 1,500 and the `QUIT`, run no `SET` behind it, and close. How the
/// stream ends is declared, not compared (ADR-0196 D6.2): Redis resets it
/// when it closes with the input behind the `QUIT` unread, and this server
/// may end it with a FIN; both are printed.
#[test]
fn a_quit_inside_a_half_closed_pipeline_ends_it_like_redis() {
    let Some((two_guard, two_cells)) = infinityd(2, scratch_base()) else {
        eprintln!("SKIPPED: INFINITYD_BIN unset — real-node compat lane not run");
        return;
    };
    let (_one_guard, one_cell) = infinityd(1, scratch_base()).expect("the node lane is on");
    let (oracle_guard, oracle) = oracle();
    let shapes = [
        ("running", HoldBy::Nothing, &two_cells, HoldBy::Nothing),
        ("stopped", oracle_hold(&oracle_guard), &two_cells, HoldBy::Signal(two_guard.pid())),
        ("stall", HoldBy::Sleep, &one_cell, HoldBy::Sleep),
    ];
    let mut failures = Vec::new();
    for (shape, redis_by, node, node_by) in shapes {
        let before = names(&format!("half-close-quit-{shape}-before"), 1_500);
        let behind = names(&format!("half-close-quit-{shape}-behind"), 2_000);
        for (who, server, by) in [("redis", &oracle, redis_by), ("node", node, node_by)] {
            let reading = quit_in_half_closed_pipeline(server, by, &before, &behind);
            let (end, landed, landed_behind) = (reading.end, reading.landed, reading.landed_behind);
            println!("{who}, {shape}: a QUIT inside a half-closed pipeline, ended by {end:?}");
            if reading.replies != oks(1_501) || end == End::Open || landed != 1_500 {
                failures.push(format!(
                    "{who}, {shape}: {} reply bytes of {} (1,500 SETs and the QUIT), then \
                     {end:?}; {landed} of 1,500 names before the QUIT set",
                    reading.replies.len(),
                    oks(1_501).len(),
                ));
            }
            if landed_behind != 0 {
                failures.push(format!("{who}, {shape}: {landed_behind} SETs behind the QUIT ran"));
            }
        }
    }
    assert!(failures.is_empty(), "a QUIT inside a half-closed pipeline:\n{}", failures.join("\n"));
}

/// One `GET big` reply: the bulk header and the 1 MiB payload.
const BIG_REPLY_BYTES: usize = "$1048576\r\n".len() + (1 << 20) + 2;

/// The messages of the unread subscriber's fill, each of [`FILL_BYTES`].
const FILL_MESSAGES: usize = 6_000;

/// One fill message's payload: 6,000 of them stay under both servers'
/// `pubsub` soft output limit of 8 MiB.
const FILL_BYTES: usize = 1024;

/// One fill message as a subscriber of `ch` reads it.
fn fill_message() -> Vec<u8> {
    let mut frame = format!("*3\r\n$7\r\nmessage\r\n$2\r\nch\r\n${FILL_BYTES}\r\n").into_bytes();
    frame.extend(std::iter::repeat_n(b'f', FILL_BYTES));
    frame.extend_from_slice(b"\r\n");
    frame
}

/// A client's `SUBSCRIBE ch` confirmation.
const SUBSCRIBED: &[u8] = b"*3\r\n$9\r\nsubscribe\r\n$2\r\nch\r\n:1\r\n";

/// The least output Redis holds for a slow-reader leg's client before its
/// `QUIT` or FIN: 1 MiB of the 6 to 8 MB the leg owes. Redis's own socket
/// takes about 1 to 2 MB for a client that reads nothing, so the rest waits in
/// Redis, where its end-of-stream rule acts on it.
const HELD_FLOOR_BYTES: u64 = 1 << 20;

/// Which server a leg reads: Redis, whose `CLIENT LIST` row shows the output
/// it holds (`omem`) and its close mark (`c` in `flags`), or the node, whose
/// buffer gauges are untracked zeros and whose rows carry no flags.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Side {
    Redis,
    Node,
}

/// A `CLIENT LIST` row's `name=value` field.
fn row_field<'row>(row: &'row str, name: &str) -> Option<&'row str> {
    row.split(' ').find_map(|field| field.strip_prefix(name)?.strip_prefix('='))
}

/// The leg's engagement on Redis: client `id`'s row names the client's own
/// socket, `own`, as its peer (`addr`), so no reader sits between Redis and
/// the client to take Redis's output (a published container port's proxy
/// does), and holds at least [`HELD_FLOOR_BYTES`] of output, and once
/// `marked` the close mark `c`, polled to the hang bound (the poll also
/// orders the next write after Redis's read of the last); the last row read
/// otherwise.
fn redis_holds(server: &TcpStream, id: &str, own: SocketAddr, marked: bool) -> Result<(), String> {
    let own = own.to_string();
    let mut last = None;
    let mut foreign = false;
    let held = eventually(|| {
        last = client_row(server, id);
        let Some(row) = last.as_deref() else { return false };
        if row_field(row, "addr") != Some(own.as_str()) {
            foreign = true;
            return true;
        }
        let omem = row_field(row, "omem").and_then(|value| value.parse::<u64>().ok());
        let flags = row_field(row, "flags").unwrap_or("");
        omem.is_some_and(|bytes| bytes >= HELD_FLOOR_BYTES) && (!marked || flags.contains('c'))
    });
    if foreign {
        return Err(format!(
            "redis's peer for client {id} is not the client's socket {own}, so a reader sits \
             between them: its row {last:?}"
        ));
    }
    if held {
        return Ok(());
    }
    let mark = if marked { " and the mark c" } else { "" };
    Err(format!(
        "redis held under {HELD_FLOOR_BYTES} B of output for client {id}{mark}: its row {last:?}"
    ))
}

/// Whether the server lists client `id` once it has read the client's FIN.
/// Redis frees a client at the end of stream it reads, so its row is polled
/// until it goes and `false` is read as soon as it holds (the poll orders the
/// next step after Redis's read of the FIN); the node keeps the row while it
/// owes the client replies (ADR-0196 D6.5), read once.
fn listed_after_fin(server: &TcpStream, side: Side, id: &str) -> bool {
    match side {
        Side::Redis => !unlisted_in_time(server, id),
        Side::Node => listed(server, id),
    }
}

/// What a client the server still owed read once it ended its stream or
/// sent `QUIT`.
struct Owed {
    /// What it read until the server closed, and how it closed.
    bytes: Vec<u8>,
    end: End,
    /// The leg's engagement: on Redis, [`redis_holds`] before the client's
    /// `QUIT` or FIN; on the node, `Ok`.
    engaged: Result<(), String>,
    /// Whether `CLIENT LIST` held its row after its last write.
    listed_while_owed: bool,
    /// Whether its row left `CLIENT LIST` within the hang bound after its
    /// stream ended.
    unlisted_after: bool,
    /// For a subscriber, a `PUBLISH` to its channel after its last write.
    count: Option<Vec<u8>>,
}

/// Eight `GET`s of a 1 MiB value from a client whose receive buffer is
/// 64 KiB, then a `QUIT` when `quit`, in one write; once Redis holds the
/// replies ([`redis_holds`]; the node is not polled), the client's FIN; then
/// it reads to the end.
fn slow_half_closed_reader(server: &TcpStream, side: Side, quit: bool) -> Owed {
    let mut setup = sibling(server);
    let value = "b".repeat(1 << 20);
    assert_eq!(cmd(&mut setup, &mut Vec::new(), &["SET", "big", &value]), b"+OK\r\n");
    let mut client = sibling_with_receive_buffer(server, 64 * 1024);
    let id = client_id(&mut client);
    let mut bytes: Vec<u8> = (0..8).flat_map(|_| wire(&["GET", "big"])).collect();
    if quit {
        bytes.extend(wire(&["QUIT"]));
    }
    let mut early = write_then_fin(&mut client, &bytes, false);
    let own = client.local_addr().expect("local addr");
    let engaged = match side {
        Side::Redis => redis_holds(server, &id, own, quit),
        Side::Node => Ok(()),
    };
    if early.is_none() {
        early = write_then_fin(&mut client, &[], true);
    }
    let listed_while_owed = listed_after_fin(server, side, &id);
    let (bytes, end) = read_on(&mut client, early);
    let unlisted_after = unlisted_in_time(server, &id);
    Owed { bytes, end, engaged, listed_while_owed, unlisted_after, count: None }
}

/// A subscriber of `ch` with a 4 KiB receive buffer reads nothing while
/// 6,000 messages of 1 KiB are published to it, so the server holds most of
/// them ([`redis_holds`] on Redis); it sends `QUIT` when `quit` and ends its
/// stream when `fin`; then a third client publishes `m` once, and the
/// subscriber reads to the end.
fn unread_subscriber(server: &TcpStream, side: Side, quit: bool, fin: bool) -> Owed {
    let mut subscriber = sibling_with_receive_buffer(server, 4096);
    let id = client_id(&mut subscriber);
    let mut sub_buf = Vec::new();
    let confirmation = cmd(&mut subscriber, &mut sub_buf, &["SUBSCRIBE", "ch"]);
    assert_eq!(confirmation, SUBSCRIBED, "the SUBSCRIBE confirmation");
    let mut publisher = sibling(server);
    let mut pub_buf = Vec::new();
    let fill = "f".repeat(FILL_BYTES);
    let burst: Vec<u8> = (0..100).flat_map(|_| wire(&["PUBLISH", "ch", &fill])).collect();
    for _ in 0..FILL_MESSAGES / 100 {
        publisher.write_all(&burst).expect("publish");
        let counts = read_frames(&mut publisher, &mut pub_buf, 100);
        assert_eq!(counts, b":1\r\n".repeat(100), "the fill's receiver counts");
    }
    let own = subscriber.local_addr().expect("local addr");
    let mut engaged = match side {
        Side::Redis => redis_holds(server, &id, own, false),
        Side::Node => Ok(()),
    };
    let mut early = None;
    if quit {
        early = write_then_fin(&mut subscriber, &wire(&["QUIT"]), false);
        if side == Side::Redis && engaged.is_ok() {
            engaged = redis_holds(server, &id, own, true);
        }
    }
    if fin && early.is_none() {
        early = write_then_fin(&mut subscriber, &[], true);
    }
    let listed_while_owed =
        if fin { listed_after_fin(server, side, &id) } else { listed(server, &id) };
    let count = cmd(&mut publisher, &mut pub_buf, &["PUBLISH", "ch", "m"]);
    let (bytes, end) = read_on(&mut subscriber, early);
    let unlisted_after = unlisted_in_time(server, &id);
    sub_buf.extend(bytes);
    Owed { bytes: sub_buf, end, engaged, listed_while_owed, unlisted_after, count: Some(count) }
}

/// Holds a reading of a client the server still owed once it ended its
/// stream to ADR-0196 A1.6's declared shape: this server sends `full`, every
/// byte it owed (D6.4), and lists the client while it owes it (D6.5); Redis,
/// engaged ([`redis_holds`]), drops at the end of stream what its socket had
/// not taken, so it sends a proper prefix of `full`, and drops the row. Both
/// close by a FIN and both unlist the client once its stream ended. Redis's
/// byte count is printed.
fn check_answered_in_full(label: &str, full: &[u8], redis: &Owed, ours: &Owed) -> Vec<String> {
    println!("{label}: redis sent {} bytes of the {} owed", redis.bytes.len(), full.len());
    let mut failures = Vec::new();
    if let Err(engagement) = &redis.engaged {
        failures.push(format!("{label}: VACUOUS engagement: {engagement}"));
    }
    if ours.bytes != full || ours.end != End::Fin {
        failures.push(format!(
            "{label}: node sent {} bytes of {} (owed all), then {:?}",
            ours.bytes.len(),
            full.len(),
            ours.end
        ));
    }
    let prefix = full.starts_with(&redis.bytes) && redis.bytes.len() < full.len();
    if !prefix || redis.end != End::Fin {
        failures.push(format!(
            "{label}: redis sent {} bytes, not a proper prefix of the {} owed (D6.4 unobserved), \
             then {:?}",
            redis.bytes.len(),
            full.len(),
            redis.end
        ));
    }
    let rows = [ours.listed_while_owed, !redis.listed_while_owed];
    if rows != [true, true] || !ours.unlisted_after || !redis.unlisted_after {
        failures.push(format!(
            "{label}: listed while owed: node {} (owed true), redis {} (owed false); unlisted \
             after the end: node {}, redis {} (owed true)",
            ours.listed_while_owed,
            redis.listed_while_owed,
            ours.unlisted_after,
            redis.unlisted_after
        ));
    }
    failures
}

/// A client the server still owes eight 1 MiB replies ends its stream, having
/// read nothing through a 64 KiB receive buffer while Redis held at least
/// 1 MiB for it: this server sends all 8,388,704 bytes, then closes, and lists
/// the client until then; Redis drops what its socket had not taken, and the
/// row, when it reads the FIN. With a `QUIT` before the FIN this server sends
/// the `QUIT`'s `+OK` after the eight replies, and Redis drops it with them.
/// Declared (ADR-0196 A1.6, D6.4 and D6.5); Redis's bytes are a proper prefix
/// of this server's.
#[test]
fn a_half_closed_slow_reader_is_answered_in_full() {
    let Some((_node_guard, node)) = infinityd(1, scratch_base()) else {
        eprintln!("SKIPPED: INFINITYD_BIN unset — real-node compat lane not run");
        return;
    };
    let (_oracle_guard, oracle) = oracle();
    let mut failures = Vec::new();
    for quit in [false, true] {
        let mut full = slow_reply().repeat(8);
        if quit {
            full.extend_from_slice(b"+OK\r\n");
        }
        let label = if quit { "eight GETs, QUIT, FIN" } else { "eight GETs, FIN" };
        let redis = slow_half_closed_reader(&oracle, Side::Redis, quit);
        let ours = slow_half_closed_reader(&node, Side::Node, quit);
        failures.extend(check_answered_in_full(label, &full, &redis, &ours));
    }
    assert!(failures.is_empty(), "a half-closed slow reader:\n{}", failures.join("\n"));
}

/// One `GET big` reply's bytes.
fn slow_reply() -> Vec<u8> {
    let mut reply = b"$1048576\r\n".to_vec();
    reply.extend(std::iter::repeat_n(b'b', 1 << 20));
    reply.extend_from_slice(b"\r\n");
    assert_eq!(reply.len(), BIG_REPLY_BYTES);
    reply
}

/// A subscriber whose end of stream the server has read is no receiver,
/// whether a `QUIT` marked it first or not: once Redis has read the FIN (its
/// row gone) a `PUBLISH` reads `:0` on both servers, and nothing published
/// after the FIN reaches the subscriber (ADR-0196 A1.2). What it reads is
/// declared (A1.6, D6.4 and D6.5): this server sends every message it held,
/// then the `QUIT`'s `+OK` when one was sent, and lists the client until
/// then; Redis drops at the FIN what its socket had not taken, and the row.
#[test]
fn a_subscriber_that_ends_its_stream_is_not_counted_like_redis() {
    let Some((_node_guard, node)) = infinityd(1, scratch_base()) else {
        eprintln!("SKIPPED: INFINITYD_BIN unset — real-node compat lane not run");
        return;
    };
    let (_oracle_guard, oracle) = oracle();
    let mut failures = Vec::new();
    for quit in [false, true] {
        let mut full = fill_message().repeat(FILL_MESSAGES);
        if quit {
            full.extend_from_slice(b"+OK\r\n");
        }
        let label =
            if quit { "an unread subscriber, QUIT, FIN" } else { "an unread subscriber, FIN" };
        let redis = unread_subscriber(&oracle, Side::Redis, quit, true);
        let ours = unread_subscriber(&node, Side::Node, quit, true);
        for (who, reading) in [("redis", &redis), ("node", &ours)] {
            if reading.count.as_deref() != Some(b":0\r\n") {
                failures.push(format!(
                    "{label}: {who}'s PUBLISH after the FIN read {:?} (owed :0)",
                    reading.count.as_deref().map(String::from_utf8_lossy)
                ));
            }
        }
        failures.extend(check_answered_in_full(label, &full, &redis, &ours));
    }
    assert!(failures.is_empty(), "a subscriber that ends its stream:\n{}", failures.join("\n"));
}

/// A subscriber marked by `QUIT` whose stream is open still counts as a
/// receiver, and is written nothing published after the mark: with 6,000
/// messages held for it (Redis holding at least 1 MiB, its row marked `c`)
/// and its `QUIT` sent, a `PUBLISH` reads `:1` on both servers, and the
/// subscriber then reads the 6,000, its `+OK` and the end, byte for byte the
/// same (ADR-0196 D4, A1.2).
#[test]
fn a_marked_subscriber_counts_until_its_stream_ends_like_redis() {
    let Some((_node_guard, node)) = infinityd(1, scratch_base()) else {
        eprintln!("SKIPPED: INFINITYD_BIN unset — real-node compat lane not run");
        return;
    };
    let (_oracle_guard, oracle) = oracle();
    let mut full = fill_message().repeat(FILL_MESSAGES);
    full.extend_from_slice(b"+OK\r\n");
    let mut failures = Vec::new();
    for (who, server, side) in [("redis", &oracle, Side::Redis), ("node", &node, Side::Node)] {
        let reading = unread_subscriber(server, side, true, false);
        if let Err(engagement) = &reading.engaged {
            failures.push(format!("{who}: VACUOUS engagement: {engagement}"));
            continue;
        }
        let count = reading.count.as_deref().map(String::from_utf8_lossy);
        if count.as_deref() != Some(":1\r\n") || reading.bytes != full || reading.end != End::Fin {
            failures.push(format!(
                "{who}: the PUBLISH while marked read {count:?} (owed :1); the subscriber read {} \
                 bytes of {} (6,000 messages and the +OK), then {:?}",
                reading.bytes.len(),
                full.len(),
                reading.end
            ));
        }
    }
    assert!(failures.is_empty(), "a marked subscriber:\n{}", failures.join("\n"));
}

/// One message to `ch` carrying `m`, as a subscriber reads it.
const MESSAGE: &[u8] = b"*3\r\n$7\r\nmessage\r\n$2\r\nch\r\n$1\r\nm\r\n";

/// A subscriber of `ch` sends `QUIT` 0.3 s into another client's publishing
/// to `ch` in bursts of 100, which goes on until the subscriber's stream
/// ends. Its bytes until the server closed it, and how it closed.
fn quitting_subscriber(server: &TcpStream) -> (Vec<u8>, End) {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;
    let mut subscriber = sibling(server);
    let mut sub_buf = Vec::new();
    let confirmation = cmd(&mut subscriber, &mut sub_buf, &["SUBSCRIBE", "ch"]);
    assert_eq!(confirmation, SUBSCRIBED, "the SUBSCRIBE confirmation");
    let stop = Arc::new(AtomicBool::new(false));
    let mut publisher = sibling(server);
    let publishing = {
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let burst: Vec<u8> = (0..100).flat_map(|_| wire(&["PUBLISH", "ch", "m"])).collect();
            let mut buf = Vec::new();
            while !stop.load(Ordering::Relaxed) {
                publisher.write_all(&burst).expect("publish");
                read_frames(&mut publisher, &mut buf, 100);
            }
        })
    };
    std::thread::sleep(Duration::from_millis(300));
    let early = write_then_fin(&mut subscriber, &wire(&["QUIT"]), false);
    let (bytes, end) = read_on(&mut subscriber, early);
    stop.store(true, Ordering::Relaxed);
    publishing.join().expect("the publisher ran to its stop");
    sub_buf.extend(bytes);
    (sub_buf, end)
}

/// A subscriber that sends `QUIT` while its channel is published to reads
/// messages, its `+OK`, then no message, and the server ends its stream
/// while the publishing goes on, on both servers: no published message is
/// written after a `QUIT` (ADR-0196 D4). The messages read before the `+OK`
/// are the leg's liveness: the publisher ran. The node runs one cell, where
/// the publisher's commands and the subscriber's `QUIT` share the parse, so a
/// delivery reaches the marked slot within its flush, and two, where the
/// publisher may be the other cell's.
#[test]
fn a_subscriber_that_quits_is_sent_no_message_after_its_ok_like_redis() {
    let Some((_one_guard, one_cell)) = infinityd(1, scratch_base()) else {
        eprintln!("SKIPPED: INFINITYD_BIN unset — real-node compat lane not run");
        return;
    };
    let (_two_guard, two_cells) = infinityd(2, scratch_base()).expect("the node lane is on");
    let (_oracle_guard, oracle) = oracle();
    let mut failures = Vec::new();
    let servers =
        [("redis", &oracle), ("node, one cell", &one_cell), ("node, two cells", &two_cells)];
    for (who, server) in servers {
        let (bytes, end) = quitting_subscriber(server);
        let mut at = 0;
        while bytes[at..].starts_with(MESSAGE) {
            at += MESSAGE.len();
        }
        let before = at / MESSAGE.len();
        let after = bytes[at..].strip_prefix(b"+OK\r\n");
        println!("{who}: {before} messages before the QUIT's +OK, then {end:?}");
        if before == 0 || after != Some(&[][..]) || end == End::Open {
            failures.push(format!(
                "{who}: {before} messages before the +OK (owed at least 1), then {:?} (owed \
                 the +OK and nothing after it), then {end:?} (owed the end)",
                String::from_utf8_lossy(&bytes[at..bytes.len().min(at + 120)])
            ));
        }
    }
    assert!(failures.is_empty(), "a subscriber that quits:\n{}", failures.join("\n"));
}

/// The shapes of the `QUIT` case: what shares the `QUIT`'s write.
#[derive(Clone, Copy, Debug)]
enum QuitShape {
    /// `QUIT` alone, on a bound connection.
    Alone,
    /// Ten `SET`s, `QUIT`, ten `SET`s, on a bound connection.
    BehindSets,
    /// A `DEL` of 32 keys both cells own, `QUIT`, ten `SET`s.
    BehindDel,
    /// `QUIT`, then ten `SET`s: the parse stops at the `QUIT`.
    Inline,
}

impl QuitShape {
    const ALL: [QuitShape; 4] =
        [QuitShape::Alone, QuitShape::BehindSets, QuitShape::BehindDel, QuitShape::Inline];

    /// Whether the node's connection is bound by `INF.NS USE` first.
    fn bound(self) -> bool {
        matches!(self, QuitShape::Alone | QuitShape::BehindSets)
    }
}

/// What one `QUIT` shape read.
struct QuitReading {
    replies: Vec<u8>,
    end: End,
    /// How many `SET`s before the `QUIT` landed, and how many behind it.
    landed: usize,
    landed_behind: usize,
}

/// `shape`'s write on a fresh connection, bound to `ns` when given, then a
/// read to the end and a count of the `SET`s that landed.
fn quit_shape(server: &TcpStream, ns: Option<&str>, shape: QuitShape) -> QuitReading {
    let label = format!("quit-{shape:?}");
    let before = names(&format!("{label}-before"), 10);
    let behind = names(&format!("{label}-behind"), 10);
    let (before, behind) = match shape {
        QuitShape::Alone => (Vec::new(), Vec::new()),
        QuitShape::BehindSets => (before, behind),
        QuitShape::BehindDel | QuitShape::Inline => (Vec::new(), behind),
    };
    let mut client = sibling(server);
    if let Some(ns) = ns {
        assert_eq!(cmd(&mut client, &mut Vec::new(), &["INF.NS", "USE", ns]), b"+OK\r\n");
    }
    let mut bytes = sets(&before);
    if let QuitShape::BehindDel = shape {
        let keys = names(&format!("{label}-del"), 32);
        let mut argv = vec!["DEL"];
        argv.extend(keys.iter().map(String::as_str));
        bytes.extend(wire(&argv));
    }
    bytes.extend(wire(&["QUIT"]));
    bytes.extend(sets(&behind));
    let early = write_then_fin(&mut client, &bytes, false);
    let (replies, end) = read_on(&mut client, early);
    let landed = existing(server, ns, &before);
    QuitReading { replies, end, landed, landed_behind: existing(server, ns, &behind) }
}

/// A frame whose bulk length is not a number: a protocol error.
const BAD_FRAME: &[u8] = b"*1\r\n$x\r\n";

/// The shapes of the protocol-error case: what precedes the bad frame in
/// its one write.
#[derive(Clone, Copy, Debug)]
enum ErrorShape {
    /// A `SET` of a key each cell owns (`foo` cell 1's, `bar` cell 0's), so
    /// whichever cell holds the connection one reply is owed when the parse
    /// meets the error.
    OwedReply,
    /// On the node a connection bound by `INF.NS USE`, where every command
    /// rides its queue: ten `SET`s, the frame, ten `SET`s.
    Queued,
    /// The frame alone: nothing is owed.
    Alone,
}

/// `shape`'s write on a fresh connection, bound to `ns` when given: its
/// replies, how the stream ended, and the `SET`s before and behind the frame
/// that landed.
fn protocol_error_shape(server: &TcpStream, ns: Option<&str>, shape: ErrorShape) -> QuitReading {
    let label = format!("protocol-{shape:?}");
    let (before, behind) = match shape {
        ErrorShape::OwedReply => (vec!["foo".to_string(), "bar".to_string()], Vec::new()),
        ErrorShape::Queued => {
            (names(&format!("{label}-before"), 10), names(&format!("{label}-behind"), 10))
        }
        ErrorShape::Alone => (Vec::new(), Vec::new()),
    };
    let mut client = sibling(server);
    if let Some(ns) = ns {
        assert_eq!(cmd(&mut client, &mut Vec::new(), &["INF.NS", "USE", ns]), b"+OK\r\n");
    }
    let mut bytes = sets(&before);
    bytes.extend_from_slice(BAD_FRAME);
    bytes.extend(sets(&behind));
    let early = write_then_fin(&mut client, &bytes, false);
    let (replies, end) = read_on(&mut client, early);
    let landed = existing(server, ns, &before);
    QuitReading { replies, end, landed, landed_behind: existing(server, ns, &behind) }
}

/// A protocol error is answered after the replies of every command before it,
/// then the server closes the connection, and nothing after the bad frame
/// runs: one reply order for every reply the parse writes, at two cells, on
/// both servers. Redis reads the bound shape's bytes on a plain connection.
#[test]
fn a_protocol_error_is_answered_after_the_replies_before_it_like_redis() {
    let Some((_node_guard, mut node)) = infinityd(2, scratch_base()) else {
        eprintln!("SKIPPED: INFINITYD_BIN unset — real-node compat lane not run");
        return;
    };
    let (_oracle_guard, oracle) = oracle();
    let create = ["INF.NS", "CREATE", "proto", "MODE", "memory"];
    assert_eq!(cmd(&mut node, &mut Vec::new(), &create), b"+OK\r\n", "INF.NS CREATE");
    let mut failures = Vec::new();
    for shape in [ErrorShape::OwedReply, ErrorShape::Queued, ErrorShape::Alone] {
        let ns = matches!(shape, ErrorShape::Queued).then_some("proto");
        let redis = protocol_error_shape(&oracle, None, shape);
        let ours = protocol_error_shape(&node, ns, shape);
        let error_last = redis.replies.ends_with(b"-ERR Protocol error: invalid bulk length\r\n");
        let same = redis.replies == ours.replies && redis.landed == ours.landed;
        let behind = (redis.landed_behind, ours.landed_behind);
        let ends = [redis.end, ours.end];
        if !error_last || !same || behind != (0, 0) || ends.contains(&End::Open) {
            failures.push(format!(
                "{shape:?}: redis {:?}, {} landed before the frame, {} after, {:?}; node {:?}, {} \
                 before, {} after, {:?}",
                String::from_utf8_lossy(&redis.replies),
                redis.landed,
                redis.landed_behind,
                redis.end,
                String::from_utf8_lossy(&ours.replies),
                ours.landed,
                ours.landed_behind,
                ours.end,
            ));
        }
    }
    assert!(failures.is_empty(), "a protocol error differs from Redis:\n{}", failures.join("\n"));
}

/// What a client that reset its connection with a pipeline unread left
/// behind: whether its row left `CLIENT LIST` in the hang bound, after which
/// the server runs nothing more of it, and how many of its `SET`s ran.
struct ResetReading {
    unlisted: bool,
    landed: usize,
}

/// `count` pipelined `SET`s written while the server is held by `by`, then
/// the client's reset (`SO_LINGER` 0, then the close), then the hold's
/// release: the server reads what its socket kept, and its first send to the
/// reset socket fails.
fn reset_while_held(server: &TcpStream, by: HoldBy, prefix: &str, count: usize) -> ResetReading {
    let names = names(prefix, count);
    let mut client = sibling(server);
    let id = client_id(&mut client);
    let hold = Hold::begin(server, by);
    assert_eq!(write_then_fin(&mut client, &sets(&names), false), None, "{prefix}: an early close");
    reset(client);
    if let Some(hold) = hold {
        hold.release();
    }
    let unlisted = unlisted_in_time(server, &id);
    ResetReading { unlisted, landed: existing(server, None, &names) }
}

/// A client writes 5,000 `SET`s while the server reads nothing, then resets
/// its connection, and the server reads again. Redis reads one buffer of the
/// pipeline (about 20 KB), runs it, fails to write the replies to the reset
/// socket (`ECONNRESET`) and frees the client, running nothing more of what
/// its socket kept; this server runs every command its receive returned
/// before the close, unanswered (ADR-0196 D1). Declared (ADR-0196 D6.3):
/// Redis runs fewer than it was sent and at least one, this server at least
/// as many as Redis; both counts are printed. One cell, where a cell's
/// `CLIENT LIST` is the node's.
#[test]
fn a_write_error_stops_redis_but_not_what_this_server_read() {
    let Some((node_guard, node)) = infinityd(1, scratch_base()) else {
        eprintln!("SKIPPED: INFINITYD_BIN unset — real-node compat lane not run");
        return;
    };
    let (oracle_guard, oracle) = oracle();
    let count = 5_000;
    let redis = reset_while_held(&oracle, oracle_hold(&oracle_guard), "write-error", count);
    let node_by = HoldBy::Signal(node_guard.pid());
    let ours = reset_while_held(&node, node_by, "write-error", count);
    println!(
        "a write error: redis ran {} of {count} SETs, this server {}",
        redis.landed, ours.landed
    );
    let mut failures = Vec::new();
    if !redis.unlisted || !ours.unlisted {
        failures.push(format!(
            "a reset client still listed: redis {}, node {}",
            !redis.unlisted, !ours.unlisted
        ));
    }
    if redis.landed == 0 || redis.landed >= count {
        failures.push(format!(
            "redis ran {} of {count} SETs (owed at least one, and fewer than all: D6.3 \
             unobserved)",
            redis.landed
        ));
    }
    if ours.landed < redis.landed {
        failures.push(format!(
            "this server ran {} of the SETs it read, fewer than redis's {}",
            ours.landed, redis.landed
        ));
    }
    assert!(failures.is_empty(), "a write error:\n{}", failures.join("\n"));
}
