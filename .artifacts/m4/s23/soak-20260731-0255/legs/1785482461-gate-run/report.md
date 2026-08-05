# M4 gate-run report

date: 1785482461 (unix) · cells: 4 · conns: 8 · pipeline: 8 · duration: 600s · dataset: 10× budget · ycsb rows: 2 (M4-S22)
env-check: FAILED (overridden — NOT citation-grade)
tier: dev (non-binding)

notes:
- harness: inf-bench ycsb — memtier cannot drive a 10× RAM zipfian tier workload (§6)
- E-adaptation: cursor-scan slices (SCAN <cursor> COUNT 1..=100, pipeline 1) — the keyspace is unordered, no ordered range-scan exists (documented deviation)
- value shape: single constant-byte value of --value-size (default 1 KiB ≈ YCSB's 10×100 B fields); D's `latest` uses a per-connection frontier estimate
- dataset: 655360 keys × 1024 B = 640 MiB = 10× the 64 MiB memory budget · seed 0x1d000826 · θ 0.99
- mode: HARNESS-VALIDATION forced by --named-absent — rows run against `soakdur` with the tiered split rendered named-absent; no tiered gate row is produced
- zipf self-check: top-1% share measured 66.11% vs analytic 65.48% (θ=0.99, n=655360, 2000000 draws)
- loader: skipped (--skip-fill — an earlier leg of this campaign filled)
- saturation (ycsb-a-zipfian): GENERATOR-LIMITED at 8 conns (+50% moved ops/s +37.7%) — absolutes understate the server; deltas remain valid at fixed generator config

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
| Write amplification, worst tiered namespace | < 3 x user bytes (wal + flush) | — | PENDING (tooling) |
| Memory-only rows append zero log records (M2 posture carried) | <= 0 records | — | PENDING (tooling) |
| Mixed-node attribution divergence (M4-S20) | <= 10 pct, worst continuous sample | — | PENDING (tooling) |
| Cache-namespace p99 isolation under the mixed node (M4-S20) | <= 10 pct vs same-campaign solo baseline | — | PENDING (tooling) |
| Hot set at memory speed: memory-hit p50 delta | <= 10 pct vs RAM-resident reference leg, memory-hit split | — | PENDING (tooling) |
| Hot set at memory speed: memory-hit p99 delta | <= 10 pct vs RAM-resident reference leg, memory-hit split | — | PENDING (tooling) |
| Hot set at memory speed: memory-hit p99.9 delta | <= 10 pct vs RAM-resident reference leg, memory-hit split (LogHistogram ~3% buckets) | — | PENDING (tooling) |
| Cold reads: p99 < 1.5 ms on NVMe under loaded zipfian rows | < 1.5 ms, cold-read split histogram, worst loaded row | — | PENDING (tooling) |
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
| ycsb-a-zipfian | n/a (no tiered namespace on the node — memory-mode row) · blob: n/a (no blob activity) |
| ycsb-b-zipfian | n/a (no tiered namespace on the node — memory-mode row) · blob: n/a (no blob activity) |

## ycsb-a-zipfian

```
workload = a (zipfian)
ops = 489202233
errors = 2860836
nils = 0
ops_per_sec = 815336
combined_client p50_us = 44 · p99_us = 123 · p999_us = 1119 · max_us = 1437258
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 66.07%
stream_checksum = 0x648b65df890b7890
memory-hit / cold split: NAMED-ABSENT — the tiered data plane is behind the ADR-0062 D8 refusal; M4-S26 emits the split service histograms (resolver-tagged {mutable, ro, cold}) under the SPLIT_FIELDS names
tripwire sqes/submit = 2.6
```

## ycsb-b-zipfian

```
workload = b (zipfian)
ops = 859848390
errors = 186092
nils = 0
ops_per_sec = 1432811
combined_client p50_us = 39 · p99_us = 111 · p999_us = 227 · max_us = 562049
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 66.07%
stream_checksum = 0x9a7d939456403b78
memory-hit / cold split: NAMED-ABSENT — the tiered data plane is behind the ADR-0062 D8 refusal; M4-S26 emits the split service histograms (resolver-tagged {mutable, ro, cold}) under the SPLIT_FIELDS names
tripwire sqes/submit = 2.9
```
