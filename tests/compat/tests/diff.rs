//! M0-S15 AC: the command × edge-case matrix replied byte-identical to real
//! Redis (allowlisted introspection payloads excepted, per the AC).
//!
//! Spawns a throwaway `redis-server` (no persistence) as the oracle and the
//! in-process executor as the candidate, runs the scripted matrix on both,
//! and diffs raw reply bytes per the case's `Check` mode. Skips (with a loud
//! marker) when `redis-server` is not installed.

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

#[test]
fn matrix_replies_match_redis() {
    let Some((_guard, mut oracle)) = spawn_redis() else {
        eprintln!("SKIPPED: redis-server not installed — compat AC stays evidence-pending");
        return;
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
                // The candidate reply must still be one complete RESP frame.
                let framed = frame_len(&candidate_reply).expect("candidate reply parses");
                assert_eq!(
                    framed,
                    Some(candidate_reply.len()),
                    "case {i} {:?} ({why}): candidate reply is not one complete frame",
                    case.argv
                );
            }
        }
    }

    let compared = MATRIX.len() - skipped;
    println!(
        "compat-diff v0: {compared} byte-compared cases, {skipped} documented deviations, {} failures",
        failures.len()
    );
    assert!(
        failures.is_empty(),
        "{} mismatches vs real Redis:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
