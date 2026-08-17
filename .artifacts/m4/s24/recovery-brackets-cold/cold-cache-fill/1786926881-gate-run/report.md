# M4 gate-run report

date: 1786926881 (unix) · cells: 4 · conns: 8 · pipeline: 8 · duration: 20s · dataset: 10× budget · ycsb rows: 10 (M4-S22)
env-check: OK
tier: dev (non-binding)

notes:
- harness: inf-bench ycsb — memtier cannot drive a 10× RAM zipfian tier workload (§6)
- E-adaptation: cursor-scan slices (SCAN <cursor> COUNT 1..=100, pipeline 1) — the keyspace is unordered, no ordered range-scan exists (documented deviation)
- value shape: single constant-byte value of --value-size (default 1 KiB ≈ YCSB's 10×100 B fields); D's `latest` uses a per-connection frontier estimate
- dataset: 10485760 keys × 1024 B = 10240 MiB = 10× the 1024 MiB memory budget · seed 0x1d0c2026 · θ 0.99
- mode: TIERED — the D8 refusal is lifted; rows run against the tiered namespace
- zipf self-check: top-1% share measured 71.23% vs analytic 70.81% (θ=0.99, n=10485760, 2000000 draws)
- loader: 10485760 keys in 121.7s (86167 sets/s, 1 passes), DBSIZE == keys asserted
- hot-set instrument role: tiered leg at conns=8 pipeline=8 value_size=1024 (ADR-0071 D6 — both legs of the comparison must share this config; the comparison refuses on a mismatch)
- saturation (ycsb-a-zipfian): GENERATOR-LIMITED at 8 conns (+50% moved ops/s -14.5%) — absolutes understate the server; deltas remain valid at fixed generator config
- hot-set gate rows (ycsb:hot_set_*): PENDING the reference leg — run `inf-bench ycsb --dataset-multiple 1` in the same campaign at the same --conns/--pipeline/--value-size and re-run this leg with `--hot-set-reference <that run's dir or mem-hit.tsv>`; this run publishes its own memory-hit split in `mem-hit.tsv` for that comparison

| gate | threshold | measured | verdict |
|---|---|---|---|
| Degenerate A/B: pipelined ops regression | <= 1 % vs M3 baseline | — | PENDING (tooling) |
| Degenerate A/B: pipelined p99.9 regression | <= 0 % vs M3 baseline — SAME HISTOGRAM BUCKET OR BETTER (ADR-0070 D4b, 2026-08-16). LogHistogram quantises at 32 sub-buckets/octave = ~3%/bucket, so the only readable states are 0.00 (same bucket) and >= 1 bucket; the former 1% threshold was unreadable and a same-binary A/A control failed it | — | PENDING (tooling) |
| Degenerate A/B: unpipelined ops regression | <= 1 % vs M3 baseline | — | PENDING (tooling) |
| Degenerate A/B: unpipelined p99.9 regression | <= 0 % vs M3 baseline — SAME HISTOGRAM BUCKET OR BETTER (ADR-0070 D4b, 2026-08-16). LogHistogram quantises at ~3%/bucket, so the only readable states are 0.00 (same bucket) and >= 1 bucket; the former 1% threshold was unreadable and a same-binary A/A control failed it | — | PENDING (tooling) |
| Degenerate A/B: ttl-heavy ops regression | <= 1 % vs M3 baseline | — | PENDING (tooling) |
| Degenerate A/B: ttl-heavy p99.9 regression | <= 0 % vs M3 baseline — SAME HISTOGRAM BUCKET OR BETTER (ADR-0070 D4b, 2026-08-16). LogHistogram quantises at ~3%/bucket, so the only readable states are 0.00 (same bucket) and >= 1 bucket; the former 1% threshold was unreadable and a same-binary A/A control failed it | — | PENDING (tooling) |
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
| Cold reads: p99 < 1.5 ms on NVMe under loaded zipfian rows | < 1.5 ms, cold-read split histogram, worst loaded row | 2.02 | FAIL (DEV-TIER, non-binding) |
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
| ycsb-a-zipfian | 1.902× worst of 4 namespace(s) · blob: n/a (no blob activity) |
| ycsb-a-uniform | 1.894× worst of 4 namespace(s) · blob: n/a (no blob activity) |
| ycsb-b-zipfian | 1.888× worst of 4 namespace(s) · blob: n/a (no blob activity) |
| ycsb-b-uniform | 1.886× worst of 4 namespace(s) · blob: n/a (no blob activity) |
| ycsb-c-zipfian | 1.886× worst of 4 namespace(s) · blob: n/a (no blob activity) |
| ycsb-c-uniform | 1.886× worst of 4 namespace(s) · blob: n/a (no blob activity) |
| ycsb-d-latest | 1.888× worst of 4 namespace(s) · blob: n/a (no blob activity) |
| ycsb-e-zipfian | 1.887× worst of 4 namespace(s) · blob: n/a (no blob activity) |
| ycsb-f-zipfian | 1.861× worst of 4 namespace(s) · blob: n/a (no blob activity) |
| ycsb-f-uniform | 1.844× worst of 4 namespace(s) · blob: n/a (no blob activity) |

## loader fill

```
ops = 10485760
errors = 0
busy_retryable = 0
elapsed_s = 121.692
ops_per_sec = 86167
p50_us = 179
p99_us = 8447
p999_us = 151551
p9999_us = 1441791
max_us = 4127109
```

## ycsb-a-zipfian

```
workload = a (zipfian)
ops = 987635
errors = 0
nils = 0
ops_per_sec = 49380
combined_client p50_us = 687 · p99_us = 17407 · p999_us = 92159 · max_us = 425399
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 71.17%
stream_checksum = 0x072ff962d9939826
tiering_cold_p50_us (worst cell) = 271
tiering_cold_p99_us (worst cell) = 2015
tiering_cold_p999_us (worst cell) = 39935
cold_read_qd_p99 (worst cell) = 5
coalesce_ratio_milli (worst cell) = 0
server ram-hit fields: WITHDRAWN — tiering_ram_hit_p50_us, tiering_ram_hit_p99_us, tiering_ram_hit_p999_us named-absent, server says `unmeasured-iteration-clock`; the reactor's per-iteration clock cannot time a non-suspending command (ADR-0064 amendment 2026-08-08). The memory-hit half below is client-derived.
tiering_cold_resolves = 905219
memory-hit split (client-derived, ADR-0071 D2):
  cold_frac = 36.6431% (cold_reads 361900 · cold_resolves 905150 — re-resolve ratio 2.50×)
  mem_hit p50_us = 503 · p99_us = 831 · p999_us = 831
  separation: mem_hit p99.9 831 µs vs server cold p50 271 µs (client tail spread 328 µs — the truncation only separates the two modes while cold service exceeds it)
  NOT gate-eligible — separation check FAILED: derived memory-hit p99.9 831 µs >= server cold p50 271 µs — the two populations overlap and the quantile truncation cannot tell them apart (client tail spread 328 µs vs cold service 271 µs)
tripwire sqes/submit = 2.8
```

## ycsb-a-uniform

```
workload = a (uniform)
ops = 543765
errors = 0
nils = 0
ops_per_sec = 27186
combined_client p50_us = 1599 · p99_us = 7935 · p999_us = 73727 · max_us = 138344
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 0.00%
stream_checksum = 0x6f897a7338f73bc1
tiering_cold_p50_us (worst cell) = 287
tiering_cold_p99_us (worst cell) = 1183
tiering_cold_p999_us (worst cell) = 56319
cold_read_qd_p99 (worst cell) = 6
coalesce_ratio_milli (worst cell) = 0
server ram-hit fields: WITHDRAWN — tiering_ram_hit_p50_us, tiering_ram_hit_p99_us, tiering_ram_hit_p999_us named-absent, server says `unmeasured-iteration-clock`; the reactor's per-iteration clock cannot time a non-suspending command (ADR-0064 amendment 2026-08-08). The memory-hit half below is client-derived.
tiering_cold_resolves = 2515657
memory-hit split (client-derived, ADR-0071 D2):
  cold_frac = 92.7967% (cold_reads 504596 · cold_resolves 1261891 — re-resolve ratio 2.50×)
  mem_hit p50_us = 575 · p99_us = 703 · p999_us = 703
  separation: mem_hit p99.9 703 µs vs server cold p50 287 µs (client tail spread 128 µs — the truncation only separates the two modes while cold service exceeds it)
  NOT gate-eligible — cold fraction 92.8% > 50% — the hot set is not memory-resident in this row, so the truncation describes no useful population
tripwire sqes/submit = 2.4
```

## ycsb-b-zipfian

```
workload = b (zipfian)
ops = 2407935
errors = 0
nils = 0
ops_per_sec = 120391
combined_client p50_us = 471 · p99_us = 1695 · p999_us = 3583 · max_us = 11833
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 71.16%
stream_checksum = 0xc6f7d120420adc55
tiering_cold_p50_us (worst cell) = 271
tiering_cold_p99_us (worst cell) = 959
tiering_cold_p999_us (worst cell) = 26111
cold_read_qd_p99 (worst cell) = 5
coalesce_ratio_milli (worst cell) = 0
server ram-hit fields: WITHDRAWN — tiering_ram_hit_p50_us, tiering_ram_hit_p99_us, tiering_ram_hit_p999_us named-absent, server says `unmeasured-iteration-clock`; the reactor's per-iteration clock cannot time a non-suspending command (ADR-0064 amendment 2026-08-08). The memory-hit half below is client-derived.
tiering_cold_resolves = 3670026
memory-hit split (client-derived, ADR-0071 D2):
  cold_frac = 23.5207% (cold_reads 566363 · cold_resolves 1154369 — re-resolve ratio 2.04×)
  mem_hit p50_us = 375 · p99_us = 735 · p999_us = 751
  separation: mem_hit p99.9 751 µs vs server cold p50 271 µs (client tail spread 376 µs — the truncation only separates the two modes while cold service exceeds it)
  NOT gate-eligible — separation check FAILED: derived memory-hit p99.9 751 µs >= server cold p50 271 µs — the two populations overlap and the quantile truncation cannot tell them apart (client tail spread 376 µs vs cold service 271 µs)
tripwire sqes/submit = 2.0
```

## ycsb-b-uniform

```
workload = b (uniform)
ops = 866276
errors = 0
nils = 0
ops_per_sec = 43311
combined_client p50_us = 1311 · p99_us = 4223 · p999_us = 6911 · max_us = 30377
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 0.00%
stream_checksum = 0x90d0bcb5f9f51094
tiering_cold_p50_us (worst cell) = 271
tiering_cold_p99_us (worst cell) = 879
tiering_cold_p999_us (worst cell) = 23551
cold_read_qd_p99 (worst cell) = 5
coalesce_ratio_milli (worst cell) = 0
server ram-hit fields: WITHDRAWN — tiering_ram_hit_p50_us, tiering_ram_hit_p99_us, tiering_ram_hit_p999_us named-absent, server says `unmeasured-iteration-clock`; the reactor's per-iteration clock cannot time a non-suspending command (ADR-0064 amendment 2026-08-08). The memory-hit half below is client-derived.
tiering_cold_resolves = 4884715
memory-hit split (client-derived, ADR-0071 D2):
  cold_frac = 69.0602% (cold_reads 598252 · cold_resolves 1214689 — re-resolve ratio 2.03×)
  mem_hit p50_us = 703 · p99_us = 975 · p999_us = 975
  separation: mem_hit p99.9 975 µs vs server cold p50 271 µs (client tail spread 272 µs — the truncation only separates the two modes while cold service exceeds it)
  NOT gate-eligible — cold fraction 69.1% > 50% — the hot set is not memory-resident in this row, so the truncation describes no useful population
tripwire sqes/submit = 1.9
```

## ycsb-c-zipfian

```
workload = c (zipfian)
ops = 2510043
errors = 0
nils = 0
ops_per_sec = 125496
combined_client p50_us = 463 · p99_us = 1535 · p999_us = 2047 · max_us = 3343
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 71.17%
stream_checksum = 0x6078f77d1cb6c374
tiering_cold_p50_us (worst cell) = 263
tiering_cold_p99_us (worst cell) = 831
tiering_cold_p999_us (worst cell) = 18431
cold_read_qd_p99 (worst cell) = 5
coalesce_ratio_milli (worst cell) = 0
server ram-hit fields: WITHDRAWN — tiering_ram_hit_p50_us, tiering_ram_hit_p99_us, tiering_ram_hit_p999_us named-absent, server says `unmeasured-iteration-clock`; the reactor's per-iteration clock cannot time a non-suspending command (ADR-0064 amendment 2026-08-08). The memory-hit half below is client-derived.
tiering_cold_resolves = 6041904
memory-hit split (client-derived, ADR-0071 D2):
  cold_frac = 23.0511% (cold_reads 578593 · cold_resolves 1157189 — re-resolve ratio 2.00×)
  mem_hit p50_us = 367 · p99_us = 735 · p999_us = 735
  separation: mem_hit p99.9 735 µs vs server cold p50 263 µs (client tail spread 368 µs — the truncation only separates the two modes while cold service exceeds it)
  NOT gate-eligible — separation check FAILED: derived memory-hit p99.9 735 µs >= server cold p50 263 µs — the two populations overlap and the quantile truncation cannot tell them apart (client tail spread 368 µs vs cold service 263 µs)
tripwire sqes/submit = 1.8
```

## ycsb-c-uniform

```
workload = c (uniform)
ops = 979011
errors = 0
nils = 0
ops_per_sec = 48947
combined_client p50_us = 1215 · p99_us = 3327 · p999_us = 4351 · max_us = 6933
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 0.00%
stream_checksum = 0x47342f4d5f363906
tiering_cold_p50_us (worst cell) = 271
tiering_cold_p99_us (worst cell) = 847
tiering_cold_p999_us (worst cell) = 16383
cold_read_qd_p99 (worst cell) = 5
coalesce_ratio_milli (worst cell) = 0
server ram-hit fields: WITHDRAWN — tiering_ram_hit_p50_us, tiering_ram_hit_p99_us, tiering_ram_hit_p999_us named-absent, server says `unmeasured-iteration-clock`; the reactor's per-iteration clock cannot time a non-suspending command (ADR-0064 amendment 2026-08-08). The memory-hit half below is client-derived.
tiering_cold_resolves = 7403166
memory-hit split (client-derived, ADR-0071 D2):
  cold_frac = 69.5222% (cold_reads 680630 · cold_resolves 1361262 — re-resolve ratio 2.00×)
  mem_hit p50_us = 655 · p99_us = 911 · p999_us = 911
  separation: mem_hit p99.9 911 µs vs server cold p50 271 µs (client tail spread 256 µs — the truncation only separates the two modes while cold service exceeds it)
  NOT gate-eligible — cold fraction 69.5% > 50% — the hot set is not memory-resident in this row, so the truncation describes no useful population
tripwire sqes/submit = 1.7
```

## ycsb-d-latest

```
workload = d (latest)
ops = 1985855
errors = 0
nils = 528856
ops_per_sec = 99288
combined_client p50_us = 559 · p99_us = 1887 · p999_us = 6783 · max_us = 28378
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 71.20%
stream_checksum = 0x4c17bd14e27536b8
tiering_cold_p50_us (worst cell) = 255
tiering_cold_p99_us (worst cell) = 815
tiering_cold_p999_us (worst cell) = 11007
cold_read_qd_p99 (worst cell) = 5
coalesce_ratio_milli (worst cell) = 0
server ram-hit fields: WITHDRAWN — tiering_ram_hit_p50_us, tiering_ram_hit_p99_us, tiering_ram_hit_p999_us named-absent, server says `unmeasured-iteration-clock`; the reactor's per-iteration clock cannot time a non-suspending command (ADR-0064 amendment 2026-08-08). The memory-hit half below is client-derived.
tiering_cold_resolves = 8583409
memory-hit split (client-derived, ADR-0071 D2):
  cold_frac = 29.7162% (cold_reads 590120 · cold_resolves 1180243 — re-resolve ratio 2.00×)
  mem_hit p50_us = 439 · p99_us = 751 · p999_us = 767
  separation: mem_hit p99.9 767 µs vs server cold p50 255 µs (client tail spread 328 µs — the truncation only separates the two modes while cold service exceeds it)
  NOT gate-eligible — separation check FAILED: derived memory-hit p99.9 767 µs >= server cold p50 255 µs — the two populations overlap and the quantile truncation cannot tell them apart (client tail spread 328 µs vs cold service 255 µs)
tripwire sqes/submit = 1.6
```

## ycsb-e-zipfian

```
workload = e (zipfian)
ops = 12573
errors = 0
nils = 0
ops_per_sec = 628
combined_client p50_us = 12799 · p99_us = 27135 · p999_us = 30719 · max_us = 47473
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 0.00%
stream_checksum = 0x8865115c385d19d9
tiering_cold_p50_us (worst cell) = 255
tiering_cold_p99_us (worst cell) = 1151
tiering_cold_p999_us (worst cell) = 24063
cold_read_qd_p99 (worst cell) = 5
coalesce_ratio_milli (worst cell) = 0
server ram-hit fields: WITHDRAWN — tiering_ram_hit_p50_us, tiering_ram_hit_p99_us, tiering_ram_hit_p999_us named-absent, server says `unmeasured-iteration-clock`; the reactor's per-iteration clock cannot time a non-suspending command (ADR-0064 amendment 2026-08-08). The memory-hit half below is client-derived.
tiering_cold_resolves = 9209230
memory-hit split (client-derived, ADR-0071 D2):
  cold_frac = 100.0000% (cold_reads 624832 · cold_resolves 625821 — re-resolve ratio 1.00×)
  mem_hit p50_us = 12 · p99_us = 12 · p999_us = 12
  separation: mem_hit p99.9 12 µs vs server cold p50 255 µs (client tail spread 0 µs — the truncation only separates the two modes while cold service exceeds it)
  NOT gate-eligible — 12573 ops < 100000 — too few for a p99.9 gate value
tripwire sqes/submit = 1.6
```

## ycsb-f-zipfian

```
workload = f (zipfian)
ops = 845010
errors = 0
nils = 0
ops_per_sec = 42249
combined_client p50_us = 375 · p99_us = 39935 · p999_us = 163839 · max_us = 632810
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 71.21%
stream_checksum = 0x208dc2b0329f46c5
tiering_cold_p50_us (worst cell) = 255
tiering_cold_p99_us (worst cell) = 1183
tiering_cold_p999_us (worst cell) = 25599
cold_read_qd_p99 (worst cell) = 5
coalesce_ratio_milli (worst cell) = 0
server ram-hit fields: WITHDRAWN — tiering_ram_hit_p50_us, tiering_ram_hit_p99_us, tiering_ram_hit_p999_us named-absent, server says `unmeasured-iteration-clock`; the reactor's per-iteration clock cannot time a non-suspending command (ADR-0064 amendment 2026-08-08). The memory-hit half below is client-derived.
tiering_cold_resolves = 9524960
memory-hit split (client-derived, ADR-0071 D2):
  cold_frac = 18.0620% (cold_reads 152626 · cold_resolves 315730 — re-resolve ratio 2.07×)
  mem_hit p50_us = 311 · p99_us = 703 · p999_us = 703
  separation: mem_hit p99.9 703 µs vs server cold p50 255 µs (client tail spread 392 µs — the truncation only separates the two modes while cold service exceeds it)
  NOT gate-eligible — separation check FAILED: derived memory-hit p99.9 703 µs >= server cold p50 255 µs — the two populations overlap and the quantile truncation cannot tell them apart (client tail spread 392 µs vs cold service 255 µs)
tripwire sqes/submit = 1.6
```

## ycsb-f-uniform

```
workload = f (uniform)
ops = 584174
errors = 0
nils = 0
ops_per_sec = 29205
combined_client p50_us = 1375 · p99_us = 6911 · p999_us = 126975 · max_us = 1215540
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 0.00%
stream_checksum = 0x0551f60db4c7cf09
tiering_cold_p50_us (worst cell) = 255
tiering_cold_p99_us (worst cell) = 1119
tiering_cold_p999_us (worst cell) = 26111
cold_read_qd_p99 (worst cell) = 5
coalesce_ratio_milli (worst cell) = 0
server ram-hit fields: WITHDRAWN — tiering_ram_hit_p50_us, tiering_ram_hit_p99_us, tiering_ram_hit_p999_us named-absent, server says `unmeasured-iteration-clock`; the reactor's per-iteration clock cannot time a non-suspending command (ADR-0064 amendment 2026-08-08). The memory-hit half below is client-derived.
tiering_cold_resolves = 10402261
memory-hit split (client-derived, ADR-0071 D2):
  cold_frac = 69.7669% (cold_reads 407560 · cold_resolves 877301 — re-resolve ratio 2.15×)
  mem_hit p50_us = 719 · p99_us = 991 · p999_us = 991
  separation: mem_hit p99.9 991 µs vs server cold p50 255 µs (client tail spread 272 µs — the truncation only separates the two modes while cold service exceeds it)
  NOT gate-eligible — cold fraction 69.8% > 50% — the hot set is not memory-resident in this row, so the truncation describes no useful population
tripwire sqes/submit = 1.6
```
