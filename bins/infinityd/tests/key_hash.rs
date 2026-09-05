#![allow(
    clippy::disallowed_methods,
    reason = "test target: harness deadlines and stamps, not cell code"
)]
//! ADR-0094 at the binary level — the key-hash secret's lifecycle on a
//! data directory:
//!
//! - D2: the first boot creates `key-hash.toml`, the second reads the
//!   same secret, a directory that holds data without the file (one that
//!   predates the ADR) is refused with the typed message, a malformed
//!   file is refused with the line named.
//! - D6: a secret replaced after a tiered checkpoint was placed is refused
//!   **before any cell starts** — the MANIFEST names the placing secret.
//! - D7: two simultaneous first boots on one directory yield exactly one
//!   owner; the other exits naming `LOCK`.
//! - D9: the file is `0600`; a lax mode is refused naming the `chmod`.
//!
//! Readiness is never TCP: a directory is "populated" when an
//! acknowledged DDL says so (`INF.NS CREATE … MODE durable` acks after
//! the `META` swap, ADR-0015 D3) and a checkpoint exists when `INF.CKPT
//! WAIT` acks — the review of `44527f4` found the TCP-readiness form
//! flaky. Every boot takes `--port 0` (kernel-assigned; the cell
//! announces `listening on <port>`), so parallel tests can never share a
//! port — the listener is `SO_REUSEPORT`, and a `free_port()`-then-spawn
//! harness let a *refused* boot look "up" through another test's server.
//! `--device-probe off` keeps every boot on the dev tier so no probe
//! runs; the data directories live under the workspace `target/` like
//! the device-probe test's.
#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

/// Per-process unique suffix for directories and stderr files.
fn unique() -> u32 {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

fn data_root(tag: &str) -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/e2e-key-hash")
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

/// A launched-but-not-yet-ready `infinityd`.
struct Launched {
    child: Child,
    stderr: PathBuf,
}

/// Start `infinityd` on `dir` without waiting, on a kernel-assigned port.
fn launch(dir: &Path) -> Launched {
    std::fs::create_dir_all(dir).expect("data dir");
    let stderr = dir.join(format!("stderr-{}-{}", std::process::id(), unique()));
    let child = Command::new(env!("CARGO_BIN_EXE_infinityd"))
        .args(["--port", "0", "--cells", "1", "--data-dir"])
        .arg(dir)
        .args(["--device-probe", "off"])
        .stdout(Stdio::null())
        .stderr(Stdio::from(std::fs::File::create(&stderr).expect("stderr file")))
        .spawn()
        .expect("spawn infinityd");
    Launched { child, stderr }
}

/// Wait for the exit (a refusal: exit status and stderr) or for **our
/// child's** `listening on <port>` line plus a connect on that port.
/// The exit is checked first, so a refused boot can never be mistaken
/// for a serving one. Serving means nothing about the directory —
/// callers that need it populated ack a DDL.
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

/// The port cell 0 announced (`infinityd: listening on N`).
fn announced_port(stderr: &str) -> Option<u16> {
    stderr
        .lines()
        .find_map(|line| line.strip_prefix("infinityd: listening on "))
        .and_then(|port| port.trim().parse().ok())
}

fn spawn(dir: &Path) -> Result<(Server, String), (i32, String)> {
    wait_up(launch(dir))
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
/// state). The listener is up before recovery completes (`-LOADING`
/// until `loading:0`), so a command is retried through that window —
/// the server's own readiness, never a sleep.
struct Client(TcpStream);

impl Client {
    fn connect(port: u16) -> Client {
        let c = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        c.set_read_timeout(Some(Duration::from_secs(60))).expect("timeout");
        Client(c)
    }

    /// The whole reply (a simple line, or a bulk string with its payload).
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

/// Create a durable namespace and ack it: after `+OK` the catalog swap
/// has landed (`META` exists) — the directory is populated, by contract.
fn populate(port: u16, dir: &Path) {
    Client::connect(port).ok(&[b"INF.NS", b"CREATE", b"populated", b"MODE", b"durable"]);
    assert!(dir.join("META").exists(), "the acked DDL persisted the catalog");
}

/// First boot creates the secret; the second reads it back unchanged
/// and says so (with its identity and the binding count); the file
/// carries the schema and function it claims.
#[test]
fn the_first_boot_creates_the_secret_and_later_boots_read_it() {
    let dir = data_root("lifecycle");
    let file = dir.join("key-hash.toml");
    let first = {
        let (_server, stderr) = spawn(&dir).expect("first boot");
        assert!(
            stderr.contains("key-hash secret: key-hash.toml (created at this first boot"),
            "{stderr}"
        );
        assert!(stderr.contains("; id 0x") && stderr.contains("0 manifest(s) bound"), "{stderr}");
        std::fs::read_to_string(&file).expect("the secret file exists after the first boot")
    };
    assert!(first.contains("schema = 1\n"), "{first}");
    assert!(first.contains("function = \"siphash13\"\n"), "{first}");
    assert!(first.contains("k0 = 0x") && first.contains("k1 = 0x"), "{first}");
    assert!(!dir.join("key-hash.toml.tmp").exists(), "no temp residue");
    assert!(dir.join("LOCK").exists(), "the owner lock file (ADR-0094 D7)");
    {
        let (_server, stderr) = spawn(&dir).expect("second boot");
        assert!(stderr.contains("key-hash secret: key-hash.toml (read"), "{stderr}");
    }
    assert_eq!(std::fs::read_to_string(&file).expect("still there"), first, "unchanged");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A directory that holds data without the secret predates ADR-0094:
/// refused with the typed message, exit 1, and no file written over it.
#[test]
fn a_directory_that_predates_the_secret_is_refused() {
    let dir = data_root("predates");
    // Boot once and populate through an acked DDL (the catalog) — the
    // shape of a directory written by a pre-ADR binary once the secret
    // is removed.
    {
        let (server, _) = spawn(&dir).expect("first boot");
        populate(server.port, &dir);
    }
    std::fs::remove_file(dir.join("key-hash.toml")).expect("remove the secret");
    let (code, stderr) = spawn(&dir).expect_err("a pre-ADR directory is refused");
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("predates keyed key hashing (ADR-0094)"), "{stderr}");
    assert!(!dir.join("key-hash.toml").exists(), "no secret written over existing data");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A malformed secret is a refusal with the line named, never a fresh
/// secret over existing refs.
#[test]
fn a_malformed_secret_is_refused() {
    let dir = data_root("malformed");
    std::fs::create_dir_all(&dir).expect("dir");
    write_private(&dir.join("key-hash.toml"), "schema = 1\nfunction = \"siphash13\"\nk0 = 1\n");
    let (code, stderr) = spawn(&dir).expect_err("refused");
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("key-hash.toml: `k1` missing"), "{stderr}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Write a file at mode 0600 (what a first boot does), replacing any.
fn write_private(path: &Path, text: &str) {
    use std::os::unix::fs::OpenOptionsExt;
    let _ = std::fs::remove_file(path);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .expect("create");
    file.write_all(text.as_bytes()).expect("write");
}

/// ADR-0094 D6: a valid `key-hash.toml` holding another secret, placed
/// after a tiered checkpoint, is refused before any cell starts — the
/// typed message names both identities, the MANIFEST is left untouched,
/// and restoring the original secret boots and serves the key.
#[test]
fn a_replaced_secret_is_refused_before_any_cell_starts() {
    let dir = data_root("replaced");
    let manifest = dir.join("shard-0").join("MANIFEST");
    let original = {
        let (server, _) = spawn(&dir).expect("first boot");
        let mut c = Client::connect(server.port);
        c.ok(&[
            b"INF.NS",
            b"CREATE",
            b"hot",
            b"MODE",
            b"durable",
            b"MEM-BUDGET",
            b"8mb",
            b"DISK-BUDGET",
            b"64mb",
            b"MUTABLE-FRACTION",
            b"100",
        ]);
        c.ok(&[b"INF.NS", b"USE", b"hot"]);
        for i in 0..64u32 {
            c.ok(&[b"SET", format!("k{i}").as_bytes(), b"v"]);
        }
        assert_eq!(c.call(&[b"GET", b"k7"]), b"$1\r\nv\r\n");
        c.ok(&[b"INF.CKPT", b"WAIT"]);
        assert!(manifest.exists(), "the acked checkpoint published a MANIFEST");
        std::fs::read_to_string(dir.join("key-hash.toml")).expect("secret")
    };
    let placed = std::fs::read(&manifest).expect("manifest bytes");
    // A different, perfectly valid secret.
    write_private(
        &dir.join("key-hash.toml"),
        "schema = 1\nfunction = \"siphash13\"\nk0 = 0x1111111111111111\nk1 = \
         0x2222222222222222\n",
    );
    let (code, stderr) = spawn(&dir).expect_err("a replaced secret is refused");
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("shard-0/MANIFEST names key-hash id 0x"), "{stderr}");
    assert!(stderr.contains("ADR-0094") && stderr.contains("fail-stop"), "{stderr}");
    assert!(!stderr.contains("cells, port"), "no cell started: {stderr}");
    assert_eq!(std::fs::read(&manifest).expect("manifest bytes"), placed, "MANIFEST untouched");
    // The original secret restored: the checkpoint's refs are the ones
    // this secret placed, and the key is served.
    write_private(&dir.join("key-hash.toml"), &original);
    {
        let (server, stderr) = spawn(&dir).expect("restored secret boots");
        assert!(stderr.contains("1 manifest(s) bound"), "{stderr}");
        let mut c = Client::connect(server.port);
        c.ok(&[b"INF.NS", b"USE", b"hot"]);
        assert_eq!(c.call(&[b"GET", b"k7"]), b"$1\r\nv\r\n");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// ADR-0094 D7: two simultaneous first boots on one directory — exactly
/// one owns it and serves; the other exits 1 naming the lock. Which one
/// wins is the kernel's choice, so the row asserts the pair.
#[test]
fn two_simultaneous_first_boots_yield_exactly_one_owner() {
    let dir = data_root("owner");
    std::fs::create_dir_all(&dir).expect("dir");
    let a = launch(&dir);
    let b = launch(&dir);
    let outcomes = [wait_up(a), wait_up(b)];
    let (mut served, mut refused) = (0, 0);
    for outcome in &outcomes {
        match outcome {
            Ok((_, stderr)) => {
                served += 1;
                assert!(stderr.contains("key-hash secret: key-hash.toml (created"), "{stderr}");
            }
            Err((code, stderr)) => {
                refused += 1;
                assert_eq!(*code, 1, "{stderr}");
                assert!(stderr.contains("LOCK: held by another process"), "{stderr}");
                assert!(!stderr.contains("key-hash secret"), "the loser never resolved: {stderr}");
            }
        }
    }
    assert_eq!((served, refused), (1, 1), "exactly one owner");
    let secret = std::fs::read_to_string(dir.join("key-hash.toml")).expect("one secret");
    assert!(secret.contains("k0 = 0x"), "{secret}");
    drop(outcomes);
    // The owner gone, the directory can be owned again — with that secret.
    let (_server, stderr) = spawn(&dir).expect("next owner");
    assert!(stderr.contains("key-hash secret: key-hash.toml (read"), "{stderr}");
    assert_eq!(std::fs::read_to_string(dir.join("key-hash.toml")).expect("same"), secret);
    let _ = std::fs::remove_dir_all(&dir);
}

/// ADR-0094 D9: the first boot's file is `0600`; a lax mode is refused
/// naming the `chmod`; private again, it boots.
#[test]
fn the_secret_is_private_and_a_lax_mode_is_refused() {
    use std::os::unix::fs::PermissionsExt;
    let dir = data_root("private");
    let file = dir.join("key-hash.toml");
    {
        let (_server, _) = spawn(&dir).expect("first boot");
    }
    let mode = std::fs::metadata(&file).expect("meta").permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "{mode:04o}");
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).expect("chmod");
    let (code, stderr) = spawn(&dir).expect_err("a world-readable secret is refused");
    assert_eq!(code, 1, "{stderr}");
    assert!(
        stderr.contains("key-hash.toml is mode 0644") && stderr.contains("chmod 600"),
        "{stderr}"
    );
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    let (_server, stderr) = spawn(&dir).expect("private again");
    assert!(stderr.contains("key-hash secret: key-hash.toml (read"), "{stderr}");
    let _ = std::fs::remove_dir_all(&dir);
}
