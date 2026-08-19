//! `infinityd` — the InfinityDB node (M0 assembly): N pinned shard cells,
//! each a complete miniature database (reactor + uring/kqueue driver + wire
//! parser + executor + store slice + fabric endpoint), one `SO_REUSEPORT`
//! listener per cell (master plan §4/§5).
//!
//! M0 surface: flags only, no config file (anti-goal); no signal handling —
//! there is no durable state before M2, so the OS reclaiming the process IS
//! clean shutdown. `--route-local-only` is the cross-cell penalty A/B leg
//! (§6 gate): the router treats every key as local to the accepting cell.
#![forbid(unsafe_code)]

use std::os::fd::IntoRawFd;
use std::rc::Rc;

use inf_alloc::BufferPool;
use inf_fabric::{CellFabric, Mesh, MeshConfig};
use inf_foundation::CellId;
use inf_foundation::time::{Clock, StdClock};
use inf_runtime::net::{bound_port, listen_reuseport, pin_current_thread};
use inf_runtime::{BackendDriver, CellLoop, LoopConfig};
use inf_server::{NodeInfo, NoopObserver, ServerPlane, StdSegmentFs};
use inf_store::{Keyspace, StoreConfig};

/// How often (iterations) each cell refreshes its INFO stats snapshot.
const STATS_EVERY: u64 = 1024;

#[derive(Clone, Debug)]
struct Args {
    port: u16,
    cells: u16,
    buffers: usize,
    buf_size: usize,
    pin_start: Option<usize>,
    route_local_only: bool,
    park_us: Option<u64>,
    /// Durable-plane root (M2-S08/S11): `--data-dir` enables the catalog,
    /// per-cell log recovery, checkpoints, and truncation. Absent = the
    /// memory-only node (the M2-S09 zero-cost posture).
    data_dir: Option<std::path::PathBuf>,
    /// Bytes-appended checkpoint trigger (0 = manual/`INF.CKPT` only).
    ckpt_interval_bytes: u64,
    /// Segment prealloc/seal size.
    segment_bytes: u32,
    /// Durability fsyncs in flight per cell (M2.5-S07 A/B knob): 1 = the
    /// ADR-0022 D3 discipline, 2 = the bounded two-in-flight pipeline.
    sync_pipeline: u8,
    /// Per-buffer log-staging capacity in MiB (M4.5-S27, ADR-0083 D3).
    /// The buffer absorbs `arrival_rate × frame-write stall`; the 4 MiB
    /// default is ~8.5 ms at 470 MB/s. With ADR-0083 D1 pacing the bound
    /// never refuses — it is a pacing point — so this is a latency/memory
    /// trade (resident = 2 × capacity × cells), and shrinking it is the
    /// deliberate way to provoke the pressure regime on a healthy device.
    log_staging_mib: u32,
    /// M2.5-S21 A/B knob: publish staged fabric ops at the head of
    /// MAINTAIN so the hop RTT overlaps local execution.
    early_fabric_flush: bool,
    /// M2.5-S21 A/B knob: resume reply-woken pumps and publish their remote
    /// ops before PARSE+EXECUTE (the overlap-loss discriminator, ADR-0027).
    remote_first_execute: bool,
    /// M2.5 Phase-H fabric-apply staged prefetch (ADR-0030, ADR-0005
    /// shape): FABRIC-IN stages drained applies and prefetches the batch's
    /// store lines before executing. Default ON (binding A/B: penalty
    /// 58.8% -> 54.6-55.7%, anchor 1.61x -> 1.74-1.78x);
    /// `--no-fabric-apply-prefetch` is the A/B off arm.
    fabric_apply_prefetch: bool,
    /// M2.5 Phase-H parse-batch staged prefetch (ADR-0029 lever 2 / ADR-0033
    /// — the ADR-0005 shape on the parse loop's local fast path). Default ON
    /// (binding A/B: all-local 6.48/6.63M -> 7.98/8.22M, +23-27%, zero arm
    /// overlap; natural flat; anchor intact);
    /// `--no-parse-batch-prefetch` is the A/B off arm.
    parse_batch_prefetch: bool,
    /// M2.5 Phase-H de-async dispatch (ADR-0030 D4 lever): the pump tries
    /// a synchronous fast path per command (single-owner remote Apply,
    /// local mirror) before constructing the `dispatch_one` future.
    /// **Rejected by A/B** (2026-07-10, ADR-0034): ~+1.5% natural vs the
    /// ≥ +4% floor — the async machinery was already near-zero-cost (L6).
    /// Kept default-off as the A/B instrument for the S19 8-cell re-read.
    deasync_dispatch: bool,
}

impl Default for Args {
    fn default() -> Args {
        Args {
            port: 6379,
            cells: 4,
            buffers: 4096,
            buf_size: 4096,
            pin_start: None,
            route_local_only: false,
            park_us: None,
            data_dir: None,
            ckpt_interval_bytes: inf_server::DEFAULT_CKPT_INTERVAL_BYTES,
            segment_bytes: inf_server::DEFAULT_SEGMENT_BYTES,
            sync_pipeline: 1,
            log_staging_mib: 4,
            early_fabric_flush: false,
            remote_first_execute: false,
            fabric_apply_prefetch: true,
            parse_batch_prefetch: true,
            deasync_dispatch: false,
        }
    }
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args::default();
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut take = |name: &str| it.next().ok_or_else(|| format!("{name} requires a value"));
        match flag.as_str() {
            "--port" => args.port = take("--port")?.parse().map_err(|e| format!("--port: {e}"))?,
            "--cells" => {
                args.cells = take("--cells")?.parse().map_err(|e| format!("--cells: {e}"))?;
            }
            "--buffers" => {
                args.buffers = take("--buffers")?.parse().map_err(|e| format!("--buffers: {e}"))?;
            }
            "--buf-size" => {
                args.buf_size =
                    take("--buf-size")?.parse().map_err(|e| format!("--buf-size: {e}"))?;
            }
            "--pin-start" => {
                args.pin_start =
                    Some(take("--pin-start")?.parse().map_err(|e| format!("--pin-start: {e}"))?);
            }
            "--route-local-only" => args.route_local_only = true,
            "--early-fabric-flush" => args.early_fabric_flush = true,
            "--remote-first-execute" => args.remote_first_execute = true,
            "--fabric-apply-prefetch" => args.fabric_apply_prefetch = true,
            "--no-fabric-apply-prefetch" => args.fabric_apply_prefetch = false,
            "--parse-batch-prefetch" => args.parse_batch_prefetch = true,
            "--no-parse-batch-prefetch" => args.parse_batch_prefetch = false,
            "--deasync-dispatch" => args.deasync_dispatch = true,
            "--no-deasync-dispatch" => args.deasync_dispatch = false,
            "--park-us" => {
                args.park_us =
                    Some(take("--park-us")?.parse().map_err(|e| format!("--park-us: {e}"))?);
            }
            "--data-dir" => args.data_dir = Some(take("--data-dir")?.into()),
            "--ckpt-interval-bytes" => {
                args.ckpt_interval_bytes = take("--ckpt-interval-bytes")?
                    .parse()
                    .map_err(|e| format!("--ckpt-interval-bytes: {e}"))?;
            }
            "--segment-bytes" => {
                args.segment_bytes = take("--segment-bytes")?
                    .parse()
                    .map_err(|e| format!("--segment-bytes: {e}"))?;
            }
            "--sync-pipeline" => {
                args.sync_pipeline = take("--sync-pipeline")?
                    .parse()
                    .map_err(|e| format!("--sync-pipeline: {e}"))?;
                if !(1..=2).contains(&args.sync_pipeline) {
                    return Err("--sync-pipeline is 1 or 2, never a queue".into());
                }
            }
            "--log-staging-mib" => {
                args.log_staging_mib = take("--log-staging-mib")?
                    .parse()
                    .map_err(|e| format!("--log-staging-mib: {e}"))?;
                // 1 MiB holds any wire-legal command's record; 64 MiB is
                // the frame decoder bound (every written frame must stay
                // readable by a default-configured reader).
                if !(1..=64).contains(&args.log_staging_mib) {
                    return Err("--log-staging-mib is 1..=64 (the frame decoder bound)".into());
                }
            }
            "--version" | "-V" => {
                println!("{}", version_line());
                std::process::exit(0);
            }
            "--help" | "-h" => {
                println!(
                    "infinityd [--port 6379] [--cells 4] [--buffers 4096] [--buf-size 4096] \
                     [--pin-start CORE] [--route-local-only] [--data-dir PATH] \
                     [--ckpt-interval-bytes N] [--segment-bytes N] [--sync-pipeline 1|2] \
                     [--log-staging-mib 4] [--early-fabric-flush] [--remote-first-execute] \
                     [--fabric-apply-prefetch|--no-fabric-apply-prefetch] \
                     [--parse-batch-prefetch|--no-parse-batch-prefetch] \
                     [--deasync-dispatch|--no-deasync-dispatch] [--version]"
                );
                std::process::exit(0);
            }
            other => return Err(format!("unknown flag {other}")),
        }
    }
    if args.cells == 0 {
        return Err("--cells must be >= 1".into());
    }
    Ok(args)
}

/// Build provenance (M1-S14): version + commit + target, stamped by
/// `build.rs`. The release pipeline owns `INF_VERSION` via the tag.
fn version_line() -> String {
    format!(
        "infinityd {} (git {}, {})",
        env!("INF_VERSION"),
        env!("INF_GIT_SHA"),
        env!("INF_BUILD_TARGET")
    )
}

fn main() {
    let args = match parse_args() {
        Ok(args) => args,
        Err(e) => {
            eprintln!("infinityd: {e}");
            std::process::exit(2);
        }
    };
    // `fabrics` is only borrowed mutably to install eventfd wakeups, which is
    // Linux-only (see the cfg block below); on other targets the binding is
    // consumed by `into_iter()` and never needs `mut`.
    #[cfg_attr(not(target_os = "linux"), allow(unused_mut))]
    let mut fabrics = Mesh::new(args.cells, MeshConfig { ring_capacity: 4096, data_credits: 1024 });

    // Doorbell wakeups (M0-R1, Linux): each cell adopts an eventfd watch;
    // peers wake a parked cell through the park board + LoopWaker. The dev
    // tier (kqueue) falls back to the park-timeout ceiling.
    let park_flags: std::sync::Arc<Vec<std::sync::atomic::AtomicBool>> = std::sync::Arc::new(
        (0..args.cells).map(|_| std::sync::atomic::AtomicBool::new(false)).collect(),
    );
    #[cfg(target_os = "linux")]
    let mut wake_fds = Vec::new();
    #[cfg(target_os = "linux")]
    {
        let mut wakers = Vec::new();
        for _ in 0..args.cells {
            let (fd, waker) = inf_runtime::net::wake_pair().expect("eventfd");
            wake_fds.push(Some(fd));
            wakers.push(waker);
        }
        for fabric in &mut fabrics {
            let wakers = wakers.clone();
            fabric.set_wakeups(std::sync::Arc::clone(&park_flags), move |cell| {
                wakers[usize::from(cell.0)].wake();
            });
        }
    }

    // Durable boot order (M2-S08, ADR-0015 D3 — the node_e2e reference):
    // catalog before cells (the id→definition map must exist before any
    // cell replays records naming ids), control thread as the catalog's
    // single writer; each cell then recovers its own log before serving.
    let boot = args.data_dir.clone().map(|dir| {
        let catalog = match inf_server::load_catalog(&dir) {
            Ok(catalog) => catalog,
            Err(e) => {
                eprintln!("infinityd: catalog load failed (fail-stop, §8.4): {e}");
                std::process::exit(1);
            }
        };
        let boot_unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let control =
            inf_server::spawn_control(dir.clone(), catalog.as_ref(), args.cells, boot_unix_ms);
        (dir, catalog, control)
    });

    let mut handles = Vec::new();
    for (i, fabric) in fabrics.into_iter().enumerate() {
        let args = args.clone();
        let boot = boot.clone();
        let park_flags = std::sync::Arc::clone(&park_flags);
        #[cfg(target_os = "linux")]
        let wake_fd = wake_fds[i].take();
        #[cfg(not(target_os = "linux"))]
        let wake_fd = None;
        handles.push(
            std::thread::Builder::new()
                .name(format!("cell-{i}"))
                .spawn(move || {
                    // Fail-stop at the thread boundary (M2.5-S01 mechanism 2):
                    // the in-order join loop below blocks on cell 0 forever, so
                    // a later cell's setup error would otherwise leave a dead
                    // cell the narrator reports as stuck in its last phase —
                    // the captured wedge was cell 3 exiting `setup:driver` on
                    // an io_uring_setup failure nobody printed. A cell that
                    // cannot run takes the node down loudly, here and now.
                    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        cell_main(i as u16, &args, boot, fabric, park_flags, wake_fd)
                    }));
                    match outcome {
                        Ok(Ok(())) => Ok::<(), std::io::Error>(()),
                        Ok(Err(e)) => {
                            eprintln!("infinityd: cell {i} failed: {e}");
                            std::process::exit(1);
                        }
                        Err(_) => {
                            // The default hook already printed the panic.
                            eprintln!("infinityd: cell {i} panicked — fail-stop");
                            std::process::exit(101);
                        }
                    }
                })
                .expect("spawn cell thread"),
        );
    }
    eprintln!("{}", version_line());
    eprintln!(
        "infinityd: {} cells, port {}, backend {}, route {}",
        args.cells,
        args.port,
        backend_name(),
        if args.route_local_only { "local-only" } else { "natural" }
    );
    for handle in handles {
        if let Err(e) = handle.join().expect("cell thread panicked") {
            eprintln!("infinityd: cell failed: {e}");
            std::process::exit(1);
        }
    }
}

type Boot = Option<(
    std::path::PathBuf,
    Option<inf_store::NsCatalog>,
    std::sync::Arc<inf_server::ControlHandle>,
)>;

fn cell_main(
    cell: u16,
    args: &Args,
    boot: Boot,
    fabric: CellFabric,
    park_flags: std::sync::Arc<Vec<std::sync::atomic::AtomicBool>>,
    wake_fd: Option<std::os::fd::OwnedFd>,
) -> std::io::Result<()> {
    // Setup-phase narration (M2.5-S01): the 500-cycle storm caught cells
    // stalling BEFORE the first loop iteration ("spawned" forever) — every
    // setup step below publishes its phase so a kernel-side stall names
    // itself on the RecoveryBoard instead of wedging silently.
    let mark = |code: u8| {
        if let Some((_, _, control)) = &boot {
            control.recovery_board().slot(cell).publish_phase(code);
        }
    };
    if let Some(start) = args.pin_start {
        pin_current_thread(start + cell as usize * 2);
    }
    mark(10); // setup:listen
    let listener = listen_reuseport(args.port)?;
    if cell == 0 {
        eprintln!("infinityd: listening on {}", bound_port(&listener)?);
    }
    mark(11); // setup:pool
    let mut pool = BufferPool::new(args.buffers, args.buf_size);
    mark(12); // setup:driver
    let mut driver = make_driver()
        .map_err(|e| std::io::Error::new(e.kind(), format!("driver setup (ring create): {e}")))?;
    mark(13); // setup:register
    driver.register_pool(&mut pool)?;
    #[cfg(target_os = "linux")]
    if let Some(fd) = wake_fd {
        driver.adopt_wake_fd(fd);
    }
    #[cfg(not(target_os = "linux"))]
    let _ = wake_fd;
    if cell == 0 {
        eprintln!("infinityd: capabilities {:?}", driver.capabilities());
    }

    let node = Rc::new(NodeInfo::default());
    // Wall-clock anchor (M1-S03): the system clock is read ONCE here, at the
    // cell clock's origin (internal ms 0); everything downstream converts
    // through the anchor (L7 — EXPIREAT/EXAT stay deterministic under DST,
    // which injects its own anchor).
    let unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    node.wall_anchor.set((0, unix_ms));
    node.rng_state.set(unix_ms ^ (u64::from(cell) << 48) ^ 0x9E37_79B9_7F4A_7C15);
    node.tcp_port.set(args.port);
    // Durable boot (M2-S08/S11/S15): the catalog seeds the keyspace, then
    // the cell serves from its first iteration — answering `-LOADING` —
    // while MAINTAIN replays MANIFEST → checkpoint → tail in bounded
    // steps (the recovery I/O itself stays the sanctioned §3.3 boot-time
    // blocking exception, now sliced). Progress/summary lines come from
    // the control thread's recovery board.
    mark(14); // setup:keyspace
    let mut ks = Keyspace::new(StoreConfig::default());
    let mut durable = None;
    if let Some((dir, catalog, control)) = &boot {
        if let Some(catalog) = catalog {
            ks.seed_catalog(catalog).map_err(|e| std::io::Error::other(format!("{e:?}")))?;
        }
        let cfg = inf_server::DurableConfig {
            data_dir: dir.clone(),
            staging: inf_server::StagingConfig { capacity_bytes: args.log_staging_mib << 20 },
            segment: inf_server::SegmentConfig {
                segment_bytes: args.segment_bytes,
                ..Default::default()
            },
            ckpt: inf_server::CkptConfig {
                interval_bytes: args.ckpt_interval_bytes,
                ..Default::default()
            },
            recover: inf_server::RecoverConfig::default(),
            sync_pipeline: args.sync_pipeline,
        };
        durable = Some((cfg, std::sync::Arc::clone(control)));
    }
    mark(15); // setup:plane
    let mut plane = ServerPlane::new(
        CellId(cell),
        args.cells,
        listener.into_raw_fd(), // the driver owns the listener fd now
        ks,
        fabric,
        Rc::clone(&node),
        NoopObserver,
        args.route_local_only,
    );
    if let Some((cfg, control)) = durable {
        plane.set_control(control);
        // ReadAheadFs (M2.5-S08): recovery reads hint the next window so
        // cold replay's device read overlaps apply; the write path is pure
        // delegation. Prefetch only when this cell recovers alone — N
        // parallel recovering cells already saturate the device, and N
        // extra prefetch streams cost sequential locality (the S08 A/B's
        // measured regime split).
        plane.begin_recovery(
            inf_server::ReadAheadFs::new(StdSegmentFs, args.cells == 1),
            &cfg,
            cell,
            StdClock::new().now(),
        );
    }
    // Doorbell wakeups (Linux): peers end this cell's park via eventfd, so
    // the park timeout is a fallback, not the hop-latency ceiling. The park
    // board only helps when the driver has a wake watch.
    plane.set_early_fabric_flush(args.early_fabric_flush);
    plane.set_fabric_apply_prefetch(args.fabric_apply_prefetch);
    plane.set_parse_batch_prefetch(args.parse_batch_prefetch);
    plane.set_deasync_dispatch(args.deasync_dispatch);
    #[cfg(target_os = "linux")]
    plane.set_park_flags(park_flags);
    #[cfg(not(target_os = "linux"))]
    let _ = park_flags;
    // Multi-cell dev-tier (kqueue, no wakeups) still parks briefly so a
    // parked peer notices doorbells within the ceiling.
    let park_us = args.park_us.unwrap_or(if args.cells > 1 { 500 } else { 5_000 });
    let config = LoopConfig {
        park_default: Some(std::time::Duration::from_micros(park_us)),
        remote_first_execute: args.remote_first_execute,
        ..Default::default()
    };
    let mut cell_loop = CellLoop::new(driver, StdClock::new(), pool, config);

    mark(16); // setup:loop — the next publish is drive_recovery's phase 1
    let mut iterations: u64 = 0;
    loop {
        cell_loop.run_iteration(&mut plane)?;
        if let Some(err) = plane.take_boot_error() {
            // §8.4 fail-stop: recovery refused — the whole node stops,
            // immediately (a half-recovered node must never serve).
            eprintln!("infinityd: cell {cell} recovery failed (fail-stop, §8.4): {err}");
            std::process::exit(1);
        }
        iterations += 1;
        if iterations.is_multiple_of(STATS_EVERY) {
            let tw = cell_loop.tripwires();
            node.tripwires.set([tw[0].1, tw[1].1, tw[2].1, tw[3].1, tw[4].1]);
            node.raw_counters.set(cell_loop.counters());
            node.wire_buffers_bytes.set(cell_loop.pool().reserved_bytes() as u64);
        }
    }
}

#[cfg(target_os = "linux")]
fn make_driver() -> std::io::Result<inf_runtime::UringDriver> {
    inf_runtime::UringDriver::new(4096)
}

#[cfg(target_os = "macos")]
fn make_driver() -> std::io::Result<inf_runtime::KqueueDriver> {
    inf_runtime::KqueueDriver::new()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn make_driver() -> std::io::Result<never::NoBackend> {
    Err(std::io::Error::other("no backend: build with --features uring on Linux"))
}

/// Uninhabitable backend for targets without one — keeps the generic node
/// code compiling everywhere while `make_driver` always errors first.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod never {
    use inf_alloc::BufferPool;
    use inf_runtime::{BackendDriver, Capabilities, Completion, IoOp, SubmitStats, Wait};

    pub struct NoBackend(core::convert::Infallible);

    impl BackendDriver for NoBackend {
        fn push(&mut self, _: IoOp) {
            match self.0 {}
        }
        fn submit_and_reap(
            &mut self,
            _: &mut BufferPool,
            _: Wait,
            _: &mut Vec<Completion>,
        ) -> std::io::Result<usize> {
            match self.0 {}
        }
        fn register_pool(&mut self, _: &mut BufferPool) -> std::io::Result<()> {
            match self.0 {}
        }
        fn capabilities(&self) -> Capabilities {
            match self.0 {}
        }
        fn submit_stats(&self) -> SubmitStats {
            match self.0 {}
        }
    }
}

fn backend_name() -> &'static str {
    #[cfg(target_os = "linux")]
    return "io_uring";
    #[cfg(target_os = "macos")]
    return "kqueue";
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    "none"
}
