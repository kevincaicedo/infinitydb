#![allow(
    clippy::disallowed_methods,
    reason = "test target: harness deadlines and stamps, not cell code"
)]
//! ADR-0095 at the binary level — the cell-topology binding on a data
//! directory (full-codebase review of 2026-08-30, C8 / F-L14-03):
//!
//! - K4: a directory populated at `--cells 2` under `FSYNC always` and
//!   killed refuses to open at any other count — typed, both numbers
//!   named, exit 1, before any cell starts — and serves every acked key
//!   back at the recorded count. Before this ADR the mismatched boot
//!   reported a clean recovery with most acked keys unreachable.
//! - K5: a pre-ADR directory (the file removed) adopts by deriving the
//!   count from its shard set — the matching count boots and stamps,
//!   any other refuses naming the derived count.
//!
//! Harness rules are the key-hash test's: `--port 0` (kernel-assigned,
//! announced on stderr), `--device-probe off` (dev tier, no probe),
//! readiness by acked commands through the `-LOADING` window — never a
//! sleep-for-TCP.
#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

fn unique() -> u32 {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

fn data_root(tag: &str) -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/e2e-topology")
        .join(format!("{tag}-{}-{}", std::process::id(), unique()));
    let _ = std::fs::remove_dir_all(&root);
    root
}

#[derive(Debug)]
struct Server {
    child: Child,
    port: u16,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Launched {
    child: Child,
    stderr: PathBuf,
}

/// Start `infinityd` on `dir` at `cells` without waiting.
fn launch(dir: &Path, cells: u16) -> Launched {
    std::fs::create_dir_all(dir).expect("data dir");
    let stderr = dir.join(format!("stderr-{}-{}", std::process::id(), unique()));
    let child = Command::new(env!("CARGO_BIN_EXE_infinityd"))
        .args(["--port", "0", "--cells", &cells.to_string(), "--data-dir"])
        .arg(dir)
        .args(["--device-probe", "off"])
        .stdout(Stdio::null())
        .stderr(Stdio::from(std::fs::File::create(&stderr).expect("stderr file")))
        .spawn()
        .expect("spawn infinityd");
    Launched { child, stderr }
}

fn wait_up(mut launched: Launched) -> Result<(Server, String), (i32, String)> {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(status) = launched.child.try_wait().expect("try_wait") {
            let text = std::fs::read_to_string(&launched.stderr).unwrap_or_default();
            return Err((status.code().unwrap_or(-1), text));
        }
        let text = std::fs::read_to_string(&launched.stderr).unwrap_or_default();
        if let Some(port) = announced_port(&text)
            && TcpStream::connect(("127.0.0.1", port)).is_ok()
        {
            return Ok((Server { child: launched.child, port }, text));
        }
        assert!(Instant::now() < deadline, "infinityd never came up: {text}");
        #[allow(clippy::disallowed_methods)] // test harness, not cell code
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn announced_port(stderr: &str) -> Option<u16> {
    stderr
        .lines()
        .find_map(|line| line.strip_prefix("infinityd: listening on "))
        .and_then(|port| port.trim().parse().ok())
}

fn spawn(dir: &Path, cells: u16) -> Result<(Server, String), (i32, String)> {
    wait_up(launch(dir, cells))
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

/// One connection, kept for a session (`INF.NS USE` is per-connection
/// state); `-LOADING` is retried through the recovery window.
struct Client(TcpStream);

impl Client {
    fn connect(port: u16) -> Client {
        let c = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        c.set_read_timeout(Some(Duration::from_secs(60))).expect("timeout");
        Client(c)
    }

    fn call(&mut self, parts: &[&[u8]]) -> Vec<u8> {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let reply = self.call_once(parts);
            if !reply.starts_with(b"-LOADING") {
                return reply;
            }
            assert!(Instant::now() < deadline, "still loading after 60 s");
            #[allow(clippy::disallowed_methods)] // test harness, not cell code
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn call_once(&mut self, parts: &[&[u8]]) -> Vec<u8> {
        self.0.write_all(&resp(parts)).expect("write");
        let mut reply = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            self.0.read_exact(&mut byte).expect("read");
            reply.push(byte[0]);
            if reply.ends_with(b"\r\n") {
                break;
            }
        }
        if reply[0] == b'$' && !reply.starts_with(b"$-1") {
            let len: usize = std::str::from_utf8(&reply[1..reply.len() - 2])
                .expect("utf8")
                .parse()
                .expect("len");
            let mut payload = vec![0u8; len + 2];
            self.0.read_exact(&mut payload).expect("bulk");
            reply.extend_from_slice(&payload);
        }
        reply
    }

    fn ok(&mut self, parts: &[&[u8]]) {
        let reply = self.call(parts);
        assert_eq!(reply, b"+OK\r\n", "{}", String::from_utf8_lossy(&reply));
    }
}

/// ADR-0095 K4 — the C8 reproduction, inverted: acked `always` data at
/// `--cells 2` survives; a reopen at 1 or 4 is the typed refusal (exit
/// 1, both numbers named, no recovery started); the recorded count
/// serves every key back.
#[test]
fn a_reopen_at_a_different_cell_count_is_refused_and_the_recorded_one_serves() {
    let dir = data_root("mismatch");
    {
        let (server, stderr) = spawn(&dir, 2).expect("first boot at 2 cells");
        assert!(stderr.contains("topology: 2 cells (stamped at this first boot"), "{stderr}");
        let mut c = Client::connect(server.port);
        c.ok(&[b"INF.NS", b"CREATE", b"led", b"MODE", b"durable", b"FSYNC", b"always"]);
        c.ok(&[b"INF.NS", b"USE", b"led"]);
        for i in 0..20u32 {
            // `always`: each +OK is fsync-gated — the ack is durability.
            c.ok(&[b"SET", format!("k:{i}").as_bytes(), format!("v:{i}").as_bytes()]);
        }
        // Server dropped here = SIGKILL (the C8 shape).
    }
    let file = dir.join("topology.toml");
    let stamped = std::fs::read_to_string(&file).expect("stamped at first boot");
    for wrong in [1u16, 4] {
        let (code, stderr) = spawn(&dir, wrong).expect_err("mismatched reopen refused");
        assert_eq!(code, 1, "{stderr}");
        assert!(
            stderr.contains("records a 2-cell topology")
                && stderr.contains(&format!("asked for {wrong}"))
                && stderr.contains("ADR-0095"),
            "{stderr}"
        );
        assert!(!stderr.contains("recovery complete"), "refusal precedes any cell: {stderr}");
        assert_eq!(std::fs::read_to_string(&file).expect("kept"), stamped, "file untouched");
    }
    {
        let (server, stderr) = spawn(&dir, 2).expect("the recorded count boots");
        assert!(stderr.contains("topology: 2 cells (read"), "{stderr}");
        let mut c = Client::connect(server.port);
        c.ok(&[b"INF.NS", b"USE", b"led"]);
        for i in 0..20u32 {
            let reply = c.call(&[b"GET", format!("k:{i}").as_bytes()]);
            let value = format!("v:{i}");
            let want = format!("${}\r\n{value}\r\n", value.len()).into_bytes();
            assert_eq!(reply, want, "k:{i}: {}", String::from_utf8_lossy(&reply));
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// ADR-0095 K5 — a pre-ADR directory (the file removed): the shard set
/// derives the count; the matching `--cells` adopts and stamps, any
/// other refuses naming the derived count, and the acked data serves
/// after adoption.
#[test]
fn a_pre_adr_directory_adopts_at_the_derived_count_and_refuses_others() {
    let dir = data_root("adopt");
    {
        let (server, _) = spawn(&dir, 2).expect("first boot at 2 cells");
        let mut c = Client::connect(server.port);
        c.ok(&[b"INF.NS", b"CREATE", b"led", b"MODE", b"durable", b"FSYNC", b"always"]);
        c.ok(&[b"INF.NS", b"USE", b"led"]);
        c.ok(&[b"SET", b"adopted", b"survives"]);
    }
    let file = dir.join("topology.toml");
    std::fs::remove_file(&file).expect("make the directory pre-ADR");
    let (code, stderr) = spawn(&dir, 4).expect_err("derived mismatch refused");
    assert_eq!(code, 1, "{stderr}");
    assert!(
        stderr.contains("holds 2 shard directories")
            && stderr.contains("asked for 4")
            && stderr.contains("--cells 2"),
        "{stderr}"
    );
    assert!(!file.exists(), "a refusal stamps nothing");
    {
        let (server, stderr) = spawn(&dir, 2).expect("the derived count adopts");
        assert!(stderr.contains("derived from the shard set and stamped"), "{stderr}");
        let mut c = Client::connect(server.port);
        c.ok(&[b"INF.NS", b"USE", b"led"]);
        assert_eq!(c.call(&[b"GET", b"adopted"]), b"$8\r\nsurvives\r\n");
    }
    assert!(file.exists(), "adoption stamped the topology");
    {
        let (_server, stderr) = spawn(&dir, 2).expect("stamped: later boots read");
        assert!(stderr.contains("topology: 2 cells (read"), "{stderr}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}
