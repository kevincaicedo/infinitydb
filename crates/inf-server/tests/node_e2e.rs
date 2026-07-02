//! End-to-end node assembly test (Linux + uring): two real cells on real
//! threads behind one SO_REUSEPORT port, driven over TCP — local fast path,
//! cross-cell Apply round-trips, multi-key aggregation, pipelined reply
//! ordering, HELLO protocol switching, and protocol-error close.
#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::fd::IntoRawFd;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use inf_alloc::BufferPool;
use inf_fabric::{Mesh, MeshConfig};
use inf_foundation::CellId;
use inf_foundation::time::{Clock, StdClock};
use inf_runtime::net::{bound_port, listen_reuseport};
use inf_runtime::{BackendDriver, CellLoop, LoopConfig, UringDriver};
use inf_server::{NodeInfo, NoopObserver, ServerPlane};
use inf_store::{Keyspace, SlotRouter, StoreConfig};

struct Node {
    port: u16,
    stop: Arc<AtomicBool>,
    handles: Vec<std::thread::JoinHandle<()>>,
    /// Control handle of a durable node (manual checkpoint trigger — the
    /// surface `INF.CKPT` rides at S20).
    control: Option<Arc<inf_server::ControlHandle>>,
}

impl Node {
    fn start(cells: u16) -> Node {
        Node::start_with(cells, None, 0)
    }

    /// A node with the durable plane enabled (M2-S08): catalog loaded and
    /// seeded before cells serve, per-cell log recovery, control thread as
    /// the catalog's single writer — the boot order infinityd adopts.
    /// Automatic checkpoints stay off (tests own the trigger).
    fn start_durable(cells: u16, data_dir: &std::path::Path) -> Node {
        Node::start_with(cells, Some(data_dir.to_path_buf()), 0)
    }

    /// Durable node with the bytes-appended checkpoint trigger armed
    /// (M2-S10, ADR-0016 D7).
    fn start_durable_auto_ckpt(
        cells: u16,
        data_dir: &std::path::Path,
        interval_bytes: u64,
    ) -> Node {
        Node::start_with(cells, Some(data_dir.to_path_buf()), interval_bytes)
    }

    fn start_with(
        cells: u16,
        data_dir: Option<std::path::PathBuf>,
        ckpt_interval_bytes: u64,
    ) -> Node {
        let stop = Arc::new(AtomicBool::new(false));
        // Bind cell 0 first on an ephemeral port, then the rest join it.
        let first = listen_reuseport(0).expect("listen");
        let port = bound_port(&first).expect("port");
        let mut listeners = vec![first];
        for _ in 1..cells {
            listeners.push(listen_reuseport(port).expect("listen same port"));
        }
        let fabrics = Mesh::new(cells, MeshConfig { ring_capacity: 1024, data_credits: 256 });
        // Catalog before cells (ADR-0015 D3): the id→definition map must
        // exist before any cell replays records that name ids.
        let boot = data_dir.map(|dir| {
            let catalog = inf_server::load_catalog(&dir).expect("readable catalog");
            let control = inf_server::spawn_control(dir.clone(), catalog.as_ref());
            (dir, catalog, control)
        });
        let mut handles = Vec::new();
        for (i, (fabric, listener)) in fabrics.into_iter().zip(listeners).enumerate() {
            let stop = Arc::clone(&stop);
            let boot = boot.clone();
            handles.push(std::thread::spawn(move || {
                let mut pool = BufferPool::new(256, 4096);
                let mut driver = UringDriver::new(256).expect("uring");
                driver.register_pool(&mut pool).expect("register");
                let node = Rc::new(NodeInfo::default());
                let mut ks = Keyspace::new(StoreConfig::default());
                let mut durable = None;
                if let Some((dir, catalog, control)) = &boot {
                    if let Some(catalog) = catalog {
                        ks.seed_catalog(catalog).expect("seed catalog");
                    }
                    let cfg = inf_server::DurableConfig {
                        data_dir: dir.clone(),
                        staging: inf_log::StagingConfig::default(),
                        segment: inf_log::SegmentConfig {
                            segment_bytes: 8 << 20, // small: tests rotate
                            ..Default::default()
                        },
                        // 0 = automatic trigger off: e2e checkpoints fire
                        // via the control handle so tests own the timing.
                        ckpt: inf_log::CkptConfig {
                            interval_bytes: ckpt_interval_bytes,
                            ..Default::default()
                        },
                    };
                    let (internal_ms, unix_ms) = node.wall_anchor.get();
                    let anchor = inf_store::WallAnchor { internal_ms, unix_ms };
                    let now = StdClock::new().now();
                    let (rotor, _stats) =
                        inf_server::open_cell_log(&mut ks, i as u16, &cfg, anchor, now)
                            .expect("cell log recovery");
                    durable = Some((cfg, rotor, Arc::clone(control)));
                }
                let mut plane = ServerPlane::new(
                    CellId(i as u16),
                    cells,
                    listener.into_raw_fd(),
                    ks,
                    fabric,
                    node,
                    NoopObserver,
                    false,
                );
                if let Some((cfg, rotor, control)) = durable {
                    plane.enable_durable(&cfg, i as u16, rotor).expect("ckpt dir scan");
                    plane.set_control(control);
                }
                let config = LoopConfig {
                    park_default: Some(Duration::from_millis(5)),
                    ..Default::default()
                };
                let mut cell_loop = CellLoop::new(driver, StdClock::new(), pool, config);
                while !stop.load(Ordering::Relaxed) {
                    cell_loop.run_iteration(&mut plane).expect("iteration");
                }
            }));
        }
        let control = boot.map(|(_, _, control)| control);
        Node { port, stop, handles, control }
    }

    fn connect(&self) -> TcpStream {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match TcpStream::connect(("127.0.0.1", self.port)) {
                Ok(s) => {
                    s.set_read_timeout(Some(Duration::from_secs(5))).expect("timeout");
                    s.set_nodelay(true).expect("nodelay");
                    return s;
                }
                Err(e) => assert!(Instant::now() < deadline, "connect: {e}"),
            }
        }
    }

    fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for handle in self.handles.drain(..) {
            handle.join().expect("cell thread");
        }
    }
}

fn cmd(parts: &[&[u8]]) -> Vec<u8> {
    let mut wire = format!("*{}\r\n", parts.len()).into_bytes();
    for p in parts {
        wire.extend_from_slice(format!("${}\r\n", p.len()).as_bytes());
        wire.extend_from_slice(p);
        wire.extend_from_slice(b"\r\n");
    }
    wire
}

fn read_exactly(stream: &mut TcpStream, want: &[u8]) {
    let mut got = vec![0u8; want.len()];
    stream.read_exact(&mut got).expect("read reply");
    assert_eq!(
        got,
        want,
        "reply mismatch: got {:?} want {:?}",
        String::from_utf8_lossy(&got),
        String::from_utf8_lossy(want)
    );
}

/// A key owned by `cell` under an N-cell contiguous router.
fn key_for_cell(cells: u16, cell: u16) -> Vec<u8> {
    let router = SlotRouter::new_contiguous(cells);
    for i in 0..100_000u32 {
        let key = format!("k:{i}");
        if router.cell_of(SlotRouter::slot_of(key.as_bytes())) == CellId(cell) {
            return key.into_bytes();
        }
    }
    panic!("no key found for cell {cell}");
}

#[test]
fn two_cell_node_serves_local_and_cross_cell() {
    let node = Node::start(2);
    let mut client = node.connect();

    let k0 = key_for_cell(2, 0);
    let k1 = key_for_cell(2, 1);

    // The connection landed on ONE cell, so at least one of these keys is
    // remote — both must work identically (pipelined, ordered).
    let mut pipeline = Vec::new();
    pipeline.extend(cmd(&[b"SET", &k0, b"zero"]));
    pipeline.extend(cmd(&[b"SET", &k1, b"one"]));
    pipeline.extend(cmd(&[b"GET", &k0]));
    pipeline.extend(cmd(&[b"GET", &k1]));
    pipeline.extend(cmd(&[b"DEL", &k0, &k1, b"missing"]));
    pipeline.extend(cmd(&[b"GET", &k0]));
    client.write_all(&pipeline).expect("write");
    read_exactly(&mut client, b"+OK\r\n+OK\r\n$4\r\nzero\r\n$3\r\none\r\n:2\r\n$-1\r\n");

    // Interleaving local and remote keys keeps reply order.
    let mut pipeline = Vec::new();
    for round in 0..20 {
        pipeline.extend(cmd(&[b"INCR", &k0]));
        pipeline.extend(cmd(&[b"INCR", &k1]));
        let _ = round;
    }
    client.write_all(&pipeline).expect("write");
    let mut want = Vec::new();
    for round in 1..=20 {
        want.extend_from_slice(format!(":{round}\r\n:{round}\r\n").as_bytes());
    }
    read_exactly(&mut client, &want);

    // EXISTS aggregates across cells, counting duplicates.
    client.write_all(&cmd(&[b"EXISTS", &k0, &k1, &k0, b"nope"])).expect("write");
    read_exactly(&mut client, b":3\r\n");

    node.stop();
}

#[test]
fn hello_switch_and_protocol_error_close() {
    let node = Node::start(2);
    let mut client = node.connect();

    // RESP2 null, switch to RESP3, RESP3 null.
    client.write_all(&cmd(&[b"GET", b"missing"])).expect("write");
    read_exactly(&mut client, b"$-1\r\n");
    client.write_all(&cmd(&[b"HELLO", b"3"])).expect("write");
    let mut header = [0u8; 3];
    client.read_exact(&mut header).expect("hello header");
    assert_eq!(&header, b"%7\r", "RESP3 map reply");
    // Drain the rest of the HELLO map: read until the trailing modules array.
    let mut rest = Vec::new();
    let mut byte = [0u8; 1];
    while !rest.ends_with(b"*0\r\n") {
        client.read_exact(&mut byte).expect("hello body");
        rest.push(byte[0]);
    }
    client.write_all(&cmd(&[b"GET", b"missing"])).expect("write");
    read_exactly(&mut client, b"_\r\n");

    // A protocol error gets an error reply, then the server closes.
    let mut bad = node.connect();
    bad.write_all(b"*1\r\n$NOTANUMBER\r\n").expect("write");
    let mut reply = Vec::new();
    bad.read_to_end(&mut reply).expect("read until close");
    assert!(reply.starts_with(b"-ERR Protocol error"), "got {:?}", String::from_utf8_lossy(&reply));

    node.stop();
}

/// The SELECTed database rides the fabric Apply byte (M1-S08/ADR-0009):
/// cross-cell single-key ops, counted splits, and scatters all act on the
/// origin connection's database — and never leak into db 0.
#[test]
fn select_travels_with_cross_cell_ops() {
    let node = Node::start(2);
    let mut conn = node.connect();
    // One key per owner: remote and local relative to whichever cell
    // accepted this connection.
    let k0 = key_for_cell(2, 0);
    let k1 = key_for_cell(2, 1);
    let mut script = Vec::new();
    script.extend_from_slice(&cmd(&[b"SELECT", b"5"]));
    script.extend_from_slice(&cmd(&[b"SET", &k0, b"zero-owner"]));
    script.extend_from_slice(&cmd(&[b"SET", &k1, b"one-owner"]));
    script.extend_from_slice(&cmd(&[b"GET", &k0]));
    script.extend_from_slice(&cmd(&[b"GET", &k1]));
    script.extend_from_slice(&cmd(&[b"DBSIZE"]));
    script.extend_from_slice(&cmd(&[b"EXISTS", &k0, &k1]));
    script.extend_from_slice(&cmd(&[b"SELECT", b"0"]));
    script.extend_from_slice(&cmd(&[b"DBSIZE"]));
    script.extend_from_slice(&cmd(&[b"MGET", &k0, &k1]));
    conn.write_all(&script).expect("write");
    let mut want = Vec::new();
    want.extend_from_slice(b"+OK\r\n"); // SELECT 5
    want.extend_from_slice(b"+OK\r\n");
    want.extend_from_slice(b"+OK\r\n");
    want.extend_from_slice(b"$10\r\nzero-owner\r\n");
    want.extend_from_slice(b"$9\r\none-owner\r\n");
    want.extend_from_slice(b":2\r\n"); // both keys live in db5 (scattered count)
    want.extend_from_slice(b":2\r\n"); // counted split sees db5
    want.extend_from_slice(b"+OK\r\n"); // SELECT 0
    want.extend_from_slice(b":0\r\n"); // db0 untouched on every cell
    want.extend_from_slice(b"*2\r\n$-1\r\n$-1\r\n"); // gather sees db0
    read_exactly(&mut conn, &want);
    node.stop();
}

/// Reads one complete RESP bulk reply (`$len\r\n<body>\r\n`) and returns
/// the body (INFO parsing).
fn read_bulk(stream: &mut TcpStream) -> Vec<u8> {
    let mut header = Vec::new();
    let mut byte = [0u8; 1];
    while !header.ends_with(b"\r\n") {
        stream.read_exact(&mut byte).expect("bulk header");
        header.push(byte[0]);
    }
    assert_eq!(header.first(), Some(&b'$'), "bulk reply: {header:?}");
    let len: usize = std::str::from_utf8(&header[1..header.len() - 2])
        .expect("ascii")
        .parse()
        .expect("bulk length");
    let mut body = vec![0u8; len + 2];
    stream.read_exact(&mut body).expect("bulk body");
    body.truncate(len);
    body
}

/// INFO text from one connection (RESP2 verbatim = bulk).
fn info_text(conn: &mut TcpStream, section: &[u8]) -> String {
    conn.write_all(&cmd(&[b"INFO", section])).expect("write");
    String::from_utf8(read_bulk(conn)).expect("ascii")
}

/// Connects until landing on `cell` (SO_REUSEPORT spreads arbitrarily).
fn conn_on_cell(node: &Node, cell: u16) -> TcpStream {
    for _ in 0..256 {
        let mut conn = node.connect();
        let info = info_text(&mut conn, b"server");
        if info.contains(&format!("cell:{cell}\r\n")) {
            return conn;
        }
    }
    panic!("no connection landed on cell {cell}");
}

/// M1-S10: channel owned by cell 0, subscribers on both cells, publisher on
/// the non-owner cell — delivery, receiver counts, RESP2/RESP3 frame
/// shapes, pattern fan-out, and the fan-out counter assert
/// (fabric messages ≤ subscriber-bearing cells, the milestone AC).
#[test]
fn pubsub_cross_cell_fanout_and_counters() {
    let node = Node::start(2);
    let ch = key_for_cell(2, 0); // channel owned by cell 0
    let mut sub0 = conn_on_cell(&node, 0);
    let mut sub1 = conn_on_cell(&node, 1);
    let mut publisher = conn_on_cell(&node, 1);

    // RESP2 subscriber on the owner cell.
    sub0.write_all(&cmd(&[b"SUBSCRIBE", &ch])).expect("write");
    let mut want = Vec::new();
    want.extend_from_slice(format!("*3\r\n$9\r\nsubscribe\r\n${}\r\n", ch.len()).as_bytes());
    want.extend_from_slice(&ch);
    want.extend_from_slice(b"\r\n:1\r\n");
    read_exactly(&mut sub0, &want);

    // RESP3 subscriber on the peer cell.
    sub1.write_all(&cmd(&[b"HELLO", b"3"])).expect("write");
    let mut drained = Vec::new();
    let mut byte = [0u8; 1];
    while !drained.ends_with(b"*0\r\n") {
        sub1.read_exact(&mut byte).expect("hello body");
        drained.push(byte[0]);
    }
    sub1.write_all(&cmd(&[b"SUBSCRIBE", &ch])).expect("write");
    let mut want = Vec::new();
    want.extend_from_slice(format!(">3\r\n$9\r\nsubscribe\r\n${}\r\n", ch.len()).as_bytes());
    want.extend_from_slice(&ch);
    want.extend_from_slice(b"\r\n:1\r\n");
    read_exactly(&mut sub1, &want);

    // Publish from the non-owner cell: both subscribers count.
    publisher.write_all(&cmd(&[b"PUBLISH", &ch, b"hello"])).expect("write");
    read_exactly(&mut publisher, b":2\r\n");
    let mut want = Vec::new();
    want.extend_from_slice(format!("*3\r\n$7\r\nmessage\r\n${}\r\n", ch.len()).as_bytes());
    want.extend_from_slice(&ch);
    want.extend_from_slice(b"\r\n$5\r\nhello\r\n");
    read_exactly(&mut sub0, &want);
    let mut want = Vec::new();
    want.extend_from_slice(format!(">3\r\n$7\r\nmessage\r\n${}\r\n", ch.len()).as_bytes());
    want.extend_from_slice(&ch);
    want.extend_from_slice(b"\r\n$5\r\nhello\r\n");
    read_exactly(&mut sub1, &want);

    // Pattern subscriber: pmessage delivery joins, channel frame first.
    sub1.write_all(&cmd(&[b"PSUBSCRIBE", b"k:*"])).expect("write");
    read_exactly(&mut sub1, b">3\r\n$10\r\npsubscribe\r\n$3\r\nk:*\r\n:2\r\n");
    publisher.write_all(&cmd(&[b"PUBLISH", &ch, b"x"])).expect("write");
    read_exactly(&mut publisher, b":3\r\n");
    let mut want = Vec::new();
    want.extend_from_slice(format!("*3\r\n$7\r\nmessage\r\n${}\r\n", ch.len()).as_bytes());
    want.extend_from_slice(&ch);
    want.extend_from_slice(b"\r\n$1\r\nx\r\n");
    read_exactly(&mut sub0, &want);
    let mut want = Vec::new();
    want.extend_from_slice(format!(">3\r\n$7\r\nmessage\r\n${}\r\n", ch.len()).as_bytes());
    want.extend_from_slice(&ch);
    want.extend_from_slice(b"\r\n$1\r\nx\r\n>4\r\n$8\r\npmessage\r\n$3\r\nk:*\r\n");
    want.extend_from_slice(format!("${}\r\n", ch.len()).as_bytes());
    want.extend_from_slice(&ch);
    want.extend_from_slice(b"\r\n$1\r\nx\r\n");
    read_exactly(&mut sub1, &want);

    // PUBSUB introspection over the owner views.
    publisher.write_all(&cmd(&[b"PUBSUB", b"NUMSUB", &ch])).expect("write");
    let mut want = Vec::new();
    want.extend_from_slice(format!("*2\r\n${}\r\n", ch.len()).as_bytes());
    want.extend_from_slice(&ch);
    want.extend_from_slice(b"\r\n:2\r\n");
    read_exactly(&mut publisher, &want);
    publisher.write_all(&cmd(&[b"PUBSUB", b"NUMPAT"])).expect("write");
    read_exactly(&mut publisher, b":1\r\n");

    // The M1-S10 counter AC: fan-out messages == subscriber-bearing remote
    // cells (cell 1, twice — never per subscriber). The owner is cell 0.
    let mut probe0 = conn_on_cell(&node, 0);
    let info = info_text(&mut probe0, b"tripwires");
    assert!(info.contains("pubsub_fan_msgs:2"), "one fan msg per publish: {info}");

    // Unsubscribe drops the channel leg; the pattern still matches.
    sub0.write_all(&cmd(&[b"UNSUBSCRIBE", &ch])).expect("write");
    let mut want = Vec::new();
    want.extend_from_slice(format!("*3\r\n$11\r\nunsubscribe\r\n${}\r\n", ch.len()).as_bytes());
    want.extend_from_slice(&ch);
    want.extend_from_slice(b"\r\n:0\r\n");
    read_exactly(&mut sub0, &want);
    publisher.write_all(&cmd(&[b"PUBLISH", &ch, b"z"])).expect("write");
    read_exactly(&mut publisher, b":2\r\n");

    node.stop();
}

/// M1-S11: a subscriber whose staged output exceeds the configured pubsub
/// hard cap is disconnected; the INFO counter increments; the registries
/// unwind (a later PUBLISH counts zero receivers).
#[test]
fn slow_subscriber_hits_the_output_cap_and_dies() {
    let node = Node::start(2);
    let ch = key_for_cell(2, 0);
    let mut sub = node.connect();
    sub.write_all(&cmd(&[b"SUBSCRIBE", &ch])).expect("write");
    let mut want = Vec::new();
    want.extend_from_slice(format!("*3\r\n$9\r\nsubscribe\r\n${}\r\n", ch.len()).as_bytes());
    want.extend_from_slice(&ch);
    want.extend_from_slice(b"\r\n:1\r\n");
    read_exactly(&mut sub, &want);

    let mut publisher = node.connect();
    publisher
        .write_all(&cmd(&[b"CONFIG", b"SET", b"client-output-buffer-limit", b"pubsub 512 0 0"]))
        .expect("write");
    read_exactly(&mut publisher, b"+OK\r\n");

    // One 2 KiB message blows the 512 B hard cap at delivery time.
    let payload = vec![b'x'; 2048];
    publisher.write_all(&cmd(&[b"PUBLISH", &ch, &payload])).expect("write");
    read_exactly(&mut publisher, b":1\r\n");

    // The subscriber is killed by the MAINTAIN sweep: EOF after whatever
    // partial output flushed first.
    let mut sink = Vec::new();
    sub.read_to_end(&mut sink).expect("read until close");

    // Registry unwound (close-path cleanup): no receivers remain.
    publisher.write_all(&cmd(&[b"PUBLISH", &ch, b"after"])).expect("write");
    read_exactly(&mut publisher, b":0\r\n");

    // The disconnect counter incremented on the subscriber's cell.
    let mut probe0 = conn_on_cell(&node, 0);
    let mut probe1 = conn_on_cell(&node, 1);
    let kills: u64 = [info_text(&mut probe0, b"stats"), info_text(&mut probe1, b"stats")]
        .iter()
        .map(|info| {
            info.lines()
                .find_map(|l| l.strip_prefix("client_output_buffer_limit_disconnections:"))
                .and_then(|v| v.trim().parse::<u64>().ok())
                .unwrap_or(0)
        })
        .sum();
    assert_eq!(kills, 1, "exactly one output-cap disconnect");

    node.stop();
}

#[test]
fn many_connections_spread_across_cells() {
    let node = Node::start(2);
    let mut clients: Vec<TcpStream> = (0..16).map(|_| node.connect()).collect();
    for (i, c) in clients.iter_mut().enumerate() {
        let key = format!("conn:{i}");
        c.write_all(&cmd(&[b"SET", key.as_bytes(), b"v"])).expect("write");
    }
    for c in &mut clients {
        read_exactly(c, b"+OK\r\n");
    }
    // Every key readable from one final connection (cross-cell GETs).
    let mut last = node.connect();
    for i in 0..16 {
        let key = format!("conn:{i}");
        last.write_all(&cmd(&[b"GET", key.as_bytes()])).expect("write");
        read_exactly(&mut last, b"$1\r\nv\r\n");
    }
    node.stop();
}

// ---- M2-S08: durable namespaces ------------------------------------------------

fn temp_data_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("inf-s08-{tag}-{}", std::process::id()));
    if dir.exists() {
        std::fs::remove_dir_all(&dir).expect("clear stale test dir");
    }
    std::fs::create_dir_all(&dir).expect("create test dir");
    dir
}

fn read_line(stream: &mut TcpStream) -> Vec<u8> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        stream.read_exact(&mut byte).expect("read byte");
        line.push(byte[0]);
        if line.ends_with(b"\r\n") {
            return line;
        }
    }
}

/// `INF.NS CREATE … MODE durable` goes live: create → USE → write → read,
/// `always` acks return (after fsync — the reply itself is the proof the
/// gate opened), then a full restart recovers both the namespace
/// definition (catalog META) and the data (log replay). The S08 ACs
/// "create durable ns → write → restart → recover" and "M1's error is
/// gone", end to end over TCP.
#[test]
fn durable_namespace_survives_restart() {
    let dir = temp_data_dir("restart");
    let node = Node::start_durable(1, &dir);
    let mut c = node.connect();

    c.write_all(&cmd(&[b"INF.NS", b"CREATE", b"ledger", b"MODE", b"durable", b"FSYNC", b"always"]))
        .expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    c.write_all(&cmd(&[b"INF.NS", b"USE", b"ledger"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    // `always`: the +OK below is fsync-gated (§8.2 ack point).
    c.write_all(&cmd(&[b"SET", b"acct:1", b"100"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    c.write_all(&cmd(&[b"SET", b"sess:9", b"tok", b"EX", b"1000"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    c.write_all(&cmd(&[b"DEL", b"acct:gone"])).expect("write");
    read_exactly(&mut c, b":0\r\n");
    c.write_all(&cmd(&[b"GET", b"acct:1"])).expect("write");
    read_exactly(&mut c, b"$3\r\n100\r\n");
    let info = {
        c.write_all(&cmd(&[b"INF.NS", b"INFO", b"ledger"])).expect("write");
        let mut buf = vec![0u8; 512];
        let n = c.read(&mut buf).expect("read info");
        String::from_utf8_lossy(&buf[..n]).into_owned()
    };
    assert!(info.contains("always"), "INFO reports the fsync class: {info}");
    drop(c);
    node.stop();

    // Restart on the same data dir: definition + data both present.
    let node = Node::start_durable(1, &dir);
    let mut c = node.connect();
    c.write_all(&cmd(&[b"INF.NS", b"USE", b"ledger"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    c.write_all(&cmd(&[b"GET", b"acct:1"])).expect("write");
    read_exactly(&mut c, b"$3\r\n100\r\n");
    c.write_all(&cmd(&[b"GET", b"acct:gone"])).expect("write");
    read_exactly(&mut c, b"$-1\r\n");
    // The replayed TTL is armed (exact value shifts with the test clock
    // anchor; the deadline's existence is the replay assertion).
    c.write_all(&cmd(&[b"TTL", b"sess:9"])).expect("write");
    let ttl = read_line(&mut c);
    assert!(ttl.starts_with(b":") && ttl != b":-1\r\n" && ttl != b":-2\r\n", "{ttl:?}");
    drop(c);
    node.stop();
    std::fs::remove_dir_all(&dir).ok();
}

/// Mixed classes interleaved on one cell (§8.2 semantics table): `memory`,
/// `everysec`, and `always` namespaces each honor their ack point, and the
/// memory namespace appends zero log records (the L2 null-log case,
/// counter-asserted through INFO persistence).
#[test]
fn mixed_classes_share_one_cell_and_memory_stays_off_the_log() {
    let dir = temp_data_dir("mixed");
    let node = Node::start_durable(1, &dir);
    let mut c = node.connect();

    for create in [
        &cmd(&[b"INF.NS", b"CREATE", b"cache2", b"MODE", b"memory"]),
        &cmd(&[b"INF.NS", b"CREATE", b"sess", b"MODE", b"durable", b"FSYNC", b"everysec"]),
        &cmd(&[b"INF.NS", b"CREATE", b"led", b"MODE", b"durable", b"FSYNC", b"always"]),
    ] {
        c.write_all(create).expect("write");
        read_exactly(&mut c, b"+OK\r\n");
    }
    // Interleave writes across the three classes on one connection (one
    // cell ⇒ one shared frame/fsync per iteration by construction).
    for (ns, key, value) in [
        (&b"cache2"[..], &b"k"[..], &b"mem"[..]),
        (b"sess", b"k", b"sec"),
        (b"led", b"k", b"alw"),
        (b"cache2", b"k2", b"mem2"),
        (b"led", b"k2", b"alw2"),
    ] {
        c.write_all(&cmd(&[b"INF.NS", b"USE", ns])).expect("write");
        read_exactly(&mut c, b"+OK\r\n");
        c.write_all(&cmd(&[b"SET", key, value])).expect("write");
        read_exactly(&mut c, b"+OK\r\n");
    }
    for (ns, key, want) in [
        (&b"cache2"[..], &b"k"[..], &b"$3\r\nmem\r\n"[..]),
        (b"sess", b"k", b"$3\r\nsec\r\n"),
        (b"led", b"k", b"$3\r\nalw\r\n"),
    ] {
        c.write_all(&cmd(&[b"INF.NS", b"USE", ns])).expect("write");
        read_exactly(&mut c, b"+OK\r\n");
        c.write_all(&cmd(&[b"GET", key])).expect("write");
        read_exactly(&mut c, want);
    }
    // Zero-cost assert (M2-S09 mechanism): exactly the durable records —
    // one everysec + two always SETs — hit the log; the two memory-ns SETs
    // stayed off it. The gauge flushes via MAINTAIN, so poll briefly.
    let deadline = Instant::now() + Duration::from_secs(5);
    let info = loop {
        c.write_all(&cmd(&[b"INFO", b"persistence"])).expect("write");
        let mut buf = vec![0u8; 2048];
        let n = c.read(&mut buf).expect("read info");
        let info = String::from_utf8_lossy(&buf[..n]).into_owned();
        if info.contains("log_records_appended:3") || Instant::now() > deadline {
            break info;
        }
    };
    assert!(info.contains("log_records_appended:3"), "2 always + 1 everysec, zero memory: {info}");
    drop(c);
    node.stop();
    std::fs::remove_dir_all(&dir).ok();
}

/// Cross-cell durable writes (ADR-0015 D1/D6): a 2-cell node, an `always`
/// namespace, keys owned by both cells — remote writes ride `ApplyNs` and
/// their acks return only after the OWNING cell's fsync (the deferred
/// fabric reply), then every key survives restart.
#[test]
fn durable_cross_cell_applyns_round_trip() {
    let dir = temp_data_dir("xcell");
    let node = Node::start_durable(2, &dir);
    let mut c = node.connect();

    c.write_all(&cmd(&[b"INF.NS", b"CREATE", b"led2", b"MODE", b"durable", b"FSYNC", b"always"]))
        .expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    c.write_all(&cmd(&[b"INF.NS", b"USE", b"led2"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    let k0 = key_for_cell(2, 0);
    let k1 = key_for_cell(2, 1);
    for (k, v) in [(&k0, &b"zero"[..]), (&k1, b"one")] {
        c.write_all(&cmd(&[b"SET", k, v])).expect("write");
        read_exactly(&mut c, b"+OK\r\n");
    }
    for (k, want) in [(&k0, &b"$4\r\nzero\r\n"[..]), (&k1, b"$3\r\none\r\n")] {
        c.write_all(&cmd(&[b"GET", k])).expect("write");
        read_exactly(&mut c, want);
    }
    drop(c);
    node.stop();

    let node = Node::start_durable(2, &dir);
    let mut c = node.connect();
    c.write_all(&cmd(&[b"INF.NS", b"USE", b"led2"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    for (k, want) in [(&k0, &b"$4\r\nzero\r\n"[..]), (&k1, b"$3\r\none\r\n")] {
        c.write_all(&cmd(&[b"GET", k])).expect("write");
        read_exactly(&mut c, want);
    }
    drop(c);
    node.stop();
    std::fs::remove_dir_all(&dir).ok();
}

/// M2-S10: a fuzzy checkpoint streams to `ckpt-000001.ick` on the REAL
/// reactor path (uring `LogWrite` sections on the `.ick` fd, `CkptSync`
/// completion barrier, rename + dir-fsync publication) while writes keep
/// flowing — then the published file validates end to end (per-section
/// CRC, footer digest + counts) and a restart on the same dir still
/// replays cleanly (the begin marker is a counted skip).
#[test]
fn fuzzy_checkpoint_streams_under_live_writes() {
    let dir = temp_data_dir("ckpt");
    let node = Node::start_durable(1, &dir);
    let mut c = node.connect();

    c.write_all(&cmd(&[
        b"INF.NS",
        b"CREATE",
        b"books",
        b"MODE",
        b"durable",
        b"FSYNC",
        b"everysec",
    ]))
    .expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    c.write_all(&cmd(&[b"INF.NS", b"USE", b"books"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    for i in 0..400 {
        let key = format!("book:{i:04}");
        let value = format!("title-{i}");
        c.write_all(&cmd(&[b"SET", key.as_bytes(), value.as_bytes()])).expect("write");
        read_exactly(&mut c, b"+OK\r\n");
    }
    c.write_all(&cmd(&[b"SET", b"book:ttl", b"loan", b"EX", b"5000"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");

    // Manual trigger (the surface INF.CKPT rides at S20), then keep
    // writing while the walker streams — the dirty-under-checkpoint shape
    // on the real path.
    node.control.as_ref().expect("durable node").request_ckpt_all();
    let deadline = Instant::now() + Duration::from_secs(20);
    let info = loop {
        for i in 0..20 {
            let key = format!("book:{:04}", 4000 + i);
            c.write_all(&cmd(&[b"SET", key.as_bytes(), b"late"])).expect("write");
            read_exactly(&mut c, b"+OK\r\n");
        }
        c.write_all(&cmd(&[b"INFO", b"persistence"])).expect("write");
        let mut buf = vec![0u8; 4096];
        let n = c.read(&mut buf).expect("read info");
        let info = String::from_utf8_lossy(&buf[..n]).into_owned();
        if info.contains("ckpts_completed:1") || Instant::now() > deadline {
            break info;
        }
    };
    assert!(info.contains("ckpts_completed:1"), "checkpoint completed under load: {info}");
    assert!(info.contains("ckpts_aborted:0"), "no aborts: {info}");
    assert!(info.contains("ckpt_buffer_bytes:0"), "buffer domain freed at completion: {info}");
    drop(c);
    node.stop();

    // The published .ick validates end to end and covers the seed writes.
    let ick = dir.join("shard-0").join("ckpt").join("ckpt-000001.ick");
    let mut post_images = 0u64;
    let (ick_info, audit) = inf_log::ckpt::read_ick(
        &inf_log::fs::StdSegmentFs,
        &ick,
        inf_log::ckpt::IckReaderConfig::default(),
        |view| {
            if matches!(view, inf_log::RecordView::StringPostImage { .. }) {
                post_images += 1;
            }
            Ok::<(), ()>(())
        },
    )
    .expect("published checkpoint validates");
    assert_eq!(ick_info.cell, 0);
    assert_eq!(ick_info.ckpt_id, 1);
    assert!(ick_info.begin_lsn.to_u64() > 0, "begin LSN recorded");
    assert!(post_images >= 401, "walk covered the pre-trigger writes: {post_images}");
    assert_eq!(audit.entries_per_ns.len(), 1, "one durable namespace walked");

    // Restart on the same dir: replay (which now crosses the begin
    // marker) still yields the data.
    let node = Node::start_durable(1, &dir);
    let mut c = node.connect();
    c.write_all(&cmd(&[b"INF.NS", b"USE", b"books"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    c.write_all(&cmd(&[b"GET", b"book:0000"])).expect("write");
    read_exactly(&mut c, b"$7\r\ntitle-0\r\n");
    drop(c);
    node.stop();
    std::fs::remove_dir_all(&dir).ok();
}

/// M2-S10 trigger policy v1: the bytes-appended-since-last-checkpoint
/// threshold fires without any manual request (ADR-0016 D7).
#[test]
fn bytes_threshold_triggers_a_checkpoint() {
    let dir = temp_data_dir("ckpt-auto");
    let node = Node::start_durable_auto_ckpt(1, &dir, 16 << 10);
    let mut c = node.connect();
    c.write_all(&cmd(&[b"INF.NS", b"CREATE", b"logs", b"MODE", b"durable", b"FSYNC", b"everysec"]))
        .expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    c.write_all(&cmd(&[b"INF.NS", b"USE", b"logs"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    let value = vec![b'v'; 256];
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut i = 0u32;
    let info = loop {
        for _ in 0..64 {
            let key = format!("evt:{i:06}");
            c.write_all(&cmd(&[b"SET", key.as_bytes(), &value])).expect("write");
            read_exactly(&mut c, b"+OK\r\n");
            i += 1;
        }
        c.write_all(&cmd(&[b"INFO", b"persistence"])).expect("write");
        let mut buf = vec![0u8; 4096];
        let n = c.read(&mut buf).expect("read info");
        let info = String::from_utf8_lossy(&buf[..n]).into_owned();
        if !info.contains("ckpts_completed:0") || Instant::now() > deadline {
            break info;
        }
    };
    assert!(!info.contains("ckpts_completed:0"), "threshold trigger produced a checkpoint: {info}");
    drop(c);
    node.stop();
    std::fs::remove_dir_all(&dir).ok();
}

/// M2-S10 slice-budget rehearsal (dev tier — run manually, output feeds
/// the artifact): a few-hundred-MB durable dataset, checkpoint triggered
/// under continuous GET load, foreground latency sampled per request and
/// the loop-iteration p99.9 scraped before/after. The binding
/// checkpoint-under-full-load row is M2-S12/S22 (saturating writes,
/// reference box); this run proves the budgeted-slice mechanism holds a
/// flat foreground tail while sections stream.
///
/// `cargo test -p inf-server --release --test node_e2e -- --ignored
/// ckpt_slice_budget_rehearsal --nocapture`
#[test]
#[ignore = "manual evidence run (writes the S10 dev-tier artifact input)"]
fn ckpt_slice_budget_rehearsal() {
    let dir = temp_data_dir("ckpt-slice");
    let node = Node::start_durable(1, &dir);
    let mut c = node.connect();
    c.write_all(&cmd(&[b"INF.NS", b"CREATE", b"bulk", b"MODE", b"durable", b"FSYNC", b"everysec"]))
        .expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    c.write_all(&cmd(&[b"INF.NS", b"USE", b"bulk"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");

    // Fill ~240 MB: 240k keys x 1 KiB, pipelined in batches.
    let value = vec![b'd'; 1024];
    let keys: u32 = 240_000;
    let batch: u32 = 512;
    let fill_started = Instant::now();
    let mut written = 0u32;
    while written < keys {
        let n = batch.min(keys - written);
        let mut wire = Vec::with_capacity(1100 * n as usize);
        for i in written..written + n {
            wire.extend_from_slice(&cmd(&[b"SET", format!("blob:{i:07}").as_bytes(), &value]));
        }
        c.write_all(&wire).expect("write batch");
        for _ in 0..n {
            read_exactly(&mut c, b"+OK\r\n");
        }
        written += n;
    }
    println!("fill: {keys} x 1 KiB in {:.1}s", fill_started.elapsed().as_secs_f64());

    let info_before = {
        c.write_all(&cmd(&[b"INFO"])).expect("write");
        let mut buf = Vec::new();
        let mut chunk = [0u8; 65536];
        loop {
            let n = c.read(&mut chunk).expect("read info");
            buf.extend_from_slice(&chunk[..n]);
            if buf.windows(2).rev().take(64).any(|w| w == b"\r\n") && n < chunk.len() {
                break;
            }
        }
        String::from_utf8_lossy(&buf).into_owned()
    };

    // Trigger, then hammer GETs and sample per-request latency until the
    // checkpoint completes.
    node.control.as_ref().expect("durable").request_ckpt_all();
    let ckpt_started = Instant::now();
    let mut max_get_us = 0u128;
    let mut gets = 0u64;
    let mut over_2ms = 0u64;
    let deadline = Instant::now() + Duration::from_secs(120);
    let done = loop {
        for i in 0..64 {
            let key = format!(
                "blob:{:07}",
                (gets as u32).wrapping_mul(2654435761).wrapping_add(i) % keys
            );
            let started = Instant::now();
            c.write_all(&cmd(&[b"GET", key.as_bytes()])).expect("write");
            let mut hdr = [0u8; 8];
            c.read_exact(&mut hdr).expect("len header");
            assert_eq!(&hdr[..5], b"$1024", "value present");
            let mut rest = vec![0u8; 1024 + 1]; // remainder of "$1024\r\n" + payload + crlf
            c.read_exact(&mut rest).expect("payload");
            let us = started.elapsed().as_micros();
            max_get_us = max_get_us.max(us);
            if us > 2_000 {
                over_2ms += 1;
            }
            gets += 1;
        }
        c.write_all(&cmd(&[b"INFO", b"persistence"])).expect("write");
        let mut buf = vec![0u8; 4096];
        let n = c.read(&mut buf).expect("read info");
        let info = String::from_utf8_lossy(&buf[..n]).into_owned();
        if info.contains("ckpts_completed:1") {
            break true;
        }
        if Instant::now() > deadline {
            break false;
        }
    };
    let ckpt_secs = ckpt_started.elapsed().as_secs_f64();
    assert!(done, "checkpoint completed within the deadline");

    let info_after = {
        c.write_all(&cmd(&[b"INFO"])).expect("write");
        let mut buf = vec![0u8; 65536];
        let n = c.read(&mut buf).expect("read info");
        String::from_utf8_lossy(&buf[..n]).into_owned()
    };
    let iter_p999 = |s: &str| {
        s.lines()
            .find(|l| l.starts_with("loop_iter_p999_us"))
            .map(|l| l.trim().to_string())
            .unwrap_or_default()
    };
    println!("checkpoint: completed in {ckpt_secs:.2}s under GET load");
    println!("foreground GETs during ckpt: {gets} · max {max_get_us} µs · >2ms: {over_2ms}");
    println!("loop_iter before: {} · after: {}", iter_p999(&info_before), iter_p999(&info_after));

    // Audit the published file (size + sections).
    let ick = dir.join("shard-0").join("ckpt").join("ckpt-000001.ick");
    let (_, audit) = inf_log::ckpt::read_ick(
        &inf_log::fs::StdSegmentFs,
        &ick,
        inf_log::ckpt::IckReaderConfig::default(),
        |_| Ok::<(), ()>(()),
    )
    .expect("published checkpoint validates");
    println!(
        "ick: {} sections · {} records · {} MiB",
        audit.sections,
        audit.records,
        audit.bytes >> 20
    );
    drop(c);
    node.stop();
    std::fs::remove_dir_all(&dir).ok();
}
