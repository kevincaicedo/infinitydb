# M4 gate-run report

date: 1786695918 (unix) · cells: 4 · conns: 8 · pipeline: 8 · duration: 600s · dataset: 10× budget · ycsb rows: 2 (M4-S22)
env-check: OK
tier: dev (non-binding)

notes:
- harness: inf-bench ycsb — memtier cannot drive a 10× RAM zipfian tier workload (§6)
- E-adaptation: cursor-scan slices (SCAN <cursor> COUNT 1..=100, pipeline 1) — the keyspace is unordered, no ordered range-scan exists (documented deviation)
- value shape: single constant-byte value of --value-size (default 1 KiB ≈ YCSB's 10×100 B fields); D's `latest` uses a per-connection frontier estimate
- dataset: 20971520 keys × 1024 B = 20480 MiB = 10× the 2048 MiB memory budget · seed 0x1d000826 · θ 0.99
- mode: TIERED — the D8 refusal is lifted; rows run against the tiered namespace
- zipf self-check: top-1% share measured 72.25% vs analytic 71.87% (θ=0.99, n=20971520, 2000000 draws)
- loader: skipped (--skip-fill — an earlier leg of this campaign filled)
- saturation (ycsb-a-zipfian): GENERATOR-LIMITED at 8 conns (+50% moved ops/s +31.6%) — absolutes understate the server; deltas remain valid at fixed generator config
- hot-set gate rows (ycsb:hot_set_*): PENDING the reference leg — run `inf-bench ycsb --dataset-multiple 1` in the same campaign and re-run this leg with `--hot-set-reference <that run's dir or mem-hit.tsv>`; this run publishes its own memory-hit split in `mem-hit.tsv` for that comparison

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
| Write amplification, worst tiered namespace | < 3 x user bytes (wal + flush) | 1.90 | PASS |
| Memory-only rows append zero log records (M2 posture carried) | <= 0 records | — | PENDING (tooling) |
| Mixed-node attribution divergence (M4-S20) | <= 10 pct, worst continuous sample | — | PENDING (tooling) |
| Cache-namespace p99 isolation under the mixed node (M4-S20) | <= 10 pct vs same-campaign solo baseline | — | PENDING (tooling) |
| Hot set at memory speed: memory-hit p50 delta | <= 10 pct vs RAM-resident reference leg, memory-hit split | — | PENDING (tooling) |
| Hot set at memory speed: memory-hit p99 delta | <= 10 pct vs RAM-resident reference leg, memory-hit split | — | PENDING (tooling) |
| Hot set at memory speed: memory-hit p99.9 delta | <= 10 pct vs RAM-resident reference leg, memory-hit split (LogHistogram ~3% buckets) | — | PENDING (tooling) |
| Cold reads: p99 < 1.5 ms on NVMe under loaded zipfian rows | < 1.5 ms, cold-read split histogram, worst loaded row | 73.73 | FAIL (DEV-TIER, non-binding) |
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
| ycsb-a-zipfian | 1.896× worst of 4 namespace(s) · blob: n/a (no blob activity) |
| ycsb-b-zipfian | 1.872× worst of 4 namespace(s) · blob: n/a (no blob activity) |

## ycsb-a-zipfian

```
workload = a (zipfian)
ops = 2304021
errors = 0
nils = 0
ops_per_sec = 3840
combined_client p50_us = 3647 · p99_us = 294911 · p999_us = 1343487 · max_us = 5107218
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 72.23%
stream_checksum = 0x687dfacb7347e35f
tiering_cold_p50_us (worst cell) = 1407
tiering_cold_p99_us (worst cell) = 73727
tiering_cold_p999_us (worst cell) = 491519
cold_read_qd_p99 (worst cell) = 5
coalesce_ratio_milli (worst cell) = 0
server ram-hit fields: WITHDRAWN — tiering_ram_hit_p50_us, tiering_ram_hit_p99_us, tiering_ram_hit_p999_us named-absent, server says `unmeasured-iteration-clock`; the reactor's per-iteration clock cannot time a non-suspending command (ADR-0064 amendment 2026-08-08). The memory-hit half below is client-derived.
tiering_cold_resolves = 1933569
memory-hit split (client-derived, ADR-0071 D2):
  cold_frac = 33.5990% (cold_reads 774128 · cold_resolves 1933449 — re-resolve ratio 2.50×)
  mem_hit p50_us = 2687 · p99_us = 5247 · p999_us = 5375
  separation: mem_hit p99.9 5375 µs vs server cold p50 1407 µs (client tail spread 2688 µs — the truncation only separates the two modes while cold service exceeds it)
  NOT gate-eligible — separation check FAILED: derived memory-hit p99.9 5375 µs >= server cold p50 1407 µs — the two populations overlap and the quantile truncation cannot tell them apart (client tail spread 2688 µs vs cold service 1407 µs)
tripwire sqes/submit = 6.3
```

## ycsb-b-zipfian

```
workload = b (zipfian)
ops = 4885397
errors = 0
nils = 0
ops_per_sec = 8142
combined_client p50_us = 2431 · p99_us = 104447 · p999_us = 458751 · max_us = 3448652
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 72.22%
stream_checksum = 0xdefbf6a0aa96ca16
tiering_cold_p50_us (worst cell) = 1375
tiering_cold_p99_us (worst cell) = 59391
tiering_cold_p999_us (worst cell) = 385023
cold_read_qd_p99 (worst cell) = 5
coalesce_ratio_milli (worst cell) = 0
server ram-hit fields: WITHDRAWN — tiering_ram_hit_p50_us, tiering_ram_hit_p99_us, tiering_ram_hit_p999_us named-absent, server says `unmeasured-iteration-clock`; the reactor's per-iteration clock cannot time a non-suspending command (ADR-0064 amendment 2026-08-08). The memory-hit half below is client-derived.
tiering_cold_resolves = 4904543
memory-hit split (client-derived, ADR-0071 D2):
  cold_frac = 19.5910% (cold_reads 957098 · cold_resolves 1947870 — re-resolve ratio 2.04×)
  mem_hit p50_us = 1951 · p99_us = 4863 · p999_us = 4991
  separation: mem_hit p99.9 4991 µs vs server cold p50 1375 µs (client tail spread 3040 µs — the truncation only separates the two modes while cold service exceeds it)
  NOT gate-eligible — separation check FAILED: derived memory-hit p99.9 4991 µs >= server cold p50 1375 µs — the two populations overlap and the quantile truncation cannot tell them apart (client tail spread 3040 µs vs cold service 1375 µs)
tripwire sqes/submit = 6.1
```
