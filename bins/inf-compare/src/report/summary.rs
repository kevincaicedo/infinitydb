//! Summaries operate on per-leg measurements, never on pooled requests.

use std::collections::BTreeMap;
use std::fmt::Write;

use super::{Cell, MemCell};

type Key = (&'static str, &'static str, u32, &'static str, &'static str);

pub(super) fn render(output: &mut String, cells: &[Cell], memory: &[MemCell]) {
    let mut groups: BTreeMap<Key, Vec<f64>> = BTreeMap::new();
    for cell in cells {
        collect_cell(&mut groups, cell);
    }
    for cell in memory {
        for (metric, value) in [
            ("keys", Some(cell.keys as f64)),
            ("baseline MiB", cell.baseline_mib),
            ("after MiB", cell.after_mib),
            ("bytes/key", cell.bytes_per_key),
        ] {
            if let Some(value) = value {
                groups.entry((cell.engine, "memory", 0, "memory", metric)).or_default().push(value);
            }
        }
    }
    let _ = writeln!(output, "## Replicate summaries\n");
    let _ = writeln!(
        output,
        "Median and range of independent leg measurements. Latency quantiles are summarized \
         per run, not pooled. Optional observations disclose their own n. Relative spread is \
         `(max-min)/median*100`; zero medians have no percentage. Memory pipeline 0 means n/a.\n"
    );
    let _ = writeln!(
        output,
        "| Engine | Workload | Pipe | Generator | Metric | n | Median | Min | Max | \
         Relative spread (%) |"
    );
    let _ = writeln!(output, "|---|---|---:|---|---|---:|---:|---:|---:|---:|");
    for ((engine, workload, pipeline, generator, metric), mut values) in groups {
        values.sort_by(f64::total_cmp);
        let count = values.len();
        let median = median(&values);
        let minimum = values[0];
        let maximum = values[count - 1];
        let relative = (maximum - minimum) / median * 100.0;
        let spread = if relative.is_finite() { format!("{relative:.2}") } else { "n/a".into() };
        let _ = writeln!(
            output,
            "| {engine} | {workload} | {pipeline} | {generator} | {metric} | {count} | \
             {median:.3} | {minimum:.3} | {maximum:.3} | {spread} |"
        );
    }
    let _ = writeln!(output);
}

fn collect_cell(groups: &mut BTreeMap<Key, Vec<f64>>, cell: &Cell) {
    let mut add = |generator, metric, value| {
        if let Some(value) = value {
            groups
                .entry((cell.engine, cell.workload, cell.pipeline, generator, metric))
                .or_default()
                .push(value);
        }
    };
    if let Some(metrics) = cell.memtier {
        for (metric, value) in [
            ("ops/s", Some(metrics.ops_per_sec)),
            ("avg ms", Some(metrics.avg_ms)),
            ("p50 ms", Some(metrics.p50_ms)),
            ("p99 ms", Some(metrics.p99_ms)),
            ("p99.9 ms", Some(metrics.p999_ms)),
            ("max ms", metrics.max_ms),
        ] {
            add("memtier", metric, value);
        }
    }
    if let Some(metrics) = cell.redisbench {
        for (metric, value) in [
            ("req/s", metrics.rps),
            ("avg ms", metrics.avg_ms),
            ("p50 ms", metrics.p50_ms),
            ("p99 ms", metrics.p99_ms),
        ] {
            add("redis-benchmark", metric, Some(value));
        }
    }
    add("observation", "RSS MiB", cell.rss_mib);
    add("observation", "server CPU %", cell.server_cpu_pct);
    add("observation", "device MiB written", cell.device_mib_written);
}

fn median(sorted: &[f64]) -> f64 {
    assert!(!sorted.is_empty(), "a summary needs samples");
    let middle = sorted.len() / 2;
    if sorted.len().is_multiple_of(2) {
        sorted[middle - 1] / 2.0 + sorted[middle] / 2.0
    } else {
        sorted[middle]
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn median_handles_odd_even_singleton_and_zero_samples() {
        assert_eq!(super::median(&[10.0, 20.0, 90.0]), 20.0);
        assert_eq!(super::median(&[10.0, 20.0, 30.0, 90.0]), 25.0);
        assert_eq!(super::median(&[3.0]), 3.0);
        assert_eq!(super::median(&[0.0, 0.0, 0.0]), 0.0);
    }
}
