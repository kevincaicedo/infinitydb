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
use inf_foundation::time::StdClock;
use inf_runtime::net::{bound_port, listen_reuseport, pin_current_thread};
use inf_runtime::{BackendDriver, CellLoop, LoopConfig};
use inf_server::{NodeInfo, NoopObserver, ServerPlane};
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
            "--park-us" => {
                args.park_us =
                    Some(take("--park-us")?.parse().map_err(|e| format!("--park-us: {e}"))?);
            }
            "--help" | "-h" => {
                println!(
                    "infinityd [--port 6379] [--cells 4] [--buffers 4096] [--buf-size 4096] \
                     [--pin-start CORE] [--route-local-only]"
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

fn main() {
    let args = match parse_args() {
        Ok(args) => args,
        Err(e) => {
            eprintln!("infinityd: {e}");
            std::process::exit(2);
        }
    };
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

    let mut handles = Vec::new();
    for (i, fabric) in fabrics.into_iter().enumerate() {
        let args = args.clone();
        let park_flags = std::sync::Arc::clone(&park_flags);
        #[cfg(target_os = "linux")]
        let wake_fd = wake_fds[i].take();
        #[cfg(not(target_os = "linux"))]
        let wake_fd = None;
        handles.push(
            std::thread::Builder::new()
                .name(format!("cell-{i}"))
                .spawn(move || cell_main(i as u16, &args, fabric, park_flags, wake_fd))
                .expect("spawn cell thread"),
        );
    }
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

fn cell_main(
    cell: u16,
    args: &Args,
    fabric: CellFabric,
    park_flags: std::sync::Arc<Vec<std::sync::atomic::AtomicBool>>,
    wake_fd: Option<std::os::fd::OwnedFd>,
) -> std::io::Result<()> {
    if let Some(start) = args.pin_start {
        pin_current_thread(start + cell as usize * 2);
    }
    let listener = listen_reuseport(args.port)?;
    if cell == 0 {
        eprintln!("infinityd: listening on {}", bound_port(&listener)?);
    }
    let mut pool = BufferPool::new(args.buffers, args.buf_size);
    let mut driver = make_driver()?;
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
    let mut plane = ServerPlane::new(
        CellId(cell),
        args.cells,
        listener.into_raw_fd(), // the driver owns the listener fd now
        Keyspace::new(StoreConfig::default()),
        fabric,
        Rc::clone(&node),
        NoopObserver,
        args.route_local_only,
    );
    // Doorbell wakeups (Linux): peers end this cell's park via eventfd, so
    // the park timeout is a fallback, not the hop-latency ceiling. The park
    // board only helps when the driver has a wake watch.
    #[cfg(target_os = "linux")]
    plane.set_park_flags(park_flags);
    #[cfg(not(target_os = "linux"))]
    let _ = park_flags;
    // Multi-cell dev-tier (kqueue, no wakeups) still parks briefly so a
    // parked peer notices doorbells within the ceiling.
    let park_us = args.park_us.unwrap_or(if args.cells > 1 { 500 } else { 5_000 });
    let config = LoopConfig {
        park_default: Some(std::time::Duration::from_micros(park_us)),
        ..Default::default()
    };
    let mut cell_loop = CellLoop::new(driver, StdClock::new(), pool, config);

    let mut iterations: u64 = 0;
    loop {
        cell_loop.run_iteration(&mut plane)?;
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
