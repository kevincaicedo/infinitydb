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

/// The checkpoint trigger a test node boots with (ADR-0088 D4). The
/// product derives its interval from the last checkpoint's size, which
/// makes any retained-log bound a function of the dataset *and* of how
/// fast the device completed the previous cycle — `truncation_bounds`
/// learned this on the NVMe (2026-08-22): a 21 MiB checkpoint derives a
/// 42 MiB trigger the test's 32 MiB trickle cannot reach, while tmpfs
/// passed on cycle ordering alone. Tests that assert a bound therefore
/// name the trigger they assert against instead of an integer.
#[derive(Clone, Copy)]
enum CkptTrigger {
    /// Automatic trigger off: e2e checkpoints fire via the control
    /// handle so tests own the timing.
    Manual,
    /// The product's derived trigger: `clamp(α × ckpt_bytes_last, floor,
    /// cap)` with the default α.
    Derived { floor_bytes: u64 },
    /// The pre-S36 fixed trigger (α = 0): every `interval_bytes` staged
    /// bytes, independent of checkpoint size and device speed.
    Fixed { interval_bytes: u64 },
}

impl CkptTrigger {
    fn config(self) -> inf_log::CkptConfig {
        let base = inf_log::CkptConfig::default();
        match self {
            CkptTrigger::Manual => inf_log::CkptConfig { interval_bytes: 0, ..base },
            CkptTrigger::Derived { floor_bytes } => {
                inf_log::CkptConfig { interval_bytes: floor_bytes, ..base }
            }
            CkptTrigger::Fixed { interval_bytes } => {
                inf_log::CkptConfig { interval_bytes, alpha: 0, ..base }
            }
        }
    }
}

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
        Node::start_with(cells, None, CkptTrigger::Manual)
    }

    /// A node with the durable plane enabled (M2-S08): catalog loaded and
    /// seeded before cells serve, per-cell log recovery, control thread as
    /// the catalog's single writer — the boot order infinityd adopts.
    /// Automatic checkpoints stay off (tests own the trigger).
    fn start_durable(cells: u16, data_dir: &std::path::Path) -> Node {
        Node::start_with(cells, Some(data_dir.to_path_buf()), CkptTrigger::Manual)
    }

    fn start_durable_with_default_ns(
        cells: u16,
        data_dir: &std::path::Path,
        default_ns: &[u8],
    ) -> Node {
        Node::start_cfg_default(
            cells,
            Some(data_dir.to_path_buf()),
            CkptTrigger::Manual,
            Default::default(),
            Vec::new(),
            false,
            false,
            false,
            None,
            inf_log::SegmentIoMode::Buffered,
            Some(default_ns.to_vec()),
            Default::default(),
        )
    }

    /// A durable node whose cells spend a **device budget** (ADR-0088
    /// D2): `model` is the per-device model the harness shares across
    /// `cells` — the product path when `io-properties.toml` is probed.
    fn start_durable_with_device_model(
        cells: u16,
        data_dir: &std::path::Path,
        model: inf_runtime::DeviceModel,
    ) -> Node {
        Node::start_cfg_default(
            cells,
            Some(data_dir.to_path_buf()),
            CkptTrigger::Manual,
            Default::default(),
            Vec::new(),
            false,
            false,
            false,
            None,
            inf_log::SegmentIoMode::Buffered,
            None,
            inf_server::DeviceConfig { model_share: model.share(cells), seal_barriers_per_s: 0 },
        )
    }

    /// Durable node with the bytes-appended checkpoint trigger armed
    /// (M2-S10, ADR-0016 D7) in its product form: the S36 derived
    /// interval above `floor_bytes`.
    fn start_durable_auto_ckpt(cells: u16, data_dir: &std::path::Path, floor_bytes: u64) -> Node {
        Node::start_with(cells, Some(data_dir.to_path_buf()), CkptTrigger::Derived { floor_bytes })
    }

    /// Durable node with a fixed `interval_bytes` trigger (α = 0) — for
    /// tests whose bound is stated in multiples of the interval.
    fn start_durable_fixed_ckpt(
        cells: u16,
        data_dir: &std::path::Path,
        interval_bytes: u64,
    ) -> Node {
        Node::start_with(cells, Some(data_dir.to_path_buf()), CkptTrigger::Fixed { interval_bytes })
    }

    fn start_with(cells: u16, data_dir: Option<std::path::PathBuf>, ckpt: CkptTrigger) -> Node {
        Node::start_full(cells, data_dir, ckpt, Default::default(), Vec::new())
    }

    /// Durable node with a boot-recovery pacing override (M2-S15): the
    /// throttled variant holds the node in its `-LOADING` window so tests
    /// can observe it.
    fn start_with_recover(
        cells: u16,
        data_dir: Option<std::path::PathBuf>,
        recover: inf_server::RecoverConfig,
    ) -> Node {
        Node::start_full(cells, data_dir, CkptTrigger::Manual, recover, Vec::new())
    }

    /// Durable node with named fault points armed on every cell thread at
    /// boot (M2-S16): the registry is thread-local, so the plan is applied
    /// by each cell before its recovery begins — deterministic per cell.
    fn start_durable_with_faults(
        cells: u16,
        data_dir: &std::path::Path,
        faults: Vec<(&'static str, inf_foundation::fault::FaultSpec)>,
    ) -> Node {
        Node::start_full(
            cells,
            Some(data_dir.to_path_buf()),
            CkptTrigger::Manual,
            Default::default(),
            faults,
        )
    }

    /// Node with the M2.5 Phase-H fabric-apply prefetch enabled (the A/B
    /// lever's on-arm correctness surface).
    fn start_with_apply_prefetch(cells: u16) -> Node {
        Node::start_cfg(
            cells,
            None,
            CkptTrigger::Manual,
            Default::default(),
            Vec::new(),
            true,
            false,
            false,
            None,
            inf_log::SegmentIoMode::Buffered,
        )
    }

    fn start_with_parse_prefetch(cells: u16) -> Node {
        Node::start_cfg(
            cells,
            None,
            CkptTrigger::Manual,
            Default::default(),
            Vec::new(),
            false,
            true,
            false,
            None,
            inf_log::SegmentIoMode::Buffered,
        )
    }

    /// Node with the M2.5 Phase-H de-async dispatch enabled (ADR-0030 D4
    /// lever): the pump's sync fast path on-arm correctness surface.
    fn start_with_deasync(cells: u16) -> Node {
        Node::start_cfg(
            cells,
            None,
            CkptTrigger::Manual,
            Default::default(),
            Vec::new(),
            false,
            false,
            true,
            None,
            inf_log::SegmentIoMode::Buffered,
        )
    }

    fn start_full(
        cells: u16,
        data_dir: Option<std::path::PathBuf>,
        ckpt: CkptTrigger,
        recover: inf_server::RecoverConfig,
        faults: Vec<(&'static str, inf_foundation::fault::FaultSpec)>,
    ) -> Node {
        Node::start_cfg(
            cells,
            data_dir,
            ckpt,
            recover,
            faults,
            false,
            false,
            false,
            None,
            inf_log::SegmentIoMode::Buffered,
        )
    }

    /// M4.5-S27: a durable node with a deliberately tiny staging domain —
    /// the pressure-regime injector (ADR-0083 D3): headroom below one
    /// pipelined burst makes admission pressure deterministic on any
    /// device, no degraded drive required.
    /// M4.5-S34 (ADR-0086): a durable node whose log segments are
    /// `Direct` — v3 frames, driver zero-fill, write-through `always`
    /// frames once pre-zeroed. Real `O_DIRECT` on the test directory.
    fn start_durable_direct(cells: u16, data_dir: &std::path::Path) -> Node {
        Node::start_cfg(
            cells,
            Some(data_dir.to_path_buf()),
            CkptTrigger::Manual,
            inf_server::RecoverConfig::default(),
            Vec::new(),
            true,
            true,
            false,
            None,
            inf_log::SegmentIoMode::Direct,
        )
    }

    /// A Direct node with `frames_in_flight = k` (M4.5-S35, ADR-0087).
    fn start_durable_pipeline(cells: u16, data_dir: &std::path::Path, k: u8) -> Node {
        Node::start_cfg(
            cells,
            Some(data_dir.to_path_buf()),
            CkptTrigger::Manual,
            inf_server::RecoverConfig::default(),
            Vec::new(),
            true,
            true,
            false,
            Some(inf_log::StagingConfig { frames_in_flight: k, ..Default::default() }),
            inf_log::SegmentIoMode::Direct,
        )
    }

    fn start_durable_small_staging(
        cells: u16,
        data_dir: &std::path::Path,
        staging_bytes: u32,
    ) -> Node {
        Node::start_cfg(
            cells,
            Some(data_dir.to_path_buf()),
            CkptTrigger::Manual,
            Default::default(),
            Vec::new(),
            false,
            false,
            false,
            Some(inf_log::StagingConfig::with_capacity(staging_bytes)),
            inf_log::SegmentIoMode::Buffered,
        )
    }

    #[allow(clippy::too_many_arguments)] // test harness funnel
    #[allow(clippy::too_many_arguments)] // test harness assembly, not an API surface
    fn start_cfg(
        cells: u16,
        data_dir: Option<std::path::PathBuf>,
        ckpt: CkptTrigger,
        recover: inf_server::RecoverConfig,
        faults: Vec<(&'static str, inf_foundation::fault::FaultSpec)>,
        apply_prefetch: bool,
        parse_prefetch: bool,
        deasync_dispatch: bool,
        staging: Option<inf_log::StagingConfig>,
        io_mode: inf_log::SegmentIoMode,
    ) -> Node {
        Node::start_cfg_default(
            cells,
            data_dir,
            ckpt,
            recover,
            faults,
            apply_prefetch,
            parse_prefetch,
            deasync_dispatch,
            staging,
            io_mode,
            None,
            Default::default(),
        )
    }

    #[allow(clippy::too_many_arguments)] // test harness assembly, not an API surface
    fn start_cfg_default(
        cells: u16,
        data_dir: Option<std::path::PathBuf>,
        ckpt: CkptTrigger,
        recover: inf_server::RecoverConfig,
        faults: Vec<(&'static str, inf_foundation::fault::FaultSpec)>,
        apply_prefetch: bool,
        parse_prefetch: bool,
        deasync_dispatch: bool,
        staging: Option<inf_log::StagingConfig>,
        io_mode: inf_log::SegmentIoMode,
        default_ns: Option<Vec<u8>>,
        device: inf_server::DeviceConfig,
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
            let boot_unix_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let control =
                inf_server::spawn_control(dir.clone(), catalog.as_ref(), cells, boot_unix_ms);
            (dir, catalog, control)
        });
        let mut handles = Vec::new();
        for (i, (fabric, listener)) in fabrics.into_iter().zip(listeners).enumerate() {
            let stop = Arc::clone(&stop);
            let boot = boot.clone();
            let faults = faults.clone();
            let default_ns = default_ns.clone();
            handles.push(std::thread::spawn(move || {
                // M2-S16: arm this cell's fault plan before recovery — the
                // registry is thread-local (cells are single-threaded, L1).
                for &(point, spec) in &faults {
                    inf_foundation::fault::arm(point, spec);
                }
                let mut pool = BufferPool::new(256, 4096);
                let mut driver = UringDriver::new(256).expect("uring");
                driver.register_pool(&mut pool).expect("register");
                let node = Rc::new(NodeInfo::default());
                *node.conn_default_ns.borrow_mut() = default_ns;
                // Real wall anchor (the infinityd boot pattern): LASTSAVE/
                // rdb_last_save_time report true unix seconds (M2-S20).
                let unix_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                node.wall_anchor.set((0, unix_ms));
                let mut ks = Keyspace::new(StoreConfig::default());
                let mut durable = None;
                if let Some((dir, catalog, control)) = &boot {
                    if let Some(catalog) = catalog {
                        ks.seed_catalog(catalog).expect("seed catalog");
                    }
                    let cfg = inf_server::DurableConfig {
                        data_dir: dir.clone(),
                        staging: staging.unwrap_or_default(),
                        segment: inf_log::SegmentConfig {
                            segment_bytes: 8 << 20, // small: tests rotate
                            io_mode,
                            ..Default::default()
                        },
                        ckpt: ckpt.config(),
                        recover,
                        flush_bound: 1,
                        fua_p50_us_probed: 0,
                        device,
                        fill: Default::default(),
                    };
                    durable = Some((cfg, Arc::clone(control)));
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
                plane.set_fabric_apply_prefetch(apply_prefetch);
                plane.set_parse_batch_prefetch(parse_prefetch);
                plane.set_deasync_dispatch(deasync_dispatch);
                if let Some((cfg, control)) = durable {
                    // Loop-resident recovery (M2-S15): the cell serves
                    // -LOADING while MAINTAIN replays its log.
                    plane.set_control(control);
                    plane.begin_recovery(
                        inf_server::StdSegmentFs,
                        &cfg,
                        i as u16,
                        StdClock::new().now(),
                    );
                }
                let config = LoopConfig {
                    park_default: Some(Duration::from_millis(5)),
                    ..Default::default()
                };
                let mut cell_loop = CellLoop::new(driver, StdClock::new(), pool, config);
                while !stop.load(Ordering::Relaxed) {
                    cell_loop.run_iteration(&mut plane).expect("iteration");
                    if let Some(err) = plane.take_boot_error() {
                        panic!("cell {i} recovery failed (fail-stop, §8.4): {err}");
                    }
                }
            }));
        }
        let control = boot.map(|(_, _, control)| control);
        let node = Node { port, stop, handles, control };
        // Most tests speak data commands immediately after start: wait out
        // the -LOADING window unless the test throttled recovery to
        // observe it (the throttle IS the -LOADING test's subject).
        if recover.throttle_bytes_per_sec.is_none()
            && let Some(control) = &node.control
        {
            let deadline = Instant::now() + Duration::from_secs(30);
            while !control.recovery_board().all_ready() {
                assert!(Instant::now() < deadline, "recovery did not finish in 30s");
                #[allow(clippy::disallowed_methods)] // test harness thread, not cell code
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        node
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
        // The control thread is detached (`spawn_control`) and outlives the
        // join above: delegated unlinks (truncated segments, stale `.ick`s —
        // ADR-0017) drain asynchronously. A restart on the same data dir
        // would race that leftover queue against the new node's boot GC —
        // both remove the same below-floor files, the loser hits ENOENT,
        // and recovery fail-stops (§8.4). A real node cannot express this
        // (one process; death takes the control thread with it, and boot GC
        // then owns the survivors alone), so the harness must quiesce: a
        // sentinel unlink queued *behind* any leftover work proves the
        // whole queue drained (single FIFO receiver), after which the old
        // thread can never touch the data dir again.
        if let Some(control) = &self.control {
            let sentinel = std::env::temp_dir().join(format!(
                "inf-e2e-drain-{}-{}",
                std::process::id(),
                self.port
            ));
            std::fs::write(&sentinel, b"drain").expect("write drain sentinel");
            while !control.request_unlink(sentinel.clone()) {
                #[allow(clippy::disallowed_methods)] // test harness thread, not cell code
                std::thread::sleep(Duration::from_millis(1));
            }
            let deadline = Instant::now() + Duration::from_secs(30);
            while sentinel.exists() {
                assert!(Instant::now() < deadline, "control thread did not drain in 30s");
                #[allow(clippy::disallowed_methods)] // test harness thread, not cell code
                std::thread::sleep(Duration::from_millis(1));
            }
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

/// The fabric-apply staged prefetch (M2.5 Phase H, `--fabric-apply-prefetch`)
/// must be behavior-invisible: same replies, same order, expiry-on-read
/// intact (a reap between the batch's prefetch pass and its execute pass is
/// the edge the unverified probe must not corrupt).
#[test]
fn cross_cell_with_apply_prefetch_matches_inline_semantics() {
    let node = Node::start_with_apply_prefetch(2);
    let mut client = node.connect();

    let k0 = key_for_cell(2, 0);
    let k1 = key_for_cell(2, 1);

    // The same pipelined mix the inline path pins: SET/GET/DEL/counted.
    let mut pipeline = Vec::new();
    pipeline.extend(cmd(&[b"SET", &k0, b"zero"]));
    pipeline.extend(cmd(&[b"SET", &k1, b"one"]));
    pipeline.extend(cmd(&[b"GET", &k0]));
    pipeline.extend(cmd(&[b"GET", &k1]));
    pipeline.extend(cmd(&[b"DEL", &k0, &k1, b"missing"]));
    pipeline.extend(cmd(&[b"GET", &k0]));
    client.write_all(&pipeline).expect("write");
    read_exactly(&mut client, b"+OK\r\n+OK\r\n$4\r\nzero\r\n$3\r\none\r\n:2\r\n$-1\r\n");

    // Reply order under a long interleaved remote/local pipeline (the
    // staged batch must preserve arrival order exactly).
    let mut pipeline = Vec::new();
    for _ in 0..50 {
        pipeline.extend(cmd(&[b"INCR", &k0]));
        pipeline.extend(cmd(&[b"INCR", &k1]));
    }
    client.write_all(&pipeline).expect("write");
    let mut want = Vec::new();
    for round in 1..=50 {
        want.extend_from_slice(format!(":{round}\r\n:{round}\r\n").as_bytes());
    }
    read_exactly(&mut client, &want);

    // Expiry-on-read through the batched path: a key expired between ops
    // still reads as gone (the probe prefetch never resurrects records).
    client.write_all(&cmd(&[b"SET", &k0, b"soon", b"PX", b"1"])).expect("write");
    read_exactly(&mut client, b"+OK\r\n");
    #[allow(clippy::disallowed_methods)] // test harness thread, not cell code
    std::thread::sleep(Duration::from_millis(10));
    client.write_all(&cmd(&[b"GET", &k0])).expect("write");
    read_exactly(&mut client, b"$-1\r\n");

    // Scatter + counted aggregation still route through the same drain.
    client.write_all(&cmd(&[b"EXISTS", &k1, &k1, b"nope"])).expect("write");
    read_exactly(&mut client, b":2\r\n");
    client.write_all(&cmd(&[b"DBSIZE"])).expect("write");
    read_exactly(&mut client, b":1\r\n");

    node.stop();
}

#[test]
fn parse_batch_prefetch_matches_inline_semantics() {
    // M2.5 Phase H (ADR-0029 lever 2): with the parse-batch stage on, every
    // reply byte must match the inline path — including across the stage's
    // flush barriers (SELECT/unknown-command/QUIT) and the bounds that force
    // inline execution (oversized values).
    let node = Node::start_with_parse_prefetch(1);
    let mut client = node.connect();

    // A long staged pipeline: order and values byte-exact.
    let mut pipeline = Vec::new();
    pipeline.extend(cmd(&[b"SET", b"a", b"1"]));
    pipeline.extend(cmd(&[b"SET", b"b", b"two"]));
    pipeline.extend(cmd(&[b"GET", b"a"]));
    pipeline.extend(cmd(&[b"INCR", b"a"]));
    pipeline.extend(cmd(&[b"GET", b"b"]));
    pipeline.extend(cmd(&[b"DEL", b"b"]));
    pipeline.extend(cmd(&[b"GET", b"b"]));
    pipeline.extend(cmd(&[b"PING"]));
    client.write_all(&pipeline).expect("write");
    read_exactly(
        &mut client,
        b"+OK\r\n+OK\r\n$1\r\n1\r\n:2\r\n$3\r\ntwo\r\n:1\r\n$-1\r\n+PONG\r\n",
    );

    // SELECT is a flush barrier and a live ConnCx mutation: staged commands
    // before it hit db 0, after it db 1 — in one pipelined buffer.
    let mut pipeline = Vec::new();
    pipeline.extend(cmd(&[b"SET", b"k", b"db0"]));
    pipeline.extend(cmd(&[b"SELECT", b"1"]));
    pipeline.extend(cmd(&[b"GET", b"k"]));
    pipeline.extend(cmd(&[b"SET", b"k", b"db1"]));
    pipeline.extend(cmd(&[b"GET", b"k"]));
    pipeline.extend(cmd(&[b"SELECT", b"0"]));
    pipeline.extend(cmd(&[b"GET", b"k"]));
    client.write_all(&pipeline).expect("write");
    read_exactly(&mut client, b"+OK\r\n+OK\r\n$-1\r\n+OK\r\n$3\r\ndb1\r\n+OK\r\n$3\r\ndb0\r\n");

    // An unknown command mid-batch is a barrier; its error keeps pipeline
    // position.
    let mut pipeline = Vec::new();
    pipeline.extend(cmd(&[b"SET", b"x", b"1"]));
    pipeline.extend(cmd(&[b"NOSUCHCMD", b"y"]));
    pipeline.extend(cmd(&[b"GET", b"x"]));
    client.write_all(&pipeline).expect("write");
    read_exactly(&mut client, b"+OK\r\n");
    let line = read_line(&mut client);
    assert!(
        line.starts_with(b"-ERR unknown command"),
        "unknown-command error in pipeline position: {}",
        String::from_utf8_lossy(&line)
    );
    read_exactly(&mut client, b"$1\r\n1\r\n");

    // A value past the stage byte bound executes inline (the bound is a
    // barrier, never a behavior change).
    let big = vec![b'v'; 4096];
    let mut pipeline = Vec::new();
    pipeline.extend(cmd(&[b"SET", b"big", &big]));
    pipeline.extend(cmd(&[b"STRLEN", b"big"]));
    pipeline.extend(cmd(&[b"GET", b"a"]));
    client.write_all(&pipeline).expect("write");
    read_exactly(&mut client, b"+OK\r\n:4096\r\n$1\r\n2\r\n");

    // Expiry-on-read through the staged path: the probe prefetch never
    // resurrects an expired record.
    client.write_all(&cmd(&[b"SET", b"soon", b"x", b"PX", b"1"])).expect("write");
    read_exactly(&mut client, b"+OK\r\n");
    #[allow(clippy::disallowed_methods)] // test harness thread, not cell code
    std::thread::sleep(Duration::from_millis(10));
    let mut pipeline = Vec::new();
    pipeline.extend(cmd(&[b"GET", b"soon"]));
    pipeline.extend(cmd(&[b"GET", b"a"]));
    client.write_all(&pipeline).expect("write");
    read_exactly(&mut client, b"$-1\r\n$1\r\n2\r\n");

    // QUIT mid-pipeline: staged commands before it answer, everything after
    // is discarded (Redis semantics), then the server closes.
    let mut pipeline = Vec::new();
    pipeline.extend(cmd(&[b"SET", b"q", b"1"]));
    pipeline.extend(cmd(&[b"QUIT"]));
    pipeline.extend(cmd(&[b"GET", b"q"]));
    client.write_all(&pipeline).expect("write");
    read_exactly(&mut client, b"+OK\r\n+OK\r\n");
    let mut rest = Vec::new();
    client.read_to_end(&mut rest).expect("server closes after QUIT");
    assert!(rest.is_empty(), "nothing after QUIT's +OK: {rest:?}");

    node.stop();
}

/// The de-async dispatch fast path (M2.5 Phase H, `--deasync-dispatch`,
/// ADR-0030 D4) must be behavior-invisible: same replies, same order —
/// across the fast arms (single-owner remote Apply, local mirror,
/// conn-state), the fallback arms interleaved with them (split DEL, MGET
/// gather, scatter DBSIZE), the restricted-subscriber reply, and
/// expiry-on-read over the remote path.
#[test]
fn deasync_dispatch_matches_pump_semantics() {
    let node = Node::start_with_deasync(2);
    let mut client = node.connect();

    let k0 = key_for_cell(2, 0);
    let k1 = key_for_cell(2, 1);

    // Fast arm + fallback split arm in one pipeline: whichever cell
    // accepted, one of k0/k1 rides the single-owner remote Apply.
    let mut pipeline = Vec::new();
    pipeline.extend(cmd(&[b"SET", &k0, b"zero"]));
    pipeline.extend(cmd(&[b"SET", &k1, b"one"]));
    pipeline.extend(cmd(&[b"GET", &k0]));
    pipeline.extend(cmd(&[b"GET", &k1]));
    pipeline.extend(cmd(&[b"DEL", &k0, &k1, b"missing"]));
    pipeline.extend(cmd(&[b"GET", &k0]));
    client.write_all(&pipeline).expect("write");
    read_exactly(&mut client, b"+OK\r\n+OK\r\n$4\r\nzero\r\n$3\r\none\r\n:2\r\n$-1\r\n");

    // Long remote/local interleave: order byte-exact for 100 replies.
    let mut pipeline = Vec::new();
    for _ in 0..50 {
        pipeline.extend(cmd(&[b"INCR", &k0]));
        pipeline.extend(cmd(&[b"INCR", &k1]));
    }
    client.write_all(&pipeline).expect("write");
    let mut want = Vec::new();
    for round in 1..=50 {
        want.extend_from_slice(format!(":{round}\r\n:{round}\r\n").as_bytes());
    }
    read_exactly(&mut client, &want);

    // SELECT mid-queue: the conn-state barrier holds its exact pipeline
    // position between remote ops (mirror arm on the fast path).
    let mut pipeline = Vec::new();
    pipeline.extend(cmd(&[b"SET", &k0, b"dbzero"]));
    pipeline.extend(cmd(&[b"SELECT", b"1"]));
    pipeline.extend(cmd(&[b"GET", &k0]));
    pipeline.extend(cmd(&[b"SET", &k0, b"dbone"]));
    pipeline.extend(cmd(&[b"SELECT", b"0"]));
    pipeline.extend(cmd(&[b"GET", &k0]));
    client.write_all(&pipeline).expect("write");
    read_exactly(&mut client, b"+OK\r\n+OK\r\n$-1\r\n+OK\r\n+OK\r\n$6\r\ndbzero\r\n");

    // MGET gather (fallback arm) interleaved with fast-arm writes.
    let mut pipeline = Vec::new();
    pipeline.extend(cmd(&[b"SET", &k1, b"vone"]));
    pipeline.extend(cmd(&[b"MGET", &k0, &k1, b"missing"]));
    pipeline.extend(cmd(&[b"GET", &k1]));
    client.write_all(&pipeline).expect("write");
    read_exactly(&mut client, b"+OK\r\n*3\r\n$6\r\ndbzero\r\n$4\r\nvone\r\n$-1\r\n$4\r\nvone\r\n");

    // Scatter DBSIZE (fallback arm): both cells' counts aggregate.
    client.write_all(&cmd(&[b"DBSIZE"])).expect("write");
    read_exactly(&mut client, b":2\r\n");

    // Expiry-on-read over the remote path: an expired key reads as gone
    // through the fast arm.
    client.write_all(&cmd(&[b"SET", &k1, b"soon", b"PX", b"1"])).expect("write");
    read_exactly(&mut client, b"+OK\r\n");
    #[allow(clippy::disallowed_methods)] // test harness thread, not cell code
    std::thread::sleep(Duration::from_millis(10));
    let mut pipeline = Vec::new();
    pipeline.extend(cmd(&[b"GET", &k1]));
    pipeline.extend(cmd(&[b"GET", &k0]));
    client.write_all(&pipeline).expect("write");
    read_exactly(&mut client, b"$-1\r\n$6\r\ndbzero\r\n");

    // Restricted subscriber: data commands answer the restriction error
    // through the pump for both local and remote keys.
    let mut sub = node.connect();
    sub.write_all(&cmd(&[b"SUBSCRIBE", b"ch"])).expect("write");
    read_exactly(&mut sub, b"*3\r\n$9\r\nsubscribe\r\n$2\r\nch\r\n:1\r\n");
    sub.write_all(&cmd(&[b"GET", &k0])).expect("write");
    let line = read_line(&mut sub);
    assert!(line.starts_with(b"-ERR"), "restricted error for local-ish key: {line:?}");
    sub.write_all(&cmd(&[b"GET", &k1])).expect("write");
    let line = read_line(&mut sub);
    assert!(line.starts_with(b"-ERR"), "restricted error for remote-ish key: {line:?}");

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

/// `--conn-default-ns` is an operator requirement, not a best-effort hint:
/// an unresolved name must never route a command to db0. Namespace DDL and
/// explicit selection remain available, and an `always` ack written after
/// recovery from the fail-closed state survives a full node restart.
#[test]
fn configured_default_namespace_fails_closed_and_durable_ack_survives_restart() {
    let dir = temp_data_dir("conn-default-ns");
    let node = Node::start_durable_with_default_ns(2, &dir, b"ledger");
    let mut c = node.connect();
    let unavailable =
        b"-ERR configured default namespace is unavailable; use SELECT or INF.NS USE\r\n";

    c.write_all(&cmd(&[b"PING"])).expect("write");
    read_exactly(&mut c, b"+PONG\r\n");
    c.write_all(&cmd(&[b"SET", b"must-not-leak", b"db0"])).expect("write");
    read_exactly(&mut c, unavailable);
    c.write_all(&cmd(&[b"GET", b"must-not-leak"])).expect("write");
    read_exactly(&mut c, unavailable);
    let k0 = key_for_cell(2, 0);
    let k1 = key_for_cell(2, 1);
    for key in [&k0, &k1] {
        c.write_all(&cmd(&[b"SET", key, b"routed-must-not-leak"])).expect("write");
        read_exactly(&mut c, unavailable);
    }

    // DDL is a recovery command. The existing connection stays fail-closed
    // until it explicitly selects the namespace; new accepts resolve it.
    c.write_all(&cmd(&[b"INF.NS", b"CREATE", b"ledger", b"MODE", b"durable", b"FSYNC", b"always"]))
        .expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    c.write_all(&cmd(&[b"SET", b"still-closed", b"x"])).expect("write");
    read_exactly(&mut c, unavailable);
    c.write_all(&cmd(&[b"INF.NS", b"USE", b"ledger"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    c.write_all(&cmd(&[b"SET", b"durable-key", b"survives"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n"); // `always`: the ack is the durability fence.

    let mut auto = node.connect();
    auto.write_all(&cmd(&[b"GET", b"durable-key"])).expect("write");
    read_exactly(&mut auto, b"$8\r\nsurvives\r\n");
    drop(auto);
    drop(c);
    node.stop();

    let node = Node::start_durable_with_default_ns(2, &dir, b"ledger");
    let mut c = node.connect();
    c.write_all(&cmd(&[b"GET", b"durable-key"])).expect("write");
    read_exactly(&mut c, b"$8\r\nsurvives\r\n");
    c.write_all(&cmd(&[b"SELECT", b"0"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    c.write_all(&cmd(&[b"GET", b"must-not-leak"])).expect("write");
    read_exactly(&mut c, b"$-1\r\n");
    for key in [&k0, &k1] {
        c.write_all(&cmd(&[b"GET", key])).expect("write");
        read_exactly(&mut c, b"$-1\r\n");
    }
    drop(c);
    node.stop();
    std::fs::remove_dir_all(&dir).ok();
}

// ---- M4.5-S27: durable admission paces instead of refusing (ADR-0083) ----

/// `n` distinct keys owned by `cell` under the N-cell contiguous router.
fn keys_for_cell(cells: u16, cell: u16, n: usize) -> Vec<Vec<u8>> {
    let router = SlotRouter::new_contiguous(cells);
    let mut keys = Vec::with_capacity(n);
    for i in 0..1_000_000u32 {
        if keys.len() == n {
            break;
        }
        let key = format!("k:{i}");
        if router.cell_of(SlotRouter::slot_of(key.as_bytes())) == CellId(cell) {
            keys.push(key.into_bytes());
        }
    }
    assert_eq!(keys.len(), n, "not enough keys routed to cell {cell}");
    keys
}

/// Connects and selects `ns`, retrying fresh connections until the DDL
/// fan has reached the landed cell (REUSEPORT spreads connections, and a
/// peer cell may not have applied the CREATE yet — the S29 bench trap).
fn connect_use(node: &Node, ns: &[u8]) -> TcpStream {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let mut c = node.connect();
        c.write_all(&cmd(&[b"INF.NS", b"USE", ns])).expect("write");
        let line = read_line(&mut c);
        if line == b"+OK\r\n" {
            return c;
        }
        assert!(
            Instant::now() < deadline,
            "USE never fanned to all cells: {:?}",
            String::from_utf8_lossy(&line)
        );
    }
}

/// One `INFO persistence` round-trip (bulk-string reply → text).
fn info_persistence(c: &mut TcpStream) -> String {
    c.write_all(&cmd(&[b"INFO", b"persistence"])).expect("write");
    let header = read_line(c);
    assert!(header.starts_with(b"$"), "bulk header: {header:?}");
    let len: usize = String::from_utf8_lossy(&header[1..header.len() - 2]).parse().expect("len");
    let mut body = vec![0u8; len + 2];
    c.read_exact(&mut body).expect("info body");
    String::from_utf8_lossy(&body[..len]).into_owned()
}

fn info_field(info: &str, field: &str) -> u64 {
    info.lines()
        .find_map(|l| l.strip_prefix(&format!("{field}:")))
        .unwrap_or_else(|| panic!("{field} missing from INFO:\n{info}"))
        .trim()
        .parse()
        .unwrap_or_else(|_| panic!("{field} not numeric"))
}

/// M4.5-S27 (ADR-0083 D1): under staging pressure every durable write —
/// local *and* fabric-routed — parks and succeeds; none refuses with
/// `-BUSY`. Pre-fix, the owner-side fabric admission answered `-BUSY`
/// while only local writes parked, so this test pins the regression: a
/// 64 KiB staging domain against pipelined 8 KiB values over keys owned
/// by both cells makes the pressure regime deterministic on any device.
#[test]
fn durable_pressure_parks_fabric_writes_instead_of_busy() {
    let dir = temp_data_dir("s27-park");
    let node = Node::start_durable_small_staging(2, &dir, 64 * 1024);
    let mut c = node.connect();
    c.write_all(&cmd(&[
        b"INF.NS",
        b"CREATE",
        b"press",
        b"MODE",
        b"durable",
        b"FSYNC",
        b"everysec",
    ]))
    .expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    drop(c);

    let value = vec![b'v'; 8 * 1024];
    let keys0 = keys_for_cell(2, 0, 50);
    let keys1 = keys_for_cell(2, 1, 50);
    let mut conns: Vec<TcpStream> = (0..3).map(|_| connect_use(&node, b"press")).collect();
    for (ci, c) in conns.iter_mut().enumerate() {
        let mut pipeline = Vec::new();
        for (k0, k1) in keys0.iter().zip(&keys1) {
            let mut k0 = k0.clone();
            let mut k1 = k1.clone();
            k0.extend_from_slice(format!(":{ci}").as_bytes());
            k1.extend_from_slice(format!(":{ci}").as_bytes());
            pipeline.extend(cmd(&[b"SET", &k0, &value]));
            pipeline.extend(cmd(&[b"SET", &k1, &value]));
        }
        c.write_all(&pipeline).expect("write burst");
    }
    // Every reply is +OK: fabric-routed writes parked (paced) instead of
    // bouncing with the typed -BUSY refusal.
    for c in &mut conns {
        for _ in 0..100 {
            read_exactly(c, b"+OK\r\n");
        }
    }
    // The pressure regime actually engaged (this is not a trivially-idle
    // pass), and no client-visible refusal was counted anywhere: sample
    // both cells via fresh REUSEPORT connections.
    let mut parked_total = 0u64;
    for _ in 0..8 {
        let mut c = node.connect();
        let info = info_persistence(&mut c);
        assert_eq!(info_field(&info, "log_admission_busy"), 0, "no -BUSY was issued:\n{info}");
        parked_total += info_field(&info, "log_admission_parked_total");
    }
    assert!(parked_total > 0, "staging pressure engaged at least once across cells");
    node.stop();
    std::fs::remove_dir_all(&dir).ok();
}

/// M4.5-S27 ordering: a GET pipelined behind a parked SET on the same
/// connection must observe that SET (read-your-write through the pump
/// FIFO) — under pressure, reads divert to the same per-origin queue so
/// nothing overtakes a parked write.
#[test]
fn durable_pressure_preserves_read_your_write_order() {
    let dir = temp_data_dir("s27-order");
    let node = Node::start_durable_small_staging(2, &dir, 64 * 1024);
    let mut c = node.connect();
    c.write_all(&cmd(&[b"INF.NS", b"CREATE", b"ordr", b"MODE", b"durable", b"FSYNC", b"everysec"]))
        .expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    drop(c);

    let keys0 = keys_for_cell(2, 0, 25);
    let keys1 = keys_for_cell(2, 1, 25);
    let mut c = connect_use(&node, b"ordr");
    // Interleaved SET/GET bursts, values large enough that the SETs park:
    // every GET must return the value its immediately-preceding SET wrote.
    let mut pipeline = Vec::new();
    let mut expected: Vec<Vec<u8>> = Vec::new();
    for (i, key) in keys0.iter().chain(&keys1).enumerate() {
        let value = vec![b'a' + (i % 26) as u8; 8 * 1024];
        pipeline.extend(cmd(&[b"SET", key, &value]));
        pipeline.extend(cmd(&[b"GET", key]));
        expected.push(value);
    }
    c.write_all(&pipeline).expect("write burst");
    for value in &expected {
        read_exactly(&mut c, b"+OK\r\n");
        read_exactly(&mut c, format!("${}\r\n", value.len()).as_bytes());
        let mut body = vec![0u8; value.len() + 2];
        c.read_exact(&mut body).expect("bulk body");
        assert_eq!(&body[..value.len()], value.as_slice(), "GET observed its preceding SET");
    }
    node.stop();
    std::fs::remove_dir_all(&dir).ok();
}

/// M4.5-S27 (ADR-0083 D2): a write whose record can never fit any drain
/// refuses up front with a typed ERR — never `-BUSY`, never a parked
/// livelock — and the connection (and node) stay serviceable after it.
#[test]
fn oversized_durable_write_refuses_typed_and_never_livelocks() {
    let dir = temp_data_dir("s27-oversized");
    let node = Node::start_durable_small_staging(2, &dir, 64 * 1024);
    let mut c = node.connect();
    c.write_all(&cmd(&[
        b"INF.NS",
        b"CREATE",
        b"tight",
        b"MODE",
        b"durable",
        b"FSYNC",
        b"everysec",
    ]))
    .expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    drop(c);

    let huge = vec![b'x'; 100 * 1024]; // > the 64 KiB staging domain
    for cell in 0..2u16 {
        let key = key_for_cell(2, cell);
        let mut c = connect_use(&node, b"tight");
        c.write_all(&cmd(&[b"SET", &key, &huge])).expect("write");
        let line = read_line(&mut c);
        assert!(
            line.starts_with(b"-ERR write exceeds durable log staging capacity"),
            "typed never-fits refusal (got {:?})",
            String::from_utf8_lossy(&line)
        );
        // The refusal is per-write, not a wedge: a normal write succeeds.
        c.write_all(&cmd(&[b"SET", &key, b"small"])).expect("write");
        read_exactly(&mut c, b"+OK\r\n");
    }
    node.stop();
    std::fs::remove_dir_all(&dir).ok();
}

/// M4.5-S27 × ADR-0082: `FSYNC always` under staging pressure — parked
/// fabric writes produce *gated* verdicts through the pump, and every
/// ack still arrives (after fsync) with zero refusals.
#[test]
fn durable_pressure_always_acks_gate_through_the_pump() {
    let dir = temp_data_dir("s27-always");
    let node = Node::start_durable_small_staging(2, &dir, 64 * 1024);
    let mut c = node.connect();
    c.write_all(&cmd(&[b"INF.NS", b"CREATE", b"led27", b"MODE", b"durable", b"FSYNC", b"always"]))
        .expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    drop(c);

    let value = vec![b'w'; 8 * 1024];
    let keys0 = keys_for_cell(2, 0, 25);
    let keys1 = keys_for_cell(2, 1, 25);
    let mut c = connect_use(&node, b"led27");
    let mut pipeline = Vec::new();
    for key in keys0.iter().chain(&keys1) {
        pipeline.extend(cmd(&[b"SET", key, &value]));
    }
    c.write_all(&pipeline).expect("write burst");
    for _ in 0..50 {
        read_exactly(&mut c, b"+OK\r\n"); // fsync-gated ack, never -BUSY
    }
    node.stop();
    std::fs::remove_dir_all(&dir).ok();
}

/// M4-S19 (ADR-0062): the tiered-namespace lifecycle over TCP — CREATE
/// with budget keys rides the DDL program (9-arg NSFAN, AllOk fan,
/// catalog persist-then-ack) and materializes per-cell tables under the
/// D4 admission bound; `INFO tiering` reports them; `SET` hot-reloads a
/// Hot key; the catalog **v2** tier block survives restart; `USE`
/// refuses typed (D8 — the data plane is not wired); `DROP` tears down
/// to the §3.3 zero contract.
#[test]
fn tiered_namespace_lifecycle_survives_restart() {
    let dir = temp_data_dir("tiered-lifecycle");
    // Two cells so the NSFAN peer fan is real, not a no-op.
    let node = Node::start_durable(2, &dir);
    let mut c = node.connect();
    c.write_all(&cmd(&[
        b"INF.NS",
        b"CREATE",
        b"hot",
        b"MODE",
        b"durable",
        b"MEM-BUDGET",
        b"8mb",
        b"DISK-BUDGET",
        b"64mb",
        b"MUTABLE-FRACTION",
        b"100",
    ]))
    .expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    // This connection's cell materialized its table; the +OK above is
    // the AllOk proof the peer did too (a peer failure surfaces as the
    // first error leg).
    let tiering = info_text(&mut c, b"tiering");
    assert!(tiering.contains("tiering_tables:1"), "{tiering}");
    assert!(tiering.contains("budget_bytes=8388608"), "{tiering}");
    assert!(tiering.contains("disk_budget_bytes=67108864"), "{tiering}");
    // M4-S26: the D8 refusal is lifted — USE routes to the tiered arm.
    c.write_all(&cmd(&[b"INF.NS", b"USE", b"hot"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    c.write_all(&cmd(&[b"SET", b"probe", b"v1"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    c.write_all(&cmd(&[b"GET", b"probe"])).expect("write");
    read_exactly(&mut c, b"$2\r\nv1\r\n");
    c.write_all(&cmd(&[b"SELECT", b"0"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    // Hot-reload rides the same DDL program (fan + persist-then-ack).
    c.write_all(&cmd(&[b"INF.NS", b"SET", b"hot", b"MUTABLE-FRACTION", b"300"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    // CreateOnly keys refuse.
    c.write_all(&cmd(&[b"INF.NS", b"SET", b"hot", b"TIER-IO-MODE", b"buffered"])).expect("write");
    let refusal = read_line(&mut c);
    assert!(refusal.starts_with(b"-ERR TIER-IO-MODE is create-only"), "{refusal:?}");
    drop(c);
    node.stop();

    // Restart: the catalog v2 tier block re-seeds and re-materializes,
    // reloaded value included.
    let node = Node::start_durable(2, &dir);
    let mut c = node.connect();
    let tiering = info_text(&mut c, b"tiering");
    assert!(tiering.contains("tiering_tables:1"), "re-materialized at boot: {tiering}");
    assert!(tiering.contains("mutable_permille=300"), "the reload persisted: {tiering}");
    c.write_all(&cmd(&[b"INF.NS", b"INFO", b"hot"])).expect("write");
    let mut buf = vec![0u8; 1024];
    let n = c.read(&mut buf).expect("read info");
    let info = String::from_utf8_lossy(&buf[..n]).into_owned();
    assert!(info.contains("mem-budget"), "{info}");
    assert!(info.contains("8388608"), "{info}");
    // Teardown: DROP removes the tables on every cell; the zero
    // contract holds again.
    c.write_all(&cmd(&[b"INF.NS", b"DROP", b"hot"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    let tiering = info_text(&mut c, b"tiering");
    assert!(tiering.contains("tiering_tables:0"), "{tiering}");
    assert!(tiering.contains("tiering_reserved_bytes:0"), "the ring returned: {tiering}");
    drop(c);
    node.stop();
    std::fs::remove_dir_all(&dir).ok();
}

/// M4-S27 (ADR-0068): the named memory-namespace pressure lifecycle over
/// TCP — CREATE and the `MAXMEMORY`/`EVICTION` hot-reload ride the DDL
/// program (the `MEMCFG` fan leg, AllOk, catalog persist-then-ack); the
/// DENYOOM verdict is scoped to the namespace (its budget refuses while
/// db0 keeps writing — the D4 gate on the wire); removing the budget
/// disarms the gate; the knobs survive restart as enforcement.
#[test]
fn named_memory_ns_pressure_enforced_and_survives_restart() {
    let dir = temp_data_dir("memns-pressure");
    // Two cells so the MEMCFG peer fan is real, not a no-op.
    let node = Node::start_durable(2, &dir);
    let mut c = node.connect();
    c.write_all(&cmd(&[b"INF.NS", b"CREATE", b"cache", b"EVICTION", b"allkeys-random"]))
        .expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    // Hot-reload: a 1-byte budget with the policy returned to inherit
    // (the node default `noeviction`) — the namespace becomes unfreeable
    // at its own budget after one write.
    c.write_all(&cmd(&[b"INF.NS", b"SET", b"cache", b"MAXMEMORY", b"1", b"EVICTION", b"inherit"]))
        .expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    c.write_all(&cmd(&[b"INF.NS", b"USE", b"cache"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    c.write_all(&cmd(&[b"SET", b"k", b"v1"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    c.write_all(&cmd(&[b"SET", b"k", b"v2"])).expect("write");
    read_exactly(&mut c, b"-OOM command not allowed when used memory > 'maxmemory'.\r\n");
    // The scope is the namespace: reads inside it and writes to db0 land.
    c.write_all(&cmd(&[b"GET", b"k"])).expect("write");
    read_exactly(&mut c, b"$2\r\nv1\r\n");
    c.write_all(&cmd(&[b"SELECT", b"0"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    c.write_all(&cmd(&[b"SET", b"free", b"v"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    // A real budget with an explicit policy re-arms serving; the values
    // are what restart must preserve.
    c.write_all(&cmd(&[
        b"INF.NS",
        b"SET",
        b"cache",
        b"MAXMEMORY",
        b"1gb",
        b"EVICTION",
        b"allkeys-lfu",
    ]))
    .expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    c.write_all(&cmd(&[b"INF.NS", b"USE", b"cache"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    c.write_all(&cmd(&[b"SET", b"k", b"v3"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    // Refusal pins on the wire: tier keys on a memory namespace, and the
    // pressure keys on a durable namespace.
    c.write_all(&cmd(&[b"INF.NS", b"SET", b"cache", b"MEM-BUDGET", b"8mb"])).expect("write");
    let refusal = read_line(&mut c);
    assert!(refusal.starts_with(b"-ERR not a tiered namespace"), "{refusal:?}");
    c.write_all(&cmd(&[b"INF.NS", b"CREATE", b"ledger", b"MODE", b"durable"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    c.write_all(&cmd(&[b"INF.NS", b"SET", b"ledger", b"MAXMEMORY", b"1mb"])).expect("write");
    let refusal = read_line(&mut c);
    assert!(refusal.starts_with(b"-ERR durable namespaces do not evict"), "{refusal:?}");
    drop(c);
    node.stop();

    // Restart: the catalog reseeds the knobs as enforcement, not display.
    let node = Node::start_durable(2, &dir);
    let mut c = node.connect();
    c.write_all(&cmd(&[b"INF.NS", b"INFO", b"cache"])).expect("write");
    let mut buf = vec![0u8; 1024];
    let n = c.read(&mut buf).expect("read info");
    let info = String::from_utf8_lossy(&buf[..n]).into_owned();
    assert!(info.contains("allkeys-lfu"), "policy survived restart: {info}");
    assert!(info.contains("1073741824"), "budget survived restart: {info}");
    drop(c);
    node.stop();
    std::fs::remove_dir_all(&dir).ok();
}

/// M4-S26: the tiered data plane end to end, then the §3.1 never-none
/// proof at node scale. String commands serve a tiered namespace over
/// TCP; a fill past `MEM-BUDGET` demotes (seal → flush → release), so
/// low keys go cold and later reads take the `IoToken` suspension path;
/// overwrites and deletes of cold keys stage displacement markers; a
/// checkpoint walks the hybrid (refs + images + live-set); a restart
/// recovers content-exactly through MANIFEST v2 → tier files →
/// checkpoint → WAL tail (content compared, never addresses).
#[test]
fn tiered_data_plane_serves_and_survives_restart() {
    let dir = temp_data_dir("tiered-data");
    let keys = 600usize;
    let value_of = |i: usize, generation: u32| {
        format!("g{generation}:{i:04}:").into_bytes().repeat(1024) // ~8 KiB
    };
    let key_of = |i: usize| format!("k:{i:04}").into_bytes();
    {
        let node = Node::start_durable(1, &dir);
        let mut c = node.connect();
        c.write_all(&cmd(&[
            b"INF.NS",
            b"CREATE",
            b"t",
            b"MODE",
            b"durable",
            b"FSYNC",
            b"everysec",
            b"MEM-BUDGET",
            b"3mb",
        ]))
        .expect("write");
        read_exactly(&mut c, b"+OK\r\n");
        c.write_all(&cmd(&[b"INF.NS", b"USE", b"t"])).expect("write");
        read_exactly(&mut c, b"+OK\r\n");
        // Fill ~4.8 MiB against a 3 MiB budget: demotion must engage.
        for i in 0..keys {
            c.write_all(&cmd(&[b"SET", &key_of(i), &value_of(i, 0)])).expect("write");
            read_exactly(&mut c, b"+OK\r\n");
        }
        c.write_all(&cmd(&[b"DBSIZE"])).expect("write");
        read_exactly(&mut c, format!(":{keys}\r\n").as_bytes());
        // Demotion progress is MAINTAIN-driven: poll until bytes flushed
        // and the head advanced (cold records exist).
        let mut demoted = false;
        for _ in 0..300 {
            let tiering = info_text(&mut c, b"tiering");
            let flushed = tiering
                .lines()
                .find_map(|l| l.strip_prefix("tiering_flush_confirmed_bytes:"))
                .and_then(|v| v.trim().parse::<u64>().ok())
                .unwrap_or(0);
            if flushed > 1 << 20 {
                demoted = true;
                break;
            }
            #[allow(clippy::disallowed_methods)] // test harness thread, not cell code
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(demoted, "demotion never flushed past 1 MiB");
        // Cold reads serve exact bytes (fetch + verify + suspension).
        for i in [0usize, 3, 7] {
            c.write_all(&cmd(&[b"GET", &key_of(i)])).expect("write");
            let value = value_of(i, 0);
            let mut expect = format!("${}\r\n", value.len()).into_bytes();
            expect.extend_from_slice(&value);
            expect.extend_from_slice(b"\r\n");
            read_exactly(&mut c, &expect);
        }
        // Overwrites displace (cold candidates fetch-verify first); the
        // markers stage ahead of the mutation records (ADR-0057 D4).
        for i in 0..40 {
            c.write_all(&cmd(&[b"SET", &key_of(i), &value_of(i, 1)])).expect("write");
            read_exactly(&mut c, b"+OK\r\n");
        }
        // Cold DEL verifies, then kills by exact address (S26 policy).
        for i in 40..60 {
            c.write_all(&cmd(&[b"DEL", &key_of(i)])).expect("write");
            read_exactly(&mut c, b":1\r\n");
        }
        // A checkpoint walks the hybrid: refs below the walk watermark,
        // RAM images above, live-set sections, manifest v2 tier ranges.
        c.write_all(&cmd(&[b"INF.CKPT", b"WAIT"])).expect("write");
        read_exactly(&mut c, b"+OK\r\n");
        // Post-checkpoint tail: these records replay from the WAL.
        for i in 60..80 {
            c.write_all(&cmd(&[b"SET", &key_of(i), &value_of(i, 2)])).expect("write");
            read_exactly(&mut c, b"+OK\r\n");
        }
        drop(c);
        node.stop();
    }

    // Restart: MANIFEST v2 → tier files → checkpoint (refs idempotent,
    // displacements exact) → WAL tail. Every surviving key serves its
    // exact bytes; deleted keys stay dead (no ref-slot resurrection).
    let node = Node::start_durable(1, &dir);
    let mut c = node.connect();
    c.write_all(&cmd(&[b"INF.NS", b"USE", b"t"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    c.write_all(&cmd(&[b"DBSIZE"])).expect("write");
    read_exactly(&mut c, format!(":{}\r\n", keys - 20).as_bytes());
    for i in 0..keys {
        c.write_all(&cmd(&[b"GET", &key_of(i)])).expect("write");
        if (40..60).contains(&i) {
            read_exactly(&mut c, b"$-1\r\n");
            continue;
        }
        let generation = match i {
            _ if i < 40 => 1,
            _ if (60..80).contains(&i) => 2,
            _ => 0,
        };
        let value = value_of(i, generation);
        let mut expect = format!("${}\r\n", value.len()).into_bytes();
        expect.extend_from_slice(&value);
        expect.extend_from_slice(b"\r\n");
        read_exactly(&mut c, &expect);
    }
    drop(c);
    node.stop();
    std::fs::remove_dir_all(&dir).ok();
}

/// M4-S17 wired by M4-S26: blob-resident values round-trip over TCP. A
/// `SET` at or above `BLOB-THRESHOLD` stores out of line (extent file +
/// 24-byte reference; the extent's fdatasync rides the ADR-0061 D3
/// ledger barrier, so the ack is fenced behind extent durability), `GET`
/// fetches it back byte-exact through chunked extent reads, an
/// overwrite displaces the old extent, `DEL` kills a reference, and a
/// restart recovers the surviving blob content-exactly (tag-9 records +
/// the checkpoint 0x05 reference map).
#[test]
fn blob_values_round_trip_and_survive_restart() {
    let dir = temp_data_dir("tiered-blob");
    let blob_of = |tag: u8| vec![tag; 8 << 10]; // 8 KiB ≥ the 4 KiB threshold
    let bulk_of = |value: &[u8]| {
        let mut expect = format!("${}\r\n", value.len()).into_bytes();
        expect.extend_from_slice(value);
        expect.extend_from_slice(b"\r\n");
        expect
    };
    {
        let node = Node::start_durable(1, &dir);
        let mut c = node.connect();
        c.write_all(&cmd(&[
            b"INF.NS",
            b"CREATE",
            b"b",
            b"MODE",
            b"durable",
            b"FSYNC",
            b"everysec",
            b"MEM-BUDGET",
            b"3mb",
            b"BLOB-THRESHOLD",
            b"4kb",
        ]))
        .expect("write");
        read_exactly(&mut c, b"+OK\r\n");
        c.write_all(&cmd(&[b"INF.NS", b"USE", b"b"])).expect("write");
        read_exactly(&mut c, b"+OK\r\n");
        c.write_all(&cmd(&[b"SET", b"big:1", &blob_of(0xA1)])).expect("write");
        read_exactly(&mut c, b"+OK\r\n");
        c.write_all(&cmd(&[b"GET", b"big:1"])).expect("write");
        read_exactly(&mut c, &bulk_of(&blob_of(0xA1)));
        // Overwrite: the old extent's reference dies (reclaimed by
        // MAINTAIN once the death is fsync-durable), the new one serves.
        c.write_all(&cmd(&[b"SET", b"big:1", &blob_of(0xB2)])).expect("write");
        read_exactly(&mut c, b"+OK\r\n");
        c.write_all(&cmd(&[b"GET", b"big:1"])).expect("write");
        read_exactly(&mut c, &bulk_of(&blob_of(0xB2)));
        c.write_all(&cmd(&[b"STRLEN", b"big:1"])).expect("write");
        read_exactly(&mut c, format!(":{}\r\n", 8 << 10).as_bytes());
        c.write_all(&cmd(&[b"SET", b"big:2", &blob_of(0xC3)])).expect("write");
        read_exactly(&mut c, b"+OK\r\n");
        c.write_all(&cmd(&[b"DEL", b"big:2"])).expect("write");
        read_exactly(&mut c, b":1\r\n");
        c.write_all(&cmd(&[b"GET", b"big:2"])).expect("write");
        read_exactly(&mut c, b"$-1\r\n");
        // Checkpoint: tag-9 images + the 0x05 extent map.
        c.write_all(&cmd(&[b"INF.CKPT", b"WAIT"])).expect("write");
        read_exactly(&mut c, b"+OK\r\n");
        drop(c);
        node.stop();
    }
    let node = Node::start_durable(1, &dir);
    let mut c = node.connect();
    c.write_all(&cmd(&[b"INF.NS", b"USE", b"b"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    c.write_all(&cmd(&[b"GET", b"big:1"])).expect("write");
    read_exactly(&mut c, &bulk_of(&blob_of(0xB2)));
    c.write_all(&cmd(&[b"GET", b"big:2"])).expect("write");
    read_exactly(&mut c, b"$-1\r\n");
    drop(c);
    node.stop();
    std::fs::remove_dir_all(&dir).ok();
}

/// ADR-0063 over the wire (the shape `m4-diskfull` proves at store/DST
/// tier, replayed through real commands): a `DISK-BUDGET`-bounded
/// namespace fills to admission refusal — the typed `DISKFULL` reply
/// whose byte shape was pinned by test before wiring — while reads and
/// deletes (no new tier bytes) keep proceeding.
#[test]
fn diskfull_admission_refuses_typed_over_tcp() {
    let dir = temp_data_dir("tiered-diskfull");
    let node = Node::start_durable(1, &dir);
    let mut c = node.connect();
    c.write_all(&cmd(&[
        b"INF.NS",
        b"CREATE",
        b"d",
        b"MODE",
        b"durable",
        b"MEM-BUDGET",
        b"3mb",
        b"DISK-BUDGET",
        b"1mb",
    ]))
    .expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    c.write_all(&cmd(&[b"INF.NS", b"USE", b"d"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    // The admission projection counts unflushed RAM bytes (ADR-0063
    // D2), so a 1 MiB disk budget closes within ~1 MiB of writes.
    let value = vec![0xD5u8; 8 << 10];
    let mut refusal: Option<Vec<u8>> = None;
    let mut accepted = 0usize;
    for i in 0..400 {
        c.write_all(&cmd(&[b"SET", format!("k:{i:04}").as_bytes(), &value])).expect("write");
        let reply = read_line(&mut c);
        if reply.starts_with(b"-DISKFULL") {
            refusal = Some(reply);
            break;
        }
        assert_eq!(reply, b"+OK\r\n", "unexpected fill reply");
        accepted += 1;
    }
    let refusal = refusal.expect("the disk budget never refused");
    assert!(
        refusal.starts_with(b"-DISKFULL tiered namespace disk budget exhausted (used="),
        "{refusal:?}"
    );
    assert!(accepted > 0, "some writes must land before the budget closes");
    // Refusal scope is exactly the new-byte placements (ADR-0063 D1):
    // reads and deletes proceed at the cap.
    c.write_all(&cmd(&[b"GET", b"k:0000"])).expect("write");
    let mut expect = format!("${}\r\n", value.len()).into_bytes();
    expect.extend_from_slice(&value);
    expect.extend_from_slice(b"\r\n");
    read_exactly(&mut c, &expect);
    c.write_all(&cmd(&[b"DEL", b"k:0000"])).expect("write");
    read_exactly(&mut c, b":1\r\n");
    drop(c);
    node.stop();
    std::fs::remove_dir_all(&dir).ok();
}

/// The S19 drop-race shape through the wire (§3.3): `INF.NS DROP` races
/// pipelined cold reads. Every in-flight read answers typed (its value
/// or the dropped-namespace error), the node stays live, and teardown's
/// pin-gated unlinks drain without a wedge.
#[test]
fn drop_races_inflight_cold_reads() {
    let dir = temp_data_dir("tiered-drop-race");
    let node = Node::start_durable(1, &dir);
    let mut c = node.connect();
    c.write_all(&cmd(&[b"INF.NS", b"CREATE", b"r", b"MODE", b"durable", b"MEM-BUDGET", b"3mb"]))
        .expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    c.write_all(&cmd(&[b"INF.NS", b"USE", b"r"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    let value = vec![0x5Eu8; 8 << 10];
    for i in 0..600 {
        c.write_all(&cmd(&[b"SET", format!("k:{i:04}").as_bytes(), &value])).expect("write");
        read_exactly(&mut c, b"+OK\r\n");
    }
    // Wait for demotion so the low keys are genuinely cold.
    let mut demoted = false;
    for _ in 0..300 {
        let tiering = info_text(&mut c, b"tiering");
        let flushed = tiering
            .lines()
            .find_map(|l| l.strip_prefix("tiering_flush_confirmed_bytes:"))
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(0);
        if flushed > 1 << 20 {
            demoted = true;
            break;
        }
        #[allow(clippy::disallowed_methods)] // test harness thread, not cell code
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(demoted, "demotion never flushed past 1 MiB");
    // Pipeline 50 cold GETs without reading replies, then DROP from a
    // second connection while they are in flight.
    let mut batch = Vec::new();
    for i in 0..50 {
        batch.extend_from_slice(&cmd(&[b"GET", format!("k:{i:04}").as_bytes()]));
    }
    c.write_all(&batch).expect("write");
    let mut c2 = node.connect();
    c2.write_all(&cmd(&[b"INF.NS", b"DROP", b"r"])).expect("write");
    read_exactly(&mut c2, b"+OK\r\n");
    // Every pipelined reply is typed: the value (read won the race) or
    // the dropped-namespace error (drop won) — never a hang or a crash.
    for _ in 0..50 {
        let reply = read_line(&mut c);
        assert!(
            reply.starts_with(b"$") || reply.starts_with(b"-ERR"),
            "untyped drop-race reply: {reply:?}"
        );
        if reply.starts_with(b"$8192") {
            // Consume the bulk payload + CRLF.
            let mut payload = vec![0u8; (8 << 10) + 2];
            c.read_exact(&mut payload).expect("bulk payload");
        }
    }
    // The node stays live.
    c2.write_all(&cmd(&[b"PING"])).expect("write");
    read_exactly(&mut c2, b"+PONG\r\n");
    drop(c);
    drop(c2);
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
    #[cfg(feature = "doc")]
    {
        c.write_all(&cmd(&[
            b"JSON.SET",
            b"book:doc",
            b"$",
            br#"{"n":40,"a":[1],"values":[1,1],"pad":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"}"#,
        ]))
        .expect("write");
        read_exactly(&mut c, b"+OK\r\n");
        c.write_all(&cmd(&[b"JSON.NUMINCRBY", b"book:doc", b".n", b"2"])).expect("write");
        read_exactly(&mut c, b"$2\r\n42\r\n");
        c.write_all(&cmd(&[b"JSON.ARRAPPEND", b"book:doc", b".a", b"2"])).expect("write");
        read_exactly(&mut c, b":2\r\n");
        c.write_all(&cmd(&[b"JSON.NUMINCRBY", b"book:doc", b"$.values[*]", b"1"])).expect("write");
        read_exactly(&mut c, b"$5\r\n[2,2]\r\n");
    }

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

    #[cfg(feature = "doc")]
    {
        let log_dir = dir.join("shard-0").join("log");
        let scan = inf_log::scan_log_dir(&inf_log::fs::StdSegmentFs, &log_dir).expect("scan log");
        let mut fulls = 0u64;
        let mut deltas = 0u64;
        let mut multi_match_deltas = 0u64;
        for &segment in scan.segments() {
            let mut reader = inf_log::SegmentReader::open(
                &inf_log::fs::StdSegmentFs,
                &log_dir,
                segment,
                inf_log::ReaderConfig::default(),
            )
            .expect("open segment");
            while let Some(frame) = reader.next_frame().expect("valid frame") {
                for record in frame.records() {
                    let (_, view) = record.expect("valid record");
                    fulls += u64::from(matches!(view, inf_log::RecordView::DocFull { .. }));
                    if let inf_log::RecordView::DocDelta { match_count, .. } = view {
                        deltas += 1;
                        multi_match_deltas += u64::from(match_count == 2);
                    }
                }
            }
        }
        assert!(fulls >= 1, "root JSON.SET staged DocFull");
        assert!(deltas >= 3, "path mutations staged DocDelta records");
        assert_eq!(
            multi_match_deltas, 1,
            "one two-match command emits one structural document record"
        );
    }

    // The published .ick validates end to end and covers the seed writes.
    let ick = dir.join("shard-0").join("ckpt").join("ckpt-000001.ick");
    let mut post_images = 0u64;
    let mut doc_fulls = 0u64;
    let (ick_info, audit) = inf_log::ckpt::read_ick(
        &inf_log::fs::StdSegmentFs,
        &ick,
        inf_log::ckpt::IckReaderConfig::default(),
        |view| {
            if matches!(view, inf_log::RecordView::StringPostImage { .. }) {
                post_images += 1;
            }
            if matches!(view, inf_log::RecordView::DocFull { .. }) {
                doc_fulls += 1;
            }
            Ok::<(), ()>(())
        },
    )
    .expect("published checkpoint validates");
    assert_eq!(ick_info.cell, 0);
    assert_eq!(ick_info.ckpt_id, 1);
    assert!(ick_info.begin_lsn.to_u64() > 0, "begin LSN recorded");
    assert!(post_images >= 401, "walk covered the pre-trigger writes: {post_images}");
    #[cfg(feature = "doc")]
    assert_eq!(doc_fulls, 1, "the live document checkpoints as one DocFull");
    assert_eq!(audit.entries_per_ns.len(), 1, "one durable namespace walked");

    // Restart on the same dir: replay (which now crosses the begin
    // marker) still yields the data.
    let node = Node::start_durable(1, &dir);
    let mut c = node.connect();
    c.write_all(&cmd(&[b"INF.NS", b"USE", b"books"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    c.write_all(&cmd(&[b"GET", b"book:0000"])).expect("write");
    read_exactly(&mut c, b"$7\r\ntitle-0\r\n");
    #[cfg(feature = "doc")]
    {
        c.write_all(&cmd(&[b"JSON.GET", b"book:doc", b".n"])).expect("write");
        read_exactly(&mut c, b"$2\r\n42\r\n");
        c.write_all(&cmd(&[b"JSON.GET", b"book:doc", b".a"])).expect("write");
        read_exactly(&mut c, b"$5\r\n[1,2]\r\n");
    }
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
    // M4.5-S36 (ADR-0088 D4/D7): the trigger is derived and reported —
    // the interval in force is at least the floor (16 KiB here) and, once
    // a checkpoint published, twice its on-disk size or the floor; the
    // write-amplification figure is defined only after the publish, and
    // the checkpoint's bytes are counted (v3 blocks: a multiple of 4 KiB).
    let interval = info_u64(&info, "ckpt_interval_bytes");
    let last = info_u64(&info, "ckpt_bytes_last");
    assert!(
        last > 0 && last.is_multiple_of(4096),
        "v3 checkpoint bytes are aligned blocks: {info}"
    );
    assert_eq!(interval, (2 * last).max(16 << 10), "interval = clamp(2 × last, floor, cap)");
    assert_eq!(info_u64(&info, "write_amp_log_checkpoint_undefined"), 0, "{info}");
    let amp = info_u64(&info, "write_amp_milli_log_checkpoint");
    assert!(amp >= 1000, "device bytes can only exceed record bytes: {info}");
    assert!(info_u64(&info, "log_frame_bytes") > 0, "{info}");
    assert!(info_u64(&info, "ckpt_bytes_total") >= last, "the total counts every publish");
    assert!(info.contains("io_budget_model:absent"), "no probe file in the test tree: {info}");
    drop(c);
    node.stop();
    std::fs::remove_dir_all(&dir).ok();
}

/// M4.5-S39d: a loop-resident boot's recovery decomposes by phase — the
/// checkpoint, the tail replay and the slack audit each report the bytes
/// they read, and the loop-clock durations sum to the total within the
/// µs rounding of five fields (every credited instant lands in exactly
/// one phase). The boot line carries the same numbers.
#[test]
fn recovery_phases_report_bytes_and_sum_to_the_total() {
    let dir = temp_data_dir("phases");
    let value = vec![b'p'; 2048];
    let tail_keys = 64u32;
    {
        let node = Node::start_durable(1, &dir);
        let mut c = node.connect();
        c.write_all(&cmd(&[b"INF.NS", b"CREATE", b"ph", b"MODE", b"durable", b"FSYNC", b"always"]))
            .expect("write");
        read_exactly(&mut c, b"+OK\r\n");
        c.write_all(&cmd(&[b"INF.NS", b"USE", b"ph"])).expect("write");
        read_exactly(&mut c, b"+OK\r\n");
        for i in 0..256u32 {
            let key = format!("base:{i:04}");
            c.write_all(&cmd(&[b"SET", key.as_bytes(), &value])).expect("write");
            read_exactly(&mut c, b"+OK\r\n");
        }
        // The checkpoint boundary: everything above is checkpoint work,
        // everything below is tail replay.
        c.write_all(&cmd(&[b"INF.CKPT", b"WAIT"])).expect("write");
        read_exactly(&mut c, b"+OK\r\n");
        for i in 0..tail_keys {
            let key = format!("tail:{i:04}");
            c.write_all(&cmd(&[b"SET", key.as_bytes(), &value])).expect("write");
            read_exactly(&mut c, b"+OK\r\n");
        }
        drop(c);
        node.stop();
    }
    let node = Node::start_durable(1, &dir);
    let mut c = node.connect();
    c.write_all(&cmd(&[b"INFO", b"persistence"])).expect("write");
    let mut buf = vec![0u8; 8192];
    let n = c.read(&mut buf).expect("read info");
    let info = String::from_utf8_lossy(&buf[..n]).into_owned();
    let f = |field: &str| info_u64(&info, field);
    assert!(f("recover_ckpt_bytes") > 256 * 2048, "the checkpoint's bytes were read: {info}");
    assert!(f("recover_replay_bytes") >= u64::from(tail_keys) * 2048, "tail bytes: {info}");
    // Frames, not records: the floor segment replays from its first
    // frame and skips the pre-begin records, so frames ≥ the tail's.
    assert!(f("recover_replay_frames") >= 1, "{info}");
    // The audit scans the active segment's slack (8 MiB segments here)
    // and the preallocated next one: far more than the data it follows.
    assert!(f("recover_audit_bytes") >= 4 << 20, "the slack audit read the slack: {info}");
    assert_eq!(f("recover_audit_foreign_frames"), 0, "no recycled life in a fresh log");
    let phases = [
        "recover_start_us",
        "recover_ckpt_us",
        "recover_replay_us",
        "recover_audit_us",
        "recover_finish_us",
    ];
    let sum: u64 = phases.iter().map(|p| f(p)).sum();
    let total = f("recover_total_us");
    assert!(total > 0, "the loop clock advanced across the boot: {info}");
    assert!(
        sum.abs_diff(total) <= phases.len() as u64,
        "phases sum to the total within µs rounding: {sum} vs {total}: {info}"
    );
    assert!(f("recover_ckpt_us") > 0 && f("recover_audit_us") > 0, "timed phases: {info}");
    drop(c);
    node.stop();
    std::fs::remove_dir_all(&dir).ok();
}

/// ADR-0088 D2 as amended (M4.5-S39d's finding): a checkpoint requested
/// on an **idle** node that spends a device budget completes. Before the
/// carry, the reference box's probe (2 540 write ops/s per device) on a
/// loop iterating every few hundred µs granted the checkpoint class
/// `⌊1270 × 0.0004⌋ × 2/10 = 0` ops per refill forever — `INF.CKPT
/// WAIT` after a fill never returned (the keep-up floor that feeds the
/// class under load is zero at idle). The budget must grant its share
/// over time whatever the refill interval.
#[test]
fn a_checkpoint_requested_on_an_idle_budgeted_node_completes() {
    let dir = temp_data_dir("idle-ckpt");
    // The harness loop parks 5 ms at idle (the product loop spun at
    // ~430 µs), so the model's op rate is scaled to reproduce the same
    // quantization: 100 ops/s per cell × 5 ms = 0.5 → 0 per refill, and
    // 0 × 2/10 = 0 for the checkpoint class. With the carry the class
    // accrues its 20 ops/s and the ~12-op checkpoint lands in < 1 s.
    let model = inf_runtime::DeviceModel {
        write_bytes_per_s: 510_132_224,
        write_ops_per_s: 200,
        read_bytes_per_s: 0,
        read_ops_per_s: 0,
    };
    let node = Node::start_durable_with_device_model(2, &dir, model);
    let mut c = node.connect();
    c.write_all(&cmd(&[b"INF.NS", b"CREATE", b"idle", b"MODE", b"durable", b"FSYNC", b"always"]))
        .expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    c.write_all(&cmd(&[b"INF.NS", b"USE", b"idle"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    let value = vec![b'i'; 4096];
    // ~4 MiB of images: a checkpoint of several 256 KiB sections — more
    // than one op's worth on every cell, so a starved op axis shows.
    for i in 0..1024u32 {
        let key = format!("k:{i:05}");
        c.write_all(&cmd(&[b"SET", key.as_bytes(), &value])).expect("write");
        read_exactly(&mut c, b"+OK\r\n");
    }
    // Idle now. The WAIT must return — the connection's 5 s read timeout
    // is the failure.
    c.set_read_timeout(Some(Duration::from_secs(20))).expect("timeout");
    let t0 = Instant::now();
    c.write_all(&cmd(&[b"INF.CKPT", b"WAIT"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    let took = t0.elapsed();
    c.write_all(&cmd(&[b"INFO", b"persistence"])).expect("write");
    let mut buf = vec![0u8; 8192];
    let n = c.read(&mut buf).expect("read info");
    let info = String::from_utf8_lossy(&buf[..n]).into_owned();
    assert!(info.contains("io_budget_model:probed"), "the budget was in force: {info}");
    assert!(info_u64(&info, "ckpts_completed") >= 1, "{info}");
    assert!(took < Duration::from_secs(10), "an idle checkpoint of ~2 MiB/cell took {took:?}");
    drop(c);
    node.stop();
    std::fs::remove_dir_all(&dir).ok();
}

/// Numeric INFO field (first match — single-cell tests).
fn info_u64(info: &str, field: &str) -> u64 {
    info.lines()
        .find_map(|l| l.strip_prefix(&format!("{field}:")))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or_else(|| panic!("field {field} in {info}"))
}

/// M2-S11 AC (dev-tier soak-lite; the 24 h leg rides the S22 soak):
/// checkpoint cycles advance the MANIFEST floor and the truncation slice
/// deletes covered segments — steady-state retained log ≈ ckpt interval +
/// one segment (+ the prealloc'd next), asserted at every sample once
/// truncation reaches steady state. Early keys then live ONLY in the
/// checkpoint (their segments are gone), so the restart leg proves
/// manifest-named recovery end to end: MANIFEST → `.ick` load → tail
/// replay from begin.
#[test]
fn truncation_bounds_log_size_and_restart_recovers_from_checkpoint() {
    let dir = temp_data_dir("trunc");
    // 8 MiB segments (harness), checkpoint every 1 MiB appended — the
    // *fixed* trigger (α = 0): the bound below is stated in multiples of
    // the interval, and the product's derived trigger would chase the
    // dataset instead (a 21 MiB checkpoint → a 42 MiB interval the 32 MiB
    // trickle cap never reaches on a real device; see `CkptTrigger`).
    let node = Node::start_durable_fixed_ckpt(1, &dir, 1 << 20);
    let mut c = node.connect();
    c.write_all(&cmd(&[b"INF.NS", b"CREATE", b"soak", b"MODE", b"durable", b"FSYNC", b"everysec"]))
        .expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    c.write_all(&cmd(&[b"INF.NS", b"USE", b"soak"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");

    // ~19 MiB of records → two 8 MiB rotations while checkpoints cycle.
    let value = vec![b'v'; 4096];
    let mut ok = Vec::new();
    for wave in 0..3u32 {
        for batch in 0..12u32 {
            let mut wire = Vec::with_capacity(128 * 4200);
            ok.clear();
            for i in 0..128u32 {
                let key = format!("w{wave}:{:05}", batch * 128 + i);
                wire.extend_from_slice(&cmd(&[b"SET", key.as_bytes(), &value]));
                ok.extend_from_slice(b"+OK\r\n");
            }
            c.write_all(&wire).expect("write wave");
            read_exactly(&mut c, &ok);
        }
    }

    // Trickle writes drive further triggers until truncation reaches
    // steady state: floor in the active segment, both sealed ones gone.
    // The trickle is byte-bounded, not just time-bounded (M2.5-S11): in
    // the release profile a lap is fast enough to write GiBs into the
    // 16 GiB tmpfs before a 30 s deadline — every shell on the box then
    // fails. 8192 ticks ≈ 32 MiB caps the footprint; the adaptive drain
    // (ADR-0022 D8.4) reaches steady state well inside it.
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut i = 0u32;
    let info = loop {
        if i < 8192 {
            for _ in 0..16 {
                let key = format!("tick:{i:06}");
                c.write_all(&cmd(&[b"SET", key.as_bytes(), &value])).expect("write");
                read_exactly(&mut c, b"+OK\r\n");
                i += 1;
            }
        } else {
            // Write cap reached: the latched checkpoint/manifest cycles
            // complete on MAINTAIN without further traffic — poll for them.
            #[allow(clippy::disallowed_methods)] // test harness thread, not cell code
            std::thread::sleep(Duration::from_millis(20));
        }
        c.write_all(&cmd(&[b"INFO", b"persistence"])).expect("write");
        let mut buf = vec![0u8; 4096];
        let n = c.read(&mut buf).expect("read info");
        let info = String::from_utf8_lossy(&buf[..n]).into_owned();
        if info_u64(&info, "segments_truncated") >= 2 || Instant::now() > deadline {
            break info;
        }
    };
    assert!(info_u64(&info, "manifests_published") >= 1, "manifest published: {info}");
    assert!(info_u64(&info, "segments_truncated") >= 2, "both sealed segments deleted: {info}");

    // Writes stopped: let the in-flight (paced — ADR-0017) cycle drain,
    // then every sample must hold the steady-state bound — interval
    // (1 MiB) fits inside the active segment, so live = active +
    // prealloc'd next (+1 transient around a rotation).
    let settle = Instant::now() + Duration::from_secs(15);
    loop {
        c.write_all(&cmd(&[b"INFO", b"persistence"])).expect("write");
        let mut buf = vec![0u8; 4096];
        let n = c.read(&mut buf).expect("read info");
        let sample = String::from_utf8_lossy(&buf[..n]).into_owned();
        if info_u64(&sample, "log_segments_live") <= 3 {
            break;
        }
        assert!(Instant::now() < settle, "never settled to the bound: {sample}");
    }
    for _ in 0..5 {
        c.write_all(&cmd(&[b"INFO", b"persistence"])).expect("write");
        let mut buf = vec![0u8; 4096];
        let n = c.read(&mut buf).expect("read info");
        let sample = String::from_utf8_lossy(&buf[..n]).into_owned();
        assert!(
            info_u64(&sample, "log_segments_live") <= 3,
            "retained log bounded by interval + one segment (+ next): {sample}"
        );
    }
    drop(c);
    node.stop();

    // On-disk truth matches the gauges: ≤ 3 segment files — polled
    // briefly, because unlinks are *delegated* to the control thread
    // (ADR-0017: a large unlink on the loop is a measured p99.9 stall)
    // and it drains its queue asynchronously after the cells stop. The
    // runtime GC kept exactly one published .ick (a walk may have been
    // mid-flight at shutdown — its .ick.new orphan is the boot GC's job,
    // asserted after the restart below).
    let unlink_deadline = Instant::now() + Duration::from_secs(10);
    let log_files = loop {
        let n = std::fs::read_dir(dir.join("shard-0").join("log")).expect("log dir").count();
        if n <= 3 || Instant::now() > unlink_deadline {
            break n;
        }
        #[allow(clippy::disallowed_methods)] // test harness thread, not cell code
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(log_files <= 3, "segment files on disk: {log_files}");
    let icks = |dir: &std::path::Path| -> Vec<String> {
        std::fs::read_dir(dir.join("shard-0").join("ckpt"))
            .expect("ckpt dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect()
    };
    let published: Vec<_> = icks(&dir).into_iter().filter(|name| name.ends_with(".ick")).collect();
    assert_eq!(published.len(), 1, "GC keeps only the manifest-named checkpoint: {published:?}");

    // Restart on the truncated log: wave-0 keys exist only in the
    // checkpoint now — this GET proves the MANIFEST → .ick → tail path.
    let node = Node::start_durable(1, &dir);
    let mut c = node.connect();
    c.write_all(&cmd(&[b"INF.NS", b"USE", b"soak"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    let mut want = b"$4096\r\n".to_vec();
    want.extend_from_slice(&value);
    want.extend_from_slice(b"\r\n");
    for key in ["w0:00000", "w2:01535", &format!("tick:{:06}", i - 1)] {
        c.write_all(&cmd(&[b"GET", key.as_bytes()])).expect("write");
        read_exactly(&mut c, &want);
    }
    drop(c);
    node.stop();

    // Boot GC removed the mid-flight .ick.new orphan (and any unnamed
    // .ick); exactly the named checkpoint remains.
    let after: Vec<String> = std::fs::read_dir(dir.join("shard-0").join("ckpt"))
        .expect("ckpt dir")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(after.len(), 1, "boot GC leaves only the named checkpoint: {after:?}");
    assert!(after[0].ends_with(".ick") && !after[0].ends_with(".ick.new"), "{after:?}");
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

/// M2-S15: the `-LOADING` window, byte-diffed against the Redis 8.0.5
/// oracle capture (`.artifacts/m2/loading-redis-capture-20260703/`).
/// Gated commands — including PING, which Redis 8.0.5 *observably* gates —
/// answer the exact Redis error bytes; ECHO/SELECT/pubsub/INFO pass;
/// unknown commands resolve before the gate (Redis order); and the same
/// connection serves normally once recovery completes (loading-era
/// connections survive the transition). Recovery is throttled by the
/// test-only pacing knob — the designed vehicle for observing the window.
#[test]
fn loading_gate_byte_matches_redis_and_lifts() {
    const LOADING: &[u8] = b"-LOADING Redis is loading the dataset in memory\r\n";
    let dir = temp_data_dir("loading");
    let val = vec![b'v'; 4096];

    // Phase 1: build ~1 MiB of durable log to replay.
    {
        let node = Node::start_durable(1, &dir);
        let mut c = node.connect();
        c.write_all(&cmd(&[
            b"INF.NS",
            b"CREATE",
            b"led",
            b"MODE",
            b"durable",
            b"FSYNC",
            b"everysec",
        ]))
        .expect("write");
        read_exactly(&mut c, b"+OK\r\n");
        c.write_all(&cmd(&[b"INF.NS", b"USE", b"led"])).expect("write");
        read_exactly(&mut c, b"+OK\r\n");
        for i in 0..256u32 {
            c.write_all(&cmd(&[b"SET", format!("k:{i}").as_bytes(), &val])).expect("write");
            read_exactly(&mut c, b"+OK\r\n");
        }
        drop(c);
        node.stop();
    }

    // Phase 2: throttled reboot — ~1 MiB at 128 KiB/s holds the window
    // ~8 s (pacing meters consumed bytes, never prealloc slack).
    let node = Node::start_with_recover(
        1,
        Some(dir.clone()),
        inf_server::RecoverConfig { step_bytes: 32 << 10, throttle_bytes_per_sec: Some(128 << 10) },
    );
    let mut c = node.connect();

    // We are inside the window: INFO passes the gate and reports the
    // loading fields (Redis field names; totals are file extents).
    let info = info_text(&mut c, b"persistence");
    assert!(info.contains("loading:1\r\n"), "window missed: {info}");
    assert!(info.contains("loading_start_time:"), "{info}");
    assert!(info.contains("loading_total_bytes:"), "{info}");
    assert!(info.contains("loading_loaded_bytes:"), "{info}");
    assert!(info.contains("loading_loaded_perc:"), "{info}");
    assert!(info.contains("loading_eta_seconds:"), "{info}");
    assert!(info.contains("loading_cells_ready:0\r\n"), "{info}");
    assert!(info.contains("loading_cells:1\r\n"), "{info}");

    // Gated commands answer the exact oracle bytes.
    for gated in [
        cmd(&[b"GET", b"k:0"]),
        cmd(&[b"SET", b"x", b"y"]),
        cmd(&[b"PING"]),
        cmd(&[b"DEL", b"x"]),
        cmd(&[b"FLUSHALL"]),
        cmd(&[b"DBSIZE"]),
        cmd(&[b"INF.NS", b"USE", b"led"]),
    ] {
        c.write_all(&gated).expect("write");
        read_exactly(&mut c, LOADING);
    }

    // Loading-allowed commands serve normally (oracle capture set).
    c.write_all(&cmd(&[b"ECHO", b"hi"])).expect("write");
    read_exactly(&mut c, b"$2\r\nhi\r\n");
    c.write_all(&cmd(&[b"SELECT", b"1"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    c.write_all(&cmd(&[b"SELECT", b"0"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");

    // Unknown commands resolve before the gate (Redis order).
    c.write_all(&cmd(&[b"NOSUCHCMD", b"x"])).expect("write");
    let line = read_line(&mut c);
    assert!(line.starts_with(b"-ERR unknown command"), "{line:?}");

    // Pub/sub is fully live during loading (loading-flagged in Redis).
    let mut sub = node.connect();
    sub.write_all(&cmd(&[b"SUBSCRIBE", b"ch"])).expect("write");
    read_exactly(&mut sub, b"*3\r\n$9\r\nsubscribe\r\n$2\r\nch\r\n:1\r\n");
    c.write_all(&cmd(&[b"PUBLISH", b"ch", b"m"])).expect("write");
    read_exactly(&mut c, b":1\r\n");
    read_exactly(&mut sub, b"*3\r\n$7\r\nmessage\r\n$2\r\nch\r\n$1\r\nm\r\n");

    // The SAME connection lifts into normal service when the load ends.
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        c.write_all(&cmd(&[b"GET", b"k:0"])).expect("write");
        let line = read_line(&mut c);
        if line == b"$-1\r\n" {
            break; // default ns: key absent — the gate lifted
        }
        assert_eq!(line, LOADING.to_vec(), "unexpected reply {line:?}");
        assert!(Instant::now() < deadline, "loading never lifted");
        #[allow(clippy::disallowed_methods)] // test harness thread, not cell code
        std::thread::sleep(Duration::from_millis(50));
    }
    let info = info_text(&mut c, b"persistence");
    assert!(info.contains("loading:0\r\n"), "{info}");

    // Recovered data intact in the durable namespace, same connection.
    c.write_all(&cmd(&[b"INF.NS", b"USE", b"led"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    c.write_all(&cmd(&[b"GET", b"k:0"])).expect("write");
    let mut want = format!("${}\r\n", val.len()).into_bytes();
    want.extend_from_slice(&val);
    want.extend_from_slice(b"\r\n");
    read_exactly(&mut c, &want);

    drop(c);
    drop(sub);
    node.stop();
    std::fs::remove_dir_all(&dir).ok();
}

/// M2-S15 orchestration: the gate lifts only when EVERY cell is ready.
/// Cell 1 has (nearly) no log and recovers instantly; cell 0 replays a
/// throttled ~1 MiB — a connection landed on the *ready* cell must still
/// answer `-LOADING` (node semantics over per-cell recovery), and INFO
/// exposes the partial readiness.
#[test]
fn loading_lifts_only_when_every_cell_recovered() {
    const LOADING: &[u8] = b"-LOADING Redis is loading the dataset in memory\r\n";
    let dir = temp_data_dir("loading-cells");
    let val = vec![b'v'; 4096];
    let k0 = key_for_cell(2, 0);

    {
        let node = Node::start_durable(2, &dir);
        let mut c = node.connect();
        c.write_all(&cmd(&[
            b"INF.NS",
            b"CREATE",
            b"led",
            b"MODE",
            b"durable",
            b"FSYNC",
            b"everysec",
        ]))
        .expect("write");
        read_exactly(&mut c, b"+OK\r\n");
        c.write_all(&cmd(&[b"INF.NS", b"USE", b"led"])).expect("write");
        read_exactly(&mut c, b"+OK\r\n");
        // Every write keys to cell 0: cell 1's log stays empty.
        for i in 0..256u32 {
            let key = [k0.as_slice(), format!(":{i}").as_bytes()].concat();
            let hashtag = [b"{", k0.as_slice(), b"}", key.as_slice()].concat();
            c.write_all(&cmd(&[b"SET", &hashtag, &val])).expect("write");
            read_exactly(&mut c, b"+OK\r\n");
        }
        drop(c);
        node.stop();
    }

    let node = Node::start_with_recover(
        2,
        Some(dir.clone()),
        inf_server::RecoverConfig { step_bytes: 32 << 10, throttle_bytes_per_sec: Some(128 << 10) },
    );

    // Wait until cell 1 (empty log) is ready while cell 0 still loads.
    let board = Arc::clone(node.control.as_ref().expect("durable node").recovery_board());
    let deadline = Instant::now() + Duration::from_secs(10);
    while board.ready_cells() == 0 {
        assert!(Instant::now() < deadline, "no cell became ready");
        #[allow(clippy::disallowed_methods)] // test harness thread, not cell code
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(!board.all_ready(), "throttled cell finished too fast for the window");

    // A connection on the READY cell still answers -LOADING: the gate is
    // node-scoped, not cell-scoped.
    let ready_cell = if board.slot(0).ready() { 0 } else { 1 };
    let mut c = conn_on_cell(&node, ready_cell);
    c.write_all(&cmd(&[b"GET", b"anykey"])).expect("write");
    read_exactly(&mut c, LOADING);
    let info = info_text(&mut c, b"persistence");
    assert!(info.contains("loading:1\r\n"), "{info}");
    assert!(info.contains("loading_cells_ready:1\r\n"), "{info}");
    assert!(info.contains("loading_cells:2\r\n"), "{info}");

    // Lift: both cells serve, the throttled cell's data is intact.
    let deadline = Instant::now() + Duration::from_secs(60);
    while !board.all_ready() {
        assert!(Instant::now() < deadline, "loading never lifted");
        #[allow(clippy::disallowed_methods)] // test harness thread, not cell code
        std::thread::sleep(Duration::from_millis(50));
    }
    let mut c2 = node.connect();
    let probe =
        [b"{".as_slice(), k0.as_slice(), b"}".as_slice(), k0.as_slice(), b":0".as_slice()].concat();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        c2.write_all(&cmd(&[b"INF.NS", b"USE", b"led"])).expect("write");
        let line = read_line(&mut c2);
        if line == b"+OK\r\n" {
            break;
        }
        assert_eq!(line, LOADING.to_vec(), "unexpected reply {line:?}");
        assert!(Instant::now() < deadline, "USE never accepted after all-ready");
        #[allow(clippy::disallowed_methods)] // test harness thread, not cell code
        std::thread::sleep(Duration::from_millis(10));
    }
    c2.write_all(&cmd(&[b"GET", &probe])).expect("write");
    let mut want = format!("${}\r\n", val.len()).into_bytes();
    want.extend_from_slice(&val);
    want.extend_from_slice(b"\r\n");
    read_exactly(&mut c2, &want);

    drop(c);
    drop(c2);
    node.stop();
    std::fs::remove_dir_all(&dir).ok();
}

/// M2-S16 (the re-bound S02 observation row): on a LIVE node whose
/// durable plane is ENOSPC-exhausted — `prealloc_no_space` armed from the
/// second prealloc, so exhaustion surfaces in the first MAINTAIN after
/// boot — durable writes refuse with the documented NOSPACE error while
/// memory namespaces (and the default DB) keep serving, and reads of
/// recovered durable state still work. Degrade loudly, never corrupt.
#[test]
fn memory_namespace_serves_while_durable_plane_is_exhausted() {
    use inf_foundation::fault::FaultSpec;
    let dir = temp_data_dir("enospc-live");

    // Seed one durable key on a healthy node, then restart exhausted.
    // FSYNC always: the +OK gates on the fsync watermark, so the seeded
    // frame is provably in the segment before `stop()`. An everysec ack
    // promises nothing at the harness stop (crash-equivalent: no drain,
    // the ring dies with the frame write still queued) — the seed was
    // occasionally lost and recovery replayed zero records (the ~1/8
    // full-suite flake this test carried).
    {
        let node = Node::start_durable(1, &dir);
        let mut c = node.connect();
        c.write_all(&cmd(&[
            b"INF.NS", b"CREATE", b"led", b"MODE", b"durable", b"FSYNC", b"always",
        ]))
        .expect("write");
        read_exactly(&mut c, b"+OK\r\n");
        c.write_all(&cmd(&[b"INF.NS", b"USE", b"led"])).expect("write");
        read_exactly(&mut c, b"+OK\r\n");
        c.write_all(&cmd(&[b"SET", b"seeded", b"1"])).expect("write");
        read_exactly(&mut c, b"+OK\r\n");
        drop(c);
        node.stop();
    }

    // Recovery reopens the tail segment without preallocating, so boot
    // completes; the first MAINTAIN's next-segment prealloc (occurrence
    // #1) fails and every retry after it — the durable plane exhausts on
    // a live, serving node.
    let node = Node::start_durable_with_faults(
        1,
        &dir,
        vec![(inf_log::fault::PREALLOC_NO_SPACE, FaultSpec::FromNth(1))],
    );
    let mut c = node.connect();

    // Recovered durable state reads fine (reads need no log append).
    c.write_all(&cmd(&[b"INF.NS", b"USE", b"led"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    c.write_all(&cmd(&[b"GET", b"seeded"])).expect("write");
    read_exactly(&mut c, b"$1\r\n1\r\n");

    // Durable writes refuse loudly with the documented NOSPACE error
    // (poll: exhaustion surfaces in a MAINTAIN slice shortly after boot).
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        c.write_all(&cmd(&[b"SET", b"blocked", b"x"])).expect("write");
        let line = read_line(&mut c);
        if line == b"-ERR durable write refused: log storage exhausted (NOSPACE)\r\n" {
            break;
        }
        assert_eq!(line, b"+OK\r\n".to_vec(), "unexpected reply {line:?}");
        assert!(Instant::now() < deadline, "exhaustion never surfaced");
        #[allow(clippy::disallowed_methods)] // test harness thread, not cell code
        std::thread::sleep(Duration::from_millis(5));
    }

    // Memory namespaces are unaffected: named memory ns and default DB.
    let mut m = node.connect();
    m.write_all(&cmd(&[b"INF.NS", b"CREATE", b"cache", b"MODE", b"memory"])).expect("write");
    read_exactly(&mut m, b"+OK\r\n");
    m.write_all(&cmd(&[b"INF.NS", b"USE", b"cache"])).expect("write");
    read_exactly(&mut m, b"+OK\r\n");
    m.write_all(&cmd(&[b"SET", b"hot", b"v"])).expect("write");
    read_exactly(&mut m, b"+OK\r\n");
    m.write_all(&cmd(&[b"GET", b"hot"])).expect("write");
    read_exactly(&mut m, b"$1\r\nv\r\n");
    let mut d = node.connect();
    d.write_all(&cmd(&[b"SET", b"plain", b"ok"])).expect("write");
    read_exactly(&mut d, b"+OK\r\n");
    d.write_all(&cmd(&[b"GET", b"plain"])).expect("write");
    read_exactly(&mut d, b"$2\r\nok\r\n");

    drop(c);
    drop(m);
    drop(d);
    node.stop();
    std::fs::remove_dir_all(&dir).ok();
}

/// M2-S20 AC: `INF.CKPT WAIT` returns only after the new MANIFEST is
/// durable — with `manifest_rename_fail` armed on the first swap attempt,
/// the reply must outlast the counted abort and land after the *retried*
/// swap commits (fault-injection verified). `BGSAVE`/`LASTSAVE` map onto
/// the same machinery: LASTSAVE is 0 before the first publication and
/// advances after it.
#[test]
fn inf_ckpt_wait_returns_after_manifest_durability() {
    let dir = temp_data_dir("ckptwait");
    let node = Node::start_durable_with_faults(
        1,
        &dir,
        vec![(inf_log::fault::MANIFEST_RENAME_FAIL, inf_foundation::fault::FaultSpec::Nth(1))],
    );
    let mut c = node.connect();

    c.write_all(&cmd(&[b"LASTSAVE"])).expect("write");
    read_exactly(&mut c, b":0\r\n");

    c.write_all(&cmd(&[b"INF.NS", b"CREATE", b"gate", b"MODE", b"durable", b"FSYNC", b"always"]))
        .expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    c.write_all(&cmd(&[b"INF.NS", b"USE", b"gate"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    c.write_all(&cmd(&[b"SET", b"k", b"v"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");

    // WAIT must survive the injected first-swap abort: the fault fires at
    // envelope step 5, the swap aborts (counted), the trigger stays
    // latched, the retry commits — only then may +OK arrive.
    c.write_all(&cmd(&[b"INF.CKPT", b"WAIT"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");

    let info = info_text(&mut c, b"persistence");
    assert!(info_u64(&info, "manifests_published") >= 1, "durable manifest: {info}");
    assert!(info_u64(&info, "manifests_aborted") >= 1, "the injected abort happened: {info}");
    assert!(info_u64(&info, "rdb_last_save_time") > 0, "save time follows the publish: {info}");

    // LASTSAVE now reports the publication (unix seconds, > 0).
    c.write_all(&cmd(&[b"LASTSAVE"])).expect("write");
    let lastsave = read_line(&mut c);
    assert!(lastsave.starts_with(b":") && lastsave != b":0\r\n", "{lastsave:?}");

    // BGSAVE: the Redis reply byte-for-byte; a later INF.CKPT WAIT
    // fences it (deterministic save-then-check without polling).
    c.write_all(&cmd(&[b"BGSAVE"])).expect("write");
    read_exactly(&mut c, b"+Background saving started\r\n");
    c.write_all(&cmd(&[b"INF.CKPT", b"WAIT"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    let info = info_text(&mut c, b"persistence");
    assert!(info_u64(&info, "manifests_published") >= 2, "BGSAVE published too: {info}");

    drop(c);
    node.stop();
    std::fs::remove_dir_all(&dir).ok();
}

/// M2-S20: `INF.CKPT CELL k WAIT` targets one cell — the reply fences that
/// cell's durable MANIFEST without requiring peers to checkpoint.
#[test]
fn inf_ckpt_cell_targets_one_cell() {
    let dir = temp_data_dir("ckptcell");
    let node = Node::start_durable(2, &dir);
    let mut c = node.connect();

    c.write_all(&cmd(&[b"INF.NS", b"CREATE", b"gate", b"MODE", b"durable", b"FSYNC", b"everysec"]))
        .expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    c.write_all(&cmd(&[b"INF.NS", b"USE", b"gate"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    for i in 0..8u32 {
        c.write_all(&cmd(&[b"SET", format!("k{i}").as_bytes(), b"v"])).expect("write");
        read_exactly(&mut c, b"+OK\r\n");
    }
    c.write_all(&cmd(&[b"INF.CKPT", b"CELL", b"0", b"WAIT"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    c.write_all(&cmd(&[b"INF.CKPT", b"CELL", b"1", b"WAIT"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    // Out-of-range cell: documented refusal.
    c.write_all(&cmd(&[b"INF.CKPT", b"CELL", b"9", b"WAIT"])).expect("write");
    let err = read_line(&mut c);
    assert!(err.starts_with(b"-ERR CELL"), "{err:?}");

    drop(c);
    node.stop();
    std::fs::remove_dir_all(&dir).ok();
}

/// M3-S11 cross-cell `JSON.MGET` (ADR-0041 D9): the gather splits per
/// key with single-key sub-ops whose `*1` elements reassemble in argv
/// order — over the default db, and over a **named** namespace, which is
/// exactly the ADR-0032 D5 binding this story executes (`send_apply_ns`
/// per remote position; the generic multi-key refusal stays for
/// everything else). Single-key remote JSON commands ride the ordinary
/// fast arm alongside.
#[test]
fn json_mget_gathers_across_cells() {
    // A durable node shape: namespace DDL needs the control plane. The
    // JSON namespace itself is memory-class (durable JSON writes refuse
    // until M3-S17 — covered by the command suite).
    let dir = temp_data_dir("jsonmget");
    let node = Node::start_durable(2, &dir);
    let mut client = node.connect();
    let k0 = key_for_cell(2, 0);
    let k1 = key_for_cell(2, 1);

    // Default-db gather: one of k0/k1 is remote from the accepting cell.
    let mut pipeline = Vec::new();
    pipeline.extend(cmd(&[b"JSON.SET", &k0, b"$", br#"{"n":1}"#]));
    pipeline.extend(cmd(&[b"JSON.SET", &k1, b"$", br#"{"n":2}"#]));
    pipeline.extend(cmd(&[b"JSON.MGET", &k0, &k1, b"missing", b"$.n"]));
    client.write_all(&pipeline).expect("write");
    read_exactly(&mut client, b"+OK\r\n+OK\r\n*3\r\n$3\r\n[1]\r\n$3\r\n[2]\r\n$-1\r\n");

    // Single-key JSON mutation on the remote key rides the fast arm.
    client.write_all(&cmd(&[b"JSON.NUMINCRBY", &k0, b"$.n", b"1"])).expect("write");
    read_exactly(&mut client, b"$3\r\n[2]\r\n");

    // Named-namespace binding (memory class — durable JSON writes refuse
    // until M3-S17): the same gather rides `send_apply_ns`.
    let mut pipeline = Vec::new();
    pipeline.extend(cmd(&[b"INF.NS", b"CREATE", b"docs", b"MODE", b"memory"]));
    pipeline.extend(cmd(&[b"INF.NS", b"USE", b"docs"]));
    pipeline.extend(cmd(&[b"JSON.SET", &k0, b"$", br#"{"n":10}"#]));
    pipeline.extend(cmd(&[b"JSON.SET", &k1, b"$", br#"{"n":20}"#]));
    pipeline.extend(cmd(&[b"JSON.MGET", &k0, &k1, b"$.n"]));
    client.write_all(&pipeline).expect("write");
    read_exactly(&mut client, b"+OK\r\n+OK\r\n+OK\r\n+OK\r\n*2\r\n$4\r\n[10]\r\n$4\r\n[20]\r\n");

    // The generic named-ns multi-key refusal is untouched (ADR-0032 D5
    // scope: only the JSON surface binds at M3).
    client.write_all(&cmd(&[b"MGET", &k0, &k1])).expect("write");
    let line = read_line(&mut client);
    assert!(line.starts_with(b"-ERR multi-key commands"), "refusal stays: {line:?}");
}

// ---- M4.5-S34: FUA-class frames on pre-zeroed O_DIRECT segments (ADR-0086) ----

/// On a `Direct` node (real `O_DIRECT` + `RWF_DSYNC` through io_uring) a
/// fresh cell starts FLUSH-class, MAINTAIN pre-zeroes the next segment,
/// the class-upgrade rotation flips it to `fua`, and from then on every
/// `always` write rides a write-through frame (`fsyncs_fua` climbs while
/// `fsyncs_linked` stops). Every acked write replays after a restart — the
/// reopened tail reads its pre-zeroed fact from the file — and padding
/// and zero-fill amplification are disclosed, never zero.
#[test]
fn direct_segments_converge_to_fua_and_survive_restart() {
    let dir = temp_data_dir("s34-direct");
    let node = Node::start_durable_direct(1, &dir);
    let mut c = node.connect();
    c.write_all(&cmd(&[b"INF.NS", b"CREATE", b"fua", b"MODE", b"durable", b"FSYNC", b"always"]))
        .expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    c.write_all(&cmd(&[b"INF.NS", b"USE", b"fua"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");

    // Segment 0 is sparse (FLUSH class); the 8 MiB next segment zero-fills
    // in eight 1 MiB driver slices plus a barrier — wait for the upgrade
    // rotation by writing until INFO reports the class.
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut writes = 0u32;
    loop {
        writes += 1;
        let key = format!("k:{writes}");
        c.write_all(&cmd(&[b"SET", key.as_bytes(), b"v"])).expect("write");
        read_exactly(&mut c, b"+OK\r\n");
        let info = info_persistence(&mut c);
        let class = info
            .lines()
            .find_map(|l| l.strip_prefix("barrier_class:"))
            .unwrap_or_else(|| panic!("barrier_class missing:\n{info}"));
        if class == "fua" {
            assert!(info_field(&info, "rotations_upgrade") >= 1, "{info}");
            assert!(info_field(&info, "zero_fill_bytes") >= 8 << 20, "{info}");
            break;
        }
        assert!(Instant::now() < deadline, "never upgraded to fua after {writes} writes:\n{info}");
        #[allow(clippy::disallowed_methods)] // test harness thread, not cell code
        std::thread::sleep(Duration::from_millis(2));
    }
    // Now every always write is a write-through frame: the FUA counter
    // climbs and the linked-fsync counter freezes.
    let before = info_persistence(&mut c);
    let linked_before = info_field(&before, "fsyncs_linked");
    let fua_before = info_field(&before, "fsyncs_fua");
    for i in 0..64u32 {
        let key = format!("w:{i}");
        c.write_all(&cmd(&[b"SET", key.as_bytes(), b"through"])).expect("write");
        read_exactly(&mut c, b"+OK\r\n");
    }
    let after = info_persistence(&mut c);
    assert_eq!(info_field(&after, "fsyncs_linked"), linked_before, "no FLUSH after the upgrade");
    assert!(info_field(&after, "fsyncs_fua") >= fua_before + 64, "{after}");
    assert!(info_field(&after, "fua_latency_p50_us") > 0, "{after}");
    assert!(info_field(&after, "log_padding_bytes") > 0, "v3 padding is disclosed");
    assert_eq!(info_field(&after, "barrier_class_degraded"), 0);
    drop(c);
    node.stop();

    // Restart: the tail reopens Direct and pre-zeroed; every acked write
    // is back; new writes are write-through from the first frame.
    let node = Node::start_durable_direct(1, &dir);
    let mut c = connect_use(&node, b"fua");
    for i in 0..64u32 {
        let key = format!("w:{i}");
        c.write_all(&cmd(&[b"GET", key.as_bytes()])).expect("write");
        read_exactly(&mut c, b"$7\r\nthrough\r\n");
    }
    c.write_all(&cmd(&[b"GET", b"k:1"])).expect("write");
    read_exactly(&mut c, b"$1\r\nv\r\n");
    c.write_all(&cmd(&[b"SET", b"after", b"restart"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    let info = info_persistence(&mut c);
    assert!(info.contains("barrier_class:fua"), "reopened tail is pre-zeroed:\n{info}");
    assert!(info_field(&info, "fsyncs_fua") >= 1, "{info}");
    drop(c);
    node.stop();
    std::fs::remove_dir_all(&dir).ok();
}

/// M4.5-S35 (ADR-0087): with `frames_in_flight = 4` on a Direct cell,
/// concurrent `always` writers fill the pipeline (INFO proves it reached
/// ≥ 2 frames in flight), every write is a write-through frame, acks stay
/// a prefix (every acked key is back after a restart), and the pipeline
/// gauges are exported. Real io_uring; run with `TMPDIR` on the NVMe for
/// the device tier (tmpfs swallows `O_DIRECT`/FUA).
/// M4.5-S36 (ADR-0088 D5 amended): a `Direct` cell with only `everysec`
/// namespaces never pre-zeroes — no write-through consumer, no second
/// write (`zero_fill_bytes` stays 0 while segments rotate un-zeroed and
/// the class reads `flush`); the first `always` namespace starts the fill
/// and the class upgrades at the next rotation (ADR-0086 D4's machinery).
#[test]
fn everysec_only_cell_skips_pre_zeroing_until_an_always_namespace_exists() {
    let dir = temp_data_dir("s36-zero-fill-gate");
    let node = Node::start_durable_pipeline(1, &dir, 3);
    let mut c = node.connect();
    c.write_all(&cmd(&[b"INF.NS", b"CREATE", b"esec", b"MODE", b"durable", b"FSYNC", b"everysec"]))
        .expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    c.write_all(&cmd(&[b"INF.NS", b"USE", b"esec"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    // Drive past several segment rotations (the test node's segments are
    // small): no zero byte is ever written, the class stays flush.
    let value = vec![b'z'; 4096];
    let deadline = Instant::now() + Duration::from_secs(30);
    let info = loop {
        for i in 0..256 {
            let key = format!("e:{i:05}");
            c.write_all(&cmd(&[b"SET", key.as_bytes(), &value])).expect("write");
            read_exactly(&mut c, b"+OK\r\n");
        }
        let info = info_persistence(&mut c);
        if info_field(&info, "rotations_unzeroed") >= 2 || Instant::now() > deadline {
            break info;
        }
    };
    assert!(info_field(&info, "rotations_unzeroed") >= 2, "segments rotated un-zeroed:\n{info}");
    assert_eq!(info_field(&info, "zero_fill_bytes"), 0, "no write-through consumer:\n{info}");
    assert!(info.contains("barrier_class:flush"), "{info}");
    assert_eq!(info_field(&info, "io_budget_bytes_zero_fill"), 0, "{info}");

    // An `always` namespace appears: the fill starts and the class
    // upgrades at the next rotation.
    c.write_all(&cmd(&[b"INF.NS", b"CREATE", b"alw", b"MODE", b"durable", b"FSYNC", b"always"]))
        .expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut i = 0u32;
    let info = loop {
        for _ in 0..64 {
            let key = format!("f:{i:05}");
            c.write_all(&cmd(&[b"SET", key.as_bytes(), &value])).expect("write");
            read_exactly(&mut c, b"+OK\r\n");
            i += 1;
        }
        let info = info_persistence(&mut c);
        if info.contains("barrier_class:fua") || Instant::now() > deadline {
            break info;
        }
    };
    assert!(
        info.contains("barrier_class:fua"),
        "the class upgraded once an always ns exists:\n{info}"
    );
    assert!(info_field(&info, "zero_fill_bytes") > 0, "{info}");
    drop(c);
    node.stop();
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn frame_pipeline_fills_under_concurrent_always_writers_and_survives_restart() {
    let dir = temp_data_dir("s35-pipeline");
    let node = Node::start_durable_pipeline(1, &dir, 4);
    let mut c = node.connect();
    c.write_all(&cmd(&[b"INF.NS", b"CREATE", b"pipe", b"MODE", b"durable", b"FSYNC", b"always"]))
        .expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    c.write_all(&cmd(&[b"INF.NS", b"USE", b"pipe"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    // Wait for the class-upgrade rotation (segment 1 pre-zeroed).
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut writes = 0u32;
    loop {
        writes += 1;
        let key = format!("k:{writes}");
        c.write_all(&cmd(&[b"SET", key.as_bytes(), b"v"])).expect("write");
        read_exactly(&mut c, b"+OK\r\n");
        let info = info_persistence(&mut c);
        if info.contains("barrier_class:fua") {
            assert_eq!(info_field(&info, "frames_in_flight"), 4, "{info}");
            break;
        }
        assert!(Instant::now() < deadline, "never upgraded to fua after {writes} writes:\n{info}");
        #[allow(clippy::disallowed_methods)] // test harness thread, not cell code
        std::thread::sleep(Duration::from_millis(2));
    }
    let before = info_persistence(&mut c);
    let linked_before = info_field(&before, "fsyncs_linked");
    let fua_before = info_field(&before, "fsyncs_fua");
    // Eight concurrent always writers: frames seal every iteration while
    // earlier ones are still in flight — the pipeline fills.
    const WRITERS: u32 = 8;
    const PER_WRITER: u32 = 200;
    let handles: Vec<_> = (0..WRITERS)
        .map(|w| {
            let mut c = connect_use(&node, b"pipe");
            std::thread::spawn(move || {
                for i in 0..PER_WRITER {
                    let key = format!("p:{w}:{i}");
                    c.write_all(&cmd(&[b"SET", key.as_bytes(), b"through"])).expect("write");
                    read_exactly(&mut c, b"+OK\r\n");
                }
            })
        })
        .collect();
    for handle in handles {
        handle.join().expect("writer");
    }
    let after = info_persistence(&mut c);
    assert_eq!(info_field(&after, "fsyncs_linked"), linked_before, "every frame write-through");
    assert!(info_field(&after, "fsyncs_fua") > fua_before, "{after}");
    assert!(info_field(&after, "frames_in_flight_max") >= 2, "the pipeline filled:\n{after}");
    assert_eq!(info_field(&after, "frame_waits_barrier"), 0, "pure always never waits:\n{after}");
    assert_eq!(info_field(&after, "barrier_class_degraded"), 0);
    drop(c);
    node.stop();

    // Restart: every acked key is back; the pipeline config persists.
    let node = Node::start_durable_pipeline(1, &dir, 4);
    let mut c = connect_use(&node, b"pipe");
    for w in 0..WRITERS {
        for i in 0..PER_WRITER {
            let key = format!("p:{w}:{i}");
            c.write_all(&cmd(&[b"GET", key.as_bytes()])).expect("write");
            read_exactly(&mut c, b"$7\r\nthrough\r\n");
        }
    }
    let info = info_persistence(&mut c);
    assert!(info.contains("barrier_class:fua"), "{info}");
    assert_eq!(info_field(&info, "frames_in_flight"), 4);
    drop(c);
    node.stop();
    std::fs::remove_dir_all(&dir).ok();
}
