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
use inf_foundation::time::{Clock, StdClock};
use inf_foundation::{CellId, KeyHasher};
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
    /// M4.5-S40: every accepted connection starts in this named
    /// namespace (as if it had sent `INF.NS USE`) — the opt-in for
    /// clients without a per-connection prelude. `None` = default dbs.
    conn_default_ns: Option<String>,
    /// Segment recycling (M4.5-S39b, ADR-0090 D1): covered pre-zeroed
    /// `Direct` segments kept for reuse by rename instead of unlinked —
    /// the zero-fill paid once per generation. `0` = off
    /// (`--no-segment-recycle`, the A/B baseline arm); default 1 (ADR-0090
    /// A7.2).
    segment_recycle_slots: u8,
    /// The pool wait (ADR-0090 D9): `quarter` (the default) re-checks the
    /// pool each MAINTAIN slice until the active segment is a quarter
    /// full before creating a fresh next segment; `eighth` bounds it
    /// tighter; `off` preallocates at rotation (the D9 A/B baseline arm).
    recycle_wait: inf_server::PreallocPolicy,
    /// M4.5-S37 step 1 (`bench-diagnostics` builds only): the blind-
    /// overwrite ceiling arm — unsound measurement instrument.
    #[cfg(feature = "bench-diagnostics")]
    blind_overwrite_ceiling: bool,
    /// Frames in flight per cell (M4.5-S35, ADR-0087 D1/D5 as amended
    /// 2026-08-22): `auto` (the default) derives K from the resolved
    /// barrier class — FUA → 3, FLUSH → 1 — after the class is known and
    /// before the staging ring is sized; `K` forces it for either class.
    /// The ring holds `K + 1` buffers of `--log-staging-mib` each;
    /// resident bytes are their product (the shipped FUA pairing is
    /// `3 × 4 MiB`, keeping the ≈ 4 MiB durable-record bound).
    frames_in_flight: inf_server::FramesInFlight,
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
    /// M4.5-S39a (ADR-0089): the frame-fill policy on aligned segments —
    /// the hold window in µs (1 000 = the design point; 0 = off) and the
    /// on-device target in KiB (16). **Default 1 000 by the third
    /// amendment (2026-08-22):** after the second amendment reverted it
    /// (the first A/B ran at K = 3 / 2 MiB and its falsifier fired as
    /// written), the predeclared rerun at the shipping K = 1 / 4 MiB
    /// passed every clause — 8 of 8 pairs ≥ baseline, padding 10–17 % →
    /// 7–10 %, zero stalls in sixteen offered legs
    /// (`.artifacts/m4.5/review3/`).
    fill_window_us: u64,
    fill_target_kib: u32,
    /// M4.5-S43 (ADR-0092): the FLUSH-class group hold's window in µs
    /// (0 = off, the shipped default until the reference-box A/B; 250 =
    /// the measured arm). Inert on the FUA class by construction.
    flush_group_window_us: u64,
    /// M4.5-S42 (ADR-0091 D1): the device-model lifecycle. `auto` (the
    /// product): no `io-properties.toml` ⇒ probe the device now, before
    /// any cell starts, and write one; a file whose identity no longer
    /// matches the device ⇒ rename it `.stale` and probe again. `off`
    /// (the dev/test tier): absent ⇒ the FLUSH class unbudgeted, loudly;
    /// a mismatched file ⇒ refusal.
    device_probe: DeviceProbe,
    /// Seconds per probe row for the in-boot probe (1..=60; `inf
    /// probe-device` defaults to 2 — the boot pays ≈ 10 s at 1).
    probe_seconds: u64,
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

/// `--device-probe auto|off` (M4.5-S42, ADR-0091 D1).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum DeviceProbe {
    Auto,
    Off,
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
            conn_default_ns: None,
            segment_recycle_slots: inf_server::DEFAULT_RECYCLE_SLOTS,
            recycle_wait: inf_server::PreallocPolicy::DEFAULT,
            #[cfg(feature = "bench-diagnostics")]
            blind_overwrite_ceiling: false,
            frames_in_flight: inf_server::FramesInFlight::Auto,
            barrier_class: None,
            device_write_mbps: None,
            seal_pace: None,
            fill_window_us: 1_000,
            fill_target_kib: 16,
            // M4.5-S43 (ADR-0092, campaign K 2026-08-26): the FLUSH-class
            // group hold ships at the measured arm's window — 250 µs; `0`
            // is the off arm; inert under the FUA class (rule 3).
            flush_group_window_us: 250,
            device_probe: DeviceProbe::Auto,
            probe_seconds: inf_probe::BOOT_SECONDS_PER_ROW,
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
            "--segment-recycle-slots" => {
                args.segment_recycle_slots = take("--segment-recycle-slots")?
                    .parse()
                    .map_err(|e| format!("--segment-recycle-slots: {e}"))?;
                if args.segment_recycle_slots > 8 {
                    return Err(
                        "--segment-recycle-slots is 0..=8 (disk is a product surface)".into()
                    );
                }
            }
            "--no-segment-recycle" => args.segment_recycle_slots = 0,
            #[cfg(feature = "bench-diagnostics")]
            "--blind-overwrite-ceiling" => args.blind_overwrite_ceiling = true,
            "--recycle-wait" => {
                let text = take("--recycle-wait")?;
                args.recycle_wait = inf_server::PreallocPolicy::parse(&text)
                    .ok_or_else(|| format!("--recycle-wait {text}: expected off|quarter|eighth"))?;
            }
            "--conn-default-ns" => {
                let name = take("--conn-default-ns")?;
                if name.is_empty() {
                    return Err("--conn-default-ns needs a namespace name".into());
                }
                args.conn_default_ns = Some(name);
            }
            "--frames-in-flight" => {
                let raw = take("--frames-in-flight")?;
                args.frames_in_flight = if raw == "auto" {
                    inf_server::FramesInFlight::Auto
                } else {
                    let k: u8 = raw.parse().map_err(|e| format!("--frames-in-flight: {e}"))?;
                    if !(1..=inf_server::MAX_FRAMES_IN_FLIGHT).contains(&k) {
                        return Err(format!(
                            "--frames-in-flight is auto or 1..={} — bounded, never a queue",
                            inf_server::MAX_FRAMES_IN_FLIGHT
                        ));
                    }
                    inf_server::FramesInFlight::Fixed(k)
                };
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
            "--flush-group-window-us" => {
                args.flush_group_window_us = take("--flush-group-window-us")?
                    .parse()
                    .map_err(|e| format!("--flush-group-window-us: {e}"))?;
                // A hold is a fraction of a ≥ 1 ms FLUSH window, never a
                // batch interval of its own (ADR-0092 D2).
                if args.flush_group_window_us > 5_000 {
                    return Err("--flush-group-window-us is 0..=5000 (µs)".into());
                }
            }
            "--device-probe" => {
                args.device_probe = match take("--device-probe")?.as_str() {
                    "auto" => DeviceProbe::Auto,
                    "off" => DeviceProbe::Off,
                    other => return Err(format!("--device-probe is auto|off, got {other}")),
                };
            }
            "--probe-seconds" => {
                args.probe_seconds = take("--probe-seconds")?
                    .parse()
                    .map_err(|e| format!("--probe-seconds: {e}"))?;
                if !inf_probe::SECONDS_PER_ROW_RANGE.contains(&args.probe_seconds) {
                    return Err("--probe-seconds is 1..=60".into());
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
                     [--ckpt-interval-bytes N] [--segment-bytes N] [--segment-recycle-slots 1] \
                     [--no-segment-recycle] [--recycle-wait off|quarter|eighth] \
                     [--conn-default-ns NAME] [--frames-in-flight auto|K] \
                     [--barrier-class flush|fua] [--device-write-mbps N] [--seal-pace probe|N] \
                     [--fill-window-us 1000] [--fill-target-kib 16] \
                     [--flush-group-window-us 250] [--device-probe auto|off] \
                     [--probe-seconds 1] [--log-staging-mib 4] \
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
    // The operator values that reach a cell-crate constructor are bounded
    // here, where a violation is a usage error — never at the constructor's
    // release assert (ADR-0107 D2: `buffer_pool.rs` / `router.rs` C rows
    // cite this function). `--cells` above SLOT_COUNT never reached the
    // router assert at all: the fabric mesh (cells² rings) is built first
    // and ate the box (2026-09-02).
    if args.cells == 0 {
        return Err("--cells must be >= 1".into());
    }
    if args.cells > inf_foundation::SLOT_COUNT {
        return Err(format!("--cells must be <= {}", inf_foundation::SLOT_COUNT));
    }
    if args.buffers == 0 {
        return Err("--buffers must be >= 1".into());
    }
    // Batch 12 of the 2026-08-30 review (Theme 4): the io_uring driver
    // addresses provided recv buffers by a u16 buffer id and hands recv
    // lengths to the kernel as i32 — `--buffers 65536` and `--buf-size`
    // above i32::MAX booted, printed "listening", and died in the cell
    // thread's first recv arm (`uring.rs` asserts, proven at the binary).
    // The bound lives here, where every operator value is validated.
    if args.buffers > usize::from(u16::MAX) {
        return Err(format!(
            "--buffers must be <= {} (the io_uring provided-buffer group addresses buffers by u16 id)",
            u16::MAX
        ));
    }
    if args.buf_size == 0 {
        return Err("--buf-size must be >= 1".into());
    }
    if args.buf_size > i32::MAX as usize {
        return Err(format!(
            "--buf-size must be <= {} (recv lengths are i32 at the io_uring boundary)",
            i32::MAX
        ));
    }
    Ok(args)
}

/// The device model a boot runs on (M4.5-S42, ADR-0091 D1/D2): the
/// parsed file, where it came from, and the boot line's sentence.
struct ResolvedIoProperties {
    props: inf_server::IoProperties,
    provenance: inf_server::IoProvenance,
    note: String,
}

/// Load → identity → (probe) → the properties. Boot code, before any
/// cell: blocking I/O and the probe's clocks are fine here.
fn resolve_io_properties(
    dir: &std::path::Path,
    mode: DeviceProbe,
    probe_seconds: u64,
) -> Result<ResolvedIoProperties, String> {
    use inf_server::{IoProperties, IoPropertiesSource, IoProvenance};
    let loaded =
        IoProperties::load(dir).map_err(|e| format!("{e} (fail-stop: fix or remove the file)"))?;
    let Some(props) = loaded else {
        return match mode {
            DeviceProbe::Off => Ok(ResolvedIoProperties {
                props: IoProperties::default(),
                provenance: IoProvenance::default(),
                note: "absent (--device-probe off): the FLUSH class, K = 1, no device budget — \
                       the dev tier; `--device-probe auto` or `inf probe-device` enables the \
                       probed classes (ADR-0091)"
                    .to_owned(),
            }),
            DeviceProbe::Auto => {
                probe_now(dir, probe_seconds, IoPropertiesSource::ProbedAtBoot, "absent")
            }
        };
    };
    let current = inf_probe::identity_of(dir);
    // Identity first, then the block-size assertion on the file that
    // will actually be used (review of 2026-08-26): a foreign model is
    // decided by its identity alone — under `auto` it is re-probed even
    // when its block size would have been refused, exactly as ADR-0091
    // D1 promises; the block-size rule then judges the fresh file.
    match existing_file_decision(&props, &current, mode) {
        FileDecision::Use(verdict) => {
            check_block_size(&props)?;
            let identity_note = identity_note(&props, verdict);
            Ok(ResolvedIoProperties {
                provenance: IoProvenance {
                    source: IoPropertiesSource::File,
                    schema: props.probe_schema,
                    identity: verdict,
                },
                note: format!(
                    "{} (schema {}, {identity_note})",
                    inf_server::IO_PROPERTIES_FILE,
                    props.probe_schema
                ),
                props,
            })
        }
        FileDecision::Refuse(reason) => Err(format!(
            "{}: the model describes another device — {reason}; re-run `inf probe-device {}`, \
             remove the file, or boot with `--device-probe auto` (fail-stop, ADR-0091 D1)",
            inf_server::IO_PROPERTIES_FILE,
            dir.display()
        )),
        FileDecision::Reprobe(reason) => {
            let stale = dir.join("io-properties.toml.stale");
            std::fs::rename(dir.join(inf_server::IO_PROPERTIES_FILE), &stale)
                .map_err(|e| format!("rename stale io-properties.toml: {e}"))?;
            probe_now(
                dir,
                probe_seconds,
                IoPropertiesSource::Reprobed,
                &format!("stale — {reason}; kept as {}", stale.display()),
            )
        }
    }
}

/// What a boot does with the file it found (ADR-0091 D1, pure — the
/// binary test pins the I/O around it, the unit tests pin the rule).
#[derive(Clone, Debug, PartialEq, Eq)]
enum FileDecision {
    /// Use the file; the identity verdict for `INFO`.
    Use(inf_foundation::IdentityVerdict),
    /// `auto`: rename `.stale` and probe again (the reason, for the log).
    Reprobe(String),
    /// `off`: refuse the boot (the reason, for the operator).
    Refuse(String),
}

/// Identity before block size: a schema-3 file whose identity mismatches
/// the device is stale whatever else it says; schema ≤ 2 files carry no
/// identity and are used as-is (`unverifiable`).
fn existing_file_decision(
    props: &inf_server::IoProperties,
    current: &inf_foundation::DeviceIdentity,
    mode: DeviceProbe,
) -> FileDecision {
    use inf_foundation::IdentityVerdict;
    if props.probe_schema < 3 {
        return FileDecision::Use(IdentityVerdict::Unverifiable);
    }
    match props.identity.mismatch(current) {
        (IdentityVerdict::Mismatch, reason) => {
            let reason = reason.unwrap_or_else(|| "identity mismatch".to_owned());
            match mode {
                DeviceProbe::Off => FileDecision::Refuse(reason),
                DeviceProbe::Auto => FileDecision::Reprobe(reason),
            }
        }
        (verdict, _) => FileDecision::Use(verdict),
    }
}

fn identity_note(
    props: &inf_server::IoProperties,
    verdict: inf_foundation::IdentityVerdict,
) -> String {
    use inf_foundation::IdentityVerdict;
    match verdict {
        IdentityVerdict::Verified => format!(
            "identity verified ({} {} {})",
            props.identity.fs_type,
            props.identity.device_path,
            short_uuid(&props.identity.fs_uuid)
        ),
        IdentityVerdict::Unverifiable => {
            format!("identity unverifiable (schema {})", props.probe_schema)
        }
        IdentityVerdict::Mismatch => "identity mismatch".to_owned(),
    }
}

/// The identity verdict of a file the probe just wrote, re-derived from
/// the device rather than assumed (review of 2026-08-26): a host that
/// exposes neither a filesystem UUID nor a device path writes an empty
/// identity block, and that file is `unverifiable`, not `verified`. A
/// mismatch here is a probe-integrity violation (the file names a device
/// other than the one it was written on) — surfaced typed, fail-stop.
fn probed_identity_verdict(
    props: &inf_server::IoProperties,
    current: &inf_foundation::DeviceIdentity,
) -> Result<inf_foundation::IdentityVerdict, String> {
    use inf_foundation::IdentityVerdict;
    match props.identity.mismatch(current) {
        (IdentityVerdict::Mismatch, reason) => Err(format!(
            "the probe wrote a model whose identity does not match the device it ran on \
             ({}) — fail-stop",
            reason.unwrap_or_default()
        )),
        (verdict, _) => Ok(verdict),
    }
}

/// Run the probe, write the file, read it back the way every later boot
/// will (one parser, one truth).
fn probe_now(
    dir: &std::path::Path,
    probe_seconds: u64,
    source: inf_server::IoPropertiesSource,
    why: &str,
) -> Result<ResolvedIoProperties, String> {
    use inf_server::{IoProperties, IoProvenance};
    eprintln!(
        "infinityd: io-properties {why}: probing the device under {} ({probe_seconds} s per row, \
         256 MiB scratch) — once per data directory (ADR-0091 D1)",
        dir.display()
    );
    let opts = inf_probe::ProbeOptions { seconds_per_row: probe_seconds };
    let (path, report) = inf_probe::run_and_write(dir, opts).map_err(|e| {
        format!(
            "device probe failed under {}: {e} (fix the directory — the probe needs a 256 MiB \
             scratch file — or boot with `--device-probe off`)",
            dir.display()
        )
    })?;
    for line in report.row_lines() {
        eprintln!("infinityd: probe: {line}");
    }
    let props = IoProperties::load(dir)
        .map_err(|e| format!("{e} (the probe wrote a file this binary cannot read)"))?
        .ok_or_else(|| format!("the probe wrote no {}", path.display()))?;
    check_block_size(&props)?;
    let identity = probed_identity_verdict(&props, &inf_probe::identity_of(dir))?;
    Ok(ResolvedIoProperties {
        provenance: IoProvenance { source, schema: props.probe_schema, identity },
        note: format!(
            "{} (schema {}, {:.1} s; identity {} {} {}) → barrier class {}",
            source.as_str(),
            props.probe_schema,
            report.elapsed.as_secs_f64(),
            if props.identity.fs_type.is_empty() { "?" } else { &props.identity.fs_type },
            if props.identity.device_path.is_empty() { "?" } else { &props.identity.device_path },
            short_uuid(&props.identity.fs_uuid),
            report.verdict.class.name()
        ),
        props,
    })
}

/// ADR-0091 D2: a logical block larger than `FRAME_ALIGN` would be a
/// torn-write unit the frame reader's rule does not cover — refuse.
fn check_block_size(props: &inf_server::IoProperties) -> Result<(), String> {
    let logical = props.identity.block_logical_bytes;
    if logical > inf_server::FRAME_ALIGN {
        return Err(format!(
            "io-properties: the device's logical block is {logical} bytes, larger than the \
             {}-byte frame alignment — unsupported (ADR-0086 D3, ADR-0091 D2)",
            inf_server::FRAME_ALIGN
        ));
    }
    Ok(())
}

fn short_uuid(uuid: &str) -> String {
    if uuid.is_empty() {
        "(no uuid)".to_owned()
    } else {
        format!("uuid {}…", &uuid[..uuid.len().min(8)])
    }
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

    // The data directory's owner lock (ADR-0094 D7), before anything
    // else touches the directory — the secret, its binding scan, the
    // device profile, the catalog, every shard. Held for the process's
    // life (the binding below; the kernel releases it on any exit).
    let _data_dir_lock =
        args.data_dir.as_deref().map(|dir| match inf_server::DataDirLock::acquire(dir) {
            Ok(lock) => lock,
            Err(e) => {
                eprintln!("infinityd: {e}");
                std::process::exit(1);
            }
        });

    // The key-hash secret (ADR-0094 D2): a data directory's persisted
    // secret — created from OS entropy at its first boot, read on every
    // later one, refused when the directory holds data that predates the
    // ADR — or a fresh secret per memory-only boot. Every store of this
    // node hashes with it. Then the binding scan (D6): every shard's
    // MANIFEST must name this secret, checked before any cell starts.
    let hasher = match &args.data_dir {
        Some(dir) => match inf_server::resolve_key_hash(dir, os_entropy) {
            Ok((hasher, source)) => {
                let binding = match inf_server::verify_key_hash_binding(dir, hasher) {
                    Ok(binding) => binding,
                    Err(e) => {
                        eprintln!("infinityd: {e}");
                        std::process::exit(1);
                    }
                };
                eprintln!(
                    "infinityd: key-hash secret: {} ({}; id {}; {} manifest(s) bound, {} shard(s) \
                     unpublished)",
                    inf_server::KEY_HASH_FILE,
                    match source {
                        inf_server::KeyHashSource::File => "read — siphash13, ADR-0094",
                        inf_server::KeyHashSource::Created =>
                            "created at this first boot — siphash13, ADR-0094",
                    },
                    hasher.identity(),
                    binding.manifests_bound,
                    binding.shards_unpublished
                );
                hasher
            }
            Err(e) => {
                eprintln!("infinityd: {e}");
                std::process::exit(1);
            }
        },
        None => match os_entropy() {
            Ok(bytes) => KeyHasher::from_keys(
                u64::from_le_bytes(bytes[..8].try_into().expect("8 bytes")),
                u64::from_le_bytes(bytes[8..].try_into().expect("8 bytes")),
            ),
            Err(e) => {
                eprintln!("infinityd: no entropy for the key-hash secret: {e} (fail-stop)");
                std::process::exit(1);
            }
        },
    };

    // The cell topology binding (ADR-0095, review C8): the slot→cell
    // partition is a fact of the directory, not of this boot's argv —
    // stamped at first boot under the owner lock, derived from the
    // shard set for pre-ADR directories, and a `--cells` that disagrees
    // is a typed refusal before the catalog or any cell is touched
    // (reopening at another count silently loses acked durable data).
    if let Some(dir) = &args.data_dir {
        match inf_server::resolve_topology(dir, args.cells) {
            Ok(source) => eprintln!(
                "infinityd: topology: {} cells ({}; {})",
                args.cells,
                match source {
                    inf_server::TopologySource::File => "read — ADR-0095",
                    inf_server::TopologySource::Created => "stamped at this first boot — ADR-0095",
                    inf_server::TopologySource::Adopted =>
                        "derived from the shard set and stamped — ADR-0095 D3",
                },
                inf_server::TOPOLOGY_FILE,
            ),
            Err(e) => {
                eprintln!("infinityd: {e}");
                std::process::exit(1);
            }
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
        // Device-model lifecycle (M4.5-S42, ADR-0091 D1): the file, the
        // device's identity, and — under `auto` — the probe, before any
        // cell starts. A malformed or foreign file is a refusal, never a
        // silent fallback to the slow class.
        let resolved = match resolve_io_properties(&dir, args.device_probe, args.probe_seconds) {
            Ok(resolved) => resolved,
            Err(e) => {
                eprintln!("infinityd: {e}");
                std::process::exit(1);
            }
        };
        eprintln!("infinityd: io-properties: {}", resolved.note);
        let (mut io, provenance) = (resolved.props, resolved.provenance);
        // Barrier class (M4.5-S34, ADR-0086 D7): the probe file decides,
        // the flag overrides.
        let source = match args.barrier_class {
            Some(forced) => {
                io.io_mode = forced;
                "--barrier-class"
            }
            None => provenance.source.as_str(),
        };
        let class = match io.io_mode {
            inf_server::SegmentIoMode::Direct => "fua",
            inf_server::SegmentIoMode::Buffered => "flush",
        };
        // K (ADR-0087 D5 as amended 2026-08-22): resolved from the class
        // just decided, before the staging ring is sized — the resolver
        // takes the class, the config takes the resolver's answer.
        let frames_in_flight = args.frames_in_flight.resolve(io.io_mode);
        eprintln!(
            "infinityd: log barrier class {class} (source: {source}; fua_max_frame_bytes {}; \
             probed p50 fua {} µs / flush {} µs); frames in flight {} ({}) × {} MiB staging",
            io.fua_max_frame_bytes,
            io.fua_p50_us_4k,
            io.flush_p50_us_4k,
            frames_in_flight,
            args.frames_in_flight.source(io.io_mode),
            args.log_staging_mib
        );
        if io.io_mode == inf_server::SegmentIoMode::Direct {
            eprintln!(
                "infinityd: segment recycling {} (ADR-0090)",
                match args.segment_recycle_slots {
                    0 => "off".to_owned(),
                    n => format!(
                        "on, {n} slot(s) × {} MiB per cell, pool wait {}",
                        args.segment_bytes >> 20,
                        args.recycle_wait
                    ),
                }
            );
        }
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
        eprintln!(
            "infinityd: checkpoint cap replay term {} MiB/s per cell ({}; ADR-0088 D4 as amended)",
            replay_bytes_per_s_per_cell(
                io.device.read_bytes_per_s,
                io.read_bytes_per_s_256k_qd1,
                args.cells
            ) >> 20,
            replay_term_origin(
                io.device.read_bytes_per_s,
                io.read_bytes_per_s_256k_qd1,
                args.cells
            )
        );
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
        (dir, catalog, control, io, seal_barriers_per_s, frames_in_flight, provenance)
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
                        cell_main(i as u16, &args, boot, hasher, fabric, park_flags, wake_fd)
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
    // K, resolved from the barrier class at boot (ADR-0087 D5 as amended).
    u8,
    inf_server::IoProvenance,
)>;

fn cell_main(
    cell: u16,
    args: &Args,
    boot: Boot,
    hasher: KeyHasher,
    fabric: CellFabric,
    park_flags: std::sync::Arc<Vec<std::sync::atomic::AtomicBool>>,
    wake_fd: Option<std::os::fd::OwnedFd>,
) -> std::io::Result<()> {
    // Setup-phase narration (M2.5-S01): the 500-cycle storm caught cells
    // stalling BEFORE the first loop iteration ("spawned" forever) — every
    // setup step below publishes its phase so a kernel-side stall names
    // itself on the RecoveryBoard instead of wedging silently.
    let mark = |code: u8| {
        if let Some((_, _, control, _, _, _, _)) = &boot {
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
    if let Some(name) = &args.conn_default_ns {
        *node.conn_default_ns.borrow_mut() = Some(name.clone().into_bytes());
    }
    // Durable boot (M2-S08/S11/S15): the catalog seeds the keyspace, then
    // the cell serves from its first iteration — answering `-LOADING` —
    // while MAINTAIN replays MANIFEST → checkpoint → tail in bounded
    // steps (the recovery I/O itself stays the sanctioned §3.3 boot-time
    // blocking exception, now sliced). Progress/summary lines come from
    // the control thread's recovery board.
    mark(14); // setup:keyspace
    let mut ks = Keyspace::new(StoreConfig { hasher, ..StoreConfig::default() });
    let mut durable = None;
    if let Some((dir, catalog, control, io, seal_barriers_per_s, frames_in_flight, provenance)) =
        &boot
    {
        if let Some(catalog) = catalog {
            ks.seed_catalog(catalog).map_err(|e| std::io::Error::other(format!("{e:?}")))?;
        }
        let cfg = inf_server::DurableConfig {
            data_dir: dir.clone(),
            staging: inf_server::StagingConfig {
                capacity_bytes: args.log_staging_mib << 20,
                frames_in_flight: *frames_in_flight,
            },
            segment: inf_server::SegmentConfig {
                segment_bytes: args.segment_bytes,
                io_mode: io.io_mode,
                fua_max_frame_bytes: io.fua_max_frame_bytes,
                recycle_slots: args.segment_recycle_slots,
                prealloc: args.recycle_wait,
                ..Default::default()
            },
            // ADR-0088 D4 as amended twice (M4.5-S34 campaign M3; the
            // second amendment): the cap's replay term is `min(qd1, qd4 ÷
            // cells)` when the model carries both read rows, `qd4 ÷
            // max(cells, 4)` on a schema-3 file, else the D4 constant —
            // conservative at every cell count.
            ckpt: inf_server::CkptConfig {
                interval_bytes: args.ckpt_interval_bytes,
                replay_bytes_per_s: replay_bytes_per_s_per_cell(
                    io.device.read_bytes_per_s,
                    io.read_bytes_per_s_256k_qd1,
                    args.cells,
                ),
                ..Default::default()
            },
            // Boot-read prefetch (M2.5-S08; ADR-0109): recovery reads hint
            // the next window so cold replay's device read overlaps apply.
            // Only when this cell recovers alone — N parallel recovering
            // cells already saturate the device, and N extra prefetch
            // streams cost sequential locality (the S08 A/B's measured
            // regime split). Boot-scoped by type: the wrapper lives inside
            // `Recovery`; the plane's filesystem stays `StdSegmentFs`.
            recover: inf_server::RecoverConfig {
                boot_prefetch: args.cells == 1,
                ..inf_server::RecoverConfig::default()
            },
            flush_bound: 1,
            fua_p50_us_probed: io.fua_p50_us_4k,
            // ADR-0088 D2/D2b: static per-cell shares, computed once here
            // (L1). Absent ⇒ `Default` ⇒ unbudgeted and unpaced.
            device: inf_server::DeviceConfig {
                model_share: io.device.share(args.cells),
                seal_barriers_per_s: seal_barriers_per_s / u64::from(args.cells.max(1)),
                provenance: *provenance,
            },
            // M4.5-S39a (ADR-0089, third amendment): the fill policy at
            // the design point by default; `--fill-window-us 0` is the
            // pre-S39a cadence (the baseline arm).
            fill: inf_server::FillConfig {
                window: inf_foundation::time::Nanos::from_micros(args.fill_window_us),
                target_bytes: args.fill_target_kib << 10,
            },
            // M4.5-S43 (ADR-0092, campaign K): the FLUSH-class group hold —
            // 250 µs by default since 2026-08-26, `0` the off arm.
            group: inf_server::GroupHoldConfig {
                window: inf_foundation::time::Nanos::from_micros(args.flush_group_window_us),
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
        // The bare filesystem is the plane's for the node's life; the
        // boot-read prefetch rides `cfg.recover.boot_prefetch` inside
        // `Recovery` and never escapes it (ADR-0109).
        plane.begin_recovery(StdSegmentFs, &cfg, cell, StdClock::new().now());
    }
    // Doorbell wakeups (Linux): peers end this cell's park via eventfd, so
    // the park timeout is a fallback, not the hop-latency ceiling. The park
    // board only helps when the driver has a wake watch.
    plane.set_early_fabric_flush(args.early_fabric_flush);
    plane.set_fabric_apply_prefetch(args.fabric_apply_prefetch);
    plane.set_parse_batch_prefetch(args.parse_batch_prefetch);
    plane.set_deasync_dispatch(args.deasync_dispatch);
    #[cfg(feature = "bench-diagnostics")]
    if args.blind_overwrite_ceiling {
        eprintln!(
            "infinityd: BLIND-OVERWRITE CEILING ARM (bench-diagnostics) — unsound, never serve"
        );
        plane.set_blind_overwrite_ceiling(true);
    }
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

/// Sixteen bytes of OS randomness for a key-hash secret (ADR-0094 D2):
/// boot code, blocking, never a fixed fallback.
fn os_entropy() -> std::io::Result<[u8; 16]> {
    use std::io::Read;
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn backend_name() -> &'static str {
    #[cfg(target_os = "linux")]
    return "io_uring";
    #[cfg(target_os = "macos")]
    return "kqueue";
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    "none"
}

/// The checkpoint cap's replay term (ADR-0088 D4 as amended — third
/// amendment, review of 2026-08-28): a conservative interpolation from
/// the two probed geometries, with its assumptions named. The probe
/// measured one reader (`qd1`) and four (`qd4`); nothing in between and
/// nothing beyond. With both rows:
///
/// - one cell: `qd1` — measured directly, no assumption;
/// - two to four cells: `min(qd1, qd4 ÷ 4)` — the **four-reader share**,
///   conservative under A1 (a reader's share does not grow with more
///   contending readers: `agg(n)/n ≥ agg(4)/4` for `n ≤ 4`); the earlier
///   `qd4 ÷ cells` assumed `agg(2) = agg(4)`, which is optimistic;
/// - more than four cells: `min(qd1, qd4 ÷ cells)` — conservative under
///   A2 (the aggregate does not fall as readers grow past four:
///   `agg(n) ≥ agg(4)`), the one assumption a probe at the node's own
///   geometry would remove.
///
/// With the four-reader row only (a schema-3 file): `qd4 ÷ max(cells,
/// 4)`. With the one-reader row only: `qd1 ÷ cells` (A2 again). With
/// neither: the D4 constant. `cells = 0` counts as one.
fn replay_bytes_per_s_per_cell(read_qd4: u64, read_qd1: u64, cells: u16) -> u64 {
    let cells = u64::from(cells.max(1));
    match (read_qd4, read_qd1) {
        (0, 0) => inf_server::DEFAULT_REPLAY_BYTES_PER_S,
        (qd4, 0) => qd4 / cells.max(4),
        (0, qd1) => qd1 / cells,
        (_, qd1) if cells == 1 => qd1,
        (qd4, qd1) => qd1.min(qd4 / cells.max(4)),
    }
}

/// The boot line's account of the rows [`replay_bytes_per_s_per_cell`]
/// used, so a schema-3 file's quarter-share rule is visible as such.
fn replay_term_origin(read_qd4: u64, read_qd1: u64, cells: u16) -> String {
    let cells = cells.max(1);
    match (read_qd4, read_qd1) {
        (0, 0) => "the D4 constant — no probed read row".to_owned(),
        (qd4, 0) => {
            format!("qd4 read row only {} MiB/s (schema-3 file) ÷ max({cells}, 4) cells", qd4 >> 20)
        }
        (0, qd1) => format!("qd1 read row only {} MiB/s ÷ {cells} cells", qd1 >> 20),
        (_, qd1) if cells == 1 => format!("probed read row qd1 {} MiB/s (one cell)", qd1 >> 20),
        (qd4, qd1) if cells <= 4 => format!(
            "min(probed read rows qd1 {} MiB/s, qd4 {} MiB/s ÷ 4 — the four-reader share at \
             {cells} cells, assumption A1)",
            qd1 >> 20,
            qd4 >> 20
        ),
        (qd4, qd1) => format!(
            "min(probed read rows qd1 {} MiB/s, qd4 {} MiB/s ÷ {cells} cells, assumption A2)",
            qd1 >> 20,
            qd4 >> 20
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use inf_foundation::{DeviceIdentity, IdentityVerdict};

    fn props(schema: u64, uuid: &str, block: u32) -> inf_server::IoProperties {
        inf_server::IoProperties {
            probe_schema: schema,
            identity: DeviceIdentity {
                fs_type: "ext4".into(),
                fs_uuid: uuid.into(),
                device_path: "/dev/nvme0n1p3".into(),
                block_logical_bytes: block,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn device(uuid: &str) -> DeviceIdentity {
        DeviceIdentity {
            fs_type: "ext4".into(),
            fs_uuid: uuid.into(),
            device_path: "/dev/nvme0n1p3".into(),
            ..Default::default()
        }
    }

    /// ADR-0091 D1 (review of 2026-08-26): a foreign model is decided by
    /// its identity before its block size is judged — `auto` re-probes
    /// it even when the stale file's block size would be refused; `off`
    /// refuses naming the device, not the block.
    #[test]
    fn a_foreign_model_is_reprobed_or_refused_before_its_block_size_is_judged() {
        let foreign = props(3, "0000-dead", 8192);
        let here = device("c97c5418");
        assert!(matches!(
            existing_file_decision(&foreign, &here, DeviceProbe::Auto),
            FileDecision::Reprobe(reason) if reason.contains("uuid")
        ));
        assert!(matches!(
            existing_file_decision(&foreign, &here, DeviceProbe::Off),
            FileDecision::Refuse(reason) if reason.contains("uuid")
        ));
    }

    /// A model that describes this device is used, and only then does the
    /// block-size rule apply (`check_block_size` refuses 8 KiB blocks).
    #[test]
    fn a_verified_model_is_used_and_its_block_size_is_then_checked() {
        let here = device("c97c5418");
        let ok = props(3, "c97c5418", 512);
        assert_eq!(
            existing_file_decision(&ok, &here, DeviceProbe::Off),
            FileDecision::Use(IdentityVerdict::Verified)
        );
        assert!(check_block_size(&ok).is_ok());
        let wide = props(3, "c97c5418", 8192);
        assert_eq!(
            existing_file_decision(&wide, &here, DeviceProbe::Auto),
            FileDecision::Use(IdentityVerdict::Verified)
        );
        assert!(check_block_size(&wide).unwrap_err().contains("8192 bytes"));
        // Schema ≤ 2 carries no identity: used, unverifiable, either mode.
        let legacy = props(2, "", 512);
        assert_eq!(
            existing_file_decision(&legacy, &here, DeviceProbe::Off),
            FileDecision::Use(IdentityVerdict::Unverifiable)
        );
    }

    /// A freshly probed file's verdict is re-derived from the device: an
    /// empty identity block is `unverifiable`, a matching one `verified`,
    /// and a mismatch is a probe-integrity error, never a silent
    /// `verified`.
    #[test]
    fn a_probed_file_reports_the_verdict_the_device_supports() {
        let here = device("c97c5418");
        let empty = props(3, "", 512);
        let mut empty = empty;
        empty.identity = DeviceIdentity::default();
        assert_eq!(
            probed_identity_verdict(&empty, &DeviceIdentity::default()),
            Ok(IdentityVerdict::Unverifiable)
        );
        assert_eq!(
            probed_identity_verdict(&props(3, "c97c5418", 512), &here),
            Ok(IdentityVerdict::Verified)
        );
        assert!(
            probed_identity_verdict(&props(3, "0000-dead", 512), &here)
                .unwrap_err()
                .contains("does not match the device")
        );
    }

    /// ADR-0088 D4 as amended twice: the replay term is conservative at
    /// every cell count — `min(qd1, qd4 ÷ cells)` with both rows, the
    /// quarter-share rule on a schema-3 file, `qd1 ÷ cells` with the
    /// one-reader row alone, the constant with neither.
    #[test]
    fn replay_term_is_the_probed_read_row_per_cell_or_the_constant() {
        const QD4: u64 = 1_083_000_000;
        const QD1: u64 = 612_000_000;
        // Neither row: the D4 constant at every cell count.
        assert_eq!(replay_bytes_per_s_per_cell(0, 0, 1), inf_server::DEFAULT_REPLAY_BYTES_PER_S);
        assert_eq!(replay_bytes_per_s_per_cell(0, 0, 8), inf_server::DEFAULT_REPLAY_BYTES_PER_S);
        // Both rows: one reader's rate bounds the small node, the
        // aggregate's share the large one (they cross between 1 and 2).
        assert_eq!(replay_bytes_per_s_per_cell(QD4, QD1, 1), QD1);
        // Two and three cells take the four-reader share, not qd4 ÷ cells
        // (the review of 2026-08-28: a two-reader aggregate can be lower
        // than the four-reader one — `qd4 ÷ 2` was unsupported).
        assert_eq!(replay_bytes_per_s_per_cell(QD4, QD1, 2), 270_750_000);
        assert_eq!(replay_bytes_per_s_per_cell(QD4, QD1, 3), 270_750_000);
        assert_eq!(replay_bytes_per_s_per_cell(QD4, QD1, 4), 270_750_000);
        assert_eq!(replay_bytes_per_s_per_cell(QD4, QD1, 8), 135_375_000);
        // A one-reader row below the four-reader share binds everywhere.
        assert_eq!(replay_bytes_per_s_per_cell(QD4, 200_000_000, 2), 200_000_000);
        // The four-reader row only (a schema-3 file): never more than a
        // quarter share, even at one cell.
        assert_eq!(replay_bytes_per_s_per_cell(QD4, 0, 1), 270_750_000);
        assert_eq!(replay_bytes_per_s_per_cell(QD4, 0, 4), 270_750_000);
        assert_eq!(replay_bytes_per_s_per_cell(QD4, 0, 8), 135_375_000);
        // The one-reader row only: divided across the cells.
        assert_eq!(replay_bytes_per_s_per_cell(0, QD1, 1), QD1);
        assert_eq!(replay_bytes_per_s_per_cell(0, QD1, 8), 76_500_000);
        // Zero cells counts as one.
        assert_eq!(replay_bytes_per_s_per_cell(QD4, QD1, 0), QD1);
        assert_eq!(replay_bytes_per_s_per_cell(QD4, 0, 0), 270_750_000);
        // The boot line names the rows it used.
        assert!(replay_term_origin(0, 0, 4).contains("D4 constant"));
        assert!(replay_term_origin(QD4, 0, 1).contains("schema-3 file"));
        assert!(replay_term_origin(QD4, 0, 1).contains("max(1, 4)"));
        assert!(replay_term_origin(0, QD1, 8).contains("qd1 read row only"));
        assert!(replay_term_origin(QD4, QD1, 2).contains("four-reader share at 2 cells"));
        assert!(replay_term_origin(QD4, QD1, 8).contains("÷ 8 cells, assumption A2"));
        assert!(replay_term_origin(QD4, QD1, 1).contains("(one cell)"));
    }
}
