//! Drive `redis-benchmark` and parse its `--csv` output.
//!
//! This is the independent cross-check generator: a second, widely-used tool
//! hitting the same engine/workload so the in-house and memtier numbers can
//! never quietly agree through a shared bug (master plan §22). redis-benchmark
//! is request-count-based (`-n`), not time-based like memtier, and reports
//! only p50/p95/p99 — so p99.9 always comes from memtier, and this feeds a
//! throughput-agreement check, not a co-equal latency table.
//!
//! Command gating matters: redis-benchmark's *default* `-t` set fires
//! `lpush/sadd/hset/zadd/lrange`, none of which M1 implements. The caller only
//! ever passes an explicit, M1-surface `-t` test (set/get/incr).

use std::path::Path;
use std::process::Command;

#[derive(Clone, Copy, Debug)]
pub struct Metrics {
    pub rps: f64,
    pub avg_ms: f64,
    pub p50_ms: f64,
    pub p99_ms: f64,
}

#[derive(Clone, Copy, Debug)]
pub struct Plan<'a> {
    pub host: &'a str,
    pub port: u16,
    pub requests: u64,
    pub clients: usize,
    pub data_size: usize,
    pub keyspace: u64,
}

/// Run a single `-t test` and parse the matching CSV row. `csv_path` keeps the
/// raw output as an artifact.
pub fn run(plan: &Plan, test: &str, pipeline: u32, csv_path: &Path) -> Result<Metrics, String> {
    let out = Command::new("redis-benchmark")
        .args([
            "-h",
            plan.host,
            "-p",
            &plan.port.to_string(),
            "-t",
            test,
            "-n",
            &plan.requests.to_string(),
            "-c",
            &plan.clients.to_string(),
            "-P",
            &pipeline.to_string(),
            "-d",
            &plan.data_size.to_string(),
            "-r",
            &plan.keyspace.to_string(),
            "--csv",
        ])
        .output()
        .map_err(|e| format!("run redis-benchmark: {e} (is it on PATH?)"))?;
    if !out.status.success() {
        return Err(format!(
            "redis-benchmark -t {test} exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let csv = String::from_utf8_lossy(&out.stdout).into_owned();
    std::fs::write(csv_path, &csv).map_err(|e| format!("write redis-benchmark csv: {e}"))?;
    parse_csv(&csv, test)
}

/// CSV header (redis 8): `test,rps,avg_latency_ms,min,p50,p95,p99,max`.
fn parse_csv(csv: &str, test: &str) -> Result<Metrics, String> {
    for line in csv.lines().skip(1) {
        let fields: Vec<&str> = line.split(',').map(|f| f.trim().trim_matches('"')).collect();
        if fields.len() >= 8 && fields[0].eq_ignore_ascii_case(test) {
            let num = |i: usize| {
                fields[i]
                    .parse::<f64>()
                    .map_err(|_| format!("redis-benchmark csv: `{}` is not a number", fields[i]))
            };
            return Ok(Metrics { rps: num(1)?, avg_ms: num(2)?, p50_ms: num(4)?, p99_ms: num(6)? });
        }
    }
    Err(format!("redis-benchmark csv: no row matched test `{test}`\n{csv}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_matching_row() {
        let csv = "\"test\",\"rps\",\"avg_latency_ms\",\"min_latency_ms\",\"p50_latency_ms\",\"p95_latency_ms\",\"p99_latency_ms\",\"max_latency_ms\"\n\
                   \"SET\",\"176470.58\",\"0.017\",\"0.000\",\"0.015\",\"0.031\",\"0.039\",\"0.895\"\n";
        let m = parse_csv(csv, "set").unwrap();
        assert_eq!(m.rps, 176470.58);
        assert_eq!(m.p99_ms, 0.039);
    }

    #[test]
    fn missing_test_is_an_error() {
        let csv = "\"test\",\"rps\",\"a\",\"b\",\"c\",\"d\",\"e\",\"f\"\n\"GET\",\"1\",\"0\",\"0\",\"0\",\"0\",\"0\",\"0\"\n";
        assert!(parse_csv(csv, "set").is_err());
    }
}
