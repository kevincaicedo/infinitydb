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

use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

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

/// A spawned node, killed on drop: a failed assertion never leaves it
/// running and holding locked memory the next boot is charged against.
struct Spawned(Child);

impl Drop for Spawned {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// How one boot ended, with everything the node wrote to stderr.
enum Boot {
    Served(String),
    Exited(String),
}

/// Boots `infinityd` under a `memlock_kib` limit. Served means `DBSIZE`
/// answered: it scatters to every cell, so every cell's setup is done.
fn boot(cells: &str, memlock_kib: u32) -> Boot {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("probe bind")
        .local_addr()
        .expect("probe address")
        .port();
    let binary = std::env::var("INF_MEMLOCK_BOOT_BIN")
        .unwrap_or_else(|_| env!("CARGO_BIN_EXE_infinityd").to_owned());
    let mut node = Spawned(
        Command::new("sh")
            .args(["-c", r#"ulimit -l "$1" && shift && exec "$@""#, "sh"])
            .arg(memlock_kib.to_string())
            .arg(binary)
            .args(["--port", &port.to_string(), "--cells", cells])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn infinityd under sh"),
    );
    let child = &mut node.0;
    let deadline = Instant::now() + Duration::from_secs(30);
    let served = loop {
        if child.try_wait().expect("try_wait").is_some() {
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
        wait_for_exit(child, deadline);
    }
    let _ = child.kill();
    let _ = child.wait();
    let mut stderr = String::new();
    child.stderr.take().expect("piped stderr").read_to_string(&mut stderr).expect("stderr");
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

fn wait_for_exit(child: &mut Child, deadline: Instant) {
    while child.try_wait().expect("try_wait").is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// The line that says why a node exited (its last stderr line).
fn reason(stderr: &str) -> &str {
    stderr.lines().last().unwrap_or("(no stderr)")
}

/// The `Uid:` and `VmPin:` fields of one `/proc/<pid>/status`.
fn uid_and_pinned_kib(status: &str) -> (Option<&str>, u64) {
    let field = |name: &str| {
        status
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .and_then(|rest| rest.split_whitespace().next())
    };
    (field("Uid:"), field("VmPin:").and_then(|kib| kib.parse().ok()).unwrap_or(0))
}

/// This user's other processes with pinned memory: io_uring charges a
/// registered buffer to `VmPin` and to the same per-user budget the
/// control leg is refused by. A ring alone shows no `VmPin`.
fn pinned_peers() -> String {
    let own = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let own_uid = uid_and_pinned_kib(&own).0.unwrap_or("?").to_owned();
    let self_pid = std::process::id().to_string();
    let mut peers = Vec::new();
    for entry in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let pid = entry.file_name().to_string_lossy().into_owned();
        if pid == self_pid || !pid.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(status) = std::fs::read_to_string(entry.path().join("status")) else { continue };
        let (uid, pinned_kib) = uid_and_pinned_kib(&status);
        if uid == Some(own_uid.as_str()) && pinned_kib > 0 {
            let name = status.lines().next().and_then(|l| l.strip_prefix("Name:")).unwrap_or("");
            peers.push(format!("pid {pid} ({}) VmPin {pinned_kib} kB", name.trim()));
        }
    }
    if peers.is_empty() {
        "none with VmPin (a ring-only holder shows none)".to_owned()
    } else {
        peers.join(", ")
    }
}

/// `INF_MEMLOCK_BOOT_REQUIRE=1`: a skip is a failure (a runner whose
/// kernel stopped charging rings would otherwise pass silently).
fn skip_is_failure() -> bool {
    std::env::var_os("INF_MEMLOCK_BOOT_REQUIRE").is_some_and(|value| value == "1")
}

#[test]
fn a_node_boots_under_the_default_memlock_limit() {
    // Engagement: where rings are not charged (an older kernel, or
    // CAP_IPC_LOCK) the race cannot occur and this test cannot go red.
    match boot("1", MEMLOCK_KIB_BELOW_ONE_RING) {
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
    // Control: one cell has no sibling to race, so its refusal means the
    // budget is held elsewhere: by another process of this user, or by a
    // node an earlier test stopped, whose rings and registered buffers the
    // kernel uncharges only after the process is gone. Wait until it boots.
    let deadline = Instant::now() + Duration::from_secs(10);
    while let Boot::Exited(stderr) = boot("1", MEMLOCK_KIB) {
        assert!(
            Instant::now() < deadline,
            "a one-cell node fails under {MEMLOCK_KIB} KiB: {}; this user's processes \
             holding pinned memory: {}",
            reason(&stderr),
            pinned_peers()
        );
    }
    let mut failed = Vec::new();
    for index in 0..BOOTS {
        match boot(CELLS, MEMLOCK_KIB) {
            Boot::Served(stderr) => assert!(
                stderr.contains("fixed_buffers: false"),
                "boot {index}: the probe was not refused, so it never charged up to the \
                 limit: {stderr}"
            ),
            Boot::Exited(stderr) => failed.push(format!("boot {index}: {}", reason(&stderr))),
        }
    }
    assert!(failed.is_empty(), "{} of {BOOTS} boots failed: {failed:#?}", failed.len());
}
