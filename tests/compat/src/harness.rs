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
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::matrix::{Case, Check};
use crate::resp::{encode_command, frame_len};

/// A spawned server process killed on drop. The scratch directory goes
/// with it — unless the dropping thread is panicking and the process
/// wrote a log there: a failing test keeps the directory and prints the
/// log tail (batch 59: two flakes in batches 57/58 left no evidence
/// because the node died mid-test and this `rm` took `infinityd.log`).
pub struct ProcessGuard {
    child: Child,
    scratch: Option<PathBuf>,
    /// The process's stderr file inside `scratch`, when it has one.
    log: Option<PathBuf>,
}

/// Bytes of the kept log printed on failure.
const LOG_TAIL_BYTES: usize = 16 * 1024;

impl Drop for ProcessGuard {
    fn drop(&mut self) {
        // Whether the process ended on its own before the test did — a
        // node that died mid-test is the evidence this guard used to erase.
        let exited = self.child.try_wait().ok().flatten();
        let _ = self.child.kill();
        let _ = self.child.wait();
        let Some(dir) = self.scratch.take() else { return };
        if std::thread::panicking()
            && let Some(log) = &self.log
        {
            let bytes = std::fs::read(log).unwrap_or_default();
            let tail = &bytes[bytes.len().saturating_sub(LOG_TAIL_BYTES)..];
            let state = match exited {
                Some(status) => format!("exited before the test ended: {status}"),
                None => "still running at teardown (killed)".to_string(),
            };
            eprintln!(
                "--- kept {} (test failed; process {state}); log tail:\n{}--- end of log tail",
                dir.display(),
                String::from_utf8_lossy(tail)
            );
            return;
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}

impl ProcessGuard {
    /// The scratch directory (kept on failure — see the type doc).
    pub fn scratch_dir(&self) -> Option<&Path> {
        self.scratch.as_deref()
    }

    /// The spawned process's pid (what `INFO server:process_id` must name).
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Waits up to `timeout` for the process to exit on its own; `None`
    /// when it is still running (a refusal a test asserts on must exit).
    pub fn wait_exit(&mut self, timeout: Duration) -> Option<std::process::ExitStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Ok(Some(status)) = self.child.try_wait() {
                return Some(status);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// The process's log so far (empty when it has none).
    pub fn log_text(&self) -> String {
        self.log
            .as_ref()
            .map(|p| std::fs::read_to_string(p).unwrap_or_default())
            .unwrap_or_default()
    }
}

/// A port for one spawned server, never repeated within this process
/// (batch 59). The old `bind(0)`-then-drop probe released the port before
/// the node bound it, and the kernel's ephemeral pick repeats within a few
/// hundred draws — in the full lane two spawning tests took one port and
/// the second `infinityd` *joined* the first's `SO_REUSEPORT` group (the
/// batch-57 "eight PONGs" and the batch-58 reset). Now: a process-wide
/// counter from a per-process base, each candidate confirmed free by a
/// plain bind (which a foreign reuseport group also refuses), so two
/// callers here never share a port and a foreign owner is skipped.
///
/// The range sits below the kernel's ephemeral floor (`ip_local_port_range`
/// starts at 32768 by default; batch 61): a process that took its port
/// from `bind(0)` — every probe script, a leftover measurement node —
/// can never land on a harness port, and only a hand-chosen `--port`
/// remains, which readiness's identity check names.
pub fn reserve_port() -> u16 {
    use std::sync::atomic::{AtomicU16, Ordering};
    // The base is per process (pid-derived) so two harness processes on
    // one box start from different points of the range.
    static NEXT: AtomicU16 = AtomicU16::new(0);
    const LOW: u16 = 20_000;
    const SPAN: u16 = 12_768;
    if NEXT.load(Ordering::Relaxed) == 0 {
        let base = LOW + u16::try_from(std::process::id() % u32::from(SPAN)).expect("< SPAN");
        let _ = NEXT.compare_exchange(0, base, Ordering::Relaxed, Ordering::Relaxed);
    }
    for _ in 0..SPAN {
        let mut port = NEXT.fetch_add(1, Ordering::Relaxed);
        if !(LOW..LOW + SPAN).contains(&port) {
            port = LOW;
            NEXT.store(LOW + 1, Ordering::Relaxed);
        }
        // Bind-only (batch 61): a listening probe copied into a sibling
        // test's spawning child (posix_spawn keeps the fd table until the
        // exec) answered the readiness `PING` and reset it at the exec.
        let addr = std::net::SocketAddrV4::new(std::net::Ipv4Addr::LOCALHOST, port);
        if inf_server::probe_addr_unowned(addr).is_ok() {
            return port;
        }
    }
    panic!("no free port in {LOW}..{}", LOW + SPAN);
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
    let port = reserve_port();
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
    let guard = ProcessGuard { child, scratch: Some(dir), log: None };
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
/// connected stream. `None` when optional and `INFINITYD_BIN` is unset.
/// `INF_COMPAT_REQUIRE_BINARY=1` forbids skipping. A broken binary panics: an
/// asked-for candidate must not silently skip (the F-L19-11 principle).
pub fn infinityd(cells: u16, scratch_base: &Path) -> Option<(ProcessGuard, TcpStream)> {
    let bin = candidate_binary()?;
    match infinityd_at(&bin, cells, reserve_port(), scratch_base) {
        Ok(ready) => Some(ready),
        Err(refused) => panic!("{refused}"),
    }
}

/// The candidate binary path when the lane is on (`None` = skip).
pub fn candidate() -> Option<String> {
    candidate_binary()
}

/// Spawns `bin` on an explicit `port` and waits for readiness. `Err`
/// carries the story when the process exits before answering `PING`
/// (its status and log tail); a process that neither answers nor exits
/// within 30 s panics with the log tail. A `PONG` is only readiness when
/// `INFO server:process_id` names the spawned child (batch 61): a foreign
/// node already on the port answers first, before the child reaches its
/// owned-port refusal, and pre-fix the test was silently paired with it.
/// A test for the owned-port refusal spawns with [`spawn_infinityd_at`]
/// and waits for the exit instead.
///
/// # Errors
/// The process exited before readiness (the message names the exit
/// status and quotes the log tail), or another process answered on the
/// port (the message names its pid and command line).
pub fn infinityd_at(
    bin: &str,
    cells: u16,
    port: u16,
    scratch_base: &Path,
) -> Result<(ProcessGuard, TcpStream), String> {
    let mut guard = spawn_infinityd_at(bin, cells, port, scratch_base);
    let dir = guard.scratch.clone().expect("spawned node has a scratch dir");
    let deadline = Instant::now() + Duration::from_secs(30);
    let stream = loop {
        if let Ok(Some(status)) = guard.child.try_wait() {
            let log = std::fs::read_to_string(dir.join("infinityd.log")).unwrap_or_default();
            return Err(format!(
                "infinityd exited before answering PING on {port}: {status}; log tail:\n{log}"
            ));
        }
        if Instant::now() >= deadline {
            let log = std::fs::read_to_string(dir.join("infinityd.log")).unwrap_or_default();
            panic!("infinityd never answered PING on {port}; log tail:\n{log}");
        }
        if let Ok(mut s) = TcpStream::connect(("127.0.0.1", port)) {
            s.set_read_timeout(Some(Duration::from_secs(5))).expect("timeout");
            if s.write_all(b"*1\r\n$4\r\nPING\r\n").is_ok() {
                let mut buf = Vec::new();
                // A reset or EOF here is a foreign socket on the port or a
                // dying node — name what the node was doing (batch 61).
                let mut probe = [0u8; 64];
                let first = s.read(&mut probe).and_then(|n| {
                    if n == 0 { Err(std::io::Error::other("EOF before any reply")) } else { Ok(n) }
                });
                match first {
                    Ok(n) => buf.extend_from_slice(&probe[..n]),
                    Err(e) => {
                        // Test orchestration thread — not cell code.
                        #[allow(clippy::disallowed_methods)]
                        std::thread::sleep(Duration::from_millis(300));
                        let exit = guard.child.try_wait().ok().flatten();
                        let log =
                            std::fs::read_to_string(dir.join("infinityd.log")).unwrap_or_default();
                        return Err(format!(
                            "readiness read on port {port} failed: {e} (spawned infinityd pid {} \
                             300 ms later: {exit:?}); its log:\n{log}",
                            guard.child.id()
                        ));
                    }
                }
                let reply = read_frames(&mut s, &mut buf, 1);
                if reply == b"+PONG\r\n" {
                    let child = guard.child.id();
                    match info_process_id(&mut s, &mut buf) {
                        Some(pid) if pid == child => break s,
                        Some(pid) => {
                            return Err(format!(
                                "port {port} is answered by pid {pid} ({}), not the spawned \
                                 infinityd (pid {child}) — a foreign node owns the port",
                                cmdline_of(pid)
                            ));
                        }
                        None => {
                            return Err(format!("port {port}: INFO server names no process_id"));
                        }
                    }
                }
                // `-LOADING …` while recovery replays: retry below.
            }
        }
        // Test orchestration thread — not cell code.
        #[allow(clippy::disallowed_methods)]
        std::thread::sleep(Duration::from_millis(50));
    };
    Ok((guard, stream))
}

/// `INFO server:process_id` over `stream` (`None` when absent).
fn info_process_id(stream: &mut TcpStream, buf: &mut Vec<u8>) -> Option<u32> {
    stream.write_all(b"*2\r\n$4\r\nINFO\r\n$6\r\nserver\r\n").ok()?;
    let reply = read_frames(stream, buf, 1);
    let text = String::from_utf8_lossy(&reply);
    text.lines().find_map(|l| l.strip_prefix("process_id:")).and_then(|v| v.trim().parse().ok())
}

/// A pid's command line for the refusal message (Linux; empty elsewhere).
fn cmdline_of(pid: u32) -> String {
    std::fs::read(format!("/proc/{pid}/cmdline"))
        .map(|bytes| {
            String::from_utf8_lossy(&bytes)
                .split('\0')
                .filter(|a| !a.is_empty())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default()
}

/// Spawns `bin` on `port` with a fresh scratch dir and its stderr in
/// `infinityd.log` there — no readiness wait.
pub fn spawn_infinityd_at(bin: &str, cells: u16, port: u16, scratch_base: &Path) -> ProcessGuard {
    let dir = scratch_base.join(format!(
        "inf-compat-node-{}-{port}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("create node scratch dir");
    let log_path = dir.join("infinityd.log");
    let log = std::fs::File::create(&log_path).expect("create node log");
    let child = Command::new(bin)
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
    ProcessGuard { child, scratch: Some(dir), log: Some(log_path) }
}

fn candidate_binary() -> Option<String> {
    let required = match std::env::var("INF_COMPAT_REQUIRE_BINARY") {
        Err(std::env::VarError::NotPresent) => false,
        Ok(value) if value == "1" => true,
        _ => panic!("INF_COMPAT_REQUIRE_BINARY must be unset or 1"),
    };
    let bin = std::env::var("INFINITYD_BIN");
    if required {
        assert!(
            bin.as_ref().is_ok_and(|path| !path.is_empty()),
            "INF_COMPAT_REQUIRE_BINARY=1 requires INFINITYD_BIN"
        );
    }
    bin.ok()
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Batch 59: the allocator never hands one port to two callers of the
    /// same process, however briefly the first holds it. The old probe
    /// (`bind(0)`, then drop) released the port before the node bound it,
    /// and the kernel's random ephemeral pick repeats within a few
    /// thousand draws (birthday bound) — in the full lane two spawning
    /// tests took one port, and the second node *joined* the first's
    /// reuseport group (batch 57's "eight PONGs", batch 58's reset).
    /// Batch 61 — the compat flake's mechanism: `posix_spawn` copies the
    /// parent's fd table until the child execs, so a *listening* probe
    /// socket alive in one thread at the instant another thread spawns a
    /// node lives on in that child for the exec's duration (milliseconds
    /// for a debug `infinityd` under load) — accepting the readiness
    /// `PING` on the just-reserved port and resetting it at exec. The
    /// probe must never listen: a bound, non-listening socket refuses.
    #[test]
    fn a_reserved_port_is_never_answered_by_a_ghost_listener() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let spawners: Vec<_> = (0..3)
            .map(|_| {
                let stop = std::sync::Arc::clone(&stop);
                std::thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        if let Ok(mut c) = Command::new("/bin/true").spawn() {
                            let _ = c.wait();
                        }
                    }
                })
            })
            .collect();
        let mut ghosts = Vec::new();
        for _ in 0..4000 {
            let port = reserve_port();
            match TcpStream::connect(("127.0.0.1", port)) {
                Ok(_) => ghosts.push(port),
                Err(e) => {
                    assert_eq!(e.kind(), std::io::ErrorKind::ConnectionRefused, "{port}: {e}")
                }
            }
        }
        stop.store(true, Ordering::Relaxed);
        for t in spawners {
            t.join().expect("spawner");
        }
        assert!(ghosts.is_empty(), "a spawned child answered on reserved ports {ghosts:?}");
    }

    #[test]
    fn reserved_ports_never_repeat_within_a_process() {
        let mut seen = std::collections::HashSet::new();
        for i in 0..2000 {
            let port = reserve_port();
            assert!(seen.insert(port), "port {port} handed out twice (draw {i})");
            // Batch 61: below the kernel's ephemeral floor, so no
            // `bind(0)` process can ever be on a harness port.
            assert!((20_000..32_768).contains(&port), "port {port} inside the ephemeral range");
        }
    }
}
