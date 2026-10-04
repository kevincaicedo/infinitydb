#![allow(
    clippy::disallowed_types,
    reason = "test-only: filesystem fixtures outside cell code (ADR-0144 D5)"
)]
#![allow(
    clippy::disallowed_methods,
    reason = "test target: harness deadlines and stamps, not cell code"
)]
//! The refusal line an operator reads, at the binary: a log whose valid
//! data ends below its MANIFEST's begin-LSN stops the node with exit 1
//! and one stderr line naming the cell and the evidence. The operations
//! page quotes that line verbatim, so the test compares the page's quote
//! with the line the binary printed, the run-dependent fields (the cell
//! index and the two log positions) masked: a reworded message, or a
//! reworded quote, is red here.
//!
//! Shape: a one-cell node acks `always` writes in a durable namespace and
//! stops gracefully (`SIGTERM`; the stop checkpoint publishes a MANIFEST
//! whose begin-LSN lies inside the log's last frame); one header byte of
//! the frame holding the begin-LSN is flipped, so the valid data ends at
//! that frame's base. Harness rules are the topology test's: `--port 0`,
//! readiness is a `DBSIZE` answer, a refusal is the exit.
#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use inf_log::{Lsn, ReaderConfig, SegmentReader, read_manifest, segment_file_name};

/// The page that quotes the refusal.
const OPERATIONS_PAGE: &str = include_str!("../../../website/site/docs/operations.html");

fn unique() -> u32 {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

fn data_root(tag: &str) -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/e2e-boot-refusal")
        .join(format!("{tag}-{}-{}", std::process::id(), unique()));
    let _ = std::fs::remove_dir_all(&root);
    root
}

struct Server {
    child: Child,
    port: u16,
    stderr: PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Server {
    /// `SIGTERM` (the graceful stop, with its stop checkpoint), then the
    /// exit status; the process is killed by `Drop` if it outlives 30 s.
    fn stop(&mut self) -> std::process::ExitStatus {
        let sent = Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status()
            .expect("kill(1)");
        assert!(sent.success(), "kill -TERM failed");
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(status) = self.child.try_wait().expect("try_wait") {
                return status;
            }
            let text = std::fs::read_to_string(&self.stderr).unwrap_or_default();
            assert!(Instant::now() < deadline, "still running 30 s after SIGTERM: {text}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

struct Launched {
    child: Child,
    stderr: PathBuf,
}

fn launch(dir: &Path) -> Launched {
    std::fs::create_dir_all(dir).expect("data dir");
    let stderr = dir.join(format!("stderr-{}-{}", std::process::id(), unique()));
    let child = Command::new(env!("CARGO_BIN_EXE_infinityd"))
        .args(["--port", "0", "--cells", "1", "--data-dir"])
        .arg(dir)
        .args(["--device-probe", "off"])
        .stdout(Stdio::null())
        .stderr(Stdio::from(std::fs::File::create(&stderr).expect("stderr file")))
        .spawn()
        .expect("spawn infinityd");
    Launched { child, stderr }
}

/// Up = listening *and* out of the `-LOADING` window; an exit at any
/// point is the refusal (a loop-resident recovery listens first).
fn wait_up(mut launched: Launched) -> Result<Server, (i32, String)> {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(status) = launched.child.try_wait().expect("try_wait") {
            let text = std::fs::read_to_string(&launched.stderr).unwrap_or_default();
            return Err((status.code().unwrap_or(-1), text));
        }
        let text = std::fs::read_to_string(&launched.stderr).unwrap_or_default();
        if let Some(port) = announced_port(&text)
            && serving(port)
        {
            return Ok(Server { child: launched.child, port, stderr: launched.stderr });
        }
        assert!(Instant::now() < deadline, "infinityd never came up: {text}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// One `DBSIZE` on a throwaway connection: an integer reply means ready.
fn serving(port: u16) -> bool {
    let Ok(mut s) = TcpStream::connect(("127.0.0.1", port)) else { return false };
    s.set_read_timeout(Some(Duration::from_secs(5))).expect("timeout");
    if s.write_all(&resp(&[b"DBSIZE"])).is_err() {
        return false;
    }
    let mut buf = [0u8; 64];
    match s.read(&mut buf) {
        Ok(n) => buf[..n].starts_with(b":"),
        Err(_) => false,
    }
}

fn announced_port(stderr: &str) -> Option<u16> {
    stderr
        .lines()
        .find_map(|line| line.strip_prefix("infinityd: listening on "))
        .and_then(|port| port.trim().parse().ok())
}

fn resp(parts: &[&[u8]]) -> Vec<u8> {
    let mut wire = format!("*{}\r\n", parts.len()).into_bytes();
    for p in parts {
        wire.extend_from_slice(format!("${}\r\n", p.len()).as_bytes());
        wire.extend_from_slice(p);
        wire.extend_from_slice(b"\r\n");
    }
    wire
}

/// One connection (`INF.NS USE` is per-connection); each reply here is
/// one simple line.
struct Client(TcpStream);

impl Client {
    fn connect(port: u16) -> Client {
        let c = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        c.set_read_timeout(Some(Duration::from_secs(60))).expect("timeout");
        Client(c)
    }

    fn ok(&mut self, parts: &[&[u8]]) {
        self.0.write_all(&resp(parts)).expect("write");
        let mut reply = Vec::new();
        let mut byte = [0u8; 1];
        while !reply.ends_with(b"\r\n") {
            self.0.read_exact(&mut byte).expect("read");
            reply.push(byte[0]);
        }
        assert_eq!(reply, b"+OK\r\n", "{}", String::from_utf8_lossy(&reply));
    }
}

/// The base of the frame in `begin`'s segment that holds `begin`, read
/// with the log's own reader. Asserts it is the segment's last frame:
/// a later frame would make the plant an interior gap, another refusal.
fn frame_holding(log_dir: &Path, begin: Lsn) -> u32 {
    let mut reader = SegmentReader::open(
        &inf_server::StdSegmentFs,
        log_dir,
        begin.segment,
        ReaderConfig::default(),
    )
    .expect("open the begin segment");
    let mut holding = None;
    let mut frames_after = 0u32;
    loop {
        let base = reader.offset();
        if reader.next_frame().expect("the stopped log's frames are whole").is_none() {
            break;
        }
        if holding.is_some() {
            frames_after += 1;
        } else if base < begin.offset && begin.offset < reader.offset() {
            holding = Some(base);
        }
    }
    assert_eq!(frames_after, 0, "the begin-LSN's frame is the log's last");
    holding.expect("a frame holds the MANIFEST's begin-LSN")
}

/// The line with its run-dependent fields masked: a decimal word (the
/// cell index) reads `<n>`, a log position (`seg-NNNNNN:XXXXXXXX`, the
/// trailing `,` or `:` kept) reads `<lsn>`.
fn shape(line: &str) -> String {
    let is_lsn = |word: &str| {
        let Some((segment, offset)) = word.strip_prefix("seg-").and_then(|w| w.split_once(':'))
        else {
            return false;
        };
        segment.len() == 6
            && segment.bytes().all(|b| b.is_ascii_digit())
            && offset.len() == 8
            && offset.bytes().all(|b| b.is_ascii_hexdigit())
    };
    let words: Vec<String> = line
        .split(' ')
        .map(|word| {
            let core = word.trim_end_matches([',', ':']);
            if !word.is_empty() && word.bytes().all(|b| b.is_ascii_digit()) {
                "<n>".to_owned()
            } else if is_lsn(core) {
                format!("<lsn>{}", &word[core.len()..])
            } else {
                word.to_owned()
            }
        })
        .collect();
    words.join(" ")
}

/// The page's quoted refusal: the one line holding `boot failed`, its
/// tags removed and its entities decoded. An entity this does not know
/// is a failure, never a silent mismatch.
fn quoted_refusal(page: &str) -> String {
    let mut lines = page.lines().filter(|line| line.contains("boot failed"));
    let line = lines.next().expect("the operations page quotes a refusal");
    assert!(lines.next().is_none(), "the page quotes one refusal line");
    let mut text = String::new();
    let mut in_tag = false;
    for c in line.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => text.push(c),
            _ => {}
        }
    }
    let text = text
        .trim()
        .replace("&mdash;", "—")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&#39;", "'")
        .replace("&rsquo;", "’")
        .replace("&amp;", "&");
    assert!(!text.contains('&'), "an entity the test does not decode: {text}");
    text
}

/// The masking itself is scoped: the page's line keeps every word but
/// the cell index and the two positions.
#[test]
fn the_shape_masks_the_cell_and_the_two_positions_only() {
    let quoted = shape(&quoted_refusal(OPERATIONS_PAGE));
    assert_eq!(quoted.matches("<n>").count(), 1, "{quoted}");
    assert_eq!(quoted.matches("<lsn>").count(), 2, "{quoted}");
    assert_eq!(
        shape("cell 12 at seg-000001:0000abcd, below seg-000002:00000000: 7x"),
        "cell <n> at <lsn>, below <lsn>: 7x"
    );
}

#[test]
fn a_log_ending_below_the_manifest_begin_refuses_with_the_quoted_line() {
    let dir = data_root("begin");
    {
        let mut server = wait_up(launch(&dir)).expect("first boot");
        let mut c = Client::connect(server.port);
        c.ok(&[b"INF.NS", b"CREATE", b"orders", b"MODE", b"durable", b"FSYNC", b"always"]);
        c.ok(&[b"INF.NS", b"USE", b"orders"]);
        for i in 0..16u32 {
            c.ok(&[b"SET", format!("k:{i}").as_bytes(), b"v"]);
        }
        drop(c);
        let status = server.stop();
        assert_eq!(status.code(), Some(0), "a graceful stop exits 0");
    }
    let shard = dir.join("shard-0");
    let begin = read_manifest(&inf_server::StdSegmentFs, &shard)
        .expect("read the MANIFEST")
        .expect("the stop checkpoint published a MANIFEST")
        .begin_lsn;
    let log_dir = shard.join("log");
    let base = frame_holding(&log_dir, begin);
    // A byte of the frame's `seq` (header offset 24): its CRC fails, so
    // the valid data ends at the frame's base, below the begin-LSN.
    let segment = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(log_dir.join(segment_file_name(begin.segment)))
        .expect("open the begin segment");
    let at = u64::from(base) + 24;
    let mut byte = [0u8; 1];
    segment.read_exact_at(&mut byte, at).expect("read the header byte");
    segment.write_all_at(&[byte[0] ^ 0xAA], at).expect("flip it");
    segment.sync_all().expect("sync");

    let (code, stderr) = match wait_up(launch(&dir)) {
        Err(refused) => refused,
        Ok(_) => panic!("booted on a log that ends below its MANIFEST's begin-LSN"),
    };
    assert_eq!(code, 1, "{stderr}");
    let line = stderr
        .lines()
        .find(|line| line.contains("boot failed"))
        .unwrap_or_else(|| panic!("no refusal line: {stderr}"));
    let end = Lsn::new(begin.segment, base);
    assert_eq!(
        line,
        format!(
            "infinityd: cell 0 boot failed (fail-stop): the log's valid data ends at {end}, \
             below the MANIFEST begin-LSN {begin}: fsync-covered bytes are missing — refusing \
             to start"
        ),
        "{stderr}"
    );
    assert_eq!(shape(line), shape(&quoted_refusal(OPERATIONS_PAGE)), "the page's quote");
    let _ = std::fs::remove_dir_all(&dir);
}
