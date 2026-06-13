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
    cmd.args(["--port", &port.to_string(), "--cells", &cells.to_string()])
        .args(extra)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let child = cmd.spawn().map_err(|e| format!("spawn {bin}: {e}"))?;
    let guard = ServerGuard { child, port };
    wait_ready(port)?;
    Ok(guard)
}

pub(crate) fn spawn_redis(bin: &str) -> Result<ServerGuard, String> {
    let port = free_port();
    let child = Command::new(bin)
        .args(["--port", &port.to_string(), "--save", "", "--appendonly", "no"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("spawn {bin}: {e}"))?;
    let guard = ServerGuard { child, port };
    wait_ready(port)?;
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

fn max_field(infos: &[BTreeMap<String, String>], field: &str) -> u64 {
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

pub(crate) fn median(values: &mut [f64]) -> f64 {
    values.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
    values[values.len() / 2]
}

pub(crate) struct Measurements {
    pub(crate) values: BTreeMap<&'static str, f64>,
    pub(crate) notes: Vec<String>,
    pub(crate) raw: String,
}

impl Measurements {
    pub(crate) fn new() -> Measurements {
        Measurements { values: BTreeMap::new(), notes: Vec::new(), raw: String::new() }
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
pub(crate) fn load_gates(flags: &Flags, milestone: &str) -> Result<Vec<gates::Gate>, String> {
    let default = format!("../docs/milestones/{milestone}-gates.toml");
    let gates_path = flags.str_or("gates", &default);
    gates::load(&gates_path)
        .or_else(|_| gates::load(&format!("docs/milestones/{milestone}-gates.toml")))
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
/// binding gate failed.
pub(crate) fn finish_report(
    milestone: &str,
    gates_list: &[gates::Gate],
    m: &Measurements,
    env_ok: bool,
    reference_box: bool,
    artifacts_root: &str,
    header_facts: &str,
) -> Result<(), String> {
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
    report.push_str(&m.raw);

    let report_path = format!("{dir}/report.md");
    let mut file =
        std::fs::File::create(&report_path).map_err(|e| format!("{report_path}: {e}"))?;
    file.write_all(report.as_bytes()).map_err(|e| format!("{report_path}: {e}"))?;
    println!("\ngate-run: report written to {report_path}");
    if binding_failures > 0 {
        return Err(format!("{binding_failures} binding gate(s) FAILED"));
    }
    Ok(())
}

pub(crate) const GATE_RUN_FLAGS: (&[&str], &[&str]) = (
    &["allow-dirty", "unsafe-env", "reference-box", "skip-fill"],
    &[
        "allow-dirty",
        "unsafe-env",
        "reference-box",
        "skip-fill",
        "gates",
        "artifacts-root",
        "replicates",
        "duration",
        "cells",
        "infinityd-bin",
        "redis-bin",
        "fill-keys",
        // M1 rows (ignored by the m0 flow):
        "storm-keys",
        "flushall-keys",
        "maxmemory-mb",
        "subs",
        "sub-channels",
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
        other => Err(format!("unknown milestone {other} (have: m0, m1)")),
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
    let natural = spawn_infinityd(&infinityd, cells, &[])?;
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
    let local_only = spawn_infinityd(&infinityd, cells, &["--route-local-only"])?;
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
            + sum_field(&infos, "wire_buffers_bytes")
            + sum_field(&infos, "conn_state_bytes");
        let divergence = ((our_rss as f64 - domains as f64) / our_rss as f64 * 100.0).abs();
        m.set("attribution_divergence_pct", divergence);
        m.note(format!(
            "attribution: domains {domains} B vs VmRSS {our_rss} B ({divergence:.1}% divergence)"
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
