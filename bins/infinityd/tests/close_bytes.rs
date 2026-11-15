//! A close runs the bytes its client handed over, at the binary (ADR-0196
//! and its A1; ADR-0123 A2). A client that ends its stream (a FIN) is
//! answered for every command read before the end, then closed; a client
//! whose socket is gone (a reset) has those commands run, unanswered; a
//! cell stalled by `DEBUG SLEEP` runs them once the stall ends, though the
//! close came during it. Each test starts its own node and reads the store
//! back over a fresh connection. A stopped process (`SIGSTOP`) reads
//! nothing, so the bytes and the end of the stream reach the node at one
//! reap once it runs again: the deterministic shape of a client whose last
//! command and FIN share one completion pass. Linux: the receive buffer and
//! the reset are set with `setsockopt`.
#![cfg(target_os = "linux")]
#![allow(
    clippy::disallowed_methods,
    reason = "test target: harness deadlines and a data directory, not cell code"
)]
#![allow(
    clippy::disallowed_types,
    reason = "test target: the node's stderr file and the signal commands, not cell code"
)]

#[path = "support/node.rs"]
mod node;

use node::{Node, NodeBinary};
use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How long a write may take to become readable once the node runs: it is
/// read in a few iterations, so the bound is slack by orders of magnitude;
/// past it the write did not land. A hang bound, never the verdict.
const LAND_DEADLINE: Duration = Duration::from_secs(30);

/// A reply read's hang bound.
const READ_TIMEOUT: Duration = Duration::from_secs(30);

struct Server {
    child: Node,
    port: u16,
    stderr: PathBuf,
    dir: PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        // A stopped process must run again before it can die of its kill.
        let _ = signal(self.child.id(), "-CONT");
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Server {
    /// A node of `cells` cells, every cell reachable through the accept
    /// hand-off, on a data directory of its own.
    fn spawn(cells: u16, name: &str) -> Server {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/e2e-close-bytes")
            .join(format!("{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("data dir");
        let stderr = dir.join("stderr");
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .expect("probe bind")
            .local_addr()
            .expect("addr")
            .port()
            .to_string();
        let child = NodeBinary::built()
            .command()
            .args(["--port", &port, "--cells", &cells.to_string()])
            .args(["--accept-handoff", "on", "--device-probe", "off", "--data-dir"])
            .arg(dir.join("data"))
            .stdout(Stdio::null())
            .stderr(Stdio::from(std::fs::File::create(&stderr).expect("stderr file")))
            .spawn()
            .expect("spawn infinityd");
        let mut server = Server { child, port: 0, stderr, dir };
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if let Some(status) = server.child.try_wait().expect("try_wait") {
                panic!("infinityd exited during boot: {status} — {}", server.text());
            }
            let text = server.text();
            if let Some(port) = text
                .lines()
                .find_map(|line| line.strip_prefix("infinityd: listening on "))
                .and_then(|port| port.trim().parse().ok())
                && text.contains("control: recovery complete")
                && TcpStream::connect(("127.0.0.1", port)).is_ok()
            {
                server.port = port;
                return server;
            }
            assert!(Instant::now() < deadline, "infinityd never came up: {text}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn text(&self) -> String {
        std::fs::read_to_string(&self.stderr).unwrap_or_default()
    }

    fn connect(&self) -> TcpStream {
        let stream = TcpStream::connect(("127.0.0.1", self.port)).expect("connect");
        stream.set_read_timeout(Some(READ_TIMEOUT)).expect("timeout");
        stream.set_nodelay(true).expect("nodelay");
        stream
    }

    /// Stops the process: it reads nothing until [`Server::resume`].
    fn stop(&self) {
        assert!(signal(self.child.id(), "-STOP"), "SIGSTOP");
    }

    fn resume(&self) {
        assert!(signal(self.child.id(), "-CONT"), "SIGCONT");
    }
}

/// Sends `signal` (`-STOP`, `-CONT`) to `pid` with `kill(1)`.
fn signal(pid: u32, signal: &str) -> bool {
    Command::new("kill")
        .args([signal, &pid.to_string()])
        .status()
        .is_ok_and(|status| status.success())
}

fn wire(parts: &[&[u8]]) -> Vec<u8> {
    let mut wire = format!("*{}\r\n", parts.len()).into_bytes();
    for part in parts {
        wire.extend_from_slice(format!("${}\r\n", part.len()).as_bytes());
        wire.extend_from_slice(part);
        wire.extend_from_slice(b"\r\n");
    }
    wire
}

/// The names `prefix:0` to `prefix:{count - 1}`.
fn names(prefix: &str, count: usize) -> Vec<String> {
    (0..count).map(|n| format!("{prefix}:{n}")).collect()
}

/// A `SET` of every name, pipelined.
fn sets(names: &[String]) -> Vec<u8> {
    names.iter().flat_map(|name| wire(&[b"SET", name.as_bytes(), b"1"])).collect()
}

/// `n` `+OK` replies.
fn oks(n: usize) -> Vec<u8> {
    b"+OK\r\n".repeat(n)
}

/// How a read to the end ended.
#[derive(Debug, PartialEq, Eq)]
enum End {
    /// The server's FIN.
    Fin,
    /// The server's reset.
    Reset,
}

/// Every byte the server sends until it closes, and how it closed. A read
/// past [`READ_TIMEOUT`] is a hang: the server never closed.
fn read_to_end(stream: &mut TcpStream, what: &str) -> (Vec<u8>, End) {
    let mut delivered = Vec::new();
    let mut chunk = vec![0u8; 1 << 16];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => return (delivered, End::Fin),
            Ok(n) => delivered.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => {
                return (delivered, End::Reset);
            }
            Err(e) => {
                panic!("{what}: the server never closed after {} bytes ({e})", delivered.len())
            }
        }
    }
}

/// One reply line, then a bulk's payload when the line opens one.
fn reply(stream: &mut TcpStream) -> Vec<u8> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    while !line.ends_with(b"\r\n") {
        stream.read_exact(&mut byte).expect("read");
        line.push(byte[0]);
    }
    if line[0] == b'$' && !line.starts_with(b"$-1") {
        let len: usize =
            std::str::from_utf8(&line[1..line.len() - 2]).expect("utf8").parse().expect("len");
        let mut payload = vec![0u8; len + 2];
        stream.read_exact(&mut payload).expect("payload");
        line.extend_from_slice(&payload);
    }
    line
}

fn call(stream: &mut TcpStream, parts: &[&[u8]]) -> Vec<u8> {
    stream.write_all(&wire(parts)).expect("write");
    reply(stream)
}

/// How many of `names` exist, counted by `EXISTS` in batches of 500.
fn existing(server: &Server, ns: Option<&[u8]>, names: &[String]) -> usize {
    let mut reader = server.connect();
    if let Some(ns) = ns {
        assert_eq!(call(&mut reader, &[b"INF.NS", b"USE", ns]), b"+OK\r\n", "USE");
    }
    let mut count = 0;
    for batch in names.chunks(500) {
        let mut parts: Vec<&[u8]> = vec![b"EXISTS"];
        parts.extend(batch.iter().map(String::as_bytes));
        let line = call(&mut reader, &parts);
        let text = std::str::from_utf8(&line).expect("an integer reply");
        count += text.trim_start_matches(':').trim_end().parse::<usize>().expect("a count");
    }
    count
}

/// Polls until every one of `names` exists, or the land deadline passes.
fn assert_all_land(server: &Server, ns: Option<&[u8]>, names: &[String], what: &str) {
    let deadline = Instant::now() + LAND_DEADLINE;
    loop {
        let count = existing(server, ns, names);
        if count == names.len() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{what}: {count} of {} names were set {LAND_DEADLINE:?} after the close",
            names.len()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Sets the socket option `option` of `stream` to `value`.
fn set_option<T>(stream: &TcpStream, option: libc::c_int, value: &T) {
    let len = libc::socklen_t::try_from(std::mem::size_of::<T>()).expect("a small option");
    // SAFETY: `stream` owns an open socket for the call's whole duration, and
    // `value` is a live, initialised `T` of `len` bytes that setsockopt only
    // reads; neither pointer is kept past the call.
    let status = unsafe {
        libc::setsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            option,
            std::ptr::from_ref(value).cast(),
            len,
        )
    };
    assert_eq!(status, 0, "setsockopt {option}: {}", std::io::Error::last_os_error());
}

/// Closes `stream` with a reset: `SO_LINGER` 0, then the close.
fn reset(stream: TcpStream) {
    set_option(&stream, libc::SO_LINGER, &libc::linger { l_onoff: 1, l_linger: 0 });
    drop(stream);
}

/// Sets the receive buffer of `stream` to `bytes`.
fn small_receive_buffer(stream: &TcpStream, bytes: libc::c_int) {
    set_option(stream, libc::SO_RCVBUF, &bytes);
}

/// `CLIENT LIST` read from a fresh connection.
fn client_list(server: &Server) -> String {
    let mut probe = server.connect();
    String::from_utf8(call(&mut probe, &[b"CLIENT", b"LIST"])).expect("CLIENT LIST is text")
}

/// Whether `CLIENT LIST` holds a row of client `id`.
fn has_row(list: &str, id: &str) -> bool {
    list.lines().any(|line| line.contains(&format!("id={id} ")))
}

/// A `SET` and the FIN written while the process is stopped reach the node
/// at one reap: the command is run and answered, then the connection
/// closes, and a fresh client reads the value.
#[test]
#[ignore = "a connection's last bytes and its end of stream reaped together close the slot \
            first, so the bytes are released unparsed; red until the end of stream keeps the \
            parser and the commands read before it are run and answered"]
fn a_set_written_with_its_fin_is_run_and_answered() {
    let server = Server::spawn(2, "pre-eof");
    let mut writer = server.connect();
    server.stop();
    writer.write_all(&wire(&[b"SET", b"pre-eof", b"v"])).expect("write");
    writer.shutdown(Shutdown::Write).expect("FIN");
    server.resume();
    let (delivered, end) = read_to_end(&mut writer, "the writer");
    assert_all_land(&server, None, &["pre-eof".to_string()], "the SET written with its FIN");
    assert_eq!(delivered, b"+OK\r\n", "the writer read {:?}", String::from_utf8_lossy(&delivered));
    assert_eq!(end, End::Fin, "the answered client is closed with a FIN");
}

/// HC1: 100 and then 5,000 pipelined `SET`s and the FIN, the node running:
/// every reply, then the FIN, and every name set. With the node running,
/// the data's reap may come iterations before the FIN's, so this reading is
/// the wire's witness, not the deterministic red (HC2 is).
#[test]
#[ignore = "an end of stream closes the slot at once and RESPOND skips a closing slot, so the \
            replies of commands read before the FIN are lost when they share an iteration; red \
            until the end of stream moves the slot to a flush that answers them first"]
fn a_half_closed_pipeline_is_answered_then_closed() {
    let server = Server::spawn(2, "hc1");
    for (prefix, count) in [("hc1-small", 100), ("hc1-large", 5_000)] {
        let names = names(prefix, count);
        let mut client = server.connect();
        client.write_all(&sets(&names)).expect("write");
        client.shutdown(Shutdown::Write).expect("FIN");
        let (delivered, end) = read_to_end(&mut client, prefix);
        assert_all_land(&server, None, &names, prefix);
        assert!(
            delivered == oks(count),
            "{prefix}: {} reply bytes of {} for {count} SETs",
            delivered.len(),
            oks(count).len()
        );
        assert_eq!(end, End::Fin, "{prefix}: closed by a FIN after its replies");
    }
}

/// HC2: the same with the node stopped until the FIN is written, so the
/// data and the FIN share one reap: the deterministic red of the half-close.
#[test]
#[ignore = "an end of stream reaped with its connection's data closes the slot before PARSE, so \
            none of the commands read before it is answered; red until the end of stream moves \
            the slot to a flush that answers them first"]
fn a_half_closed_pipeline_reaped_with_its_fin_is_answered() {
    let server = Server::spawn(2, "hc2");
    for (prefix, count) in [("hc2-small", 100), ("hc2-large", 5_000)] {
        let names = names(prefix, count);
        let mut client = server.connect();
        server.stop();
        client.write_all(&sets(&names)).expect("write");
        client.shutdown(Shutdown::Write).expect("FIN");
        server.resume();
        let (delivered, end) = read_to_end(&mut client, prefix);
        assert_all_land(&server, None, &names, prefix);
        assert!(
            delivered == oks(count),
            "{prefix}: {} reply bytes of {} for {count} SETs",
            delivered.len(),
            oks(count).len()
        );
        assert_eq!(end, End::Fin, "{prefix}: closed by a FIN after its replies");
    }
}

/// HC4: 1,500 `SET`s, `QUIT`, 2,000 `SET`s and the FIN: the 1,500 and the
/// `QUIT` are answered, then a FIN (ADR-0196 D6.2); nothing behind the
/// `QUIT` runs.
#[test]
#[ignore = "an end of stream closes the slot at once, so the replies read before it, the QUIT's \
            included, are lost when they share an iteration; red until the end of stream moves \
            the slot to a flush that answers them first"]
fn a_quit_inside_a_half_closed_pipeline_ends_it() {
    let server = Server::spawn(2, "hc4");
    let before = names("hc4-a", 1_500);
    let behind = names("hc4-b", 2_000);
    let mut bytes = sets(&before);
    bytes.extend(wire(&[b"QUIT"]));
    bytes.extend(sets(&behind));
    let mut client = server.connect();
    client.write_all(&bytes).expect("write");
    client.shutdown(Shutdown::Write).expect("FIN");
    let (delivered, end) = read_to_end(&mut client, "hc4");
    assert_all_land(&server, None, &before, "hc4: the SETs before the QUIT");
    assert_eq!(existing(&server, None, &behind), 0, "hc4: a SET behind the QUIT ran");
    assert!(
        delivered == oks(1_501),
        "hc4: {} reply bytes of {} (1,500 SETs and the QUIT)",
        delivered.len(),
        oks(1_501).len()
    );
    assert_eq!(end, End::Fin, "hc4: closed by a FIN (ADR-0196 D6.2)");
}

/// Eight `GET`s of a 1 MiB value and the FIN from a client whose receive
/// buffer is 64 KiB and that reads nothing for 0.5 s, so the replies wait on
/// the socket (`quit` adds a `QUIT` before the FIN). Returns the bytes read,
/// how the stream ended, and whether `CLIENT LIST` held the client's row
/// 0.2 s after the FIN and after its end.
fn slow_half_closed_reader(server: &Server, quit: bool) -> (Vec<u8>, End, bool, bool) {
    let mut setup = server.connect();
    assert_eq!(call(&mut setup, &[b"SET", b"big", &vec![b'b'; 1 << 20]]), b"+OK\r\n");
    let mut client = server.connect();
    small_receive_buffer(&client, 64 * 1024);
    let id = call(&mut client, &[b"CLIENT", b"ID"]);
    let id = std::str::from_utf8(&id).expect("text").trim_start_matches(':').trim_end().to_string();
    let mut bytes: Vec<u8> = (0..8).flat_map(|_| wire(&[b"GET", b"big"])).collect();
    if quit {
        bytes.extend(wire(&[b"QUIT"]));
    }
    client.write_all(&bytes).expect("write");
    client.shutdown(Shutdown::Write).expect("FIN");
    std::thread::sleep(Duration::from_millis(200));
    let during = has_row(&client_list(server), &id);
    std::thread::sleep(Duration::from_millis(300));
    let (delivered, end) = read_to_end(&mut client, "the slow reader");
    let after = has_row(&client_list(server), &id);
    (delivered, end, during, after)
}

/// One `GET big` reply: the bulk header and the 1 MiB payload.
const BIG_REPLY_BYTES: usize = "$1048576\r\n".len() + (1 << 20) + 2;

/// HC5 and HC8: every byte of the eight replies, 8,388,704, then the end of
/// stream (D6.4: Redis drops them at 1,064,944); the client's row is listed
/// while its replies are unsent and gone after its end (D6.5).
#[test]
#[ignore = "an end of stream closes the slot at once, so replies a slow reader has not taken are \
            lost and its row leaves CLIENT LIST before them; red until the end of stream moves \
            the slot to a flush that sends every reply first"]
fn a_slow_half_closed_reader_reads_every_reply() {
    let server = Server::spawn(2, "hc5");
    let (delivered, end, during, after) = slow_half_closed_reader(&server, false);
    assert_eq!(delivered.len(), 8 * BIG_REPLY_BYTES, "hc5: the bytes read before the end");
    assert_eq!(end, End::Fin, "hc5: closed by a FIN after its replies");
    assert!(during, "hc8: the half-closed client's row left CLIENT LIST with replies unsent");
    assert!(!after, "hc8: the closed client's row is still listed");
}

/// HC10: the same client sends `QUIT` before its FIN: the eight replies,
/// then the `QUIT`'s `+OK` (D6.4: Redis drops it), then the end.
#[test]
#[ignore = "an end of stream closes the slot at once, so replies a slow reader has not taken, the \
            QUIT's included, are lost; red until the end of stream moves the slot to a flush \
            that sends every reply first"]
fn a_slow_reader_that_quits_before_its_fin_reads_every_reply() {
    let server = Server::spawn(2, "hc10");
    let (delivered, end, during, after) = slow_half_closed_reader(&server, true);
    assert_eq!(delivered.len(), 8 * BIG_REPLY_BYTES + 5, "hc10: the bytes read before the end");
    assert!(delivered.ends_with(b"+OK\r\n"), "hc10: the QUIT's +OK follows the eight replies");
    assert_eq!(end, End::Fin, "hc10: closed by a FIN after its replies");
    assert!(during, "hc10: the client's row left CLIENT LIST with replies unsent");
    assert!(!after, "hc10: the closed client's row is still listed");
}

/// HC9: 100 `SET`s, then a reset (`SO_LINGER` 0), with the node stopped:
/// every name is set though no reply can reach the client.
#[test]
#[ignore = "a reset reaped with its connection's data closes the slot before PARSE, so the bytes \
            the client sent are released unparsed; red until a gone peer's received bytes are \
            parsed and run unanswered"]
fn a_reset_pipeline_is_run_unanswered() {
    let server = Server::spawn(2, "hc9");
    let names = names("hc9", 100);
    let client = server.connect();
    let mut writer = client.try_clone().expect("clone");
    server.stop();
    writer.write_all(&sets(&names)).expect("write");
    drop(writer);
    reset(client);
    server.resume();
    assert_all_land(&server, None, &names, "hc9: the SETs sent before the reset");
}

/// A connection bound by `INF.NS USE`, so every command rides its pump:
/// 1,000 `SET`s, then a reset with the node stopped. The queue still runs
/// after `Closed`, unanswered.
#[test]
#[ignore = "Closed drops a connection with its queue and a reset reaped with the data releases \
            the bytes unparsed, so commands a gone client sent never run; red until the parser \
            and the queue run to their end at every close"]
fn a_namespace_bound_reset_runs_its_queue() {
    let server = Server::spawn(2, "queue");
    let mut setup = server.connect();
    assert_eq!(
        call(&mut setup, &[b"INF.NS", b"CREATE", b"queued", b"MODE", b"memory"]),
        b"+OK\r\n"
    );
    // One hash tag, so every name has one owner and a batch `EXISTS` is one
    // cell's.
    let names = names("{queue}", 1_000);
    let mut client = server.connect();
    assert_eq!(call(&mut client, &[b"INF.NS", b"USE", b"queued"]), b"+OK\r\n");
    server.stop();
    client.write_all(&sets(&names)).expect("write");
    reset(client);
    server.resume();
    assert_all_land(&server, Some(b"queued"), &names, "the bound client's SETs");
}

/// The stall variant, one cell: client A sends `DEBUG SLEEP 1`; 0.2 s in,
/// client B sends `CLIENT SETNAME ghost`, 100 `SET`s and its FIN, and
/// client C sends `CLIENT SETNAME ghost2`, 100 `SET`s and a reset. After
/// A's `+OK` every name is set, B reads its 101 `+OK`s then the end, and
/// neither name is a `CLIENT LIST` row (ADR-0196 HC3, A1's HC9).
#[test]
#[ignore = "a stalled cell keeps a connection's received bytes in its inbox past the connection's \
            Closed, and PARSE then releases them at the vacant slot; red until the slot keeps its \
            parser past Closed and parses the entries reaped before it"]
fn a_stall_runs_the_bytes_of_connections_closed_during_it() {
    let server = Server::spawn(1, "stall");
    let mut sleeper = server.connect();
    let mut fin = server.connect();
    let reset_client = server.connect();
    let fin_names = names("stall-fin", 100);
    let reset_names = names("stall-reset", 100);
    sleeper.write_all(&wire(&[b"DEBUG", b"SLEEP", b"1"])).expect("write");
    std::thread::sleep(Duration::from_millis(200));
    let mut bytes = wire(&[b"CLIENT", b"SETNAME", b"ghost"]);
    bytes.extend(sets(&fin_names));
    fin.write_all(&bytes).expect("write");
    fin.shutdown(Shutdown::Write).expect("FIN");
    let mut bytes = wire(&[b"CLIENT", b"SETNAME", b"ghost2"]);
    bytes.extend(sets(&reset_names));
    let mut writer = reset_client.try_clone().expect("clone");
    writer.write_all(&bytes).expect("write");
    drop(writer);
    reset(reset_client);
    assert_eq!(reply(&mut sleeper), b"+OK\r\n", "the sleeper's +OK");
    assert_all_land(&server, None, &fin_names, "the FIN client's SETs");
    assert_all_land(&server, None, &reset_names, "the reset client's SETs");
    let (delivered, end) = read_to_end(&mut fin, "the FIN client");
    assert!(
        delivered == oks(101),
        "the FIN client read {} reply bytes of {}",
        delivered.len(),
        oks(101).len()
    );
    assert_eq!(end, End::Fin, "the FIN client is closed with a FIN after its replies");
    let list = client_list(&server);
    assert!(!list.contains("name=ghost "), "a closed client's name is listed: {list}");
    assert!(!list.contains("name=ghost2 "), "a reset client's name is listed: {list}");
}
