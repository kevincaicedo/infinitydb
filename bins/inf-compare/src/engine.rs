//! Launch, probe, sample, and tear down a system-under-test.
//!
//! Three modes, one [`Target`] type:
//! - **host** — spawn the binary as a child process (RSS via `/proc`).
//! - **docker** — `docker run -d` a container (RSS via `docker stats`);
//!   infinitydb needs the io_uring seccomp profile because its only Linux
//!   backend is io_uring and Docker's default seccomp denies it.
//! - **attach** — talk to an already-running server at host:port (no RSS).
//!
//! Each engine's exact launch command is recorded for the report's "published
//! configs" section (master plan §22: no comparison without the competitor in
//! the run, configs published).

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::resp;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum EngineKind {
    Redis,
    /// The pinned RedisJSON comparator (M3 `json` lanes, ADR-0025 D3).
    /// Docker-only: redis-stack ships no host binary.
    RedisStack,
    Dragonfly,
    InfinityDb,
}

impl EngineKind {
    pub fn label(self) -> &'static str {
        match self {
            EngineKind::Redis => "redis",
            EngineKind::RedisStack => "redis-stack",
            EngineKind::Dragonfly => "dragonfly",
            EngineKind::InfinityDb => "infinitydb",
        }
    }

    pub fn parse(s: &str) -> Result<EngineKind, String> {
        match s {
            "redis" => Ok(EngineKind::Redis),
            "redis-stack" | "redisstack" | "stack" => Ok(EngineKind::RedisStack),
            "dragonfly" | "df" => Ok(EngineKind::Dragonfly),
            "infinitydb" | "infinity" | "infinityd" => Ok(EngineKind::InfinityDb),
            other => Err(format!(
                "unknown engine `{other}` (known: redis, redis-stack, dragonfly, infinitydb)"
            )),
        }
    }

    /// `true` if this engine can be host-launched on this box right now.
    pub fn host_available(self) -> bool {
        match self {
            EngineKind::Redis => on_path("redis-server"),
            EngineKind::RedisStack => false, // docker-only
            EngineKind::Dragonfly => on_path("dragonfly"),
            EngineKind::InfinityDb => resolve_infinityd().exists() || on_path("infinityd"),
        }
    }

    /// `true` if the engine serves the `JSON.*` surface (`json` lanes run;
    /// otherwise the lane is skipped with a report note).
    pub fn has_json(self) -> bool {
        match self {
            EngineKind::Redis => false,
            // Dragonfly ships JSON.* natively; redis-stack is the pinned
            // oracle; InfinityDb is the M3 system under test.
            EngineKind::RedisStack | EngineKind::Dragonfly | EngineKind::InfinityDb => true,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Host,
    Docker,
    Attach,
}

impl Mode {
    fn label(self) -> &'static str {
        match self {
            Mode::Host => "host",
            Mode::Docker => "docker",
            Mode::Attach => "attach",
        }
    }
}

/// Durability class every engine in a run is configured for (M4.5-S40):
/// `None` is the in-memory row (redis `--appendonly no`, infinitydb's
/// default dbs); `Everysec` is redis `--appendonly yes --appendfsync
/// everysec` against an infinitydb `FSYNC everysec` namespace every
/// connection starts in (`--conn-default-ns`) — the same loss window on
/// both sides, each engine's own mechanism.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Durability {
    None,
    Everysec,
}

impl Durability {
    pub fn parse(s: &str) -> Result<Durability, String> {
        match s {
            "none" => Ok(Durability::None),
            "everysec" => Ok(Durability::Everysec),
            other => Err(format!("--durability {other}: expected none|everysec")),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Durability::None => "none (in-memory)",
            Durability::Everysec => "everysec",
        }
    }
}

/// The namespace infinitydb's connections start in under a durable run.
pub const DURABLE_NS: &str = "cmp";

/// How to launch one engine.
#[derive(Clone, Debug)]
pub struct Spec {
    pub kind: EngineKind,
    pub host: String,
    pub port: u16,
    pub threads: u16,
    pub pin_start: Option<usize>,
    pub maxmemory_mb: Option<u64>,
    pub durability: Durability,
    /// Per-engine durable state directory (wiped at launch); `None`
    /// under `Durability::None`.
    pub data_dir: Option<PathBuf>,
    /// `io-properties.toml` copied into infinitydb's data dir (the
    /// probed barrier class and device model); absent = FLUSH class.
    pub probe_file: Option<PathBuf>,
}

/// Docker image references (defaults overridable on the CLI).
#[derive(Clone, Debug)]
pub struct Images {
    pub redis: String,
    /// Pinned by digest to the M3 compat oracle's exact image
    /// (`tests/compat/src/json_oracle.rs` is the canonical constant; this
    /// default mirrors it because inf-compare shares no code with the
    /// system under test or its test crates — zero-dep policy).
    pub redis_stack: String,
    pub dragonfly: String,
    pub infinitydb: String,
    pub seccomp: PathBuf,
}

/// A launched-or-attached engine. Dropping it does NOT stop it — call
/// [`teardown`].
#[derive(Debug)]
pub struct Target {
    pub kind: EngineKind,
    pub host: String,
    pub port: u16,
    pub mode: Mode,
    pub version: String,
    pub launch_cmd: String,
    pid: Option<u32>,
    child: Option<Child>,
    container: Option<String>,
}

impl Target {
    pub fn mode_label(&self) -> &'static str {
        self.mode.label()
    }
}

/// Spawn `spec` as a host child process, wait until it answers `PING`.
pub fn launch_host(spec: &Spec, log_dir: &Path) -> Result<Target, String> {
    prepare_data_dir(spec)?;
    let (program, argv) = host_argv(spec)?;
    let launch_cmd = render_cmd(&program, &argv);
    let child = spawn(&program, &argv, log_dir, spec.kind.label())?;
    let pid = child.id();
    wait_ready(&spec.host, spec.port, Duration::from_secs(20))
        .map_err(|e| format!("{} (host): {e}", spec.kind.label()))?;
    durable_setup(spec)?;
    let version = host_version(&program, &argv);
    Ok(Target {
        kind: spec.kind,
        host: spec.host.clone(),
        port: spec.port,
        mode: Mode::Host,
        version,
        launch_cmd,
        pid: Some(pid),
        child: Some(child),
        container: None,
    })
}

/// A fresh durable state directory for the engine (M4.5-S40): wiped and
/// recreated so every run starts from an empty log/AOF; the probe file
/// is copied in for infinitydb.
fn prepare_data_dir(spec: &Spec) -> Result<(), String> {
    let Some(dir) = &spec.data_dir else { return Ok(()) };
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    if spec.kind == EngineKind::InfinityDb
        && let Some(probe) = &spec.probe_file
    {
        std::fs::copy(probe, dir.join("io-properties.toml"))
            .map_err(|e| format!("copy {}: {e}", probe.display()))?;
    }
    Ok(())
}

/// Post-launch durable setup (M4.5-S40): infinitydb's `everysec`
/// namespace, created before any generator connection is opened so
/// `--conn-default-ns` puts every one of them in it. Redis needs
/// nothing: its AOF is a server-wide setting.
fn durable_setup(spec: &Spec) -> Result<(), String> {
    if spec.durability == Durability::Everysec && spec.kind == EngineKind::InfinityDb {
        let reply = resp::command(
            &spec.host,
            spec.port,
            &[
                b"INF.NS",
                b"CREATE",
                DURABLE_NS.as_bytes(),
                b"MODE",
                b"durable",
                b"FSYNC",
                b"everysec",
            ],
        )?;
        if !reply.starts_with(b"+OK") {
            return Err(format!(
                "infinitydb INF.NS CREATE {DURABLE_NS}: {}",
                String::from_utf8_lossy(&reply).trim()
            ));
        }
        // Prove a fresh connection lands in the namespace (the knob is
        // what makes a memtier row durable at all): a probe SET on a
        // fresh connection must be visible under `cmp` and absent from
        // db0 — then it is removed.
        resp::command(&spec.host, spec.port, &[b"SET", b"__inf_compare_probe", b"1"])?;
        let in_db0 = resp::commands(
            &spec.host,
            spec.port,
            &[&[b"INF.NS", b"USE", b"db0"], &[b"EXISTS", b"__inf_compare_probe"]],
        )?;
        let in_ns = resp::commands(
            &spec.host,
            spec.port,
            &[&[b"INF.NS", b"USE", DURABLE_NS.as_bytes()], &[b"EXISTS", b"__inf_compare_probe"]],
        )?;
        if !in_db0.starts_with(b":0") || !in_ns.starts_with(b":1") {
            return Err(format!(
                "infinitydb --conn-default-ns {DURABLE_NS} did not take (probe in db0: {}, in \
                 {DURABLE_NS}: {})",
                String::from_utf8_lossy(&in_db0).trim(),
                String::from_utf8_lossy(&in_ns).trim()
            ));
        }
        resp::command(&spec.host, spec.port, &[b"DEL", b"__inf_compare_probe"])?;
    }
    Ok(())
}

/// `docker run -d` `spec` as a container; wait until it answers `PING`.
pub fn launch_docker(spec: &Spec, images: &Images, log_dir: &Path) -> Result<Target, String> {
    if spec.durability != Durability::None {
        return Err("--durability is a host-launch row (the data dir is a host path); drop \
                    --docker or attach"
            .into());
    }
    let name = format!("inf-compare-{}-{}", spec.kind.label(), spec.port);
    // Best-effort: clear a stale container of the same name from a prior run.
    let _ = Command::new("docker")
        .args(["rm", "-f", &name])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    let argv = docker_argv(spec, images, &name)?;
    let launch_cmd = render_cmd("docker", &argv);
    let out = Command::new("docker")
        .args(&argv)
        .output()
        .map_err(|e| format!("run docker: {e} (is docker installed?)"))?;
    if !out.status.success() {
        return Err(format!(
            "docker run {}: {}",
            spec.kind.label(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    // Persist the container id for debugging next to the host logs.
    let _ = std::fs::write(log_dir.join(format!("{}.container", spec.kind.label())), &out.stdout);

    wait_ready(&spec.host, spec.port, Duration::from_secs(30)).map_err(|e| {
        let logs = Command::new("docker").args(["logs", "--tail", "20", &name]).output();
        let tail =
            logs.map(|o| String::from_utf8_lossy(&o.stderr).into_owned()).unwrap_or_default();
        format!("{} (docker): {e}\n--- docker logs {name} ---\n{tail}", spec.kind.label())
    })?;
    let version = info_version(&spec.host, spec.port);
    Ok(Target {
        kind: spec.kind,
        host: spec.host.clone(),
        port: spec.port,
        mode: Mode::Docker,
        version,
        launch_cmd,
        pid: None,
        child: None,
        container: Some(name),
    })
}

/// Verify an already-running engine at `host:port` is reachable; read version
/// from `INFO server`. Nothing is spawned or torn down.
pub fn attach(kind: EngineKind, host: &str, port: u16) -> Result<Target, String> {
    wait_ready(host, port, Duration::from_secs(5))
        .map_err(|e| format!("{} (attach {host}:{port}): {e}", kind.label()))?;
    Ok(Target {
        kind,
        host: host.to_string(),
        port,
        mode: Mode::Attach,
        version: info_version(host, port),
        launch_cmd: format!("(attached at {host}:{port})"),
        pid: None,
        child: None,
        container: None,
    })
}

/// Apply `maxmemory` + `allkeys-lru` over RESP. Used for infinitydb (no
/// launch flag); redis/dragonfly already took it on the command line.
pub fn set_maxmemory(target: &Target, mb: u64) -> Result<(), String> {
    let bytes = (mb * 1024 * 1024).to_string();
    resp::command(&target.host, target.port, &[b"CONFIG", b"SET", b"maxmemory", bytes.as_bytes()])?;
    resp::command(
        &target.host,
        target.port,
        &[b"CONFIG", b"SET", b"maxmemory-policy", b"allkeys-lru"],
    )?;
    Ok(())
}

pub fn flushall(target: &Target) -> Result<(), String> {
    resp::command(&target.host, target.port, &[b"FLUSHALL"]).map(|_| ())
}

/// `DBSIZE` of db0 — the live key count, for bytes/key attribution.
pub fn dbsize(target: &Target) -> Result<u64, String> {
    let reply = resp::command(&target.host, target.port, &[b"DBSIZE"])?;
    // `:<n>\r\n`
    let text = String::from_utf8_lossy(&reply);
    text.trim()
        .trim_start_matches(':')
        .trim()
        .parse()
        .map_err(|_| format!("DBSIZE reply not an integer: {text:?}"))
}

/// Current RSS in MiB (host: `/proc` VmRSS; docker: `docker stats`; attach: none).
pub fn rss_now_mib(target: &Target) -> Option<f64> {
    match target.mode {
        Mode::Host => proc_status_kib(target.pid?).1.map(kib_to_mib),
        Mode::Docker => docker_mem_mib(target.container.as_deref()?),
        Mode::Attach => None,
    }
}

/// Peak RSS in MiB (host only: `/proc` VmHWM; not available under docker/attach).
pub fn rss_peak_mib(target: &Target) -> Option<f64> {
    match target.mode {
        Mode::Host => proc_status_kib(target.pid?).0.map(kib_to_mib),
        Mode::Docker => docker_mem_mib(target.container.as_deref()?),
        Mode::Attach => None,
    }
}

/// Stop the engine: host → SIGKILL + reap; docker → `rm -f`; attach → nothing.
pub fn teardown(target: Target) {
    match target.mode {
        Mode::Host => {
            if let Some(mut child) = target.child {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
        Mode::Docker => {
            if let Some(name) = target.container {
                let _ = Command::new("docker")
                    .args(["rm", "-f", &name])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
        }
        Mode::Attach => {}
    }
}

// ---- readiness ----------------------------------------------------------

fn wait_ready(host: &str, port: u16, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    while !resp::ping(host, port) {
        if Instant::now() >= deadline {
            return Err(format!("{host}:{port} not ready after {timeout:?} (see logs/)"));
        }
        // Tooling tier (never the data plane): a readiness poll legitimately
        // sleeps. The ban targets cell-resident code; mirrors inf-bench.
        #[allow(clippy::disallowed_methods)]
        thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

// ---- host argv ----------------------------------------------------------

fn host_argv(spec: &Spec) -> Result<(String, Vec<String>), String> {
    match spec.kind {
        EngineKind::Redis => {
            let (p, a) = redis_argv(spec, false);
            Ok(maybe_pin(p, a, spec.pin_start, 1)) // redis is single-threaded
        }
        EngineKind::RedisStack => {
            Err("redis-stack is docker-only (no host binary); pass --docker".into())
        }
        EngineKind::Dragonfly => {
            let (p, a) = dragonfly_argv(spec, false);
            Ok(maybe_pin(p, a, spec.pin_start, spec.threads as usize))
        }
        // infinityd pins its own cells via --pin-start; never taskset-wrapped.
        EngineKind::InfinityDb => infinity_host_argv(spec),
    }
}

fn redis_argv(spec: &Spec, in_docker: bool) -> (String, Vec<String>) {
    let port = if in_docker { 6379 } else { spec.port };
    let mut argv =
        vec!["--port".to_string(), port.to_string(), "--save".to_string(), String::new()];
    match (spec.durability, &spec.data_dir) {
        (Durability::Everysec, Some(dir)) => argv.extend([
            "--appendonly".to_string(),
            "yes".to_string(),
            "--appendfsync".to_string(),
            "everysec".to_string(),
            "--dir".to_string(),
            dir.display().to_string(),
        ]),
        _ => argv.extend(["--appendonly".to_string(), "no".to_string()]),
    }
    if let Some(mb) = spec.maxmemory_mb {
        argv.extend([
            "--maxmemory".to_string(),
            (mb * 1024 * 1024).to_string(),
            "--maxmemory-policy".to_string(),
            "allkeys-lru".to_string(),
        ]);
    }
    ("redis-server".to_string(), argv)
}

fn dragonfly_argv(spec: &Spec, in_docker: bool) -> (String, Vec<String>) {
    let port = if in_docker { 6379 } else { spec.port };
    let mut argv = vec![
        "--port".to_string(),
        port.to_string(),
        "--proactor_threads".to_string(),
        spec.threads.to_string(),
        "--cache_mode".to_string(),
        "--dbfilename".to_string(),
        String::new(),
    ];
    if let Some(mb) = spec.maxmemory_mb {
        argv.extend(["--maxmemory".to_string(), (mb * 1024 * 1024).to_string()]);
    }
    if !in_docker {
        // keep snapshots out of cwd; send logs to stderr so we capture them
        argv.extend(["--logtostderr".to_string(), "--dir".to_string(), "/tmp".to_string()]);
    }
    ("dragonfly".to_string(), argv)
}

/// Wrap a host command in `taskset -c LO-HI` (`width` cores from `pin_start`)
/// when pinning is requested and `taskset` exists; no-op otherwise.
fn maybe_pin(
    program: String,
    argv: Vec<String>,
    pin: Option<usize>,
    width: usize,
) -> (String, Vec<String>) {
    match pin {
        Some(base) if taskset_available() => {
            let hi = base + width.max(1) - 1;
            let range = if hi > base { format!("{base}-{hi}") } else { base.to_string() };
            let mut wrapped = vec!["-c".to_string(), range, program];
            wrapped.extend(argv);
            ("taskset".to_string(), wrapped)
        }
        _ => (program, argv),
    }
}

fn infinity_host_argv(spec: &Spec) -> Result<(String, Vec<String>), String> {
    let bin = resolve_infinityd();
    let mut argv = vec![
        "--port".to_string(),
        spec.port.to_string(),
        "--cells".to_string(),
        spec.threads.to_string(),
    ];
    if let Some(core) = spec.pin_start {
        argv.extend(["--pin-start".to_string(), core.to_string()]);
    }
    if let (Durability::Everysec, Some(dir)) = (spec.durability, &spec.data_dir) {
        argv.extend([
            "--data-dir".to_string(),
            dir.display().to_string(),
            "--conn-default-ns".to_string(),
            DURABLE_NS.to_string(),
        ]);
    }
    Ok((bin.to_string_lossy().into_owned(), argv))
}

/// CPU time (user + system, clock ticks of 1/100 s) a host-launched
/// engine has consumed — `/proc/<pid>/stat` fields 14 + 15; `None` for
/// docker/attach targets. The S40 row discloses server CPU per leg.
pub fn cpu_ticks(target: &Target) -> Option<u64> {
    let pid = target.pid?;
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &stat[stat.rfind(')')? + 2..];
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    Some(utime + stime)
}

/// Sectors written on `/sys/block/<dev>/stat` (field 7): the block
/// device's view of a leg, journal and metadata included.
pub fn device_sectors_written(dev: &str) -> Option<u64> {
    std::fs::read_to_string(format!("/sys/block/{dev}/stat"))
        .ok()
        .and_then(|s| s.split_whitespace().nth(6).and_then(|v| v.parse().ok()))
}

// ---- docker argv --------------------------------------------------------

fn docker_argv(spec: &Spec, images: &Images, name: &str) -> Result<Vec<String>, String> {
    let mut argv = vec![
        "run".to_string(),
        "-d".to_string(),
        "--rm".to_string(),
        "--name".to_string(),
        name.to_string(),
        "-p".to_string(),
        format!("{}:6379", spec.port),
    ];
    match spec.kind {
        EngineKind::Redis => {
            argv.push(images.redis.clone());
            argv.push("redis-server".to_string());
            let (_, inner) = redis_argv(spec, true);
            argv.extend(inner);
        }
        EngineKind::RedisStack => {
            // The stack image's entrypoint loads the modules itself; a
            // `redis-server` command override would skip RedisJSON. Config
            // rides the REDIS_ARGS env var (the CI oracle's launch shape).
            argv.splice(
                1..1,
                ["-e".to_string(), "REDIS_ARGS=--save '' --appendonly no".to_string()],
            );
            argv.push(images.redis_stack.clone());
        }
        EngineKind::Dragonfly => {
            // dragonfly needs unlimited locked memory for its io_uring rings.
            argv.splice(1..1, ["--ulimit".to_string(), "memlock=-1".to_string()]);
            argv.push(images.dragonfly.clone());
            let (_, inner) = dragonfly_argv(spec, true);
            argv.extend(inner);
        }
        EngineKind::InfinityDb => {
            let seccomp = images
                .seccomp
                .canonicalize()
                .map_err(|e| format!("seccomp profile {}: {e}", images.seccomp.display()))?;
            // infinityd's only Linux backend is io_uring; Docker's default
            // seccomp denies io_uring_setup/_enter, so plain `docker run` exits 1.
            argv.splice(
                1..1,
                ["--security-opt".to_string(), format!("seccomp={}", seccomp.display())],
            );
            argv.push(images.infinitydb.clone());
            argv.extend([
                "--port".to_string(),
                "6379".to_string(),
                "--cells".to_string(),
                spec.threads.to_string(),
            ]);
        }
    }
    Ok(argv)
}

// ---- versions -----------------------------------------------------------

/// First line of `<program> --version`, ANSI-stripped (dragonfly colorizes).
fn host_version(program: &str, _argv: &[String]) -> String {
    Command::new(program)
        .arg("--version")
        .output()
        .ok()
        .and_then(|o| {
            let bytes = if o.stdout.is_empty() { o.stderr } else { o.stdout };
            String::from_utf8_lossy(&bytes).lines().next().map(strip_ansi)
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Version from `INFO server` (docker/attach): prefer the engine-specific
/// field, fall back to `redis_version`.
fn info_version(host: &str, port: u16) -> String {
    let Ok(reply) = resp::command(host, port, &[b"INFO", b"server"]) else {
        return "unknown".to_string();
    };
    let text = String::from_utf8_lossy(&reply);
    let field = |key: &str| {
        text.lines().find_map(|l| l.trim().strip_prefix(key).map(|v| v.trim().to_string()))
    };
    field("dragonfly_version:")
        .or_else(|| field("infinitydb_version:"))
        .or_else(|| field("redis_version:"))
        .unwrap_or_else(|| "unknown".to_string())
}

fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // skip until the terminating letter of the CSI sequence
            for e in chars.by_ref() {
                if e.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out.trim().to_string()
}

// ---- RSS ----------------------------------------------------------------

/// `(VmHWM, VmRSS)` in KiB from `/proc/<pid>/status`.
fn proc_status_kib(pid: u32) -> (Option<u64>, Option<u64>) {
    let Ok(text) = std::fs::read_to_string(format!("/proc/{pid}/status")) else {
        // non-Linux fallback: `ps` gives current RSS only
        let cur = Command::new("ps")
            .args(["-o", "rss=", "-p", &pid.to_string()])
            .output()
            .ok()
            .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse::<u64>().ok());
        return (None, cur);
    };
    let mut peak = None;
    let mut cur = None;
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("VmHWM:") {
            peak = first_u64(v);
        } else if let Some(v) = line.strip_prefix("VmRSS:") {
            cur = first_u64(v);
        }
    }
    (peak, cur)
}

/// Current container memory in MiB via `docker stats --no-stream`.
fn docker_mem_mib(name: &str) -> Option<f64> {
    let out = Command::new("docker")
        .args(["stats", "--no-stream", "--format", "{{.MemUsage}}", name])
        .output()
        .ok()?;
    let usage = String::from_utf8_lossy(&out.stdout);
    // "12.34MiB / 31.3GiB" → 12.34 MiB
    let first = usage.split('/').next()?.trim();
    parse_size_mib(first)
}

/// Parse a `docker stats` size token ("12.3MiB", "1.5GiB", "900KiB", "512B").
fn parse_size_mib(tok: &str) -> Option<f64> {
    let tok = tok.trim();
    let split = tok.find(|c: char| c.is_ascii_alphabetic())?;
    let value: f64 = tok[..split].trim().parse().ok()?;
    let unit = tok[split..].trim();
    let mib = match unit {
        "B" => value / (1024.0 * 1024.0),
        "KiB" | "kB" | "KB" => value / 1024.0,
        "MiB" | "MB" => value,
        "GiB" | "GB" => value * 1024.0,
        _ => return None,
    };
    Some(mib)
}

fn first_u64(s: &str) -> Option<u64> {
    s.split_whitespace().next()?.parse().ok()
}

fn kib_to_mib(kib: u64) -> f64 {
    kib as f64 / 1024.0
}

// ---- process plumbing ---------------------------------------------------

fn spawn(program: &str, argv: &[String], log_dir: &Path, label: &str) -> Result<Child, String> {
    let log = log_dir.join(format!("{label}.log"));
    let out = std::fs::File::create(&log).map_err(|e| format!("create {}: {e}", log.display()))?;
    let err = out.try_clone().map_err(|e| format!("clone log fd: {e}"))?;
    Command::new(program)
        .args(argv)
        .stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err))
        .spawn()
        .map_err(|e| format!("spawn `{program}`: {e}"))
}

fn render_cmd(program: &str, argv: &[String]) -> String {
    std::iter::once(program.to_string())
        .chain(argv.iter().map(|t| shell_token(t)))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Single-quote a token if empty or whitespace-bearing so the rendered command
/// round-trips through a shell (e.g. redis's `--save ""`).
fn shell_token(s: &str) -> String {
    if s.is_empty() || s.contains(char::is_whitespace) { format!("'{s}'") } else { s.to_string() }
}

/// Prefer a release build next to the workspace, then debug, then `$PATH`.
fn resolve_infinityd() -> PathBuf {
    for candidate in ["target/release/infinityd", "target/debug/infinityd"] {
        let path = PathBuf::from(candidate);
        if path.exists() {
            return path;
        }
    }
    PathBuf::from("infinityd")
}

fn taskset_available() -> bool {
    Command::new("taskset")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn on_path(program: &str) -> bool {
    Command::new(program)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
