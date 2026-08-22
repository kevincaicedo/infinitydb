//! Render the markdown report. One file per run, plus the raw generator output
//! it was built from (left in `raw/` by the caller). The report leads with the
//! tier banner so a number can never be quoted without its honesty context.

use std::fmt::Write;

use crate::env::Env;
use crate::memtier;
use crate::redisbench;

/// One engine's published launch config + peak memory.
pub struct EngineConfig {
    pub label: &'static str,
    pub version: String,
    pub mode: &'static str,
    pub launch_cmd: String,
    pub peak_rss_mib: Option<f64>,
}

/// One measured (engine × workload × pipeline) cell — either or both generators.
pub struct Cell {
    pub engine: &'static str,
    pub workload: &'static str,
    pub pipeline: u32,
    pub memtier: Option<memtier::Metrics>,
    pub redisbench: Option<redisbench::Metrics>,
    pub rss_mib: Option<f64>,
    /// M4.5-S40 disclosures: the server's CPU across the memtier row
    /// (% of one core; host launches only) and the block device's bytes
    /// written during it (`--device-stat`).
    pub server_cpu_pct: Option<f64>,
    pub device_mib_written: Option<f64>,
    /// Selected before/after persistence facts; complete raw INFO snapshots
    /// are stored beside the generator output.
    pub persistence_delta: Option<String>,
}

/// One bytes/key attribution row.
pub struct MemCell {
    pub engine: &'static str,
    pub keys: u64,
    pub value_size: usize,
    pub baseline_mib: Option<f64>,
    pub after_mib: Option<f64>,
    pub bytes_per_key: Option<f64>,
}

/// Run-level parameters echoed into the header.
pub struct Params {
    pub stamp_secs: u64,
    pub mode: String,
    pub generators: String,
    pub duration: u64,
    pub threads: u16,
    pub clients: usize,
    pub data_size: usize,
    pub keyspace: u64,
    pub pipelines: Vec<u32>,
    pub fill_secs: u64,
    pub rb_requests: u64,
    pub crosscheck_pct: f64,
    pub maxmemory_mb: Option<u64>,
    /// M4.5-S40: the offered rate (`None` = closed loop), the durability
    /// class every engine ran in, the durable data root, the device
    /// sampled.
    pub rate: Option<u64>,
    pub durability: String,
    pub data_root: Option<String>,
    pub device_stat: Option<String>,
    pub redis_no_auto_rewrite: bool,
}

pub fn render(
    env: &Env,
    p: &Params,
    engines: &[EngineConfig],
    cells: &[Cell],
    mem: &[MemCell],
) -> String {
    let mut md = String::new();
    let dirty = if env.git_dirty { "-dirty" } else { "" };
    let pipelines = p.pipelines.iter().map(u32::to_string).collect::<Vec<_>>().join(", ");

    // ---- banner ----
    let _ = writeln!(md, "# inf-compare — competitive benchmark report\n");
    let _ = writeln!(md, "> **Tier:** {}", env.tier);
    if !env.reasons.is_empty() {
        let _ = writeln!(md, ">");
        for reason in &env.reasons {
            let _ = writeln!(md, "> - {reason}");
        }
    }
    let _ = writeln!(md);

    // ---- meta ----
    let maxmem = p.maxmemory_mb.map(|m| format!("{m} MB")).unwrap_or_else(|| "unset".into());
    let _ = writeln!(md, "| | |");
    let _ = writeln!(md, "|---|---|");
    let _ = writeln!(md, "| Generated | unix {} |", p.stamp_secs);
    let _ = writeln!(md, "| Git | `{}{}` |", env.git_sha, dirty);
    let _ = writeln!(md, "| Host | {}, {} cores |", env.kernel, env.cores);
    let _ = writeln!(md, "| CPU governor / EPP | `{}` / `{}` |", env.governor, env.epp);
    if let Some(detail) = &env.envcheck {
        let _ = writeln!(md, "| inf-bench env-check | {detail} |");
    }
    let _ = writeln!(md, "| Mode | {} |", p.mode);
    let _ = writeln!(md, "| Generators | {} |", p.generators);
    let _ = writeln!(md, "| memtier | `{}` |", env.memtier_version);
    if p.generators.contains("redis-benchmark") {
        let _ = writeln!(md, "| redis-benchmark | `{}` |", env.redisbench_version);
    }
    let _ = writeln!(
        md,
        "| Parameters | duration={}s · threads={} · clients={} · value={} B · keyspace={} · pipeline={} · maxmemory={} |",
        p.duration, p.threads, p.clients, p.data_size, p.keyspace, pipelines, maxmem
    );
    let _ = writeln!(
        md,
        "| Load shape | {} · durability={}{}{} |",
        p.rate.map_or("closed loop".to_string(), |r| format!(
            "offered {r} ops/s (memtier --rate-limiting {} per connection × {} connections)",
            r.div_ceil((u64::from(p.threads) * p.clients as u64).max(1)),
            u64::from(p.threads) * p.clients as u64
        )),
        p.durability,
        p.data_root.as_deref().map_or(String::new(), |d| format!(" · data root `{d}`")),
        p.device_stat.as_deref().map_or(String::new(), |d| format!(" · device `{d}`"))
    );
    let _ = writeln!(md);

    // ---- published configs ----
    let _ = writeln!(md, "## Engines — published configs\n");
    let _ = writeln!(md, "| Engine | Mode | Version | Peak RSS (MiB) | Launch command |");
    let _ = writeln!(md, "|---|---|---|---:|---|");
    for e in engines {
        let _ = writeln!(
            md,
            "| {} | {} | {} | {} | `{}` |",
            e.label,
            e.mode,
            e.version,
            fmt_opt(e.peak_rss_mib, 1),
            e.launch_cmd
        );
    }
    let _ = writeln!(md);

    // ---- memtier results ----
    if cells.iter().any(|c| c.memtier.is_some()) {
        let _ = writeln!(md, "## Results — memtier_benchmark\n");
        let _ = writeln!(
            md,
            "| Engine | Workload | Pipe | Throughput (ops/s) | achieved/offered | avg (ms) | p50 (ms) | p99 (ms) | p99.9 (ms) | max (ms) | server CPU (%) | device MiB written | RSS (MiB) |"
        );
        let _ = writeln!(md, "|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|");
        for c in cells {
            let Some(m) = c.memtier else { continue };
            let achieved = m.offered_ops_per_sec.map(|offered| {
                let x = m.ops_per_sec / offered.max(1) as f64;
                if x < 0.9 { format!("{x:.2} ⚠ generator short") } else { format!("{x:.2}") }
            });
            let _ = writeln!(
                md,
                "| {} | {} | {} | {:.0} | {} | {:.3} | {:.3} | {:.3} | {:.3} | {} | {} | {} | {} |",
                c.engine,
                c.workload,
                c.pipeline,
                m.ops_per_sec,
                achieved.unwrap_or_else(|| "closed loop".to_string()),
                m.avg_ms,
                m.p50_ms,
                m.p99_ms,
                m.p999_ms,
                fmt_opt(m.max_ms, 3),
                fmt_opt(c.server_cpu_pct, 0),
                fmt_opt(c.device_mib_written, 1),
                fmt_opt(c.rss_mib, 1)
            );
        }
        let _ = writeln!(md);

        let _ = writeln!(md, "### Persistence and stall deltas\n");
        let _ = writeln!(md, "| Engine | Workload | Pipe | Before/after summary |");
        let _ = writeln!(md, "|---|---|---:|---|");
        for c in cells.iter().filter(|c| c.memtier.is_some()) {
            let _ = writeln!(
                md,
                "| {} | {} | {} | {} |",
                c.engine,
                c.workload,
                c.pipeline,
                c.persistence_delta.as_deref().unwrap_or("n/a")
            );
        }
        let _ = writeln!(md);
    }

    // ---- redis-benchmark results ----
    if cells.iter().any(|c| c.redisbench.is_some()) {
        let _ = writeln!(md, "## Results — redis-benchmark\n");
        let _ = writeln!(
            md,
            "| Engine | Workload | Pipe | Throughput (req/s) | avg (ms) | p50 (ms) | p99 (ms) |"
        );
        let _ = writeln!(md, "|---|---|---:|---:|---:|---:|---:|");
        for c in cells {
            let Some(r) = c.redisbench else { continue };
            let _ = writeln!(
                md,
                "| {} | {} | {} | {:.0} | {:.3} | {:.3} | {:.3} |",
                c.engine, c.workload, c.pipeline, r.rps, r.avg_ms, r.p50_ms, r.p99_ms
            );
        }
        let _ = writeln!(md);
    }

    // ---- cross-check ----
    if cells.iter().any(|c| c.memtier.is_some() && c.redisbench.is_some()) {
        let _ = writeln!(md, "## Cross-check — memtier vs redis-benchmark throughput\n");
        let _ = writeln!(
            md,
            "Independent-generator agreement on the same engine/workload. Flagged when the two disagree by more than {:.0}%.\n",
            p.crosscheck_pct
        );
        let _ = writeln!(
            md,
            "| Engine | Workload | Pipe | memtier (ops/s) | redis-bench (req/s) | Δ | |"
        );
        let _ = writeln!(md, "|---|---|---:|---:|---:|---:|:--|");
        for c in cells {
            let (Some(m), Some(r)) = (c.memtier, c.redisbench) else { continue };
            let delta = if r.rps > 0.0 { (m.ops_per_sec - r.rps) / r.rps * 100.0 } else { 0.0 };
            let flag = if delta.abs() > p.crosscheck_pct { "⚠ diverges" } else { "ok" };
            let _ = writeln!(
                md,
                "| {} | {} | {} | {:.0} | {:.0} | {:+.1}% | {} |",
                c.engine, c.workload, c.pipeline, m.ops_per_sec, r.rps, delta, flag
            );
        }
        let _ = writeln!(md);
    }

    // ---- memory attribution ----
    if !mem.is_empty() {
        let _ = writeln!(md, "## Memory attribution — bytes/key\n");
        let _ = writeln!(
            md,
            "Fill the keyspace, then `(RSS_after − RSS_baseline) ÷ DBSIZE`. The L5 gate shape; the binding ≤ 1.0× Redis gate is `inf-bench gate-run m1` on the reference box.\n"
        );
        let _ = writeln!(
            md,
            "| Engine | Keys | Value (B) | RSS baseline (MiB) | RSS after (MiB) | bytes/key |"
        );
        let _ = writeln!(md, "|---|---:|---:|---:|---:|---:|");
        for m in mem {
            let _ = writeln!(
                md,
                "| {} | {} | {} | {} | {} | {} |",
                m.engine,
                m.keys,
                m.value_size,
                fmt_opt(m.baseline_mib, 1),
                fmt_opt(m.after_mib, 1),
                fmt_opt(m.bytes_per_key, 1)
            );
        }
        let _ = writeln!(md);
    }

    // ---- notes ----
    let _ = writeln!(md, "## Notes & honesty\n");
    if !env.binding {
        let _ = writeln!(
            md,
            "- **Non-citable run.** DEV-TIER numbers prove the harness and show relative shape only. A binding number needs `--reference-box` on a clean box (the M0-R2 standing obligation). Authoritative gate: `inf-bench env-check`."
        );
    }
    let _ = writeln!(
        md,
        "- redis command execution is single-threaded, but its process tree was allowed the same {} CPUs as InfinityDB's cells so AOF rewrite children did not contend with the command thread on one pinned CPU. Each engine's config is recorded above.",
        p.threads
    );
    let _ = writeln!(
        md,
        "- GET rows were measured after a {}s sequential populate; redis-benchmark uses its own key format, so its GET cross-check reads against keys memtier didn't write (throughput-comparable, hit rate not).",
        p.fill_secs
    );
    let _ = writeln!(
        md,
        "- redis-benchmark is request-count based (`-n {}`) and reports only p50/p95/p99; p99.9 always comes from memtier. The two are compared on throughput, not latency.",
        p.rb_requests
    );
    let _ = writeln!(
        md,
        "- Pub/sub fan-out latency is **not** measured here — memtier/redis-benchmark don't set up subscribers. That row lives in `inf-bench gate-run m1` (delivery-acked)."
    );
    if p.mode.contains("docker") {
        let _ = writeln!(
            md,
            "- Under docker, RSS is the container's `docker stats` memory (no separate peak); infinitydb runs with the io_uring seccomp profile because Docker's default seccomp denies io_uring."
        );
    }
    if p.rate.is_some() {
        let _ = writeln!(
            md,
            "- **Offered-rate row (M4.5-S40).** memtier paces each connection at `--rate-limiting` = rate ÷ connections; `achieved/offered` below 0.90 means the generator (or the server) could not hold the rate and the latency columns are not an offered-rate measurement. `max (ms)` is memtier's worst request; server CPU covers the host process plus Redis's completed AOF-child CPU and live descendants; device MiB written is the block device's sectors-written delta (journal and metadata included, NAND amplification not). Raw INFO before/after each row is under `raw/`."
        );
    }
    if p.redis_no_auto_rewrite {
        let _ = writeln!(
            md,
            "- **Non-production diagnostic arm.** Redis ran `auto-aof-rewrite-percentage 0`; this isolates automatic rewrite cost and cannot support a production/default-config comparison."
        );
    }
    if p.durability != "none (in-memory)" {
        let _ = writeln!(
            md,
            "- **Durability {}.** redis ran `--appendonly yes --appendfsync everysec` (its AOF under the data root); infinitydb ran `--data-dir` with every connection starting in an `FSYNC everysec` namespace (`--conn-default-ns cmp`, proven by a probe key before the row) — the same ≤ 1 s power-loss window on both sides, each engine's own mechanism, both on the same device.",
            p.durability
        );
    }
    let _ =
        writeln!(md, "- Raw memtier JSON + redis-benchmark CSV for every row are under `raw/`.");

    md
}

fn fmt_opt(value: Option<f64>, places: usize) -> String {
    match value {
        Some(v) => format!("{v:.places$}"),
        None => "n/a".to_string(),
    }
}
