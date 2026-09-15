//! H3 binary tier: cross-owner moves under deterministic memory pressure.
//! Preload with no limit, then lower maxmemory below both cells' live bytes.

use std::io::Write;
use std::net::TcpStream;
use std::path::Path;

use compat::harness::{infinityd, oracle, read_frames};
use inf_foundation::CellId;
use inf_store::SlotRouter;

/// INFO counters follow the Redis oracle across both rename directions.
#[test]
fn cross_cell_rename_stats_match_redis() {
    let Some((_guard, mut node)) = infinityd(2, Path::new(env!("CARGO_TARGET_TMPDIR"))) else {
        eprintln!("SKIPPED: INFINITYD_BIN unset — H3 INFO counters not run");
        return;
    };
    let Some((_oracle_guard, mut redis)) = oracle() else {
        eprintln!("SKIPPED: redis-server not installed — H3 INFO counters not run");
        return;
    };
    let peer = other_cell_connection(&mut node);
    for node in &mut [node, peer] {
        let owner = connection_cell(node);
        let source = key(2, owner, "stats-source");
        let target = key(2, 1 - owner, "stats-target");
        for command in [b"RENAME".as_slice(), b"RENAMENX", b"COPY"] {
            for existing in [false, true] {
                let mut deltas = Vec::new();
                for node in [&mut redis, &mut *node] {
                    let _ = call(node, &[b"DEL", &source, &target]);
                    assert_eq!(call(node, &[b"SET", &source, b"v"]), b"+OK\r\n");
                    if existing {
                        assert_eq!(call(node, &[b"SET", &target, b"old"]), b"+OK\r\n");
                    }
                    let before = hit_miss_counters(node);
                    let reply = call(node, &[command, &source, &target]);
                    assert!(!reply.starts_with(b"-"), "{reply:?}");
                    let after = hit_miss_counters(node);
                    deltas.push((after.0 - before.0, after.1 - before.1));
                }
                assert_eq!(deltas[1], deltas[0], "H3 follow-up: binary INFO counter drift");
            }
        }
    }
    println!("H3 binary INFO counters: 12 rows, 0 differences from Redis");
}

/// INFO stats are cell-local, so each counter read must name the source owner.
fn other_cell_connection(node: &mut TcpStream) -> TcpStream {
    let first = connection_cell(node);
    for _ in 0..64 {
        let mut peer =
            TcpStream::connect(node.peer_addr().expect("node address")).expect("connect");
        peer.set_read_timeout(Some(std::time::Duration::from_secs(5))).expect("timeout");
        if connection_cell(&mut peer) != first {
            return peer;
        }
    }
    panic!("both cells must be observed before comparing their counters");
}

fn connection_cell(node: &mut TcpStream) -> u16 {
    let info = call(node, &[b"INFO", b"server"]);
    std::str::from_utf8(&info)
        .expect("INFO text")
        .lines()
        .find_map(|line| line.strip_prefix("cell:"))
        .expect("connection cell")
        .parse()
        .expect("u16")
}

fn hit_miss_counters(node: &mut TcpStream) -> (u64, u64) {
    let info = call(node, &[b"INFO", b"stats"]);
    let text = std::str::from_utf8(&info).expect("INFO text");
    let field = |name: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(name))
            .expect("counter")
            .parse()
            .expect("u64")
    };
    (field("keyspace_hits:"), field("keyspace_misses:"))
}

fn call(stream: &mut TcpStream, argv: &[&[u8]]) -> Vec<u8> {
    let mut wire = format!("*{}\r\n", argv.len()).into_bytes();
    for arg in argv {
        wire.extend_from_slice(format!("${}\r\n", arg.len()).as_bytes());
        wire.extend_from_slice(arg);
        wire.extend_from_slice(b"\r\n");
    }
    stream.write_all(&wire).expect("write command");
    read_frames(stream, &mut Vec::new(), 1)
}

fn key(cells: u16, owner: u16, prefix: &str) -> Vec<u8> {
    let router = SlotRouter::new_contiguous(cells);
    (0..10000)
        .map(|n| format!("{prefix}:{n}").into_bytes())
        .find(|k| router.cell_of(SlotRouter::slot_of(k)) == CellId(owner))
        .expect("owner has a key")
}

fn bulk(value: &[u8]) -> Vec<u8> {
    let mut reply = format!("${}\r\n", value.len()).into_bytes();
    reply.extend_from_slice(value);
    reply.extend_from_slice(b"\r\n");
    reply
}

fn check(label: &str, actual: Vec<u8>, expected: &[u8], failures: &mut Vec<String>) {
    if actual != expected {
        failures.push(format!(
            "{label}: got {:?}, expected {:?}",
            String::from_utf8_lossy(&actual),
            String::from_utf8_lossy(expected)
        ));
    }
}

/// Destination pressure, Redis's way (ADR-0110 third amendment, batch 17):
/// `RENAME`/`RENAMENX` land under `maxmemory` — neither is DENYOOM — and
/// `COPY` refuses; every outcome keeps both values and both absolute
/// deadlines exact. Pre-fix the destination leg was a client-shaped `SET`,
/// so all 24 rename rows answered `-OOM` and moved nothing.
#[test]
fn cross_cell_move_pressure_matrix() {
    let mut failures = Vec::new();
    let mut rows = 0;
    for cells in [2, 4] {
        let Some((_guard, mut node)) = infinityd(cells, Path::new(env!("CARGO_TARGET_TMPDIR")))
        else {
            eprintln!("SKIPPED: INFINITYD_BIN unset — H3 binary refusal matrix not run");
            return;
        };
        let source = key(cells, 0, "source");
        let target = key(cells, cells - 1, "target");
        for db in [b"0".as_slice(), b"3"] {
            assert_eq!(call(&mut node, &[b"SELECT", db]), b"+OK\r\n");
            for protocol in [b"2".as_slice(), b"3"] {
                let _ = call(&mut node, &[b"HELLO", protocol]);
                for command in [b"RENAME".as_slice(), b"RENAMENX", b"COPY"] {
                    for existing in [false, true] {
                        let label = format!(
                            "H3 cells={cells} db={} resp={} cmd={} existing={existing}",
                            String::from_utf8_lossy(db),
                            String::from_utf8_lossy(protocol),
                            String::from_utf8_lossy(command)
                        );
                        pressure_row(
                            &mut node,
                            &source,
                            &target,
                            protocol,
                            command,
                            existing,
                            &label,
                            &mut failures,
                        );
                        rows += 1;
                    }
                }
            }
        }
    }
    println!("H3 binary pressure matrix: {rows} rows, {} failures", failures.len());
    for failure in &failures {
        println!("{failure}");
    }
    assert!(failures.is_empty(), "H3: a move under destination pressure diverged from Redis");
}

/// One pressure row: the reply Redis gives, then both values and both
/// absolute deadlines — moved exactly for a landed rename, untouched for a
/// refused `COPY` or a `RENAMENX` no-op.
#[allow(clippy::too_many_arguments)] // Explicit matrix dimensions, test-only.
fn pressure_row(
    node: &mut TcpStream,
    source: &[u8],
    target: &[u8],
    protocol: &[u8],
    command: &[u8],
    existing: bool,
    label: &str,
    failures: &mut Vec<String>,
) {
    assert_eq!(call(node, &[b"CONFIG", b"SET", b"maxmemory", b"0"]), b"+OK\r\n");
    let _ = call(node, &[b"DEL", source, target]);
    assert_eq!(call(node, &[b"SET", source, b"IMPORTANT-PAYLOAD", b"PX", b"600000"]), b"+OK\r\n");
    if existing {
        assert_eq!(call(node, &[b"SET", target, b"previous", b"PX", b"900000"]), b"+OK\r\n");
    }
    let source_deadline = call(node, &[b"PEXPIRETIME", source]);
    let target_deadline = call(node, &[b"PEXPIRETIME", target]);
    assert_eq!(
        call(node, &[b"CONFIG", b"SET", b"maxmemory-policy", b"noeviction", b"maxmemory", b"1"]),
        b"+OK\r\n"
    );
    let reply = call(node, &[command, source, target]);
    let nil: &[u8] = if protocol == b"3" { b"_\r\n" } else { b"$-1\r\n" };
    // Redis 8.0.5 under `maxmemory 1` / `noeviction` (oracle-pinned):
    // RENAME `+OK`, RENAMENX `:1` / `:0` on an existing target, COPY `-OOM`.
    let lands = match (command, existing) {
        (b"COPY", _) => {
            if !reply.starts_with(b"-OOM ") {
                failures.push(format!("{label}: COPY must keep DENYOOM, got {reply:?}"));
            }
            false
        }
        (b"RENAMENX", true) => {
            check(&format!("{label} reply"), reply, b":0\r\n", failures);
            false
        }
        (b"RENAME", _) => {
            check(&format!("{label} reply"), reply, b"+OK\r\n", failures);
            true
        }
        _ => {
            check(&format!("{label} reply"), reply, b":1\r\n", failures);
            true
        }
    };
    let payload = bulk(b"IMPORTANT-PAYLOAD");
    let old = bulk(b"previous");
    let (source_value, target_value, source_after, target_after): (&[u8], &[u8], &[u8], &[u8]) =
        if lands {
            (nil, &payload, b":-2\r\n", &source_deadline)
        } else {
            (&payload, if existing { &old } else { nil }, &source_deadline, &target_deadline)
        };
    check(&format!("{label} source"), call(node, &[b"GET", source]), source_value, failures);
    check(&format!("{label} target"), call(node, &[b"GET", target]), target_value, failures);
    check(
        &format!("{label} source deadline"),
        call(node, &[b"PEXPIRETIME", source]),
        source_after,
        failures,
    );
    check(
        &format!("{label} target deadline"),
        call(node, &[b"PEXPIRETIME", target]),
        target_after,
        failures,
    );
}

/// Boundary-size binary payloads and deadlines survive either move direction.
#[test]
fn cross_cell_move_success_and_copy_variants() {
    let Some((_guard, mut node)) = infinityd(4, Path::new(env!("CARGO_TARGET_TMPDIR"))) else {
        eprintln!("SKIPPED: INFINITYD_BIN unset — H3 binary success variants not run");
        return;
    };
    let value: Vec<u8> = (0..65536).map(|n| (n % 256) as u8).collect();
    let mut rows = 0;
    for owner in [0, 3] {
        let source = key(4, owner, &"s".repeat(250));
        let target = key(4, 3 - owner, &"t".repeat(250));
        assert!(source.len() <= 255);
        for command in [b"RENAME".as_slice(), b"RENAMENX", b"COPY"] {
            for expiry in [false, true] {
                let _ = call(&mut node, &[b"DEL", &source, &target]);
                let mut set: Vec<&[u8]> = vec![b"SET", &source, &value];
                if expiry {
                    set.extend_from_slice(&[b"PX", b"600000"]);
                }
                assert_eq!(call(&mut node, &set), b"+OK\r\n");
                let deadline = call(&mut node, &[b"PEXPIRETIME", &source]);
                assert_eq!(
                    call(&mut node, &[command, &source, &target]),
                    if command == b"RENAME" { &b"+OK\r\n"[..] } else { b":1\r\n" }
                );
                assert_eq!(call(&mut node, &[b"GET", &target]), bulk(&value));
                assert_eq!(call(&mut node, &[b"PEXPIRETIME", &target]), deadline);
                assert_eq!(
                    call(&mut node, &[b"EXISTS", &source]),
                    if command == b"COPY" { &b":1\r\n"[..] } else { b":0\r\n" }
                );
                rows += 1;
            }
        }
    }
    copy_and_overwrite_variants(&mut node);
    println!(
        "H3 binary success matrix: {rows} large-binary rows plus COPY DB/REPLACE and overwrite \
             controls, 0 failures"
    );
}

/// COPY DB/REPLACE and RENAME overwrite preserve their existing semantics.
fn copy_and_overwrite_variants(node: &mut TcpStream) {
    let source = key(4, 0, "copy-source");
    let target = key(4, 3, "copy-target");
    assert_eq!(call(node, &[b"SET", &source, b"source"]), b"+OK\r\n");
    assert_eq!(call(node, &[b"COPY", &source, &target, b"DB", b"3"]), b":1\r\n");
    assert_eq!(call(node, &[b"SELECT", b"3"]), b"+OK\r\n");
    assert_eq!(call(node, &[b"GET", &target]), bulk(b"source"));
    assert_eq!(call(node, &[b"SET", &target, b"old"]), b"+OK\r\n");
    assert_eq!(call(node, &[b"SELECT", b"0"]), b"+OK\r\n");
    assert_eq!(call(node, &[b"COPY", &source, &target, b"DB", b"3"]), b":0\r\n");
    assert_eq!(call(node, &[b"COPY", &source, &target, b"DB", b"3", b"REPLACE"]), b":1\r\n");
    assert_eq!(call(node, &[b"SELECT", b"3"]), b"+OK\r\n");
    assert_eq!(call(node, &[b"GET", &target]), bulk(b"source"));
    assert_eq!(call(node, &[b"SELECT", b"0"]), b"+OK\r\n");
    assert_eq!(call(node, &[b"SET", &target, b"old"]), b"+OK\r\n");
    assert_eq!(call(node, &[b"RENAMENX", &source, &target]), b":0\r\n");
    assert_eq!(call(node, &[b"GET", &target]), bulk(b"old"));
    assert_eq!(call(node, &[b"RENAME", &source, &target]), b"+OK\r\n");
    assert_eq!(call(node, &[b"GET", &target]), bulk(b"source"));
}

/// A bounds refusal is reachable without memory pressure and must preserve data.
#[test]
fn cross_cell_move_bounds_and_missing_source() {
    let Some((_guard, mut node)) = infinityd(4, Path::new(env!("CARGO_TARGET_TMPDIR"))) else {
        eprintln!("SKIPPED: INFINITYD_BIN unset — H3 binary bounds matrix not run");
        return;
    };
    let source = key(4, 0, "bounds-source");
    let target = key(4, 3, &"t".repeat(256));
    let mut failures = Vec::new();
    for command in [b"RENAME".as_slice(), b"RENAMENX", b"COPY"] {
        assert_eq!(call(&mut node, &[b"SET", &source, b"keep"]), b"+OK\r\n");
        let reply = call(&mut node, &[command, &source, &target]);
        assert!(reply.starts_with(b"-ERR key or value exceeds"));
        check(
            "H3 bounds source",
            call(&mut node, &[b"GET", &source]),
            &bulk(b"keep"),
            &mut failures,
        );
    }
    let target = key(4, 3, "exists-target");
    let _ = call(&mut node, &[b"DEL", &source]);
    assert_eq!(call(&mut node, &[b"SET", &target, b"keep"]), b"+OK\r\n");
    for command in [b"RENAME".as_slice(), b"RENAMENX"] {
        check(
            "H3 missing source",
            call(&mut node, &[command, &source, &target]),
            b"-ERR no such key\r\n",
            &mut failures,
        );
    }
    println!("H3 binary bounds/missing matrix: 5 rows, {} failures", failures.len());
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// A document cannot silently become string bytes through SET at another cell.
#[test]
fn cross_cell_move_document_refuses_without_mutation() {
    let Some((_guard, mut node)) = infinityd(4, Path::new(env!("CARGO_TARGET_TMPDIR"))) else {
        eprintln!("SKIPPED: INFINITYD_BIN unset — H3 document refusal not run");
        return;
    };
    let source = key(4, 0, "doc-source");
    let target = key(4, 3, "doc-target");
    for command in [b"RENAME".as_slice(), b"RENAMENX", b"COPY"] {
        let _ = call(&mut node, &[b"DEL", &source, &target]);
        assert_eq!(call(&mut node, &[b"JSON.SET", &source, b"$", br#"{"x":1}"#]), b"+OK\r\n");
        let before = call(&mut node, &[b"JSON.GET", &source]);
        assert_eq!(
            call(&mut node, &[command, &source, &target]),
            b"-WRONGTYPE Operation against a key holding the wrong kind of value\r\n"
        );
        assert_eq!(call(&mut node, &[b"JSON.GET", &source]), before);
        assert_eq!(call(&mut node, &[b"EXISTS", &target]), b":0\r\n");
    }
}

/// Named memory/durable/tiered namespaces retain the existing spanning-key refusal.
#[test]
fn cross_cell_move_namespace_refusals_preserve_source() {
    let Some((_guard, mut node)) = infinityd(4, Path::new(env!("CARGO_TARGET_TMPDIR"))) else {
        eprintln!("SKIPPED: INFINITYD_BIN unset — H3 namespace refusal not run");
        return;
    };
    let source = key(4, 0, "ns-source");
    let target = key(4, 3, "ns-target");
    let specs: &[&[&[u8]]] = &[
        &[b"INF.NS", b"CREATE", b"move-memory", b"MODE", b"memory"],
        &[b"INF.NS", b"CREATE", b"move-durable", b"MODE", b"durable", b"FSYNC", b"always"],
        &[
            b"INF.NS",
            b"CREATE",
            b"move-tiered",
            b"MODE",
            b"durable",
            b"FSYNC",
            b"everysec",
            b"MEM-BUDGET",
            b"64mb",
        ],
    ];
    for spec in specs {
        assert_eq!(call(&mut node, spec), b"+OK\r\n");
        assert_eq!(call(&mut node, &[b"INF.NS", b"USE", spec[2]]), b"+OK\r\n");
        assert_eq!(call(&mut node, &[b"SET", &source, b"keep"]), b"+OK\r\n");
        for command in [b"RENAME".as_slice(), b"RENAMENX"] {
            assert!(call(&mut node, &[command, &source, &target]).starts_with(b"-ERR "));
            assert_eq!(call(&mut node, &[b"GET", &source]), bulk(b"keep"));
            assert_eq!(call(&mut node, &[b"EXISTS", &target]), b":0\r\n");
        }
        assert_eq!(call(&mut node, &[b"SELECT", b"0"]), b"+OK\r\n");
    }
}
