//! `inf-bench gate-run m0` (M0-S18/S19): one command produces the whole M0
//! evidence package — env-check refusal, server orchestration, interleaved
//! A/B replicates, windowed tripwire scrapes, memory attribution, and the
//! per-gate PASS/FAIL report against `docs/milestones/m0-gates.toml`.
//!
//! Tier honesty (L10): gates marked `tier = "linux-reference-box"` get a
//! verdict prefix `DEV-TIER` unless `--reference-box` asserts the run is on
//! the reference box — measured numbers are always reported, but only
//! reference-box runs can bind the milestone verdict.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use crate::cli::Flags;
use crate::envcheck;
use crate::gates;
use crate::load::{LoadSpec, render, run as run_load};
use crate::resp::{connect, parse_info, request};

pub(crate) struct ServerGuard {
    child: Child,
    pub port: u16,
    /// Spawn to first accepted connection — the boot the M4.5-S42 row
    /// times (a probing first boot against a file-backed second one).
    pub boot: Duration,
}

impl ServerGuard {
    /// Non-blocking liveness probe: `Some(status)` once the server process
    /// has exited. A fail-stopped infinityd (M2.5-S01: `cell N failed: …`)
    /// can die *after* its reuseport listener accepted the readiness
    /// probe — callers polling for ready must distinguish that corpse from
    /// a stall instead of burning their timeout on it.
    pub(crate) fn try_exited(&mut self) -> Option<std::process::ExitStatus> {
        self.child.try_wait().ok().flatten()
    }
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl ServerGuard {
    pub(crate) fn rss_bytes(&self) -> u64 {
        std::fs::read_to_string(format!("/proc/{}/status", self.child.id()))
            .ok()
            .and_then(|s| {
                s.lines()
                    .find(|l| l.starts_with("VmRSS:"))
                    .and_then(|l| l.split_whitespace().nth(1).and_then(|kb| kb.parse::<u64>().ok()))
            })
            .map_or(0, |kb| kb * 1024)
    }

    /// Server pid for out-of-band samplers (M2-S12 RSS tracking).
    pub(crate) fn pid(&self) -> u32 {
        self.child.id()
    }
}

/// CPU time (user + system, in clock ticks) the process has consumed —
/// fields 14 and 15 of `/proc/<pid>/stat` (M4.5-S36, ADR-0088 D7: the
/// server-busy tripwire of the pure-write row). Linux `USER_HZ` is 100
/// on every mainstream kernel; [`CLOCK_TICKS_PER_S`] names it.
pub(crate) fn cpu_ticks_of(pid: u32) -> u64 {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|s| {
            // The comm field may contain spaces; split after its ')'.
            let rest = s.rsplit_once(')').map(|(_, r)| r)?;
            let fields: Vec<&str> = rest.split_whitespace().collect();
            // After ')', field 3 (state) is index 0; utime is field 14 →
            // index 11, stime field 15 → index 12.
            let utime: u64 = fields.get(11)?.parse().ok()?;
            let stime: u64 = fields.get(12)?.parse().ok()?;
            Some(utime + stime)
        })
        .unwrap_or(0)
}

/// Linux `USER_HZ` (the `/proc/<pid>/stat` tick).
pub(crate) const CLOCK_TICKS_PER_S: u64 = 100;

/// Peak-VmRSS of `pid` (same parse as [`ServerGuard::rss_bytes`], usable
/// from a sampler thread that must not borrow the guard).
pub(crate) fn rss_bytes_of(pid: u32) -> u64 {
    std::fs::read_to_string(format!("/proc/{pid}/status"))
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmRSS:"))
                .and_then(|l| l.split_whitespace().nth(1).and_then(|kb| kb.parse::<u64>().ok()))
        })
        .map_or(0, |kb| kb * 1024)
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("probe bind")
        .local_addr()
        .expect("addr")
        .port()
}

fn wait_ready(port: u16) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!("server on {port} never came up"));
        }
        std::thread::yield_now();
    }
}

pub(crate) fn spawn_infinityd(
    bin: &str,
    cells: u16,
    extra: &[&str],
) -> Result<ServerGuard, String> {
    let port = free_port();
    let mut cmd = Command::new(bin);
    cmd.args(["--port", &port.to_string(), "--cells", &cells.to_string()]);
    // M4.5-S42 (ADR-0091 D4): a harness spawn never probes silently — arms
    // are explicit (`copy_probe_file`, `--barrier-class`, `--model-absent`);
    // a campaign measuring the shipped first boot forwards `--device-probe
    // auto` itself.
    if !extra.contains(&"--device-probe") {
        cmd.args(["--device-probe", "off"]);
    }
    cmd.args(extra).stdout(Stdio::null());
    // S22 watch item ("server closed connection under load", stderr was
    // nulled in every sighting): INF_GATERUN_STDERR_DIR=<dir> captures
    // each spawned server's stderr as <dir>/infinityd-<port>.stderr.
    match std::env::var("INF_GATERUN_STDERR_DIR") {
        Ok(dir) => {
            let path = format!("{dir}/infinityd-{port}.stderr");
            let file = std::fs::File::create(&path).map_err(|e| format!("{path}: {e}"))?;
            cmd.stderr(Stdio::from(file));
        }
        Err(_) => {
            cmd.stderr(Stdio::null());
        }
    }
    // A boot that probes (ADR-0091 D1: `--device-probe auto` on a fresh
    // directory, ≈ 10 s on the reference device, bounded by
    // `--probe-seconds` × nine rows) needs a longer ready deadline than
    // the instant start every explicit arm gets.
    let probes = extra.windows(2).any(|w| w[0] == "--device-probe" && w[1] == "auto");
    let ready_within = Duration::from_secs(if probes { 180 } else { 10 });
    let started = Instant::now();
    let child = cmd.spawn().map_err(|e| format!("spawn {bin}: {e}"))?;
    let mut guard = ServerGuard { child, port, boot: Duration::ZERO };
    // Poll the child alongside the port: a fail-stopped server (M2.5-S01 —
    // e.g. io_uring_setup ENOMEM prints `cell N failed: …` and exits) is
    // detected in milliseconds and named, instead of burning the full TCP
    // deadline on a corpse and reporting an unclassified "never came up".
    let deadline = started + ready_within;
    loop {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            guard.boot = started.elapsed();
            return Ok(guard);
        }
        if let Some(status) = guard.child.try_wait().map_err(|e| e.to_string())? {
            return Err(format!(
                "server on {port} exited before ready ({status}) — fail-stop \
                 (stderr: infinityd-{port}.stderr)"
            ));
        }
        if Instant::now() >= deadline {
            return Err(format!("server on {port} never came up"));
        }
        std::thread::yield_now();
    }
}

/// Comparator spawn (ADR-0006 shape): thread count matched to our cell
/// count, persistence off, version recorded by the caller.
pub(crate) fn spawn_dragonfly(bin: &str, cells: u16) -> Result<ServerGuard, String> {
    let port = free_port();
    let child = Command::new(bin)
        .args([
            &format!("--port={port}"),
            &format!("--proactor_threads={cells}"),
            "--dbfilename=",
            "--logtostderr",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("spawn {bin}: {e}"))?;
    let started = Instant::now();
    let mut guard = ServerGuard { child, port, boot: Duration::ZERO };
    wait_ready(port)?;
    guard.boot = started.elapsed();
    Ok(guard)
}

pub(crate) fn dragonfly_version(bin: &str) -> String {
    std::process::Command::new(bin)
        .arg("--version")
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map_or_else(
            || format!("{bin} (version unknown)"),
            |v| v.lines().next().unwrap_or("").to_string(),
        )
}

pub(crate) fn spawn_redis(bin: &str) -> Result<ServerGuard, String> {
    let port = free_port();
    let child = Command::new(bin)
        .args(["--port", &port.to_string(), "--save", "", "--appendonly", "no"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("spawn {bin}: {e}"))?;
    let started = Instant::now();
    let mut guard = ServerGuard { child, port, boot: Duration::ZERO };
    wait_ready(port)?;
    guard.boot = started.elapsed();
    Ok(guard)
}

/// Scrape every cell's INFO (REUSEPORT spreads connections; retry until all
/// distinct cells answered or attempts run out).
pub(crate) fn scrape_cells(port: u16, cells: u16) -> Result<Vec<BTreeMap<String, String>>, String> {
    let mut seen: BTreeMap<u16, BTreeMap<String, String>> = BTreeMap::new();
    for _ in 0..512 {
        let mut stream = connect("127.0.0.1", port)?;
        let info = parse_info(&request(&mut stream, &[b"INFO"])?);
        let cell: u16 = info.get("cell").and_then(|v| v.parse().ok()).unwrap_or(0);
        seen.insert(cell, info);
        if seen.len() == usize::from(cells) {
            break;
        }
    }
    if seen.len() != usize::from(cells) {
        return Err(format!("scraped {}/{} cells (REUSEPORT spread)", seen.len(), cells));
    }
    Ok(seen.into_values().collect())
}

pub(crate) fn sum_field(infos: &[BTreeMap<String, String>], field: &str) -> u64 {
    infos.iter().filter_map(|i| i.get(field)).filter_map(|v| v.parse::<u64>().ok()).sum()
}

pub fn max_field(infos: &[BTreeMap<String, String>], field: &str) -> u64 {
    infos
        .iter()
        .filter_map(|i| i.get(field))
        .filter_map(|v| v.parse::<u64>().ok())
        .max()
        .unwrap_or(0)
}

/// Raw counters summed across cells: (submits, sqes, cqes).
fn raw_counters(infos: &[BTreeMap<String, String>]) -> (u64, u64, u64) {
    (sum_field(infos, "raw_submits"), sum_field(infos, "raw_sqes"), sum_field(infos, "raw_cqes"))
}

/// The filesystem type a path resolves to, from `/proc/self/mounts`
/// (longest mount-point prefix of the canonicalized path). `None` off
/// Linux or when the table cannot be read — callers treat unknown as
/// "not a memory filesystem" and say so.
///
/// The reason this is a table lookup and not a `/tmp` prefix test: the
/// S35 reference-box arms (2026-08-21) wrote every FUA frame to tmpfs
/// because the `m2` flow read only `--pressure-data-root` (default =
/// the system temp dir) and the prefix heuristic was a *note*, not a
/// refusal — three "binding" reports at 1.7 M ops/s with 113 µs
/// "barriers" (tmpfs swallows `O_DIRECT` and `RWF_DSYNC`; the arm
/// measured a memcpy).
pub(crate) fn fs_type_of(path: &std::path::Path) -> Option<String> {
    let probe = {
        // The directory may not exist yet (rows create it); walk up to
        // the nearest existing ancestor before canonicalizing.
        let mut p = path.to_path_buf();
        while !p.exists() {
            p = p.parent()?.to_path_buf();
        }
        std::fs::canonicalize(p).ok()?
    };
    let table = std::fs::read_to_string("/proc/self/mounts").ok()?;
    let mut best: Option<(usize, String)> = None;
    for line in table.lines() {
        let mut cols = line.split_whitespace();
        let (Some(_src), Some(target), Some(fstype)) = (cols.next(), cols.next(), cols.next())
        else {
            continue;
        };
        // /proc/self/mounts escapes spaces as `\040`; mount points with
        // spaces are not a shape this harness runs on, so unescape the
        // common case only.
        let target = target.replace("\\040", " ");
        let target_path = std::path::Path::new(&target);
        if probe.starts_with(target_path)
            && best.as_ref().is_none_or(|(len, _)| target.len() > *len)
        {
            best = Some((target.len(), fstype.to_string()));
        }
    }
    best.map(|(_, t)| t)
}

/// A memory-backed filesystem: a durable row on it measures the page
/// cache (fsync ~77 µs) and a FUA-class row measures a memcpy.
pub(crate) fn is_memory_fs(fstype: &str) -> bool {
    matches!(fstype, "tmpfs" | "ramfs" | "devtmpfs" | "hugetlbfs")
}

/// The device-row admission rule, checked **before any row runs** (the
/// M2-S24 lesson: a carrier validated after the rows is a report that
/// cannot be un-written). Returns the fstype for the report header.
///
/// - binding (`--reference-box`) ⇒ refuse on a memory filesystem: a
///   binding durable row must hit the device or it binds nothing;
/// - `--barrier-class fua` ⇒ refuse on a memory filesystem in any tier:
///   tmpfs accepts `O_DIRECT|RWF_DSYNC` and does nothing with them, so
///   the arm's "barrier" is a memcpy and the A/B compares two memcpys;
/// - otherwise a memory filesystem is disclosed as a note by the caller.
pub(crate) fn admit_device_root(
    flags: &Flags,
    root: &std::path::Path,
    reference_box: bool,
) -> Result<String, String> {
    let fstype = fs_type_of(root).unwrap_or_else(|| "unknown".to_string());
    if is_memory_fs(&fstype) {
        if reference_box {
            return Err(format!(
                "data root {} is {fstype}: a binding (--reference-box) durable row on a memory \
                 filesystem measures the page cache, not the device — point --data-root at \
                 the filesystem under test",
                root.display()
            ));
        }
        if flags.get("barrier-class").is_some_and(|c| c.eq_ignore_ascii_case("fua")) {
            return Err(format!(
                "data root {} is {fstype}: --barrier-class fua on a memory filesystem is a \
                 memcpy (O_DIRECT|RWF_DSYNC are accepted and ignored) — the arm would measure \
                 nothing; point --data-root at the filesystem under test",
                root.display()
            ));
        }
    }
    Ok(fstype)
}

pub(crate) fn median(values: &mut [f64]) -> f64 {
    values.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
    values[values.len() / 2]
}

/// One workload row's write-amplification obligation (M4-S16). Opened by
/// the row, filled after it; a row that reaches
/// [`finish_report`] unfilled is an invalid row and fails the run.
struct RowWriteAmp {
    name: String,
    disposition: Option<String>,
}

pub(crate) struct Measurements {
    pub(crate) values: BTreeMap<&'static str, f64>,
    pub(crate) notes: Vec<String>,
    pub(crate) raw: String,
    rows: Vec<RowWriteAmp>,
    sidecars: Vec<(String, String)>,
}

impl Measurements {
    pub(crate) fn new() -> Measurements {
        Measurements {
            values: BTreeMap::new(),
            notes: Vec::new(),
            raw: String::new(),
            rows: Vec::new(),
            sidecars: Vec::new(),
        }
    }

    /// Registers a machine-readable file to write beside `report.md` in
    /// this run's artifact directory. Campaign legs that must be compared
    /// across processes (the ADR-0071 D3 hot-set reference carrier) hand
    /// their numbers forward this way rather than through a human parsing
    /// a report table back into a flag.
    pub(crate) fn sidecar(&mut self, name: &str, body: String) {
        self.sidecars.push((name.to_string(), body));
    }

    /// Declares a workload row. From M4-S16 on, every declared row owes a
    /// write-amplification disposition ([`row_write_amp`](Self::row_write_amp))
    /// — declaring is what makes the debt visible, so a row added later
    /// cannot quietly ship without the number.
    pub(crate) fn row_open(&mut self, name: &str) {
        self.rows.push(RowWriteAmp { name: name.to_string(), disposition: None });
    }

    /// Records the open row's write-amplification disposition.
    ///
    /// # Panics
    /// Panics when no row is open: the call order is the caller's own
    /// program text, so getting it wrong is a programmer error, not an
    /// operating condition.
    pub(crate) fn row_write_amp(&mut self, disposition: &str) {
        let row = self.rows.last_mut().expect("row_write_amp without an open row");
        row.disposition = Some(disposition.to_string());
    }

    pub(crate) fn set(&mut self, key: &'static str, value: f64) {
        self.values.insert(key, value);
    }

    pub(crate) fn note(&mut self, text: impl Into<String>) {
        self.notes.push(text.into());
    }

    pub(crate) fn raw_section(&mut self, title: &str, body: &str) {
        self.raw.push_str(&format!("\n## {title}\n\n```\n{body}```\n"));
    }
}

/// Loads a milestone gates file via the usual relative-path candidates.
///
/// Every candidate stays **inside this checkout**: `docs/milestones/` is
/// the single authority for gate values, and the `../` forms only walk up
/// from a crate/bin subdirectory back to the workspace root. A candidate
/// that escaped the repo (`../docs/…` used to lead the list) resolved to a
/// second, silently drifting copy of the same file on the box where this
/// repo is nested inside the planning repo, and to nothing at all on CI.
pub(crate) fn load_gates(flags: &Flags, milestone: &str) -> Result<Vec<gates::Gate>, String> {
    let default = format!("docs/milestones/{milestone}-gates.toml");
    let gates_path = flags.str_or("gates", &default);
    gates::load(&gates_path)
        .or_else(|_| gates::load(&format!("../docs/milestones/{milestone}-gates.toml")))
        .or_else(|_| gates::load(&format!("../../docs/milestones/{milestone}-gates.toml")))
}

/// The env-check refusal shared by every gate campaign (M0-S18 AC):
/// `--unsafe-env` records the violation and continues, explicitly
/// non-citation-grade. Returns whether the env passed.
pub(crate) fn env_gate(flags: &Flags) -> Result<bool, String> {
    let mut env_args: Vec<String> = Vec::new();
    if flags.bool("allow-dirty") {
        env_args.push("--allow-dirty".into());
    }
    let env_verdict = envcheck::cmd_env_check(&env_args);
    let env_ok = env_verdict.is_ok();
    if let Err(e) = env_verdict {
        if !flags.bool("unsafe-env") {
            return Err(format!(
                "{e}\ngate-run refuses to run (pass --unsafe-env to record a non-citable dev run)"
            ));
        }
        eprintln!("gate-run: CONTINUING WITH FAILED ENV-CHECK — results are not citation-grade");
    }
    Ok(env_ok)
}

/// Per-gate verdicts + the report file (shared epilogue). Errs when any
/// binding gate failed, or when a declared workload row never reported its
/// write amplification (M4-S16: a row missing WA is an invalid row — the
/// generator refuses it rather than publishing a report whose silence
/// reads like a good number).
pub(crate) fn finish_report(
    milestone: &str,
    gates_list: &[gates::Gate],
    m: &Measurements,
    env_ok: bool,
    reference_box: bool,
    artifacts_root: &str,
    header_facts: &str,
) -> Result<(), String> {
    // The row obligation is checked *before* anything is written: an
    // invalid run must not leave a report file behind to be cited later.
    let unreported: Vec<&str> =
        m.rows.iter().filter(|r| r.disposition.is_none()).map(|r| r.name.as_str()).collect();
    if !unreported.is_empty() {
        return Err(format!(
            "row(s) [{}] finished without a write-amplification disposition — an M4 report row \
             without WA is an invalid row (M4-S16); measure it or name why there is none",
            unreported.join(", ")
        ));
    }

    let stamp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("clock after epoch")
        .as_secs();
    let dir = format!("{artifacts_root}/{stamp}-gate-run");
    std::fs::create_dir_all(&dir).map_err(|e| format!("{dir}: {e}"))?;

    let mut report = String::new();
    report.push_str(&format!(
        "# {} gate-run report\n\ndate: {stamp} (unix) · {header_facts}\nenv-check: {}\ntier: {}\n\nnotes:\n",
        milestone.to_uppercase(),
        if env_ok { "OK" } else { "FAILED (overridden — NOT citation-grade)" },
        if reference_box { "reference-box (binding)" } else { "dev (non-binding)" },
    ));
    for note in &m.notes {
        report.push_str(&format!("- {note}\n"));
    }
    report.push_str("\n| gate | threshold | measured | verdict |\n|---|---|---|---|\n");
    println!("\n== gate verdicts ==");
    let mut binding_failures = 0;
    for gate in gates_list {
        let measured = m.values.get(gate.source.as_str()).copied();
        let (measured_text, verdict) = match measured {
            None => ("—".to_string(), "PENDING (tooling)".to_string()),
            Some(value) => {
                let pass = gate.passes(value);
                let tag = if pass { "PASS" } else { "FAIL" };
                let verdict = if gate.informational {
                    format!("{tag} (informational)")
                } else if gate.tier == "linux-reference-box" && !reference_box {
                    format!("{tag} (DEV-TIER, non-binding)")
                } else {
                    if !pass {
                        binding_failures += 1;
                    }
                    tag.to_string()
                };
                (format!("{value:.2}"), verdict)
            }
        };
        println!("  {:<38} {}", gate.name, verdict);
        report.push_str(&format!(
            "| {} | {} {} {} | {} | {} |\n",
            gate.name, gate.comparator, gate.threshold, gate.unit, measured_text, verdict
        ));
    }
    if !m.rows.is_empty() {
        report.push_str(
            "\n## write amplification by row\n\n\
             Per namespace, worst first — never a node-wide blend (M4-S16).\n\n\
             | row | write amplification |\n|---|---|\n",
        );
        for row in &m.rows {
            let disposition = row.disposition.as_deref().unwrap_or("MISSING");
            report.push_str(&format!("| {} | {} |\n", row.name, disposition));
        }
    }
    report.push_str(&m.raw);

    let report_path = format!("{dir}/report.md");
    let mut file =
        std::fs::File::create(&report_path).map_err(|e| format!("{report_path}: {e}"))?;
    file.write_all(report.as_bytes()).map_err(|e| format!("{report_path}: {e}"))?;
    println!("\ngate-run: report written to {report_path}");
    for (name, body) in &m.sidecars {
        let path = format!("{dir}/{name}");
        std::fs::write(&path, body).map_err(|e| format!("{path}: {e}"))?;
        println!("gate-run: sidecar written to {path}");
    }
    if binding_failures > 0 {
        return Err(format!("{binding_failures} binding gate(s) FAILED"));
    }
    Ok(())
}

pub(crate) const GATE_RUN_FLAGS: (&[&str], &[&str]) = (
    &[
        "allow-dirty",
        "unsafe-env",
        "reference-box",
        "skip-fill",
        "with-zipfian",
        "skip-pressure",
        "grouping-canary",
        "early-fabric-flush",
        "remote-first-execute",
        "fabric-apply-prefetch",
        "no-fabric-apply-prefetch",
        "parse-batch-prefetch",
        "no-parse-batch-prefetch",
        "deasync-dispatch",
        "no-deasync-dispatch",
        "skip-comparator",
        "only-always",
        // M4.5 S27 row (the A/B arms skip the S29 legs).
        "only-s27",
        // M4.5 S31 A/B (the mirror: the S29 row without the S27 legs).
        "only-s29",
        // M4.5 S35 frame-pipeline arms (the row alone, one report per arm).
        "only-s35",
        // M4.5 S36 device-budget row (the row alone, one report per arm),
        // and its unbudgeted baseline arm (no probe file copied in).
        "only-s36",
        "only-s39b",
        "only-s39d",
        "only-s40",
        "only-s37",
        // M4.5-S42 (ADR-0091 D6): the stock first-boot row.
        "only-s42",
        // M4.5-S34 (2026-08-25): the m2 everysec penalty row alone (the
        // barrier-class A/B leg).
        "only-everysec",
        // M4.5-S34 (2026-08-26): the S35 read leg on a filled, quiesced
        // namespace (both arms read the same keys, no misses).
        "read-leg-fill",
        "model-absent",
    ],
    &[
        "allow-dirty",
        "unsafe-env",
        "reference-box",
        "skip-fill",
        "skip-pressure",
        "grouping-canary",
        "gates",
        "artifacts-root",
        "replicates",
        "duration",
        "cells",
        "infinityd-bin",
        "redis-bin",
        "dragonfly-bin",
        "early-fabric-flush",
        "remote-first-execute",
        "fabric-apply-prefetch",
        "no-fabric-apply-prefetch",
        "parse-batch-prefetch",
        "no-parse-batch-prefetch",
        "deasync-dispatch",
        "no-deasync-dispatch",
        "skip-comparator",
        "fill-keys",
        // M1 rows (ignored by the m0 flow):
        "storm-keys",
        "flushall-keys",
        "maxmemory-mb",
        "subs",
        "sub-channels",
        // M1 hit-rate-parity row (--with-zipfian; see zipfian.rs):
        "with-zipfian",
        "zipfian-keyspace",
        "zipfian-ops",
        "zipfian-maxmemory-mb",
        // M2 zero-cost A/B (ignored by the m0/m1 flows): the pre-M2
        // infinityd build the ab:* rows compare against, and optional cell
        // pinning (both legs share the set — they never run concurrently).
        "baseline-bin",
        "pin-start",
        // M2-S12 pressure row (checkpoint under full durable load).
        "pressure-replicates",
        // Data-dir root for the pressure legs. Default = the system temp
        // dir, which is often tmpfs — point it at a real filesystem for
        // rows that must exercise the device (disclosed in the report).
        "pressure-data-root",
        // M4.5 S29 row: server data-dir root. Must not be tmpfs — the
        // row's fsyncs must hit a real device (same rule as above).
        "data-root",
        // M2-S22 campaign: durable attribution fill size, external gate
        // artifacts (values measured by campaign tooling; provenance is
        // mandatory via --campaign-note), and the provenance note itself.
        "attribution-keys",
        // Retired by ADR-0087 D5 (accepted for one campaign, logged as a
        // no-op, never forwarded — the S27 recipe must not break mid-run).
        "sync-pipeline",
        // M4.5-S35 (ADR-0087 D5/D8) A/B arms, forwarded to every durable
        // spawn and disclosed in the report notes: the frame pipeline
        // depth, the barrier class, and the per-buffer staging size the
        // L5-neutral pairing needs (`--frames-in-flight 3
        // --log-staging-mib 2`).
        "frames-in-flight",
        // M4.5-S39b (ADR-0090 D6): the recycling row's knobs — segment
        // size, the checkpoint floor that follows it, the pool bound,
        // and the block device whose sectors-written the row samples.
        "segment-bytes",
        "ckpt-interval-bytes",
        "segment-recycle-slots",
        // ADR-0090 D9 (A9): the arm's pool wait (`off|quarter|eighth`,
        // forwarded to the arm spawn only) and the row's baseline
        // (`recycle-off` = D6's `--no-segment-recycle`, `wait-off` = the
        // same pool bound with the wait off — the causal D9 A/B).
        "recycle-wait",
        "s39b-baseline",
        "device-stat",
        "barrier-class",
        "staging-mib",
        "only-always",
        "only-s27",
        "only-s29",
        "only-s35",
        "only-s36",
        "only-s39b",
        // M4.5-S39d (ADR-0090 A10): the fixed-work recovery row and its
        // two record counts (warm = before the boundary checkpoint,
        // tail = after it).
        "only-s39d",
        "s39d-warm-records",
        "s39d-tail-records",
        // M4.5-S40: the stall-attribution row's key space (the memtier
        // shape's `--keyspace`).
        "only-s40",
        "s40-keys",
        // M4.5-S37 step 1: the ceiling row's key space (beyond-RAM by
        // construction against the row's 128 MB budget).
        "only-s37",
        "s37-keys",
        // M4.5-S37 step 2 discriminator: `COLD-READ-QD` arms, baseline first.
        "s37-cold-read-qd",
        // M4.5-S42 / M4.5-S34.
        "only-s42",
        "only-everysec",
        // M4.5-S39d baseline: `recycling-off` (ADR-0090 A10) or
        // `flush-class` (the S34 C38b replay clause).
        "s39d-baseline",
        "read-leg-fill",
        "model-absent",
        // M4.5-S35 row: seconds idled before every durable leg (the S34
        // drive-state rule; default 40, 0 for harness smoke).
        "leg-idle-s",
        // M4.5-S36 row (ADR-0088 D7): the offered rate of the S27 D5
        // `max` leg (comparator-matched, disclosed), the tmpfs control
        // root (the 0.85× denominator — the one memory-fs root the
        // admission rule exempts, labelled as a control), and the
        // server's device-model override forwarded to every spawn.
        "offered-ops",
        "tmpfs-control-root",
        "device-write-mbps",
        "seal-pace",
        // M4.5-S39a: the frame-fill arms, forwarded to every durable spawn
        // through `pipeline_args` (window in µs — 0 = the baseline arm —
        // and target in KiB).
        "fill-window-us",
        "fill-target-kib",
        // M4.5-S43 (ADR-0092 D4): the FLUSH-class group-hold arm (window
        // in µs — 0 = the baseline arm), forwarded through `pipeline_args`.
        "flush-group-window-us",
        // M4.5-S42 (ADR-0091 D4): every spawn names its tier — `off`
        // unless a campaign measures the shipped first boot (`auto`).
        "device-probe",
        "recovery-gbps-per-cell",
        "recovery-boot-s",
        // ADR-0070 D7 (2026-08-16): Phase::Start overhead, split out of the
        // replay row after F33 showed the two were being conflated.
        "recovery-setup-s",
        "dst-violations",
        "crash-failures",
        "m0m1-pct",
        "campaign-note",
        // M4-S16: an externally measured write-amplification figure in
        // milli-units (the store-side harness measures it; this flag binds
        // the gate row and demands --campaign-note for provenance).
        "write-amp-milli",
        // M4-S24 campaign carriers for §7 rows measured by external
        // tooling (mixed-audit, soak-m4 verdict, M3 gate set, storm
        // artifacts; --campaign-note mandatory for every one).
        "mixed-attribution-pct",
        "cache-isolation-pct",
        "m3-regression-pct",
        "foreground-p999-ms",
        "endurance-rss-slope-pct",
        "endurance-crashes",
        "hot-set-p50-pct",
        "hot-set-p99-pct",
        "hot-set-p999-pct",
        "cold-read-p99-ms",
    ],
);

#[allow(clippy::too_many_lines)] // orchestration script: linear, not branchy
pub fn cmd_gate_run(args: &[String]) -> Result<(), String> {
    let Some((milestone, rest)) = args.split_first() else {
        return Err("usage: gate-run m0|m1 [flags]".into());
    };
    let flags = Flags::parse(rest, GATE_RUN_FLAGS.0, GATE_RUN_FLAGS.1)?;
    match milestone.as_str() {
        "m0" => cmd_gate_run_m0(&flags),
        "m1" => crate::m1rows::cmd_gate_run_m1(&flags),
        "m2" => crate::m2rows::cmd_gate_run_m2(&flags),
        "m4" => crate::m4rows::cmd_gate_run_m4(&flags),
        "m4.5" => crate::m45rows::cmd_gate_run_m45(&flags),
        other => Err(format!("unknown milestone {other} (have: m0, m1, m2, m4, m4.5)")),
    }
}

#[allow(clippy::too_many_lines)] // orchestration script: linear, not branchy
fn cmd_gate_run_m0(flags: &Flags) -> Result<(), String> {
    let gates_list = load_gates(flags, "m0")?;
    let artifacts_root = flags.str_or("artifacts-root", ".artifacts/m0");
    let replicates: usize = flags
        .get("replicates")
        .map_or(Ok(3), str::parse)
        .map_err(|e| format!("--replicates: {e}"))?;
    let duration: u64 =
        flags.get("duration").map_or(Ok(10), str::parse).map_err(|e| format!("--duration: {e}"))?;
    let cells: u16 =
        flags.get("cells").map_or(Ok(4), str::parse).map_err(|e| format!("--cells: {e}"))?;
    let fill_keys: u64 = flags
        .get("fill-keys")
        .map_or(Ok(10_000_000), str::parse)
        .map_err(|e| format!("--fill-keys: {e}"))?;
    let infinityd = flags.str_or("infinityd-bin", "target/release/infinityd");
    let redis_bin = flags.str_or("redis-bin", "redis-server");
    let reference_box = flags.bool("reference-box");

    // 1. env-check refusal (M0-S18 AC).
    let env_ok = env_gate(flags)?;

    let mut m = Measurements::new();
    if !env_ok {
        m.note("env-check FAILED and was overridden (--unsafe-env): not citation-grade");
    }
    if !reference_box {
        m.note("dev-tier run: reference-box gates report measured values, non-binding verdicts");
    }

    // 2. Pipelined replicates on infinityd (natural routing) + windowed
    //    tripwires from raw counter deltas.
    println!("\n== pipelined replicates (conns=64 P=16, {duration}s x {replicates}) ==");
    // M2.5-S21 lever passthrough: the A/B legs of the penalty campaign
    // spawn every infinityd with the lever flag when requested.
    let mut server_extra: Vec<&str> = Vec::new();
    if flags.bool("early-fabric-flush") {
        server_extra.push("--early-fabric-flush");
    }
    if flags.bool("remote-first-execute") {
        server_extra.push("--remote-first-execute");
    }
    if flags.bool("fabric-apply-prefetch") {
        server_extra.push("--fabric-apply-prefetch");
    }
    if flags.bool("parse-batch-prefetch") {
        server_extra.push("--parse-batch-prefetch");
    }
    if flags.bool("no-parse-batch-prefetch") {
        server_extra.push("--no-parse-batch-prefetch");
    }
    if flags.bool("no-fabric-apply-prefetch") {
        server_extra.push("--no-fabric-apply-prefetch");
    }
    if flags.bool("deasync-dispatch") {
        server_extra.push("--deasync-dispatch");
    }
    if flags.bool("no-deasync-dispatch") {
        server_extra.push("--no-deasync-dispatch");
    }
    let natural = spawn_infinityd(&infinityd, cells, &server_extra)?;
    let mut pipelined_ops: Vec<f64> = Vec::new();
    let mut pipelined_p999: Vec<f64> = Vec::new();
    let mut windowed_sqes_per_submit: Vec<f64> = Vec::new();
    for rep in 0..replicates {
        let before = raw_counters(&scrape_cells(natural.port, cells)?);
        let spec = LoadSpec {
            port: natural.port,
            duration: Duration::from_secs(duration),
            ..Default::default()
        };
        let report = run_load(&spec)?;
        let after = raw_counters(&scrape_cells(natural.port, cells)?);
        let sqes = (after.1 - before.1) as f64 / (after.0 - before.0).max(1) as f64;
        println!(
            "  rep {rep}: {:.0} ops/s, p999 {} us, windowed sqes/submit {sqes:.1}",
            report.ops_per_sec, report.p999_us
        );
        m.raw_section(&format!("pipelined rep {rep}"), &render(&report));
        pipelined_ops.push(report.ops_per_sec);
        pipelined_p999.push(report.p999_us as f64);
        windowed_sqes_per_submit.push(sqes);
    }
    m.set("loadgen:ops_per_sec", median(&mut pipelined_ops));
    m.set("loadgen:p999_us", median(&mut pipelined_p999));
    m.set("tripwire:sqes_per_submit", median(&mut windowed_sqes_per_submit));
    let infos = scrape_cells(natural.port, cells)?;
    m.set("tripwire:loop_iter_p999_us", max_field(&infos, "loop_iter_p999_us") as f64);
    m.set(
        "external:fabric_token_histogram",
        max_field(&infos, "fabric_rtt_p50_ns") as f64 / 1000.0,
    );
    m.note("fabric RTT measured at loop granularity (shared.now updates once per step)");

    // 3. Cross-cell penalty: same workload, --route-local-only A/B.
    println!("\n== cross-cell penalty (natural vs --route-local-only) ==");
    let mut local_extra = server_extra.clone();
    local_extra.push("--route-local-only");
    let local_only = spawn_infinityd(&infinityd, cells, &local_extra)?;
    let mut natural_ops: Vec<f64> = Vec::new();
    let mut local_ops: Vec<f64> = Vec::new();
    for _ in 0..replicates {
        for (target, bucket) in [(&natural, &mut natural_ops), (&local_only, &mut local_ops)] {
            let spec = LoadSpec {
                port: target.port,
                duration: Duration::from_secs(duration.min(5)),
                ..Default::default()
            };
            bucket.push(run_load(&spec)?.ops_per_sec);
        }
    }
    let nat = median(&mut natural_ops);
    let loc = median(&mut local_ops);
    let penalty = ((loc - nat) / loc * 100.0).max(0.0);
    println!("  natural {nat:.0} ops/s vs all-local {loc:.0} ops/s => penalty {penalty:.1}%");
    m.set("external:slotmap_ab", penalty);
    drop(local_only);

    // 3b. Comparator anchor (ADR-0006, in-run per §19: a cross-cell claim
    // without Dragonfly in the same run does not exist). Interleaved ABBA
    // against `dragonfly --proactor_threads=<cells>`, same workload.
    if flags.bool("skip-comparator") {
        m.note("comparator leg skipped (--skip-comparator): comparator_ab PENDING");
    } else {
        println!("\n== comparator anchor (dragonfly --proactor_threads={cells}) ==");
        let dragonfly_bin = flags.str_or("dragonfly-bin", "dragonfly");
        match spawn_dragonfly(&dragonfly_bin, cells) {
            Err(e) => m.note(format!("comparator leg skipped: {e} — comparator_ab PENDING")),
            Ok(dragonfly) => {
                let mut ours: Vec<f64> = Vec::new();
                let mut theirs: Vec<f64> = Vec::new();
                for rep in 0..replicates {
                    let ours_first = rep % 2 == 0;
                    for leg in 0..2 {
                        let (port, bucket) = if (leg == 0) == ours_first {
                            (natural.port, &mut ours)
                        } else {
                            (dragonfly.port, &mut theirs)
                        };
                        let spec = LoadSpec {
                            port,
                            duration: Duration::from_secs(duration.min(5)),
                            ..Default::default()
                        };
                        bucket.push(run_load(&spec)?.ops_per_sec);
                    }
                }
                let a = median(&mut ours);
                let b = median(&mut theirs);
                let ratio = if b > 0.0 { a / b } else { 0.0 };
                println!(
                    "  infinityd natural {a:.0} ops/s vs dragonfly {b:.0} ops/s => {ratio:.2}x"
                );
                m.set("external:comparator_ab", ratio);
                m.note(format!(
                    "comparator: {} · interleaved ABBA x {replicates} · persistence off both \
                     (ADR-0006 shape)",
                    dragonfly_version(&dragonfly_bin)
                ));
            }
        }
    }

    // 4. Unpipelined 512-conn A/B vs Redis (interleaved replicates).
    println!("\n== unpipelined 512-conn A/B vs redis ==");
    match spawn_redis(&redis_bin) {
        Err(e) => m.note(format!("A/B skipped: {e} — unpipelined ratio PENDING")),
        Ok(redis) => {
            let mut ours: Vec<f64> = Vec::new();
            let mut theirs: Vec<f64> = Vec::new();
            for _ in 0..replicates {
                for (port, bucket) in [(natural.port, &mut ours), (redis.port, &mut theirs)] {
                    let spec = LoadSpec {
                        port,
                        conns: 512,
                        pipeline: 1,
                        duration: Duration::from_secs(duration.min(5)),
                        ..Default::default()
                    };
                    bucket.push(run_load(&spec)?.ops_per_sec);
                }
            }
            let a = median(&mut ours);
            let b = median(&mut theirs);
            println!("  infinityd {a:.0} ops/s vs redis {b:.0} ops/s => {:.2}x", a / b);
            m.set("ab:ops_per_sec_ratio", a / b);
        }
    }
    drop(natural);

    // 5. RSS @ fill_keys x (16 B, 64 B) vs Redis + attribution divergence.
    if flags.bool("skip-fill") {
        m.note("fill/RSS phase skipped (--skip-fill)");
    } else {
        println!("\n== RSS fill: {fill_keys} keys x (16 B, 64 B), both engines ==");
        let ours = spawn_infinityd(&infinityd, cells, &[])?;
        let fill = LoadSpec {
            port: ours.port,
            conns: 32,
            pipeline: 64,
            fill: Some(fill_keys),
            duration: Duration::from_secs(3600),
            ..Default::default()
        };
        let fill_report = run_load(&fill)?;
        println!("  infinityd fill: {:.0} sets/s", fill_report.ops_per_sec);
        let our_rss = ours.rss_bytes();
        let infos = scrape_cells(ours.port, cells)?;
        let domains = sum_field(&infos, "records_resident_bytes")
            + sum_field(&infos, "index_bytes")
            + sum_field(&infos, "doc_resident_bytes")
            + sum_field(&infos, "doc_scratch_bytes")
            + sum_field(&infos, "doc_path_cache_bytes")
            + sum_field(&infos, "wire_buffers_bytes")
            + sum_field(&infos, "conn_state_bytes");
        let doc_domains = sum_field(&infos, "doc_resident_bytes")
            + sum_field(&infos, "doc_scratch_bytes")
            + sum_field(&infos, "doc_path_cache_bytes");
        let divergence = ((our_rss as f64 - domains as f64) / our_rss as f64 * 100.0).abs();
        m.set("attribution_divergence_pct", divergence);
        m.note(format!(
            "attribution: domains {domains} B (document {doc_domains} B) vs VmRSS {our_rss} B \
             ({divergence:.1}% divergence)"
        ));
        drop(ours);

        match spawn_redis(&redis_bin) {
            Err(e) => m.note(format!("redis RSS leg skipped: {e}")),
            Ok(redis) => {
                let fill = LoadSpec { port: redis.port, ..fill.clone() };
                let report = run_load(&fill)?;
                println!("  redis fill: {:.0} sets/s", report.ops_per_sec);
                let redis_rss = redis.rss_bytes();
                let ratio = our_rss as f64 / redis_rss as f64;
                println!("  RSS: infinityd {our_rss} B vs redis {redis_rss} B => {ratio:.3}x");
                m.set("external:rss_attribution", ratio);
            }
        }
    }

    // 6. Per-gate verdicts + report.
    finish_report(
        "m0",
        &gates_list,
        &m,
        env_ok,
        reference_box,
        &artifacts_root,
        &format!("cells: {cells} · replicates: {replicates} · duration: {duration}s"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one_gate() -> Vec<gates::Gate> {
        vec![gates::Gate {
            id: "probe".into(),
            name: "probe".into(),
            threshold: 1.0,
            comparator: "<=".into(),
            unit: "x".into(),
            tier: "any".into(),
            source: "probe:value".into(),
            informational: false,
        }]
    }

    /// M4-S16: a declared row that never reported its write amplification
    /// makes the whole report invalid — and the refusal happens *before*
    /// any file is written, so no unreported report can be cited later.
    #[test]
    fn a_row_without_write_amplification_refuses_the_report() {
        let dir = std::env::temp_dir().join(format!("inf-bench-wa-{}", std::process::id()));
        let root = dir.to_str().expect("utf-8 temp path");
        let mut m = Measurements::new();
        m.set("probe:value", 0.5);
        m.row_open("silent row");
        let err = finish_report("m4", &one_gate(), &m, true, false, root, "unit test")
            .expect_err("an unreported row is refused");
        assert!(err.contains("silent row"), "{err}");
        assert!(err.contains("invalid row"), "{err}");
        assert!(!dir.exists(), "the refusal must not leave a report directory behind");

        // The same run with the disposition filled writes its report.
        m.row_write_amp("n/a (no tiered namespace)");
        finish_report("m4", &one_gate(), &m, true, false, root, "unit test").expect("reported");
        assert!(dir.exists(), "the reported run wrote its artifact");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// M4.5-S35 (2026-08-21): a binding run or a FUA arm on a memory
    /// filesystem refuses before any row runs; a non-memory root admits.
    /// (Linux-only facts: `/proc/self/mounts` and the fstype of `/`.)
    #[test]
    #[cfg(target_os = "linux")]
    fn device_root_admission_refuses_memory_filesystems_for_binding_and_fua_runs() {
        let root_fs = fs_type_of(std::path::Path::new("/")).expect("the root mount is listed");
        assert!(!root_fs.is_empty());
        // A path that does not exist yet resolves through its ancestors.
        assert_eq!(fs_type_of(std::path::Path::new("/definitely/not/here")), Some(root_fs));
        assert!(is_memory_fs("tmpfs") && is_memory_fs("ramfs") && !is_memory_fs("ext4"));

        let (bools, values) = GATE_RUN_FLAGS;
        let fua =
            Flags::parse(&["--barrier-class".into(), "fua".into()], bools, values).expect("flags");
        let plain = Flags::parse(&[], bools, values).expect("flags");
        let Some(tmpfs) = ["/dev/shm", "/run", "/tmp"]
            .into_iter()
            .map(std::path::Path::new)
            .find(|p| fs_type_of(p).is_some_and(|t| is_memory_fs(&t)))
        else {
            eprintln!("no memory filesystem mounted — refusal legs skipped");
            return;
        };
        let err = admit_device_root(&plain, tmpfs, true).expect_err("binding on tmpfs refuses");
        assert!(err.contains("binding"), "{err}");
        let err = admit_device_root(&fua, tmpfs, false).expect_err("fua on tmpfs refuses");
        assert!(err.contains("memcpy"), "{err}");
        // Dev-tier non-FUA on tmpfs admits (disclosed by the caller).
        assert!(is_memory_fs(&admit_device_root(&plain, tmpfs, false).expect("admitted")));
        // A non-memory root admits in every mode.
        let home = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        if !fs_type_of(home).is_some_and(|t| is_memory_fs(&t)) {
            admit_device_root(&fua, home, true).expect("device root admits a binding fua run");
        }
    }
}
