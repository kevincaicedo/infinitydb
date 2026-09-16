//! Review 2026-08-30 F-L15-08 (batch 51, ADR-0124): `SIGTERM` is a
//! graceful stop. Black-box over the real binary — these compile and run
//! against the pre-fix tree, where the child dies by signal 15 with no
//! exit code and the next boot replays every frame.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)] // harness process, not cell code

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

/// One spawning test at a time (batch 69): macOS sets `CLOEXEC` after
/// `socket(2)`, so a sibling test's `posix_spawn` in that window inherits
/// this test's client socket and its port picker — the client then never
/// sees the server's FIN, and the picked port is a ghost's.
static SPAWN_LOCK: Mutex<()> = Mutex::new(());

fn unique() -> u32 {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

fn data_root(tag: &str) -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/e2e-graceful-stop")
        .join(format!("{tag}-{}-{}", std::process::id(), unique()));
    let _ = std::fs::remove_dir_all(&root);
    root
}

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
    /// `--port 0` gives every cell its own kernel-assigned port and only
    /// cell 0's is announced, so a multi-cell node takes a probed free
    /// port (the compat harness's pattern).
    fn spawn(dir: &Path, cells: &str, extra: &[&str]) -> Server {
        std::fs::create_dir_all(dir).expect("data dir");
        let stderr = dir.join(format!("stderr-{}-{}", std::process::id(), unique()));
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .expect("probe bind")
            .local_addr()
            .expect("addr")
            .port()
            .to_string();
        let child = Command::new(env!("CARGO_BIN_EXE_infinityd"))
            .args(["--port", &port, "--cells", cells, "--data-dir"])
            .arg(dir)
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

    fn text(&self) -> String {
        std::fs::read_to_string(&self.stderr).unwrap_or_default()
    }

    /// `SIGTERM`, then the exit status within `wait` — `None` if the
    /// process outlived it (it is killed by `Drop`).
    fn sigterm_and_wait(&mut self, wait: Duration) -> Option<std::process::ExitStatus> {
        let status = Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status()
            .expect("kill(1)");
        assert!(status.success(), "kill -TERM failed");
        let deadline = Instant::now() + wait;
        loop {
            if let Some(status) = self.child.try_wait().expect("try_wait") {
                return Some(status);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

fn resp(parts: &[&[u8]]) -> Vec<u8> {
    let mut wire = format!("*{}\r\n", parts.len()).into_bytes();
    for p in parts {
        wire.extend_from_slice(format!("${}\r\n", p.len()).as_bytes());
        wire.extend_from_slice(p);
        wire.extend_from_slice(b"\r\n");
    }
    wire
}

struct Client(TcpStream);

impl Client {
    fn connect(port: u16) -> Client {
        // macOS answers a connect right behind a closed one on the same
        // pair with a transient `EADDRNOTAVAIL` (batch 69): retry briefly.
        let deadline = Instant::now() + Duration::from_secs(5);
        let c = loop {
            match TcpStream::connect(("127.0.0.1", port)) {
                Ok(c) => break c,
                Err(e) if Instant::now() < deadline => {
                    let _ = e;
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(e) => panic!("connect: {e:?}"),
            }
        };
        c.set_read_timeout(Some(Duration::from_secs(60))).expect("timeout");
        c.set_nodelay(true).expect("nodelay");
        Client(c)
    }

    fn call(&mut self, parts: &[&[u8]]) -> Vec<u8> {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            self.0.write_all(&resp(parts)).expect("write");
            let reply = self.read_reply();
            if !reply.starts_with(b"-LOADING") {
                return reply;
            }
            assert!(Instant::now() < deadline, "still loading after 60 s");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn read_line(&mut self) -> Vec<u8> {
        let mut line = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            self.0.read_exact(&mut byte).expect("read");
            line.push(byte[0]);
            if line.ends_with(b"\r\n") {
                return line;
            }
        }
    }

    fn read_reply(&mut self) -> Vec<u8> {
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

    fn ok(&mut self, parts: &[&[u8]]) {
        let reply = self.call(parts);
        assert_eq!(reply, b"+OK\r\n", "{}", String::from_utf8_lossy(&reply));
    }

    fn info_field(&mut self, section: &[u8], field: &str) -> String {
        let text = self.call(&[b"INFO", section]);
        let text = String::from_utf8_lossy(&text).into_owned();
        text.lines()
            .find_map(|line| line.strip_prefix(&format!("{field}:")))
            .unwrap_or_else(|| panic!("no {field} in {text}"))
            .trim()
            .to_string()
    }
}

/// `INFO` is cell-scoped for `# Persistence`: connect until every cell
/// of the node has answered, and return `field` per cell (every tier
/// since batch 70: the macOS accept hand-off, ADR-0128).
fn per_cell_field(port: u16, cells: usize, section: &[u8], field: &str) -> Vec<(u16, String)> {
    let mut seen = std::collections::BTreeMap::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    while seen.len() < cells {
        let mut c = Client::connect(port);
        let cell: u16 = c.info_field(b"server", "cell").parse().expect("cell id");
        let value = c.info_field(section, field);
        seen.insert(cell, value);
        assert!(Instant::now() < deadline, "never reached every cell: {seen:?}");
    }
    seen.into_iter().collect()
}

const N: usize = 4_000;

/// Pre-fix: the child dies by signal 15 — no exit code, no `clean stop`
/// line. Post-fix a client with a pipeline still arriving is answered up
/// to the stop (every executed command is answered; input after the mark
/// is dropped unexecuted — QUIT's rule, ADR-0124 D2) and then sees a
/// clean EOF, not a reset.
#[test]
fn sigterm_drains_and_exits_zero() {
    let _one_at_a_time = SPAWN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = data_root("exit");
    let mut server = Server::spawn(&dir, "2", &[]);
    let mut c = Client::connect(server.port);
    c.ok(&[b"INF.NS", b"CREATE", b"esec", b"MODE", b"durable", b"FSYNC", b"everysec"]);
    c.ok(&[b"INF.NS", b"USE", b"esec"]);
    let mut wire = Vec::new();
    for i in 0..N {
        wire.extend_from_slice(&resp(&[b"SET", format!("k{i}").as_bytes(), b"v"]));
    }
    c.0.write_all(&wire).expect("pipeline");
    let status = server.sigterm_and_wait(Duration::from_secs(20));
    let text = server.text();
    let status = status.unwrap_or_else(|| panic!("still running 20 s after SIGTERM: {text}"));
    assert_eq!(status.code(), Some(0), "not a clean exit: {status} — {text}");
    assert!(text.contains("infinityd: clean stop"), "no clean-stop line: {text}");
    // A prefix of the pipeline's replies, every one `+OK`, then a clean
    // EOF (no reset).
    let mut rest = Vec::new();
    let read = c.0.read_to_end(&mut rest);
    assert!(read.is_ok(), "the close was a reset, not a FIN: {read:?}");
    let lines: Vec<&[u8]> = rest.split(|&b| b == b'\n').filter(|l| !l.is_empty()).collect();
    assert!(!lines.is_empty(), "no reply reached the client");
    assert!(lines.len() <= N, "more replies than commands: {}", lines.len());
    assert!(
        lines.iter().all(|l| *l == b"+OK\r"),
        "a reply is not +OK: {:?}",
        &rest[..64.min(rest.len())]
    );
}

/// Pre-fix: the everysec keys survive only because a process death keeps
/// the page cache, and the next boot replays every frame (`recover_
/// replay_frames > 0` on every cell) — there is no stop checkpoint.
#[test]
fn sigterm_keeps_every_acked_write_and_the_next_boot_replays_nothing() {
    let _one_at_a_time = SPAWN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = data_root("replay");
    let mut server = Server::spawn(&dir, "2", &[]);
    let mut c = Client::connect(server.port);
    c.ok(&[b"INF.NS", b"CREATE", b"esec", b"MODE", b"durable", b"FSYNC", b"everysec"]);
    c.ok(&[b"INF.NS", b"USE", b"esec"]);
    let mut wire = Vec::new();
    for i in 0..N {
        wire.extend_from_slice(&resp(&[b"SET", format!("k{i}").as_bytes(), b"v"]));
    }
    c.0.write_all(&wire).expect("pipeline");
    for _ in 0..N {
        assert_eq!(c.read_reply(), b"+OK\r\n");
    }
    drop(c);
    let status = server.sigterm_and_wait(Duration::from_secs(20));
    let text = server.text();
    let status = status.unwrap_or_else(|| panic!("still running 20 s after SIGTERM: {text}"));
    assert_eq!(status.code(), Some(0), "not a clean exit: {status} — {text}");
    drop(server);

    let server = Server::spawn(&dir, "2", &[]);
    let mut c = Client::connect(server.port);
    c.ok(&[b"INF.NS", b"USE", b"esec"]);
    let dbsize = c.call(&[b"DBSIZE"]);
    assert_eq!(dbsize, format!(":{N}\r\n").as_bytes(), "acked writes lost across the stop");
    // Pre-fix: no checkpoint at all (`recover_ckpt_bytes:0` — 4 000 small
    // SETs never reach the bytes trigger) and the whole tail replays.
    // Every cell, on every tier (batch 69 had this Linux-only: the macOS
    // listener group handed every connection to one cell — lane L11 N19,
    // fixed by the accept hand-off in batch 70).
    let ckpt = per_cell_field(server.port, 2, b"persistence", "recover_ckpt_bytes");
    assert!(ckpt.iter().all(|(_, v)| v != "0"), "no stop checkpoint was loaded: {ckpt:?}");
    let replayed = per_cell_field(server.port, 2, b"persistence", "recover_replay_records");
    assert!(replayed.iter().all(|(_, v)| v == "0"), "a clean stop still paid replay: {replayed:?}");
}

/// `--shutdown-checkpoint off`: still a clean exit, still every acked
/// write, and the next boot replays the tail (the operator chose it).
#[test]
fn shutdown_checkpoint_off_is_a_clean_exit_that_replays() {
    let _one_at_a_time = SPAWN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = data_root("nockpt");
    let mut server = Server::spawn(&dir, "1", &["--shutdown-checkpoint", "off"]);
    let mut c = Client::connect(server.port);
    c.ok(&[b"INF.NS", b"CREATE", b"esec", b"MODE", b"durable", b"FSYNC", b"everysec"]);
    c.ok(&[b"INF.NS", b"USE", b"esec"]);
    for i in 0..64 {
        c.ok(&[b"SET", format!("k{i}").as_bytes(), b"v"]);
    }
    drop(c);
    let status = server.sigterm_and_wait(Duration::from_secs(20)).expect("exited");
    assert_eq!(status.code(), Some(0), "{}", server.text());
    drop(server);
    let server = Server::spawn(&dir, "1", &[]);
    let mut c = Client::connect(server.port);
    c.ok(&[b"INF.NS", b"USE", b"esec"]);
    assert_eq!(c.call(&[b"DBSIZE"]), b":64\r\n");
    assert_eq!(c.info_field(b"persistence", "recover_ckpt_bytes"), "0");
    assert_eq!(c.info_field(b"persistence", "recover_replay_records"), "64");
}

/// Batch 71 (L11, Linux/io_uring): a close over unread input, measured.
/// One write pipelines `N` SETs, a `QUIT`, then `N` more SETs the server
/// never reads: every reply up to and including QUIT's `+OK` reaches the
/// client, whatever the kernel does with the tail (Linux answers a close
/// over unread receive data with RST, exactly as Redis 8.0.5 does on the
/// same probe; the driver having drained the pipeline first ends in a
/// FIN instead). The pool is four 4 KiB buffers, so the pipeline runs
/// well past it and multishot recv pauses and resumes underneath.
#[test]
fn quit_past_the_pool_delivers_every_earlier_reply() {
    let _one_at_a_time = SPAWN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = data_root("quit-pool");
    let server = Server::spawn(&dir, "1", &["--buffers", "4", "--buf-size", "4096"]);
    let mut c = Client::connect(server.port);
    assert_eq!(c.call(&[b"PING"]), b"+PONG\r\n");
    let mut wire = Vec::new();
    for i in 0..N {
        wire.extend_from_slice(&resp(&[b"SET", format!("k{i}").as_bytes(), b"v"]));
    }
    wire.extend_from_slice(&resp(&[b"QUIT"]));
    for i in 0..N {
        wire.extend_from_slice(&resp(&[b"SET", format!("z{i}").as_bytes(), b"v"]));
    }
    c.0.write_all(&wire).expect("pipeline");
    let mut rest = Vec::new();
    let tail = c.0.read_to_end(&mut rest);
    let replies = rest.split(|&b| b == b'\n').filter(|l| *l == b"+OK\r").count();
    assert_eq!(replies, N + 1, "replies before the close (tail {tail:?})");
    assert!(
        tail.is_ok()
            || tail.as_ref().is_err_and(|e| e.kind() == std::io::ErrorKind::ConnectionReset),
        "the tail is neither EOF nor a reset: {tail:?}"
    );
}
