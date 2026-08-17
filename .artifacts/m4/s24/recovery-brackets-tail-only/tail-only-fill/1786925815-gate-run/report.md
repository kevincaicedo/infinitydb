# M4 gate-run report

date: 1786925815 (unix) · cells: 4 · conns: 8 · pipeline: 8 · duration: 20s · dataset: 10× budget · ycsb rows: 10 (M4-S22)
env-check: OK
tier: dev (non-binding)

notes:
- harness: inf-bench ycsb — memtier cannot drive a 10× RAM zipfian tier workload (§6)
- E-adaptation: cursor-scan slices (SCAN <cursor> COUNT 1..=100, pipeline 1) — the keyspace is unordered, no ordered range-scan exists (documented deviation)
- value shape: single constant-byte value of --value-size (default 1 KiB ≈ YCSB's 10×100 B fields); D's `latest` uses a per-connection frontier estimate
- dataset: 10485760 keys × 1024 B = 10240 MiB = 10× the 1024 MiB memory budget · seed 0x1d0c2026 · θ 0.99
- mode: TIERED — the D8 refusal is lifted; rows run against the tiered namespace
- zipf self-check: top-1% share measured 71.23% vs analytic 70.81% (θ=0.99, n=10485760, 2000000 draws)
- loader: 10485760 keys in 105.4s (99512 sets/s, 1 passes), DBSIZE == keys asserted
- hot-set instrument role: tiered leg at conns=8 pipeline=8 value_size=1024 (ADR-0071 D6 — both legs of the comparison must share this config; the comparison refuses on a mismatch)
- saturation (ycsb-a-zipfian): GENERATOR-LIMITED at 8 conns (+50% moved ops/s +94.8%) — absolutes understate the server; deltas remain valid at fixed generator config
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
| Cold reads: p99 < 1.5 ms on NVMe under loaded zipfian rows | < 1.5 ms, cold-read split histogram, worst loaded row | 3.46 | FAIL (DEV-TIER, non-binding) |
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
| ycsb-a-uniform | 1.883× worst of 4 namespace(s) · blob: n/a (no blob activity) |
| ycsb-b-zipfian | 1.876× worst of 4 namespace(s) · blob: n/a (no blob activity) |
| ycsb-b-uniform | 1.874× worst of 4 namespace(s) · blob: n/a (no blob activity) |
| ycsb-c-zipfian | 1.874× worst of 4 namespace(s) · blob: n/a (no blob activity) |
| ycsb-c-uniform | 1.874× worst of 4 namespace(s) · blob: n/a (no blob activity) |
| ycsb-d-latest | 1.875× worst of 4 namespace(s) · blob: n/a (no blob activity) |
| ycsb-e-zipfian | 1.875× worst of 4 namespace(s) · blob: n/a (no blob activity) |
| ycsb-f-zipfian | 1.832× worst of 4 namespace(s) · blob: n/a (no blob activity) |
| ycsb-f-uniform | 1.815× worst of 4 namespace(s) · blob: n/a (no blob activity) |

## loader fill

```
ops = 10485760
errors = 0
busy_retryable = 0
elapsed_s = 105.372
ops_per_sec = 99512
p50_us = 121
p99_us = 8703
p999_us = 188415
p9999_us = 1081343
max_us = 3616523
```

## ycsb-a-zipfian

```
workload = a (zipfian)
ops = 1038858
errors = 0
nils = 0
ops_per_sec = 51941
combined_client p50_us = 639 · p99_us = 11263 · p999_us = 98303 · max_us = 502616
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 71.18%
stream_checksum = 0x2be0cc2f486088cb
tiering_cold_p50_us (worst cell) = 251
tiering_cold_p99_us (worst cell) = 3455
tiering_cold_p999_us (worst cell) = 41983
cold_read_qd_p99 (worst cell) = 5
coalesce_ratio_milli (worst cell) = 0
server ram-hit fields: WITHDRAWN — tiering_ram_hit_p50_us, tiering_ram_hit_p99_us, tiering_ram_hit_p999_us named-absent, server says `unmeasured-iteration-clock`; the reactor's per-iteration clock cannot time a non-suspending command (ADR-0064 amendment 2026-08-08). The memory-hit half below is client-derived.
tiering_cold_resolves = 929509
memory-hit split (client-derived, ADR-0071 D2):
  cold_frac = 35.7792% (cold_reads 371695 · cold_resolves 929458 — re-resolve ratio 2.50×)
  mem_hit p50_us = 479 · p99_us = 783 · p999_us = 783
  separation: mem_hit p99.9 783 µs vs server cold p50 251 µs (client tail spread 304 µs — the truncation only separates the two modes while cold service exceeds it)
  NOT gate-eligible — separation check FAILED: derived memory-hit p99.9 783 µs >= server cold p50 251 µs — the two populations overlap and the quantile truncation cannot tell them apart (client tail spread 304 µs vs cold service 251 µs)
tripwire sqes/submit = 2.5
```

## ycsb-a-uniform

```
workload = a (uniform)
ops = 580100
errors = 0
nils = 0
ops_per_sec = 29003
combined_client p50_us = 1695 · p99_us = 20479 · p999_us = 47103 · max_us = 239304
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 0.00%
stream_checksum = 0x7046edebf976137f
tiering_cold_p50_us (worst cell) = 251
tiering_cold_p99_us (worst cell) = 1215
tiering_cold_p999_us (worst cell) = 25599
cold_read_qd_p99 (worst cell) = 5
coalesce_ratio_milli (worst cell) = 0
server ram-hit fields: WITHDRAWN — tiering_ram_hit_p50_us, tiering_ram_hit_p99_us, tiering_ram_hit_p999_us named-absent, server says `unmeasured-iteration-clock`; the reactor's per-iteration clock cannot time a non-suspending command (ADR-0064 amendment 2026-08-08). The memory-hit half below is client-derived.
tiering_cold_resolves = 3051213
memory-hit split (client-derived, ADR-0071 D2):
  cold_frac = 95.2255% (cold_reads 552403 · cold_resolves 1381476 — re-resolve ratio 2.50×)
  mem_hit p50_us = 495 · p99_us = 607 · p999_us = 607
  separation: mem_hit p99.9 607 µs vs server cold p50 251 µs (client tail spread 112 µs — the truncation only separates the two modes while cold service exceeds it)
  NOT gate-eligible — cold fraction 95.2% > 50% — the hot set is not memory-resident in this row, so the truncation describes no useful population
tripwire sqes/submit = 2.1
```

## ycsb-b-zipfian

```
workload = b (zipfian)
ops = 2511369
errors = 0
nils = 0
ops_per_sec = 125559
combined_client p50_us = 423 · p99_us = 1599 · p999_us = 18943 · max_us = 53521
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 71.17%
stream_checksum = 0x57b9a98a834cd884
tiering_cold_p50_us (worst cell) = 239
tiering_cold_p99_us (worst cell) = 943
tiering_cold_p999_us (worst cell) = 24063
cold_read_qd_p99 (worst cell) = 5
coalesce_ratio_milli (worst cell) = 0
server ram-hit fields: WITHDRAWN — tiering_ram_hit_p50_us, tiering_ram_hit_p99_us, tiering_ram_hit_p999_us named-absent, server says `unmeasured-iteration-clock`; the reactor's per-iteration clock cannot time a non-suspending command (ADR-0064 amendment 2026-08-08). The memory-hit half below is client-derived.
tiering_cold_resolves = 4199538
memory-hit split (client-derived, ADR-0071 D2):
  cold_frac = 22.4337% (cold_reads 563392 · cold_resolves 1148325 — re-resolve ratio 2.04×)
  mem_hit p50_us = 335 · p99_us = 687 · p999_us = 687
  separation: mem_hit p99.9 687 µs vs server cold p50 239 µs (client tail spread 352 µs — the truncation only separates the two modes while cold service exceeds it)
  NOT gate-eligible — separation check FAILED: derived memory-hit p99.9 687 µs >= server cold p50 239 µs — the two populations overlap and the quantile truncation cannot tell them apart (client tail spread 352 µs vs cold service 239 µs)
tripwire sqes/submit = 1.9
```

## ycsb-b-uniform

```
workload = b (uniform)
ops = 981465
errors = 0
nils = 0
ops_per_sec = 49069
combined_client p50_us = 1087 · p99_us = 3967 · p999_us = 43007 · max_us = 54648
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 0.00%
stream_checksum = 0xc2d3368d5cac7345
tiering_cold_p50_us (worst cell) = 247
tiering_cold_p99_us (worst cell) = 863
tiering_cold_p999_us (worst cell) = 23039
cold_read_qd_p99 (worst cell) = 5
coalesce_ratio_milli (worst cell) = 0
server ram-hit fields: WITHDRAWN — tiering_ram_hit_p50_us, tiering_ram_hit_p99_us, tiering_ram_hit_p999_us named-absent, server says `unmeasured-iteration-clock`; the reactor's per-iteration clock cannot time a non-suspending command (ADR-0064 amendment 2026-08-08). The memory-hit half below is client-derived.
tiering_cold_resolves = 5566057
memory-hit split (client-derived, ADR-0071 D2):
  cold_frac = 68.5816% (cold_reads 673104 · cold_resolves 1366519 — re-resolve ratio 2.03×)
  mem_hit p50_us = 559 · p99_us = 799 · p999_us = 799
  separation: mem_hit p99.9 799 µs vs server cold p50 247 µs (client tail spread 240 µs — the truncation only separates the two modes while cold service exceeds it)
  NOT gate-eligible — cold fraction 68.6% > 50% — the hot set is not memory-resident in this row, so the truncation describes no useful population
tripwire sqes/submit = 1.8
```

## ycsb-c-zipfian

```
workload = c (zipfian)
ops = 2755015
errors = 0
nils = 0
ops_per_sec = 137746
combined_client p50_us = 415 · p99_us = 1439 · p999_us = 1951 · max_us = 39767
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 71.18%
stream_checksum = 0xa813298720f7fa6f
tiering_cold_p50_us (worst cell) = 239
tiering_cold_p99_us (worst cell) = 799
tiering_cold_p999_us (worst cell) = 18431
cold_read_qd_p99 (worst cell) = 5
coalesce_ratio_milli (worst cell) = 0
server ram-hit fields: WITHDRAWN — tiering_ram_hit_p50_us, tiering_ram_hit_p99_us, tiering_ram_hit_p999_us named-absent, server says `unmeasured-iteration-clock`; the reactor's per-iteration clock cannot time a non-suspending command (ADR-0064 amendment 2026-08-08). The memory-hit half below is client-derived.
tiering_cold_resolves = 6781671
memory-hit split (client-derived, ADR-0071 D2):
  cold_frac = 22.0616% (cold_reads 607801 · cold_resolves 1215614 — re-resolve ratio 2.00×)
  mem_hit p50_us = 335 · p99_us = 671 · p999_us = 687
  separation: mem_hit p99.9 687 µs vs server cold p50 239 µs (client tail spread 352 µs — the truncation only separates the two modes while cold service exceeds it)
  NOT gate-eligible — separation check FAILED: derived memory-hit p99.9 687 µs >= server cold p50 239 µs — the two populations overlap and the quantile truncation cannot tell them apart (client tail spread 352 µs vs cold service 239 µs)
tripwire sqes/submit = 1.7
```

## ycsb-c-uniform

```
workload = c (uniform)
ops = 1012293
errors = 0
nils = 0
ops_per_sec = 50611
combined_client p50_us = 1183 · p99_us = 3263 · p999_us = 4351 · max_us = 6618
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 0.00%
stream_checksum = 0x7d6bac4c9b3a6a3c
tiering_cold_p50_us (worst cell) = 243
tiering_cold_p99_us (worst cell) = 783
tiering_cold_p999_us (worst cell) = 16895
cold_read_qd_p99 (worst cell) = 5
coalesce_ratio_milli (worst cell) = 0
server ram-hit fields: WITHDRAWN — tiering_ram_hit_p50_us, tiering_ram_hit_p99_us, tiering_ram_hit_p999_us named-absent, server says `unmeasured-iteration-clock`; the reactor's per-iteration clock cannot time a non-suspending command (ADR-0064 amendment 2026-08-08). The memory-hit half below is client-derived.
tiering_cold_resolves = 8178789
memory-hit split (client-derived, ADR-0071 D2):
  cold_frac = 69.0072% (cold_reads 698555 · cold_resolves 1397118 — re-resolve ratio 2.00×)
  mem_hit p50_us = 639 · p99_us = 879 · p999_us = 895
  separation: mem_hit p99.9 895 µs vs server cold p50 243 µs (client tail spread 256 µs — the truncation only separates the two modes while cold service exceeds it)
  NOT gate-eligible — cold fraction 69.0% > 50% — the hot set is not memory-resident in this row, so the truncation describes no useful population
tripwire sqes/submit = 1.6
```

## ycsb-d-latest

```
workload = d (latest)
ops = 1179295
errors = 0
nils = 264409
ops_per_sec = 58963
combined_client p50_us = 623 · p99_us = 2303 · p999_us = 126975 · max_us = 639447
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 71.18%
stream_checksum = 0x024c2311b74b5092
tiering_cold_p50_us (worst cell) = 239
tiering_cold_p99_us (worst cell) = 767
tiering_cold_p999_us (worst cell) = 18431
cold_read_qd_p99 (worst cell) = 5
coalesce_ratio_milli (worst cell) = 0
server ram-hit fields: WITHDRAWN — tiering_ram_hit_p50_us, tiering_ram_hit_p99_us, tiering_ram_hit_p999_us named-absent, server says `unmeasured-iteration-clock`; the reactor's per-iteration clock cannot time a non-suspending command (ADR-0064 amendment 2026-08-08). The memory-hit half below is client-derived.
tiering_cold_resolves = 9014097
memory-hit split (client-derived, ADR-0071 D2):
  cold_frac = 35.4155% (cold_reads 417653 · cold_resolves 835308 — re-resolve ratio 2.00×)
  mem_hit p50_us = 455 · p99_us = 751 · p999_us = 767
  separation: mem_hit p99.9 767 µs vs server cold p50 239 µs (client tail spread 312 µs — the truncation only separates the two modes while cold service exceeds it)
  NOT gate-eligible — separation check FAILED: derived memory-hit p99.9 767 µs >= server cold p50 239 µs — the two populations overlap and the quantile truncation cannot tell them apart (client tail spread 312 µs vs cold service 239 µs)
tripwire sqes/submit = 1.6
```

## ycsb-e-zipfian

```
workload = e (zipfian)
ops = 13025
errors = 0
nils = 0
ops_per_sec = 651
combined_client p50_us = 12287 · p99_us = 26111 · p999_us = 31743 · max_us = 45330
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 0.00%
stream_checksum = 0x85ae6957f3bc301d
tiering_cold_p50_us (worst cell) = 239
tiering_cold_p99_us (worst cell) = 1151
tiering_cold_p999_us (worst cell) = 24063
cold_read_qd_p99 (worst cell) = 5
coalesce_ratio_milli (worst cell) = 0
server ram-hit fields: WITHDRAWN — tiering_ram_hit_p50_us, tiering_ram_hit_p99_us, tiering_ram_hit_p999_us named-absent, server says `unmeasured-iteration-clock`; the reactor's per-iteration clock cannot time a non-suspending command (ADR-0064 amendment 2026-08-08). The memory-hit half below is client-derived.
tiering_cold_resolves = 9661536
memory-hit split (client-derived, ADR-0071 D2):
  cold_frac = 100.0000% (cold_reads 646420 · cold_resolves 647439 — re-resolve ratio 1.00×)
  mem_hit p50_us = 14 · p99_us = 14 · p999_us = 14
  separation: mem_hit p99.9 14 µs vs server cold p50 239 µs (client tail spread 0 µs — the truncation only separates the two modes while cold service exceeds it)
  NOT gate-eligible — 13025 ops < 100000 — too few for a p99.9 gate value
tripwire sqes/submit = 1.6
```

## ycsb-f-zipfian

```
workload = f (zipfian)
ops = 1717345
errors = 0
nils = 0
ops_per_sec = 85863
combined_client p50_us = 463 · p99_us = 2559 · p999_us = 54271 · max_us = 245947
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 71.19%
stream_checksum = 0x855371798bb12d26
tiering_cold_p50_us (worst cell) = 235
tiering_cold_p99_us (worst cell) = 1087
tiering_cold_p999_us (worst cell) = 24063
cold_read_qd_p99 (worst cell) = 5
coalesce_ratio_milli (worst cell) = 0
server ram-hit fields: WITHDRAWN — tiering_ram_hit_p50_us, tiering_ram_hit_p99_us, tiering_ram_hit_p999_us named-absent, server says `unmeasured-iteration-clock`; the reactor's per-iteration clock cannot time a non-suspending command (ADR-0064 amendment 2026-08-08). The memory-hit half below is client-derived.
tiering_cold_resolves = 10608922
memory-hit split (client-derived, ADR-0071 D2):
  cold_frac = 24.7876% (cold_reads 425689 · cold_resolves 947386 — re-resolve ratio 2.23×)
  mem_hit p50_us = 351 · p99_us = 751 · p999_us = 767
  separation: mem_hit p99.9 767 µs vs server cold p50 235 µs (client tail spread 416 µs — the truncation only separates the two modes while cold service exceeds it)
  NOT gate-eligible — separation check FAILED: derived memory-hit p99.9 767 µs >= server cold p50 235 µs — the two populations overlap and the quantile truncation cannot tell them apart (client tail spread 416 µs vs cold service 235 µs)
tripwire sqes/submit = 1.6
```

## ycsb-f-uniform

```
workload = f (uniform)
ops = 680512
errors = 0
nils = 0
ops_per_sec = 34020
combined_client p50_us = 1183 · p99_us = 13311 · p999_us = 100351 · max_us = 416023
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 0.00%
stream_checksum = 0xc0696e959494bad1
tiering_cold_p50_us (worst cell) = 235
tiering_cold_p99_us (worst cell) = 1007
tiering_cold_p999_us (worst cell) = 24063
cold_read_qd_p99 (worst cell) = 5
coalesce_ratio_milli (worst cell) = 0
server ram-hit fields: WITHDRAWN — tiering_ram_hit_p50_us, tiering_ram_hit_p99_us, tiering_ram_hit_p999_us named-absent, server says `unmeasured-iteration-clock`; the reactor's per-iteration clock cannot time a non-suspending command (ADR-0064 amendment 2026-08-08). The memory-hit half below is client-derived.
tiering_cold_resolves = 11608397
memory-hit split (client-derived, ADR-0071 D2):
  cold_frac = 68.4842% (cold_reads 466043 · cold_resolves 999475 — re-resolve ratio 2.14×)
  mem_hit p50_us = 639 · p99_us = 879 · p999_us = 879
  separation: mem_hit p99.9 879 µs vs server cold p50 235 µs (client tail spread 240 µs — the truncation only separates the two modes while cold service exceeds it)
  NOT gate-eligible — cold fraction 68.5% > 50% — the hot set is not memory-resident in this row, so the truncation describes no useful population
tripwire sqes/submit = 1.6
```
