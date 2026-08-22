//! M2-S17 fsyncgate row (ADR-0020 D3, §8.4): injected EIO on the fsync
//! completion of a live uring node → the process **fail-stops** with
//! [`inf_server::EXIT_DURABLE_FAILSTOP`], **zero acks** are emitted for
//! any write in the failed sync's batch (the watermark froze), and a
//! restart on the same directory recovers with the pre-batch acked state
//! intact — fsync failure is never caught-and-continued, never retried
//! against possibly-clean pages (the PostgreSQL fsyncgate lesson).
//!
//! Shape: the parent test re-invokes this test binary as a **child
//! process** (`--exact fsyncgate_child --ignored`) so the exit code is
//! the real process contract, not a panic captured by libtest. The
//! `durable_fsync_eio` point is armed `Nth(2)` on the cell thread: the
//! registry is thread-local, so control-thread META barriers never count
//! — fsync completion #1 is the first SET's ack (it must pass), #2 is
//! the second SET's, which fires.
//!
//! Kill-physics disclosure (L10): the failed batch's *bytes* survive —
//! the LogWrite completed and the injected failure hits the completion
//! path after the real fdatasync — so the restart legitimately replays
//! the un-acked write. §8.2 makes no promise either way for un-acked
//! writes; the tier where those bytes vanish is the M2-S18 sim disk.
#![cfg(target_os = "linux")]

use std::io::Read;
use std::io::Write;
use std::net::TcpStream;
use std::os::fd::IntoRawFd;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use inf_alloc::BufferPool;
use inf_fabric::{Mesh, MeshConfig};
use inf_foundation::CellId;
use inf_foundation::fault::FaultSpec;
use inf_foundation::time::{Clock, StdClock};
use inf_runtime::net::{bound_port, listen_reuseport};
use inf_runtime::{BackendDriver, CellLoop, LoopConfig, UringDriver};
use inf_server::{NodeInfo, NoopObserver, ServerPlane};
use inf_store::{Keyspace, StoreConfig};

/// Minimal single-cell durable node (the node_e2e assembly, trimmed).
/// Returns the bound port and the thread handles; the node serves until
/// `stop` flips — or until a durable fail-stop exits the process.
fn start_node(
    data_dir: &Path,
    faults: Vec<(&'static str, FaultSpec)>,
    stop: Arc<AtomicBool>,
) -> (u16, Vec<std::thread::JoinHandle<()>>) {
    let listener = listen_reuseport(0).expect("listen");
    let port = bound_port(&listener).expect("port");
    let catalog = inf_server::load_catalog(data_dir).expect("readable catalog");
    let boot_unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let control =
        inf_server::spawn_control(data_dir.to_path_buf(), catalog.as_ref(), 1, boot_unix_ms);
    let dir = data_dir.to_path_buf();
    let fabric = Mesh::new(1, MeshConfig { ring_capacity: 1024, data_credits: 256 })
        .into_iter()
        .next()
        .expect("one cell");
    let board = Arc::clone(&control);
    let handle = std::thread::spawn(move || {
        for &(point, spec) in &faults {
            inf_foundation::fault::arm(point, spec);
        }
        let mut pool = BufferPool::new(256, 4096);
        let mut driver = UringDriver::new(256).expect("uring");
        driver.register_pool(&mut pool).expect("register");
        let mut ks = Keyspace::new(StoreConfig::default());
        if let Some(catalog) = &catalog {
            ks.seed_catalog(catalog).expect("seed catalog");
        }
        let cfg = inf_server::DurableConfig {
            data_dir: dir,
            staging: inf_log::StagingConfig::default(),
            segment: inf_log::SegmentConfig::default(),
            ckpt: inf_log::CkptConfig { interval_bytes: 0, ..Default::default() },
            recover: Default::default(),
            flush_bound: 1,
            fua_p50_us_probed: 0,
            device: Default::default(),
            fill: Default::default(),
        };
        let mut plane = ServerPlane::new(
            CellId(0),
            1,
            listener.into_raw_fd(),
            ks,
            fabric,
            Rc::new(NodeInfo::default()),
            NoopObserver,
            false,
        );
        plane.set_control(Arc::clone(&board));
        plane.begin_recovery(inf_server::StdSegmentFs, &cfg, 0, StdClock::new().now());
        let config =
            LoopConfig { park_default: Some(Duration::from_millis(5)), ..Default::default() };
        let mut cell_loop = CellLoop::new(driver, StdClock::new(), pool, config);
        while !stop.load(Ordering::Relaxed) {
            cell_loop.run_iteration(&mut plane).expect("iteration");
            if let Some(err) = plane.take_boot_error() {
                panic!("recovery failed (fail-stop, §8.4): {err}");
            }
        }
    });
    // Serve only once recovery finished (fresh dirs finish immediately).
    let deadline = Instant::now() + Duration::from_secs(30);
    while !control.recovery_board().all_ready() {
        assert!(Instant::now() < deadline, "recovery did not finish in 30s");
        #[allow(clippy::disallowed_methods)] // test harness thread, not cell code
        std::thread::sleep(Duration::from_millis(1));
    }
    (port, vec![handle])
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

/// Child body: boots the node with `durable_fsync_eio` armed on the cell
/// thread, publishes its port, and serves until the fail-stop exits the
/// process (the parent asserts the exit code). Ignored so it only ever
/// runs via the parent's re-invocation.
///
/// Nth(8): the first six fsync completions on this single-cell assembly
/// are the M2.5-S01 metadata barriers (log/ckpt/shard/parent dir handles
/// and the fresh segment fd at boot, then the first MAINTAIN's deferred
/// next-segment prealloc dir barrier). Completion seven is the first
/// SET's linked fsync (its ack must land); completion eight — the second
/// SET's — reports the injected EIO.
#[test]
#[ignore = "fsyncgate child body — run only via fsyncgate_fail_stop"]
fn fsyncgate_child() {
    let dir = PathBuf::from(std::env::var("FSYNCGATE_DIR").expect("run via the parent test"));
    let port_file = PathBuf::from(std::env::var("FSYNCGATE_PORT_FILE").expect("port file"));
    let stop = Arc::new(AtomicBool::new(false));
    let (port, handles) = start_node(
        &dir,
        vec![(inf_server::fault::DURABLE_FSYNC_EIO, FaultSpec::Nth(8))],
        Arc::clone(&stop),
    );
    std::fs::write(&port_file, port.to_string()).expect("publish port");
    // Serve until the injected fsync EIO fail-stops the process. The
    // parent kills us if that never happens (its failure mode).
    for handle in handles {
        handle.join().expect("cell thread");
    }
}

fn wait_exit(child: &mut Child, timeout: Duration) -> std::process::ExitStatus {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            return status;
        }
        if Instant::now() > deadline {
            child.kill().ok();
            panic!("fsyncgate child did not exit within {timeout:?} — no fail-stop happened");
        }
        #[allow(clippy::disallowed_methods)] // test harness, not cell code
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn fsyncgate_fail_stop() {
    let dir = std::env::temp_dir().join(format!("inf-fsyncgate-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).expect("data dir");
    let port_file = dir.join("port");

    let exe = std::env::current_exe().expect("test binary");
    let mut child = Command::new(&exe)
        .args(["--exact", "fsyncgate_child", "--ignored", "--nocapture"])
        .env("FSYNCGATE_DIR", &dir)
        .env("FSYNCGATE_PORT_FILE", &port_file)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn child");

    // Discover the child's port.
    let deadline = Instant::now() + Duration::from_secs(30);
    let port: u16 = loop {
        if let Ok(text) = std::fs::read_to_string(&port_file)
            && let Ok(port) = text.trim().parse()
        {
            break port;
        }
        assert!(Instant::now() < deadline, "child never published its port");
        #[allow(clippy::disallowed_methods)] // test harness, not cell code
        std::thread::sleep(Duration::from_millis(10));
    };
    let mut c = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    c.set_read_timeout(Some(Duration::from_secs(10))).expect("timeout");
    c.set_nodelay(true).expect("nodelay");

    // Durable ns; first SET durably acked — fsync completion #1 passes.
    c.write_all(&cmd(&[b"INF.NS", b"CREATE", b"gate", b"MODE", b"durable", b"FSYNC", b"always"]))
        .expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    c.write_all(&cmd(&[b"INF.NS", b"USE", b"gate"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    c.write_all(&cmd(&[b"SET", b"acct:1", b"pre-batch"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");

    // Second SET: its covering fsync completion fires the point → EIO →
    // fail-stop. ZERO bytes may arrive for it (the frozen watermark is
    // the no-ack proof) — the connection just dies.
    c.write_all(&cmd(&[b"SET", b"acct:2", b"in-failed-batch"])).expect("write");
    let mut leftover = Vec::new();
    match c.read_to_end(&mut leftover) {
        Ok(_) => {}
        Err(err) => assert!(
            matches!(err.kind(), std::io::ErrorKind::ConnectionReset),
            "unexpected socket error: {err}"
        ),
    }
    assert!(
        leftover.is_empty(),
        "an ack escaped for the failed batch: {:?}",
        String::from_utf8_lossy(&leftover)
    );

    // The process contract: exit code 3 + the typed stderr line.
    let status = wait_exit(&mut child, Duration::from_secs(30));
    assert_eq!(
        status.code(),
        Some(inf_server::EXIT_DURABLE_FAILSTOP),
        "fail-stop must exit with the formalized code"
    );
    let mut stderr = String::new();
    child.stderr.take().expect("piped").read_to_string(&mut stderr).expect("stderr");
    assert!(
        stderr.contains("fail-stop, §8.4") && stderr.contains("errno 5"),
        "typed fail-stop line missing from stderr: {stderr}"
    );

    // Restart on the same directory: recovery succeeds and the pre-batch
    // acked state is intact. The un-acked write's bytes survived the
    // kill (completed LogWrite — kill physics, disclosed in module docs).
    let stop = Arc::new(AtomicBool::new(false));
    let (port, handles) = start_node(&dir, Vec::new(), Arc::clone(&stop));
    let mut c = TcpStream::connect(("127.0.0.1", port)).expect("reconnect");
    c.set_read_timeout(Some(Duration::from_secs(10))).expect("timeout");
    c.write_all(&cmd(&[b"INF.NS", b"USE", b"gate"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    c.write_all(&cmd(&[b"GET", b"acct:1"])).expect("write");
    read_exactly(&mut c, b"$9\r\npre-batch\r\n");
    c.write_all(&cmd(&[b"GET", b"acct:2"])).expect("write");
    read_exactly(&mut c, b"$15\r\nin-failed-batch\r\n");
    drop(c);
    stop.store(true, Ordering::Relaxed);
    for handle in handles {
        handle.join().expect("cell thread");
    }
    std::fs::remove_dir_all(&dir).ok();
}
