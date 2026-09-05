#![allow(
    clippy::disallowed_methods,
    reason = "harness crate: process deadlines and run stamps, not cell code"
)]
//! Shared process plumbing and the matrix-compare loop for the compat
//! lanes: the pinned redis-server oracle (spawned from PATH, or
//! `INF_COMPAT_ORACLE_ADDR` for the dockerized CI pin) and the real
//! `infinityd` candidate (`INFINITYD_BIN` — review 2026-08-30,
//! F-L19-09: until this mode existed, every compat claim was proven
//! against one in-process `Keyspace` with no cells, no namespaces and
//! no tier).

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::matrix::{Case, Check};
use crate::resp::{encode_command, frame_len};

/// A spawned server process killed (and its scratch directory removed)
/// on drop.
pub struct ProcessGuard {
    child: Child,
    scratch: Option<PathBuf>,
}

impl Drop for ProcessGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(dir) = &self.scratch {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

fn free_port() -> u16 {
    let probe = TcpListener::bind("127.0.0.1:0").expect("probe bind");
    probe.local_addr().expect("addr").port()
}

/// Pinned-oracle mode: connect to an externally managed redis-server
/// (the dockerized CI oracle). Panics if the address never answers —
/// CI asked for a pinned oracle, so silently skipping would be a lie.
fn connect_external(addr: &str) -> TcpStream {
    let deadline = Instant::now() + Duration::from_secs(10);
    let stream = loop {
        match TcpStream::connect(addr) {
            Ok(s) => break s,
            Err(_) if Instant::now() < deadline => {
                // Test orchestration thread — not cell code.
                #[allow(clippy::disallowed_methods)]
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => panic!("INF_COMPAT_ORACLE_ADDR={addr} never answered: {e}"),
        }
    };
    stream.set_read_timeout(Some(Duration::from_secs(5))).expect("timeout");
    stream
}

fn spawn_redis() -> Option<(ProcessGuard, TcpStream)> {
    let port = free_port();
    // A scratch working directory per oracle (found 2026-09-01 while
    // building the node lane): the matrix's `BGSAVE` case makes the
    // oracle write `dump.rdb` into its cwd — the package directory —
    // and the *next* spawned oracle loads it at boot, so one leftover
    // key (`oomk`) shifted every later DBSIZE by one. An oracle must
    // not be able to leave state for its successor.
    let dir = std::env::temp_dir().join(format!("inf-compat-oracle-{port}"));
    std::fs::create_dir_all(&dir).ok()?;
    let child = Command::new("redis-server")
        .args([
            "--port",
            &port.to_string(),
            "--save",
            "",
            "--appendonly",
            "no",
            "--bind",
            "127.0.0.1",
            "--enable-debug-command",
            "yes",
        ])
        .current_dir(&dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let guard = ProcessGuard { child, scratch: Some(dir) };
    let deadline = Instant::now() + Duration::from_secs(10);
    let stream = loop {
        match TcpStream::connect(("127.0.0.1", port)) {
            Ok(s) => break s,
            Err(_) if Instant::now() < deadline => {
                // Test orchestration thread waiting on a child process —
                // not cell code (the deny-list protects the data plane).
                #[allow(clippy::disallowed_methods)]
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => panic!("redis-server never came up on {port}: {e}"),
        }
    };
    stream.set_read_timeout(Some(Duration::from_secs(5))).expect("timeout");
    Some((guard, stream))
}

/// The redis oracle: `INF_COMPAT_ORACLE_ADDR` when pinned (CI), else a
/// throwaway spawn from PATH. `None` means redis-server is not
/// installed — the caller prints the loud SKIP marker.
pub fn oracle() -> Option<(Option<ProcessGuard>, TcpStream)> {
    match std::env::var("INF_COMPAT_ORACLE_ADDR") {
        Ok(addr) => Some((None, connect_external(&addr))),
        Err(_) => spawn_redis().map(|(guard, stream)| (Some(guard), stream)),
    }
}

/// The real-node candidate (F-L19-09): spawns `$INFINITYD_BIN` with
/// `cells` cells and a fresh durable root under `scratch_base`, waits
/// for readiness (`PING` → `+PONG`; `-LOADING` retries), and returns a
/// connected stream. `None` when `INFINITYD_BIN` is unset — the caller
/// prints the loud SKIP marker. A set-but-broken binary **panics**: an
/// asked-for candidate must not silently skip (the F-L19-11 principle).
pub fn infinityd(cells: u16, scratch_base: &Path) -> Option<(ProcessGuard, TcpStream)> {
    let bin = std::env::var("INFINITYD_BIN").ok()?;
    let port = free_port();
    let dir = scratch_base.join(format!(
        "inf-compat-node-{}-{port}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("create node scratch dir");
    let log = std::fs::File::create(dir.join("infinityd.log")).expect("create node log");
    let child = Command::new(&bin)
        .args([
            "--port",
            &port.to_string(),
            "--cells",
            &cells.to_string(),
            "--data-dir",
            dir.to_str().expect("utf-8 scratch path"),
            "--device-probe",
            "off",
        ])
        .stdout(Stdio::null())
        .stderr(log)
        .spawn()
        .unwrap_or_else(|e| panic!("INFINITYD_BIN={bin} failed to spawn: {e}"));
    let guard = ProcessGuard { child, scratch: Some(dir.clone()) };
    let deadline = Instant::now() + Duration::from_secs(30);
    let stream = loop {
        if Instant::now() >= deadline {
            let log = std::fs::read_to_string(dir.join("infinityd.log")).unwrap_or_default();
            panic!("infinityd never answered PING on {port}; log tail:\n{log}");
        }
        if let Ok(mut s) = TcpStream::connect(("127.0.0.1", port)) {
            s.set_read_timeout(Some(Duration::from_secs(5))).expect("timeout");
            if s.write_all(b"*1\r\n$4\r\nPING\r\n").is_ok() {
                let mut buf = Vec::new();
                let reply = read_frames(&mut s, &mut buf, 1);
                if reply == b"+PONG\r\n" {
                    break s;
                }
                // `-LOADING …` while recovery replays: retry below.
            }
        }
        // Test orchestration thread — not cell code.
        #[allow(clippy::disallowed_methods)]
        std::thread::sleep(Duration::from_millis(50));
    };
    Some((guard, stream))
}

/// Reads exactly `n` complete RESP frames from `stream`, buffering
/// across reads in `buf`, and returns their concatenated bytes.
pub fn read_frames(stream: &mut TcpStream, buf: &mut Vec<u8>, n: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for _ in 0..n {
        loop {
            if let Some(len) = frame_len(buf).expect("server sent invalid RESP") {
                out.extend_from_slice(&buf[..len]);
                buf.drain(..len);
                break;
            }
            let mut chunk = [0u8; 4096];
            let read = stream.read(&mut chunk).expect("server read");
            assert!(read > 0, "server closed the connection mid-script");
            buf.extend_from_slice(&chunk[..read]);
        }
    }
    out
}

/// Parses `:N\r\n`.
pub fn parse_int_reply(reply: &[u8]) -> Option<i64> {
    let text = reply.strip_prefix(b":")?.strip_suffix(b"\r\n")?;
    std::str::from_utf8(text).ok()?.parse().ok()
}

/// How many complete RESP frames exactly cover `buf` (`None` when the
/// bytes are not whole frames).
pub fn count_frames(buf: &[u8]) -> Option<usize> {
    let mut at = 0;
    let mut frames = 0;
    while at < buf.len() {
        match frame_len(&buf[at..]).ok()? {
            Some(n) => {
                at += n;
                frames += 1;
            }
            None => return None,
        }
    }
    Some(frames)
}

/// One expected candidate-topology divergence (the node lanes): consulted
/// only when the default compare FAILS, so a case that matches the oracle
/// (e.g. the mid-script `FLUSHALL` before any durable namespace exists)
/// is never excused by an override on the same argv. The expectation pins
/// exact bytes or an exact shape — drift in a deviation is itself a
/// finding.
pub struct CaseOverride {
    pub argv: &'static [&'static str],
    pub expect: Expect,
    pub why: &'static str,
}

pub enum Expect {
    /// The candidate must answer exactly these bytes (a declared typed
    /// refusal, e.g. ADR-0015's M2 cut lines).
    CandidateExact(&'static [u8]),
    /// The candidate's frames are a permutation of the oracle's frames
    /// (frame *content* byte-exact, order divergent — a filed ordering
    /// finding, never a silent pass).
    FramePermutation,
}

fn split_frames(buf: &[u8]) -> Option<Vec<&[u8]>> {
    let mut at = 0;
    let mut frames = Vec::new();
    while at < buf.len() {
        let n = frame_len(&buf[at..]).ok()??;
        frames.push(&buf[at..at + n]);
        at += n;
    }
    Some(frames)
}

/// Whether a failing case is an expected, pinned divergence.
fn overridden(
    overrides: &[CaseOverride],
    case: &Case,
    oracle_reply: &[u8],
    candidate_reply: &[u8],
) -> Option<&'static str> {
    let o = overrides.iter().find(|o| o.argv == case.argv)?;
    let holds = match o.expect {
        Expect::CandidateExact(bytes) => candidate_reply == bytes,
        Expect::FramePermutation => {
            let (Some(mut a), Some(mut b)) =
                (split_frames(oracle_reply), split_frames(candidate_reply))
            else {
                return None;
            };
            a.sort_unstable();
            b.sort_unstable();
            a == b
        }
    };
    holds.then_some(o.why)
}

/// Outcome of one scripted-matrix run against one candidate.
pub struct MatrixReport {
    pub compared: usize,
    pub skipped: usize,
    pub failures: Vec<String>,
    /// Cases rescued by a [`CaseOverride`] — printed, never silent.
    pub deviations: Vec<String>,
}

/// Runs the scripted `matrix` against the oracle stream and one
/// candidate, diffing per the case's `Check` mode. `exec` executes one
/// encoded command on the candidate and returns its raw reply bytes;
/// its second argument is the frame count this case produces (1 except
/// `Check::Frames(n)`) — the TCP candidate must read exactly that many,
/// the in-process candidate may ignore it (its executor returns every
/// frame the command emitted).
pub fn run_matrix(
    matrix: &[Case],
    oracle: &mut TcpStream,
    overrides: &[CaseOverride],
    mut exec: impl FnMut(&[u8], usize) -> Vec<u8>,
) -> MatrixReport {
    let mut oracle_buf = Vec::new();
    let mut failures = Vec::new();
    let mut deviations = Vec::new();
    let mut skipped = 0;

    for (i, case) in matrix.iter().enumerate() {
        let argv: Vec<String> = case.argv.iter().map(|s| (*s).to_string()).collect();
        let wire = encode_command(&argv);
        let frames = match case.check {
            Check::Frames(n) => n,
            _ => 1,
        };

        oracle.write_all(&wire).expect("oracle write");
        let oracle_reply = read_frames(oracle, &mut oracle_buf, frames);
        let candidate_reply = exec(&wire, frames);

        match case.check {
            Check::ByteExact => {
                // One command, one reply — asserted structurally, not just
                // by the byte compare below: a candidate reply that splits
                // into two frames desynchronises the connection even when
                // its first frame matches (review 2026-08-30, C6).
                if count_frames(&candidate_reply) != Some(1) {
                    failures.push(format!(
                        "case {i} {:?}: candidate answered {:?} frames, not 1:\n  {:?}",
                        case.argv,
                        count_frames(&candidate_reply),
                        String::from_utf8_lossy(&candidate_reply),
                    ));
                }
                if oracle_reply != candidate_reply {
                    if let Some(why) = overridden(overrides, case, &oracle_reply, &candidate_reply)
                    {
                        deviations.push(format!("case {i} {:?}: {why}", case.argv));
                    } else {
                        failures.push(format!(
                            "case {i} {:?}:\n  oracle    {:?}\n  candidate {:?}",
                            case.argv,
                            String::from_utf8_lossy(&oracle_reply),
                            String::from_utf8_lossy(&candidate_reply),
                        ));
                    }
                }
            }
            Check::Frames(n) => {
                // One command, N frames (pub/sub confirmations/deliveries):
                // the concatenation is compared byte-exact.
                let candidate_frames = count_frames(&candidate_reply);
                if oracle_reply != candidate_reply || candidate_frames != Some(n) {
                    if candidate_frames == Some(n)
                        && let Some(why) =
                            overridden(overrides, case, &oracle_reply, &candidate_reply)
                    {
                        deviations.push(format!("case {i} {:?}: {why}", case.argv));
                    } else {
                        failures.push(format!(
                            "case {i} {:?} ({n} frames, candidate has {candidate_frames:?}):\n  oracle    {:?}\n  candidate {:?}",
                            case.argv,
                            String::from_utf8_lossy(&oracle_reply),
                            String::from_utf8_lossy(&candidate_reply),
                        ));
                    }
                }
            }
            Check::IntWithin(tolerance) => {
                let (Some(a), Some(b)) =
                    (parse_int_reply(&oracle_reply), parse_int_reply(&candidate_reply))
                else {
                    failures.push(format!(
                        "case {i} {:?}: non-integer replies (oracle {:?}, candidate {:?})",
                        case.argv,
                        String::from_utf8_lossy(&oracle_reply),
                        String::from_utf8_lossy(&candidate_reply),
                    ));
                    continue;
                };
                if (a - b).abs() > tolerance {
                    failures
                        .push(format!("case {i} {:?}: {a} vs {b} exceeds ±{tolerance}", case.argv));
                }
            }
            Check::SkipDiff(why) => {
                skipped += 1;
                // The candidate reply must still be complete RESP frames.
                assert!(
                    count_frames(&candidate_reply).is_some_and(|n| n >= 1),
                    "case {i} {:?} ({why}): candidate reply is not complete frames",
                    case.argv
                );
            }
        }
    }

    MatrixReport { compared: matrix.len() - skipped, skipped, failures, deviations }
}
