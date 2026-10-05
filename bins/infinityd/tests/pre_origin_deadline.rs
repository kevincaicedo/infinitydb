#![allow(clippy::disallowed_methods, reason = "test target: process deadlines, not cell code")]
#![cfg(any(target_os = "linux", target_os = "macos"))]
//! A deadline before the internal clock's origin is expired at every reading
//! of that clock, the origin's own millisecond included (ADR-0111). The node
//! binds its listener before each cell's clock starts, so a pipeline sent at
//! connect waits in the backlog and runs in the cells' first iterations,
//! inside internal millisecond 0, where a deadline clamped onto the origin
//! read as live. Each boot pipelines `SET k v EXAT 1` / `GET k` at connect,
//! and Redis answers nil. Nil is the answer at every timing, so the test
//! cannot go red on a correct binary; a regression goes red only when the
//! pipeline lands inside that millisecond, so the test boots several times.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Boots per run. A release binary that clamped onto the origin served the
/// key on 19 of 40 boots, so eight boots miss that regression with
/// probability about 0.53⁸ ≈ 0.6 %. A debug binary reaches its first
/// command after the millisecond has passed (0 of 40 boots served): run
/// against a debug build, the test checks the answer and cannot see the
/// regression; `cargo test --release` or `INF_PRE_ORIGIN_BIN` runs it as a
/// red-capable check.
const BOOTS: usize = 8;
const CELLS: &str = "4";
const PIPELINE: &[u8] = b"*5\r\n$3\r\nSET\r\n$3\r\nsxp\r\n$1\r\nv\r\n$4\r\nEXAT\r\n$1\r\n1\r\n\
    *2\r\n$3\r\nGET\r\n$3\r\nsxp\r\n";

struct Node {
    child: Child,
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The end of the RESP2 reply starting at `start` (simple, error, integer
/// or bulk), or `None` while it is incomplete.
fn reply_end(buf: &[u8], start: usize) -> Option<usize> {
    let line = buf.get(start..)?.windows(2).position(|w| w == b"\r\n")? + start;
    let after_line = line + 2;
    match buf[start] {
        b'$' => {
            let len: i64 = std::str::from_utf8(&buf[start + 1..line]).ok()?.parse().ok()?;
            let Ok(len) = usize::try_from(len) else { return Some(after_line) };
            let end = after_line + len + 2;
            (buf.len() >= end).then_some(end)
        }
        _ => Some(after_line),
    }
}

/// Reads until `count` whole replies have arrived, within `deadline`.
fn read_replies(stream: &mut TcpStream, count: usize, deadline: Instant) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let mut end = Some(0);
        for _ in 0..count {
            end = end.and_then(|at| reply_end(&buf, at));
        }
        if end.is_some() {
            return buf;
        }
        assert!(
            Instant::now() < deadline,
            "replies incomplete: {:?}",
            String::from_utf8_lossy(&buf)
        );
        let read = stream.read(&mut chunk).expect("read replies");
        assert!(read > 0, "connection closed: {:?}", String::from_utf8_lossy(&buf));
        buf.extend_from_slice(&chunk[..read]);
    }
}

/// Boots a volatile node and connects the moment its listener accepts.
fn boot_and_connect() -> (Node, TcpStream) {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("probe bind")
        .local_addr()
        .expect("probe address")
        .port();
    // `INF_PRE_ORIGIN_BIN` points the test at another build, to show it red
    // against a binary that clamps onto the origin.
    let binary = std::env::var("INF_PRE_ORIGIN_BIN")
        .unwrap_or_else(|_| env!("CARGO_BIN_EXE_infinityd").to_owned());
    let child = Command::new(binary)
        .args(["--port", &port.to_string(), "--cells", CELLS])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn infinityd");
    let mut node = Node { child };
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(stream) = TcpStream::connect(("127.0.0.1", port)) {
            return (node, stream);
        }
        if let Some(status) = node.child.try_wait().expect("try_wait") {
            panic!("infinityd exited during boot: {status}");
        }
        assert!(Instant::now() < deadline, "infinityd never accepted on {port}");
        std::thread::sleep(Duration::from_micros(200));
    }
}

#[test]
fn a_pre_origin_exat_sent_at_connect_is_nil_on_every_boot() {
    let mut served = Vec::new();
    for boot in 0..BOOTS {
        let (node, mut stream) = boot_and_connect();
        stream.write_all(PIPELINE).expect("send the pipeline");
        let deadline = Instant::now() + Duration::from_secs(30);
        stream.set_read_timeout(Some(Duration::from_secs(30))).expect("read timeout");
        let replies = read_replies(&mut stream, 2, deadline);
        // The node answering is the one this boot spawned, not a process
        // that took the port between the probe bind and the boot.
        stream.write_all(b"*2\r\n$4\r\nINFO\r\n$6\r\nserver\r\n").expect("send INFO");
        let info = read_replies(&mut stream, 1, deadline);
        let pid = format!("process_id:{}\r\n", node.child.id());
        assert!(String::from_utf8_lossy(&info).contains(&pid), "boot {boot}: another server");
        if replies != b"+OK\r\n$-1\r\n" {
            served.push(format!("boot {boot}: {:?}", String::from_utf8_lossy(&replies)));
        }
    }
    assert!(served.is_empty(), "SET EXAT 1 / GET at connect, not nil: {served:?}");
}
