//! ADR-0115 binary tier: the fabric-program primitives (`INF.TAKE`,
//! `INF.PEEK`, `INF.PUT`) are internal commands — a client typing one
//! gets exactly what redis-server answers for a command it does not
//! know, on the plain path and under `maxmemory`; `COMMAND` hides them;
//! and the cross-cell `RENAME` whose legs they are still lands.
//! Pre-fix (`ccc265c`): `INF.PUT k v -1` answered `+OK` and the key
//! existed, also at `maxmemory 1` where `SET` answers `-OOM`.

use std::io::Write;
use std::net::TcpStream;
use std::path::Path;

use compat::harness::{infinityd, oracle, read_frames};
use inf_foundation::CellId;
use inf_store::SlotRouter;

#[test]
fn internal_commands_are_unknown_to_clients_at_the_binary() {
    let Some((_guard, mut node)) = infinityd(2, Path::new(env!("CARGO_TARGET_TMPDIR"))) else {
        eprintln!("SKIPPED: INFINITYD_BIN unset — internal-command lane not run");
        return;
    };
    let (_oracle_guard, mut redis) = oracle();
    let mut failures = Vec::new();
    let mut compare = |label: &str, argv: &[&[u8]], node: &mut TcpStream, redis: &mut TcpStream| {
        let want = call(redis, argv);
        let got = call(node, argv);
        if got != want {
            failures.push(format!(
                "{label}: node {:?}, redis {:?}",
                String::from_utf8_lossy(&got),
                String::from_utf8_lossy(&want)
            ));
        }
    };
    let local = key(2, connection_cell(&mut node), "internal");
    let remote = key(2, 1 - connection_cell(&mut node), "internal");
    for key in [&local, &remote] {
        // The plain path: every internal row, every spelling, before arity.
        compare("INF.PUT", &[b"INF.PUT", key, b"v", b"-1"], &mut node, &mut redis);
        compare("inf.put", &[b"inf.put", key, b"v", b"-1"], &mut node, &mut redis);
        compare("INF.PUT arity", &[b"INF.PUT"], &mut node, &mut redis);
        compare("INF.TAKE", &[b"INF.TAKE", key], &mut node, &mut redis);
        compare("INF.TAKE IF", &[b"INF.TAKE", key, b"IF", b"v", b"-1"], &mut node, &mut redis);
        compare("INF.PEEK", &[b"INF.PEEK", key, b"ABS", b"NOSTATS"], &mut node, &mut redis);
        compare("nothing landed", &[b"GET", key], &mut node, &mut redis);
    }
    compare(
        "COMMAND INFO",
        &[b"COMMAND", b"INFO", b"INF.PUT", b"INF.TAKE", b"INF.PEEK"],
        &mut node,
        &mut redis,
    );
    compare(
        "COMMAND GETKEYS",
        &[b"COMMAND", b"GETKEYS", b"INF.PUT", b"k", b"v", b"-1"],
        &mut node,
        &mut redis,
    );
    // Under memory pressure: SET refuses on both; the put stays unknown.
    for server in [&mut redis, &mut node] {
        assert_eq!(call(server, &[b"CONFIG", b"SET", b"maxmemory", b"1"]), b"+OK\r\n");
        assert_eq!(
            call(server, &[b"CONFIG", b"SET", b"maxmemory-policy", b"noeviction"]),
            b"+OK\r\n"
        );
    }
    compare("SET under maxmemory", &[b"SET", &local, b"v"], &mut node, &mut redis);
    compare("INF.PUT under maxmemory", &[b"INF.PUT", &local, b"v", b"-1"], &mut node, &mut redis);
    compare("nothing landed under maxmemory", &[b"GET", &local], &mut node, &mut redis);
    for server in [&mut redis, &mut node] {
        assert_eq!(call(server, &[b"CONFIG", b"SET", b"maxmemory", b"0"]), b"+OK\r\n");
    }
    let mut expect = |label: &str, got: Vec<u8>, want: &[u8]| {
        if got != want {
            failures.push(format!(
                "{label}: got {:?}, expected {:?}",
                String::from_utf8_lossy(&got),
                String::from_utf8_lossy(want)
            ));
        }
    };
    // The program still runs its legs: a cross-cell RENAME lands.
    expect("seed", call(&mut node, &[b"SET", &local, b"payroll", b"PX", b"600000"]), b"+OK\r\n");
    expect("RENAME across cells", call(&mut node, &[b"RENAME", &local, &remote]), b"+OK\r\n");
    expect("moved value", call(&mut node, &[b"GET", &remote]), b"$7\r\npayroll\r\n");
    expect("source gone", call(&mut node, &[b"EXISTS", &local]), b":0\r\n");
    let ttl = call(&mut node, &[b"PTTL", &remote]);
    if !(ttl.starts_with(b":5") || ttl.starts_with(b":6")) {
        failures.push(format!("the deadline travelled: {ttl:?}"));
    }
    // COMMAND and COMMAND COUNT agree with each other and hide the rows.
    let listed = call(&mut node, &[b"COMMAND"]);
    let count = call(&mut node, &[b"COMMAND", b"COUNT"]);
    let n: usize =
        std::str::from_utf8(&count[1..count.len() - 2]).expect("int").parse().expect("count");
    let rows = listed.windows(3).filter(|w| w == b"*10").count();
    if rows != n {
        failures.push(format!("COMMAND lists {rows} rows, COMMAND COUNT says {n}"));
    }
    for name in [&b"inf.put"[..], b"inf.take", b"inf.peek"] {
        if listed.windows(name.len()).any(|w| w == name) {
            failures.push(format!("COMMAND lists {}", String::from_utf8_lossy(name)));
        }
    }
    assert!(
        failures.is_empty(),
        "ADR-0115 binary lane: {} failures\n{}",
        failures.len(),
        failures.join("\n")
    );
    println!(
        "ADR-0115 binary lane: internal commands unknown to clients, 0 differences from Redis"
    );
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
