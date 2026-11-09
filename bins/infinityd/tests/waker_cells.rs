#![allow(
    clippy::disallowed_types,
    reason = "test target: std containers in test code, outside cell code (ADR-0163 D2)"
)]
#![allow(
    clippy::disallowed_methods,
    reason = "test-only: process, socket and filesystem fixtures outside cell code (ADR-0144 D5)"
)]
//! No product path moves a waker off its cell (ADR-0149 D1, kept by ADR-0003 A1.4): a waker
//! used on another thread takes the executor's foreign lane, which counts each operation in
//! `waker_foreign_ops` (`INFO tripwires`, cell scope). Where cells are threads — the binary at
//! two cells — every cell reads 0 after a cross-cell workload. Its reach is two of the product's
//! waker-clone sites, each with its engagement, else the test is `VACUOUS`: the keyed fabric
//! gate's wait, which every remote leg takes (`raw_fabric_msgs` above 0 on each cell), and the
//! durable watermark's wait, which an ack gated on its fsync takes (`acks_gated` above 0). Not
//! reached, and named: the wait lists, whose engagement the wire does not count, and a tiered
//! namespace's cold gate.
#![cfg(target_os = "linux")]

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const CELLS: u16 = 2;
/// Keys each connection writes: enough that both cells own some.
const KEYS: usize = 32;

/// A scratch data directory under the build's own temporary directory, removed on drop.
struct DataDir(PathBuf);

impl DataDir {
    fn new(tag: &str) -> DataDir {
        let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "waker-cells-{tag}-{}-{}",
            std::process::id(),
            nanos()
        ));
        std::fs::create_dir_all(&path).expect("scratch data directory");
        DataDir(path)
    }
}

impl Drop for DataDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos())
}

/// `infinityd` at two cells on a data directory, killed and reaped on drop.
struct Server {
    child: Child,
    port: u16,
    stderr: PathBuf,
    _dir: DataDir,
}

impl Server {
    fn spawn(dir: DataDir) -> Server {
        let stderr = dir.0.join("stderr");
        // One port for both cells' listeners: a probe bind picks a free one.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .expect("probe bind")
            .local_addr()
            .expect("addr")
            .port()
            .to_string();
        let child = Command::new(env!("CARGO_BIN_EXE_infinityd"))
            .args(["--port", &port, "--cells", &CELLS.to_string()])
            .args(["--accept-handoff", "on", "--device-probe", "off"])
            .arg("--data-dir")
            .arg(&dir.0)
            .stdout(Stdio::null())
            .stderr(Stdio::from(std::fs::File::create(&stderr).expect("stderr file")))
            .spawn()
            .expect("spawn infinityd");
        let mut server = Server { child, port: 0, stderr, _dir: dir };
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
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Client(TcpStream);

impl Client {
    fn connect(port: u16) -> Client {
        let c = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        c.set_read_timeout(Some(Duration::from_secs(30))).expect("timeout");
        c.set_nodelay(true).expect("nodelay");
        Client(c)
    }

    fn call(&mut self, parts: &[&[u8]]) -> Vec<u8> {
        let mut wire = format!("*{}\r\n", parts.len()).into_bytes();
        for p in parts {
            wire.extend_from_slice(format!("${}\r\n", p.len()).as_bytes());
            wire.extend_from_slice(p);
            wire.extend_from_slice(b"\r\n");
        }
        self.0.write_all(&wire).expect("write");
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

/// How a connection deletes the keys it wrote.
#[derive(Clone, Copy)]
enum Delete {
    /// One `DEL` of all of them, across both cells: the numbered database's.
    AllAtOnce,
    /// One `DEL` a key: a named namespace refuses a multi-key command that spans cells.
    EachKey,
}

/// From `conn`: a `SET` and a `GET` of each of `KEYS` keys, which land on both cells, then their
/// `DEL`.
fn cross_cell_writes(conn: &mut Client, cell: u64, delete: Delete) {
    let keys: Vec<Vec<u8>> = (0..KEYS).map(|k| format!("w:{cell}:{k}").into_bytes()).collect();
    for key in &keys {
        conn.ok(&[b"SET", key, b"value"]);
        assert_eq!(conn.call(&[b"GET", key]), b"$5\r\nvalue\r\n");
    }
    match delete {
        Delete::AllAtOnce => {
            let mut del: Vec<&[u8]> = vec![b"DEL"];
            del.extend(keys.iter().map(Vec::as_slice));
            let deleted = conn.call(&del);
            assert_eq!(deleted, format!(":{KEYS}\r\n").into_bytes(), "every key deleted");
        }
        Delete::EachKey => {
            for key in &keys {
                assert_eq!(conn.call(&[b"DEL", key]), b":1\r\n", "the key deleted");
            }
        }
    }
}

/// The binary at two cells, two threads: a cross-cell workload on a connection to each cell, in
/// the numbered database and in a namespace of `always` durability, then `INFO tripwires` on
/// each cell reads `waker_foreign_ops:0`. A waker used on the peer's thread would count there.
#[test]
fn no_waker_takes_the_foreign_lane_at_two_cells() {
    let server = Server::spawn(DataDir::new("two"));
    let mut cells = connection_per_cell(server.port);
    for (cell, conn) in &mut cells {
        cross_cell_writes(conn, *cell, Delete::AllAtOnce);
    }
    let (first, _) = cells.first_key_value().expect("a cell");
    let first = *first;
    cells
        .get_mut(&first)
        .expect("the first cell")
        .ok(&[b"INF.NS", b"CREATE", b"always", b"MODE", b"durable", b"FSYNC", b"always"]);
    for (cell, conn) in &mut cells {
        conn.ok(&[b"INF.NS", b"USE", b"always"]);
        cross_cell_writes(conn, *cell, Delete::EachKey);
    }
    let mut acks_gated = 0;
    for (cell, conn) in &mut cells {
        let tripwires = conn.info("tripwires");
        let fabric = field(&tripwires, "raw_fabric_msgs");
        assert!(fabric > 0, "VACUOUS: cell {cell} moved no fabric message: no remote leg waited");
        acks_gated += field(&conn.info("persistence"), "acks_gated");
        assert_eq!(
            field(&tripwires, "waker_foreign_ops"),
            0,
            "cell {cell}: waker_foreign_ops above 0: a waker took the foreign lane off its thread"
        );
    }
    assert!(acks_gated > 0, "VACUOUS: no ack waited on its fsync: the watermark wait unreached");
    drop(server);
}
