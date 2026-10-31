#![allow(
    clippy::disallowed_types,
    reason = "test-only: filesystem fixtures outside cell code (ADR-0144 D5)"
)]
#![allow(clippy::disallowed_methods, reason = "test target: process deadlines, not cell code")]
//! A segment smaller than the largest frame the staging ring can seal is a
//! startup refusal at the binary, never a stop while serving (ADR-0191 D3).
//! A segment holds at least one frame, so `--segment-bytes` must be at least
//! `--log-staging-mib` × 2²⁰, and under the fua class a multiple of 4096.
//! `infinityd` judges the pair once the device probe has named the barrier
//! class, before it listens or starts a cell, and exits 2 with one line
//! naming both flags and the numbers. These boots exit after the catalog
//! load, the control thread and the probe, so they live here and not in
//! `boot_flags.rs`, whose cases exit at the parser.
//!
//! Harness: `--port 0`, `--device-probe off`, `--data-dir` set (a durable
//! node, the only kind with a log). Readiness is a `DBSIZE` answer and a
//! refusal is the exit. Every child is killed when its handle drops, so a
//! boot that serves cannot outlive its test.
#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// A launched node and its scratch directory; dropping it kills the child
/// before the directory goes.
struct Node {
    child: Child,
    root: PathBuf,
    stderr: PathBuf,
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// How a boot ended within the deadline.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    /// The node answered `DBSIZE`: it listened and served.
    Served,
    /// The process exited before it served, with this code.
    Exited(Option<i32>),
    /// Neither within the deadline.
    Stalled,
}

/// A boot's outcome and everything it wrote to stderr.
#[derive(Debug)]
struct Boot {
    outcome: Outcome,
    stderr: String,
}

fn launch(tag: &str, flags: &[&str]) -> Node {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/e2e-log-geometry")
        .join(format!("{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("data dir");
    let stderr = root.join("stderr");
    let child = Command::new(env!("CARGO_BIN_EXE_infinityd"))
        .args(["--port", "0", "--cells", "1", "--device-probe", "off", "--data-dir"])
        .arg(&root)
        .args(flags)
        .stdout(Stdio::null())
        .stderr(Stdio::from(std::fs::File::create(&stderr).expect("stderr file")))
        .spawn()
        .expect("spawn infinityd");
    Node { child, root, stderr }
}

/// Boots `infinityd` with `flags` and reads how it ended. The node is
/// killed on return, whichever way it ended.
fn boot(tag: &str, flags: &[&str]) -> Boot {
    let mut node = launch(tag, flags);
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let stderr = std::fs::read_to_string(&node.stderr).unwrap_or_default();
        if let Some(status) = node.child.try_wait().expect("try_wait") {
            let stderr = std::fs::read_to_string(&node.stderr).unwrap_or_default();
            return Boot { outcome: Outcome::Exited(status.code()), stderr };
        }
        if announced_port(&stderr).is_some_and(serving) {
            return Boot { outcome: Outcome::Served, stderr };
        }
        if Instant::now() >= deadline {
            return Boot { outcome: Outcome::Stalled, stderr };
        }
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
    let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) else { return false };
    stream.set_read_timeout(Some(Duration::from_secs(5))).expect("timeout");
    if stream.write_all(b"*1\r\n$6\r\nDBSIZE\r\n").is_err() {
        return false;
    }
    let mut buf = [0u8; 64];
    match stream.read(&mut buf) {
        Ok(n) => buf[..n].starts_with(b":"),
        Err(_) => false,
    }
}

/// The refusal: exit 2, one line naming both flags and every `words`, no
/// panic, and the node never listened.
fn assert_refused(boot: &Boot, words: &[&str]) {
    assert_eq!(boot.outcome, Outcome::Exited(Some(2)), "expected exit 2 before listen: {boot:?}");
    let stderr = &boot.stderr;
    let line = stderr
        .lines()
        .find(|line| line.starts_with("infinityd: --segment-bytes/--log-staging-mib: "))
        .unwrap_or_else(|| panic!("no refusal line naming both flags:\n{stderr}"));
    for word in words {
        assert!(line.contains(word), "the refusal line does not name {word:?}: {line}");
    }
    assert!(!stderr.contains("panicked"), "the refusal panicked:\n{stderr}");
    assert!(announced_port(stderr).is_none(), "the node listened before refusing:\n{stderr}");
}

/// The pair the simulator first met: a 64 KiB segment under the default
/// 4 MiB staging ring, whose first LOG step past 64 KiB of records seals a
/// frame no segment holds.
#[test]
fn a_segment_below_the_staging_frame_is_refused_before_listen() {
    let boot = boot("ledger-pair", &["--segment-bytes", "65536", "--log-staging-mib", "4"]);
    assert_refused(&boot, &["65536", "4194304", "raise --segment-bytes to at least 4194304"]);
}

/// The control leg: the floor pair, a segment exactly one frame long, serves.
#[test]
fn a_segment_of_one_staging_frame_serves() {
    let boot = boot("floor", &["--segment-bytes", "4194304", "--log-staging-mib", "4"]);
    assert_eq!(boot.outcome, Outcome::Served, "the floor pair did not serve: {boot:?}");
}

/// One byte below the floor under the flush class: the segment row.
#[test]
fn one_byte_below_the_floor_is_refused() {
    let boot = boot(
        "one-short",
        &["--segment-bytes", "4194303", "--log-staging-mib", "4", "--barrier-class", "flush"],
    );
    assert_refused(&boot, &["4194303", "4194304"]);
}

/// The same value under the fua class meets the alignment row first: an
/// aligned frame needs an aligned segment end.
#[test]
fn an_unaligned_segment_under_fua_is_refused() {
    let boot = boot(
        "one-short-fua",
        &["--segment-bytes", "4194303", "--log-staging-mib", "4", "--barrier-class", "fua"],
    );
    assert_refused(&boot, &["4194303", "4096"]);
}

/// A zero segment holds no frame.
#[test]
fn a_zero_segment_is_refused() {
    let boot = boot("zero", &["--segment-bytes", "0"]);
    assert_refused(&boot, &["segment_bytes 0", "4194304"]);
}
