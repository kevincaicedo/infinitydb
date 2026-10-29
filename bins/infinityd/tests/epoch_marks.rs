//! The binary arm of the extent-reclaim marks (ADR-0061 D5; ADR-0163 D2):
//! each tiered namespace keeps one `(seq, epoch)` mark per ack class, so a
//! burst of writes inside one everysec period holds a few marks where the
//! uncapped rule held one per reactor iteration with a write. A two-cell
//! node with one frame in flight takes 64 blob `SET`s of one hash tag (one
//! owning cell) to a tiered `everysec` namespace and 64 overwrites that kill
//! their extents, one command at a time; `INFO caps` then reads the
//! `epoch-marks` row at a high water of at most 6 — one frame behind the
//! written prefix, two FLUSH-class barriers, a rotation's seal, the trailing
//! written entry and the builder — with no crossing on any cell, and once the
//! next tick's fsync covers the overwrites the owning cell reclaims all 64
//! extents. The cap's merge needs a wedged device and is the simulator's.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)] // harness process, not cell code

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const CELLS: u16 = 2;
const KEYS: usize = 64;
/// A blob: above the namespace's 4 KiB `BLOB-THRESHOLD`.
const BLOB_LEN: usize = 5 << 10;
/// The marks' bound with one frame in flight (ADR-0163 D2's `F + C + 2`).
const MARKS_HIGH_WATER_MAX: u32 = 6;

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
    /// Two cells with one frame in flight; every cell reachable through the
    /// accept hand-off, so a fresh connection lands on either.
    fn spawn(dir: &Path) -> Server {
        let stderr = dir.join("stderr");
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .expect("probe bind")
            .local_addr()
            .expect("addr")
            .port()
            .to_string();
        let child = Command::new(env!("CARGO_BIN_EXE_infinityd"))
            .args(["--port", &port, "--cells", &CELLS.to_string(), "--frames-in-flight", "1"])
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

/// The `epoch-marks` row of an `INFO caps` text: `(high_water, crossings)`.
fn marks_row(caps: &str) -> (u32, u64) {
    let row = caps
        .lines()
        .find_map(|line| line.strip_prefix("cap_row_epoch-marks:"))
        .unwrap_or_else(|| panic!("no epoch-marks row in {caps}"));
    let number = |key: &str| -> u64 {
        row.split(',')
            .find_map(|kv| kv.strip_prefix(key).and_then(|v| v.strip_prefix('=')))
            .unwrap_or_else(|| panic!("no {key} in {row}"))
            .parse()
            .expect("a number")
    };
    (u32::try_from(number("high_water")).expect("u32"), number("crossings"))
}

#[test]
fn a_burst_in_one_everysec_period_holds_few_marks_and_reclaims_every_death() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/e2e-epoch-marks")
        .join(std::process::id().to_string());
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("data dir");
    let server = Server::spawn(&dir);
    let mut client = Client::connect(server.port);
    client.ok(&[
        b"INF.NS",
        b"CREATE",
        b"em",
        b"MODE",
        b"durable",
        b"FSYNC",
        b"everysec",
        b"MEM-BUDGET",
        b"3mb",
        b"MAINTAIN-SLICE",
        b"1mb",
        b"BLOB-THRESHOLD",
        b"4kb",
    ]);
    client.ok(&[b"INF.NS", b"USE", b"em"]);
    let blob = vec![b'b'; BLOB_LEN];
    for i in 0..KEYS {
        client.ok(&[b"SET", format!("{{em}}k{i}").as_bytes(), &blob]);
    }
    for i in 0..KEYS {
        client.ok(&[b"SET", format!("{{em}}k{i}").as_bytes(), b"inline"]);
    }
    let mut cells = connection_per_cell(server.port);
    let mut held = 0;
    for (cell, conn) in &mut cells {
        let caps = conn.info("caps");
        assert!(caps.contains("caps_scope:cell\r\n"), "cell {cell}: {caps}");
        assert_eq!(field(&caps, "cap_assembly_violations"), 0, "cell {cell}: {caps}");
        let (high_water, crossings) = marks_row(&caps);
        assert!(
            high_water <= MARKS_HIGH_WATER_MAX,
            "cell {cell}: {high_water} reclaim marks held, above {MARKS_HIGH_WATER_MAX}: {caps}"
        );
        assert_eq!(crossings, 0, "cell {cell}: a cap merge with no wedge: {caps}");
        held = held.max(high_water);
    }
    assert!(held >= 2, "VACUOUS: the owning cell's marks never held two");
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let reclaimed: u64 = cells
            .values_mut()
            .map(|conn| field(&conn.info("tiering"), "tiering_blob_extents_reclaimed"))
            .sum();
        if reclaimed == KEYS as u64 {
            break;
        }
        assert!(reclaimed < KEYS as u64, "{reclaimed} extents reclaimed for {KEYS} deaths");
        assert!(Instant::now() < deadline, "{reclaimed} of {KEYS} extents reclaimed after 20 s");
        std::thread::sleep(Duration::from_millis(50));
    }
    drop(server);
    let _ = std::fs::remove_dir_all(&dir);
}
