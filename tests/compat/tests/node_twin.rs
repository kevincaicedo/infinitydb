//! The cell-count twin lane (review of 2026-08-30, C1 / Theme 3 — the
//! "command-table-iterating coverage test" the C1 entry owed): every
//! scripted compat case is replayed against **two** real `infinityd`
//! processes — one cell and four cells — from the same binding, and the
//! two replies are compared per case. The cell count must be invisible
//! to a client: C1 was `SCAN`/`KEYS`/`RANDOMKEY`/`FLUSHALL` on a
//! namespace-bound connection serving one cell of four while reporting a
//! complete answer, and the fix was applied command by command until
//! `DEBUG OBJECT` (L12-01) closed the class. This lane asserts the class:
//! the whole `MATRIX` and the `JSON_CASES` script, from four bindings —
//! the default database, a memory namespace, a flat durable namespace,
//! and a tiered namespace — in two passes each, with a static check that
//! every registry row appears in the scripts, so a new command cannot
//! land outside it.
//!
//! **Two passes per binding.** *Spread*: the script's keys as written,
//! placed by the secret-seeded hash over four cells — the C1 shape for
//! the keyspace-wide programs. Under a namespace binding a multi-key
//! command whose keys span cells refuses typed on the 4-cell node
//! (ADR-0015's recorded M2 cut) and succeeds on the 1-cell node; that
//! refusal is the one pinned deviation, and when the refused command was
//! a write its keys are **tainted** — the twins' state has diverged for
//! them, so later cases naming a tainted key (or sizing the whole
//! keyspace) compare by shape and are counted, never silently passed
//! and never blamed on the cell count. *Tagged*: every key position
//! rewritten under one hashtag (`{twin}k`), so no command spans cells,
//! nothing is tainted, and every reply — including the multi-key
//! commands that ride `ApplyNs` to the tag's owner from a connection on
//! another cell — compares exactly.
//!
//! What is compared: `ByteExact`/`Frames` cases byte-for-byte (one frame
//! per command asserted, as in the oracle lane); `IntWithin` within the
//! same tolerance; cases the oracle lane skips are still compared here
//! (both sides are InfinityDB): `InfinityDB extension` and `arity+keyspec`
//! cases byte-exact, payloads with per-node identity or per-cell state by
//! shape; `KEYS` replies as sets (order is cell-dependent), `SCAN` and
//! `RANDOMKEY` by shape (cursor encoding and the random draw are
//! topology-specific). Identical bytes always pass.
//!
//! Gating: `INFINITYD_BIN` (set by `just compat` and CI); unset skips
//! loudly. No redis oracle is needed — the 1-cell node is the reference.

use std::collections::BTreeSet;
use std::io::Write;
use std::net::TcpStream;
use std::path::Path;

use compat::harness::{count_frames, infinityd, parse_int_reply, read_frames};
use compat::json_oracle::JSON_CASES;
use compat::matrix::{Check, MATRIX};
use compat::resp::encode_command;
use inf_wire::{
    COMMANDS, CmdFlags, CommandId, KeySpec, KeyspaceScope, key_spec, keyspace_scope, lookup,
};

fn scratch_base() -> &'static Path {
    Path::new(env!("CARGO_TARGET_TMPDIR"))
}

const SPANNING_REFUSAL: &[u8] =
    b"-ERR multi-key commands spanning cells are not yet supported in named namespaces (M2)\r\n";
const TAG: &str = "{twin}";

/// The bindings the script runs under. Each is a fresh connection per
/// node with the same prefix on both.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum Binding {
    DefaultDb,
    MemoryNs,
    DurableNs,
    TieredNs,
}

/// Spread keys (as scripted) or tagged keys (one hashtag, no spanning).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum Pass {
    Spread,
    Tagged,
}

impl Binding {
    const ALL: [Binding; 4] =
        [Binding::DefaultDb, Binding::MemoryNs, Binding::DurableNs, Binding::TieredNs];

    /// One namespace per binding × pass: a pass must not inherit the
    /// other pass's (possibly tainted) state.
    fn ns(self, pass: Pass) -> Option<String> {
        let stem = match self {
            Binding::DefaultDb => return None,
            Binding::MemoryNs => "twin-mem",
            Binding::DurableNs => "twin-dur",
            Binding::TieredNs => "twin-tier",
        };
        let suffix = match pass {
            Pass::Spread => "spread",
            Pass::Tagged => "tagged",
        };
        Some(format!("{stem}-{suffix}"))
    }

    fn create(self, pass: Pass) -> Option<Vec<String>> {
        let ns = self.ns(pass)?;
        let tail: &[&str] = match self {
            Binding::DefaultDb => unreachable!("no namespace"),
            Binding::MemoryNs => &["MODE", "memory"],
            Binding::DurableNs => &["MODE", "durable", "FSYNC", "everysec"],
            Binding::TieredNs => &["MODE", "durable", "MEM-BUDGET", "64mb", "DISK-BUDGET", "256mb"],
        };
        let mut argv = vec!["INF.NS".to_string(), "CREATE".to_string(), ns];
        argv.extend(tail.iter().map(|s| (*s).to_string()));
        Some(argv)
    }
}

/// How one case is compared between the twins.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum Twin {
    Exact,
    Frames(usize),
    IntWithin(i64),
    /// Array replies compared as sets (reply order is cell-dependent).
    SetEqual,
    /// Same reply type, both complete frames (payload carries per-node
    /// identity, per-cell state, a cursor, or a random draw).
    Shape,
    /// The twins' state diverged for this case's keys (a refused
    /// spanning write earlier in the script): both replies must be
    /// complete frames, and nothing else is claimed.
    Tainted,
}

fn twin_check(argv: &[String], check: Check) -> Twin {
    let head = argv[0].to_ascii_uppercase();
    match head.as_str() {
        "KEYS" => return Twin::SetEqual,
        "SCAN" | "RANDOMKEY" => return Twin::Shape,
        _ => {}
    }
    match check {
        Check::ByteExact => Twin::Exact,
        Check::Frames(n) => Twin::Frames(n),
        Check::IntWithin(t) => Twin::IntWithin(t),
        Check::SkipDiff(why) => {
            if why.contains("InfinityDB extension") || why.contains("arity+keyspec") {
                Twin::Exact
            } else {
                Twin::Shape
            }
        }
    }
}

/// `*N` of bulks → the element set (`None` for any other shape).
fn bulk_set(reply: &[u8]) -> Option<BTreeSet<Vec<u8>>> {
    if reply.first() != Some(&b'*') {
        return None;
    }
    let header_end = reply.windows(2).position(|w| w == b"\r\n")? + 2;
    let count: usize = std::str::from_utf8(&reply[1..header_end - 2]).ok()?.parse().ok()?;
    let mut at = header_end;
    let mut set = BTreeSet::new();
    for _ in 0..count {
        if reply.get(at) != Some(&b'$') {
            return None;
        }
        let len_end = at + reply[at..].windows(2).position(|w| w == b"\r\n")? + 2;
        let len: usize = std::str::from_utf8(&reply[at + 1..len_end - 2]).ok()?.parse().ok()?;
        set.insert(reply.get(len_end..len_end + len)?.to_vec());
        at = len_end + len + 2;
    }
    (at == reply.len()).then_some(set)
}

/// Key positions of one case per the registry's scoped key spec.
fn key_positions(argv: &[String]) -> Vec<usize> {
    let Some(meta) = lookup(argv[0].as_bytes()) else { return Vec::new() };
    let spec = key_spec(meta, argv.get(1).map(|s| s.as_bytes()));
    if spec == KeySpec::NONE || argv.is_empty() {
        return Vec::new();
    }
    let last = if spec.last >= 0 {
        usize::from(spec.last.unsigned_abs())
    } else {
        argv.len().saturating_sub(usize::from(spec.last.unsigned_abs()))
    };
    let first = usize::from(spec.first);
    if last < first || last >= argv.len() {
        return Vec::new();
    }
    (first..=last).step_by(usize::from(spec.step.max(1))).collect()
}

fn scope_of(argv: &[String]) -> KeyspaceScope {
    lookup(argv[0].as_bytes())
        .map_or(KeyspaceScope::None, |m| keyspace_scope(m, argv.get(1).map(|s| s.as_bytes())))
}

fn is_write(argv: &[String]) -> bool {
    lookup(argv[0].as_bytes()).is_some_and(|m| m.flags.contains(CmdFlags::WRITE))
}

/// The case's argv under `pass`: tagged rewrites every key position.
fn argv_for(argv: &[&str], pass: Pass) -> Vec<String> {
    let mut owned: Vec<String> = argv.iter().map(|s| (*s).to_string()).collect();
    if pass == Pass::Tagged {
        for at in key_positions(&owned) {
            owned[at] = format!("{TAG}{}", owned[at]);
        }
    }
    owned
}

struct Conn {
    stream: TcpStream,
    buf: Vec<u8>,
}

impl Conn {
    fn call(&mut self, argv: &[String], frames: usize) -> Vec<u8> {
        self.stream.write_all(&encode_command(argv)).expect("write");
        read_frames(&mut self.stream, &mut self.buf, frames)
    }

    fn call_str(&mut self, argv: &[&str]) -> Vec<u8> {
        let owned: Vec<String> = argv.iter().map(|s| (*s).to_string()).collect();
        self.call(&owned, 1)
    }
}

#[derive(Default)]
struct Report {
    compared: usize,
    exact: usize,
    /// Cases compared by shape because a tainted key (or the whole
    /// keyspace after a taint) was involved.
    tainted: usize,
    deviations: Vec<String>,
    failures: Vec<String>,
}

/// Replays `cases` on both twins under `binding` / `pass`.
fn replay(
    label: &str,
    binding: Binding,
    pass: Pass,
    cases: &[(&[&str], Check)],
    one: &mut Conn,
    four: &mut Conn,
    report: &mut Report,
) {
    let mut taint: BTreeSet<String> = BTreeSet::new();
    for (i, (raw, check)) in cases.iter().enumerate() {
        let argv = argv_for(raw, pass);
        let mut twin = twin_check(&argv, *check);
        let frames = match twin {
            Twin::Frames(n) => n,
            _ => 1,
        };
        let positions = key_positions(&argv);
        let a = one.call(&argv, frames);
        let b = four.call(&argv, frames);
        report.compared += 1;
        let name = format!("{label} case {i} {raw:?} under {binding:?}/{pass:?}");
        let show = |r: &[u8]| String::from_utf8_lossy(&r[..r.len().min(160)]).into_owned();
        // ADR-0015's recorded M2 cut, and nothing else: only under a
        // namespace binding, only with ≥ 2 key positions, only this exact
        // refusal, only on the 4-cell side. A refused write diverges the
        // twins' state for its keys: taint them.
        if binding != Binding::DefaultDb && b == SPANNING_REFUSAL && positions.len() >= 2 && a != b
        {
            report
                .deviations
                .push(format!("{name}: keys span cells — typed refusal (ADR-0015 M2)"));
            if is_write(&argv) {
                for &at in &positions {
                    taint.insert(argv[at].clone());
                }
            }
            continue;
        }
        let touches_taint = positions.iter().any(|&at| taint.contains(&argv[at]))
            || (!taint.is_empty() && scope_of(&argv) == KeyspaceScope::Whole);
        if touches_taint && a != b {
            report.tainted += 1;
            twin = Twin::Tainted;
            // A write through a tainted key keeps it tainted; nothing
            // untaints (the script never converges the twins).
        }
        if a == b && count_frames(&b) == Some(frames) {
            report.exact += 1;
            twin = Twin::Exact;
        }
        match twin {
            Twin::Exact | Twin::Frames(_) => {
                if count_frames(&b) != Some(frames) {
                    report.failures.push(format!(
                        "{name}: 4-cell answered {:?} frames, not {frames}: {}",
                        count_frames(&b),
                        show(&b)
                    ));
                } else if a != b {
                    report.failures.push(format!(
                        "{name}:\n  1-cell {}\n  4-cell {}",
                        show(&a),
                        show(&b)
                    ));
                }
            }
            Twin::IntWithin(tolerance) => match (parse_int_reply(&a), parse_int_reply(&b)) {
                (Some(x), Some(y)) if (x - y).abs() <= tolerance => {}
                _ => report.failures.push(format!(
                    "{name}: integers within ±{tolerance} expected:\n  1-cell {}\n  4-cell {}",
                    show(&a),
                    show(&b)
                )),
            },
            Twin::SetEqual => match (bulk_set(&a), bulk_set(&b)) {
                (Some(x), Some(y)) if x == y => {}
                _ => report.failures.push(format!(
                    "{name}: array sets differ:\n  1-cell {}\n  4-cell {}",
                    show(&a),
                    show(&b)
                )),
            },
            Twin::Tainted => {
                let ok = count_frames(&a).is_some_and(|n| n >= 1)
                    && count_frames(&b).is_some_and(|n| n >= 1);
                if !ok {
                    report.failures.push(format!(
                        "{name}: a tainted case answered incomplete frames:\n  1-cell {}\n  4-cell {}",
                        show(&a),
                        show(&b)
                    ));
                }
            }
            Twin::Shape => {
                let ok = count_frames(&a).is_some_and(|n| n >= 1)
                    && count_frames(&b).is_some_and(|n| n >= 1)
                    && a.first() == b.first();
                if !ok {
                    report.failures.push(format!(
                        "{name}: reply shapes differ:\n  1-cell {}\n  4-cell {}",
                        show(&a),
                        show(&b)
                    ));
                }
            }
        }
        // A successful `SELECT` replaces the namespace binding on both
        // twins: re-bind so the rest of the script stays under `binding`.
        if let Some(ns) = binding.ns(pass)
            && argv[0].eq_ignore_ascii_case("SELECT")
            && a == b"+OK\r\n"
        {
            for (who, conn) in [("1-cell", &mut *one), ("4-cell", &mut *four)] {
                let r = conn.call_str(&["INF.NS", "USE", &ns]);
                assert_eq!(r, b"+OK\r\n", "{who}: re-binding {ns} after {raw:?}");
            }
        }
    }
}

/// Every registry row appears in the scripts the twins replay, so a new
/// command cannot land outside the cell-count-invariance check. The
/// exclusions carry their reason: two internal fabric-program ops a
/// client never speaks, and `QUIT`, which ends the scripted connection
/// (its bindings are covered by `node_e2e::connection_level_commands_
/// ignore_the_bound_namespace`).
#[test]
fn every_registry_command_is_scripted() {
    let mut scripted: BTreeSet<String> = BTreeSet::new();
    for case in MATRIX {
        scripted.insert(case.argv[0].to_ascii_uppercase());
    }
    for case in JSON_CASES {
        scripted.insert(case.argv[0].to_ascii_uppercase());
    }
    let excluded = |id: CommandId| match id {
        CommandId::InfTake | CommandId::InfPeek => {
            Some("internal fabric-program op (RENAME/COPY legs) — never a client case")
        }
        CommandId::Quit => Some("closes the scripted connection — covered by the conn-level e2e"),
        _ => None,
    };
    let missing: Vec<&str> = COMMANDS
        .iter()
        .filter(|m| excluded(m.id).is_none())
        .filter(|m| !scripted.contains(&m.name.to_ascii_uppercase()))
        .map(|m| m.name)
        .collect();
    assert!(
        missing.is_empty(),
        "registry rows with no scripted case (add one to MATRIX or JSON_CASES): {missing:?}"
    );
    let mut excluded_rows = 0;
    for m in COMMANDS.iter().filter(|m| excluded(m.id).is_some()) {
        excluded_rows += 1;
        if m.id != CommandId::Quit {
            assert!(
                !scripted.contains(&m.name.to_ascii_uppercase()),
                "{}: {}",
                m.name,
                excluded(m.id).expect("excluded")
            );
        }
    }
    println!(
        "twin coverage: {} of {} registry rows scripted ({excluded_rows} excluded with reasons)",
        COMMANDS.len() - excluded_rows,
        COMMANDS.len()
    );
}

#[test]
fn one_cell_and_four_cell_nodes_answer_alike_under_every_binding() {
    let Some((_one_guard, one_admin)) = infinityd(1, scratch_base()) else {
        eprintln!("SKIPPED: INFINITYD_BIN unset — twin lane not run (C1 coverage)");
        return;
    };
    let (_four_guard, four_admin) = infinityd(4, scratch_base()).expect("INFINITYD_BIN is set");
    let mut one_admin = Conn { stream: one_admin, buf: Vec::new() };
    let mut four_admin = Conn { stream: four_admin, buf: Vec::new() };
    let one_addr = one_admin.stream.peer_addr().expect("addr");
    let four_addr = four_admin.stream.peer_addr().expect("addr");
    // Namespaces exist on both twins before any binding runs (the script
    // is stateful: every binding sees the same catalog on both sides).
    for binding in Binding::ALL {
        for pass in [Pass::Spread, Pass::Tagged] {
            if let Some(create) = binding.create(pass) {
                for (who, conn) in [("1-cell", &mut one_admin), ("4-cell", &mut four_admin)] {
                    assert_eq!(conn.call(&create, 1), b"+OK\r\n", "{who}: {create:?}");
                }
            }
        }
    }
    let matrix: Vec<(&[&str], Check)> = MATRIX.iter().map(|c| (c.argv, c.check)).collect();
    let json: Vec<(&[&str], Check)> =
        JSON_CASES.iter().map(|c| (c.argv, Check::ByteExact)).collect();
    let mut report = Report::default();
    let mut lines = Vec::new();
    for pass in [Pass::Spread, Pass::Tagged] {
        for binding in Binding::ALL {
            let connect = |addr| {
                let s = TcpStream::connect(addr).expect("connect twin");
                s.set_read_timeout(Some(std::time::Duration::from_secs(10))).expect("timeout");
                Conn { stream: s, buf: Vec::new() }
            };
            let mut one = connect(one_addr);
            let mut four = connect(four_addr);
            if let Some(ns) = binding.ns(pass) {
                for (who, conn) in [("1-cell", &mut one), ("4-cell", &mut four)] {
                    assert_eq!(
                        conn.call_str(&["INF.NS", "USE", &ns]),
                        b"+OK\r\n",
                        "{who}: USE {ns}"
                    );
                }
            }
            let before = (
                report.compared,
                report.exact,
                report.tainted,
                report.deviations.len(),
                report.failures.len(),
            );
            replay("matrix", binding, pass, &matrix, &mut one, &mut four, &mut report);
            replay("json", binding, pass, &json, &mut one, &mut four, &mut report);
            lines.push(format!(
                "{binding:?}/{pass:?}: {} compared ({} byte-identical), {} tainted-by-shape, {} \
                 spanning-key deviations, {} failures",
                report.compared - before.0,
                report.exact - before.1,
                report.tainted - before.2,
                report.deviations.len() - before.3,
                report.failures.len() - before.4
            ));
        }
    }
    println!(
        "compat twin lane (1-cell vs 4-cell infinityd): {} cases compared over {} bindings × 2 \
         passes — {} byte-identical, {} tainted-by-shape, {} spanning-key deviations (ADR-0015 M2 \
         cut), {} failures",
        report.compared,
        Binding::ALL.len(),
        report.exact,
        report.tainted,
        report.deviations.len(),
        report.failures.len()
    );
    for line in &lines {
        println!("  {line}");
    }
    for line in &report.deviations {
        println!("  deviation: {line}");
    }
    assert!(
        report.failures.is_empty(),
        "{} twin mismatches (the cell count leaked into a reply):\n{}",
        report.failures.len(),
        report.failures.join("\n")
    );
    // The deviation is a namespace-binding, spread-pass phenomenon by
    // construction: under the default database every multi-key command
    // gathers, and under one hashtag nothing spans.
    assert!(
        report.deviations.iter().all(|d| !d.contains("DefaultDb") && !d.contains("Tagged")),
        "a deviation outside the spread/namespace class:\n{}",
        report.deviations.join("\n")
    );
    // The tagged pass is the exact half: nothing tainted, so every case
    // that is not shape-by-rule compared byte-for-byte.
    assert!(
        lines.iter().filter(|l| l.contains("Tagged")).all(|l| l.contains(" 0 tainted-by-shape")),
        "the tagged pass tainted something:\n{}",
        lines.join("\n")
    );
}
