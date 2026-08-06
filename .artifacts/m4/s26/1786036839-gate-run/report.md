# M4 gate-run report

date: 1786036839 (unix) · cells: 4 · conns: 8 · pipeline: 8 · duration: 5s · dataset: 2× budget · ycsb rows: 1 (M4-S22)
env-check: FAILED (overridden — NOT citation-grade)
tier: dev (non-binding)

notes:
- harness: inf-bench ycsb — memtier cannot drive a 10× RAM zipfian tier workload (§6)
- E-adaptation: cursor-scan slices (SCAN <cursor> COUNT 1..=100, pipeline 1) — the keyspace is unordered, no ordered range-scan exists (documented deviation)
- value shape: single constant-byte value of --value-size (default 1 KiB ≈ YCSB's 10×100 B fields); D's `latest` uses a per-connection frontier estimate
- dataset: 131072 keys × 1024 B = 128 MiB = 2× the 64 MiB memory budget · seed 0x1d0c2026 · θ 0.99
- mode: TIERED — the D8 refusal is lifted; rows run against the tiered namespace
- zipf self-check: top-1% share measured 62.06% vs analytic 61.29% (θ=0.99, n=131072, 2000000 draws)
- loader: 131072 keys in 0.2s (867773 sets/s, 1 passes), DBSIZE == keys asserted
- saturation (ycsb-b-zipfian): GENERATOR-LIMITED at 8 conns (+50% moved ops/s +16.9%) — absolutes understate the server; deltas remain valid at fixed generator config
- hot-set gate rows (ycsb:hot_set_*) require the reference leg — run `inf-bench ycsb --dataset-multiple 1` in the same campaign and compare the memory-hit split percentiles (S24 runbook step); this run reports the tiered side only

| gate | threshold | measured | verdict |
|---|---|---|---|
| Degenerate A/B: pipelined ops regression | <= 1 % vs M3 baseline | — | PENDING (tooling) |
| Degenerate A/B: pipelined p99.9 regression | <= 1 % vs M3 baseline (LogHistogram ~3% buckets: nonzero spans >= 1 bucket) | — | PENDING (tooling) |
| Degenerate A/B: unpipelined ops regression | <= 1 % vs M3 baseline | — | PENDING (tooling) |
| Degenerate A/B: unpipelined p99.9 regression | <= 1 % vs M3 baseline | — | PENDING (tooling) |
| Degenerate A/B: ttl-heavy ops regression | <= 1 % vs M3 baseline | — | PENDING (tooling) |
| Degenerate A/B: ttl-heavy p99.9 regression | <= 1 % vs M3 baseline | — | PENDING (tooling) |
| Degenerate A/B: peak-RSS regression (worst row) | <= 1 % vs M3 baseline | — | PENDING (tooling) |
| Memory-mode node constructs zero tiered tables | <= 0 tables | — | PENDING (tooling) |
| Tiering code-path counters identically zero | <= 0 counter sum | — | PENDING (tooling) |
| Write amplification, worst tiered namespace | < 3 x user bytes (wal + flush) | 1.29 | PASS |
| Memory-only rows append zero log records (M2 posture carried) | <= 0 records | — | PENDING (tooling) |
| Mixed-node attribution divergence (M4-S20) | <= 10 pct, worst continuous sample | — | PENDING (tooling) |
| Cache-namespace p99 isolation under the mixed node (M4-S20) | <= 10 pct vs same-campaign solo baseline | — | PENDING (tooling) |
| Hot set at memory speed: memory-hit p50 delta | <= 10 pct vs RAM-resident reference leg, memory-hit split | — | PENDING (tooling) |
| Hot set at memory speed: memory-hit p99 delta | <= 10 pct vs RAM-resident reference leg, memory-hit split | — | PENDING (tooling) |
| Hot set at memory speed: memory-hit p99.9 delta | <= 10 pct vs RAM-resident reference leg, memory-hit split (LogHistogram ~3% buckets) | — | PENDING (tooling) |
| Cold reads: p99 < 1.5 ms on NVMe under loaded zipfian rows | < 1.5 ms, cold-read split histogram, worst loaded row | 0.07 | PASS (DEV-TIER, non-binding) |
| Memory honesty: RSS slope over the 24 h endurance run | < 0.5 pct per 24 h (storm-resistant first/last-5% medians) | — | PENDING (tooling) |
| Endurance: zero crashes over the full 24 h run | <= 0 crashes | — | PENDING (tooling) |
| M3 regression: worst M3 gate delta on memory-mode namespaces | <= 5 pct vs M3 baseline artifact, worst gate | — | PENDING (tooling) |
| Recovery with tiering on: replay throughput per cell | >= 1 GB/s/cell | — | PENDING (tooling) |
| Recovery with tiering on: 10 GB boot | < 15 s | — | PENDING (tooling) |
| Never-none invariant: zero violations in the 10k-seed DST sweep | <= 0 violations | — | PENDING (tooling) |
| Crash + ENOSPC matrices: all fault points green | <= 0 failing rows | — | PENDING (tooling) |
| Foreground protection: p99.9 during demotion + compaction storms | < 2 ms | — | PENDING (tooling) |

## write amplification by row

Per namespace, worst first — never a node-wide blend (M4-S16).

| row | write amplification |
|---|---|
| ycsb-b-zipfian | 1.293× worst of 4 namespace(s) · blob: n/a (no blob activity) |

## loader fill

```
ops = 131072
errors = 0
elapsed_s = 0.151
ops_per_sec = 867773
p50_us = 113
p99_us = 751
p999_us = 1215
p9999_us = 1262
max_us = 1262
```

## ycsb-b-zipfian

```
workload = b (zipfian)
ops = 4015970
errors = 0
nils = 0
ops_per_sec = 803149
combined_client p50_us = 75 · p99_us = 155 · p999_us = 207 · max_us = 2303
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 62.02%
stream_checksum = 0xc5c3737adff7087c
tiering_ram_hit_p50_us (worst cell) = 0
tiering_ram_hit_p99_us (worst cell) = 0
tiering_ram_hit_p999_us (worst cell) = 0
tiering_cold_p50_us (worst cell) = 42
tiering_cold_p99_us (worst cell) = 65
tiering_cold_p999_us (worst cell) = 79
cold_read_qd_p99 (worst cell) = 4
coalesce_ratio_milli (worst cell) = 1000
tiering_cold_resolves = 920152
tripwire sqes/submit = 2.6
```
