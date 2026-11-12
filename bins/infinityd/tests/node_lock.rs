//! The node lock's contract (`support/node.rs`), on `sleep` stand-ins in
//! private directories and one box-wide node. The oracle is the kernel's: a
//! raw `File::try_lock` on a lock file and, on Linux, `/proc` (a node's
//! `fdinfo`, the thread's children). A writer runs in a child process of
//! this binary (`child_entry`), since one in the readers' process is refused.
//!
//! Each case leaves the lock clean: a raw try on each lock file succeeds,
//! the process holds no node and no lease, and the thread has no child.
#![cfg(unix)]
#![allow(clippy::disallowed_methods, reason = "test target: deadlines, sleeps and a wall clock")]
#![allow(clippy::disallowed_types, reason = "test target: lock files and scratch directories")]

#[path = "support/node.rs"]
mod node;

use std::fs::{File, OpenOptions, TryLockError};
use std::io::{BufRead, BufReader};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use node::{DirReason, Live, NodeBinary, NodeError, NodeLock, Scope, Ulimit, Waits};

/// The child's case, and its lock directory and waits.
const CHILD_CASE: &str = "INF_NODE_LOCK_CHILD";
const CHILD_DIR: &str = "INF_NODE_LOCK_DIR";
const CHILD_EXCLUSIVE_MS: &str = "INF_NODE_LOCK_EXCLUSIVE_MS";
const CHILD_SETTLE_MS: &str = "INF_NODE_LOCK_SETTLE_MS";
/// The planted holder's timing (each node's life, the start interval, the
/// starting window) and its nodes' cells; how long a writer holds its lease.
const CHILD_HOLD_MS: &str = "INF_NODE_LOCK_HOLD_MS";
const CHILD_EVERY_MS: &str = "INF_NODE_LOCK_EVERY_MS";
const CHILD_FOR_MS: &str = "INF_NODE_LOCK_FOR_MS";
const CHILD_CELLS: &str = "INF_NODE_LOCK_CELLS";
/// The variable the missing-binary case points `built_or` at.
const MISSING_BINARY: &str = "INF_NODE_LOCK_MISSING_BINARY";
/// The prefix of a child's report line.
const REPORT: &str = "node-lock-report:";

const WAITS: Waits = Waits {
    shared: Duration::from_secs(10),
    exclusive: Duration::from_secs(10),
    settle: Duration::ZERO,
};

fn scratch(case: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("node-lock-{case}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch directory");
    dir
}

/// A raw try on one lock file through a description of its own: the
/// kernel's answer, independent of the module.
fn raw_try(dir: &Path, name: &str, exclusive: bool) -> bool {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join(name))
        .expect("open a lock file");
    let tried = if exclusive { file.try_lock() } else { file.try_lock_shared() };
    match tried {
        Ok(()) => true,
        Err(TryLockError::WouldBlock) => false,
        Err(TryLockError::Error(error)) => panic!("try {name}: {error}"),
    }
}

/// Whether an exclusive raw try succeeds within 1 s. Under `cargo test`
/// the tests are threads of one process, and a sibling's spawn copies this
/// test's descriptions into its child until that child's `exec`: a copy
/// holds the lock for that instant. A lock held past 1 s is held.
fn free(dir: &Path, name: &str) -> bool {
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        if raw_try(dir, name, true) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// This thread's live children (Linux: a child shows only in its spawner's
/// file); empty elsewhere.
fn children() -> String {
    if cfg!(target_os = "linux") {
        std::fs::read_to_string("/proc/thread-self/children").expect("thread children")
    } else {
        String::new()
    }
}

fn assert_clean<S: Scope>(lock: &NodeLock<S>) {
    for name in ["turnstile", "nodes"] {
        assert!(free(lock.dir(), name), "{name} held: {}", takers(&lock.dir().join(name)));
    }
    assert_eq!(lock.live(), Live { process: 0, thread: 0, leased: false });
    assert_eq!(children().trim(), "", "this thread's children");
}

/// The processes `/proc/locks` names on one lock file (Linux), for a red.
fn takers(path: &Path) -> String {
    let Ok(ino) = std::fs::metadata(path).map(|metadata| metadata.ino()) else {
        return format!("{} is gone", path.display());
    };
    let table = std::fs::read_to_string("/proc/locks").unwrap_or_default();
    let mut found = Vec::new();
    for line in table.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let Some(at) = fields.iter().position(|field| field.matches(':').count() == 2) else {
            continue;
        };
        if at >= 2 && fields[at].ends_with(&format!(":{ino}")) {
            let pid = fields[at - 1];
            let command = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
            let command = String::from_utf8_lossy(&command).replace('\0', " ");
            found.push(format!("pid {pid} {} ({})", fields[at - 2], command.trim()));
        }
    }
    format!("{}: {}", path.display(), found.join(", "))
}

fn unix_ms() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).expect("after the epoch").as_millis()
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "{what} within 10 s");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// A child process of this binary, killed and reaped on drop.
struct ChildRun(std::process::Child);

impl Drop for ChildRun {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl ChildRun {
    fn start(case: &str, setup: impl FnOnce(&mut Command)) -> Self {
        let mut command = Command::new(std::env::current_exe().expect("test binary"));
        command
            .args(["--exact", "child_entry", "--nocapture", "--test-threads", "1"])
            .env(CHILD_CASE, case)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        setup(&mut command);
        Self(command.spawn().expect("spawn this test binary"))
    }

    fn writer(dir: &Path, exclusive: Duration, settle: Duration) -> Self {
        Self::start("writer", |command| {
            command
                .env(CHILD_DIR, dir)
                .env(CHILD_EXCLUSIVE_MS, exclusive.as_millis().to_string())
                .env(CHILD_SETTLE_MS, settle.as_millis().to_string());
        })
    }

    /// Waits for the child, asserts it passed, and returns its report line.
    fn finish(mut self) -> String {
        let stdout = self.0.stdout.take().expect("piped stdout");
        let mut report = None;
        for line in BufReader::new(stdout).lines() {
            let line = line.expect("child stdout");
            // With `--nocapture` the line follows libtest's `test … ` prefix.
            if let Some((_, rest)) = line.split_once(REPORT) {
                report = Some(rest.trim().to_owned());
            }
        }
        let status = self.0.wait().expect("child wait");
        let mut stderr = String::new();
        if let Some(mut pipe) = self.0.stderr.take() {
            let _ = std::io::Read::read_to_string(&mut pipe, &mut stderr);
        }
        assert!(status.success(), "the child failed: {status}: {stderr}");
        report.unwrap_or_else(|| panic!("the child reported nothing: {stderr}"))
    }
}

fn field(report: &str, key: &str) -> String {
    report
        .split_whitespace()
        .find_map(|word| word.strip_prefix(key)?.strip_prefix('='))
        .unwrap_or_else(|| panic!("no {key} in {report}"))
        .to_owned()
}

/// The lock rides the child: with every test-side handle but the
/// `Node` gone, `nodes` is held until the stand-in exits; a raw command's
/// child holds nothing (control).
#[test]
fn the_lock_rides_the_child() {
    let dir = scratch("rides");
    let lock = NodeLock::at(&dir, WAITS);
    let mut raw = ChildRun(Command::new("sleep").arg("5").spawn().expect("raw sleep"));
    assert!(free(&dir, "nodes"), "a raw command's child holds no lock");
    let _ = raw.0.kill();
    let _ = raw.0.wait();
    drop(raw);

    let mut node = lock.stand_in().arg("0.5").spawn().expect("a stand-in");
    assert!(!raw_try(&dir, "nodes", true), "a live stand-in holds `nodes`");
    assert!(raw_try(&dir, "nodes", false), "shared beside shared");
    assert!(free(&dir, "turnstile"), "a start releases the turnstile");
    assert_eq!(lock.live(), Live { process: 1, thread: 1, leased: false });
    node.wait().expect("the stand-in exits");
    assert!(free(&dir, "nodes"), "an exited stand-in holds nothing");
    drop(node);
    assert_clean(&lock);
}

/// A served node's fd 0 is the box-wide `nodes` file, read-locked:
/// the node binary keeps the lock it was handed until it exits.
#[cfg(target_os = "linux")]
#[test]
fn a_served_node_holds_the_lock_on_fd_0() {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("probe bind")
        .local_addr()
        .expect("probe address")
        .port()
        .to_string();
    let lock = NodeLock::box_wide();
    let mut node = NodeBinary::built()
        .command()
        .args(["--port", &port, "--cells", "1"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn infinityd under the node lock");
    let deadline = Instant::now() + Duration::from_secs(30);
    while std::net::TcpStream::connect(("127.0.0.1", port.parse::<u16>().expect("port"))).is_err() {
        assert!(node.try_wait().expect("try_wait").is_none(), "infinityd exited before serving");
        assert!(Instant::now() < deadline, "infinityd never accepted on {port}");
        std::thread::sleep(Duration::from_millis(5));
    }
    let fd0 = std::fs::read_link(format!("/proc/{}/fd/0", node.id())).expect("the node's fd 0");
    assert_eq!(fd0, lock.dir().join("nodes"), "the node's fd 0");
    let fdinfo = std::fs::read_to_string(format!("/proc/{}/fdinfo/0", node.id())).expect("fdinfo");
    assert!(
        fdinfo.lines().any(|line| line.contains("FLOCK") && line.contains("READ")),
        "the node's fd 0 holds no shared flock: {fdinfo}"
    );
    drop(node);
    assert_eq!(lock.live(), Live { process: 0, thread: 0, leased: false });
    assert_eq!(children().trim(), "", "this thread's children");
}

/// A nested start passes a draining writer: beside a live stand-in and a
/// writer that holds the turnstile, the same thread's second start passes
/// (its wait is a hang bound, under the writer's own deadline), and the
/// writer's lease follows both stand-ins' ends.
#[test]
fn a_nested_start_passes_a_draining_writer() {
    let dir = scratch("nested");
    let lock = NodeLock::at(&dir, WAITS);
    let mut first = lock.stand_in().arg("60").spawn().expect("the first stand-in");
    let writer = ChildRun::writer(&dir, Duration::from_secs(30), Duration::ZERO);
    wait_until("the writer takes the turnstile", || !raw_try(&dir, "turnstile", false));
    let began = Instant::now();
    let mut second = lock.stand_in().arg("60").spawn().expect("a nested start");
    let nested = began.elapsed();
    assert!(nested < Duration::from_secs(10), "the nested start waited {nested:?}");
    assert_eq!(lock.live(), Live { process: 2, thread: 2, leased: false });
    // Both still hold `nodes`: the writer can lease only after this.
    let ending = unix_ms();
    for node in [&mut first, &mut second] {
        node.kill().expect("end a stand-in");
        node.wait().expect("reap a stand-in");
    }
    let report = writer.finish();
    assert_eq!(field(&report, "outcome"), "leased", "{report}");
    let leased: u128 = field(&report, "leased_at_ms").parse().expect("ms");
    assert!(leased >= ending, "the writer leased before the stand-ins ended: {report}");
    drop((first, second));
    assert_clean(&lock);
}

/// Writer preference: a first start (from a thread with no live node)
/// that meets a waiting writer waits behind it at the turnstile, so it
/// returns only after the writer's lease, however long the stream of starts.
#[test]
fn a_first_start_waits_behind_a_waiting_writer() {
    let dir = scratch("preference");
    let lock = Arc::new(NodeLock::at(&dir, WAITS));
    let mut holder = lock.stand_in().arg("60").spawn().expect("the holder");
    let writer = ChildRun::writer(&dir, Duration::from_secs(30), Duration::ZERO);
    wait_until("the writer takes the turnstile", || !raw_try(&dir, "turnstile", false));
    let other = Arc::clone(&lock);
    let starter = std::thread::spawn(move || {
        let node = other.stand_in().arg("0").spawn().expect("a first start");
        let returned_at = unix_ms();
        drop(node);
        returned_at
    });
    // The starter's start is counted before it waits: it asked while the
    // writer waited, before the holder ends and lets the writer in.
    wait_until("the first start asks", || lock.live().process == 2);
    holder.kill().expect("end the holder");
    holder.wait().expect("reap the holder");
    let returned_at = starter.join().expect("the starter");
    let report = writer.finish();
    assert_eq!(field(&report, "outcome"), "leased", "{report}");
    let leased: u128 = field(&report, "leased_at_ms").parse().expect("ms");
    assert!(returned_at >= leased, "a first start passed a waiting writer: {report}");
    drop(holder);
    assert_clean(&*lock);
}

/// The settle: the writer holds both locks for its settle after the
/// last holder ends, so its lease returns no earlier than that.
#[test]
fn the_writer_settles_before_its_lease_returns() {
    let dir = scratch("settle");
    let lock = NodeLock::at(&dir, WAITS);
    let mut stand_in = lock.stand_in().arg("60").spawn().expect("a stand-in");
    let writer = ChildRun::writer(&dir, Duration::from_secs(30), Duration::from_millis(500));
    wait_until("the writer takes the turnstile", || !raw_try(&dir, "turnstile", false));
    // The stand-in holds `nodes` until after this; whole milliseconds on
    // both sides, so the settle reads at least 499.
    let ending = unix_ms();
    stand_in.kill().expect("end the stand-in");
    stand_in.wait().expect("reap the stand-in");
    let report = writer.finish();
    assert_eq!(field(&report, "outcome"), "leased", "{report}");
    let leased: u128 = field(&report, "leased_at_ms").parse().expect("ms");
    assert!(leased >= ending + 499, "leased {} ms after the holder ended", leased - ending);
    drop(stand_in);
    assert_clean(&lock);
}

/// In-process refusals: a leased process refuses a shared start, and
/// a process with a live node refuses an ask, each at once and typed.
#[test]
fn a_process_never_holds_the_lease_beside_its_own_node() {
    let dir = scratch("in-process");
    let lock = NodeLock::at(&dir, WAITS);
    let lease = lock.exclusive().expect("a free lock's lease");
    assert!(lock.live().leased);
    let began = Instant::now();
    let refused = lock.stand_in().arg("5").spawn();
    assert!(matches!(refused, Err(NodeError::LeasedHere)), "{:?}", refused.err());
    assert!(began.elapsed() < Duration::from_millis(100));
    assert_eq!(lock.live(), Live { process: 0, thread: 0, leased: true });
    let again = lock.exclusive();
    assert!(matches!(again, Err(NodeError::LeasedHere)), "{:?}", again.err());
    drop(lease);
    assert_clean(&lock);

    let mut node = lock.stand_in().arg("0.3").spawn().expect("a stand-in");
    let began = Instant::now();
    let refused = lock.exclusive();
    assert!(matches!(refused, Err(NodeError::NodesHere(1))), "{:?}", refused.err());
    assert!(began.elapsed() < Duration::from_millis(100));
    assert!(!lock.live().leased, "the refused ask cleared its flag");
    node.wait().expect("the stand-in exits");
    drop(node);
    assert_clean(&lock);
}

/// The writer's deadline: a stand-in outlives the writer's wait; its
/// ask times out with the turnstile released and its flag cleared (the
/// child asserts both), and a first start then passes at once.
#[test]
fn the_writers_deadline_frees_what_it_took() {
    let dir = scratch("deadline");
    let lock = Arc::new(NodeLock::at(&dir, WAITS));
    let mut stand_in = lock.stand_in().arg("5").spawn().expect("a stand-in");
    let report = ChildRun::writer(&dir, Duration::from_millis(500), Duration::ZERO).finish();
    assert_eq!(field(&report, "outcome"), "timed-out", "{report}");
    let other = Arc::clone(&lock);
    let waited = std::thread::spawn(move || {
        let began = Instant::now();
        let node = other.stand_in().arg("0").spawn().expect("a first start");
        let waited = began.elapsed();
        drop(node);
        waited
    })
    .join()
    .expect("the starting thread");
    assert!(waited < Duration::from_millis(100), "a first start waited {waited:?}");
    stand_in.kill().expect("kill the stand-in");
    drop(stand_in);
    assert_clean(&*lock);
}

/// A node ends with its guard: a closure that starts a 60 s stand-in and
/// panics returns within 1 s, through `spawn`, through `spawn_under`, and
/// through a `.ulimit` command, whose shell must exec the stand-in in its
/// own pid (Linux reads it), so the kill reaches it.
#[test]
fn a_node_ends_when_its_guard_unwinds() {
    let dir = scratch("unwind");
    let lock = NodeLock::at(&dir, WAITS);
    for (leased, limited) in [(false, false), (true, false), (false, true)] {
        let mut panicked = None;
        let mut execed = true;
        let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let command = lock.stand_in().arg("60");
            if leased {
                let lease = lock.exclusive().expect("a free lock's lease");
                let _node = command.spawn_under(&lease).expect("a stand-in");
                panicked = Some(Instant::now());
                panic!("planted: a leased node's test fails");
            }
            let command =
                if limited { command.ulimit(Ulimit::AddressSpaceKib(4 << 20)) } else { command };
            let node = command.spawn().expect("a stand-in");
            if limited {
                execed = runs_as(node.id(), "sleep");
            }
            panicked = Some(Instant::now());
            panic!("planted: a node's test fails");
        }));
        assert!(result.is_err(), "the closure panics");
        let unwound = panicked.expect("the closure started its stand-in").elapsed();
        let case = format!("leased {leased}, limited {limited}");
        assert!(execed, "{case}: the shell did not exec the stand-in in its own pid");
        assert!(unwound < Duration::from_secs(1), "{case}: unwinding took {unwound:?}");
        assert_clean(&lock);
    }
}

/// Whether `pid` runs `name` within 5 s (Linux, by `/proc`; true elsewhere).
fn runs_as(pid: u32, name: &str) -> bool {
    if !cfg!(target_os = "linux") {
        return true;
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
        if comm.trim() == name {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// A foreign exclusive holder: a start times out at its deadline,
/// within a watchdog, with nothing kept, and is clean once it is released.
#[test]
fn a_foreign_exclusive_holder_times_a_start_out() {
    let dir = scratch("foreign");
    let waits = Waits { shared: Duration::from_millis(500), ..WAITS };
    let lock = Arc::new(NodeLock::at(&dir, waits));
    let foreign = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join("nodes"))
        .expect("open nodes");
    foreign.try_lock().expect("a foreign exclusive hold");
    let (sender, receiver) = std::sync::mpsc::channel();
    let other = Arc::clone(&lock);
    std::thread::spawn(move || {
        let began = Instant::now();
        let outcome = other.stand_in().arg("5").spawn().map(drop);
        let _ = sender.send((outcome, began.elapsed()));
    });
    let (outcome, waited) =
        receiver.recv_timeout(Duration::from_secs(3)).expect("watchdog: the start never returned");
    match outcome {
        Err(NodeError::TimedOut(timeout)) => assert_eq!(timeout.lock, dir.join("nodes")),
        other => panic!("a start beside a foreign holder: {:?}", other.err()),
    }
    assert!(waited >= Duration::from_millis(500), "timed out early: {waited:?}");
    assert!(waited < Duration::from_secs(1), "timed out late: {waited:?}");
    assert!(free(&dir, "turnstile"), "the timed-out start released the turnstile");
    drop(foreign);
    assert_clean(&*lock);
}

/// Whether a refusal carries the reason a case expects.
type Expected = fn(&DirReason) -> bool;

fn denied(reason: &DirReason) -> bool {
    matches!(reason, DirReason::Io(error) if error.kind() == std::io::ErrorKind::PermissionDenied)
}

/// The refusals of a lock path, in process: a regular file, a path
/// under one, a fresh 0500 directory, and `/` (root's, standing in for
/// another user's). Each start and ask refuses, typed, and leaves nothing.
#[test]
fn a_refused_lock_path_leaves_nothing() {
    let base = scratch("paths");
    let file = base.join("file");
    std::fs::write(&file, b"").expect("a regular file");
    let own = std::fs::metadata(&file).expect("the file's owner").uid();
    assert_ne!(own, 0, "under uid 0 the 0500 and `/` paths do not refuse");
    let read_only = base.join("read-only");
    std::fs::create_dir(&read_only).expect("a directory");
    std::fs::set_permissions(&read_only, std::fs::Permissions::from_mode(0o500)).expect("0500");
    let under = file.join("under");
    let cases: [(&Path, Expected); 4] = [
        (&file, |reason| matches!(reason, DirReason::NotADirectory)),
        (&under, |reason| matches!(reason, DirReason::Io(_))),
        (&read_only, denied),
        (Path::new("/"), |reason| matches!(reason, DirReason::OwnedBy { uid: 0, .. })),
    ];
    for (path, expected) in cases {
        let lock = NodeLock::at(path, WAITS);
        for refused in [lock.stand_in().arg("5").spawn().err(), lock.exclusive().err()] {
            match refused {
                Some(NodeError::Dir(refusal)) => {
                    assert!(expected(&refusal.reason), "{}: {:?}", path.display(), refusal)
                }
                other => panic!("{}: {other:?}", path.display()),
            }
        }
        assert_eq!(lock.live(), Live { process: 0, thread: 0, leased: false });
    }
    let left = std::fs::read_dir(&read_only).expect("read the 0500 directory").count();
    assert_eq!(left, 0, "a refused start left a file in {}", read_only.display());
    assert_eq!(children().trim(), "", "this thread's children");
}

/// The box-wide refusals, each in a child whose environment carries
/// the case: `XDG_RUNTIME_DIR` a regular file, then a 0500 directory; with
/// it private, `built_or` naming a missing path, then a temp directory the
/// uid probe cannot write (refused against the probe's path, its kind kept).
/// The starts pass `--help`, so a wrong spawn exits before a ring.
#[test]
fn a_refused_box_wide_start_leaves_nothing() {
    let base = scratch("box-wide");
    let file = base.join("file");
    std::fs::write(&file, b"").expect("a regular file");
    let read_only = base.join("read-only");
    std::fs::create_dir(&read_only).expect("a directory");
    std::fs::set_permissions(&read_only, std::fs::Permissions::from_mode(0o500)).expect("0500");
    let private = base.join("runtime");
    std::fs::create_dir(&private).expect("a runtime directory");
    for (case, runtime) in [("xdg-file", &file), ("xdg-read-only", &read_only)] {
        let report = ChildRun::start(case, |command| {
            command.env("XDG_RUNTIME_DIR", runtime);
        })
        .finish();
        assert_eq!(field(&report, "outcome"), "refused", "{case}: {report}");
    }
    let report = ChildRun::start("missing-binary", |command| {
        command.env("XDG_RUNTIME_DIR", &private).env(MISSING_BINARY, base.join("missing-binary"));
    })
    .finish();
    assert_eq!(field(&report, "outcome"), "refused", "{report}");
    assert!(free(&private.join("infinity-locked-memory"), "nodes"));
    assert!(free(&private.join("infinity-locked-memory"), "turnstile"));
    let report = ChildRun::start("uid-probe", |command| {
        command.env("XDG_RUNTIME_DIR", &private).env("TMPDIR", &read_only);
    })
    .finish();
    assert_eq!(field(&report, "outcome"), "refused", "{report}");
}

/// The child's side: a writer (on a private lock, or the box's when no
/// directory is given), a planted holder of box-wide one-cell nodes, or a
/// box-wide refusal case. A run with no case passes at once.
#[test]
fn child_entry() {
    let Some(case) = std::env::var_os(CHILD_CASE) else { return };
    match case.to_str().expect("a Unicode case") {
        "writer" => writer(),
        "nodes" => planted_nodes(),
        "xdg-file" => box_wide_refusal(|reason| matches!(reason, DirReason::Io(_))),
        "xdg-read-only" => box_wide_refusal(denied),
        "missing-binary" => missing_binary(),
        "uid-probe" => uid_probe(),
        other => panic!("unknown child case {other}"),
    }
}

fn env_ms(key: &str) -> u64 {
    std::env::var(key).map_or(0, |value| value.parse().expect("milliseconds"))
}

fn writer() {
    let Some(dir) = std::env::var_os(CHILD_DIR) else {
        return ask(NodeLock::box_wide(), Duration::from_millis(env_ms(CHILD_HOLD_MS)));
    };
    let waits = Waits {
        exclusive: Duration::from_millis(env_ms(CHILD_EXCLUSIVE_MS)),
        settle: Duration::from_millis(env_ms(CHILD_SETTLE_MS)),
        ..WAITS
    };
    ask(NodeLock::at(Path::new(&dir), waits), Duration::ZERO);
}

/// One exclusive ask, reported; a lease is held for `hold`.
fn ask<S: Scope>(lock: NodeLock<S>, hold: Duration) {
    let began = Instant::now();
    match lock.exclusive() {
        Ok(lease) => {
            let leased_at = unix_ms();
            let ask = began.elapsed().as_millis();
            println!("{REPORT} outcome=leased leased_at_ms={leased_at} ask_ms={ask}");
            std::thread::sleep(hold);
            drop(lease);
        }
        Err(NodeError::TimedOut(_)) => {
            assert!(!lock.live().leased, "a timed-out ask cleared its flag");
            assert!(free(lock.dir(), "turnstile"), "a timed-out ask released the turnstile");
            println!("{REPORT} outcome=timed-out");
        }
        Err(other) => panic!("the writer's ask: {other}"),
    }
}

/// Box-wide nodes of `CHILD_CELLS` cells (one by default), one every
/// `CHILD_EVERY_MS` for `CHILD_FOR_MS`, each living `CHILD_HOLD_MS` from its
/// start, each started by a thread of its own (a thread with no live node):
/// a planted holder for the lock's falsifiers. Each reports its start's
/// wait and whether it accepted a connection before the hold ended.
fn planted_nodes() {
    let (hold, every) = (env_ms(CHILD_HOLD_MS), env_ms(CHILD_EVERY_MS).max(1));
    let starts = env_ms(CHILD_FOR_MS).div_ceil(every).max(1);
    let cells = std::env::var(CHILD_CELLS).unwrap_or_else(|_| "1".to_owned());
    let began = Instant::now();
    let threads: Vec<_> = (0..starts)
        .map(|index| {
            let due = began + Duration::from_millis(index * every);
            std::thread::sleep(due.saturating_duration_since(Instant::now()));
            let cells = cells.clone();
            std::thread::spawn(move || planted_node(index, &cells, Duration::from_millis(hold)))
        })
        .collect();
    let mut waits = Vec::new();
    let mut served = 0;
    for thread in threads {
        let (waited, up) = thread.join().expect("a starter");
        waits.push(waited);
        served += usize::from(up);
    }
    let max = waits.iter().max().copied().unwrap_or(0);
    println!("{REPORT} outcome=held starts={} served={served} max_wait_ms={max}", waits.len());
}

/// One planted node: its start's wait, and whether it served.
fn planted_node(index: u64, cells: &str, hold: Duration) -> (u128, bool) {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|probe| probe.local_addr())
        .expect("probe port")
        .port();
    let asked = Instant::now();
    let mut node = NodeBinary::built()
        .command()
        .args(["--port", &port.to_string(), "--cells", cells])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("a planted node");
    let waited = asked.elapsed().as_millis();
    let started = Instant::now();
    let up = loop {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            break true;
        }
        if node.try_wait().expect("try_wait").is_some() || started.elapsed() >= hold {
            break false;
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    println!("node-lock-start: index={index} pid={} waited_ms={waited} served={up}", node.id());
    std::thread::sleep(hold.saturating_sub(started.elapsed()));
    drop(node);
    (waited, up)
}

fn box_wide_refusal(expected: Expected) {
    let lock = NodeLock::box_wide();
    match NodeBinary::built().command().arg("--help").output() {
        Err(NodeError::Dir(refusal)) => assert!(expected(&refusal.reason), "{refusal:?}"),
        other => panic!("a start in {}: {other:?}", lock.dir().display()),
    }
    assert_eq!(lock.live(), Live { process: 0, thread: 0, leased: false });
    assert_eq!(children().trim(), "", "this thread's children");
    println!("{REPORT} outcome=refused");
}

/// The uid probe fails in an unwritable temp directory: the refusal names
/// the probe's path there and keeps the error's kind.
fn uid_probe() {
    let temp = std::env::temp_dir();
    match NodeBinary::built().command().arg("--help").output() {
        Err(NodeError::Dir(refusal)) => {
            assert!(refusal.path.starts_with(&temp), "refused against {refusal:?}");
            assert!(denied(&refusal.reason), "{refusal:?}");
        }
        other => panic!("a start with no uid probe: {other:?}"),
    }
    println!("{REPORT} outcome=refused");
}

fn missing_binary() {
    let lock = NodeLock::box_wide();
    let binary = NodeBinary::built_or(MISSING_BINARY);
    let command = binary.command().arg("--help");
    // Neither `Debug` text prints the binary's path: each is its fixed string.
    assert_eq!(format!("{binary:?}"), "NodeBinary(..)");
    assert_eq!(format!("{command:?}"), "NodeCommand(..)");
    match command.output() {
        Err(NodeError::Spawn(error)) => assert_eq!(error.kind(), std::io::ErrorKind::NotFound),
        other => panic!("a missing binary: {other:?}"),
    }
    assert_eq!(lock.live(), Live { process: 0, thread: 0, leased: false });
    assert!(free(lock.dir(), "nodes"), "a failed spawn kept `nodes`");
    assert!(free(lock.dir(), "turnstile"), "a failed spawn kept the turnstile");
    println!("{REPORT} outcome=refused");
}
