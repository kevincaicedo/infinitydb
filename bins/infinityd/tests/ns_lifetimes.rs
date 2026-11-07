#![allow(
    clippy::disallowed_types,
    reason = "test-only: filesystem fixtures outside cell code (ADR-0144 D5)"
)]
#![allow(
    clippy::disallowed_methods,
    reason = "test target: harness deadlines and stamps, not cell code"
)]
//! Tiered namespace lifetimes at the binary: a node holds at most 64
//! tiered namespace lifetimes (ADR-0148 D1, one node-wide grant being one
//! position on every cell), and the 65th tiered `INF.NS CREATE` is refused
//! with `-ERR tiered namespace limit reached` while no grant is retiring
//! (ADR-0172 D3), before any effect.
//!
//! Shape: a two-cell durable node, each namespace's ring at the 4 MiB floor
//! (`MEM-BUDGET 3mb` beside the default 1 MiB `MAINTAIN-SLICE`: a ring is
//! their sum rounded up to a power of two), so 65 rings reserve 260 MiB of
//! addresses on a cell and the node's reserved-address admission (256 GiB
//! by default, a share per cell) is not what refuses. Harness rules are the
//! topology test's: `--port 0`, readiness is a `DBSIZE` answer, a refusal
//! is the exit.
#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

/// Tiered namespace lifetimes a node holds (ADR-0148 A1.1's limit).
const LIFETIMES_MAX: usize = 64;

fn unique() -> u32 {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

fn data_root(tag: &str) -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/e2e-ns-lifetimes")
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
    /// `SIGTERM` (the graceful stop), then the exit status; the process is
    /// killed by `Drop` if it outlives 60 s.
    fn stop(&mut self) -> std::process::ExitStatus {
        let sent = Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status()
            .expect("kill(1)");
        assert!(sent.success(), "kill -TERM failed");
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if let Some(status) = self.child.try_wait().expect("try_wait") {
                return status;
            }
            let text = std::fs::read_to_string(&self.stderr).unwrap_or_default();
            assert!(Instant::now() < deadline, "still running 60 s after SIGTERM: {text}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

/// A two-cell durable node on `dir`, up: listening and out of `-LOADING`.
fn start(dir: &Path) -> Server {
    std::fs::create_dir_all(dir).expect("data dir");
    let stderr = dir.join(format!("stderr-{}-{}", std::process::id(), unique()));
    let mut child = Command::new(env!("CARGO_BIN_EXE_infinityd"))
        .args(["--port", "0", "--cells", "2", "--data-dir"])
        .arg(dir)
        .args(["--device-probe", "off"])
        .stdout(Stdio::null())
        .stderr(Stdio::from(std::fs::File::create(&stderr).expect("stderr file")))
        .spawn()
        .expect("spawn infinityd");
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let text = std::fs::read_to_string(&stderr).unwrap_or_default();
        if let Some(status) = child.try_wait().expect("try_wait") {
            panic!("infinityd exited {status} before serving: {text}");
        }
        if let Some(port) = announced_port(&text)
            && serving(port)
        {
            return Server { child, port, stderr };
        }
        assert!(Instant::now() < deadline, "infinityd never came up: {text}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn announced_port(stderr: &str) -> Option<u16> {
    stderr
        .lines()
        .find_map(|line| line.strip_prefix("infinityd: listening on "))
        .and_then(|port| port.trim().parse().ok())
}

/// One `DBSIZE` on a throwaway connection: an integer reply means ready.
fn serving(port: u16) -> bool {
    let Ok(mut s) = TcpStream::connect(("127.0.0.1", port)) else { return false };
    s.set_read_timeout(Some(Duration::from_secs(5))).expect("timeout");
    if s.write_all(&resp(&[b"DBSIZE"])).is_err() {
        return false;
    }
    let mut buf = [0u8; 64];
    match s.read(&mut buf) {
        Ok(n) => buf[..n].starts_with(b":"),
        Err(_) => false,
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

/// One simple-line reply to `parts`.
fn line(c: &mut TcpStream, parts: &[&[u8]]) -> String {
    c.write_all(&resp(parts)).expect("write");
    let mut reply = Vec::new();
    let mut byte = [0u8; 1];
    while !reply.ends_with(b"\r\n") {
        c.read_exact(&mut byte).expect("read");
        reply.push(byte[0]);
    }
    String::from_utf8_lossy(&reply).into_owned()
}

/// The 65th tiered `CREATE` on a two-cell node is refused at the limit,
/// with the 64 before it accepted: a node holds at most 64 tiered
/// namespace lifetimes.
#[test]
#[ignore = "a node admits tiered namespaces without a lifetime bound, so the 65th tiered CREATE \
            answers +OK; red until a node holds at most 64 tiered namespace lifetimes and the \
            next tiered CREATE is refused with the limit's typed reply"]
fn the_sixty_fifth_tiered_create_is_refused_at_the_limit() {
    let dir = data_root("limit");
    let mut server = start(&dir);
    let mut c = TcpStream::connect(("127.0.0.1", server.port)).expect("connect");
    c.set_read_timeout(Some(Duration::from_secs(60))).expect("timeout");
    let mut replies = Vec::new();
    for at in 0..=LIFETIMES_MAX {
        let name = format!("tiered-{at:02}");
        let parts: [&[u8]; 7] =
            [b"INF.NS", b"CREATE", name.as_bytes(), b"MODE", b"durable", b"MEM-BUDGET", b"3mb"];
        replies.push(line(&mut c, &parts));
    }
    drop(c);
    let status = server.stop();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(status.success(), "the node stopped gracefully: {status}");
    let refused: Vec<(usize, &String)> =
        replies[..LIFETIMES_MAX].iter().enumerate().filter(|(_, r)| *r != "+OK\r\n").collect();
    assert!(refused.is_empty(), "a CREATE below the limit was refused: {refused:?}");
    assert_eq!(
        replies[LIFETIMES_MAX], "-ERR tiered namespace limit reached\r\n",
        "the 65th tiered CREATE on a two-cell node, rings at the 4 MiB floor"
    );
}
