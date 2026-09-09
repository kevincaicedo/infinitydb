#![allow(
    clippy::disallowed_methods,
    reason = "test target: harness deadlines and stamps, not cell code"
)]
//! F-L14-01 at the binary (full-codebase review of 2026-08-30): the
//! end-of-replay checks (ADR-0057 D4 displacement audit, the extent
//! sweep seed, the shadow-ticket rebuild, the sidecar commit) must run
//! after the *last* replayed record. A stale-residue hole "lift"
//! (ADR-0031 D5 as amended) replays the probed segments after the hole —
//! before the fix those checks had already run at the first probe step,
//! so a lifted log ending between a displacement marker and its paired
//! mutation booted and served instead of refusing.
//!
//! Shape: a real one-cell node creates a tiered namespace and acks
//! `always` writes; killed; the log is then hand-edited into the lift
//! shape (a validating frame of the same life beyond a zero gap, and a
//! next segment written by a later life), the later life's last frame
//! being the record under test. Harness rules are the topology test's.
#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use inf_log::{
    FRAME_HEADER_LEN, FrameBuilder, FrameLayout, FrameStamp, Lsn, NsId, ReaderConfig, RecordView,
    SegmentId, SegmentReader, segment_file_name,
};

/// The first named namespace's id — the one `INF.NS CREATE` below gets.
const HOT: NsId = NsId(inf_store::FIRST_NAMED_NS_ID);
const SEGMENT_BYTES: &str = "4194304";

fn unique() -> u32 {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

fn data_root(tag: &str) -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/e2e-recover-lift")
        .join(format!("{tag}-{}-{}", std::process::id(), unique()));
    let _ = std::fs::remove_dir_all(&root);
    root
}

struct Server {
    child: Child,
    port: u16,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
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
        .args(["--device-probe", "off", "--segment-bytes", SEGMENT_BYTES])
        .stdout(Stdio::null())
        .stderr(Stdio::from(std::fs::File::create(&stderr).expect("stderr file")))
        .spawn()
        .expect("spawn infinityd");
    Launched { child, stderr }
}

/// Up = listening *and* out of the `-LOADING` window (a loop-resident
/// recovery listens before it finishes, and a fail-stop inside it exits
/// after the port was announced — so readiness is a gated command
/// answering, and an exit at any point is the refusal).
fn wait_up(mut launched: Launched) -> Result<(Server, String), (i32, String)> {
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
            return Ok((Server { child: launched.child, port }, text));
        }
        assert!(Instant::now() < deadline, "infinityd never came up: {text}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// One `DBSIZE` (no LOADING flag) on a throwaway connection: an integer
/// reply means every cell is ready; `-LOADING`, a refused connect or a
/// reset are all "not yet".
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

fn spawn(dir: &Path) -> Result<(Server, String), (i32, String)> {
    wait_up(launch(dir))
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

/// One connection (`INF.NS USE` is per-connection); `-LOADING` retried.
struct Client(TcpStream);

impl Client {
    fn connect(port: u16) -> Client {
        let c = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        c.set_read_timeout(Some(Duration::from_secs(60))).expect("timeout");
        Client(c)
    }

    fn call(&mut self, parts: &[&[u8]]) -> Vec<u8> {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let reply = self.call_once(parts);
            if !reply.starts_with(b"-LOADING") {
                return reply;
            }
            assert!(Instant::now() < deadline, "still loading after 60 s");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn call_once(&mut self, parts: &[&[u8]]) -> Vec<u8> {
        self.0.write_all(&resp(parts)).expect("write");
        let mut reply = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            self.0.read_exact(&mut byte).expect("read");
            reply.push(byte[0]);
            if reply.ends_with(b"\r\n") {
                break;
            }
        }
        if reply[0] == b'$' && !reply.starts_with(b"$-1") {
            let len: usize = std::str::from_utf8(&reply[1..reply.len() - 2])
                .expect("utf8")
                .parse()
                .expect("len");
            let mut payload = vec![0u8; len + 2];
            self.0.read_exact(&mut payload).expect("bulk");
            reply.extend_from_slice(&payload);
        }
        reply
    }

    fn ok(&mut self, parts: &[&[u8]]) {
        let reply = self.call(parts);
        assert_eq!(reply, b"+OK\r\n", "{}", String::from_utf8_lossy(&reply));
    }

    fn get(&mut self, key: &[u8]) -> String {
        String::from_utf8_lossy(&self.call(&[b"GET", key])).into_owned()
    }
}

/// Life 1: a tiered namespace and four acked `always` writes, then SIGKILL.
fn first_life(dir: &Path) {
    let (server, stderr) = spawn(dir).expect("first boot");
    assert!(stderr.contains("topology: 1 cells (stamped at this first boot"), "{stderr}");
    let mut c = Client::connect(server.port);
    c.ok(&[
        b"INF.NS",
        b"CREATE",
        b"hot",
        b"MODE",
        b"durable",
        b"FSYNC",
        b"always",
        b"MEM-BUDGET",
        b"8mb",
        b"DISK-BUDGET",
        b"64mb",
        b"MUTABLE-FRACTION",
        b"100",
    ]);
    c.ok(&[b"INF.NS", b"USE", b"hot"]);
    for i in 0..4u32 {
        c.ok(&[b"SET", format!("k:{i}").as_bytes(), format!("v:{i}").as_bytes()]);
    }
    // Server dropped here = SIGKILL.
}

fn frame(
    segment: SegmentId,
    offset: u32,
    records: &[RecordView<'_>],
    stamp: FrameStamp,
) -> Vec<u8> {
    let mut b = FrameBuilder::new();
    for record in records {
        b.append(record);
    }
    let first = Lsn::new(segment, offset + FRAME_HEADER_LEN as u32);
    b.finalize(first, stamp, FrameLayout::Packed).to_vec()
}

fn poke(path: &Path, offset: u32, bytes: &[u8]) {
    let file = std::fs::OpenOptions::new().write(true).open(path).expect("open segment");
    file.write_all_at(bytes, u64::from(offset)).expect("write frame");
    file.sync_all().expect("sync");
}

/// Rewrites life 1's log into the lift shape: a validating life-1 frame
/// beyond a zero gap in segment 0 (the hole the audit cannot classify
/// locally), and segment 1 written by life 2 — a control record, then a
/// last frame carrying `tail`.
fn plant_lifted_tail(dir: &Path, tail: &[RecordView<'_>]) {
    let log_dir = dir.join("shard-0/log");
    let seg0 = log_dir.join(segment_file_name(SegmentId(0)));
    let seg1 = log_dir.join(segment_file_name(SegmentId(1)));
    let (end, last) = {
        let mut reader = SegmentReader::open(
            &inf_server::StdSegmentFs,
            &log_dir,
            SegmentId(0),
            ReaderConfig::default(),
        )
        .expect("open segment 0");
        let mut last = None;
        while let Some(frame) = reader.next_frame().expect("life 1's frames are whole") {
            last = frame.stamp();
        }
        (reader.offset(), last.expect("life 1 stamped its frames"))
    };
    assert!(end > 0, "life 1 wrote nothing");
    let residue_at = end + 63;
    let residue = frame(
        SegmentId(0),
        residue_at,
        &[RecordView::StringPostImage { ns: HOT, key: b"ghost", value: b"stale" }],
        FrameStamp { epoch: last.epoch, seq: last.seq + 2, covered_lsn: u64::from(end) },
    );
    poke(&seg0, residue_at, &residue);
    if !seg1.exists() {
        let len = std::fs::metadata(&seg0).expect("segment 0").len();
        std::fs::File::create(&seg1).expect("segment 1").set_len(len).expect("prealloc");
    }
    let life2 = last.epoch + 1;
    let first = frame(
        SegmentId(1),
        0,
        &[RecordView::StringPostImage { ns: HOT, key: b"lifted", value: b"alive" }],
        FrameStamp { epoch: life2, seq: 1, covered_lsn: u64::from(end) },
    );
    let first_len = u32::try_from(first.len()).expect("fits u32");
    poke(&seg1, 0, &first);
    let covered = Lsn::new(SegmentId(1), 0).advance(first_len).to_u64();
    let second = frame(
        SegmentId(1),
        first_len,
        tail,
        FrameStamp { epoch: life2, seq: 2, covered_lsn: covered },
    );
    poke(&seg1, first_len, &second);
}

/// The falsifier: the lifted log ends with an unpaired displacement
/// marker — a fail-stop by ADR-0057 D4 (exit 1, named), never a serving
/// node. Pre-fix the node booted and served the lifted segment.
#[test]
fn a_lifted_log_ending_in_an_unpaired_marker_refuses_to_boot() {
    let dir = data_root("marker");
    first_life(&dir);
    plant_lifted_tail(&dir, &[RecordView::ColdDisplace { ns: HOT, old_addr: 4096 }]);
    match spawn(&dir) {
        Err((code, stderr)) => {
            assert_eq!(code, 1, "{stderr}");
            assert!(
                stderr.contains("1 unpaired displacement marker") && stderr.contains("ADR-0057 D4"),
                "{stderr}"
            );
            assert!(!stderr.contains("panicked"), "{stderr}");
            let line = stderr.lines().find(|l| l.contains("ADR-0057 D4")).unwrap_or_default();
            println!("recover-lift binary: refused — {line}");
        }
        Ok((server, stderr)) => {
            let mut c = Client::connect(server.port);
            c.ok(&[b"INF.NS", b"USE", b"hot"]);
            let lifted = c.get(b"lifted");
            let k0 = c.get(b"k:0");
            panic!(
                "booted and served on a log ending between a displacement marker and its \
                 mutation: GET lifted = {lifted:?}, GET k:0 = {k0:?}; stderr:\n{stderr}"
            );
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// The control: the same lift with the marker paired in its frame boots,
/// serves life 1's keys, the lifted record, and the paired mutation —
/// and never the discarded life's residue.
#[test]
fn a_lifted_log_with_a_paired_marker_boots_and_serves_the_lifted_records() {
    let dir = data_root("paired");
    first_life(&dir);
    plant_lifted_tail(
        &dir,
        &[
            RecordView::ColdDisplace { ns: HOT, old_addr: 4096 },
            RecordView::StringPostImage { ns: HOT, key: b"t", value: b"paired" },
        ],
    );
    let (server, stderr) = spawn(&dir).expect("a paired marker is legal");
    assert!(!stderr.contains("recovery failed"), "{stderr}");
    let mut c = Client::connect(server.port);
    c.ok(&[b"INF.NS", b"USE", b"hot"]);
    for i in 0..4u32 {
        assert_eq!(c.get(format!("k:{i}").as_bytes()), format!("$3\r\nv:{i}\r\n"));
    }
    assert_eq!(c.get(b"lifted"), "$5\r\nalive\r\n");
    assert_eq!(c.get(b"t"), "$6\r\npaired\r\n");
    assert_eq!(c.get(b"ghost"), "$-1\r\n", "the discarded life never replays");
    drop(server);
    let _ = std::fs::remove_dir_all(&dir);
}
