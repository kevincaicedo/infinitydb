//! Drive `memtier_benchmark` and parse its `--json-out-file` output.
//!
//! Run-level numbers come from `ALL STATS / Totals` (always present, even for a
//! pure 1:0 or 0:1 ratio). Percentiles come from the `Percentile Latencies`
//! aggregate, never a per-second `Time-Serie` bucket. Latencies are
//! milliseconds, as memtier reports them.

use std::path::Path;
use std::process::{Command, Stdio};

use crate::json::Json;
use crate::workload::{Kind, Workload};

/// One memtier invocation's measured result.
#[derive(Clone, Copy, Debug)]
pub struct Metrics {
    pub ops_per_sec: f64,
    pub avg_ms: f64,
    pub p50_ms: f64,
    pub p99_ms: f64,
    pub p999_ms: f64,
}

/// Parameters shared by every memtier run in a campaign.
#[derive(Clone, Copy, Debug)]
pub struct Plan<'a> {
    pub host: &'a str,
    pub port: u16,
    pub threads: u16,
    pub clients: usize,
    pub duration: u64,
    pub data_size: usize,
    pub keyspace: u64,
}

/// Run one timed workload at `pipeline` depth; parse `json_path`.
pub fn run(plan: &Plan, wl: &Workload, pipeline: u32, json_path: &Path) -> Result<Metrics, String> {
    let mut args = base_args(plan);
    match wl.kind {
        Kind::Ratio { ratio, expiry } => {
            args.extend(["--ratio".into(), ratio.into(), "--key-pattern".into(), "R:R".into()]);
            if let Some(range) = expiry {
                args.extend(["--expiry-range".into(), range.into()]);
            }
        }
        Kind::Command { command } => {
            args.extend([
                "--command".into(),
                command.into(),
                "--command-key-pattern".into(),
                "R".into(),
            ]);
        }
        Kind::Memory => return Err("memory workload is measured via fill(), not run()".into()),
    }
    if pipeline > 1 {
        args.extend(["--pipeline".into(), pipeline.to_string()]);
    }
    args.extend([
        "--print-percentiles".into(),
        "50,99,99.9".into(),
        "--json-out-file".into(),
        json_path.display().to_string(),
    ]);
    invoke(&args)?;

    let text = std::fs::read_to_string(json_path)
        .map_err(|e| format!("read memtier json {}: {e}", json_path.display()))?;
    let json = Json::parse(&text)?;
    let pct = ["ALL STATS", "Totals", "Percentile Latencies"];
    Ok(Metrics {
        ops_per_sec: json.num_at(&["ALL STATS", "Totals", "Ops/sec"])?,
        avg_ms: json.num_at(&["ALL STATS", "Totals", "Average Latency"])?,
        p50_ms: json.num_at(&[pct[0], pct[1], pct[2], "p50.00"])?,
        p99_ms: json.num_at(&[pct[0], pct[1], pct[2], "p99.00"])?,
        p999_ms: json.num_at(&[pct[0], pct[1], pct[2], "p99.90"])?,
    })
}

/// Sequential write pass to populate the keyspace (GET fill, memory fill). No
/// JSON parsed — this is setup, not a measured row.
pub fn fill(plan: &Plan, secs: u64) -> Result<(), String> {
    let mut args = base_args(plan);
    if let Some(pos) = args.iter().position(|a| a == "--test-time") {
        args[pos + 1] = secs.to_string();
    }
    args.extend(["--ratio".into(), "1:0".into(), "--key-pattern".into(), "S:S".into()]);
    invoke(&args)
}

/// Flags every run shares: target, concurrency, time, value size, keyspace.
fn base_args(plan: &Plan) -> Vec<String> {
    vec![
        "-s".into(),
        plan.host.to_string(),
        "-p".into(),
        plan.port.to_string(),
        "--protocol".into(),
        "redis".into(),
        "-t".into(),
        plan.threads.to_string(),
        "-c".into(),
        plan.clients.to_string(),
        "--test-time".into(),
        plan.duration.to_string(),
        "--data-size".into(),
        plan.data_size.to_string(),
        "--key-maximum".into(),
        plan.keyspace.to_string(),
        "--random-data".into(),
        "--hide-histogram".into(),
    ]
}

fn invoke(args: &[String]) -> Result<(), String> {
    let status = Command::new("memtier_benchmark")
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|e| format!("run memtier_benchmark: {e} (is it on PATH?)"))?;
    if status.success() { Ok(()) } else { Err(format!("memtier_benchmark exited with {status}")) }
}
