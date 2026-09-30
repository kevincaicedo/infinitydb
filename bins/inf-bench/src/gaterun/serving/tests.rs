use std::net::TcpListener;
use std::os::unix::process::ExitStatusExt as _;
use std::thread::JoinHandle;

use super::*;

/// One scripted answer of the fake node, consumed per whole command.
#[derive(Clone, Copy)]
pub(crate) enum Step {
    Reply(&'static [u8]),
    /// Close this connection without answering (a reset mid-boot).
    Close,
    /// Keep the connection and never answer (a silent node).
    Silent,
}

/// A fake node on a fresh port. It answers each whole command with the
/// next step; once the script is spent it counts the bytes the client
/// sends until it closes (the silent-window witness) and returns them.
pub(crate) fn fake_node(script: Vec<Step>) -> (u16, JoinHandle<usize>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    let port = listener.local_addr().expect("addr").port();
    (port, std::thread::spawn(move || serve_script(&listener, script)))
}

fn serve_script(listener: &TcpListener, script: Vec<Step>) -> usize {
    let mut steps = script.into_iter().peekable();
    loop {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut pending = Vec::new();
        while read_command(&mut stream, &mut pending) {
            match steps.next() {
                Some(Step::Reply(bytes)) => stream.write_all(bytes).expect("reply"),
                Some(Step::Close) | None => break,
                Some(Step::Silent) => return count_until_close(&mut stream, pending.len()),
            }
            if steps.peek().is_none() {
                return count_until_close(&mut stream, pending.len());
            }
        }
    }
}

/// Reads until `pending` holds one whole command and drains it; `false`
/// when the client closed first.
fn read_command(stream: &mut TcpStream, pending: &mut Vec<u8>) -> bool {
    let mut chunk = [0u8; 256];
    loop {
        if let Some(length) = reply_len(pending) {
            pending.drain(..length);
            return true;
        }
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return false,
            Ok(n) => pending.extend_from_slice(&chunk[..n]),
        }
    }
}

fn count_until_close(stream: &mut TcpStream, already: usize) -> usize {
    let mut total = already;
    let mut chunk = [0u8; 256];
    while let Ok(n) = stream.read(&mut chunk) {
        if n == 0 {
            break;
        }
        total += n;
    }
    total
}

fn never_exits() -> Option<ExitStatus> {
    None
}

const TEST_DEADLINE: Duration = Duration::from_secs(5);

/// The readiness-trap canary: a node that accepts at once and answers
/// `-LOADING` k times must read at least k pauses, where an estimator
/// that stops at the accept reads ≈ 0.
#[test]
fn a_loading_node_is_not_booted_at_its_accept() {
    const LOADING: u64 = 20;
    let mut script = vec![Step::Reply(b"-LOADING InfinityDB is loading\r\n"); 20];
    script.extend([Step::Reply(b"+PONG\r\n"), Step::Reply(b":0\r\n")]);
    let (port, node) = fake_node(script);
    let serving = wait_pong(port, Instant::now(), TEST_DEADLINE, never_exits).expect("serves");
    assert_eq!(serving.loading_replies(), LOADING);
    assert!(
        serving.boot() >= Duration::from_millis(LOADING * SERVE_POLL_MS),
        "boot {:?} under {LOADING} pauses",
        serving.boot()
    );
    drop(serving);
    assert_eq!(node.join().expect("fake node"), 0);
}

#[test]
fn a_reset_mid_boot_retries_on_a_new_connection() {
    let (port, node) =
        fake_node(vec![Step::Close, Step::Reply(b"+PONG\r\n"), Step::Reply(b":0\r\n")]);
    let serving = wait_pong(port, Instant::now(), TEST_DEADLINE, never_exits).expect("serves");
    assert_eq!(serving.loading_replies(), 0);
    drop(serving);
    node.join().expect("fake node");
}

#[test]
fn dbsize_must_answer_zero_after_the_pong() {
    let (port, _node) = fake_node(vec![Step::Reply(b"+PONG\r\n"), Step::Reply(b"-LOADING x\r\n")]);
    let result = wait_pong(port, Instant::now(), TEST_DEADLINE, never_exits);
    assert!(matches!(result, Err(ServeError::PongBeforeServing)), "{result:?}");
    let (port, _node) = fake_node(vec![Step::Reply(b"+PONG\r\n"), Step::Reply(b":3\r\n")]);
    let result = wait_pong(port, Instant::now(), TEST_DEADLINE, never_exits);
    assert!(matches!(result, Err(ServeError::NotEmpty(3))), "{result:?}");
}

#[test]
fn an_unknown_reply_is_refused_and_quoted_within_its_bound() {
    let (port, _node) = fake_node(vec![Step::Reply(b"+OK\r\n")]);
    let result = wait_pong(port, Instant::now(), TEST_DEADLINE, never_exits);
    assert!(
        matches!(&result, Err(ServeError::Unexpected(quote)) if quote == "+OK\r\n"),
        "{result:?}"
    );
    let long: &'static [u8] =
        Box::leak(format!("-ERR {}\r\n", "x".repeat(500)).into_bytes().into());
    let (port, _node) = fake_node(vec![Step::Reply(long)]);
    let result = wait_pong(port, Instant::now(), TEST_DEADLINE, never_exits);
    let quoted = match &result {
        Err(ServeError::Unexpected(quote)) => quote.len(),
        other => panic!("{other:?}"),
    };
    assert_eq!(quoted, UNEXPECTED_QUOTE_BYTES_MAX, "the quote is cut at its bound");
}

#[test]
fn a_silent_node_times_out() {
    let (port, _node) = fake_node(vec![Step::Silent]);
    let result = wait_pong(port, Instant::now(), Duration::from_millis(100), never_exits);
    assert!(matches!(result, Err(ServeError::Timeout { loading_replies: 0 })), "{result:?}");
}

#[test]
fn an_exit_before_the_pong_is_named() {
    let port = TcpListener::bind(("127.0.0.1", 0)).expect("bind").local_addr().expect("a").port();
    let mut polls = 0u32;
    let exits_on_third_poll = || {
        polls += 1;
        (polls >= 3).then(|| ExitStatus::from_raw(1 << 8))
    };
    let result = wait_pong(port, Instant::now(), TEST_DEADLINE, exits_on_third_poll);
    assert!(matches!(result, Err(ServeError::Exited(status)) if status.code() == Some(1)));
}
