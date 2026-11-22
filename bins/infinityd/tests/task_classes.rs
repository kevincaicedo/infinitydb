//! A cell's task and gate-holder bounds, read from the wire at two cells
//! (ADR-0177 D2, D4; ADR-0123 A1). A connection's work is one `conn` task,
//! alive while its slot is occupied, a draining slot's among them, so a cell
//! holds at most its connection share `C = max(1, maxclients / cells)` of
//! them; and a task holds at most its window of gate units, so a cell's
//! fabric gate holds at most `H = Σ T_j × W_j + (cells − 1) × d` holders and
//! one connection's fan at most its window of them. `INFO caps` reports
//! both rows, `conn-tasks` and `fabric-gate-holders`, per cell: the cap, the
//! high water and the crossings. Each test starts its own node and reads the
//! caps of every cell through a connection held on it. A stopped process
//! (`SIGSTOP`) reads nothing, so what clients send while it is stopped
//! reaches the node at one reap once it runs again: the deterministic shape
//! of closes and connects that meet in one pass. One process holds every
//! cell, so no peer is stopped alone, and a stalled cell (`DEBUG SLEEP`)
//! keeps serving the fabric.
#![allow(
    clippy::disallowed_methods,
    reason = "test target: harness deadlines and a data directory, not cell code"
)]
#![allow(
    clippy::disallowed_types,
    reason = "test target: the node's stderr file and the signal commands, not cell code"
)]

#[path = "support/node.rs"]
mod node;

use node::{Node, NodeBinary};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const CELLS: u16 = 2;

/// A reply read's hang bound.
const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the node may take to retire the slots of closed subscribers and
/// admit clients again: a few iterations, so the bound is slack by orders of
/// magnitude. A hang bound, never the verdict.
const SETTLE_DEADLINE: Duration = Duration::from_secs(30);

/// Redis's reply past `maxclients` (networking.c), then a close.
const MAXCLIENTS_REFUSAL: &[u8] = b"-ERR max number of clients reached\r\n";

struct Server {
    child: Node,
    port: u16,
    stderr: PathBuf,
    dir: PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        // A stopped process must run again before it can die of its kill.
        let _ = signal(self.child.id(), "-CONT");
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Server {
    /// A node of two cells, every cell reachable through the accept
    /// hand-off, on a data directory of its own; `maxclients` is the node's
    /// connection bound `B`, the shipped default when `None`.
    fn spawn(maxclients: Option<u32>, name: &str) -> Server {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/e2e-task-classes")
            .join(format!("{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("data dir");
        let stderr = dir.join("stderr");
        // `--port 0`: the node binds a port of the kernel's choosing and
        // prints it, so no probe's port is lost to a concurrent binder.
        let mut command =
            NodeBinary::built().command().args(["--port", "0", "--cells", &CELLS.to_string()]);
        if let Some(maxclients) = maxclients {
            command = command.args(["--maxclients", &maxclients.to_string()]);
        }
        let child = command
            .args(["--accept-handoff", "on", "--device-probe", "off", "--data-dir"])
            .arg(dir.join("data"))
            .stdout(Stdio::null())
            .stderr(Stdio::from(std::fs::File::create(&stderr).expect("stderr file")))
            .spawn()
            .expect("spawn infinityd");
        let mut server = Server { child, port: 0, stderr, dir };
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

    fn connect(&self) -> TcpStream {
        let stream = TcpStream::connect(("127.0.0.1", self.port)).expect("connect");
        stream.set_read_timeout(Some(READ_TIMEOUT)).expect("timeout");
        stream.set_nodelay(true).expect("nodelay");
        stream
    }

    /// Stops the process: it reads nothing until [`Server::resume`].
    fn stop(&self) {
        assert!(signal(self.child.id(), "-STOP"), "SIGSTOP");
    }

    fn resume(&self) {
        assert!(signal(self.child.id(), "-CONT"), "SIGCONT");
    }
}

/// Sends `signal` (`-STOP`, `-CONT`) to `pid` with `kill(1)`.
fn signal(pid: u32, signal: &str) -> bool {
    Command::new("kill")
        .args([signal, &pid.to_string()])
        .status()
        .is_ok_and(|status| status.success())
}

fn wire(parts: &[&[u8]]) -> Vec<u8> {
    let mut wire = format!("*{}\r\n", parts.len()).into_bytes();
    for part in parts {
        wire.extend_from_slice(format!("${}\r\n", part.len()).as_bytes());
        wire.extend_from_slice(part);
        wire.extend_from_slice(b"\r\n");
    }
    wire
}

/// One reply line, then a bulk's payload when the line opens one.
fn reply(stream: &mut TcpStream) -> Vec<u8> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    while !line.ends_with(b"\r\n") {
        stream.read_exact(&mut byte).expect("read");
        line.push(byte[0]);
    }
    if line[0] == b'$' && !line.starts_with(b"$-1") {
        let len: usize =
            std::str::from_utf8(&line[1..line.len() - 2]).expect("utf8").parse().expect("len");
        let mut payload = vec![0u8; len + 2];
        stream.read_exact(&mut payload).expect("payload");
        line.extend_from_slice(&payload);
    }
    line
}

fn call(stream: &mut TcpStream, parts: &[&[u8]]) -> Vec<u8> {
    stream.write_all(&wire(parts)).expect("write");
    reply(stream)
}

/// An integer reply's value.
fn integer(line: &[u8]) -> u64 {
    let text = std::str::from_utf8(line).expect("text");
    let Some(value) = text.strip_prefix(':') else { panic!("not an integer reply: {text:?}") };
    value.trim_end().parse().expect("an integer")
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

fn info(stream: &mut TcpStream, section: &str) -> String {
    String::from_utf8(call(stream, &[b"INFO", section.as_bytes()])).expect("INFO is text")
}

/// One census row of a cell's `INFO caps`.
#[derive(Debug)]
struct CapRow {
    entries_max: u64,
    high_water: u64,
    crossings: u64,
}

/// The row `name` of `stream`'s cell, read from its `INFO caps`.
fn cap_row(stream: &mut TcpStream, name: &str) -> CapRow {
    let caps = info(stream, "caps");
    assert!(caps.contains("caps_scope:cell\r\n"), "{caps}");
    let prefix = format!("cap_row_{name}:");
    let row = caps
        .lines()
        .find_map(|line| line.strip_prefix(prefix.as_str()))
        .unwrap_or_else(|| panic!("no {name} row in {caps}"));
    let number = |key: &str| -> u64 {
        row.split(',')
            .find_map(|kv| kv.strip_prefix(key).and_then(|v| v.strip_prefix('=')))
            .unwrap_or_else(|| panic!("no {key} in {row}"))
            .parse()
            .expect("a number")
    };
    CapRow {
        entries_max: number("entries_max"),
        high_water: number("high_water"),
        crossings: number("crossings"),
    }
}

/// Every byte the server sends until it closes, by a FIN or by a reset (a
/// refused client's command unread at the close).
fn read_to_end(stream: &mut TcpStream, what: &str) -> Vec<u8> {
    let mut delivered = Vec::new();
    let mut chunk = vec![0u8; 1 << 12];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => return delivered,
            Ok(n) => delivered.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => return delivered,
            Err(e) => {
                panic!("{what}: the server never closed after {} bytes ({e})", delivered.len())
            }
        }
    }
}

/// What the node did with a fresh connection that sent `CLIENT ID`.
#[derive(Debug)]
enum Admission {
    /// Admitted on `cell` (a client id is `cell << 48 | seq`).
    Admitted { cell: usize },
    /// Answered Redis's `maxclients` frame, then closed.
    Refused,
}

/// Reads the answer to a `CLIENT ID` written on `stream`: a refused client
/// reads the frame and then the end, nothing else.
fn admission(stream: &mut TcpStream) -> Admission {
    let line = reply(stream);
    if line == MAXCLIENTS_REFUSAL {
        let rest = read_to_end(stream, "a refused client");
        assert!(rest.is_empty(), "a refused client read {rest:?} after the frame");
        return Admission::Refused;
    }
    let cell = usize::try_from(integer(&line) >> 48).expect("a cell index");
    assert!(cell < usize::from(CELLS), "a client id names cell {cell} of {CELLS}");
    Admission::Admitted { cell }
}

/// The connections the churn holds on one cell: the probe that reads the
/// cell's `INFO`, and its pattern subscribers.
struct Held {
    probe: TcpStream,
    /// `C`, the `conn-tasks` cap the cell reports.
    share: u64,
    subscribers: Vec<TcpStream>,
}

impl Held {
    fn count(&self) -> u64 {
        1 + self.subscribers.len() as u64
    }
}

/// The churn's state: what it holds on each cell.
struct Churn<'s> {
    server: &'s Server,
    cells: Vec<Option<Held>>,
    /// Patterns subscribed so far, each a subscriber's own.
    patterns: usize,
}

impl Churn<'_> {
    /// Keeps a client the node admitted on `cell`: the cell's probe when it
    /// has none, reading the cell's share from its caps, else a subscriber
    /// to a pattern of its own. A cell that admits a client past the share
    /// it holds is the bound broken where a client sees it.
    fn keep(&mut self, cell: usize, mut stream: TcpStream) {
        let Some(held) = self.cells[cell].as_mut() else {
            let share = cap_row(&mut stream, "conn-tasks").entries_max;
            let maxclients = field(&info(&mut stream, "clients"), "maxclients");
            let cells = field(&info(&mut stream, "server"), "cells");
            assert_eq!(
                share,
                (maxclients / cells).max(1),
                "cell {cell}: the conn class reports {share} slots for maxclients {maxclients} \
                 at {cells} cells (C = max(1, maxclients / cells))"
            );
            self.cells[cell] = Some(Held { probe: stream, share, subscribers: Vec::new() });
            return;
        };
        assert!(
            held.count() < held.share,
            "cell {cell} admitted a client past the {} connections it holds, its share",
            held.count()
        );
        let pattern = format!("tc:{}:*", self.patterns);
        self.patterns += 1;
        let confirmed =
            format!("*3\r\n$10\r\npsubscribe\r\n${}\r\n{pattern}\r\n:1\r\n", pattern.len());
        stream.write_all(&wire(&[b"PSUBSCRIBE", pattern.as_bytes()])).expect("write");
        let mut read = vec![0u8; confirmed.len()];
        stream.read_exact(&mut read).expect("the subscription's confirmation");
        assert_eq!(read, confirmed.as_bytes(), "PSUBSCRIBE {pattern}");
        held.subscribers.push(stream);
    }

    fn full(&self) -> bool {
        self.cells.iter().all(|held| held.as_ref().is_some_and(|held| held.count() == held.share))
    }

    /// Connects clients until every cell holds its share; a refusal (a cell
    /// full, or one whose closed subscribers' slots are not yet vacated)
    /// leaves the client closed, and the next one tries again.
    fn fill(&mut self, what: &str) {
        let deadline = Instant::now() + SETTLE_DEADLINE;
        while !self.full() {
            let mut stream = self.server.connect();
            stream.write_all(&wire(&[b"CLIENT", b"ID"])).expect("write");
            if let Admission::Admitted { cell } = admission(&mut stream) {
                self.keep(cell, stream);
            }
            assert!(Instant::now() < deadline, "{what}: the cells never refilled to their share");
        }
    }

    /// The smallest share of a cell.
    fn share_min(&self) -> u64 {
        self.cells.iter().flatten().map(|held| held.share).min().expect("a cell")
    }
}

/// The churn: each cell holds its share `C` of connections, a probe and
/// `C − 1` pattern subscribers, each to a pattern of its own, so each close
/// takes its pattern from one subscriber to none and owes the peer a delta.
/// Eight rounds: with the node stopped, every subscriber closes and `C − 1`
/// clients connect, no more than either cell's closed subscribers, so a
/// refusal is explained only by a slot not yet vacated. Each cell reaps its
/// subscribers' ends and its connects at one pass: a closing slot stays
/// occupied, its task with it, until its pump has shipped its delta and read
/// the ack, a fabric round trip after that pass. So a client that reaches a
/// cell then is refused (VACUOUS if none was), and one admitted later takes
/// a vacated slot; the cells then fill to their share again. A node that made
/// a task per close, beside a vacated slot, would hold tasks past `C` under
/// this churn, which no client could see.
///
/// Oracle, from the wire: no cell admits a client past the share it holds;
/// a refused client reads the frame and then the end; once the last
/// subscribers close and the node is quiet, `PUBSUB NUMPAT` reads 0 on each
/// cell and each admits `C − 1` clients again; and each cell's `conn-tasks`
/// row reports a cap of `C = max(1, maxclients / cells)` (the class is the
/// boot share, ADR-0123 A1) and a high water of at most `C`, reached.
#[test]
fn subscribers_closed_with_their_deltas_out_hold_no_task_past_the_share() {
    /// `B`: a share of 8 connections a cell, which the churn fills.
    const MAXCLIENTS: u32 = 16;
    const ROUNDS: usize = 8;
    let server = Server::spawn(Some(MAXCLIENTS), "churn");
    let mut churn =
        Churn { server: &server, cells: (0..CELLS).map(|_| None).collect(), patterns: 0 };
    churn.fill("the first fill");
    let arrivals = churn.share_min() - 1;
    let (mut closed, mut refused_at_reap) = (0usize, 0u64);
    for round in 0..ROUNDS {
        server.stop();
        for held in churn.cells.iter_mut().flatten() {
            closed += held.subscribers.len();
            held.subscribers.clear();
        }
        let connects: Vec<TcpStream> = (0..arrivals)
            .map(|_| {
                let mut stream = server.connect();
                stream.write_all(&wire(&[b"CLIENT", b"ID"])).expect("write");
                stream
            })
            .collect();
        server.resume();
        for mut stream in connects {
            match admission(&mut stream) {
                Admission::Admitted { cell } => churn.keep(cell, stream),
                Admission::Refused => refused_at_reap += 1,
            }
        }
        churn.fill(&format!("round {round}"));
    }
    for held in churn.cells.iter_mut().flatten() {
        closed += held.subscribers.len();
        held.subscribers.clear();
    }
    let deadline = Instant::now() + SETTLE_DEADLINE;
    for (cell, held) in churn.cells.iter_mut().enumerate() {
        let held = held.as_mut().expect("a probe on every cell");
        loop {
            let patterns = integer(&call(&mut held.probe, &[b"PUBSUB", b"NUMPAT"]));
            if patterns == 0 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "cell {cell}: {patterns} patterns still counted {SETTLE_DEADLINE:?} after every \
                 subscriber closed"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    churn.fill("the fill after the last closes");
    for (cell, held) in churn.cells.iter_mut().enumerate() {
        let held = held.as_mut().expect("a probe on every cell");
        let row = cap_row(&mut held.probe, "conn-tasks");
        assert!(
            row.high_water <= held.share,
            "cell {cell}: {} conn tasks at once, past its share of {} ({row:?})",
            row.high_water,
            held.share
        );
        assert_eq!(row.high_water, held.share, "VACUOUS: cell {cell}'s conn class never filled");
    }
    assert!(
        refused_at_reap >= 1,
        "VACUOUS: no client of {ROUNDS} rounds reached a cell while its closed subscribers' \
         slots were occupied"
    );
    assert!(closed as u64 > churn.share_min(), "VACUOUS: {closed} subscribers closed");
}

/// `DELS` commands of `KEYS_PER_DEL` keys each, one connection.
const DELS: usize = 64;
const KEYS_PER_DEL: usize = 1_023;

/// The `j`-th key of `DEL` number `del`: no hash tag, so the keys spread
/// over both cells and about half of each `DEL` is remote.
fn fan_key(del: usize, j: usize) -> String {
    format!("tc:del{del}:{j}")
}

/// The keys `DEL` number `del` finds: its first `KEYS_PER_DEL − del`, so
/// each of the 64 replies is a count of its own, in pipeline order.
fn present(del: usize) -> usize {
    KEYS_PER_DEL - del
}

/// One connection sets the keys, then pipelines 64 `DEL`s of 1,023 keys
/// each in one write: every reply is its own exact count, in order. A
/// connection's task holds at most its window `W` of gate units (ADR-0177
/// D4), so a `DEL` sends the first `W` of its remote legs and settles its
/// oldest before the next: the cell's gate holds at most `W` holders for the
/// connection, beside the `owner-pub` pump's one unit per peer, and never
/// fills its `H` (no crossing). The window fills: the connection's cell holds
/// `W` at once (VACUOUS otherwise). A fan sent whole, every remote leg of
/// the pipeline out before its first await, holds tens of thousands of
/// holders, past the window and under the shipped `H`; a gate it fills parks
/// its sends with no leg settled, so no reply comes.
#[test]
fn sixty_four_pipelined_dels_answer_exact_counts_within_their_window() {
    let server = Server::spawn(None, "fans");
    let mut fan = server.connect();
    fan.write_all(&wire(&[b"CLIENT", b"ID"])).expect("write");
    let Admission::Admitted { cell: fan_cell } = admission(&mut fan) else {
        panic!("the first client of an empty node was refused")
    };
    for del in 0..DELS {
        let sets: Vec<u8> = (0..present(del))
            .flat_map(|j| wire(&[b"SET", fan_key(del, j).as_bytes(), b"1"]))
            .collect();
        fan.write_all(&sets).expect("write");
        let mut oks = vec![0u8; 5 * present(del)];
        fan.read_exact(&mut oks).expect("the SETs' replies");
        assert!(oks == b"+OK\r\n".repeat(present(del)), "DEL {del}'s keys were not all set");
    }
    let dels: Vec<u8> = (0..DELS)
        .flat_map(|del| {
            let keys: Vec<String> = (0..KEYS_PER_DEL).map(|j| fan_key(del, j)).collect();
            let mut argv: Vec<&[u8]> = vec![b"DEL"];
            argv.extend(keys.iter().map(String::as_bytes));
            wire(&argv)
        })
        .collect();
    fan.write_all(&dels).expect("write");
    for del in 0..DELS {
        let line = reply(&mut fan);
        assert_eq!(
            String::from_utf8_lossy(&line),
            format!(":{}\r\n", present(del)),
            "DEL {del} of {DELS}, pipelined"
        );
    }
    let mut probes: Vec<Option<TcpStream>> = (0..CELLS).map(|_| None).collect();
    probes[fan_cell] = Some(fan);
    for _ in 0..64 {
        if probes.iter().all(Option::is_some) {
            break;
        }
        let mut probe = server.connect();
        probe.write_all(&wire(&[b"CLIENT", b"ID"])).expect("write");
        if let Admission::Admitted { cell } = admission(&mut probe) {
            probes[cell].get_or_insert(probe);
        }
    }
    let window = u64::from(inf_server::limits::GATE_UNITS_PER_CONN_TASK);
    let owner_pub_units = u64::from(CELLS) - 1;
    for (cell, probe) in probes.iter_mut().enumerate() {
        let probe = probe.as_mut().unwrap_or_else(|| panic!("no probe of 64 reached cell {cell}"));
        let row = cap_row(probe, "fabric-gate-holders");
        assert!(
            row.high_water <= row.entries_max,
            "cell {cell}: {} gate holders at once, past H = {} ({row:?})",
            row.high_water,
            row.entries_max
        );
        assert_eq!(
            row.crossings, 0,
            "cell {cell}: the gate filled its H = {} and paced a send ({row:?})",
            row.entries_max
        );
        let fans = u64::from(cell == fan_cell);
        assert!(
            row.high_water <= window * fans + owner_pub_units,
            "cell {cell}: {} gate holders at once for {fans} fanning connections of a window \
             of {window} ({row:?})",
            row.high_water
        );
        if cell == fan_cell {
            assert!(
                row.high_water >= window,
                "VACUOUS: the fanning connection's cell held {} holders, under its window of \
                 {window}",
                row.high_water
            );
        }
    }
}
