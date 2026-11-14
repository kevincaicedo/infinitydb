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
//! Engagement: after each `SET` the test reads the owning cell's
//! `frame_waits_fill`. The run is engaged when 8 or more episodes began
//! inside one fill window, so that a fill timer kept per episode would have
//! all eight armed at the eighth's start; else it is `VACUOUS`. A window is
//! read between two readings, from the instant before the first's `SET` is
//! written to the instant the second's `INFO` reply is read. A reading
//! counts every episode begun before its `SET` was written and none begun
//! after its reply, so each episode the second counts and the first does
//! not began inside that span, which can overstate, never understate, the
//! time they took.
//!
//! The medium. An episode needs a sealable frame, and none seals while the
//! cell's K frames are in flight, so a device that other workloads share can
//! keep the ring full past the burst. The data directory is therefore a
//! fresh directory on `/dev/shm`, a tmpfs, where a frame's write is a copy
//! into memory whatever else writes to the host's disks. The fua class opens
//! its segments `O_DIRECT` (ADR-0086 D1), which tmpfs takes from Linux 6.6:
//! before any node starts, a `/dev/shm` that is missing, is not a tmpfs, or
//! refuses the engine's own `O_DIRECT` open fails the run as `MEDIUM`, with
//! no fallback to a disk directory. The node owns the directory and is
//! reaped before it goes; a red after the node's spawn prints its exit
//! status and the last lines of its stderr, which lives on the medium. The
//! directory is removed on every exit but a kill, and a killed run's by the
//! next run, which reads a run's liveness from `/proc`: its scope is one
//! PID namespace per `/dev/shm`.
#![cfg(target_os = "linux")]
#![allow(clippy::disallowed_methods, clippy::disallowed_types)] // harness process, not cell code

#[path = "support/node.rs"]
mod node;

use node::{Node, NodeBinary};

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use inf_log::fs::SegmentFs;
use inf_server::StdSegmentFs;

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
/// The medium: a tmpfs, so no device's latency reaches the frame ring.
const MEDIUM: &str = "/dev/shm";
/// A run's data directory on the medium: this prefix, then the run's pid.
const RUN_PREFIX: &str = "infinityd-timer-owners-";
/// The lines of the node's stderr a red prints, its last ones.
const STDERR_TAIL_LINES: usize = 40;

/// The run's data directory on the medium, removed when it drops: a tmpfs
/// holds its bytes in memory, and in the user's quota, until then.
struct DataDir(PathBuf);

impl DataDir {
    /// Checks the medium, removes killed runs' directories and makes this
    /// run's, fresh. Each refusal is red before any node starts.
    fn create() -> DataDir {
        let medium = Path::new(MEDIUM);
        if let Err(e) = std::fs::canonicalize(medium) {
            // `identity_of` reads nothing of a path it cannot resolve, its
            // kernel release included.
            let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
            panic!("MEDIUM: {MEDIUM} is missing ({e}) (kernel {})", kernel.trim());
        }
        // The node's own reader of a directory's filesystem.
        let identity = inf_probe::identity_of(medium);
        let kernel = identity.kernel_release.as_str();
        let kind = if identity.fs_type.is_empty() { "unknown" } else { identity.fs_type.as_str() };
        assert!(kind == "tmpfs", "MEDIUM: {MEDIUM} is {kind}, not a tmpfs (kernel {kernel})");
        sweep_killed_runs();
        let dir = DataDir(medium.join(format!("{RUN_PREFIX}{}", std::process::id())));
        // A directory of this pid is a killed run's from an earlier boot.
        let _ = std::fs::remove_dir_all(&dir.0);
        std::fs::create_dir(&dir.0).expect("data dir");
        // The engine's own open, `O_DIRECT` verified in the fd's flags; no
        // bytes, since the open is the probe.
        let probe = dir.0.join("o-direct-probe");
        if let Err(e) = StdSegmentFs.create_segment_direct(&probe, 0) {
            panic!(
                "MEDIUM: {MEDIUM} refuses O_DIRECT ({e}); tmpfs takes it from Linux 6.6, \
                 this kernel is {kernel}"
            );
        }
        std::fs::remove_file(&probe).expect("probe removed");
        dir
    }

    /// The bytes the medium holds for the run: its files' allocated blocks.
    fn footprint_bytes(&self) -> u64 {
        use std::os::unix::fs::MetadataExt;
        let mut total = 0;
        let mut pending = vec![self.0.clone()];
        while let Some(dir) = pending.pop() {
            for entry in std::fs::read_dir(&dir).expect("read_dir") {
                let entry = entry.expect("entry");
                let meta = entry.metadata().expect("metadata");
                total += meta.blocks() * 512;
                if meta.is_dir() {
                    pending.push(entry.path());
                }
            }
        }
        total
    }
}

impl Drop for DataDir {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_dir_all(&self.0) {
            eprintln!("timer_owners: {} not removed: {e}", self.0.display());
        }
    }
}

/// Removes the data directories of runs whose process is gone (a killed
/// run's): a name of `RUN_PREFIX` and a pid with no `/proc` entry, nothing
/// else. A run's pid has its entry before its directory exists, so inside
/// one PID namespace no run removes a live run's directory.
fn sweep_killed_runs() {
    for entry in std::fs::read_dir(MEDIUM).expect("the medium lists") {
        let name = entry.expect("a medium entry").file_name();
        let Some(pid) = name.to_str().and_then(|n| n.strip_prefix(RUN_PREFIX)).and_then(run_pid)
        else {
            continue;
        };
        if !Path::new("/proc").join(pid.to_string()).exists() {
            let _ = std::fs::remove_dir_all(Path::new(MEDIUM).join(&name));
        }
    }
}

/// A pid as a run writes it: ASCII digits with no leading zero, in a
/// `u32`. A name no run makes (`+1`, `01`) is never read as a pid.
fn run_pid(text: &str) -> Option<u32> {
    let digits = !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit());
    if digits && !text.starts_with('0') { text.parse().ok() } else { None }
}

/// The node, which owns its data directory: a struct's fields drop after
/// its `drop` body returns, so the node is reaped before the directory goes.
struct Server {
    child: Node,
    port: u16,
    stderr: PathBuf,
    dir: DataDir,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let status = self.child.wait();
        if std::thread::panicking() {
            // The red's reason. The exit status lives off the medium, so a
            // full medium still shows the node's fail-stop.
            match status {
                Ok(status) => eprintln!("infinityd: {status}"),
                Err(e) => eprintln!("infinityd: not reaped ({e})"),
            }
            let text = self.text();
            let lines: Vec<&str> = text.lines().collect();
            for line in &lines[lines.len().saturating_sub(STDERR_TAIL_LINES)..] {
                eprintln!("infinityd stderr: {line}");
            }
        }
    }
}

impl Server {
    /// Two cells on the fua class with the fill window at its maximum;
    /// every cell reachable through the accept hand-off.
    fn spawn(dir: DataDir) -> Server {
        let stderr = dir.0.join("stderr");
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .expect("probe bind")
            .local_addr()
            .expect("addr")
            .port()
            .to_string();
        let child = NodeBinary::built()
            .command()
            .args(["--port", &port, "--cells", &CELLS.to_string()])
            .args(["--accept-handoff", "on", "--device-probe", "off"])
            .args(["--barrier-class", "fua", "--fill-window-us", &FILL_WINDOW_US.to_string()])
            .arg("--data-dir")
            .arg(&dir.0)
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

/// One reading of the owning cell's `frame_waits_fill`: an episode it does
/// not count began after `from`, the instant before its `SET` was written;
/// one it counts began before `read`, the instant its `INFO` reply was read.
#[derive(Clone, Copy)]
struct Reading {
    from: Instant,
    read: Instant,
    waits: u64,
}

/// The most episodes counted between two readings whose span, from the
/// first's `from` to the second's `read`, is under `window`, so that each
/// began inside it; and that span. Every pair of the burst's 65 readings:
/// 2,080 compares.
fn densest_window(readings: &[Reading], window: Duration) -> (u64, Duration) {
    let mut densest = (0, Duration::ZERO);
    for (index, first) in readings.iter().enumerate() {
        for second in &readings[index + 1..] {
            let span = second.read.duration_since(first.from);
            let counted =
                second.waits.checked_sub(first.waits).expect("frame_waits_fill only rises");
            if span < window && counted > densest.0 {
                densest = (counted, span);
            }
        }
    }
    densest
}

// nextest-alone: 0.1-0.6 s. Engagement needs eight fill episodes on one cell
// inside one 100 ms fill window, and the CPU that sibling test processes take
// thins the burst (ADR-0207 D4). Alone, a run can still read `VACUOUS`: this
// keeps the rate where `cargo test`, which ran this binary alone, had it.
#[test]
fn a_burst_of_fill_episodes_leaves_one_fill_timer_per_cell() {
    let server = Server::spawn(DataDir::create());
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
    // Reading 0 counts the probe's episode: the owning cell answered
    // another `INFO` between the probe's `+OK` and `waits_before`.
    let mut readings = Vec::with_capacity(SETS + 1);
    readings.push(Reading { from: start, read: start, waits: waits_before });
    for i in 0..SETS {
        let from = Instant::now();
        writer.ok(&[b"SET", format!("{{tm}}k{i}").as_bytes(), &value]);
        let owner_conn = cells.get_mut(&owner).expect("owner");
        let waits = field(&owner_conn.info("persistence"), "frame_waits_fill");
        readings.push(Reading { from, read: Instant::now(), waits });
    }
    let burst = start.elapsed();
    let (densest, span) = densest_window(&readings, Duration::from_micros(FILL_WINDOW_US));
    let persistence: BTreeMap<u64, String> =
        cells.iter_mut().map(|(cell, conn)| (*cell, conn.info("persistence"))).collect();
    let episodes = field(&persistence[&owner], "frame_waits_fill") - waits_before;
    println!("owner cell {owner}: {episodes} fill episodes in {burst:?}");
    println!("densest window: {densest} episodes in {span:?}");
    println!("medium {MEDIUM}: {} KiB held", server.dir.footprint_bytes() >> 10);
    for (cell, info) in &persistence {
        println!("cell {cell}: timers_armed_max {}", field(info, "timers_armed_max"));
    }
    assert!(
        densest >= EPISODES_MIN,
        "VACUOUS: {densest} fill episodes on cell {owner} inside one {FILL_WINDOW_US} µs window \
         (want {EPISODES_MIN}); {episodes} in the {burst:?} burst"
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
}

/// The estimator on planted readings: eight episodes 10 ms apart lie inside
/// one window and read 8; the same eight 13 ms apart fit no window and read
/// 7, the red an estimator that reads every run engaged would miss; and a
/// span of exactly the window lies outside it.
#[test]
fn the_densest_window_counts_only_episodes_begun_inside_it() {
    let window = Duration::from_micros(FILL_WINDOW_US);
    let start = Instant::now();
    let at = |from_ms: u64, waits: u64| Reading {
        from: start + Duration::from_millis(from_ms),
        read: start + Duration::from_millis(from_ms + 1),
        waits,
    };
    let dense: Vec<Reading> = (0..=EPISODES_MIN).map(|k| at(10 * k, k)).collect();
    assert_eq!(densest_window(&dense, window), (EPISODES_MIN, Duration::from_millis(81)));
    let spread: Vec<Reading> = (0..=EPISODES_MIN).map(|k| at(13 * k, k)).collect();
    assert_eq!(densest_window(&spread, window), (EPISODES_MIN - 1, Duration::from_millis(92)));
    let edge = [at(0, 0), Reading { read: start + window, ..at(99, EPISODES_MIN) }];
    assert_eq!(densest_window(&edge, window), (0, Duration::ZERO));
}
