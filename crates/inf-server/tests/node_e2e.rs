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
use inf_foundation::time::StdClock;
use inf_runtime::net::{bound_port, listen_reuseport};
use inf_runtime::{BackendDriver, CellLoop, LoopConfig, UringDriver};
use inf_server::{NodeInfo, NoopObserver, ServerPlane};
use inf_store::{Keyspace, SlotRouter, StoreConfig};

struct Node {
    port: u16,
    stop: Arc<AtomicBool>,
    handles: Vec<std::thread::JoinHandle<()>>,
}

impl Node {
    fn start(cells: u16) -> Node {
        let stop = Arc::new(AtomicBool::new(false));
        // Bind cell 0 first on an ephemeral port, then the rest join it.
        let first = listen_reuseport(0).expect("listen");
        let port = bound_port(&first).expect("port");
        let mut listeners = vec![first];
        for _ in 1..cells {
            listeners.push(listen_reuseport(port).expect("listen same port"));
        }
        let fabrics = Mesh::new(cells, MeshConfig { ring_capacity: 1024, data_credits: 256 });
        let mut handles = Vec::new();
        for (i, (fabric, listener)) in fabrics.into_iter().zip(listeners).enumerate() {
            let stop = Arc::clone(&stop);
            handles.push(std::thread::spawn(move || {
                let mut pool = BufferPool::new(256, 4096);
                let mut driver = UringDriver::new(256).expect("uring");
                driver.register_pool(&mut pool).expect("register");
                let node = Rc::new(NodeInfo::default());
                let mut plane = ServerPlane::new(
                    CellId(i as u16),
                    cells,
                    listener.into_raw_fd(),
                    Keyspace::new(StoreConfig::default()),
                    fabric,
                    node,
                    NoopObserver,
                    false,
                );
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
        Node { port, stop, handles }
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
