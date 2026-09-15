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
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;

use compat::harness::{
    CaseOverride, Expect, candidate, infinityd, infinityd_at, oracle, parse_int_reply, read_frames,
    run_matrix, spawn_infinityd_at,
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
    reason = "one linear phase script; splitting would scatter the \
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
            &b"-ERR multi-key commands spanning cells are not yet supported in named namespaces \
                 (M2)\r\n"[..],
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
    let Some((_oracle_guard, mut oracle)) = oracle() else {
        eprintln!("SKIPPED: redis-server not installed — compat AC stays evidence-pending");
        return;
    };
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
    let Some((_oracle_guard, mut oracle)) = oracle() else {
        eprintln!("SKIPPED: redis-server not installed — compat AC stays evidence-pending");
        return;
    };
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
    let Some((_oracle_guard, mut oracle)) = oracle() else {
        eprintln!("SKIPPED: redis-server not installed — compat AC stays evidence-pending");
        return;
    };
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
    let Some((_oracle_guard, mut oracle)) = oracle() else {
        eprintln!("SKIPPED: redis-server not installed — compat AC stays evidence-pending");
        return;
    };
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
    let Some((_oracle_guard, mut oracle)) = oracle() else {
        eprintln!("SKIPPED: redis-server not installed — compat AC stays evidence-pending");
        return;
    };
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
    let Some((_oracle_guard, mut oracle)) = oracle() else {
        eprintln!("SKIPPED: redis-server not installed — compat AC stays evidence-pending");
        return;
    };
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
    let Some((_oracle_guard, mut oracle)) = oracle() else {
        eprintln!("SKIPPED: redis-server not installed — compat AC stays evidence-pending");
        return;
    };
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
    let Some((_oracle_guard, oracle)) = oracle() else {
        eprintln!("SKIPPED: redis-server not installed — compat AC stays evidence-pending");
        return;
    };
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
    let Some((_oracle_guard, mut oracle)) = oracle() else {
        eprintln!("SKIPPED: redis-server not installed — compat AC stays evidence-pending");
        return;
    };
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
    let Some((_oracle_guard, mut oracle)) = oracle() else {
        eprintln!("SKIPPED: redis-server not installed — compat AC stays evidence-pending");
        return;
    };
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
