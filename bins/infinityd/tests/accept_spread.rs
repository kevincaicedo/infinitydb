//! Lane L11 N19 (batch 70): every cell of a multi-cell node is reachable
//! from a client on every tier. On XNU `SO_REUSEPORT` does not spread a
//! listener group — one listener takes every connection — so the
//! accepting cell hands accepted sockets round-robin across the fabric
//! (ADR-0128). Pre-fix on macOS: 32 connections, `cell:0` × 32.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)] // harness process, not cell code

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const CELLS: u16 = 2;
const CONNS: usize = 32;

struct Server {
    child: Child,
    port: u16,
    stderr: PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn(tag: &str, extra: &[&str]) -> Server {
    let dir = std::env::temp_dir().join(format!("inf-accept-spread-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("data dir");
    let stderr = dir.join("stderr");
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("probe bind")
        .local_addr()
        .expect("addr")
        .port()
        .to_string();
    // `INF_ACCEPT_SPREAD_BIN` points the suite at another build — the
    // review's pre-fix red (batch 70) ran it against the batch-69 binary.
    let bin = std::env::var("INF_ACCEPT_SPREAD_BIN")
        .unwrap_or_else(|_| env!("CARGO_BIN_EXE_infinityd").to_string());
    let child = Command::new(bin)
        .args(["--port", &port, "--cells", &CELLS.to_string(), "--data-dir"])
        .arg(&dir)
        .args(["--device-probe", "off"])
        .args(extra)
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
        if let Some(port) = server
            .text()
            .lines()
            .find_map(|line| line.strip_prefix("infinityd: listening on "))
            .and_then(|port| port.trim().parse().ok())
            && TcpStream::connect(("127.0.0.1", port)).is_ok()
        {
            server.port = port;
            return server;
        }
        assert!(Instant::now() < deadline, "infinityd never came up: {}", server.text());
        std::thread::sleep(Duration::from_millis(20));
    }
}

impl Server {
    fn text(&self) -> String {
        std::fs::read_to_string(&self.stderr).unwrap_or_default()
    }
}

/// `INFO server` → `cell:` over a fresh connection, held open so the
/// node's connection count keeps growing (every socket a distinct fd).
fn cell_of(port: u16, keep: &mut Vec<TcpStream>) -> u16 {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut c = loop {
        match TcpStream::connect(("127.0.0.1", port)) {
            Ok(c) => break c,
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => panic!("connect: {e:?}"),
        }
    };
    c.set_read_timeout(Some(Duration::from_secs(10))).expect("timeout");
    c.write_all(b"*2\r\n$4\r\nINFO\r\n$6\r\nserver\r\n").expect("write");
    let mut head = [0u8; 4096];
    let n = c.read(&mut head).expect("read");
    let text = String::from_utf8_lossy(&head[..n]).into_owned();
    let cell = text
        .lines()
        .find_map(|l| l.strip_prefix("cell:"))
        .unwrap_or_else(|| panic!("no cell field in {text}"))
        .trim()
        .parse()
        .expect("cell id");
    keep.push(c);
    cell
}

fn spread(port: u16) -> BTreeMap<u16, usize> {
    let mut keep = Vec::new();
    let mut seen = BTreeMap::new();
    for _ in 0..CONNS {
        *seen.entry(cell_of(port, &mut keep)).or_insert(0) += 1;
    }
    seen
}

/// The hand-off asked for explicitly: every cell answers, and no cell
/// takes more than its round-robin share plus the kernel's own spread.
#[test]
fn handoff_on_reaches_every_cell() {
    let server = spawn("on", &["--accept-handoff", "on"]);
    let seen = spread(server.port);
    assert_eq!(seen.len(), usize::from(CELLS), "not every cell answered: {seen:?}");
    let max = seen.values().copied().max().unwrap_or(0);
    assert!(max <= CONNS * 3 / 4, "one cell took {max} of {CONNS}: {seen:?}");
}

/// The platform default: on macOS the hand-off is on, so every cell is
/// reachable without a flag (the pre-fix tier answered `cell:0` × 32).
#[cfg(target_os = "macos")]
#[test]
fn default_reaches_every_cell_on_macos() {
    let server = spawn("default", &[]);
    let seen = spread(server.port);
    assert_eq!(seen.len(), usize::from(CELLS), "not every cell answered: {seen:?}");
}

/// `off` keeps the kernel's own spread — on XNU that is one cell, which
/// this test only records (the flag is the operator's choice).
#[test]
fn handoff_off_still_serves() {
    let server = spawn("off", &["--accept-handoff", "off"]);
    let seen = spread(server.port);
    assert_eq!(seen.values().sum::<usize>(), CONNS, "{seen:?}");
}
