#![allow(
    clippy::disallowed_methods,
    reason = "inf-compare is the comparator load generator: the wall clock is its instrument; it \
         drives engines over the wire and runs no cell"
)]
//! `inf-compare` — InfinityDB competitive benchmark harness.
//!
//! Drives the industry-standard load generators (memtier_benchmark,
//! redis-benchmark) against redis, dragonfly, and infinitydb on one box and
//! renders a single markdown report (throughput / latency / RSS / bytes-key).
//! It is the competitor-anchored complement to `inf-bench`'s in-house loadgen —
//! the "independent generator" cross-check the master plan §22 requires.
//!
//! Tooling tier: blocking sockets and `std::thread` are fine; it never touches
//! the data plane and shares no code (and no dependency surface) with the
//! system under test.
#![forbid(unsafe_code)]

mod affinity;
mod campaign;
mod cli;
mod engine;
mod env;
mod json;
mod memtier;
mod redisbench;
mod report;
mod resp;
mod workload;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use cli::Flags;
use engine::{EngineKind, Images, Mode, Spec, Target};
use report::{Cell, EngineConfig, MemCell, Params};
use workload::{Kind, Workload};

const USAGE: &str = "\
inf-compare — InfinityDB competitive benchmark harness

USAGE:
    inf-compare run [options]
    inf-compare list-workloads
    inf-compare help

OPTIONS (run):
    --engines       redis,dragonfly,infinitydb   # default: all present on host/docker
    --generator     both | memtier | redis-benchmark   # default: both
    --workload      set|mixed|get|incr|mset|ttl|memory|eviction|all   # default: all
    --replicates    N               # default: 3; 1–5 dev, 3–5 reference; rotating order
    --duration      SECS            # memtier test-time per row; default: 30
    --threads       N               # → infinityd --cells, dragonfly --proactor_threads; default: 4
    --clients       N               # connections per generator thread; default: 50
    --pipeline      1,16            # comma list, one row each; default: 1,16
    --data-size     BYTES           # value size; default: 64
    --keyspace      N               # key space (--key-maximum / -r); default: 1000000
    --maxmemory-mb  N               # cap all engines (allkeys-lru); enables `eviction`
    --rb-requests   N               # redis-benchmark request count (-n); default: 1000000
    --crosscheck-threshold PCT      # flag memtier/redis-benchmark divergence; default: 25
    --rate          OPS_PER_SEC     # offered rate, total across connections (memtier
                                    # --rate-limiting per connection = rate / (threads ×
                                    # clients)); default: closed loop
    --durability    none|everysec   # everysec: redis --appendonly yes --appendfsync everysec,
                                    # infinitydb FSYNC everysec namespace every connection starts in
                                    # (--conn-default-ns); host launches only; Dragonfly refused
                                    # (no equivalent mode); default: none
    --data-root     DIR             # durable state root (per-engine subdirs, wiped);
                                    # default: .artifacts/compare-data
    --probe-file    PATH            # io-properties.toml copied into infinitydb's data dir
                                    # (barrier class)
    --device-stat   DEV             # /sys/block/DEV/stat sectors-written sampled per row
                                    # (e.g. nvme0n1)
    --redis-no-auto-rewrite         # diagnostic only: Redis auto-aof-rewrite-percentage 0

  Placement:
    --docker        # run servers in containers; generator stays on host
    --attach        redis=127.0.0.1:6379,dragonfly=...   # use running servers, skip launch
    --port-base     N               # launched legs get N, N+1, ...; default: 7000
    --pin-start     CORE            # server process mask: CORE .. CORE+threads-1 (Linux host)
    --load-pin-start CORE           # disjoint generator mask; required with --pin-start
    --load-cpus     N               # generator mask width; default: --threads

  Docker images (with --docker):
    --redis-image       (default redis:8.0.5)
    --dragonfly-image   (default docker.dragonflydb.io/dragonflydb/dragonfly)
    --infinitydb-image  (default infinitydb:dev)
    --seccomp PATH      (default deploy/seccomp/infinitydb-seccomp.json)

  Evidence:
    --out DIR           # artifacts root; default: .artifacts/compare
    --reference-box     # needs both CPU ranges and a clean box: env-check must pass
    --unsafe-env        # proceed on a non-clean box (stamps the run non-citable)";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first() else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    let outcome = match cmd.as_str() {
        "run" => cmd_run(&args[1..]),
        "list-workloads" => {
            print_workloads();
            Ok(())
        }
        "help" | "--help" | "-h" => {
            println!("{USAGE}");
            Ok(())
        }
        other => Err(format!("unknown subcommand `{other}`\n\n{USAGE}")),
    };
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("inf-compare: {msg}");
            ExitCode::FAILURE
        }
    }
}

const BOOL_FLAGS: &[&str] = &["docker", "reference-box", "unsafe-env", "redis-no-auto-rewrite"];
const VALUE_FLAGS: &[&str] = &[
    "engines",
    "generator",
    "workload",
    "duration",
    "replicates",
    "threads",
    "clients",
    "pipeline",
    "data-size",
    "keyspace",
    "maxmemory-mb",
    "rb-requests",
    "crosscheck-threshold",
    "rate",
    "durability",
    "data-root",
    "probe-file",
    "device-stat",
    "attach",
    "port-base",
    "pin-start",
    "load-pin-start",
    "load-cpus",
    "redis-image",
    "redis-stack-image",
    "dragonfly-image",
    "infinitydb-image",
    "seccomp",
    "out",
];

#[derive(Clone, Copy)]
struct Generators {
    memtier: bool,
    redisbench: bool,
}

struct LoadParams {
    threads: u16,
    load_cpus: Option<affinity::CpuRange>,
    clients: usize,
    duration: u64,
    data_size: usize,
    keyspace: u64,
    rb_requests: u64,
    /// M4.5-S40: offered rate (total ops/s), `None` = closed loop.
    rate: Option<u64>,
    durability: engine::Durability,
    data_root: PathBuf,
    probe_file: Option<PathBuf>,
    device_stat: Option<String>,
    redis_no_auto_rewrite: bool,
}

fn cmd_run(args: &[String]) -> Result<(), String> {
    let known: Vec<&str> = BOOL_FLAGS.iter().chain(VALUE_FLAGS).copied().collect();
    let flags = Flags::parse(args, BOOL_FLAGS, &known)?;

    let replicates = campaign::replicates(&flags)?;

    // --- placement + generators ---
    let docker = flags.bool("docker");
    let generators = parse_generators(&flags.str_or("generator", "both"))?;
    let attach = parse_attach(flags.get("attach"))?;
    let images = Images {
        redis: flags.str_or("redis-image", "redis:8.0.5"),
        // Mirrors the pinned M3 compat oracle (tests/compat json_oracle.rs).
        redis_stack: flags.str_or(
            "redis-stack-image",
            "redis/redis-stack-server@sha256:798ab84d9f266936b034ab11c4d04a2b8e4b441884c5aa7d17ac9\
                 51eefdf742a",
        ),
        dragonfly: flags.str_or("dragonfly-image", "docker.dragonflydb.io/dragonflydb/dragonfly"),
        infinitydb: flags.str_or("infinitydb-image", "infinitydb:dev"),
        seccomp: PathBuf::from(flags.str_or("seccomp", "deploy/seccomp/infinitydb-seccomp.json")),
    };

    // --- engine set: explicit, else attach keys, else everything available ---
    let engines = resolve_engines(&flags, docker, &attach, &images)?;
    let workloads = workload::select(&flags.str_or("workload", "all"))?;

    // --- sizing ---
    let threads = flags.u16_or("threads", 4)?;
    let lp = LoadParams {
        threads,
        load_cpus: None,
        clients: flags.usize_or("clients", 50)?,
        duration: flags.u64_or("duration", 30)?,
        data_size: flags.usize_or("data-size", 64)?,
        keyspace: flags.u64_or("keyspace", 1_000_000)?,
        rb_requests: flags.u64_or("rb-requests", 1_000_000)?,
        rate: flags.opt_u64("rate")?,
        durability: engine::Durability::parse(&flags.str_or("durability", "none"))?,
        data_root: PathBuf::from(flags.str_or("data-root", ".artifacts/compare-data")),
        probe_file: flags.get("probe-file").map(PathBuf::from),
        device_stat: flags.get("device-stat").map(str::to_string),
        redis_no_auto_rewrite: flags.bool("redis-no-auto-rewrite"),
    };
    let pipelines = flags.u32_list_or("pipeline", &[1, 16])?;
    let maxmemory_mb = flags.opt_u64("maxmemory-mb")?;
    let port_base = flags.u16_or("port-base", 7000)?;
    let crosscheck_pct = flags.f64_or("crosscheck-threshold", 25.0)?;
    let out = flags.str_or("out", ".artifacts/compare");
    let reference_box = flags.bool("reference-box");
    let unsafe_env = flags.bool("unsafe-env");
    let fill_secs = lp.duration.clamp(2, 5);
    campaign::validate(&engines, &workloads, &pipelines)?;
    let legs = campaign::leg_count(engines.len(), &workloads, pipelines.len(), replicates);
    let last_offset = u16::try_from(legs - 1).map_err(|_| "too many campaign legs")?;
    if engines.iter().any(|kind| !attach.contains_key(kind))
        && (port_base == 0 || port_base.checked_add(last_offset).is_none())
    {
        return Err("--port-base must give every launched leg a port in 1–65535".into());
    }

    for &kind in &engines {
        kind.validate_durability(lp.durability)?;
    }
    if lp.durability != engine::Durability::None
        && (docker || engines.iter().any(|kind| attach.contains_key(kind)))
    {
        return Err("--durability everysec requires host launches; drop --docker and --attach \
            (attached persistence is unverified)"
            .into());
    }

    if workloads.iter().any(|w| w.name == "eviction") && maxmemory_mb.is_none() {
        eprintln!(
            "inf-compare: WARNING — `eviction` without --maxmemory-mb is just a write storm (no \
                 cap)"
        );
    }

    // --- environment + tier; refuse a non-clean binding run ---
    let environment = env::gather(reference_box, unsafe_env);
    if reference_box && !environment.binding && !unsafe_env {
        return Err(format!(
            "refusing a binding run on a non-clean box:\n  - {}\nfix the box, or pass --unsafe-env \
                 to proceed non-citably",
            environment.reasons.join("\n  - ")
        ));
    }
    if !docker
        && attach.is_empty()
        && lp.durability != engine::Durability::None
        && engines.contains(&EngineKind::Redis)
    {
        let ambient = engine::ambient_redis_processes();
        if !ambient.is_empty() {
            return Err(format!(
                "refusing durable comparison with unrelated redis-server process(es):\n  - \
                     {}\nstop them before the campaign",
                ambient.join("\n  - ")
            ));
        }
    }

    let placement = affinity::placement(
        &flags,
        threads,
        docker,
        engines.iter().any(|kind| attach.contains_key(kind)),
    )?;
    let pin_start = placement.map(|placement| placement.server.start());
    let lp = LoadParams { load_cpus: placement.map(|placement| placement.load), ..lp };

    // --- artifact layout ---
    let stamp_secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH).map_err(|e| e.to_string())?.as_nanos();
    let run_dir = PathBuf::from(&out).join(format!("{nonce}-compare"));
    std::fs::create_dir_all(&out).map_err(|error| format!("create output root: {error}"))?;
    std::fs::create_dir(&run_dir).map_err(|error| format!("create unique run: {error}"))?;
    let placement_description = affinity::description(placement);
    std::fs::write(run_dir.join("placement.txt"), &placement_description)
        .map_err(|error| format!("write CPU placement: {error}"))?;
    let raw_dir = run_dir.join("raw");
    let log_dir = run_dir.join("logs");
    std::fs::create_dir_all(&raw_dir).map_err(|e| format!("create {}: {e}", raw_dir.display()))?;
    std::fs::create_dir_all(&log_dir).map_err(|e| format!("create {}: {e}", log_dir.display()))?;

    let mode_label = mode_label(docker, &attach, &engines);
    eprintln!("inf-compare: {placement_description}");
    eprintln!(
        "inf-compare: {} · {mode_label} · {} · {} engine(s) · {} workload(s) · pipeline \
             {pipelines:?}",
        environment.tier,
        generators_label(generators),
        engines.len(),
        workloads.len()
    );

    // --- rotate engines inside each scenario and replicate ---
    let campaign = campaign::Campaign {
        engines: &engines,
        attach: &attach,
        docker,
        port_base,
        load: &lp,
        pin_start,
        maxmemory_mb,
        images: &images,
        generators,
        run_dir: &run_dir,
        replicates,
    };
    let campaign::Results { cells, memory: mems, configs } =
        campaign.run(&workloads, &pipelines)?;

    // --- render + persist ---
    let params = Params {
        stamp_secs,
        replicates,
        mode: mode_label,
        generators: generators_label(generators).to_string(),
        duration: lp.duration,
        threads: lp.threads,
        placement,
        clients: lp.clients,
        data_size: lp.data_size,
        keyspace: lp.keyspace,
        pipelines,
        fill_secs,
        rb_requests: lp.rb_requests,
        crosscheck_pct,
        maxmemory_mb,
        rate: lp.rate,
        data_root: (lp.durability != engine::Durability::None)
            .then(|| lp.data_root.display().to_string()),
        device_stat: lp.device_stat.clone(),
        redis_no_auto_rewrite: lp.redis_no_auto_rewrite,
    };
    let md = report::render(&environment, &params, &configs, &cells, &mems);
    let report_path = run_dir.join("report.md");
    let pending_report = run_dir.join("report.md.tmp");
    std::fs::write(&pending_report, md)
        .and_then(|()| std::fs::rename(&pending_report, &report_path))
        .map_err(|e| format!("publish report: {e}"))?;

    println!(
        "\ninf-compare: {} latency/throughput rows + {} memory rows across {} engine(s)",
        cells.len(),
        mems.len(),
        engines.len()
    );
    println!("inf-compare: report → {}", report_path.display());
    Ok(())
}

/// Attach, docker-launch, or host-launch `kind`.
#[allow(clippy::too_many_arguments)]
fn bring_up(
    kind: EngineKind,
    attach: &BTreeMap<EngineKind, (String, u16)>,
    docker: bool,
    port: u16,
    lp: &LoadParams,
    pin_start: Option<usize>,
    maxmemory_mb: Option<u64>,
    images: &Images,
    log_dir: &Path,
) -> Result<Target, String> {
    if let Some((host, aport)) = attach.get(&kind) {
        eprintln!("inf-compare: attaching {} at {host}:{aport}", kind.label());
        return engine::attach(kind, host, *aport);
    }
    let spec = Spec {
        kind,
        host: "127.0.0.1".to_string(),
        port,
        threads: lp.threads,
        pin_start,
        maxmemory_mb,
        durability: lp.durability,
        data_dir: (lp.durability != engine::Durability::None)
            .then(|| lp.data_root.join(kind.label())),
        probe_file: lp.probe_file.clone(),
        redis_no_auto_rewrite: lp.redis_no_auto_rewrite,
    };
    if docker {
        eprintln!("inf-compare: launching {} (docker) on :{port}", kind.label());
        engine::launch_docker(&spec, images, log_dir)
    } else {
        eprintln!("inf-compare: launching {} (host) on :{port}", kind.label());
        engine::launch_host(&spec, log_dir)
    }
}

/// Run every workload against one launched/attached engine.
#[allow(clippy::too_many_arguments)]
fn bench_engine(
    target: &Target,
    workloads: &[Workload],
    pipelines: &[u32],
    lp: &LoadParams,
    gens: Generators,
    raw_dir: &Path,
    fill_secs: u64,
    mem_fill_secs: u64,
    _crosscheck_pct: f64,
) -> Result<(Vec<Cell>, Vec<MemCell>), String> {
    let label = target.kind.label();
    let mt_plan = memtier::Plan {
        host: &target.host,
        cpus: lp.load_cpus,
        port: target.port,
        threads: lp.threads,
        clients: lp.clients,
        duration: lp.duration,
        data_size: lp.data_size,
        keyspace: lp.keyspace,
        rate: lp.rate,
    };
    let rate_tag = lp.rate.map_or(String::new(), |r| format!("-r{r}"));

    let mut cells = Vec::new();
    let mut mems = Vec::new();
    for wl in workloads {
        if wl.requires_json && !target.kind.has_json() {
            // Visible in the run log; the report simply carries no row for
            // this engine × lane (an absent row is not a zero).
            eprintln!("inf-compare:   {label} {} SKIPPED (no JSON.* surface)", wl.name);
            continue;
        }
        if matches!(wl.kind, Kind::Memory) {
            eprintln!("inf-compare:   {label} memory (fill {mem_fill_secs}s + DBSIZE)");
            mems.push(measure_memory(target, &mt_plan, mem_fill_secs, lp.data_size)?);
            continue;
        }
        for &pipeline in pipelines {
            // Campaigns launch a fresh server/data directory for every durable leg.
            if lp.durability == engine::Durability::None {
                engine::flushall(target)?;
            }
            if wl.needs_fill && wl.requires_json {
                // Document preload: every key in the keyspace holds the
                // lane document, so the read lane never measures misses.
                eprintln!("inf-compare:   {label} {} document preload", wl.name);
                resp::json_fill(&target.host, target.port, lp.keyspace, workload::JSON_DOC_1K)?;
            } else if wl.needs_fill {
                eprintln!("inf-compare:   {label} {} populate ({fill_secs}s)", wl.name);
                memtier::fill(&mt_plan, fill_secs)?;
            }
            let observation_before = engine::observe(target)?;
            let info_tag = format!("{label}-{}-p{pipeline}{rate_tag}", wl.name);
            std::fs::write(
                raw_dir.join(format!("{info_tag}.info.before.txt")),
                &observation_before.raw_info,
            )
            .map_err(|e| format!("write INFO before: {e}"))?;
            let sectors_before = lp.device_stat.as_deref().and_then(engine::device_sectors_written);
            let wall = std::time::Instant::now();
            let memtier_m = if gens.memtier {
                eprintln!(
                    "inf-compare:   {label} {} memtier pipeline={pipeline}{rate_tag}",
                    wl.name
                );
                let json =
                    raw_dir.join(format!("{label}-{}-p{pipeline}{rate_tag}.memtier.json", wl.name));
                Some(memtier::run(&mt_plan, wl, pipeline, &json)?)
            } else {
                None
            };
            // Server CPU across the memtier row (host launches) and the
            // block device's sectors written — the S40 disclosures.
            let wall_s = wall.elapsed().as_secs_f64();
            let observation_after = engine::observe(target)?;
            std::fs::write(
                raw_dir.join(format!("{info_tag}.info.after.txt")),
                &observation_after.raw_info,
            )
            .map_err(|e| format!("write INFO after: {e}"))?;
            let server_cpu_pct =
                match (observation_before.cpu_seconds, observation_after.cpu_seconds) {
                    (Some(a), Some(b)) if wall_s > 0.0 => Some((b - a).max(0.0) / wall_s * 100.0),
                    _ => None,
                };
            let device_mib_written = match (
                sectors_before,
                lp.device_stat.as_deref().and_then(engine::device_sectors_written),
            ) {
                (Some(a), Some(b)) => Some((b.saturating_sub(a) * 512) as f64 / (1024.0 * 1024.0)),
                _ => None,
            };
            let redisbench_m = match (gens.redisbench, wl.redisbench_test) {
                (true, Some(test)) => {
                    eprintln!(
                        "inf-compare:   {label} {} redis-benchmark -t {test} pipeline={pipeline}",
                        wl.name
                    );
                    let rb_plan = redisbench::Plan {
                        host: &target.host,
                        cpus: lp.load_cpus,
                        port: target.port,
                        requests: lp.rb_requests,
                        clients: lp.clients,
                        data_size: lp.data_size,
                        keyspace: lp.keyspace,
                    };
                    let csv = raw_dir
                        .join(format!("{label}-{}-p{pipeline}{rate_tag}.redisbench.csv", wl.name));
                    Some(redisbench::run(&rb_plan, test, pipeline, &csv)?)
                }
                _ => None,
            };
            if memtier_m.is_none() && redisbench_m.is_none() {
                continue; // nothing measured for this generator/workload combo
            }
            cells.push(Cell {
                replicate: 0,
                ordinal: 0,
                engine: label,
                workload: wl.name,
                pipeline,
                memtier: memtier_m,
                redisbench: redisbench_m,
                rss_mib: engine::rss_now_mib(target),
                server_cpu_pct,
                device_mib_written,
                persistence_delta: Some(engine::observation_delta(
                    target.kind,
                    &observation_before,
                    &observation_after,
                )),
            });
        }
    }
    Ok((cells, mems))
}

/// bytes/key = (RSS_after − RSS_baseline) ÷ DBSIZE after a fill.
fn measure_memory(
    target: &Target,
    plan: &memtier::Plan,
    fill_secs: u64,
    data_size: usize,
) -> Result<MemCell, String> {
    engine::flushall(target)?;
    let baseline = engine::rss_now_mib(target);
    memtier::fill(plan, fill_secs)?;
    let keys = engine::dbsize(target)?;
    let after = engine::rss_now_mib(target);
    let bytes_per_key = match (baseline, after) {
        (Some(b), Some(a)) if keys > 0 && a > b => Some((a - b) * 1024.0 * 1024.0 / keys as f64),
        _ => None,
    };
    Ok(MemCell {
        replicate: 0,
        ordinal: 0,
        engine: target.kind.label(),
        keys,
        value_size: data_size,
        baseline_mib: baseline,
        after_mib: after,
        bytes_per_key,
    })
}

// ---- option resolution --------------------------------------------------

fn parse_generators(s: &str) -> Result<Generators, String> {
    match s {
        "memtier" => Ok(Generators { memtier: true, redisbench: false }),
        "redis-benchmark" | "redisbench" => Ok(Generators { memtier: false, redisbench: true }),
        "both" => Ok(Generators { memtier: true, redisbench: true }),
        other => Err(format!("unknown --generator `{other}` (memtier | redis-benchmark | both)")),
    }
}

fn generators_label(g: Generators) -> &'static str {
    match (g.memtier, g.redisbench) {
        (true, true) => "memtier + redis-benchmark",
        (true, false) => "memtier",
        (false, true) => "redis-benchmark",
        (false, false) => "none",
    }
}

fn parse_attach(s: Option<&str>) -> Result<BTreeMap<EngineKind, (String, u16)>, String> {
    let mut map = BTreeMap::new();
    let Some(s) = s else { return Ok(map) };
    for entry in s.split(',').filter(|e| !e.trim().is_empty()) {
        let (k, addr) = entry
            .split_once('=')
            .ok_or_else(|| format!("--attach `{entry}` must be engine=host:port"))?;
        let kind = EngineKind::parse(k.trim())?;
        let (host, port) = addr
            .trim()
            .rsplit_once(':')
            .ok_or_else(|| format!("--attach `{addr}` must be host:port"))?;
        let port: u16 =
            port.trim().parse().map_err(|_| format!("--attach port `{port}` invalid"))?;
        map.insert(kind, (host.trim().to_string(), port));
    }
    Ok(map)
}

fn resolve_engines(
    flags: &Flags,
    docker: bool,
    attach: &BTreeMap<EngineKind, (String, u16)>,
    images: &Images,
) -> Result<Vec<EngineKind>, String> {
    if let Some(list) = flags.get("engines") {
        let mut out = Vec::new();
        for name in list.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            let kind = EngineKind::parse(name)?;
            if !out.contains(&kind) {
                out.push(kind);
            }
        }
        return Ok(out);
    }
    if !attach.is_empty() {
        return Ok(attach.keys().copied().collect());
    }
    let all = [EngineKind::Redis, EngineKind::Dragonfly, EngineKind::InfinityDb];
    let available: Vec<EngineKind> = all
        .into_iter()
        .filter(|k| {
            if docker { docker_image_present(image_for(*k, images)) } else { k.host_available() }
        })
        .collect();
    if available.is_empty() {
        return Err(format!(
            "no engines available ({}); pass --engines explicitly",
            if docker { "no local docker images" } else { "no engine binaries on PATH" }
        ));
    }
    Ok(available)
}

fn image_for(kind: EngineKind, images: &Images) -> &str {
    match kind {
        EngineKind::Redis => &images.redis,
        EngineKind::RedisStack => &images.redis_stack,
        EngineKind::Dragonfly => &images.dragonfly,
        EngineKind::InfinityDb => &images.infinitydb,
    }
}

fn docker_image_present(image: &str) -> bool {
    Command::new("docker")
        .args(["image", "inspect", image])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn mode_label(
    docker: bool,
    attach: &BTreeMap<EngineKind, (String, u16)>,
    engines: &[EngineKind],
) -> String {
    let all_attached = !attach.is_empty() && engines.iter().all(|k| attach.contains_key(k));
    if all_attached {
        "attach".to_string()
    } else if docker && !attach.is_empty() {
        "docker + attach".to_string()
    } else if docker {
        "docker (containers; generator on host)".to_string()
    } else if !attach.is_empty() {
        "host + attach".to_string()
    } else {
        "host".to_string()
    }
}

fn print_workloads() {
    println!("inf-compare workloads (gated to the M1 string surface):\n");
    for w in workload::catalog() {
        let rb = w.redisbench_test.map(|t| format!(", redis-benchmark -t {t}")).unwrap_or_default();
        let tag = if w.in_all { "" } else { "  [opt-in]" };
        println!("  {:<9} {}{rb}{tag}", w.name, w.about);
    }
    println!("\n  all        — every workload not marked [opt-in]");
}
