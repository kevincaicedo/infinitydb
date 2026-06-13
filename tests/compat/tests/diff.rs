//! M0-S15 AC: the command × edge-case matrix replied byte-identical to real
//! Redis (allowlisted introspection payloads excepted, per the AC).
//!
//! Spawns a throwaway `redis-server` (no persistence) as the oracle and the
//! in-process executor as the candidate, runs the scripted matrix on both,
//! and diffs raw reply bytes per the case's `Check` mode. Skips (with a loud
//! marker) when `redis-server` is not installed.
//!
//! Oracle pinning (M1-S14): when `INF_COMPAT_ORACLE_ADDR=host:port` is set,
//! the harness connects to that server instead of spawning one — CI runs the
//! pinned `redis:8.0.5` container (started with `--enable-debug-command yes`,
//! no persistence) so the oracle version can never drift with the runner's
//! apt archive. The local dev path (spawn from PATH) is unchanged.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use compat::candidate::Candidate;
use compat::matrix::{Check, MATRIX};
use compat::resp::{encode_command, frame_len};

struct RedisGuard {
    child: Child,
}

impl Drop for RedisGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
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

fn spawn_redis() -> Option<(RedisGuard, TcpStream)> {
    let port = {
        let probe = TcpListener::bind("127.0.0.1:0").expect("probe bind");
        probe.local_addr().expect("addr").port()
    };
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
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let guard = RedisGuard { child };
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

fn read_one_reply(stream: &mut TcpStream, buf: &mut Vec<u8>) -> Vec<u8> {
    loop {
        if let Some(n) = frame_len(buf).expect("oracle sent invalid RESP") {
            let reply = buf[..n].to_vec();
            buf.drain(..n);
            return reply;
        }
        let mut chunk = [0u8; 4096];
        let n = stream.read(&mut chunk).expect("oracle read");
        assert!(n > 0, "oracle closed the connection mid-script");
        buf.extend_from_slice(&chunk[..n]);
    }
}

fn parse_int_reply(reply: &[u8]) -> Option<i64> {
    let text = reply.strip_prefix(b":")?.strip_suffix(b"\r\n")?;
    std::str::from_utf8(text).ok()?.parse().ok()
}

/// How many complete RESP frames exactly cover `buf` (`None` when the bytes
/// are not whole frames).
fn count_frames(buf: &[u8]) -> Option<usize> {
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

#[test]
fn matrix_replies_match_redis() {
    let (_guard, mut oracle) = match std::env::var("INF_COMPAT_ORACLE_ADDR") {
        Ok(addr) => (None, connect_external(&addr)),
        Err(_) => match spawn_redis() {
            Some((guard, stream)) => (Some(guard), stream),
            None => {
                eprintln!("SKIPPED: redis-server not installed — compat AC stays evidence-pending");
                return;
            }
        },
    };
    let mut candidate = Candidate::new();
    let mut oracle_buf = Vec::new();
    let mut failures = Vec::new();
    let mut skipped = 0;

    for (i, case) in MATRIX.iter().enumerate() {
        let argv: Vec<String> = case.argv.iter().map(|s| (*s).to_string()).collect();
        let wire = encode_command(&argv);

        oracle.write_all(&wire).expect("oracle write");
        let oracle_reply = read_one_reply(&mut oracle, &mut oracle_buf);
        let candidate_reply = candidate.execute_wire(&wire);

        match case.check {
            Check::ByteExact => {
                if oracle_reply != candidate_reply {
                    failures.push(format!(
                        "case {i} {:?}:\n  oracle    {:?}\n  candidate {:?}",
                        case.argv,
                        String::from_utf8_lossy(&oracle_reply),
                        String::from_utf8_lossy(&candidate_reply),
                    ));
                }
            }
            Check::Frames(n) => {
                // One command, N frames (pub/sub confirmations/deliveries):
                // the concatenation is compared byte-exact.
                let mut oracle_all = oracle_reply;
                for _ in 1..n {
                    oracle_all.extend_from_slice(&read_one_reply(&mut oracle, &mut oracle_buf));
                }
                let candidate_frames = count_frames(&candidate_reply);
                if oracle_all != candidate_reply || candidate_frames != Some(n) {
                    failures.push(format!(
                        "case {i} {:?} ({n} frames, candidate has {candidate_frames:?}):\n  oracle    {:?}\n  candidate {:?}",
                        case.argv,
                        String::from_utf8_lossy(&oracle_all),
                        String::from_utf8_lossy(&candidate_reply),
                    ));
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

    let compared = MATRIX.len() - skipped;
    println!(
        "compat-diff v1: {compared} byte-compared cases, {skipped} documented deviations, {} failures",
        failures.len()
    );
    assert!(
        failures.is_empty(),
        "{} mismatches vs real Redis:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
