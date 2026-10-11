//! The boot instant is the first `+PONG`, never an accept. infinityd binds
//! its listener before the pool, the driver and recovery, and `PING` lacks
//! the allowed-while-loading flag, so the `-LOADING` gate refuses it until
//! every cell serves: an accepted connection proves nothing about serving.
//! After the `+PONG`, `DBSIZE` must answer `:0`, or the estimator's
//! premise (a `+PONG` means serving, on an empty node) is false.

use std::io::{Read as _, Write as _};
use std::net::TcpStream;
use std::process::ExitStatus;
use std::time::{Duration, Instant};

use crate::resp::{encode_command, reply_len};

/// The pause between boot probes (a refused connect, a `-LOADING`): the
/// boot row's resolution. Fixed.
pub(crate) const SERVE_POLL_MS: u64 = 1;

/// The longest wait for a boot to serve, from the spawn. Crossing:
/// `ServeError::Timeout`. It covers a probing first boot (≈ 13 s: four
/// FLUSH rows of 1 s plus the scratch file, ADR-0091 D6).
pub(crate) const EMPTY_BOOT_DEADLINE_S: u64 = 180;

/// The largest reply read while waiting (`+PONG`, `-LOADING …`, `:n`).
/// Crossing: `ServeError::Unexpected`.
const SERVE_REPLY_BYTES_MAX: usize = 4096;

/// The most bytes of an unexpected reply quoted in an error. Crossing:
/// the quote is cut there.
const UNEXPECTED_QUOTE_BYTES_MAX: usize = 64;

/// A node that answered `+PONG` and then `DBSIZE` `:0`: the harness's one
/// boot instant. The fields are private, so `confirm_empty` is the only
/// constructor and no other module can forge a boot without a `+PONG`.
#[derive(Debug)]
pub(crate) struct Serving {
    /// Spawn to the first `+PONG`.
    boot: Duration,
    /// `-LOADING` replies before the `+PONG`.
    loading_replies: u64,
    /// The connection that saw both replies, kept open and silent: the
    /// node's one idle client.
    connection: TcpStream,
}

impl Serving {
    /// Spawn to the first `+PONG`.
    pub(crate) fn boot(&self) -> Duration {
        self.boot
    }

    /// `-LOADING` replies before the `+PONG`.
    pub(crate) fn loading_replies(&self) -> u64 {
        self.loading_replies
    }

    /// The idle client: borrowed, never written, while a window runs.
    pub(crate) fn connection(&self) -> &TcpStream {
        &self.connection
    }
}

/// Why a boot did not reach [`Serving`].
#[derive(Debug)]
pub(crate) enum ServeError {
    /// No `+PONG` before the deadline.
    Timeout { loading_replies: u64 },
    /// The process exited before serving.
    Exited(ExitStatus),
    /// A reply the boot machine has no transition for (quoted, bounded).
    Unexpected(String),
    /// `DBSIZE` answered `-LOADING` after a `+PONG`.
    PongBeforeServing,
    /// `DBSIZE` answered `:n`, n > 0: not an empty node.
    NotEmpty(u64),
    /// The connection failed after the `+PONG`.
    Disconnected(String),
}

impl std::fmt::Display for ServeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ServeError::Timeout { loading_replies } => {
                write!(f, "no +PONG within the deadline ({loading_replies} -LOADING replies)")
            }
            ServeError::Exited(status) => write!(f, "the server exited before serving ({status})"),
            ServeError::Unexpected(quote) => write!(f, "unexpected reply {quote:?}"),
            ServeError::PongBeforeServing => write!(f, "DBSIZE answered -LOADING after +PONG"),
            ServeError::NotEmpty(keys) => write!(f, "DBSIZE answered :{keys}: not an empty node"),
            ServeError::Disconnected(why) => write!(f, "connection lost after +PONG: {why}"),
        }
    }
}

/// One complete reply, or the connection is gone (retry on a new one).
enum ReadOutcome {
    Reply(Vec<u8>),
    Lost(String),
}

/// Waits from `spawned_at` for the first `+PONG` on `port`, then checks
/// `DBSIZE`. `exited` reports the process's exit, checked between reads;
/// `deadline` is [`EMPTY_BOOT_DEADLINE_S`] outside tests.
pub(crate) fn wait_pong<F>(
    port: u16,
    spawned_at: Instant,
    deadline: Duration,
    mut exited: F,
) -> Result<Serving, ServeError>
where
    F: FnMut() -> Option<ExitStatus>,
{
    let give_up = spawned_at + deadline;
    let mut loading_replies = 0u64;
    let mut connection: Option<TcpStream> = None;
    loop {
        let reused = connection.take();
        let Some(mut stream) = reused.or_else(|| open(port)) else {
            pause(&mut exited, give_up, loading_replies)?;
            continue;
        };
        match exchange(&mut stream, b"PING", give_up, &mut exited, loading_replies)? {
            ReadOutcome::Reply(reply) if reply.as_slice() == b"+PONG\r\n" => {
                let boot = spawned_at.elapsed();
                return confirm_empty(stream, boot, loading_replies, give_up, exited);
            }
            ReadOutcome::Reply(reply) if reply.starts_with(b"-LOADING") => {
                loading_replies += 1;
                connection = Some(stream);
                pause(&mut exited, give_up, loading_replies)?;
            }
            ReadOutcome::Reply(reply) => return Err(ServeError::Unexpected(quote(&reply))),
            ReadOutcome::Lost(_) => pause(&mut exited, give_up, loading_replies)?,
        }
    }
}

/// `DBSIZE` on the connection that saw the `+PONG`.
fn confirm_empty<F>(
    mut stream: TcpStream,
    boot: Duration,
    loading_replies: u64,
    give_up: Instant,
    mut exited: F,
) -> Result<Serving, ServeError>
where
    F: FnMut() -> Option<ExitStatus>,
{
    let reply = match exchange(&mut stream, b"DBSIZE", give_up, &mut exited, loading_replies)? {
        ReadOutcome::Reply(reply) => reply,
        ReadOutcome::Lost(why) => return Err(ServeError::Disconnected(why)),
    };
    if reply.starts_with(b"-LOADING") {
        return Err(ServeError::PongBeforeServing);
    }
    let keys = reply
        .strip_prefix(b":")
        .and_then(|rest| rest.strip_suffix(b"\r\n"))
        .and_then(|digits| std::str::from_utf8(digits).ok())
        .and_then(|digits| digits.parse::<u64>().ok())
        .ok_or_else(|| ServeError::Unexpected(quote(&reply)))?;
    if keys > 0 {
        return Err(ServeError::NotEmpty(keys));
    }
    Ok(Serving { boot, loading_replies, connection: stream })
}

fn open(port: u16) -> Option<TcpStream> {
    let stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream.set_nodelay(true).ok()?;
    stream.set_read_timeout(Some(Duration::from_millis(SERVE_POLL_MS))).ok()?;
    Some(stream)
}

/// One `SERVE_POLL_MS` pause, then the exit and deadline checks.
fn pause<F>(exited: &mut F, give_up: Instant, loading_replies: u64) -> Result<(), ServeError>
where
    F: FnMut() -> Option<ExitStatus>,
{
    std::thread::sleep(Duration::from_millis(SERVE_POLL_MS));
    alive(exited, give_up, loading_replies)
}

fn alive<F>(exited: &mut F, give_up: Instant, loading_replies: u64) -> Result<(), ServeError>
where
    F: FnMut() -> Option<ExitStatus>,
{
    if let Some(status) = exited() {
        return Err(ServeError::Exited(status));
    }
    if Instant::now() >= give_up {
        return Err(ServeError::Timeout { loading_replies });
    }
    Ok(())
}

/// Sends one command and reads its one reply. A read timeout is not a
/// reply: the wait goes on, with the exit and deadline checked each
/// `SERVE_POLL_MS`, so one command is outstanding at a time.
fn exchange<F>(
    stream: &mut TcpStream,
    command: &[u8],
    give_up: Instant,
    exited: &mut F,
    loading_replies: u64,
) -> Result<ReadOutcome, ServeError>
where
    F: FnMut() -> Option<ExitStatus>,
{
    if let Err(e) = stream.write_all(&encode_command(&[command])) {
        return Ok(ReadOutcome::Lost(e.to_string()));
    }
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 512];
    loop {
        if let Some(length) = reply_len(&buf) {
            buf.truncate(length);
            return Ok(ReadOutcome::Reply(buf));
        }
        match stream.read(&mut chunk) {
            Ok(0) => return Ok(ReadOutcome::Lost("end of stream".into())),
            Ok(n) if buf.len() + n > SERVE_REPLY_BYTES_MAX => {
                buf.extend_from_slice(&chunk[..n]);
                return Err(ServeError::Unexpected(quote(&buf)));
            }
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) if is_wait(&e) => alive(exited, give_up, loading_replies)?,
            Err(e) => return Ok(ReadOutcome::Lost(e.to_string())),
        }
    }
}

fn is_wait(error: &std::io::Error) -> bool {
    matches!(error.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut)
}

fn quote(reply: &[u8]) -> String {
    let cut = reply.len().min(UNEXPECTED_QUOTE_BYTES_MAX);
    String::from_utf8_lossy(&reply[..cut]).into_owned()
}

#[cfg(test)]
pub(crate) mod tests;
