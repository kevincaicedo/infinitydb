//! The binary arm of the cold read's budgeted delivery (ADR-0181 R1, R2): a
//! device receipt marks its read's window ready and visits none of its
//! members; the loop's `before_execute` delivers at most a turn's 64 of them
//! and the rest on later turns, so one read never stalls the loop. A one-cell
//! node demotes a tiered namespace's small values to its tier files, then
//! takes one `GET` of an adjacent cold key on each of 128 connections, sent
//! while the process is stopped so they arrive at one iteration and merge
//! into shared device reads: a connection's pipeline runs its commands one
//! after another, so only separate connections read at once. Every reply is
//! its value, and `INFO tiering` reads fewer device reads than requests,
//! every request's member visited once, a turn's delivery at most 64 visits
//! and nothing left to deliver.
//! Linux: the process is stopped and continued with `kill(1)`.
#![cfg(target_os = "linux")]
#![allow(clippy::disallowed_methods, clippy::disallowed_types)] // harness process, not cell code

// The writes fill the tiered window of its 3 MiB MEM-BUDGET four times over
// at one pipelined writer, faster than a flush round confirms, so a write can
// wait on flush progress: the namespace states a `TAIL-STALL-TIMEOUT` that
// the writer's batches never reach. The cold reads park on the device, never
// on a stall.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Keys written, each a small value: about 13 MB, four times the
/// namespace's budget, so the first ones age to the tier and its flush
/// confirms them.
const KEYS: usize = 30_000;
/// A value: small, so a 16 KiB device read covers about 37 adjacent keys.
const VALUE_LEN: usize = 400;
/// The `SET`s pipelined per round trip while the keys are written.
const SET_BATCH: usize = 500;
/// The readers, each sending one `GET` of the next cold key.
const READERS: usize = 128;
/// A turn's delivery allowance, `COLD_DELIVERIES_PER_TURN`.
const DELIVERIES_PER_TURN: u64 = 64;
/// How long the demotion and the gauges' flush may take: slack by orders of
/// magnitude over a few MiB written and a few iterations.
const DEADLINE: Duration = Duration::from_secs(60);
/// The namespace's tail-stall bound, ms: a write waits at most this long on
/// flush progress, far past what one batch of the writer waits.
const TAIL_STALL_TIMEOUT_MS: &[u8] = b"60000";

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
    /// One cell, no device probe.
    fn spawn(dir: &Path) -> Server {
        let stderr = dir.join("stderr");
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .expect("probe bind")
            .local_addr()
            .expect("addr")
            .port()
            .to_string();
        let child = Command::new(env!("CARGO_BIN_EXE_infinityd"))
            .args(["--port", &port, "--cells", "1", "--device-probe", "off", "--data-dir"])
            .arg(dir)
            .stdout(Stdio::null())
            .stderr(Stdio::from(std::fs::File::create(&stderr).expect("stderr file")))
            .spawn()
            .expect("spawn infinityd");
        let mut server = Server { child, port: 0, stderr };
        let deadline = Instant::now() + DEADLINE;
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

fn key(i: usize) -> Vec<u8> {
    format!("c:{i:05}").into_bytes()
}

/// Key `i`'s value: its index spelled into every byte's neighbourhood, so a
/// value served from another key's range reads wrong.
fn value(i: usize) -> Vec<u8> {
    format!("{i:05}-").into_bytes().repeat(VALUE_LEN / 6 + 1)[..VALUE_LEN].to_vec()
}

/// `GET`'s reply for key `i`.
fn bulk(i: usize) -> Vec<u8> {
    let mut want = format!("${VALUE_LEN}\r\n").into_bytes();
    want.extend_from_slice(&value(i));
    want.extend_from_slice(b"\r\n");
    want
}

/// Writes every key, pipelined, then waits until the tier holds more than
/// the namespace's budget and the window gave pages back: the first keys
/// are cold.
fn write_and_demote(client: &mut Client) {
    for batch in (0..KEYS).collect::<Vec<_>>().chunks(SET_BATCH) {
        let mut wire = Vec::new();
        for &i in batch {
            wire.extend_from_slice(&Client::wire(&[b"SET", &key(i), &value(i)]));
        }
        client.0.write_all(&wire).expect("write");
        for _ in batch {
            assert_eq!(client.reply(), b"+OK\r\n");
        }
    }
    let deadline = Instant::now() + DEADLINE;
    loop {
        let tiering = client.info("tiering");
        if field(&tiering, "tiering_flush_confirmed_bytes") > 3 << 20
            && field(&tiering, "tiering_region_decommit_pages") > 0
        {
            return;
        }
        assert!(Instant::now() < deadline, "demotion never made a cold working set: {tiering}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_cold_burst_is_delivered_a_turn_at_a_time() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/e2e-cold-delivery")
        .join(std::process::id().to_string());
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("data dir");
    let server = Server::spawn(&dir);
    let mut setup = Client::connect(server.port);
    setup.ok(&[
        b"INF.NS",
        b"CREATE",
        b"cd",
        b"MODE",
        b"durable",
        b"FSYNC",
        b"everysec",
        b"MEM-BUDGET",
        b"3mb",
        b"TAIL-STALL-TIMEOUT",
        TAIL_STALL_TIMEOUT_MS,
    ]);
    setup.ok(&[b"INF.NS", b"USE", b"cd"]);
    write_and_demote(&mut setup);
    let before = setup.info("tiering");
    let mut readers: Vec<Client> = (0..READERS)
        .map(|_| {
            let mut reader = Client::connect(server.port);
            reader.ok(&[b"INF.NS", b"USE", b"cd"]);
            reader
        })
        .collect();
    // Stopped, the node reads nothing: the readers' `GET`s wait in the
    // socket buffers and arrive at one iteration once it runs again, so the
    // adjacent keys' requests queue together and merge.
    assert!(signal(server.child.id(), "-STOP"), "SIGSTOP");
    for (i, reader) in readers.iter_mut().enumerate() {
        reader.0.write_all(&Client::wire(&[b"GET", &key(i)])).expect("write");
    }
    assert!(signal(server.child.id(), "-CONT"), "SIGCONT");
    for (i, reader) in readers.iter_mut().enumerate() {
        let reply = reader.reply();
        assert!(reply == bulk(i), "GET c:{i:05} answered {}", String::from_utf8_lossy(&reply));
    }
    // The gauges are flushed by the tiered MAINTAIN: read until the
    // delivery has caught up with every request.
    let gets = READERS as u64;
    let deadline = Instant::now() + DEADLINE;
    let after = loop {
        let tiering = setup.info("tiering");
        let enqueued =
            field(&tiering, "cold_reads_enqueued") - field(&before, "cold_reads_enqueued");
        let work = field(&tiering, "cold_delivery_work") - field(&before, "cold_delivery_work");
        if work == enqueued && field(&tiering, "cold_delivery_pending_members") == 0 {
            break tiering;
        }
        assert!(Instant::now() < deadline, "delivery never caught up: {tiering}");
        std::thread::sleep(Duration::from_millis(20));
    };
    let delta = |name: &str| field(&after, name) - field(&before, name);
    let enqueued = delta("cold_reads_enqueued");
    assert!(enqueued >= gets, "{enqueued} cold requests for {gets} GETs: the keys were not cold");
    let issued = delta("cold_reads_issued");
    assert!(issued < enqueued, "{enqueued} cold requests took {issued} device reads: none merged");
    let turn_max = field(&after, "cold_delivery_work_turn_max");
    let work = delta("cold_delivery_work");
    eprintln!(
        "cold_delivery: {gets} GETs, {enqueued} cold requests, {issued} device reads, {work} \
         member visits, at most {turn_max} in one turn"
    );
    assert!(turn_max <= DELIVERIES_PER_TURN, "a turn visited {turn_max} members");
    assert!(turn_max > 0, "engagement: no turn delivered");
    assert_eq!(field(&after, "cold_delivery_pending_members"), 0, "every member delivered");
    assert_eq!(field(&after, "cold_receipt_foreign"), 0, "every receipt named its read");
    assert_eq!(field(&after, "cold_link_stale"), 0);
    drop(readers);
    drop(setup);
    drop(server);
    let _ = std::fs::remove_dir_all(&dir);
}
