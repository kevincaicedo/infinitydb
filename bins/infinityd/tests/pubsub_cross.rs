#![allow(
    clippy::disallowed_types,
    reason = "test target: std containers in test code, outside cell code (ADR-0163 D2)"
)]
#![allow(
    clippy::disallowed_methods,
    reason = "test-only: process, socket and deadline fixtures outside cell code (ADR-0144 D5)"
)]
//! Two cells publishing to each other's channels at the shipped topology:
//! the binary at `--cells 2`, its 1,024 credits per directed pair, 40
//! publishers on each cell, each with 64 `PUBLISH`es pipelined to channels
//! the other cell owns, and a `PSUBSCRIBE *` on each cell, so every owner
//! fans a leg back toward the publishers' cell. Each side's publishes keep
//! more operations in flight than the window holds, so unanswered
//! `INF.PUB`s can hold every credit of both directions while each owner's
//! pump fans back: every reply arrives, `:2`, only while the fan's legs draw
//! a credit no unanswered publish can hold.

#[path = "support/node.rs"]
mod node;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use inf_store::SlotRouter;
use node::{Node, NodeBinary};

const CELLS: u16 = 2;
const PUBLISHERS_PER_CELL: usize = 40;
const PUBLISHES_PER_CONNECTION: usize = 64;
/// Connections opened to find 40 publishers and a subscriber on each cell,
/// at most: the kernel spreads them, the test does not place them.
const CONNECT_ATTEMPTS_MAX: usize = 1_000;
/// Every reply arrives within this, or the run is stalled.
const REPLY_DEADLINE: Duration = Duration::from_secs(30);

struct Server {
    child: Node,
    stderr: PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.stderr);
    }
}

impl Server {
    fn text(&self) -> String {
        std::fs::read_to_string(&self.stderr).unwrap_or_default()
    }
}

/// The memory-only binary at two cells, on a port the kernel picked.
fn spawn() -> (Server, u16) {
    let stderr =
        std::env::temp_dir().join(format!("inf-pubsub-cross-{}.stderr", std::process::id()));
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("probe bind")
        .local_addr()
        .expect("addr")
        .port()
        .to_string();
    let child = NodeBinary::built()
        .command()
        .args(["--port", &port, "--cells", &CELLS.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::from(std::fs::File::create(&stderr).expect("stderr file")))
        .spawn()
        .expect("spawn infinityd");
    let mut server = Server { child, stderr };
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
            return (server, port);
        }
        assert!(Instant::now() < deadline, "infinityd never came up: {}", server.text());
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A connection and the cell that accepted it, read from `INFO server`'s
/// `cell:` line.
fn connect(port: u16) -> (TcpStream, u16) {
    let mut c = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    c.set_read_timeout(Some(REPLY_DEADLINE)).expect("timeout");
    c.write_all(b"*2\r\n$4\r\nINFO\r\n$6\r\nserver\r\n").expect("write");
    let mut reader = BufReader::new(c.try_clone().expect("clone"));
    let mut head = String::new();
    reader.read_line(&mut head).expect("bulk header");
    let len: usize = head.trim().strip_prefix('$').expect("a bulk reply").parse().expect("length");
    let mut body = vec![0u8; len + 2];
    reader.read_exact(&mut body).expect("INFO body");
    let text = String::from_utf8_lossy(&body);
    let cell = text
        .lines()
        .find_map(|l| l.strip_prefix("cell:"))
        .unwrap_or_else(|| panic!("no cell field in {text}"))
        .trim()
        .parse()
        .expect("cell id");
    (c, cell)
}

/// A channel the cell `owner` owns, for publisher `index`.
fn channel_owned_by(owner: u16, index: usize) -> String {
    let router = SlotRouter::new_contiguous(CELLS);
    (0..)
        .map(|n| format!("chan:{index}:{n}"))
        .find(|name| router.cell_of(SlotRouter::slot_of(name.as_bytes())).0 == owner)
        .expect("each cell owns slots")
}

fn encode(argv: &[&[u8]]) -> Vec<u8> {
    let mut wire = format!("*{}\r\n", argv.len()).into_bytes();
    for arg in argv {
        wire.extend_from_slice(format!("${}\r\n", arg.len()).as_bytes());
        wire.extend_from_slice(arg);
        wire.extend_from_slice(b"\r\n");
    }
    wire
}

#[test]
fn two_cells_publishing_to_each_others_channels_answer_every_publish() {
    let (_server, port) = spawn();
    let mut publishers: Vec<Vec<TcpStream>> = vec![Vec::new(), Vec::new()];
    let mut subscribers: Vec<Option<TcpStream>> = vec![None, None];
    for _ in 0..CONNECT_ATTEMPTS_MAX {
        let done = publishers.iter().all(|p| p.len() == PUBLISHERS_PER_CELL)
            && subscribers.iter().all(Option::is_some);
        if done {
            break;
        }
        let (c, cell) = connect(port);
        let cell = usize::from(cell);
        if subscribers[cell].is_none() {
            subscribers[cell] = Some(c);
        } else if publishers[cell].len() < PUBLISHERS_PER_CELL {
            publishers[cell].push(c);
        }
    }
    for (cell, sub) in subscribers.iter_mut().enumerate() {
        let sub = sub.as_mut().unwrap_or_else(|| panic!("no subscriber on cell {cell}"));
        sub.write_all(&encode(&[b"PSUBSCRIBE", b"*"])).expect("psubscribe");
        let mut confirm = [0u8; 64];
        let n = sub.read(&mut confirm).expect("psubscribe confirmation");
        assert!(confirm[..n].starts_with(b"*3\r\n$10\r\npsubscribe"), "cell {cell}: {confirm:?}");
    }
    // Each subscriber drains on its own thread, so delivery never backs up
    // into the cells while the publishers' replies are read.
    let delivered = PUBLISHERS_PER_CELL * PUBLISHES_PER_CONNECTION * usize::from(CELLS);
    let drains: Vec<_> = subscribers
        .into_iter()
        .map(|sub| {
            let mut sub = sub.expect("a subscriber per cell");
            std::thread::spawn(move || {
                const NEEDLE: &[u8] = b"$8\r\npmessage\r\n";
                let mut seen = 0usize;
                let mut carry: Vec<u8> = Vec::new();
                let mut buf = vec![0u8; 64 * 1024];
                let deadline = Instant::now() + REPLY_DEADLINE * 2;
                while seen < delivered && Instant::now() < deadline {
                    match sub.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            // A needle split across two reads is counted once:
                            // the carry keeps fewer bytes than the needle.
                            carry.extend_from_slice(&buf[..n]);
                            seen += carry.windows(NEEDLE.len()).filter(|w| *w == NEEDLE).count();
                            let keep = carry.len().min(NEEDLE.len() - 1);
                            carry.drain(..carry.len() - keep);
                        }
                    }
                }
                seen
            })
        })
        .collect();
    // Every publisher writes its whole burst before any reply is read.
    for (cell, conns) in publishers.iter_mut().enumerate() {
        let other = u16::try_from(1 - cell).expect("two cells");
        assert_eq!(conns.len(), PUBLISHERS_PER_CELL, "publishers on cell {cell}");
        for (index, conn) in conns.iter_mut().enumerate() {
            let channel = channel_owned_by(other, cell * PUBLISHERS_PER_CELL + index);
            let mut burst = Vec::new();
            for n in 0..PUBLISHES_PER_CONNECTION {
                burst.extend(encode(&[b"PUBLISH", channel.as_bytes(), format!("m{n}").as_bytes()]));
            }
            conn.write_all(&burst).expect("publish burst");
        }
    }
    let want = b":2\r\n".repeat(PUBLISHES_PER_CONNECTION);
    for (cell, conns) in publishers.iter_mut().enumerate() {
        for (index, conn) in conns.iter_mut().enumerate() {
            let mut got = vec![0u8; want.len()];
            conn.read_exact(&mut got).unwrap_or_else(|e| {
                panic!(
                    "cell {cell} publisher {index}: replies stalled past {REPLY_DEADLINE:?}: {e}"
                )
            });
            assert_eq!(
                got,
                want,
                "cell {cell} publisher {index}: {:?}",
                String::from_utf8_lossy(&got)
            );
        }
    }
    for (cell, drain) in drains.into_iter().enumerate() {
        let seen = drain.join().expect("drain thread");
        assert_eq!(seen, delivered, "cell {cell}'s subscriber saw {seen} of {delivered}");
    }
}
