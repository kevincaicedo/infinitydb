//! The binary arm of ADR-0174 D1: a four-cell node under `MEM-BUDGET
//! 3mb` (a 4 MiB window per cell) takes 64 MiB of tiered `SET`s with
//! rewrites at distances under and over a window, deletes of recent keys
//! and of keys written over a window ago, and a delete then a `SET`, and a
//! `SET` then a delete, of such keys; it is killed and boots again —
//! replay demotes the tail that does not fit instead of failing the boot,
//! a replayed delete verifies and removes the copy the boot demoted (R6),
//! and every acknowledged key answers its bytes. Two arms, `FSYNC always`
//! and `everysec`; `TIER-IO-MODE direct` (the default) on a disk
//! filesystem, the workspace `target/` directory. Red before the replay
//! seam: the restarted process exits non-zero (`replay apply failed …
//! OutOfMemory`). The control leg is the same load under `MEM-BUDGET
//! 64mb`, inside every cell's window: the restart must not demote, and
//! every counter of the zero set reads zero in `INFO` (D6).
//!
//! The `everysec` arm's durability point is a barrier, not a sleep:
//! every load key carries one of 64 hash tags, and after the last load
//! reply one `SET` per tag goes to a second namespace created
//! `FSYNC always` — a key's cell is a function of its hash tag, a cell's
//! durable records share one FIFO log, and an `always` reply waits for
//! the fsync watermark to cover its record — so every acknowledged load
//! write is under a completed barrier and both arms compare exactly.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)] // harness process, not cell code

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

static SPAWN_LOCK: Mutex<()> = Mutex::new(());

const CELLS: &str = "4";
const TAGS: u64 = 64;
/// 64 MiB of load: 16 MiB per cell on average, four windows each.
const LOAD_BYTES: u64 = 64 << 20;
const VALUE_LEN: usize = 3 << 10;
const PIPELINE: usize = 256;

fn unique() -> u32 {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

fn data_root(tag: &str) -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/e2e-replay-spill")
        .join(format!("{tag}-{}-{}", std::process::id(), unique()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("data root");
    root
}

/// The filesystem type under `path`, from `/proc/self/mountinfo` (the
/// longest mount point that prefixes the canonical path).
fn fs_type(path: &Path) -> String {
    let canonical = std::fs::canonicalize(path).expect("canonical data root");
    let canonical = canonical.to_string_lossy().into_owned();
    let mounts = std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
    let mut best: Option<(usize, String)> = None;
    for line in mounts.lines() {
        let mut halves = line.splitn(2, " - ");
        let (Some(head), Some(tail)) = (halves.next(), halves.next()) else { continue };
        let fields: Vec<&str> = head.split(' ').collect();
        let Some(point) = fields.get(4) else { continue };
        let Some(kind) = tail.split(' ').next() else { continue };
        let point = point.replace("\\040", " ");
        let covers = canonical == point
            || canonical.starts_with(&format!("{}/", point.trim_end_matches('/')));
        if covers && best.as_ref().is_none_or(|(len, _)| point.len() > *len) {
            best = Some((point.len(), kind.to_owned()));
        }
    }
    best.map_or_else(String::new, |(_, kind)| kind)
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
    /// Four cells, pinned; `--port 0` gives every cell its own port and
    /// only cell 0's is announced.
    fn spawn(dir: &Path) -> Result<Server, String> {
        let stderr = dir.join(format!("stderr-{}-{}", std::process::id(), unique()));
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .expect("probe bind")
            .local_addr()
            .expect("addr")
            .port()
            .to_string();
        let child = Command::new(env!("CARGO_BIN_EXE_infinityd"))
            .args(["--port", &port, "--cells", CELLS, "--pin-start", "4", "--pin-stride", "2"])
            .arg("--data-dir")
            .arg(dir)
            .args(["--device-probe", "off"])
            .stdout(Stdio::null())
            .stderr(Stdio::from(std::fs::File::create(&stderr).expect("stderr file")))
            .spawn()
            .expect("spawn infinityd");
        let mut server = Server { child, port: 0, stderr };
        let deadline = Instant::now() + Duration::from_secs(300);
        // The binary listens before recovery ends: a boot is up only once
        // the control line says every cell serves, and a cell that fails
        // its replay ends the process after the port was announced.
        loop {
            if let Some(status) = server.child.try_wait().expect("try_wait") {
                return Err(format!("infinityd exited during boot: {status} — {}", server.text()));
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
                return Ok(server);
            }
            assert!(Instant::now() < deadline, "infinityd never came up: {text}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn text(&self) -> String {
        std::fs::read_to_string(&self.stderr).unwrap_or_default()
    }

    fn sigkill(&mut self) {
        let status = Command::new("kill")
            .args(["-KILL", &self.child.id().to_string()])
            .status()
            .expect("kill(1)");
        assert!(status.success(), "kill -KILL failed");
        let _ = self.child.wait();
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
        c.set_read_timeout(Some(Duration::from_secs(120))).expect("timeout");
        c.set_nodelay(true).expect("nodelay");
        Client(c)
    }

    fn call(&mut self, parts: &[&[u8]]) -> Vec<u8> {
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            self.0.write_all(&resp(parts)).expect("write");
            let reply = self.read_reply();
            if !reply.starts_with(b"-LOADING") {
                return reply;
            }
            assert!(Instant::now() < deadline, "still loading after 120 s");
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

    /// One pipelined batch: every command's reply, in order.
    fn pipeline(&mut self, commands: &[Vec<Vec<u8>>]) -> Vec<Vec<u8>> {
        let mut wire = Vec::new();
        for command in commands {
            let parts: Vec<&[u8]> = command.iter().map(Vec::as_slice).collect();
            wire.extend_from_slice(&resp(&parts));
        }
        self.0.write_all(&wire).expect("pipeline write");
        commands.iter().map(|_| self.read_reply()).collect()
    }
}

/// The acknowledged key → value state of the load.
type Model = BTreeMap<Vec<u8>, Vec<u8>>;

fn key(tag: u64, ordinal: u64) -> Vec<u8> {
    format!("{{{tag:02}}}:k:{ordinal:07}").into_bytes()
}

fn value(ordinal: u64, generation: u32) -> Vec<u8> {
    let mut v = vec![(ordinal % 251) as u8 ^ generation as u8; VALUE_LEN];
    v[..8].copy_from_slice(&ordinal.to_le_bytes());
    v[8..12].copy_from_slice(&generation.to_le_bytes());
    v
}

/// The load: distinct keys; a delete of a recent key every 13th op; a
/// delete of a key written over a window ago every 17th, the last of
/// those written again every 19th; the last key rewritten from over a
/// window ago deleted every 23rd; a rewrite of a key written over a
/// window ago every 11th, and of one under a window ago every 7th.
/// "Over a window" is six windows of node ops, a window and a half of
/// each cell's: by the restart's replay of that delete or rewrite, the
/// boot has demoted the key's copy. Returns the model after the load and
/// the keys whose last acknowledged op was a delete.
fn load(c: &mut Client) -> (Model, BTreeSet<Vec<u8>>) {
    let mut model = Model::new();
    let mut deleted: BTreeSet<Vec<u8>> = BTreeSet::new();
    let mut generation: BTreeMap<u64, u32> = BTreeMap::new();
    let mut bytes = 0u64;
    let mut ordinal = 0u64;
    let per_window_ops = (4u64 << 20) / VALUE_LEN as u64; // per cell ≈ 1 365 of its ops
    let old = 6 * per_window_ops;
    let (mut old_deleted, mut old_rewritten): (Option<u64>, Option<u64>) = (None, None);
    while bytes < LOAD_BYTES {
        let mut batch: Vec<Vec<Vec<u8>>> = Vec::with_capacity(PIPELINE);
        let mut expect: Vec<Vec<u8>> = Vec::with_capacity(PIPELINE);
        for _ in 0..PIPELINE {
            let op = ordinal;
            let target = if op % 13 == 12 && op > 3 {
                Some((op - 3, true))
            } else if op % 17 == 16 && op > old {
                old_deleted = Some(op - old);
                Some((op - old, true))
            } else if op % 19 == 18
                && let Some(victim) = old_deleted.take()
            {
                Some((victim, false))
            } else if op % 23 == 22
                && let Some(victim) = old_rewritten.take()
            {
                Some((victim, true))
            } else if op % 11 == 10 && op > old {
                old_rewritten = Some(op - old);
                Some((op - old, false))
            } else if op % 7 == 6 && op > per_window_ops / 4 {
                Some((op - per_window_ops / 4, false))
            } else {
                None
            };
            match target {
                Some((victim, true)) => {
                    let k = key(victim % TAGS, victim);
                    let present = model.remove(&k).is_some();
                    deleted.insert(k.clone());
                    batch.push(vec![b"DEL".to_vec(), k]);
                    expect.push(if present { b":1\r\n".to_vec() } else { b":0\r\n".to_vec() });
                }
                Some((victim, false)) => {
                    let k = key(victim % TAGS, victim);
                    let g = generation.entry(victim).or_insert(0);
                    *g += 1;
                    let v = value(victim, *g);
                    bytes += (k.len() + v.len()) as u64;
                    deleted.remove(&k);
                    model.insert(k.clone(), v.clone());
                    batch.push(vec![b"SET".to_vec(), k, v]);
                    expect.push(b"+OK\r\n".to_vec());
                }
                None => {
                    let k = key(op % TAGS, op);
                    let v = value(op, 0);
                    bytes += (k.len() + v.len()) as u64;
                    model.insert(k.clone(), v.clone());
                    generation.insert(op, 0);
                    batch.push(vec![b"SET".to_vec(), k, v]);
                    expect.push(b"+OK\r\n".to_vec());
                }
            }
            ordinal += 1;
        }
        let replies = c.pipeline(&batch);
        for ((reply, want), command) in replies.iter().zip(&expect).zip(&batch) {
            assert_eq!(
                reply,
                want,
                "{} answered {}",
                String::from_utf8_lossy(&command[0]),
                String::from_utf8_lossy(reply)
            );
        }
    }
    (model, deleted)
}

/// Which leg a run is: the window below every cell's share of the load,
/// so the restart must demote, or above it — the control leg, the same
/// load, where the restart must not.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Leg {
    /// `MEM-BUDGET 3mb`: a 4 MiB window per cell under its 16 MiB.
    Demote,
    /// `MEM-BUDGET 64mb`: a 65 MiB window per cell over its 16 MiB.
    Fit,
}

/// ADR-0174 D6's zero set in `INFO persistence`: every counter a boot
/// whose replay fits every window leaves at zero. Outside it: the markers
/// skipped, the dead-life files removed and the step-charge gauge.
const ZERO_SET: [&str; 10] = [
    "recover_node_tier_demote_steps",
    "recover_node_tier_pads_placed",
    "recover_node_tier_bytes_written",
    "recover_node_tier_barriers",
    "recover_node_tier_files_sealed",
    "recover_node_tier_settle_reads",
    "recover_node_tier_settled_same_key",
    "recover_node_tier_settled_distinct",
    "recover_node_tier_deletes_verified",
    "recover_node_tier_blob_releases",
];

fn run_arm(fsync: &str, leg: Leg) {
    let _one_at_a_time = SPAWN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = data_root(&format!("spill-{fsync}-{leg:?}"));
    let kind = fs_type(&dir);
    assert!(
        kind != "tmpfs",
        "VACUOUS: the data root {} is on tmpfs; direct mode needs a disk filesystem",
        dir.display()
    );
    let mut server = Server::spawn(&dir).expect("first boot");
    let mut c = Client::connect(server.port);
    c.ok(&[
        b"INF.NS",
        b"CREATE",
        b"t",
        b"MODE",
        b"durable",
        b"FSYNC",
        fsync.as_bytes(),
        b"MEM-BUDGET",
        match leg {
            Leg::Demote => b"3mb",
            Leg::Fit => b"64mb",
        },
    ]);
    c.ok(&[b"INF.NS", b"USE", b"t"]);
    let (model, deleted) = load(&mut c);
    assert!(model.len() > 10_000, "the load left {} keys", model.len());
    // The durability point (both arms): one `always` write per hash tag,
    // behind every load write of that tag's cell in its FIFO log.
    c.ok(&[b"INF.NS", b"CREATE", b"barrier", b"MODE", b"durable", b"FSYNC", b"always"]);
    c.ok(&[b"INF.NS", b"USE", b"barrier"]);
    let barrier: Vec<Vec<Vec<u8>>> = (0..TAGS)
        .map(|tag| vec![b"SET".to_vec(), format!("{{{tag:02}}}:b").into_bytes(), b"1".to_vec()])
        .collect();
    for reply in c.pipeline(&barrier) {
        assert_eq!(reply, b"+OK\r\n");
    }
    drop(c);
    server.sigkill();
    drop(server);

    // The restart: the tail above every cell's window must replay.
    let server = match Server::spawn(&dir) {
        Ok(server) => server,
        Err(err) => panic!("the restart failed (arm {fsync}): {err}"),
    };
    let mut c = Client::connect(server.port);
    c.ok(&[b"INF.NS", b"USE", b"t"]);
    // Engagement (ADR-0174 D6, the node fold in `INFO persistence`): the
    // restart demoted, wrote tier bytes and sealed files of its own.
    // The fold is taken once every cell is ready, on the serving cell's
    // first look at the board: wait for `loading:0` on this cell.
    let deadline = Instant::now() + Duration::from_secs(120);
    let info = loop {
        let info = String::from_utf8_lossy(&c.call(&[b"INFO", b"persistence"])).into_owned();
        if info.contains("loading:0") {
            break info;
        }
        assert!(Instant::now() < deadline, "still loading after 120 s: {info}");
        std::thread::sleep(Duration::from_millis(20));
    };
    let field = |name: &str| -> u64 {
        info.lines()
            .find_map(|line| line.strip_prefix(name).and_then(|rest| rest.strip_prefix(':')))
            .and_then(|value| value.trim().parse().ok())
            .unwrap_or_else(|| panic!("INFO persistence has no {name}: {info}"))
    };
    let demoted = field("recover_node_tier_demote_steps");
    eprintln!(
        "replay_spill arm {fsync} {leg:?}: {demoted} demote steps, {} tier bytes, {} barriers, {} \
         files sealed, {} settle reads, {} deletes verified, {} markers skipped, step charge max \
         {} bytes; {} keys, {} deleted",
        field("recover_node_tier_bytes_written"),
        field("recover_node_tier_barriers"),
        field("recover_node_tier_files_sealed"),
        field("recover_node_tier_settle_reads"),
        field("recover_node_tier_deletes_verified"),
        field("recover_node_tier_markers_skipped"),
        field("recover_node_tier_step_charge_max_bytes"),
        model.len(),
        deleted.len(),
    );
    let keys: Vec<&Vec<u8>> = model.keys().collect();
    for chunk in keys.chunks(PIPELINE) {
        let gets: Vec<Vec<Vec<u8>>> =
            chunk.iter().map(|k| vec![b"GET".to_vec(), (*k).clone()]).collect();
        for (reply, k) in c.pipeline(&gets).into_iter().zip(chunk) {
            let want = model[*k].clone();
            let mut bulk = format!("${}\r\n", want.len()).into_bytes();
            bulk.extend_from_slice(&want);
            bulk.extend_from_slice(b"\r\n");
            assert!(reply == bulk, "GET {} differs after the restart", String::from_utf8_lossy(k));
        }
    }
    // A deleted key stays deleted: a replayed delete removed the copy the
    // boot demoted (R6), or the key returns.
    let gone: Vec<&Vec<u8>> = deleted.iter().collect();
    for chunk in gone.chunks(PIPELINE) {
        let gets: Vec<Vec<Vec<u8>>> =
            chunk.iter().map(|k| vec![b"GET".to_vec(), (*k).clone()]).collect();
        for (reply, k) in c.pipeline(&gets).into_iter().zip(chunk) {
            assert!(
                reply == b"$-1\r\n",
                "DELETED KEY PRESENT: GET {} answers after the restart",
                String::from_utf8_lossy(k)
            );
        }
    }
    let dbsize = c.call(&[b"DBSIZE"]);
    assert_eq!(dbsize, format!(":{}\r\n", model.len()).into_bytes(), "DBSIZE after the restart");
    // Engagement after the content oracle, so a planted violation the
    // oracle must see is read there first.
    match leg {
        Leg::Demote => {
            // Engagement: the restart demoted, wrote and sealed tier files
            // of its own, settled records against their demoted copies and
            // verified deletes against them (R6) — VACUOUS otherwise.
            assert!(demoted > 0, "VACUOUS (arm {fsync}): the restart did not demote: {info}");
            assert!(field("recover_node_tier_bytes_written") > 0, "{info}");
            assert!(field("recover_node_tier_files_sealed") > 0, "{info}");
            let reads = field("recover_node_tier_settle_reads");
            assert!(reads > 0, "VACUOUS (arm {fsync}): no settle read: {info}");
            let verified = field("recover_node_tier_deletes_verified");
            assert!(verified > 0, "VACUOUS (arm {fsync}): no delete verified: {info}");
        }
        Leg::Fit => {
            // The control leg: a boot that fits every window demotes
            // nothing and leaves the zero set at zero.
            for name in ZERO_SET {
                assert_eq!(field(name), 0, "{name} moved on a boot that fits: {info}");
            }
        }
    }
    drop(c);
    drop(server);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_tail_above_every_cells_window_boots_under_fsync_always() {
    run_arm("always", Leg::Demote);
}

#[test]
fn a_tail_above_every_cells_window_boots_under_fsync_everysec() {
    run_arm("everysec", Leg::Demote);
}

#[test]
fn the_same_load_inside_every_cells_window_boots_without_demoting() {
    run_arm("always", Leg::Fit);
}
