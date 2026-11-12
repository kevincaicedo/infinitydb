#![allow(
    clippy::disallowed_methods,
    reason = "test target: process deadlines and /proc reads, not cell code"
)]
#![allow(clippy::disallowed_types, reason = "test-only: a /proc scan outside cell code")]
#![cfg(target_os = "linux")]
//! A node boots under the kernel's default `RLIMIT_MEMLOCK` (8 MiB).
//! io_uring charges each cell's ring and its fixed-buffer registration to
//! the user's one locked-memory budget. The registration is a probe that
//! degrades when refused, but a refused probe first charges up to the
//! limit, and a sibling cell creating its ring inside that window failed
//! with `ENOMEM`: the node exited, and a client another cell had already
//! accepted saw a reset. Every ring now exists before any cell registers.
//!
//! The window is a race. A binary that let a probe run beside a ring
//! creation failed 42 of 500 boots here at the default four cells (debug;
//! release: 43 of 500), so 100 boots miss it with probability about
//! 0.92¹⁰⁰ ≈ 0.02 %. `INF_MEMLOCK_BOOT_BIN` names another build, to show
//! the test red; `INF_MEMLOCK_BOOT_REQUIRE=1` (CI's Linux legs) makes the
//! skip below a failure.
//!
//! The budget is the user's, so any other process of the user that holds
//! it turns this test red. Every node a test starts holds the box's node
//! lock shared (`support/node.rs`); this test takes it exclusive after its
//! engagement boot and boots its control and its 100 nodes under the
//! lease, so no node another test started, in this lane or another, runs
//! beside them. A failure lists the budget's other holders (a process with
//! an io_uring or perf ring), each with its ancestors, cgroup, pinned
//! memory and standard input: a node whose standard input is not the
//! lock's `nodes` file was started outside the lock.

// nextest-alone: 4.0-5.2 s under nextest on a ring-charging kernel, the
// lease's 1 s settle included; a drain of other lanes' nodes before the lease
// adds to it. The kernel keeps the pinned-memory budget per user, and every
// node a sibling test process boots charges its rings to it, so the budget
// this verdict reads is the lane's unless it runs alone. Nodes other lanes
// start through the node module the box's node lock keeps away (ADR-0203
// D2); a node started outside the module it does not.

#[path = "support/node.rs"]
mod node;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use node::{BoxWide, ExclusiveLease, LeasedNode, Node, NodeBinary, NodeLock, Ulimit};

const BOOTS: usize = 100;
/// The shipped default: four rings of about 404 KiB each, so three
/// killed boots whose rings the kernel has not uncharged yet still leave
/// room for the next boot's four.
const CELLS: &str = "4";
/// The kernel's default limit, in the KiB `ulimit -l` takes. The probe's
/// pool (4096 buffers of 4 KiB) charges far more, so it is always refused.
const MEMLOCK_KIB: u32 = 8 * 1024;
/// Below one ring: a kernel that charges rings refuses a node here.
const MEMLOCK_KIB_BELOW_ONE_RING: u32 = 64;
/// How many ancestors a holder's line names.
const ANCESTORS: usize = 16;

/// A started node: shared (the engagement boot) or under the lease. Either
/// kills and reaps its child on drop, so a failed assertion never leaves
/// it running and holding locked memory the next boot is charged against.
enum Started<'lease> {
    Shared(Node),
    Leased(LeasedNode<'lease>),
}

impl Started<'_> {
    fn try_exited(&mut self) -> bool {
        let status = match self {
            Started::Shared(node) => node.try_wait(),
            Started::Leased(node) => node.try_wait(),
        };
        status.expect("try_wait").is_some()
    }

    fn end(&mut self) {
        let _ = match self {
            Started::Shared(node) => node.kill().and_then(|()| node.wait().map(drop)),
            Started::Leased(node) => node.kill().and_then(|()| node.wait().map(drop)),
        };
    }

    fn stderr(&mut self) -> String {
        let pipe = match self {
            Started::Shared(node) => node.take_stderr(),
            Started::Leased(node) => node.take_stderr(),
        };
        let mut stderr = String::new();
        pipe.expect("piped stderr").read_to_string(&mut stderr).expect("stderr");
        stderr
    }
}

/// How one boot ended, with everything the node wrote to stderr.
enum Boot {
    Served(String),
    Exited(String),
}

/// Boots `infinityd` under a `memlock_kib` limit: shared through the node
/// lock, or under `lease`. Served means `DBSIZE` answered: it scatters to
/// every cell, so every cell's setup is done.
fn boot(cells: &str, memlock_kib: u32, lease: Option<&ExclusiveLease<BoxWide>>) -> Boot {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("probe bind")
        .local_addr()
        .expect("probe address")
        .port();
    let command = NodeBinary::built_or("INF_MEMLOCK_BOOT_BIN")
        .command()
        .ulimit(Ulimit::LockedKib(memlock_kib))
        .args(["--port", &port.to_string(), "--cells", cells])
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut node = match lease {
        None => Started::Shared(command.spawn().expect("spawn infinityd under the node lock")),
        Some(lease) => Started::Leased(command.spawn_under(lease).expect("spawn infinityd")),
    };
    let deadline = Instant::now() + Duration::from_secs(30);
    let served = loop {
        if node.try_exited() {
            break false;
        }
        if let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) {
            break answers_dbsize(&mut stream);
        }
        assert!(Instant::now() < deadline, "infinityd never accepted on {port}");
        std::thread::sleep(Duration::from_millis(1));
    };
    if !served {
        // A reset can precede the failing cell's stderr line; let it exit.
        wait_for_exit(&mut node, deadline);
    }
    node.end();
    let stderr = node.stderr();
    if !served {
        return Boot::Exited(stderr);
    }
    // The answer came from this boot's node, not a process that took the
    // port between the probe bind and the boot.
    assert!(stderr.contains(&format!("listening on {port}\n")), "another server: {stderr}");
    Boot::Served(stderr)
}

/// `:0`, or `false` when the connection resets or closes first.
fn answers_dbsize(stream: &mut TcpStream) -> bool {
    stream.set_read_timeout(Some(Duration::from_secs(10))).expect("read timeout");
    if stream.write_all(b"*1\r\n$6\r\nDBSIZE\r\n").is_err() {
        return false;
    }
    let mut reply = [0u8; 4];
    stream.read_exact(&mut reply).is_ok() && &reply == b":0\r\n"
}

fn wait_for_exit(node: &mut Started<'_>, deadline: Instant) {
    while !node.try_exited() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// The line that says why a node exited (its last stderr line).
fn reason(stderr: &str) -> &str {
    stderr.lines().last().unwrap_or("(no stderr)")
}

/// One field of a `/proc/<pid>/status` text.
fn status_field<'a>(status: &'a str, name: &str) -> Option<&'a str> {
    status.lines().find_map(|line| line.strip_prefix(name)).map(str::trim)
}

/// A process's name and its ancestors', nearest first.
fn lineage(pid: &str) -> String {
    let mut names = Vec::new();
    let mut current = pid.to_owned();
    for _ in 0..ANCESTORS {
        let Ok(status) = std::fs::read_to_string(format!("/proc/{current}/status")) else { break };
        names.push(status_field(&status, "Name:").unwrap_or("?").to_owned());
        match status_field(&status, "PPid:") {
            Some(parent) if parent != "0" => current = parent.to_owned(),
            _ => break,
        }
    }
    names.join(" < ")
}

/// This user's other processes that hold an io_uring or perf ring, the
/// counter's holders: each with its ancestors, cgroup, `VmPin` and fd 0. A
/// holder whose fd 0 is not the lock's `nodes` file holds outside the lock.
fn holders(lock: &NodeLock<BoxWide>) -> String {
    let nodes = lock.dir().join("nodes");
    let own = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let own_uid = status_field(&own, "Uid:").and_then(|uid| uid.split_whitespace().next());
    let self_pid = std::process::id().to_string();
    let mut found = Vec::new();
    for entry in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let pid = entry.file_name().to_string_lossy().into_owned();
        if pid == self_pid || !pid.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(status) = std::fs::read_to_string(entry.path().join("status")) else { continue };
        let uid = status_field(&status, "Uid:").and_then(|uid| uid.split_whitespace().next());
        if uid.is_none() || uid != own_uid {
            continue;
        }
        let (mut rings, mut perf) = (0, 0);
        for fd in std::fs::read_dir(entry.path().join("fd")).into_iter().flatten().flatten() {
            match std::fs::read_link(fd.path()).ok().as_deref().and_then(Path::to_str) {
                Some("anon_inode:[io_uring]") => rings += 1,
                Some("anon_inode:[perf_event]") => perf += 1,
                _ => {}
            }
        }
        if rings == 0 && perf == 0 {
            continue;
        }
        let fd0 = std::fs::read_link(entry.path().join("fd/0")).ok();
        let outside =
            if fd0.as_deref() == Some(nodes.as_path()) { "" } else { ", outside the lock" };
        let fd0 = fd0.map_or_else(|| "(none)".to_owned(), |path| path.display().to_string());
        let cgroup = std::fs::read_to_string(entry.path().join("cgroup")).unwrap_or_default();
        found.push(format!(
            "pid {pid} ({}): {rings} io_uring and {perf} perf_event fds, VmPin {}, cgroup {}, \
             fd 0 {fd0}{outside}",
            lineage(&pid),
            status_field(&status, "VmPin:").unwrap_or("?"),
            cgroup.lines().last().unwrap_or("?"),
        ));
    }
    if found.is_empty() { "none".to_owned() } else { found.join("; ") }
}

/// The takers of the lock's two files in `/proc/locks`. `/proc/locks`
/// names the process that took a lock, not every process that holds it: a
/// node holds `nodes` through the description its starter took, so its
/// taker is a test, possibly gone, its pid dead or reused. `fd0_holders`
/// names the nodes themselves.
fn lock_takers(lock: &NodeLock<BoxWide>) -> String {
    use std::os::unix::fs::MetadataExt;
    let files: Vec<(&str, u64)> = ["turnstile", "nodes"]
        .into_iter()
        .filter_map(|name| Some((name, std::fs::metadata(lock.dir().join(name)).ok()?.ino())))
        .collect();
    let table = std::fs::read_to_string("/proc/locks").unwrap_or_default();
    let mut takers = Vec::new();
    for line in table.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let Some(at) = fields.iter().position(|field| field.matches(':').count() == 2) else {
            continue;
        };
        let inode = fields[at].rsplit(':').next().and_then(|inode| inode.parse::<u64>().ok());
        for (name, ino) in &files {
            if inode == Some(*ino) && at >= 2 {
                takers.push(format!("taker pid {} {} {name}", fields[at - 1], fields[at - 2]));
            }
        }
    }
    if takers.is_empty() { "none".to_owned() } else { takers.join(", ") }
}

/// This user's processes whose standard input is the lock's `nodes` file:
/// the nodes that hold it shared, each with its ancestors.
fn fd0_holders(lock: &NodeLock<BoxWide>) -> String {
    let nodes = lock.dir().join("nodes");
    let mut found = Vec::new();
    for entry in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let pid = entry.file_name().to_string_lossy().into_owned();
        if !pid.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        if std::fs::read_link(entry.path().join("fd/0")).ok().as_deref() == Some(nodes.as_path()) {
            found.push(format!("pid {pid} ({})", lineage(&pid)));
        }
    }
    if found.is_empty() { "none".to_owned() } else { found.join(", ") }
}

/// `INF_MEMLOCK_BOOT_REQUIRE=1`: a skip is a failure (a runner whose
/// kernel stopped charging rings would otherwise pass silently).
fn skip_is_failure() -> bool {
    std::env::var_os("INF_MEMLOCK_BOOT_REQUIRE").is_some_and(|value| value == "1")
}

#[test]
fn a_node_boots_under_the_default_memlock_limit() {
    // Engagement: where rings are not charged (an older kernel, or
    // CAP_IPC_LOCK) the race cannot occur and this test cannot go red. A
    // shared start: it reads nothing of the budget but its own refusal.
    match boot("1", MEMLOCK_KIB_BELOW_ONE_RING, None) {
        Boot::Served(_) => {
            let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
            let why = format!(
                "a ring is not charged to RLIMIT_MEMLOCK on kernel {} — no race exists",
                kernel.trim()
            );
            assert!(!skip_is_failure(), "INF_MEMLOCK_BOOT_REQUIRE=1 and {why}");
            eprintln!("SKIPPED: {why}");
            return;
        }
        Boot::Exited(stderr) => assert!(
            reason(&stderr).contains("ring create"),
            "under {MEMLOCK_KIB_BELOW_ONE_RING} KiB a node fails for another reason: {stderr}"
        ),
    }
    // From here no node another test starts holds the budget: they wait.
    let lock = NodeLock::box_wide();
    let lease = lock.exclusive().unwrap_or_else(|error| {
        panic!(
            "the node lock: {error}; its takers: {}; its holders by fd 0: {}; the budget's \
             holders: {}",
            lock_takers(&lock),
            fd0_holders(&lock),
            holders(&lock)
        )
    });
    // Control: one cell has no sibling to race, so its refusal means the
    // budget is held elsewhere: by a process of this user outside the lock,
    // or by a node that just ended, whose rings and registered buffers the
    // kernel uncharges only after the process is gone. Wait until it boots.
    let deadline = Instant::now() + Duration::from_secs(10);
    while let Boot::Exited(stderr) = boot("1", MEMLOCK_KIB, Some(&lease)) {
        assert!(
            Instant::now() < deadline,
            "a one-cell node fails under {MEMLOCK_KIB} KiB: {}; the budget's holders: {}",
            reason(&stderr),
            holders(&lock)
        );
    }
    let mut failed = Vec::new();
    for index in 0..BOOTS {
        match boot(CELLS, MEMLOCK_KIB, Some(&lease)) {
            Boot::Served(stderr) => assert!(
                stderr.contains("fixed_buffers: false"),
                "boot {index}: the probe was not refused, so it never charged up to the \
                 limit: {stderr}"
            ),
            // The holders now, while one that failed this boot may still live.
            Boot::Exited(stderr) => failed.push(format!(
                "boot {index}: {}; the budget's holders: {}",
                reason(&stderr),
                holders(&lock)
            )),
        }
    }
    assert!(failed.is_empty(), "{} of {BOOTS} boots failed: {failed:#?}", failed.len());
}
