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
use inf_store::{CellStore, SlotRouter, StoreConfig};

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
                    CellStore::new(StoreConfig::default()),
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
