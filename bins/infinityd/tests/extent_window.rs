//! The binary arm of the extent-seal window (ADR-0061 D3; ADR-0163 D2): a
//! blob `SET` holds its extent file open until the extent's fdatasync is
//! folded into the fsync ledger, and the window bounds those barriers at
//! 16 per cell, so a burst holds at most 16 extent descriptors where it
//! held one per write. A two-cell node takes 64 blob `SET`s of one hash tag
//! (one owning cell), sent on 64 connections to that cell while the process
//! is stopped, so they arrive together, none behind a fabric hop; the
//! process may open 24 more descriptors than it holds once the connections
//! are up (the window's 16 and a margin of 8 for the files the node opens
//! beside them, at any cell count). Every reply is `+OK`, and `INFO caps`
//! reads the `extent-window` row at its cap on exactly one cell, whose
//! `extent_parks` counts the 48 writers the full window parked. Linux: the
//! descriptor limit is set with `prlimit` and the open descriptors are
//! read from `/proc`.
#![cfg(target_os = "linux")]
#![allow(clippy::disallowed_methods, clippy::disallowed_types)] // harness process, not cell code

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const CELLS: u16 = 2;
const WRITERS: usize = 64;
/// A blob: at the namespace's 4 KiB `BLOB-THRESHOLD`.
const BLOB_LEN: usize = 4 << 10;
/// The window's cap: unfolded extent barriers per cell.
const WINDOW: u64 = 16;
/// Descriptors the node may open past those it holds once every
/// connection is up: the window's 16 and a margin of 8.
const DESCRIPTOR_HEADROOM: usize = 24;

struct Server {
    child: Child,
    port: u16,
    stderr: PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        // A stopped process must run again before it can die of its kill.
        let _ = signal(self.child.id(), "-CONT");
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Server {
    /// Two cells; every cell reachable through the accept hand-off, so a
    /// fresh connection lands on either.
    fn spawn(dir: &Path) -> Server {
        let stderr = dir.join("stderr");
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .expect("probe bind")
            .local_addr()
            .expect("addr")
            .port()
            .to_string();
        let child = Command::new(env!("CARGO_BIN_EXE_infinityd"))
            .args(["--port", &port, "--cells", &CELLS.to_string()])
            .args(["--accept-handoff", "on", "--device-probe", "off", "--data-dir"])
            .arg(dir)
            .stdout(Stdio::null())
            .stderr(Stdio::from(std::fs::File::create(&stderr).expect("stderr file")))
            .spawn()
            .expect("spawn infinityd");
        let mut server = Server { child, port: 0, stderr };
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if let Some(status) = server.child.try_wait().expect("try_wait") {
                panic!("infinityd exited during boot: {status} — {}", server.text());
            }
            let text = server.text();
            if let Some(port) = text
                .lines()
                .find_map(|line| line.strip_prefix("infinityd: listening on "))
                .and_then(|port| port.trim().parse().ok())
                && text.contains("control: recovery complete")
                && TcpStream::connect(("127.0.0.1", port)).is_ok()
            {
                server.port = port;
                return server;
            }
            assert!(Instant::now() < deadline, "infinityd never came up: {text}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn text(&self) -> String {
        std::fs::read_to_string(&self.stderr).unwrap_or_default()
    }

    /// Descriptors the process holds now.
    fn open_descriptors(&self) -> usize {
        std::fs::read_dir(format!("/proc/{}/fd", self.child.id()))
            .expect("the node's descriptor table")
            .count()
    }
}

/// Sends `signal` (`-STOP`, `-CONT`) to `pid` with `kill(1)`.
fn signal(pid: u32, signal: &str) -> bool {
    Command::new("kill")
        .args([signal, &pid.to_string()])
        .status()
        .is_ok_and(|status| status.success())
}

struct Client(TcpStream);

impl Client {
    fn connect(port: u16) -> Client {
        let c = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        c.set_read_timeout(Some(Duration::from_secs(30))).expect("timeout");
        c.set_nodelay(true).expect("nodelay");
        Client(c)
    }

    fn wire(parts: &[&[u8]]) -> Vec<u8> {
        let mut wire = format!("*{}\r\n", parts.len()).into_bytes();
        for p in parts {
            wire.extend_from_slice(format!("${}\r\n", p.len()).as_bytes());
            wire.extend_from_slice(p);
            wire.extend_from_slice(b"\r\n");
        }
        wire
    }

    fn call(&mut self, parts: &[&[u8]]) -> Vec<u8> {
        self.0.write_all(&Client::wire(parts)).expect("write");
        self.reply()
    }

    fn reply(&mut self) -> Vec<u8> {
        let mut reply = self.read_line();
        if reply[0] == b'$' && !reply.starts_with(b"$-1") {
            let len: usize = std::str::from_utf8(&reply[1..reply.len() - 2])
                .expect("utf8")
                .parse()
                .expect("len");
            let mut payload = vec![0u8; len + 2];
            self.0.read_exact(&mut payload).expect("payload");
            reply.extend_from_slice(&payload);
        }
        reply
    }

    fn read_line(&mut self) -> Vec<u8> {
        let mut line = Vec::new();
        let mut byte = [0u8; 1];
        while !line.ends_with(b"\r\n") {
            self.0.read_exact(&mut byte).expect("read");
            line.push(byte[0]);
        }
        line
    }

    fn ok(&mut self, parts: &[&[u8]]) {
        let reply = self.call(parts);
        assert_eq!(reply, b"+OK\r\n", "{}", String::from_utf8_lossy(&reply));
    }

    fn info(&mut self, section: &str) -> String {
        String::from_utf8(self.call(&[b"INFO", section.as_bytes()])).expect("INFO is text")
    }
}

/// `field` of an `INFO` text, as a number.
fn field(info: &str, name: &str) -> u64 {
    info.lines()
        .find_map(|line| line.strip_prefix(name).and_then(|rest| rest.strip_prefix(':')))
        .unwrap_or_else(|| panic!("no {name} in {info}"))
        .trim()
        .parse()
        .expect("a number")
}

/// One connection per cell, keyed by the cell it landed on.
fn connection_per_cell(port: u16) -> BTreeMap<u64, Client> {
    let mut cells = BTreeMap::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    while cells.len() < usize::from(CELLS) {
        let mut client = Client::connect(port);
        let cell = field(&client.info("server"), "cell");
        cells.entry(cell).or_insert(client);
        assert!(Instant::now() < deadline, "only cells {:?} answered", cells.keys());
    }
    cells
}

/// The cell that owns the `{ew}` hash tag: the one that created the extent
/// of a probe blob `SET`.
fn owning_cell(setup: &mut Client, cells: &mut BTreeMap<u64, Client>) -> u64 {
    setup.ok(&[b"INF.NS", b"USE", b"ew"]);
    setup.ok(&[b"SET", b"{ew}probe", &vec![b'p'; BLOB_LEN]]);
    let owners: Vec<u64> = cells
        .iter_mut()
        .filter_map(|(cell, conn)| {
            let created = field(&conn.info("tiering"), "tiering_blob_extents_created");
            (created > 0).then_some(*cell)
        })
        .collect();
    assert_eq!(owners.len(), 1, "one cell owns the hash tag: {owners:?}");
    owners[0]
}

/// The `extent-window` row of an `INFO caps` text: `(high_water, crossings)`.
fn window_row(caps: &str) -> (u64, u64) {
    let row = caps
        .lines()
        .find_map(|line| line.strip_prefix("cap_row_extent-window:"))
        .unwrap_or_else(|| panic!("no extent-window row in {caps}"));
    let number = |key: &str| -> u64 {
        row.split(',')
            .find_map(|kv| kv.strip_prefix(key).and_then(|v| v.strip_prefix('=')))
            .unwrap_or_else(|| panic!("no {key} in {row}"))
            .parse()
            .expect("a number")
    };
    (number("high_water"), number("crossings"))
}

#[test]
fn a_burst_of_blob_writes_holds_at_most_the_window_of_extent_descriptors() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/e2e-extent-window")
        .join(std::process::id().to_string());
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("data dir");
    let server = Server::spawn(&dir);
    let mut setup = Client::connect(server.port);
    setup.ok(&[
        b"INF.NS",
        b"CREATE",
        b"ew",
        b"MODE",
        b"durable",
        b"FSYNC",
        b"always",
        b"MEM-BUDGET",
        b"3mb",
        b"MAINTAIN-SLICE",
        b"1mb",
        b"BLOB-THRESHOLD",
        b"4kb",
    ]);
    let mut cells = connection_per_cell(server.port);
    let owner = owning_cell(&mut setup, &mut cells);
    // Every writer on the owning cell: its 64 writes are read at one
    // iteration, none behind a fabric hop. A connection the hand-off put on
    // the other cell stays open and idle.
    let mut writers = Vec::new();
    let mut idle = Vec::new();
    while writers.len() < WRITERS {
        let mut client = Client::connect(server.port);
        if field(&client.info("server"), "cell") == owner {
            client.ok(&[b"INF.NS", b"USE", b"ew"]);
            writers.push(client);
        } else {
            idle.push(client);
        }
        assert!(idle.len() < 4 * WRITERS, "the hand-off never reached cell {owner}");
    }
    let limit = server.open_descriptors() + DESCRIPTOR_HEADROOM;
    let pid = server.child.id().to_string();
    let prlimit = Command::new("prlimit")
        .args(["--pid", &pid, &format!("--nofile={limit}:")])
        .status()
        .expect("prlimit");
    assert!(prlimit.success(), "prlimit --nofile={limit} refused");
    // Stopped, the node reads nothing: the 64 writes wait in the socket
    // buffers and arrive at one reactor iteration once it runs again.
    assert!(signal(server.child.id(), "-STOP"), "SIGSTOP");
    let blob = vec![b'b'; BLOB_LEN];
    for (i, writer) in writers.iter_mut().enumerate() {
        let key = format!("{{ew}}k{i}");
        writer.0.write_all(&Client::wire(&[b"SET", key.as_bytes(), &blob])).expect("write");
    }
    assert!(signal(server.child.id(), "-CONT"), "SIGCONT");
    for (i, writer) in writers.iter_mut().enumerate() {
        let reply = writer.reply();
        assert_eq!(reply, b"+OK\r\n", "writer {i}: {}", String::from_utf8_lossy(&reply));
    }
    let mut full = Vec::new();
    for (cell, conn) in &mut cells {
        let caps = conn.info("caps");
        assert_eq!(field(&caps, "cap_assembly_violations"), 0, "cell {cell}: {caps}");
        let (high_water, _) = window_row(&caps);
        assert!(high_water <= WINDOW, "cell {cell}: {high_water} extent barriers: {caps}");
        if high_water == WINDOW {
            full.push((*cell, field(&conn.info("persistence"), "extent_parks")));
        }
    }
    assert_eq!(full.len(), 1, "the owning cell alone fills its window: {full:?}");
    let parks = full[0].1;
    assert!(parks >= (WRITERS as u64) - WINDOW, "{parks} writers parked: {full:?}");
    drop(server);
    let _ = std::fs::remove_dir_all(&dir);
}
