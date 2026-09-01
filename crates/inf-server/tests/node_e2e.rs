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
    /// Manual trigger with a tightened fill slice and stream pace, so a
    /// walk spans many MAINTAIN calls and a test can interleave foreground
    /// mutations with specific walk passes (the 2026-08-30 review's C4
    /// reproduction shape).
    Paced { slice_bytes: u32, stream_bytes_per_sec: u32 },
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
            CkptTrigger::Paced { slice_bytes, stream_bytes_per_sec } => inf_log::CkptConfig {
                interval_bytes: 0,
                slice_bytes,
                // One section per fill slice: every slice pays a real
                // section write before the next fill call runs, so
                // foreground commands interleave with every walk slice.
                section_bytes: slice_bytes,
                stream_bytes_per_sec,
                ..base
            },
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
            inf_server::DeviceConfig {
                model_share: model.share(cells),
                seal_barriers_per_s: 0,
                provenance: Default::default(),
            },
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
                        group: Default::default(),
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
                // A cell thread that exited during recovery panicked on
                // its boot error (fail-stop, §8.4): report that now, with
                // the refusal on stderr, instead of a 30 s timeout.
                assert!(
                    !node.handles.iter().any(std::thread::JoinHandle::is_finished),
                    "a cell thread exited during recovery (fail-stop — see stderr)"
                );
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

    // Review 2026-09-01 (INFINITYD_BIN compat lane): PUBSUB and PUBLISH
    // are plane pub/sub yet NOT in the Redis subscriber-mode allowlist —
    // the plane gate must refuse them like `execute`'s fast path does
    // (pre-fix, the node answered `PUBSUB CHANNELS` with the channel
    // list; oracle-pinned refusal byte shape).
    sub.write_all(&cmd(&[b"PUBSUB", b"CHANNELS"])).expect("write");
    read_exactly(
        &mut sub,
        b"-ERR Can't execute 'pubsub|channels': only (P|S)SUBSCRIBE / (P|S)UNSUBSCRIBE / \
          PING / QUIT / RESET are allowed in this context\r\n",
    );
    sub.write_all(&cmd(&[b"PUBLISH", b"ch", b"m"])).expect("write");
    let line = read_line(&mut sub);
    assert!(line.starts_with(b"-ERR Can't execute 'publish'"), "restricted PUBLISH: {line:?}");
    // The allowlist itself still passes: a further SUBSCRIBE works.
    sub.write_all(&cmd(&[b"SUBSCRIBE", b"ch2"])).expect("write");
    read_exactly(&mut sub, b"*3\r\n$9\r\nsubscribe\r\n$3\r\nch2\r\n:2\r\n");

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

/// Review of 2026-08-28 (M4.5-S37 finding 2): `DBSIZE` on a
/// namespace-bound connection is the **node-wide** count — the compat
/// matrix's `DBSIZE | full` — on a memory namespace, a flat durable one
/// and a tiered one, with keys spread over both cells of a two-cell
/// node; before, it answered the connection's cell alone. Asked from a
/// second connection too (whichever cell it lands on, the same count).
#[test]
fn namespace_bound_dbsize_counts_every_cell() {
    let dir = temp_data_dir("ns-dbsize");
    let node = Node::start_durable(2, &dir);
    let mut c = node.connect();
    let namespaces: [&[&[u8]]; 3] = [
        &[b"INF.NS", b"CREATE", b"cache", b"MODE", b"memory"],
        &[b"INF.NS", b"CREATE", b"ledger", b"MODE", b"durable", b"FSYNC", b"everysec"],
        &[
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
        ],
    ];
    for create in namespaces {
        c.write_all(&cmd(create)).expect("write");
        read_exactly(&mut c, b"+OK\r\n");
    }
    // Keys owned by each cell, ten per cell, so a per-cell answer would
    // read 10 and the node-wide one 20.
    let keys: Vec<Vec<u8>> = (0..10u32)
        .flat_map(|i| {
            let mut a = key_for_cell(2, 0);
            a.extend_from_slice(format!(":{i}").as_bytes());
            let mut b = key_for_cell(2, 1);
            b.extend_from_slice(format!(":{i}").as_bytes());
            [a, b]
        })
        .collect();
    for ns in [&b"cache"[..], b"ledger", b"hot"] {
        c.write_all(&cmd(&[b"INF.NS", b"USE", ns])).expect("write");
        read_exactly(&mut c, b"+OK\r\n");
        c.write_all(&cmd(&[b"DBSIZE"])).expect("write");
        read_exactly(&mut c, b":0\r\n");
        for key in &keys {
            c.write_all(&cmd(&[b"SET", key, b"v"])).expect("write");
            read_exactly(&mut c, b"+OK\r\n");
        }
        c.write_all(&cmd(&[b"DBSIZE"])).expect("write");
        read_exactly(&mut c, b":20\r\n");
        // Single-key deletes (a multi-key DEL spanning cells is the
        // recorded M2 limitation of named namespaces): one per cell + one.
        for key in &keys[..3] {
            c.write_all(&cmd(&[b"DEL", key])).expect("write");
            read_exactly(&mut c, b":1\r\n");
        }
        c.write_all(&cmd(&[b"DBSIZE"])).expect("write");
        read_exactly(&mut c, b":17\r\n");
        // A second connection, bound to the same namespace — the same
        // node-wide answer whichever cell accepted it.
        let mut other = connect_use(&node, ns);
        other.write_all(&cmd(&[b"DBSIZE"])).expect("write");
        read_exactly(&mut other, b":17\r\n");
    }
}

/// Reads one flat RESP array of bulk strings (the `KEYS` reply shape).
fn read_key_array(stream: &mut TcpStream) -> std::collections::BTreeSet<Vec<u8>> {
    let header = read_line(stream);
    assert_eq!(header.first(), Some(&b'*'), "KEYS reply shape: {header:?}");
    let count: usize =
        String::from_utf8_lossy(&header[1..header.len() - 2]).parse().expect("array length");
    (0..count).map(|_| read_bulk(stream)).collect()
}

/// Review of 2026-08-30 (full-codebase review C1 / F-L13-07): on a
/// namespace-bound connection of a multi-cell node, `SCAN`, `KEYS` and
/// `RANDOMKEY` cover **every** cell, and `FLUSHALL` deletes node-wide —
/// the same programs the default database rides, with `ApplyNs` legs.
/// Before the fix each served the connection's own cell and reported a
/// complete answer (`FLUSHALL` replied `+OK` having deleted 1/cells).
/// `FLUSHDB` keeps its honest typed refusal (ADR-0015): under a named
/// namespace it means "flush the namespace", which is not yet a thing.
#[test]
fn namespace_bound_scan_keys_flushall_cover_every_cell() {
    // A durable node whose only namespace is a memory one: DDL needs the
    // control plane, while FLUSHALL refuses only when a *durable*
    // namespace exists (exec.rs's ADR-0015 guard).
    let dir = temp_data_dir("ns-scan-flushall");
    let node = Node::start_durable(2, &dir);
    let mut c = node.connect();
    c.write_all(&cmd(&[b"INF.NS", b"CREATE", b"cache", b"MODE", b"memory"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    let mut c = connect_use(&node, b"cache");
    let cell0: std::collections::BTreeSet<Vec<u8>> = keys_for_cell(2, 0, 10).into_iter().collect();
    let cell1: std::collections::BTreeSet<Vec<u8>> = keys_for_cell(2, 1, 10).into_iter().collect();
    let expected: std::collections::BTreeSet<Vec<u8>> = cell0.union(&cell1).cloned().collect();
    for key in &expected {
        c.write_all(&cmd(&[b"SET", key, b"v"])).expect("write");
        read_exactly(&mut c, b"+OK\r\n");
    }
    // Keys in the *default* database too — FLUSHALL's blast radius.
    let mut plain = node.connect();
    for key in &expected {
        plain.write_all(&cmd(&[b"SET", key, b"db0"])).expect("write");
        read_exactly(&mut plain, b"+OK\r\n");
    }
    c.write_all(&cmd(&[b"DBSIZE"])).expect("write");
    read_exactly(&mut c, b":20\r\n");
    // SCAN and KEYS name the whole namespace — set equality, both ways
    // (the defect's signature was a complete-looking 1/cells answer).
    assert_eq!(scan_to_completion(&mut c, b"7"), expected, "SCAN covers every cell");
    c.write_all(&cmd(&[b"KEYS", b"*"])).expect("write");
    assert_eq!(read_key_array(&mut c), expected, "KEYS covers every cell");
    // RANDOMKEY draws from every cell's pool (200 draws: the chance of
    // missing one cell with both non-empty is 2⁻²⁰⁰-ish).
    let mut saw = (false, false);
    for _ in 0..200 {
        c.write_all(&cmd(&[b"RANDOMKEY"])).expect("write");
        let key = read_bulk(&mut c);
        saw.0 |= cell0.contains(&key);
        saw.1 |= cell1.contains(&key);
        assert!(expected.contains(&key), "RANDOMKEY named a foreign key: {key:?}");
    }
    assert!(saw.0 && saw.1, "RANDOMKEY drew from one cell's pool only: {saw:?}");
    // FLUSHDB: the honest refusal, nothing deleted.
    c.write_all(&cmd(&[b"FLUSHDB"])).expect("write");
    let refusal = read_line(&mut c);
    assert!(refusal.starts_with(b"-ERR"), "FLUSHDB refuses typed: {refusal:?}");
    c.write_all(&cmd(&[b"DBSIZE"])).expect("write");
    read_exactly(&mut c, b":20\r\n");
    // FLUSHALL: +OK means gone — from every cell, namespace and db0 both.
    c.write_all(&cmd(&[b"FLUSHALL"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    c.write_all(&cmd(&[b"DBSIZE"])).expect("write");
    read_exactly(&mut c, b":0\r\n");
    c.write_all(&cmd(&[b"KEYS", b"*"])).expect("write");
    read_exactly(&mut c, b"*0\r\n");
    plain.write_all(&cmd(&[b"DBSIZE"])).expect("write");
    read_exactly(&mut plain, b":0\r\n");
    drop(plain);
    drop(c);
    node.stop();
    std::fs::remove_dir_all(&dir).ok();
}

/// The C1 sweep's durable half: `SCAN`/`KEYS` on a namespace-bound
/// connection cover both cells of a flat durable namespace, and tiered
/// `SCAN` hops cells through the packed cursor (its per-cell walk was
/// the same 1/cells defect through `plane/tiered.rs`). Tiered `KEYS`/
/// `RANDOMKEY` keep their honest typed refusals.
#[test]
fn namespace_bound_scan_covers_durable_and_tiered_namespaces() {
    let dir = temp_data_dir("ns-scan-sweep");
    let node = Node::start_durable(2, &dir);
    let mut c = node.connect();
    let namespaces: [&[&[u8]]; 2] = [
        &[b"INF.NS", b"CREATE", b"ledger", b"MODE", b"durable", b"FSYNC", b"everysec"],
        &[
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
        ],
    ];
    for create in namespaces {
        c.write_all(&cmd(create)).expect("write");
        read_exactly(&mut c, b"+OK\r\n");
    }
    drop(c);
    let expected: std::collections::BTreeSet<Vec<u8>> =
        keys_for_cell(2, 0, 10).into_iter().chain(keys_for_cell(2, 1, 10)).collect();
    for ns in [&b"ledger"[..], b"hot"] {
        let mut c = connect_use(&node, ns);
        for key in &expected {
            c.write_all(&cmd(&[b"SET", key, b"v"])).expect("write");
            read_exactly(&mut c, b"+OK\r\n");
        }
        c.write_all(&cmd(&[b"DBSIZE"])).expect("write");
        read_exactly(&mut c, b":20\r\n");
        let label = String::from_utf8_lossy(ns).into_owned();
        assert_eq!(scan_to_completion(&mut c, b"7"), expected, "{label}: SCAN covers every cell");
        c.write_all(&cmd(&[b"KEYS", b"*"])).expect("write");
        if ns == b"hot" {
            let reply = read_line(&mut c);
            assert!(reply.starts_with(b"-ERR"), "{label}: tiered KEYS refuses typed: {reply:?}");
            c.write_all(&cmd(&[b"RANDOMKEY"])).expect("write");
            let reply = read_line(&mut c);
            assert!(reply.starts_with(b"-ERR"), "{label}: tiered RANDOMKEY refusal: {reply:?}");
        } else {
            assert_eq!(read_key_array(&mut c), expected, "{label}: KEYS covers every cell");
        }
    }
    node.stop();
    std::fs::remove_dir_all(&dir).ok();
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

/// Review of 2026-08-30 (H1 / F-L17-12 defect B, ADR-0098): an erroring
/// `CONFIG SET` must leave **every** cell unchanged. Pre-fix the local
/// leg applied pairs while validating them, then the error reply
/// suppressed the peer fan-out — cell 0 held `maxmemory 100mb` while
/// cells 1–3 held the default, permanently and silently.
#[test]
fn config_set_error_leaves_every_cell_unchanged() {
    let node = Node::start(4);
    let mut c = conn_on_cell(&node, 0);
    c.write_all(&cmd(&[b"CONFIG", b"SET", b"maxmemory", b"100mb", b"databases", b"32"]))
        .expect("write");
    let reply = read_line(&mut c);
    assert!(reply.starts_with(b"-ERR CONFIG SET failed"), "{:?}", String::from_utf8_lossy(&reply));
    // The cell-scope scrape (the S37 convention): CONFIG GET reads the
    // executing cell's replica, so every cell must report the default.
    for cell in 0..4 {
        let mut peer = conn_on_cell(&node, cell);
        peer.write_all(&cmd(&[b"CONFIG", b"GET", b"maxmemory"])).expect("write");
        read_exactly(&mut peer, b"*2\r\n$9\r\nmaxmemory\r\n$1\r\n0\r\n");
    }
    node.stop();
}

/// Review of 2026-08-30 (H1 / F-L13-01, ADR-0098): a `CONFIG SET` whose
/// argv exceeds the fabric codec's `MAX_APPLY_ARGS` (8 pairs = 18
/// args, over the 16-slice cap) must still reach every peer. Pre-fix
/// every peer leg's
/// `send_apply` refusal was silently swallowed: the reply was `+OK`,
/// the local cell applied, and the peers never saw the command.
#[test]
fn config_set_eight_pairs_reaches_every_cell() {
    let node = Node::start(2);
    let mut c = conn_on_cell(&node, 0);
    c.write_all(&cmd(&[
        b"CONFIG",
        b"SET",
        b"maxmemory",
        b"64mb",
        b"maxmemory-policy",
        b"allkeys-lru",
        b"maxmemory-samples",
        b"7",
        b"proto-max-bulk-len",
        b"268435456",
        b"tcp-keepalive",
        b"200",
        b"timeout",
        b"30",
        b"tiered-promote-on-read",
        b"no",
        b"save",
        b"900 1",
    ]))
    .expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    for cell in 0..2 {
        let mut peer = conn_on_cell(&node, cell);
        peer.write_all(&cmd(&[b"CONFIG", b"GET", b"maxmemory"])).expect("write");
        read_exactly(&mut peer, b"*2\r\n$9\r\nmaxmemory\r\n$8\r\n67108864\r\n");
        peer.write_all(&cmd(&[b"CONFIG", b"GET", b"maxmemory-policy"])).expect("write");
        read_exactly(&mut peer, b"*2\r\n$16\r\nmaxmemory-policy\r\n$11\r\nallkeys-lru\r\n");
        peer.write_all(&cmd(&[b"CONFIG", b"GET", b"timeout"])).expect("write");
        read_exactly(&mut peer, b"*2\r\n$7\r\ntimeout\r\n$2\r\n30\r\n");
    }
    node.stop();
}

/// Connects until landing on `cell`, then selects `ns` (retrying fresh
/// connections until the DDL fan reached that cell).
fn conn_on_cell_use(node: &Node, cell: u16, ns: &[u8]) -> TcpStream {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let mut c = conn_on_cell(node, cell);
        c.write_all(&cmd(&[b"INF.NS", b"USE", ns])).expect("write");
        if read_line(&mut c) == b"+OK\r\n" {
            return c;
        }
        assert!(Instant::now() < deadline, "USE never fanned to cell {cell}");
    }
}

/// Review of 2026-08-30 (H2 / F-L13-06, F-L17-11, ADR-0098): a
/// partially-failing `MSET` on a durable namespace. Pre-fix the first
/// pair applied (readable for hours), the error reply skipped durable
/// staging, and recovery silently rolled the key back while a later
/// acked write survived — the review's proven L2 breach. The fix makes
/// the command atomic (bounds validated before any pair applies), so
/// the live store and recovery agree on the pre-command state.
#[test]
fn durable_mset_bounds_failure_is_atomic_across_recovery() {
    let dir = temp_data_dir("mset-atomic");
    let node = Node::start_durable(1, &dir);
    let mut c = node.connect();
    c.write_all(&cmd(&[b"INF.NS", b"CREATE", b"pay", b"MODE", b"durable", b"FSYNC", b"always"]))
        .expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    c.write_all(&cmd(&[b"INF.NS", b"USE", b"pay"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    c.write_all(&cmd(&[b"SET", b"a", b"old"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    let long_key = vec![b'k'; 256];
    c.write_all(&cmd(&[b"MSET", b"a", b"new", &long_key, b"v"])).expect("write");
    read_exactly(&mut c, b"-ERR key or value exceeds InfinityDB M0 record bounds\r\n");
    // Atomic: the error reply implies zero mutation (pre-fix: "new").
    c.write_all(&cmd(&[b"GET", b"a"])).expect("write");
    read_exactly(&mut c, b"$3\r\nold\r\n");
    // The log stays live and healthy — a later write acks durably.
    c.write_all(&cmd(&[b"SET", b"z", b"9"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    drop(c);
    node.stop();

    // Recovery agrees with everything the client observed.
    let node = Node::start_durable(1, &dir);
    let mut c = node.connect();
    c.write_all(&cmd(&[b"INF.NS", b"USE", b"pay"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    c.write_all(&cmd(&[b"GET", b"a"])).expect("write");
    read_exactly(&mut c, b"$3\r\nold\r\n");
    c.write_all(&cmd(&[b"GET", b"z"])).expect("write");
    read_exactly(&mut c, b"$1\r\n9\r\n");
    c.write_all(&cmd(&[b"DBSIZE"])).expect("write");
    read_exactly(&mut c, b":2\r\n");
    drop(c);
    node.stop();
    std::fs::remove_dir_all(&dir).ok();
}

/// The `mset_midway_oom` crash-matrix row, local-pump site (review of
/// 2026-08-30, H2 / F-L17-11, ADR-0098): when a multi-key write genuinely
/// applies a prefix and then fails (arena OOM — bounds are pre-validated
/// now, so the fault point is the deterministic stand-in), the applied
/// prefix must be durably staged despite the error reply. Pre-fix the
/// emission gate read the reply's first byte, so the prefix was applied
/// in RAM, never logged, and silently rolled back by recovery.
#[test]
fn durable_mset_midway_failure_stages_what_it_wrote() {
    let dir = temp_data_dir("mset-midway");
    let node = Node::start_durable_with_faults(
        1,
        &dir,
        vec![(inf_server::fault::MSET_MIDWAY_OOM, inf_foundation::fault::FaultSpec::Nth(1))],
    );
    let mut c = node.connect();
    c.write_all(&cmd(&[b"INF.NS", b"CREATE", b"pay", b"MODE", b"durable", b"FSYNC", b"always"]))
        .expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    c.write_all(&cmd(&[b"INF.NS", b"USE", b"pay"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    c.write_all(&cmd(&[b"SET", b"a", b"old"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    // Pair 1 applies; the armed point fails pair 2.
    c.write_all(&cmd(&[b"MSET", b"a", b"new", b"b", b"vb"])).expect("write");
    read_exactly(&mut c, b"-OOM command not allowed when used memory > 'maxmemory'.\r\n");
    c.write_all(&cmd(&[b"GET", b"a"])).expect("write");
    read_exactly(&mut c, b"$3\r\nnew\r\n"); // the prefix is live and read-visible
    c.write_all(&cmd(&[b"GET", b"b"])).expect("write");
    read_exactly(&mut c, b"$-1\r\n");
    drop(c);
    node.stop();

    // Recovery must replay exactly the live store — never roll back a
    // read-visible key (pre-fix: `a` came back as "old").
    let node = Node::start_durable(1, &dir);
    let mut c = node.connect();
    c.write_all(&cmd(&[b"INF.NS", b"USE", b"pay"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    c.write_all(&cmd(&[b"GET", b"a"])).expect("write");
    read_exactly(&mut c, b"$3\r\nnew\r\n");
    c.write_all(&cmd(&[b"GET", b"b"])).expect("write");
    read_exactly(&mut c, b"$-1\r\n");
    drop(c);
    node.stop();
    std::fs::remove_dir_all(&dir).ok();
}

/// The same staged-prefix contract on the fabric path (`ApplyNs`, the
/// owner-side emission gate at `Shared::execute_ns_owned`): a hashtag
/// routes both pairs to one remote owner, the owner applies pair 1,
/// the armed point fails pair 2, and the origin's client sees the error
/// while the owner's staged prefix survives restart.
#[test]
fn durable_mset_midway_failure_stages_on_the_fabric_path() {
    let dir = temp_data_dir("mset-midway-fabric");
    let node = Node::start_durable_with_faults(
        2,
        &dir,
        vec![(inf_server::fault::MSET_MIDWAY_OOM, inf_foundation::fault::FaultSpec::Nth(1))],
    );
    let mut boot = node.connect();
    boot.write_all(&cmd(&[b"INF.NS", b"CREATE", b"pay", b"MODE", b"durable", b"FSYNC", b"always"]))
        .expect("write");
    read_exactly(&mut boot, b"+OK\r\n");
    drop(boot);
    // Both keys share the `{t}` hashtag slot; connect on the other cell
    // so the MSET rides `ApplyNs` to the owner.
    let router = SlotRouter::new_contiguous(2);
    let owner = router.cell_of(SlotRouter::slot_of(b"{t}a")).0;
    let mut c = conn_on_cell_use(&node, 1 - owner, b"pay");
    c.write_all(&cmd(&[b"SET", b"{t}a", b"old"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    c.write_all(&cmd(&[b"MSET", b"{t}a", b"new", b"{t}b", b"vb"])).expect("write");
    read_exactly(&mut c, b"-OOM command not allowed when used memory > 'maxmemory'.\r\n");
    c.write_all(&cmd(&[b"GET", b"{t}a"])).expect("write");
    read_exactly(&mut c, b"$3\r\nnew\r\n");
    c.write_all(&cmd(&[b"GET", b"{t}b"])).expect("write");
    read_exactly(&mut c, b"$-1\r\n");
    drop(c);
    node.stop();

    let node = Node::start_durable(2, &dir);
    let mut c = connect_use(&node, b"pay");
    c.write_all(&cmd(&[b"GET", b"{t}a"])).expect("write");
    read_exactly(&mut c, b"$3\r\nnew\r\n");
    c.write_all(&cmd(&[b"GET", b"{t}b"])).expect("write");
    read_exactly(&mut c, b"$-1\r\n");
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

/// Review of 2026-09-01 (found by Group 0 item 3 — the widened DST
/// value generator; N1 in the review report): `fetch_extent` passed its
/// accumulator straight to `tier_extract`, which **replaces** its
/// output's contents — so every continuation window erased the bytes
/// already assembled and the loop could never terminate. Any `GET` of a
/// blob value needing more than one cold window (> 16,368 data bytes)
/// spun device reads forever: no reply, no error, the connection's pump
/// held, unbounded foreground I/O. Client-reachable on any namespace
/// with a sub-16 KiB `BLOB-THRESHOLD` (a public CREATE option), and at
/// the 16 MiB default by any tiered value over 16 MiB. Pre-fix this
/// test times out on the first big GET; post-fix all sizes round-trip.
#[test]
fn tiered_blob_get_spanning_multiple_cold_windows() {
    let dir = temp_data_dir("blob-multiwindow");
    let node = Node::start_durable(1, &dir);
    let mut c = node.connect();
    c.write_all(&cmd(&[
        b"INF.NS",
        b"CREATE",
        b"blobs",
        b"MODE",
        b"durable",
        b"MEM-BUDGET",
        b"8mb",
        b"DISK-BUDGET",
        b"64mb",
        b"BLOB-THRESHOLD",
        b"4kb",
    ]))
    .expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    c.write_all(&cmd(&[b"INF.NS", b"USE", b"blobs"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    // One-window (≤ 16,368), boundary-crossing, and four-window sizes:
    // the continuation loop must terminate for every one of them.
    for (name, len) in [(&b"one"[..], 16_000usize), (&b"cross"[..], 17_000), (&b"four"[..], 50_000)]
    {
        let value: Vec<u8> = (0..len).map(|i| b'a' + (i % 23) as u8).collect();
        c.write_all(&cmd(&[b"SET", name, &value])).expect("write");
        read_exactly(&mut c, b"+OK\r\n");
        c.write_all(&cmd(&[b"STRLEN", name])).expect("write");
        read_exactly(&mut c, format!(":{len}\r\n").as_bytes());
        c.write_all(&cmd(&[b"GET", name])).expect("write");
        let mut want = format!("${len}\r\n").into_bytes();
        want.extend_from_slice(&value);
        want.extend_from_slice(b"\r\n");
        read_exactly(&mut c, &want);
    }
    drop(c);
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

/// Reads one `SCAN` reply (`[next-cursor, [key…]]`) off the stream.
fn read_scan_page(stream: &mut TcpStream) -> (u64, Vec<Vec<u8>>) {
    let header = read_line(stream);
    assert_eq!(header, b"*2\r\n", "SCAN reply shape: {header:?}");
    let cursor_text = read_bulk(stream);
    let cursor: u64 =
        String::from_utf8_lossy(&cursor_text).parse().expect("SCAN cursor is a decimal u64");
    let count_line = read_line(stream);
    assert_eq!(count_line.first(), Some(&b'*'), "SCAN keys array: {count_line:?}");
    let count: usize = String::from_utf8_lossy(&count_line[1..count_line.len() - 2])
        .parse()
        .expect("array length");
    let keys = (0..count).map(|_| read_bulk(stream)).collect();
    (cursor, keys)
}

/// Drives `SCAN` to cursor 0 and returns every key named, as a set.
fn scan_to_completion(stream: &mut TcpStream, count: &[u8]) -> std::collections::BTreeSet<Vec<u8>> {
    let mut collected = std::collections::BTreeSet::new();
    let mut cursor: Vec<u8> = b"0".to_vec();
    for _ in 0..10_000 {
        stream.write_all(&cmd(&[b"SCAN", &cursor, b"COUNT", count])).expect("write");
        let (next, keys) = read_scan_page(stream);
        collected.extend(keys);
        if next == 0 {
            return collected;
        }
        cursor = next.to_string().into_bytes();
    }
    panic!("SCAN never returned cursor 0");
}

/// Review of 2026-08-30 (full-codebase review C2 / F-L07-05): `SCAN` on a
/// tiered namespace names **every** live key — including cold records
/// whose value overruns the 4-frame (~16 KiB) cold-read window. Before
/// the fix, `fetch_key` demanded the whole record from one window and
/// silently dropped the key while the cursor advanced: values from
/// ~16,368 bytes up to the blob threshold vanished from a "complete"
/// iteration (DBSIZE 920 / SCAN 373 in the review's reproduction) while
/// `GET` still served them. The small-value band pins the control: keys
/// inside the window were never affected.
#[test]
fn tiered_scan_names_every_cold_key_across_the_window() {
    let dir = temp_data_dir("tiered-scan-window");
    let big = 150usize; // 40 KB values — far past the 16,368-byte window
    let small = 150usize; // 50 B values — the always-worked control band
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
    let mut expected = std::collections::BTreeSet::new();
    for i in 0..big {
        let key = format!("big:{i:04}").into_bytes();
        let value = format!("B{i:04}:").into_bytes().repeat(6_667); // ~40 KB
        c.write_all(&cmd(&[b"SET", &key, &value])).expect("write");
        read_exactly(&mut c, b"+OK\r\n");
        expected.insert(key);
    }
    for i in 0..small {
        let key = format!("small:{i:04}").into_bytes();
        c.write_all(&cmd(&[b"SET", &key, &[b's'; 50]])).expect("write");
        read_exactly(&mut c, b"+OK\r\n");
        expected.insert(key);
    }
    c.write_all(&cmd(&[b"DBSIZE"])).expect("write");
    read_exactly(&mut c, format!(":{}\r\n", big + small).as_bytes());
    // ~6 MB against a 3 MB budget: poll MAINTAIN-driven demotion until
    // most of the fill is flushed AND released pages exist (records are
    // genuinely cold — a RAM-served SCAN would not exercise the window).
    let info_u64 = |c: &mut TcpStream, field: &str| {
        let tiering = info_text(c, b"tiering");
        tiering
            .lines()
            .find_map(|l| l.strip_prefix(field))
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(0)
    };
    let mut demoted = false;
    let mut last = (0u64, 0u64);
    for _ in 0..1000 {
        last = (
            info_u64(&mut c, "tiering_flush_confirmed_bytes:"),
            info_u64(&mut c, "tiering_region_decommit_pages:"),
        );
        if last.0 > 3 << 20 && last.1 > 0 {
            demoted = true;
            break;
        }
        #[allow(clippy::disallowed_methods)] // test harness thread, not cell code
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(demoted, "demotion never released (flushed {}, decommitted pages {})", last.0, last.1);
    // The defect's signature: the count is right and the contents are
    // wrong — so assert exact set equality, not cardinality alone.
    let cold_reads_before = info_u64(&mut c, "cold_reads_enqueued:");
    let collected = scan_to_completion(&mut c, b"64");
    let cold_reads = info_u64(&mut c, "cold_reads_enqueued:") - cold_reads_before;
    assert!(cold_reads > 0, "SCAN resolved no cold slot — the window path was not exercised");
    let missing: Vec<_> =
        expected.difference(&collected).map(|k| String::from_utf8_lossy(k).into_owned()).collect();
    let phantom: Vec<_> =
        collected.difference(&expected).map(|k| String::from_utf8_lossy(k).into_owned()).collect();
    assert!(
        missing.is_empty() && phantom.is_empty(),
        "SCAN vs DBSIZE: {} of {} named; missing {:?}…; phantom {:?}",
        collected.len(),
        expected.len(),
        &missing[..missing.len().min(5)],
        &phantom[..phantom.len().min(5)],
    );
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

/// Reads one GET reply: `Ok(body)` for a bulk, `Err(line)` for an error
/// reply, `Ok(empty)` is unreachable here (no test key is empty).
fn read_get(stream: &mut TcpStream) -> Result<Vec<u8>, String> {
    let header = read_line(stream);
    match header.first() {
        Some(&b'-') => Err(String::from_utf8_lossy(&header).into_owned()),
        Some(&b'$') => {
            let len: i64 = std::str::from_utf8(&header[1..header.len() - 2])
                .expect("ascii")
                .parse()
                .expect("bulk length");
            if len < 0 {
                return Ok(Vec::new()); // nil
            }
            let mut body = vec![0u8; len as usize + 2];
            stream.read_exact(&mut body).expect("bulk body");
            body.truncate(len as usize);
            Ok(body)
        }
        other => panic!("unexpected GET reply head {other:?}: {header:?}"),
    }
}

// ---- the paced-checkpoint fixture (Group 0, from the batch-2 traps) ----

/// One `INFO` integer field from this connection's cell. Scope caveats
/// apply: `Memory` is a node fold, `Tiering`/`Persistence` are
/// cell-scope — multiply by cells before comparing to node totals.
fn scrape_u64(c: &mut TcpStream, section: &[u8], field: &str) -> u64 {
    info_text(c, section)
        .lines()
        .find_map(|l| l.strip_prefix(field))
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

/// Waits until demotion has genuinely pushed records cold. Batch-1 trap
/// (review remediation, 2026-08-31): cold-ness must be **asserted**
/// (flush-confirmed bytes past `min_confirmed` AND decommitted pages),
/// never assumed from write volume — an e2e that "forces demotion" by
/// filling and then reads immediately measures the RAM path.
fn wait_demoted(c: &mut TcpStream, min_confirmed: u64) {
    for _ in 0..1500 {
        if scrape_u64(c, b"tiering", "tiering_flush_confirmed_bytes:") > min_confirmed
            && scrape_u64(c, b"tiering", "tiering_region_decommit_pages:") > 0
        {
            return;
        }
        #[allow(clippy::disallowed_methods)] // test harness thread, not cell code
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    panic!("demotion never pushed the records cold (confirmed > {min_confirmed} + decommit)");
}

/// The reusable paced-checkpoint driver (review remediation batch 2 →
/// Group 0): requests a checkpoint on every cell, then drives `pump`
/// between the walk's MAINTAIN slices — 10 ms pacing, because an unpaced
/// pump exhausts its whole schedule inside pass 0 and never overlaps the
/// later passes (the batch-2 harness trap this fixture exists to keep).
/// Returns the number of pump calls that landed inside the walk. The
/// node must be booted with `CkptTrigger::Paced` (with
/// `section_bytes == slice_bytes` a section is written per fill slice);
/// with any other trigger the walk completes inside one MAINTAIN call
/// and no schedule can interleave.
fn drive_paced_ckpt_with_pump(
    node: &Node,
    probe: &mut TcpStream,
    mut pump: impl FnMut(&mut TcpStream),
    timeout: Duration,
) -> u32 {
    let before = scrape_u64(probe, b"persistence", "ckpts_completed:");
    node.control.as_ref().expect("durable node").request_ckpt_all();
    let deadline = Instant::now() + timeout;
    let mut pumped = 0u32;
    loop {
        pump(probe);
        pumped += 1;
        #[allow(clippy::disallowed_methods)] // test harness thread, not cell code
        std::thread::sleep(std::time::Duration::from_millis(10));
        if scrape_u64(probe, b"persistence", "ckpts_completed:") > before {
            return pumped;
        }
        assert!(Instant::now() < deadline, "checkpoint never completed under the pump");
    }
}

/// Review of 2026-08-30 (C4 / F-L03-01 + C7 / F-L04-08, F-L14-02): the
/// checkpoint's 0x05 blob-reference walk must not lose a live entry when
/// foreground `DEL`s remove reference-map entries *below its cursor*
/// between MAINTAIN slices. Before the fix the pass-3 resume was a
/// positional `.skip(ordinal)` into the mutating `BTreeMap`: each
/// below-cursor removal shifted every later rank down one, the resume
/// stepped over one live entry, the checkpoint published without its
/// 0x05 row, and the next boot's orphan sweep unlinked the extent — a
/// `GET` of an acked, never-deleted key then failed forever.
///
/// The walk is paced (1 KiB fill slices) so pass 3 spans many MAINTAIN
/// calls, and the `DEL` pump runs on the same cell for the whole stream:
/// deletes land between pass-3 slices at the lowest-ranked addresses —
/// the exact adversarial schedule. After the fix (address-keyed resume)
/// the schedule is harmless by construction, so this test is
/// deterministic-green; before it, each in-window DEL dropped one
/// surviving key's extent (observed red: GET → ERR blob extent read
/// failed after reopen).
#[test]
fn blob_refs_survive_a_checkpoint_walk_racing_deletes() {
    let dir = temp_data_dir("blob-ckpt-del-race");
    let blobs = 600usize;
    let fillers = 2400usize;
    let blob_value = |i: usize| format!("V{i:04}!").into_bytes().repeat(700); // 4,200 B ≥ 4 KiB threshold
    let mut deleted = std::collections::BTreeSet::new();
    {
        let node = Node::start_with(
            1,
            Some(dir.clone()),
            CkptTrigger::Paced { slice_bytes: 1 << 10, stream_bytes_per_sec: 64 << 10 },
        );
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
            // Copy-forward off (a 100% dead-ratio trigger never fires):
            // compaction would otherwise relocate the cold blob
            // references to the RAM tail once the filler is deleted, and
            // pass 3 would have nothing to walk.
            b"COMPACTION-DEAD-RATIO",
            b"100",
        ]))
        .expect("write");
        read_exactly(&mut c, b"+OK\r\n");
        c.write_all(&cmd(&[b"INF.NS", b"USE", b"b"])).expect("write");
        read_exactly(&mut c, b"+OK\r\n");
        // Blob keys first: their 24-byte reference records take the lowest
        // addresses, so the DEL pump below always removes entries ranked
        // below any pass-3 cursor position.
        for i in 0..blobs {
            let key = format!("big:{i:04}");
            c.write_all(&cmd(&[b"SET", key.as_bytes(), &blob_value(i)])).expect("write");
            read_exactly(&mut c, b"+OK\r\n");
        }
        // Inline filler past the budget: forces demotion, so every blob
        // reference record is cold (below the walk watermark) and pass 3
        // owns all 600 entries.
        for i in 0..fillers {
            let key = format!("fill:{i:04}");
            c.write_all(&cmd(&[b"SET", key.as_bytes(), &[b'f'; 3000]])).expect("write");
            read_exactly(&mut c, b"+OK\r\n");
        }
        wait_demoted(&mut c, 3 << 20);
        // Clear the filler (compaction is off, so the cold blob refs stay
        // put): pass 1 then emits nothing and the paced walk's wall time
        // splits between pass 0 and pass 3 — the DEL pump lands between
        // pass-3 slices.
        for i in 0..fillers {
            let key = format!("fill:{i:04}");
            c.write_all(&cmd(&[b"DEL", key.as_bytes()])).expect("write");
            read_exactly(&mut c, b":1\r\n");
        }
        // The paced walk with the review's adversarial schedule: DEL from
        // the low end between the walk's MAINTAIN slices.
        let walk_started = Instant::now();
        let mut next_del = 0usize;
        let pumped = drive_paced_ckpt_with_pump(
            &node,
            &mut c,
            |c| {
                if next_del < 300 {
                    let key = format!("big:{next_del:04}");
                    c.write_all(&cmd(&[b"DEL", key.as_bytes()])).expect("write");
                    read_exactly(c, b":1\r\n");
                    deleted.insert(key.into_bytes());
                    next_del += 1;
                }
            },
            Duration::from_secs(30),
        );
        eprintln!(
            "walk took {:?}, {next_del} DELs landed during it ({pumped} pump rounds)",
            walk_started.elapsed()
        );
        assert!(next_del > 20, "the pump barely ran — the walk finished before the schedule");
        // Let everysec cover the DEL deaths, then stop without a further
        // checkpoint (a second walk would re-emit the intact RAM map and
        // mask the omission).
        #[allow(clippy::disallowed_methods)] // test harness thread, not cell code
        std::thread::sleep(std::time::Duration::from_millis(1500));
        drop(c);
        node.stop();
    }
    {
        // At-least-once floor on the published 0x05 section. The count
        // alone cannot prove correctness (the recorded falsifier run
        // emitted a *count-right, contents-wrong* section: 560 entries,
        // 19 live keys skipped, 19 dead/duplicate rows in their place) —
        // the GET sweep below is the contents oracle.
        let ick = dir.join("shard-0").join("ckpt").join("ckpt-000001.ick");
        let mut blob_entries = 0usize;
        let _ = inf_log::ckpt::read_ick_hybrid(
            &inf_log::fs::StdSegmentFs,
            &ick,
            inf_log::ckpt::IckReaderConfig::default(),
            |_| Ok::<(), ()>(()),
            |_| Ok(()),
            |_| Ok(()),
            |section| {
                blob_entries += section.len();
                Ok(())
            },
            |_| Ok(()),
        )
        .expect("published checkpoint validates");
        assert!(blob_entries > 0, "pass 3 emitted nothing — the setup lost its cold refs");
        eprintln!("published 0x05 entries: {blob_entries}");
    }
    // Reopen: replay = short 0x05 section + the tail. Pre-fix, the boot
    // sweep unlinks the never-emitted extent and its key errors forever.
    let node = Node::start_durable(1, &dir);
    let mut c = node.connect();
    c.write_all(&cmd(&[b"INF.NS", b"USE", b"b"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    // Drain the boot reclaim backlog (the deleted extents' deaths) so a
    // pre-fix run cannot pass by racing the unlink.
    let deadline = Instant::now() + Duration::from_secs(20);
    while scrape_u64(&mut c, b"tiering", "tiering_blob_reclaimable:") > 0 {
        assert!(Instant::now() < deadline, "boot reclaim backlog never drained");
        #[allow(clippy::disallowed_methods)] // test harness thread, not cell code
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    // Every surviving key's extent is referenced again (the recorded
    // falsifier run booted at live 541 < 560 survivors). `>=`, not `==`:
    // an acked everysec DEL that missed the last fsync legitimately
    // revives its key (and extent) at replay.
    let live = scrape_u64(&mut c, b"tiering", "tiering_blob_extents_live:");
    assert!(
        live >= (blobs - deleted.len()) as u64,
        "extents live after reopen ({live}) below the {} surviving blob keys",
        blobs - deleted.len()
    );
    let mut lost: Vec<String> = Vec::new();
    for i in 0..blobs {
        let key = format!("big:{i:04}");
        if deleted.contains(key.as_bytes()) {
            continue; // acked DELs may or may not have replayed (everysec)
        }
        c.write_all(&cmd(&[b"GET", key.as_bytes()])).expect("write");
        match read_get(&mut c) {
            Ok(body) if body == blob_value(i) => {}
            Ok(body) => lost.push(format!("{key}: served {} bytes", body.len())),
            Err(err) => lost.push(format!("{key}: {err}")),
        }
    }
    assert!(
        lost.is_empty(),
        "{} of {} surviving blob keys lost after the checkpoint/DEL race (first: {:?})",
        lost.len(),
        blobs - deleted.len(),
        &lost[..lost.len().min(5)]
    );
    drop(c);
    node.stop();
    std::fs::remove_dir_all(&dir).ok();
}

/// Review of 2026-08-30 (C2′ / F-L06-04 + F-L06-02's BUSY leg): a
/// failed cold read is a **typed error on every read command** — never
/// "the key is not there". Before the fix, `MGET` rendered
/// `Resolved::Fail` as a nil element and `EXISTS`/`TOUCH` skipped the
/// count, so the node answered *differently for the same key in the
/// same instant* depending on which command asked (`GET` → `-BUSY`,
/// `EXISTS` → `:0`, `MGET` → nil) — and `EXISTS` is exactly what a
/// cache-fill path uses to decide whether to overwrite. The
/// `cold_enqueue_full` fault point is the deterministic stand-in for a
/// saturated `ColdReads` queue (the review's `overflow_cap` scenario);
/// genuinely absent keys keep their miss shapes — a miss never reaches
/// the queue.
#[test]
fn tiered_cold_read_failure_is_typed_for_every_read_command() {
    let dir = temp_data_dir("tiered-cold-busy");
    let node = Node::start_durable_with_faults(
        1,
        &dir,
        vec![(inf_server::fault::COLD_ENQUEUE_FULL, inf_foundation::fault::FaultSpec::Always)],
    );
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
    let keys = 150usize;
    for i in 0..keys {
        let key = format!("big:{i:04}").into_bytes();
        let value = format!("B{i:04}:").into_bytes().repeat(6_667); // ~40 KB
        c.write_all(&cmd(&[b"SET", &key, &value])).expect("write");
        read_exactly(&mut c, b"+OK\r\n");
    }
    let info_u64 = |c: &mut TcpStream, field: &str| {
        info_text(c, b"tiering")
            .lines()
            .find_map(|l| l.strip_prefix(field))
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(0)
    };
    let mut demoted = false;
    for _ in 0..1000 {
        if info_u64(&mut c, "tiering_flush_confirmed_bytes:") > 3 << 20
            && info_u64(&mut c, "tiering_region_decommit_pages:") > 0
        {
            demoted = true;
            break;
        }
        #[allow(clippy::disallowed_methods)] // test harness thread, not cell code
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(demoted, "demotion never made a cold working set");
    // Per-key agreement: whatever GET answers, EXISTS/TOUCH/MGET must
    // agree — a key is served, absent, or *unreadable, typed*; the
    // defect's signature was GET erroring while the others said absent.
    let read_int_or_err = |c: &mut TcpStream| -> Result<i64, String> {
        let line = read_line(c);
        match line.first() {
            Some(&b'-') => Err(String::from_utf8_lossy(&line).into_owned()),
            Some(&b':') => Ok(std::str::from_utf8(&line[1..line.len() - 2])
                .expect("ascii")
                .parse()
                .expect("int")),
            other => panic!("unexpected reply head {other:?}: {line:?}"),
        }
    };
    let mut cold_failures = 0usize;
    let mut disagreements: Vec<String> = Vec::new();
    for i in 0..keys {
        let key = format!("big:{i:04}");
        c.write_all(&cmd(&[b"GET", key.as_bytes()])).expect("write");
        let get = read_get(&mut c);
        c.write_all(&cmd(&[b"EXISTS", key.as_bytes()])).expect("write");
        let exists = read_int_or_err(&mut c);
        c.write_all(&cmd(&[b"TOUCH", key.as_bytes()])).expect("write");
        let touch = read_int_or_err(&mut c);
        c.write_all(&cmd(&[b"MGET", key.as_bytes()])).expect("write");
        let mget_head = read_line(&mut c);
        let mget_err = mget_head.first() == Some(&b'-');
        if !mget_err {
            assert_eq!(mget_head, b"*1\r\n", "one-key MGET array");
            let _ = read_get(&mut c); // consume the element
        }
        match get {
            Ok(_) => {
                if exists != Ok(1) || touch != Ok(1) || mget_err {
                    disagreements.push(format!(
                        "{key}: GET served but EXISTS {exists:?} / TOUCH {touch:?} / MGET err {mget_err}"
                    ));
                }
            }
            Err(_) => {
                cold_failures += 1;
                if exists.is_ok() || touch.is_ok() || !mget_err {
                    disagreements.push(format!(
                        "{key}: GET failed typed but EXISTS {exists:?} / TOUCH {touch:?} / \
                         MGET err {mget_err} — unreadability rendered as absence"
                    ));
                }
            }
        }
    }
    assert!(cold_failures > 0, "no cold read failed — the BUSY leg was never exercised");
    assert!(
        disagreements.is_empty(),
        "{} of {keys} keys answered inconsistently across read commands (first: {:?})",
        disagreements.len(),
        &disagreements[..disagreements.len().min(3)]
    );
    // Absent keys keep their miss shapes: a miss never reaches the queue.
    c.write_all(&cmd(&[b"EXISTS", b"nosuch"])).expect("write");
    read_exactly(&mut c, b":0\r\n");
    c.write_all(&cmd(&[b"TOUCH", b"nosuch"])).expect("write");
    read_exactly(&mut c, b":0\r\n");
    c.write_all(&cmd(&[b"MGET", b"nosuch"])).expect("write");
    read_exactly(&mut c, b"*1\r\n$-1\r\n");
    // The SCAN page fails typed too (the F-L06-02 BUSY leg): with every
    // cold read refused, an iteration must surface an error — never a
    // "complete" enumeration missing the cold keys.
    let mut cursor: Vec<u8> = b"0".to_vec();
    let mut scan_errored = false;
    for _ in 0..10_000 {
        c.write_all(&cmd(&[b"SCAN", &cursor, b"COUNT", b"64"])).expect("write");
        let head = read_line(&mut c);
        if head.first() == Some(&b'-') {
            scan_errored = true;
            break;
        }
        assert_eq!(head, b"*2\r\n", "scan reply shape");
        let next = read_get(&mut c).expect("cursor bulk");
        let inner = read_line(&mut c);
        assert_eq!(inner.first(), Some(&b'*'), "keys array");
        let n: usize =
            std::str::from_utf8(&inner[1..inner.len() - 2]).expect("ascii").parse().expect("len");
        for _ in 0..n {
            let _ = read_get(&mut c);
        }
        if next == b"0" {
            break;
        }
        cursor = next;
    }
    assert!(
        scan_errored,
        "SCAN completed an iteration with every cold read refused — silent omission"
    );
    // The failures are scrapeable, not just per-reply (L10): the
    // always-on counter moved for every typed refusal above.
    assert!(
        info_u64(&mut c, "tiering_cold_read_errors:") as usize >= cold_failures,
        "tiering_cold_read_errors below the observed typed failures"
    );
    drop(c);
    node.stop();
    std::fs::remove_dir_all(&dir).ok();
}

/// Review of 2026-08-30 (C7 / F-L04-08; ADR-0096) over the wire: a
/// header-valid boot orphan — an extent file no durable artifact
/// references, the crashed-blob-write shape — is **quarantined** by the
/// first boot's MAINTAIN slice (renamed, counted, bytes intact) and
/// unlinked only by the next boot's second verdict. Before ADR-0096 the
/// first slice after boot destroyed the file outright, which turned any
/// upstream accounting omission into permanent loss. A referenced blob
/// key keeps serving through both lives.
#[test]
fn blob_boot_orphan_quarantines_for_one_life_then_reclaims() {
    let dir = temp_data_dir("blob-orphan-quarantine");
    let value = vec![0xAB_u8; 8 << 10];
    let orphan_id = inf_log::ExtentId(700_001);
    let info_u64 = |c: &mut TcpStream, field: &str| {
        info_text(c, b"tiering")
            .lines()
            .find_map(|l| l.strip_prefix(field))
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(0)
    };
    // Life 1: one referenced blob key.
    {
        let node = Node::start_durable(1, &dir);
        let mut c = node.connect();
        c.write_all(&cmd(&[
            b"INF.NS",
            b"CREATE",
            b"q",
            b"MODE",
            b"durable",
            b"FSYNC",
            b"always",
            b"MEM-BUDGET",
            b"3mb",
            b"BLOB-THRESHOLD",
            b"4kb",
        ]))
        .expect("write");
        read_exactly(&mut c, b"+OK\r\n");
        c.write_all(&cmd(&[b"INF.NS", b"USE", b"q"])).expect("write");
        read_exactly(&mut c, b"+OK\r\n");
        c.write_all(&cmd(&[b"SET", b"big:1", &value])).expect("write");
        read_exactly(&mut c, b"+OK\r\n");
        // A published checkpoint: the sweep seeds from the manifest
        // recovery path, so the next boot lists the cold directory.
        c.write_all(&cmd(&[b"INF.CKPT", b"WAIT"])).expect("write");
        read_exactly(&mut c, b"+OK\r\n");
        drop(c);
        node.stop();
    }
    // Between lives: plant a well-formed orphan extent in the namespace's
    // cold dir — the "extent durable, referencing frame lost" crash shape.
    let shard = dir.join("shard-0");
    let ns_dir = std::fs::read_dir(&shard)
        .expect("shard dir")
        .filter_map(|e| e.ok())
        .find(|e| e.file_name().to_string_lossy().starts_with("ns-"))
        .expect("tiered ns dir")
        .path();
    let ns_id: u32 = ns_dir
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_prefix("ns-"))
        .and_then(|n| n.parse().ok())
        .expect("ns id from dir name");
    {
        let fs = inf_log::fs::StdSegmentFs;
        let mut w = inf_log::ExtentWriter::create(
            &fs,
            &ns_dir,
            orphan_id,
            0,
            inf_log::NsId(ns_id),
            128,
            inf_log::TierIoMode::Buffered,
        )
        .expect("orphan create");
        w.append_chunk(&[0xEE; 128]).expect("orphan bytes");
        let _ = w.finish().expect("orphan seal");
    }
    // Life 2: the boot sweep quarantines the orphan — renamed, counted,
    // bytes intact — and the referenced key serves untouched.
    {
        let node = Node::start_durable(1, &dir);
        let mut c = node.connect();
        c.write_all(&cmd(&[b"INF.NS", b"USE", b"q"])).expect("write");
        read_exactly(&mut c, b"+OK\r\n");
        let deadline = Instant::now() + Duration::from_secs(20);
        while info_u64(&mut c, "tiering_blob_quarantined:") == 0 {
            assert!(Instant::now() < deadline, "the boot orphan was never quarantined");
            #[allow(clippy::disallowed_methods)] // test harness thread, not cell code
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(info_u64(&mut c, "tiering_blob_quarantine_revived:"), 0);
        c.write_all(&cmd(&[b"GET", b"big:1"])).expect("write");
        assert_eq!(read_get(&mut c).expect("serves"), value);
        drop(c);
        node.stop();
    }
    let fs = inf_log::fs::StdSegmentFs;
    assert_eq!(
        inf_log::list_quarantined_extent_ids(&fs, &ns_dir).expect("listing"),
        vec![orphan_id],
        "the twin holds the bytes through the life"
    );
    assert!(
        !inf_log::list_extent_ids(&fs, &ns_dir).expect("listing").contains(&orphan_id),
        "the orphan left the reachable listing"
    );
    // Life 3: the second verdict — still unreferenced — unlinks the twin.
    {
        let node = Node::start_durable(1, &dir);
        let mut c = node.connect();
        c.write_all(&cmd(&[b"INF.NS", b"USE", b"q"])).expect("write");
        read_exactly(&mut c, b"+OK\r\n");
        let deadline = Instant::now() + Duration::from_secs(20);
        while !inf_log::list_quarantined_extent_ids(&fs, &ns_dir).expect("listing").is_empty() {
            assert!(Instant::now() < deadline, "the second verdict never reclaimed the twin");
            #[allow(clippy::disallowed_methods)] // test harness thread, not cell code
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(info_u64(&mut c, "tiering_blob_quarantined:"), 0, "nothing new quarantined");
        c.write_all(&cmd(&[b"GET", b"big:1"])).expect("write");
        assert_eq!(read_get(&mut c).expect("serves"), value);
        drop(c);
        node.stop();
    }
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

// ---- C6 · RESP reply framing under hostile argument bytes ------------------
//
// Review 2026-08-30, finding **C6** (= `F-L00-25` / `F-L12-04`): raw client
// bytes were spliced into line-framed error replies, so one command's
// argument could open a *second* RESP frame and forge the reply to the next
// pipelined command. The sanitizer lives in `RespWriter` (ADR-0097); these
// tests are the node-level contract — the plane writes error replies from
// paths `execute` never sees (`CONFIG`, `CLIENT`, `INF.NS`, the subcommand
// dispatchers), so the class is only closed if it is closed *here*.

/// Length of one complete RESP2/RESP3 reply at `buf[at..]`, or `None` when
/// the bytes are incomplete. `Err` when they cannot be RESP at all — which
/// is itself the C6 signature (an injected payload leaves a ragged tail).
fn reply_len(buf: &[u8], at: usize) -> Result<Option<usize>, String> {
    let Some(&tag) = buf.get(at) else { return Ok(None) };
    let Some(rel) = buf[at..].windows(2).position(|w| w == b"\r\n") else { return Ok(None) };
    let line_end = at + rel;
    let head = &buf[at + 1..line_end];
    let count = || -> Result<i64, String> {
        std::str::from_utf8(head)
            .map_err(|_| format!("non-utf8 length at {at}"))?
            .parse::<i64>()
            .map_err(|_| format!("bad length {:?} at {at}", String::from_utf8_lossy(head)))
    };
    match tag {
        b'+' | b'-' | b':' | b',' | b'#' | b'(' | b'_' => Ok(Some(line_end + 2 - at)),
        b'$' | b'=' => {
            let n = count()?;
            if n < 0 {
                return Ok(Some(line_end + 2 - at));
            }
            let end = line_end + 2 + n as usize + 2;
            Ok(if end <= buf.len() { Some(end - at) } else { None })
        }
        b'*' | b'~' | b'>' | b'%' => {
            let n = count()?;
            if n < 0 {
                return Ok(Some(line_end + 2 - at));
            }
            let elements = if tag == b'%' { n * 2 } else { n };
            let mut cur = line_end + 2;
            for _ in 0..elements {
                match reply_len(buf, cur)? {
                    Some(len) => cur += len,
                    None => return Ok(None),
                }
            }
            Ok(Some(cur - at))
        }
        other => Err(format!("unknown RESP tag {:?} at {at}", other as char)),
    }
}

/// Splits `buf` into complete replies. `Err` names the first byte that is
/// not the start of a valid reply — a split reply always ends there.
fn split_replies(buf: &[u8]) -> Result<Vec<&[u8]>, String> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < buf.len() {
        match reply_len(buf, at)? {
            Some(len) => {
                out.push(&buf[at..at + len]);
                at += len;
            }
            None => return Err(format!("incomplete reply at byte {at}")),
        }
    }
    Ok(out)
}

/// Everything the server sends until it goes quiet for `quiet`.
fn read_until_quiet(stream: &mut TcpStream, quiet: Duration) -> Vec<u8> {
    stream.set_read_timeout(Some(quiet)).expect("timeout");
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(_) => break, // WouldBlock/TimedOut: the reply is complete
        }
    }
    stream.set_read_timeout(Some(Duration::from_secs(5))).expect("timeout");
    buf
}

/// The review's scenario, end to end on a real node: an application passes
/// user data to a command InfinityDB has not implemented yet (the common
/// case during the Redis-adoption story the product is built for) and
/// pipelines a read it cares about behind it. The user's bytes must not be
/// able to answer that read.
#[test]
fn a_command_argument_cannot_forge_the_reply_to_the_next_command() {
    let node = Node::start(2);
    let mut client = node.connect();

    client.write_all(&cmd(&[b"SET", b"session:victim", b"REAL-SESSION-TOKEN"])).expect("write");
    read_exactly(&mut client, b"+OK\r\n");

    // The payload is a complete RESP bulk reply wrapped in CRLFs: if it
    // reaches the wire verbatim, the client reads it as the *next* reply.
    let payload = b"hello\r\n$18\r\nFORGED-SESSION-TOK\r\n";
    let mut pipeline = Vec::new();
    pipeline.extend(cmd(&[b"LPUSH", b"mylist", payload])); // M5 command: unknown today
    pipeline.extend(cmd(&[b"GET", b"session:victim"]));
    client.write_all(&pipeline).expect("write");

    let raw = read_until_quiet(&mut client, Duration::from_millis(400));
    let replies = split_replies(&raw)
        .unwrap_or_else(|e| panic!("two commands produced un-framed bytes ({e}): {raw:?}"));
    assert_eq!(
        replies.len(),
        2,
        "two commands must produce exactly two replies, got {}: {:?}",
        replies.len(),
        String::from_utf8_lossy(&raw)
    );
    assert!(replies[0].starts_with(b"-ERR unknown command "), "{:?}", replies[0]);
    assert_eq!(
        replies[1],
        b"$18\r\nREAL-SESSION-TOKEN\r\n",
        "the client was handed a forged reply: {:?}",
        String::from_utf8_lossy(replies[1])
    );
    // Nothing may be left over: a leftover is a permanently desynced socket.
    client.write_all(&cmd(&[b"PING"])).expect("write");
    read_exactly(&mut client, b"+PONG\r\n");

    node.stop();
}

/// Class check, not site check: every command in the registry, plus every
/// subcommand dispatcher that formats client bytes into its error, answers
/// **exactly one** RESP reply when its arguments carry CR/LF. One fresh
/// connection per case, so a split cannot hide inside the next case.
#[test]
fn no_command_can_split_its_reply_with_hostile_argument_bytes() {
    let node = Node::start(2);
    // A complete `+INJECTED\r\n` reply framed by CRLFs: if any byte of it
    // reaches the wire unsanitized, `split_replies` sees two replies.
    const SPLIT: &[u8] = b"\r\n+INJECTED\r\n";

    let mut cases: Vec<(String, Vec<Vec<u8>>)> = Vec::new();
    let hostile = |suffix: &[u8]| -> Vec<u8> {
        let mut v = suffix.to_vec();
        v.extend_from_slice(SPLIT);
        v
    };
    // The unknown-command path: the payload as the command *name*, and as
    // an argument of an unknown command.
    cases.push(("unknown-name".into(), vec![hostile(b"BAD"), b"x".to_vec()]));
    cases.push(("unknown-arg".into(), vec![b"NOSUCHCMD".to_vec(), hostile(b"a")]));
    cases.push(("lone-lf-name".into(), vec![b"BAD\nLF".to_vec(), b"x".to_vec()]));
    cases.push(("lone-cr-name".into(), vec![b"BAD\rCR".to_vec(), b"x".to_vec()]));
    cases.push(("trailing-crlf-name".into(), vec![b"BAD\r\n".to_vec()]));
    // The subcommand dispatchers, which reply through the plane rather than
    // `execute` — every one of these split the reply before the fix.
    for (name, sub) in [
        (&b"DEBUG"[..], &b"NOPE"[..]),
        (b"CONFIG", b"NOPE"),
        (b"OBJECT", b"NOPE"),
        (b"CLIENT", b"NOPE"),
        (b"PUBSUB", b"NOPE"),
        (b"INF.NS", b"NOPE"),
        (b"INF.CKPT", b"NOPE"),
    ] {
        cases.push((
            format!("{}-subcommand", String::from_utf8_lossy(name)),
            vec![name.to_vec(), hostile(sub)],
        ));
    }
    cases.push((
        "config-set-param".into(),
        vec![b"CONFIG".to_vec(), b"SET".to_vec(), hostile(b"nope"), b"1".to_vec()],
    ));
    cases.push((
        "json-path".into(),
        vec![b"JSON.GET".to_vec(), b"nokey".to_vec(), hostile(b"$.a[")],
    ));
    cases.push(("ns-use-name".into(), vec![b"INF.NS".to_vec(), b"USE".to_vec(), hostile(b"nope")]));
    // And the whole registry with a hostile trailing argument: the point is
    // that no *future* error text can reopen the hole either.
    for meta in &inf_wire::COMMANDS {
        cases.push((
            format!("registry-{}", meta.name),
            vec![meta.name.as_bytes().to_vec(), hostile(b"z")],
        ));
    }

    let mut failures = Vec::new();
    for (label, argv) in &cases {
        let parts: Vec<&[u8]> = argv.iter().map(|a| a.as_slice()).collect();
        let mut client = node.connect();
        client.write_all(&cmd(&parts)).expect("write");
        let raw = read_until_quiet(&mut client, Duration::from_millis(120));
        match split_replies(&raw) {
            Err(e) => failures.push(format!(
                "{label}: reply is not whole frames ({e}): {:?}",
                String::from_utf8_lossy(&raw)
            )),
            Ok(replies) => {
                if replies.len() != 1 {
                    failures.push(format!(
                        "{label}: {} replies for one command: {:?}",
                        replies.len(),
                        String::from_utf8_lossy(&raw)
                    ));
                } else if replies[0] == b"+INJECTED\r\n" {
                    failures.push(format!("{label}: the reply IS the injected frame"));
                }
            }
        }
        // The connection must still be usable — sanitization, not closure.
        // (A `SUBSCRIBE` case leaves RESP2 subscriber mode, where `PING`
        // answers a two-element array, so assert framing, not bytes.)
        client.write_all(&cmd(&[b"PING"])).expect("write");
        let raw = read_until_quiet(&mut client, Duration::from_millis(120));
        match split_replies(&raw) {
            Ok(replies) if replies.len() == 1 => {}
            _ => failures.push(format!(
                "{label}: connection unusable after: {:?}",
                String::from_utf8_lossy(&raw)
            )),
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} hostile-argument cases split their reply:\n{}",
        failures.len(),
        cases.len(),
        failures.join("\n")
    );

    node.stop();
}

// ---------------------------------------------------------------------
// ADR-0101 — cross-cell pub/sub self-delivery order (review finding N4).
// ---------------------------------------------------------------------

/// A channel owned by `cell` under an N-cell contiguous router.
fn channel_for_cell(cells: u16, cell: u16) -> Vec<u8> {
    let router = SlotRouter::new_contiguous(cells);
    for i in 0..100_000u32 {
        let ch = format!("ch:{i}");
        if router.cell_of(SlotRouter::slot_of(ch.as_bytes())) == CellId(cell) {
            return ch.into_bytes();
        }
    }
    panic!("no channel found for cell {cell}");
}

/// `HELLO 3`, draining the reply map (ends with the empty modules array).
fn hello3(conn: &mut TcpStream) {
    conn.write_all(&cmd(&[b"HELLO", b"3"])).expect("write");
    let mut drained = Vec::new();
    let mut byte = [0u8; 1];
    while !drained.ends_with(b"*0\r\n") {
        conn.read_exact(&mut byte).expect("hello body");
        drained.push(byte[0]);
    }
}

fn bulk_frame(s: &[u8]) -> Vec<u8> {
    let mut out = format!("${}\r\n", s.len()).into_bytes();
    out.extend_from_slice(s);
    out.extend_from_slice(b"\r\n");
    out
}

/// RESP3 `>3 message ch payload`.
fn push_message(ch: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut out = b">3\r\n$7\r\nmessage\r\n".to_vec();
    out.extend(bulk_frame(ch));
    out.extend(bulk_frame(payload));
    out
}

/// RESP3 `>4 pmessage pattern ch payload`.
fn push_pmessage(pattern: &[u8], ch: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut out = b">4\r\n$8\r\npmessage\r\n".to_vec();
    out.extend(bulk_frame(pattern));
    out.extend(bulk_frame(ch));
    out.extend(bulk_frame(payload));
    out
}

/// `SUBSCRIBE ch` on a RESP3 connection, expecting subscription count `n`.
fn subscribe3(conn: &mut TcpStream, ch: &[u8], n: u32) {
    conn.write_all(&cmd(&[b"SUBSCRIBE", ch])).expect("write");
    let mut want = b">3\r\n$9\r\nsubscribe\r\n".to_vec();
    want.extend(bulk_frame(ch));
    want.extend_from_slice(format!(":{n}\r\n").as_bytes());
    read_exactly(conn, &want);
}

/// ADR-0101 D1–D4 (review finding N4): a RESP3 connection subscribed to a
/// channel a *remote* cell owns publishes to it. Redis order is the count
/// reply, then the publisher's own push. Pre-fix the owner's `INF.PUBFAN`
/// leg wrote the push into this connection before the fabric round-trip
/// returned `:1` — the frame permutation the node compat lane pinned.
#[test]
fn cross_cell_self_publish_reply_precedes_push() {
    let node = Node::start(2);
    let mut c = conn_on_cell(&node, 0);
    hello3(&mut c);
    let ch = channel_for_cell(2, 1);
    subscribe3(&mut c, &ch, 1);
    c.write_all(&cmd(&[b"PUBLISH", &ch, b"selfmsg"])).expect("write");
    let mut want = b":1\r\n".to_vec();
    want.extend(push_message(&ch, b"selfmsg"));
    read_exactly(&mut c, &want);
    // Channel and pattern frames both ride the reply: message, then
    // pmessage, after the count — Redis order (ADR-0010 §5).
    c.write_all(&cmd(&[b"PSUBSCRIBE", b"ch:*"])).expect("write");
    read_exactly(&mut c, b">3\r\n$10\r\npsubscribe\r\n$4\r\nch:*\r\n:2\r\n");
    c.write_all(&cmd(&[b"PUBLISH", &ch, b"both"])).expect("write");
    let mut want = b":2\r\n".to_vec();
    want.extend(push_message(&ch, b"both"));
    want.extend(push_pmessage(b"ch:*", &ch, b"both"));
    read_exactly(&mut c, &want);
    // An unsubscribed publisher on the same connection state is untagged
    // and unchanged: plain count reply.
    c.write_all(&cmd(&[b"UNSUBSCRIBE", &ch])).expect("write");
    let mut want = b">3\r\n$11\r\nunsubscribe\r\n".to_vec();
    want.extend(bulk_frame(&ch));
    want.extend_from_slice(b":1\r\n");
    read_exactly(&mut c, &want);
    c.write_all(&cmd(&[b"PUNSUBSCRIBE", b"ch:*"])).expect("write");
    read_exactly(&mut c, b">3\r\n$12\r\npunsubscribe\r\n$4\r\nch:*\r\n:0\r\n");
    c.write_all(&cmd(&[b"PUBLISH", &ch, b"nobody"])).expect("write");
    read_exactly(&mut c, b":0\r\n");
    drop(c);
    node.stop();
}

/// ADR-0101 D4: pipelined publishes to channels with two *different*
/// remote owners. Their fan legs and replies reach this cell in any
/// order; pairing by sequence keeps each reply followed by exactly its
/// own frames — `:1 push(a) :1 push(b)`, never a foreign push after the
/// wrong reply (the per-connection deferral alternative's failure).
#[test]
fn pipelined_self_publishes_pair_each_reply_with_its_own_frames() {
    let node = Node::start(4);
    let mut c = conn_on_cell(&node, 0);
    hello3(&mut c);
    let a = channel_for_cell(4, 1);
    let b = channel_for_cell(4, 2);
    subscribe3(&mut c, &a, 1);
    subscribe3(&mut c, &b, 2);
    for round in 0..20u32 {
        let ma = format!("a{round}").into_bytes();
        let mb = format!("b{round}").into_bytes();
        let mut wire = cmd(&[b"PUBLISH", &a, &ma]);
        wire.extend(cmd(&[b"PUBLISH", &b, &mb]));
        c.write_all(&wire).expect("write");
        let mut want = b":1\r\n".to_vec();
        want.extend(push_message(&a, &ma));
        want.extend_from_slice(b":1\r\n");
        want.extend(push_message(&b, &mb));
        read_exactly(&mut c, &want);
    }
    drop(c);
    node.stop();
}

/// ADR-0101 D3: the tag defers only the tagged connection's frames. A
/// second subscriber on the owner cell publishing the *same payload* on
/// the same channel while the first's publish is in flight loses
/// nothing: both frames reach both connections, every count is 2, and
/// the self-subscribed publisher's own push still follows its reply.
#[test]
fn foreign_publish_during_self_publish_is_not_swallowed() {
    let node = Node::start(2);
    let ch = channel_for_cell(2, 1);
    let mut remote = conn_on_cell(&node, 0);
    hello3(&mut remote);
    subscribe3(&mut remote, &ch, 1);
    let mut owner = conn_on_cell(&node, 1);
    hello3(&mut owner);
    subscribe3(&mut owner, &ch, 1);
    for _ in 0..20 {
        // Neither reply is read before both publishes are on the wire.
        owner.write_all(&cmd(&[b"PUBLISH", &ch, b"same"])).expect("write");
        remote.write_all(&cmd(&[b"PUBLISH", &ch, b"same"])).expect("write");
        let push = push_message(&ch, b"same");
        for conn in [&mut remote, &mut owner] {
            // Three frames: `:2` and two identical pushes, in one of the
            // two legal orders — the publisher's own push is contiguous
            // with its reply, so the frame after `:2` is always a push.
            let mut got = Vec::new();
            let mut byte = [0u8; 1];
            let total = 4 + 2 * push.len();
            while got.len() < total {
                conn.read_exact(&mut byte).expect("frames");
                got.push(byte[0]);
            }
            let mut a = b":2\r\n".to_vec();
            a.extend(&push);
            a.extend(&push);
            let mut b = push.clone();
            b.extend_from_slice(b":2\r\n");
            b.extend(&push);
            assert!(got == a || got == b, "frames: {:?}", String::from_utf8_lossy(&got));
        }
    }
    drop(remote);
    drop(owner);
    node.stop();
}

// ---------------------------------------------------------------------
// ADR-0100 — namespace drop residue (review C13 / F-L14-04).
// ---------------------------------------------------------------------

/// The value of key `k{i}` in the ADR-0100 tests: every even key is an
/// 8 KiB blob (above the 4 KiB `BLOB-THRESHOLD`, so it lands in
/// `ns-16/cold` as an extent file at SET time — real residue on disk);
/// odd keys stay inline.
fn cold_value(i: u32) -> Vec<u8> {
    if i.is_multiple_of(2) {
        let mut v = format!("blob{i}:").into_bytes();
        v.resize(8 << 10, b'x');
        v
    } else {
        format!("v{i}").into_bytes()
    }
}

/// Creates the tiered namespace `cold` (id 16 on a fresh directory),
/// writes `keys` values into it (half of them blob extents), fills the
/// ring until **every cell has demoted and flush-confirmed** cold bytes
/// (so the checkpoint carries a live-set section — the residue class the
/// `m4-tiered` DST found on the fix's first seed, invisible to a
/// checkpoint of RAM-only data), publishes a checkpoint on every cell so
/// each `MANIFEST` carries its tier section, and asserts every cell holds
/// cold files — the residue the tests are about must exist, or they
/// prove nothing.
fn seed_tiered_namespace_with_checkpoint(node: &Node, dir: &std::path::Path, keys: u32) {
    let mut c = node.connect();
    c.write_all(&cmd(&[
        b"INF.NS",
        b"CREATE",
        b"cold",
        b"MODE",
        b"durable",
        b"MEM-BUDGET",
        b"8mb",
        b"DISK-BUDGET",
        b"64mb",
        b"MUTABLE-FRACTION",
        b"100",
        b"BLOB-THRESHOLD",
        b"4kb",
    ]))
    .expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    c.write_all(&cmd(&[b"INF.NS", b"USE", b"cold"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    for i in 0..keys {
        c.write_all(&cmd(&[b"SET", format!("k{i}").as_bytes(), &cold_value(i)])).expect("write");
        read_exactly(&mut c, b"+OK\r\n");
    }
    // Ring fill: 3 KiB inline values (below the blob threshold) in
    // pipelined batches until both cells report flush-confirmed bytes.
    let mut scrapers: Vec<TcpStream> = (0..2).map(|cell| conn_on_cell(node, cell)).collect();
    let filler = vec![b'f'; 3 << 10];
    let mut batch = 0u32;
    loop {
        let flushed_everywhere = scrapers
            .iter_mut()
            .all(|s| scrape_u64(s, b"tiering", "tiering_flush_confirmed_bytes:") > 0);
        if flushed_everywhere {
            break;
        }
        assert!(batch < 128, "no cell demoted after {batch} batches of 200 × 3 KiB");
        let mut wire = Vec::new();
        for i in 0..200u32 {
            wire.extend(cmd(&[b"SET", format!("fill{batch}:{i}").as_bytes(), &filler]));
        }
        c.write_all(&wire).expect("write");
        for _ in 0..200 {
            read_exactly(&mut c, b"+OK\r\n");
        }
        batch += 1;
        #[allow(clippy::disallowed_methods)] // test harness thread, not cell code
        std::thread::sleep(Duration::from_millis(10));
    }
    c.write_all(&cmd(&[b"SELECT", b"0"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    c.write_all(&cmd(&[b"INF.CKPT", b"WAIT"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    for cell in 0..2u16 {
        let shard = dir.join(format!("shard-{cell}"));
        let manifest = inf_log::read_manifest(&inf_log::fs::StdSegmentFs, &shard)
            .expect("manifest reads")
            .expect("checkpoint published");
        assert!(
            manifest.tiers.iter().any(|t| t.ns == 16),
            "cell {cell}: MANIFEST names the tiered namespace before the drop"
        );
        assert!(
            tier_residue_files(dir, cell) > 0,
            "cell {cell}: cold files exist before the drop (the residue under test)"
        );
    }
}

/// Files under `shard-k/ns-16/cold` (0 when the directory is gone).
fn tier_residue_files(dir: &std::path::Path, cell: u16) -> usize {
    let cold = dir.join(format!("shard-{cell}")).join("ns-16").join("cold");
    match std::fs::read_dir(&cold) {
        Ok(entries) => entries.count(),
        Err(_) => 0,
    }
}

/// Polls `INFO persistence` until `field` reads `want` (the gauge
/// refreshes at MAINTAIN cadence), returning the last text.
fn wait_persistence_field(conn: &mut TcpStream, field: &str, want: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let text = info_text(conn, b"persistence");
        if text.contains(&format!("{field}:{want}\r\n")) || Instant::now() >= deadline {
            return text;
        }
        #[allow(clippy::disallowed_methods)] // test harness thread, not cell code
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// ADR-0100 (review C13 / F-L14-04): dropping a tiered namespace after a
/// checkpoint named it must leave a bootable directory even when no
/// checkpoint follows the drop. Pre-fix every reopen refused with
/// "MANIFEST carries a tier section for ns 16 the catalog does not know".
/// Post-fix: the catalog's tombstone explains the residue, recovery
/// sweeps it, a post-boot checkpoint plus one DDL persist retires the
/// tombstone, and a third boot reads a tombstone-free catalog.
#[test]
fn dropped_tiered_namespace_reboots_without_a_checkpoint() {
    let dir = temp_data_dir("drop-reboot");
    let node = Node::start_durable(2, &dir);
    seed_tiered_namespace_with_checkpoint(&node, &dir, 64);
    let mut c = node.connect();
    c.write_all(&cmd(&[b"INF.NS", b"DROP", b"cold"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    drop(c);
    node.stop();

    // No checkpoint since the drop: every MANIFEST still names ns 16.
    let node = Node::start_durable(2, &dir);
    let mut c = node.connect();
    let tiering = info_text(&mut c, b"tiering");
    assert!(tiering.contains("tiering_tables:0"), "{tiering}");
    for cell in 0..2 {
        assert_eq!(tier_residue_files(&dir, cell), 0, "cell {cell}: residue swept at boot");
    }
    let text = wait_persistence_field(&mut c, "ns_drop_tombstones", "1");
    assert!(text.contains("ns_drop_tombstones:1\r\n"), "survives the boot: {text}");
    // Retirement: every cell publishes a post-boot checkpoint (the boot
    // re-stamped the tombstone with one), then any DDL persist retires it.
    c.write_all(&cmd(&[b"INF.CKPT", b"WAIT"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    c.write_all(&cmd(&[b"INF.NS", b"CREATE", b"scratch", b"MODE", b"memory"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    let text = wait_persistence_field(&mut c, "ns_drop_tombstones", "0");
    assert!(text.contains("ns_drop_tombstones:0\r\n"), "retired: {text}");
    drop(c);
    node.stop();

    // Third boot: no tier section, no tombstone, no residue.
    let node = Node::start_durable(2, &dir);
    let mut c = node.connect();
    c.write_all(&cmd(&[b"PING"])).expect("write");
    read_exactly(&mut c, b"+PONG\r\n");
    let text = wait_persistence_field(&mut c, "ns_drop_tombstones", "0");
    assert!(text.contains("ns_drop_tombstones:0\r\n"), "{text}");
    drop(c);
    node.stop();
    std::fs::remove_dir_all(&dir).ok();
}

/// Reads every key `k0..keys` from namespace `cold` on `conn`.
fn assert_cold_keys_served(conn: &mut TcpStream, keys: u32) {
    conn.write_all(&cmd(&[b"INF.NS", b"USE", b"cold"])).expect("write");
    read_exactly(conn, b"+OK\r\n");
    for i in 0..keys {
        conn.write_all(&cmd(&[b"GET", format!("k{i}").as_bytes()])).expect("write");
        let value = cold_value(i);
        let mut want = format!("${}\r\n", value.len()).into_bytes();
        want.extend_from_slice(&value);
        want.extend_from_slice(b"\r\n");
        read_exactly(conn, &want);
    }
    conn.write_all(&cmd(&[b"SELECT", b"0"])).expect("write");
    read_exactly(conn, b"+OK\r\n");
}

/// Crash-matrix row `ns_drop_before_meta` (ADR-0100 D5): the DDL stops
/// after the local apply and before the catalog persist request — the
/// on-disk state of a cut before the swap. Nothing durable changed and
/// the teardown hold kept every tier file (the origin cell's registry
/// alone lacks the namespace, so the origin's own files are the ones
/// that would have been unlinked pre-ADR). A restart serves the
/// namespace with every key.
#[test]
fn dropped_tiered_namespace_survives_a_cut_before_its_swap() {
    let dir = temp_data_dir("drop-cut-before");
    let node = Node::start_durable_with_faults(
        2,
        &dir,
        vec![(inf_server::fault::NS_DROP_BEFORE_META, inf_foundation::fault::FaultSpec::Nth(1))],
    );
    seed_tiered_namespace_with_checkpoint(&node, &dir, 32);
    let residue_before: Vec<usize> = (0..2).map(|cell| tier_residue_files(&dir, cell)).collect();
    let mut c = node.connect();
    c.write_all(&cmd(&[b"INF.NS", b"DROP", b"cold"])).expect("write");
    read_exactly(&mut c, b"-ERR fault: ns_drop_before_meta\r\n");
    // Give MAINTAIN time to run its teardown slices: the hold must keep
    // every file since no catalog swap carried the drop.
    #[allow(clippy::disallowed_methods)] // test harness thread, not cell code
    std::thread::sleep(Duration::from_millis(200));
    for cell in 0..2 {
        assert_eq!(
            tier_residue_files(&dir, cell),
            residue_before[usize::from(cell)],
            "cell {cell}: the teardown hold kept every file (ADR-0100 D5)"
        );
    }
    drop(c);
    node.stop();

    let node = Node::start_durable(2, &dir);
    let mut c = node.connect();
    let tiering = info_text(&mut c, b"tiering");
    assert!(tiering.contains("tiering_tables:1"), "namespace restored whole: {tiering}");
    assert_cold_keys_served(&mut c, 32);
    let text = wait_persistence_field(&mut c, "ns_drop_tombstones", "0");
    assert!(text.contains("ns_drop_tombstones:0\r\n"), "no tombstone was ever written: {text}");
    drop(c);
    node.stop();
    std::fs::remove_dir_all(&dir).ok();
}

/// Crash-matrix row `ns_drop_after_meta` (ADR-0100 D6): the DDL stops
/// once the catalog swap is durable and before the fan — the on-disk
/// state of a cut after the swap. `META` lacks the namespace and carries
/// its tombstone while every `MANIFEST` still names it. The restart
/// boots (the pre-ADR fail-stop refused exactly this state), sweeps the
/// residue on every cell, and the namespace is gone.
#[test]
fn dropped_tiered_namespace_survives_a_cut_after_its_swap() {
    let dir = temp_data_dir("drop-cut-after");
    let node = Node::start_durable_with_faults(
        2,
        &dir,
        vec![(inf_server::fault::NS_DROP_AFTER_META, inf_foundation::fault::FaultSpec::Nth(1))],
    );
    seed_tiered_namespace_with_checkpoint(&node, &dir, 32);
    let mut c = node.connect();
    c.write_all(&cmd(&[b"INF.NS", b"DROP", b"cold"])).expect("write");
    read_exactly(&mut c, b"-ERR fault: ns_drop_after_meta\r\n");
    drop(c);
    node.stop();

    let node = Node::start_durable(2, &dir);
    let mut c = node.connect();
    let tiering = info_text(&mut c, b"tiering");
    assert!(tiering.contains("tiering_tables:0"), "the drop was durable: {tiering}");
    c.write_all(&cmd(&[b"INF.NS", b"USE", b"cold"])).expect("write");
    let refusal = read_line(&mut c);
    assert!(refusal.starts_with(b"-ERR"), "{refusal:?}");
    for cell in 0..2 {
        assert_eq!(tier_residue_files(&dir, cell), 0, "cell {cell}: residue swept (ADR-0100 D6)");
    }
    let text = wait_persistence_field(&mut c, "ns_drop_tombstones", "1");
    assert!(text.contains("ns_drop_tombstones:1\r\n"), "{text}");
    drop(c);
    node.stop();
    std::fs::remove_dir_all(&dir).ok();
}
