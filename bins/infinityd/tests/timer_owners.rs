//! The binary arm of the cell's timer population (ADR-0154 D1, D4): a
//! cell's timers are its roles' owners, one each, so a burst of hold
//! episodes leaves at most one fill timer behind. A two-cell node on the
//! fua class, its fill window at the 100 ms maximum and an `everysec`
//! namespace, takes 64 8 KiB `SET`s of one hash tag (one owning cell) on
//! one connection, each `+OK` read before the next: the fua class writes
//! aligned frames, padded to the next 4 KiB, and a one-record frame pads to
//! 12 KiB, below the 16 KiB fill target, so the LOG step holds it and opens
//! a fill episode, which the next record's frame (20 KiB padded) seals.
//! `INFO persistence` on each cell then reads `timers_armed_max`: on the
//! owning cell its everysec owner and its fill owner (the group hold holds
//! packed frames only, and no accept fails), and at most the four roles on
//! any cell; `INFO caps` reads the `timer-owners` row at its cap with no
//! crossing.
//!
//! Engagement: the owning cell's `frame_waits_fill` rose by at least 8 and
//! the burst took less than the fill window, so every episode's timer was
//! still due when the last one began; else the run is `VACUOUS`. The data
//! directory is under the workspace `target/`, a disk filesystem, since the
//! fua class writes through `O_DIRECT`.
#![cfg(target_os = "linux")]
#![allow(clippy::disallowed_methods, clippy::disallowed_types)] // harness process, not cell code

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const CELLS: u16 = 2;
const SETS: usize = 64;
/// A value whose one-record frame pads to 12 KiB on an aligned segment,
/// below the default 16 KiB fill target, so a frame of one is held.
const VALUE_LEN: usize = 8 << 10;
/// The fill window, µs: the flag's maximum.
const FILL_WINDOW_US: u64 = 100_000;
/// Fill episodes the burst must open on the owning cell.
const EPISODES_MIN: u64 = 8;
/// A server cell's timer roles, and so its timer positions.
const ROLES: u64 = inf_server::TimerRole::COUNT as u64;
/// The owning cell's armed peak: its everysec owner and its fill owner.
const OWNER_ARMED_MAX: u64 = 2;

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

impl Server {
    /// Two cells on the fua class with the fill window at its maximum;
    /// every cell reachable through the accept hand-off.
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
            .args(["--accept-handoff", "on", "--device-probe", "off"])
            .args(["--barrier-class", "fua", "--fill-window-us", &FILL_WINDOW_US.to_string()])
            .arg("--data-dir")
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

/// Each cell's `log_records_appended`.
fn records_appended(cells: &mut BTreeMap<u64, Client>) -> BTreeMap<u64, u64> {
    cells
        .iter_mut()
        .map(|(cell, conn)| (*cell, field(&conn.info("persistence"), "log_records_appended")))
        .collect()
}

/// The `timer-owners` row of an `INFO caps` text: `(high_water,
/// crossings)`.
fn timer_owners_row(caps: &str) -> (u64, u64) {
    let row = caps
        .lines()
        .find_map(|line| line.strip_prefix("cap_row_timer-owners:"))
        .unwrap_or_else(|| panic!("no timer-owners row in {caps}"));
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
fn a_burst_of_fill_episodes_leaves_one_fill_timer_per_cell() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/e2e-timer-owners")
        .join(std::process::id().to_string());
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("data dir");
    let server = Server::spawn(&dir);
    let mut writer = Client::connect(server.port);
    writer.ok(&[b"INF.NS", b"CREATE", b"tm", b"MODE", b"durable", b"FSYNC", b"everysec"]);
    writer.ok(&[b"INF.NS", b"USE", b"tm"]);
    let mut cells = connection_per_cell(server.port);
    // The owning cell of `{tm}`: the one whose log appends the probe.
    let before = records_appended(&mut cells);
    writer.ok(&[b"SET", b"{tm}probe", b"p"]);
    let after = records_appended(&mut cells);
    let owners: Vec<u64> = after
        .iter()
        .filter(|(cell, appended)| **appended > before[cell])
        .map(|(c, _)| *c)
        .collect();
    assert_eq!(owners.len(), 1, "one cell owns the hash tag: {before:?} then {after:?}");
    let owner = owners[0];
    let waits_before =
        field(&cells.get_mut(&owner).expect("owner").info("persistence"), "frame_waits_fill");
    let value = vec![b'v'; VALUE_LEN];
    let start = Instant::now();
    for i in 0..SETS {
        writer.ok(&[b"SET", format!("{{tm}}k{i}").as_bytes(), &value]);
    }
    let burst = start.elapsed();
    let persistence: BTreeMap<u64, String> =
        cells.iter_mut().map(|(cell, conn)| (*cell, conn.info("persistence"))).collect();
    let episodes = field(&persistence[&owner], "frame_waits_fill") - waits_before;
    println!("owner cell {owner}: {episodes} fill episodes in {burst:?}");
    for (cell, info) in &persistence {
        println!("cell {cell}: timers_armed_max {}", field(info, "timers_armed_max"));
    }
    assert!(
        episodes >= EPISODES_MIN && burst < Duration::from_micros(FILL_WINDOW_US),
        "VACUOUS: {episodes} fill episodes on cell {owner} (want {EPISODES_MIN}) in {burst:?} \
         (want under {FILL_WINDOW_US} µs)"
    );
    let owner_armed = field(&persistence[&owner], "timers_armed_max");
    assert_eq!(
        owner_armed, OWNER_ARMED_MAX,
        "the owning cell held {owner_armed} timers armed at once: its everysec and fill owners"
    );
    for (cell, info) in &persistence {
        let armed = field(info, "timers_armed_max");
        assert!(armed <= ROLES, "cell {cell} held {armed} timers armed, above its {ROLES} roles");
    }
    for (cell, conn) in &mut cells {
        let (high_water, crossings) = timer_owners_row(&conn.info("caps"));
        assert_eq!((high_water, crossings), (ROLES, 0), "cell {cell}'s timer-owners row");
    }
    drop(server);
    let _ = std::fs::remove_dir_all(&dir);
}
