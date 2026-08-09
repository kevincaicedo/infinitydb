# M4 gate-run report

date: 1786085655 (unix) · cells: 4 · conns: 8 · pipeline: 8 · duration: 600s · dataset: 10× budget · ycsb rows: 2 (M4-S22)
env-check: OK
tier: dev (non-binding)

notes:
- harness: inf-bench ycsb — memtier cannot drive a 10× RAM zipfian tier workload (§6)
- E-adaptation: cursor-scan slices (SCAN <cursor> COUNT 1..=100, pipeline 1) — the keyspace is unordered, no ordered range-scan exists (documented deviation)
- value shape: single constant-byte value of --value-size (default 1 KiB ≈ YCSB's 10×100 B fields); D's `latest` uses a per-connection frontier estimate
- dataset: 10485760 keys × 1024 B = 10240 MiB = 10× the 1024 MiB memory budget · seed 0x1d000826 · θ 0.99
- mode: TIERED — the D8 refusal is lifted; rows run against the tiered namespace
- zipf self-check: top-1% share measured 71.21% vs analytic 70.81% (θ=0.99, n=10485760, 2000000 draws)
- loader: skipped (--skip-fill — an earlier leg of this campaign filled)
- saturation (ycsb-a-zipfian): GENERATOR-LIMITED at 8 conns (+50% moved ops/s +95.0%) — absolutes understate the server; deltas remain valid at fixed generator config
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
| Write amplification, worst tiered namespace | < 3 x user bytes (wal + flush) | 1.77 | PASS |
| Memory-only rows append zero log records (M2 posture carried) | <= 0 records | — | PENDING (tooling) |
| Mixed-node attribution divergence (M4-S20) | <= 10 pct, worst continuous sample | — | PENDING (tooling) |
| Cache-namespace p99 isolation under the mixed node (M4-S20) | <= 10 pct vs same-campaign solo baseline | — | PENDING (tooling) |
| Hot set at memory speed: memory-hit p50 delta | <= 10 pct vs RAM-resident reference leg, memory-hit split | — | PENDING (tooling) |
| Hot set at memory speed: memory-hit p99 delta | <= 10 pct vs RAM-resident reference leg, memory-hit split | — | PENDING (tooling) |
| Hot set at memory speed: memory-hit p99.9 delta | <= 10 pct vs RAM-resident reference leg, memory-hit split (LogHistogram ~3% buckets) | — | PENDING (tooling) |
| Cold reads: p99 < 1.5 ms on NVMe under loaded zipfian rows | < 1.5 ms, cold-read split histogram, worst loaded row | 23.55 | FAIL (DEV-TIER, non-binding) |
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
| ycsb-a-zipfian | 1.775× worst of 4 namespace(s) · blob: n/a (no blob activity) |
| ycsb-b-zipfian | 1.671× worst of 4 namespace(s) · blob: n/a (no blob activity) |

## ycsb-a-zipfian

```
workload = a (zipfian)
ops = 8369955
errors = 0
nils = 0
ops_per_sec = 13950
combined_client p50_us = 2239 · p99_us = 44031 · p999_us = 376831 · max_us = 1905638
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 71.20%
stream_checksum = 0x9aafe706b517de1b
tiering_ram_hit_p50_us (worst cell) = 0
tiering_ram_hit_p99_us (worst cell) = 0
tiering_ram_hit_p999_us (worst cell) = 0
tiering_cold_p50_us (worst cell) = 1087
tiering_cold_p99_us (worst cell) = 23551
tiering_cold_p999_us (worst cell) = 208895
cold_read_qd_p99 (worst cell) = 5
coalesce_ratio_milli (worst cell) = 1000
tiering_cold_resolves = 4945207
tripwire sqes/submit = 7.4
```

## ycsb-b-zipfian

```
workload = b (zipfian)
ops = 12198825
errors = 0
nils = 0
ops_per_sec = 20331
combined_client p50_us = 2015 · p99_us = 24063 · p999_us = 104447 · max_us = 1700375
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 71.22%
stream_checksum = 0x7d4a32ac0a47bdc2
tiering_ram_hit_p50_us (worst cell) = 0
tiering_ram_hit_p99_us (worst cell) = 0
tiering_ram_hit_p999_us (worst cell) = 0
tiering_cold_p50_us (worst cell) = 1055
tiering_cold_p99_us (worst cell) = 16895
tiering_cold_p999_us (worst cell) = 88063
cold_read_qd_p99 (worst cell) = 5
coalesce_ratio_milli (worst cell) = 1000
tiering_cold_resolves = 14118032
tripwire sqes/submit = 7.9
```
