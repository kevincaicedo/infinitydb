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
//! (ADR-0172 D3), before any effect. A tiered `CREATE` refused after its
//! catalog swap leaves no namespace behind: once `META` names the
//! definition, an origin whose store refuses it withdraws the definition
//! durably before the refusal is answered (ADR-0148 D5), so no boot after
//! a cut serves it.
//!
//! A boot whose catalog names more live tiered definitions than the node
//! holds lifetimes is refused before any cell, with a typed error and no
//! migration (ADR-0148 D5); at or below the limit it serves, and drop
//! tombstones with their residue take no lifetime. A catalog whose
//! counter is not past a live id is refused the same way.
//!
//! Shapes: the limit test runs a two-cell durable node, each namespace's
//! ring at the 4 MiB floor (`MEM-BUDGET 3mb` beside the default 1 MiB
//! `MAINTAIN-SLICE`: a ring is their sum rounded up to a power of two), so
//! 65 rings reserve 260 MiB of addresses on a cell and the node's
//! reserved-address admission (256 GiB by default, a share per cell) is not
//! what refuses. The refusal-after-swap test runs a two-cell node under a
//! 4 GiB address-space cap, so a 4 GiB ring passes that admission and
//! cannot be mapped. Harness rules are the topology test's: `--port 0`,
//! readiness is a `DBSIZE` answer, a refusal is the exit.
#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

/// Tiered namespace lifetimes a node holds (ADR-0148 A1.1's limit).
const LIFETIMES_MAX: usize = 64;

/// A child's address-space cap (`ulimit -v`, KiB), 4 GiB: a ring of 4 GiB
/// cannot be mapped beside the process's own mappings under it.
const CHILD_AS_CAP_KIB: u64 = 4 * 1024 * 1024;

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

    /// `SIGKILL`, a power cut's stand-in: nothing of the node runs after
    /// the signal, so no shutdown path persists anything.
    fn cut(&mut self) {
        self.child.kill().expect("SIGKILL");
        self.child.wait().expect("wait");
    }
}

/// A two-cell durable node on `dir`, up: listening and out of `-LOADING`.
fn start(dir: &Path) -> Server {
    spawn(dir, Command::new(env!("CARGO_BIN_EXE_infinityd")))
}

/// [`start`] under an address-space cap of `cap_kib` KiB, so the OS
/// refuses a mapping the cap cannot hold.
fn start_capped(dir: &Path, cap_kib: u64) -> Server {
    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg(format!("ulimit -v {cap_kib} && exec \"$0\" \"$@\""))
        .arg(env!("CARGO_BIN_EXE_infinityd"));
    spawn(dir, command)
}

fn spawn(dir: &Path, mut command: Command) -> Server {
    std::fs::create_dir_all(dir).expect("data dir");
    let stderr = dir.join(format!("stderr-{}-{}", std::process::id(), unique()));
    let mut child = command
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

/// A tiered `CREATE` refused after its catalog swap is not served after a
/// cut. Under the 4 GiB cap, `MEM-BUDGET 3gb` passes every check before
/// the swap (the reserved-address admission is arithmetic: 4 GiB of a
/// cell's 128 GiB share), the catalog writer accepts it and `META` names
/// it, and the origin's ring (3 GiB plus the 1 MiB slice, rounded up to
/// 4 GiB) cannot be mapped, so the origin's store refuses it. The node is
/// killed with no later DDL, so no later catalog write drops the
/// definition, and rebooted uncapped. There the same `CREATE` of another
/// name answers `+OK`: the capped refusal was the reservation's, after the
/// swap, not a check's before it. The test reads the order's consequence
/// after a cut; the order itself, the withdrawal durable before the answer,
/// is read without timing by `inf-server`'s `node_e2e`
/// `an_origin_refused_create_leaves_no_meta_naming_it_once_answered`.
#[test]
fn a_tiered_create_refused_after_its_catalog_swap_is_not_served_after_a_cut() {
    const NOT_FOUND: &str = "-ERR namespace 'refused' not found\r\n";
    let create = |name: &'static [u8]| -> [&'static [u8]; 7] {
        [b"INF.NS", b"CREATE", name, b"MODE", b"durable", b"MEM-BUDGET", b"3gb"]
    };
    let dir = data_root("refused");
    let mut capped = start_capped(&dir, CHILD_AS_CAP_KIB);
    let mut c = TcpStream::connect(("127.0.0.1", capped.port)).expect("connect");
    c.set_read_timeout(Some(Duration::from_secs(60))).expect("timeout");
    let refusal = line(&mut c, &create(b"refused"));
    let before_cut = line(&mut c, &[b"INF.NS", b"USE", b"refused"]);
    drop(c);
    capped.cut();
    drop(capped);
    let mut server = start(&dir);
    let mut c = TcpStream::connect(("127.0.0.1", server.port)).expect("connect");
    c.set_read_timeout(Some(Duration::from_secs(60))).expect("timeout");
    let after_cut = line(&mut c, &[b"INF.NS", b"USE", b"refused"]);
    let uncapped = line(&mut c, &create(b"uncapped"));
    drop(c);
    let status = server.stop();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(status.success(), "the rebooted node stopped gracefully: {status}");
    assert_eq!(
        refusal, "-ERR MEM-BUDGET + MAINTAIN-SLICE has no representable ring reservation\r\n",
        "engagement: the capped node refused the CREATE at its ring reservation"
    );
    assert_eq!(uncapped, "+OK\r\n", "engagement: the same CREATE passes every check uncapped");
    assert_eq!(before_cut, NOT_FOUND, "the refused namespace is served before the cut");
    assert_eq!(after_cut, NOT_FOUND, "the refused namespace is served after a cut and a reboot");
}

/// A durable tiered definition of `id` at the ring floor, as `INF.NS
/// CREATE tiered-<id> MODE durable MEM-BUDGET 3mb` writes it.
fn tiered_definition(id: u32) -> inf_store::NsSpec {
    inf_store::NsSpec {
        id: inf_store::NsId(id),
        name: format!("tiered-{id}").into_bytes(),
        mode: inf_store::NsMode::Durable,
        fsync: None,
        policy: None,
        maxmemory: None,
        tier: Some(inf_store::TierSpec::for_budget(3 << 20)),
    }
}

/// Replaces `dir`'s `META` with a catalog of `live` live tiered definitions
/// (ids from 16) and two drop tombstones above them, each tombstone's
/// directory holding a tier file's residue. Returns the last live name.
fn seed_tiered_catalog(dir: &Path, live: u32) -> String {
    let first = inf_store::FIRST_NAMED_NS_ID;
    let dropped = vec![first + live, first + live + 1];
    for id in &dropped {
        let cold = dir.join(format!("shard-0/ns-{id}/cold"));
        std::fs::create_dir_all(&cold).expect("residue directory");
        std::fs::write(cold.join("tier-000000.itier"), b"residue").expect("residue file");
    }
    let catalog = inf_store::NsCatalog {
        next_id: first + live + 2,
        entries: (first..first + live).map(tiered_definition).collect(),
        index: inf_store::IndexCatalog::default(),
        dropped,
    };
    let fs = inf_log::fs::StdSegmentFs;
    inf_log::meta::write_meta(&fs, dir, &catalog.encode()).expect("seed META");
    format!("tiered-{}", first + live - 1)
}

/// Starts infinityd on `dir` and waits for it to exit before it listens:
/// its status and stderr. A node that comes up is a failure.
fn boot_refused(dir: &Path) -> (std::process::ExitStatus, String) {
    let stderr = dir.join(format!("stderr-refused-{}", unique()));
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
            return (status, text);
        }
        if announced_port(&text).is_some() || Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("infinityd came up, or never exited, on a catalog it must refuse: {text}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The boot boundary (ADR-0148 D5, A1.2) at 63, 64 and 65 live tiered
/// definitions, each catalog with two drop tombstones and their residue on
/// disk: 63 and 64 boot and serve every definition, the tombstones taking
/// no lifetime, and the booted book has room for one more tiered `CREATE`
/// at 63 and none at 64; 65 is refused before any cell, exit 1, the error
/// naming the count and the limit. A catalog whose counter is not past a live id is
/// refused the same way (the counter check every boot's load runs).
#[test]
fn a_boot_serves_at_most_sixty_four_live_tiered_definitions() {
    let dir = data_root("boot-boundary");
    let mut fresh = start(&dir);
    assert!(fresh.stop().success(), "the fresh node stopped gracefully");
    for live in [63, LIFETIMES_MAX as u32] {
        let last = seed_tiered_catalog(&dir, live);
        let mut server = start(&dir);
        let mut c = TcpStream::connect(("127.0.0.1", server.port)).expect("connect");
        c.set_read_timeout(Some(Duration::from_secs(60))).expect("timeout");
        let used = line(&mut c, &[b"INF.NS", b"USE", last.as_bytes()]);
        let parts: [&[u8]; 7] =
            [b"INF.NS", b"CREATE", b"one-more", b"MODE", b"durable", b"MEM-BUDGET", b"3mb"];
        let created = line(&mut c, &parts);
        drop(c);
        let status = server.stop();
        assert_eq!(used, "+OK\r\n", "{live} live tiered definitions serve their last, {last}");
        let room = if live == 63 { "+OK\r\n" } else { "-ERR tiered namespace limit reached\r\n" };
        assert_eq!(created, room, "the booted book holds {live} grants, the tombstones none");
        assert!(status.success(), "the node of {live} stopped gracefully: {status}");
    }
    seed_tiered_catalog(&dir, LIFETIMES_MAX as u32 + 1);
    let (status, stderr) = boot_refused(&dir);
    assert_eq!(status.code(), Some(1), "65 live tiered definitions are refused at boot: {stderr}");
    assert!(
        stderr.contains("the catalog names 65 live tiered namespaces, more than the 64 tiered"),
        "the refusal names the count and the limit: {stderr}"
    );
    let low = inf_store::NsCatalog {
        next_id: 18,
        entries: vec![tiered_definition(20)],
        index: inf_store::IndexCatalog::default(),
        dropped: Vec::new(),
    };
    inf_log::meta::write_meta(&inf_log::fs::StdSegmentFs, &dir, &low.encode()).expect("META");
    let (status, stderr) = boot_refused(&dir);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(status.code(), Some(1), "a counter not past a live id is refused: {stderr}");
    assert!(
        stderr.contains("namespace id 20 is at or above the catalog's next id 18"),
        "the refusal names the id and the counter: {stderr}"
    );
}
