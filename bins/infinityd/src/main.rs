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
    /// Frames in flight per cell (M4.5-S35, ADR-0087 D1/D5): the staging
    /// ring holds `frames_in_flight + 1` buffers of `--log-staging-mib`
    /// each; resident bytes are their product (L5-neutral pairing for the
    /// reference arm: `--frames-in-flight 3 --log-staging-mib 2`).
    frames_in_flight: u8,
    /// Log barrier class override (M4.5-S34, ADR-0086 D7): `None` reads
    /// `<data-dir>/io-properties.toml` (absent ⇒ `flush`, today's path);
    /// `Some` forces the class — the A/B arm switch. `fua` needs the
    /// probe file for its `fua_max_frame_bytes`/tripwire reference or
    /// runs on the defaults (logged).
    barrier_class: Option<inf_server::SegmentIoMode>,
    /// M4.5-S36 (ADR-0088 D6): override the probed device write model
    /// (MiB/s of the whole device; 0 = unbudgeted) — the A/B arm switch
    /// like `--barrier-class`. `None` = the probe file's model, or
    /// absent.
    device_write_mbps: Option<u64>,
    /// M4.5-S36 (ADR-0088 D2b): the frame-seal pacer — `None` = off (the
    /// shipped default: the S35 reference-box campaign did not reproduce
    /// the @256 shape it was designed for, so it is an A/B arm, not a
    /// behaviour); `Some(None)` = the probe file's `write_ops_per_s_4k_
    /// qd4` (`--seal-pace probe`); `Some(Some(n))` = `n` barriers/s per
    /// device (`--seal-pace N`).
    seal_pace: Option<Option<u64>>,
    /// M4.5-S39a: the frame-fill policy on aligned segments — the hold
    /// window in µs (0 = off, the shipped default until the A/B), the
    /// on-device target in KiB (16), and the arm-B switch that extends
    /// the hold to barrier-carrying frames behind in-flight ones.
    fill_window_us: u64,
    fill_target_kib: u32,
    fill_window_always: bool,
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
            frames_in_flight: 1,
            barrier_class: None,
            device_write_mbps: None,
            seal_pace: None,
            fill_window_us: 0,
            fill_target_kib: 16,
            fill_window_always: false,
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
            "--frames-in-flight" => {
                args.frames_in_flight = take("--frames-in-flight")?
                    .parse()
                    .map_err(|e| format!("--frames-in-flight: {e}"))?;
                if !(1..=inf_server::MAX_FRAMES_IN_FLIGHT).contains(&args.frames_in_flight) {
                    return Err(format!(
                        "--frames-in-flight is 1..={} — bounded, never a queue",
                        inf_server::MAX_FRAMES_IN_FLIGHT
                    ));
                }
            }
            "--sync-pipeline" => {
                // Retired (ADR-0087 D5): the FLUSH-class bound is the
                // ADR-0022 D3 constant; the measured two-in-flight arm is
                // constructed by the harness, never a production flag.
                return Err(
                    "--sync-pipeline was retired by ADR-0087; use --frames-in-flight K".into()
                );
            }
            "--barrier-class" => {
                args.barrier_class = Some(match take("--barrier-class")?.as_str() {
                    "flush" => inf_server::SegmentIoMode::Buffered,
                    "fua" => inf_server::SegmentIoMode::Direct,
                    other => return Err(format!("--barrier-class is flush|fua, got {other}")),
                });
            }
            "--device-write-mbps" => {
                args.device_write_mbps = Some(
                    take("--device-write-mbps")?
                        .parse()
                        .map_err(|e| format!("--device-write-mbps: {e}"))?,
                );
            }
            "--seal-pace" => {
                args.seal_pace = Some(match take("--seal-pace")?.as_str() {
                    "probe" => None,
                    "off" => return Err("--seal-pace off: omit the flag instead".into()),
                    n => Some(n.parse().map_err(|e| format!("--seal-pace: {e}"))?),
                });
            }
            "--fill-window-us" => {
                args.fill_window_us = take("--fill-window-us")?
                    .parse()
                    .map_err(|e| format!("--fill-window-us: {e}"))?;
                // A hold past the everysec tick would move the loss
                // window; 100 ms is already two orders past the design
                // point (1 ms).
                if args.fill_window_us > 100_000 {
                    return Err("--fill-window-us is 0..=100000 (µs)".into());
                }
            }
            "--fill-target-kib" => {
                args.fill_target_kib = take("--fill-target-kib")?
                    .parse()
                    .map_err(|e| format!("--fill-target-kib: {e}"))?;
                if !(4..=1024).contains(&args.fill_target_kib) {
                    return Err("--fill-target-kib is 4..=1024 (one block to the FUA bound)".into());
                }
            }
            "--fill-window-always" => args.fill_window_always = true,
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
                     [--ckpt-interval-bytes N] [--segment-bytes N] [--frames-in-flight 1] \
                     [--barrier-class flush|fua] [--device-write-mbps N] [--seal-pace probe|N] \
                     [--fill-window-us 0] [--fill-target-kib 16] [--fill-window-always] \
                     [--log-staging-mib 4] \
                     [--early-fabric-flush] \
                     [--remote-first-execute] \
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
        // Barrier class (M4.5-S34, ADR-0086 D7): the probe file decides,
        // the flag overrides, absence is today's FLUSH class. A malformed
        // file is a refusal — never a silent fallback to the slow class.
        let probed = match inf_server::IoProperties::load(&dir) {
            Ok(probed) => probed,
            Err(e) => {
                eprintln!("infinityd: {e} (fail-stop: fix or remove the file)");
                std::process::exit(1);
            }
        };
        let mut io = probed.unwrap_or_default();
        let source = match (args.barrier_class, probed.is_some()) {
            (Some(forced), _) => {
                io.io_mode = forced;
                "--barrier-class"
            }
            (None, true) => inf_server::IO_PROPERTIES_FILE,
            (None, false) => "default (no io-properties.toml)",
        };
        let class = match io.io_mode {
            inf_server::SegmentIoMode::Direct => "fua",
            inf_server::SegmentIoMode::Buffered => "flush",
        };
        eprintln!(
            "infinityd: log barrier class {class} (source: {source}; fua_max_frame_bytes {}; \
             probed p50 fua {} µs / flush {} µs); frames in flight {} × {} MiB staging",
            io.fua_max_frame_bytes,
            io.fua_p50_us_4k,
            io.flush_p50_us_4k,
            args.frames_in_flight,
            args.log_staging_mib
        );
        // Device model (M4.5-S36, ADR-0088 D6): the probe file's schema-2
        // rows, the flag overriding the write rate, absence named loudly
        // — an unbudgeted cell is today's behaviour, never a silent one.
        if let Some(mbps) = args.device_write_mbps {
            io.device.write_bytes_per_s = mbps << 20;
        }
        if io.device.is_absent() {
            eprintln!(
                "infinityd: device model absent (io-properties schema {}): background I/O is \
                 unbudgeted and frame sealing unpaced — run `inf probe-device` to enable the \
                 device budget (ADR-0088)",
                io.probe_schema
            );
        } else {
            let share = io.device.share(args.cells);
            eprintln!(
                "infinityd: device model probed (schema {}): write {} MiB/s, {} ops/s (qd4 \
                 barriers {}/s); per-cell share write {} MiB/s",
                io.probe_schema,
                io.device.write_bytes_per_s >> 20,
                io.device.write_ops_per_s,
                io.write_ops_per_s_4k_qd4,
                share.write_bytes_per_s >> 20,
            );
        }
        // The seal pacer (ADR-0088 D2b) is an explicit arm: off unless asked.
        let seal_barriers_per_s = match args.seal_pace {
            None => 0,
            Some(None) => {
                if io.write_ops_per_s_4k_qd4 == 0 {
                    eprintln!(
                        "infinityd: --seal-pace probe needs a schema-2 io-properties.toml with \
                         write_ops_per_s_4k_qd4 (run `inf probe-device`)"
                    );
                    std::process::exit(1);
                }
                io.write_ops_per_s_4k_qd4
            }
            Some(Some(n)) => n,
        };
        if seal_barriers_per_s > 0 {
            eprintln!(
                "infinityd: seal pace {} barriers/s per device → {} per cell (ADR-0088 D2b arm)",
                seal_barriers_per_s,
                seal_barriers_per_s / u64::from(args.cells.max(1))
            );
        }
        if args.segment_bytes % inf_server::FRAME_ALIGN != 0
            && io.io_mode == inf_server::SegmentIoMode::Direct
        {
            eprintln!(
                "infinityd: --segment-bytes {} is not a multiple of {} — required by the fua class",
                args.segment_bytes,
                inf_server::FRAME_ALIGN
            );
            std::process::exit(1);
        }
        (dir, catalog, control, io, seal_barriers_per_s)
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
    inf_server::IoProperties,
    // The seal pacer's device rate (ADR-0088 D2b arm; 0 = off).
    u64,
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
        if let Some((_, _, control, _, _)) = &boot {
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
    if let Some((dir, catalog, control, io, seal_barriers_per_s)) = &boot {
        if let Some(catalog) = catalog {
            ks.seed_catalog(catalog).map_err(|e| std::io::Error::other(format!("{e:?}")))?;
        }
        let cfg = inf_server::DurableConfig {
            data_dir: dir.clone(),
            staging: inf_server::StagingConfig {
                capacity_bytes: args.log_staging_mib << 20,
                frames_in_flight: args.frames_in_flight,
            },
            segment: inf_server::SegmentConfig {
                segment_bytes: args.segment_bytes,
                io_mode: io.io_mode,
                fua_max_frame_bytes: io.fua_max_frame_bytes,
                ..Default::default()
            },
            ckpt: inf_server::CkptConfig {
                interval_bytes: args.ckpt_interval_bytes,
                ..Default::default()
            },
            recover: inf_server::RecoverConfig::default(),
            flush_bound: 1,
            fua_p50_us_probed: io.fua_p50_us_4k,
            // ADR-0088 D2/D2b: static per-cell shares, computed once here
            // (L1). Absent ⇒ `Default` ⇒ unbudgeted and unpaced.
            device: inf_server::DeviceConfig {
                model_share: io.device.share(args.cells),
                seal_barriers_per_s: seal_barriers_per_s / u64::from(args.cells.max(1)),
            },
            // M4.5-S39a: the fill policy (off unless `--fill-window-us`).
            fill: inf_server::FillConfig {
                window: inf_foundation::time::Nanos::from_micros(args.fill_window_us),
                target_bytes: args.fill_target_kib << 10,
                hold_due: args.fill_window_always,
            },
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
