# M4 gate-run report

date: 1786719988 (unix) · cells: 4 · conns: 8 · pipeline: 8 · duration: 600s · dataset: 10× budget · ycsb rows: 2 (M4-S22)
env-check: OK
tier: dev (non-binding)

notes:
- harness: inf-bench ycsb — memtier cannot drive a 10× RAM zipfian tier workload (§6)
- E-adaptation: cursor-scan slices (SCAN <cursor> COUNT 1..=100, pipeline 1) — the keyspace is unordered, no ordered range-scan exists (documented deviation)
- value shape: single constant-byte value of --value-size (default 1 KiB ≈ YCSB's 10×100 B fields); D's `latest` uses a per-connection frontier estimate
- dataset: 20971520 keys × 1024 B = 20480 MiB = 10× the 2048 MiB memory budget · seed 0xa0077a336 · θ 0.99
- mode: TIERED — the D8 refusal is lifted; rows run against the tiered namespace
- zipf self-check: top-1% share measured 72.27% vs analytic 71.87% (θ=0.99, n=20971520, 2000000 draws)
- loader: skipped (--skip-fill — an earlier leg of this campaign filled)
- saturation (ycsb-a-zipfian): GENERATOR-LIMITED at 8 conns (+50% moved ops/s +35.2%) — absolutes understate the server; deltas remain valid at fixed generator config
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
| Write amplification, worst tiered namespace | < 3 x user bytes (wal + flush) | 1.49 | PASS |
| Memory-only rows append zero log records (M2 posture carried) | <= 0 records | — | PENDING (tooling) |
| Mixed-node attribution divergence (M4-S20) | <= 10 pct, worst continuous sample | — | PENDING (tooling) |
| Cache-namespace p99 isolation under the mixed node (M4-S20) | <= 10 pct vs same-campaign solo baseline | — | PENDING (tooling) |
| Hot set at memory speed: memory-hit p50 delta | <= 10 pct vs RAM-resident reference leg, memory-hit split | — | PENDING (tooling) |
| Hot set at memory speed: memory-hit p99 delta | <= 10 pct vs RAM-resident reference leg, memory-hit split | — | PENDING (tooling) |
| Hot set at memory speed: memory-hit p99.9 delta | <= 10 pct vs RAM-resident reference leg, memory-hit split (LogHistogram ~3% buckets) | — | PENDING (tooling) |
| Cold reads: p99 < 1.5 ms on NVMe under loaded zipfian rows | < 1.5 ms, cold-read split histogram, worst loaded row | 61.44 | FAIL (DEV-TIER, non-binding) |
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
| ycsb-a-zipfian | 1.493× worst of 4 namespace(s) · blob: n/a (no blob activity) |
| ycsb-b-zipfian | 1.488× worst of 4 namespace(s) · blob: n/a (no blob activity) |

## ycsb-a-zipfian

```
workload = a (zipfian)
ops = 3294403
errors = 0
nils = 0
ops_per_sec = 5491
combined_client p50_us = 2495 · p99_us = 196607 · p999_us = 966655 · max_us = 4072804
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 72.22%
stream_checksum = 0xb65ac0840e364b3c
tiering_cold_p50_us (worst cell) = 1343
tiering_cold_p99_us (worst cell) = 61439
tiering_cold_p999_us (worst cell) = 376831
cold_read_qd_p99 (worst cell) = 5
coalesce_ratio_milli (worst cell) = 0
server ram-hit fields: WITHDRAWN — tiering_ram_hit_p50_us, tiering_ram_hit_p99_us, tiering_ram_hit_p999_us named-absent, server says `unmeasured-iteration-clock`; the reactor's per-iteration clock cannot time a non-suspending command (ADR-0064 amendment 2026-08-08). The memory-hit half below is client-derived.
tiering_cold_resolves = 70479145
memory-hit split (client-derived, ADR-0071 D2):
  cold_frac = 19.8315% (cold_reads 653331 · cold_resolves 1632210 — re-resolve ratio 2.50×)
  mem_hit p50_us = 1983 · p99_us = 5631 · p999_us = 5887
  separation: mem_hit p99.9 5887 µs vs server cold p50 1343 µs (client tail spread 3904 µs — the truncation only separates the two modes while cold service exceeds it)
  NOT gate-eligible — separation check FAILED: derived memory-hit p99.9 5887 µs >= server cold p50 1343 µs — the two populations overlap and the quantile truncation cannot tell them apart (client tail spread 3904 µs vs cold service 1343 µs)
tripwire sqes/submit = 6.0
```

## ycsb-b-zipfian

```
workload = b (zipfian)
ops = 4305440
errors = 0
nils = 0
ops_per_sec = 7176
combined_client p50_us = 1983 · p99_us = 131071 · p999_us = 540671 · max_us = 3511135
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 72.22%
stream_checksum = 0x8a9e722e312d16e2
tiering_cold_p50_us (worst cell) = 1311
tiering_cold_p99_us (worst cell) = 61439
tiering_cold_p999_us (worst cell) = 376831
cold_read_qd_p99 (worst cell) = 5
coalesce_ratio_milli (worst cell) = 0
server ram-hit fields: WITHDRAWN — tiering_ram_hit_p50_us, tiering_ram_hit_p99_us, tiering_ram_hit_p999_us named-absent, server says `unmeasured-iteration-clock`; the reactor's per-iteration clock cannot time a non-suspending command (ADR-0064 amendment 2026-08-08). The memory-hit half below is client-derived.
tiering_cold_resolves = 72739793
memory-hit split (client-derived, ADR-0071 D2):
  cold_frac = 13.1426% (cold_reads 565847 · cold_resolves 1146034 — re-resolve ratio 2.03×)
  mem_hit p50_us = 1727 · p99_us = 8959 · p999_us = 10239
  separation: mem_hit p99.9 10239 µs vs server cold p50 1311 µs (client tail spread 8512 µs — the truncation only separates the two modes while cold service exceeds it)
  NOT gate-eligible — separation check FAILED: derived memory-hit p99.9 10239 µs >= server cold p50 1311 µs — the two populations overlap and the quantile truncation cannot tell them apart (client tail spread 8512 µs vs cold service 1311 µs)
tripwire sqes/submit = 6.0
```
