# M4 gate-run report

date: 1786833810 (unix) · cells: 4 · conns: 8 · pipeline: 8 · duration: 60s · dataset: 10× budget · ycsb rows: 10 (M4-S22)
env-check: OK
tier: reference-box (binding)

notes:
- harness: inf-bench ycsb — memtier cannot drive a 10× RAM zipfian tier workload (§6)
- E-adaptation: cursor-scan slices (SCAN <cursor> COUNT 1..=100, pipeline 1) — the keyspace is unordered, no ordered range-scan exists (documented deviation)
- value shape: single constant-byte value of --value-size (default 1 KiB ≈ YCSB's 10×100 B fields); D's `latest` uses a per-connection frontier estimate
- dataset: 20971520 keys × 1024 B = 20480 MiB = 10× the 2048 MiB memory budget · seed 0x1d0c2026 · θ 0.99
- mode: TIERED — the D8 refusal is lifted; rows run against the tiered namespace
- zipf self-check: top-1% share measured 72.26% vs analytic 71.87% (θ=0.99, n=20971520, 2000000 draws)
- loader: 20971520 keys in 297.2s (70574 sets/s, 1 passes), DBSIZE == keys asserted
- hot-set instrument role: tiered leg at conns=8 pipeline=8 value_size=1024 (ADR-0071 D6 — both legs of the comparison must share this config; the comparison refuses on a mismatch)
- saturation (ycsb-a-zipfian): GENERATOR-LIMITED at 8 conns (+50% moved ops/s +44.6%) — absolutes understate the server; deltas remain valid at fixed generator config
- hot-set gate rows (ycsb:hot_set_*): PENDING the reference leg — run `inf-bench ycsb --dataset-multiple 1` in the same campaign at the same --conns/--pipeline/--value-size and re-run this leg with `--hot-set-reference <that run's dir or mem-hit.tsv>`; this run publishes its own memory-hit split in `mem-hit.tsv` for that comparison

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
| Write amplification, worst tiered namespace | < 3 x user bytes (wal + flush) | 1.89 | PASS |
| Memory-only rows append zero log records (M2 posture carried) | <= 0 records | — | PENDING (tooling) |
| Mixed-node attribution divergence (M4-S20) | <= 10 pct, worst continuous sample | — | PENDING (tooling) |
| Cache-namespace p99 isolation under the mixed node (M4-S20) | <= 10 pct vs same-campaign solo baseline | — | PENDING (tooling) |
| Hot set at memory speed: memory-hit p50 delta | <= 10 pct vs RAM-resident reference leg, memory-hit split | — | PENDING (tooling) |
| Hot set at memory speed: memory-hit p99 delta | <= 10 pct vs RAM-resident reference leg, memory-hit split | — | PENDING (tooling) |
| Hot set at memory speed: memory-hit p99.9 delta | <= 10 pct vs RAM-resident reference leg, memory-hit split (LogHistogram ~3% buckets) | — | PENDING (tooling) |
| Cold reads: p99 < 1.5 ms on NVMe under loaded zipfian rows | < 1.5 ms, cold-read split histogram, worst loaded row | 3.65 | FAIL |
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
| ycsb-a-zipfian | 1.889× worst of 4 namespace(s) · blob: n/a (no blob activity) |
| ycsb-a-uniform | 1.868× worst of 4 namespace(s) · blob: n/a (no blob activity) |
| ycsb-b-zipfian | 1.860× worst of 4 namespace(s) · blob: n/a (no blob activity) |
| ycsb-b-uniform | 1.858× worst of 4 namespace(s) · blob: n/a (no blob activity) |
| ycsb-c-zipfian | 1.858× worst of 4 namespace(s) · blob: n/a (no blob activity) |
| ycsb-c-uniform | 1.858× worst of 4 namespace(s) · blob: n/a (no blob activity) |
| ycsb-d-latest | 1.860× worst of 4 namespace(s) · blob: n/a (no blob activity) |
| ycsb-e-zipfian | 1.860× worst of 4 namespace(s) · blob: n/a (no blob activity) |
| ycsb-f-zipfian | 1.822× worst of 4 namespace(s) · blob: n/a (no blob activity) |
| ycsb-f-uniform | 1.803× worst of 4 namespace(s) · blob: n/a (no blob activity) |

## loader fill

```
ops = 20971520
errors = 0
busy_retryable = 0
elapsed_s = 297.155
ops_per_sec = 70574
p50_us = 147
p99_us = 21503
p999_us = 311295
p9999_us = 1507327
max_us = 3565329
```

## ycsb-a-zipfian

```
workload = a (zipfian)
ops = 2832505
errors = 0
nils = 0
ops_per_sec = 47172
combined_client p50_us = 783 · p99_us = 11007 · p999_us = 100351 · max_us = 394706
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 72.20%
stream_checksum = 0x97b2d3c9aa453ba4
tiering_cold_p50_us (worst cell) = 295
tiering_cold_p99_us (worst cell) = 3647
tiering_cold_p999_us (worst cell) = 39935
cold_read_qd_p99 (worst cell) = 5
coalesce_ratio_milli (worst cell) = 0
server ram-hit fields: WITHDRAWN — tiering_ram_hit_p50_us, tiering_ram_hit_p99_us, tiering_ram_hit_p999_us named-absent, server says `unmeasured-iteration-clock`; the reactor's per-iteration clock cannot time a non-suspending command (ADR-0064 amendment 2026-08-08). The memory-hit half below is client-derived.
tiering_cold_resolves = 2288110
memory-hit split (client-derived, ADR-0071 D2):
  cold_frac = 32.3055% (cold_reads 915054 · cold_resolves 2287990 — re-resolve ratio 2.50×)
  mem_hit p50_us = 607 · p99_us = 991 · p999_us = 1007
  separation: mem_hit p99.9 1007 µs vs server cold p50 295 µs (client tail spread 400 µs — the truncation only separates the two modes while cold service exceeds it)
  NOT gate-eligible — separation check FAILED: derived memory-hit p99.9 1007 µs >= server cold p50 295 µs — the two populations overlap and the quantile truncation cannot tell them apart (client tail spread 400 µs vs cold service 295 µs)
tripwire sqes/submit = 2.5
```

## ycsb-a-uniform

```
workload = a (uniform)
ops = 1364888
errors = 0
nils = 0
ops_per_sec = 22747
combined_client p50_us = 1887 · p99_us = 19967 · p999_us = 147455 · max_us = 647871
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 0.00%
stream_checksum = 0x2ec3cea764c81ce5
tiering_cold_p50_us (worst cell) = 335
tiering_cold_p99_us (worst cell) = 3263
tiering_cold_p999_us (worst cell) = 38911
cold_read_qd_p99 (worst cell) = 5
coalesce_ratio_milli (worst cell) = 0
server ram-hit fields: WITHDRAWN — tiering_ram_hit_p50_us, tiering_ram_hit_p99_us, tiering_ram_hit_p999_us named-absent, server says `unmeasured-iteration-clock`; the reactor's per-iteration clock cannot time a non-suspending command (ADR-0064 amendment 2026-08-08). The memory-hit half below is client-derived.
tiering_cold_resolves = 6673580
memory-hit split (client-derived, ADR-0071 D2):
  cold_frac = 90.2467% (cold_reads 1231767 · cold_resolves 3079823 — re-resolve ratio 2.50×)
  mem_hit p50_us = 815 · p99_us = 991 · p999_us = 991
  separation: mem_hit p99.9 991 µs vs server cold p50 335 µs (client tail spread 176 µs — the truncation only separates the two modes while cold service exceeds it)
  NOT gate-eligible — cold fraction 90.2% > 50% — the hot set is not memory-resident in this row, so the truncation describes no useful population
tripwire sqes/submit = 2.1
```

## ycsb-b-zipfian

```
workload = b (zipfian)
ops = 6691870
errors = 0
nils = 0
ops_per_sec = 111530
combined_client p50_us = 479 · p99_us = 1823 · p999_us = 18943 · max_us = 276698
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 72.22%
stream_checksum = 0x2dd45d38af28cdbb
tiering_cold_p50_us (worst cell) = 319
tiering_cold_p99_us (worst cell) = 1695
tiering_cold_p999_us (worst cell) = 26623
cold_read_qd_p99 (worst cell) = 5
coalesce_ratio_milli (worst cell) = 0
server ram-hit fields: WITHDRAWN — tiering_ram_hit_p50_us, tiering_ram_hit_p99_us, tiering_ram_hit_p999_us named-absent, server says `unmeasured-iteration-clock`; the reactor's per-iteration clock cannot time a non-suspending command (ADR-0064 amendment 2026-08-08). The memory-hit half below is client-derived.
tiering_cold_resolves = 9422940
memory-hit split (client-derived, ADR-0071 D2):
  cold_frac = 20.1603% (cold_reads 1349102 · cold_resolves 2749360 — re-resolve ratio 2.04×)
  mem_hit p50_us = 391 · p99_us = 799 · p999_us = 815
  separation: mem_hit p99.9 815 µs vs server cold p50 319 µs (client tail spread 424 µs — the truncation only separates the two modes while cold service exceeds it)
  NOT gate-eligible — separation check FAILED: derived memory-hit p99.9 815 µs >= server cold p50 319 µs — the two populations overlap and the quantile truncation cannot tell them apart (client tail spread 424 µs vs cold service 319 µs)
tripwire sqes/submit = 1.9
```

## ycsb-b-uniform

```
workload = b (uniform)
ops = 2440148
errors = 0
nils = 0
ops_per_sec = 40668
combined_client p50_us = 1375 · p99_us = 4479 · p999_us = 26623 · max_us = 230614
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 0.00%
stream_checksum = 0x4f23a9d282a3f31e
tiering_cold_p50_us (worst cell) = 327
tiering_cold_p99_us (worst cell) = 1215
tiering_cold_p999_us (worst cell) = 25087
cold_read_qd_p99 (worst cell) = 5
coalesce_ratio_milli (worst cell) = 0
server ram-hit fields: WITHDRAWN — tiering_ram_hit_p50_us, tiering_ram_hit_p99_us, tiering_ram_hit_p999_us named-absent, server says `unmeasured-iteration-clock`; the reactor's per-iteration clock cannot time a non-suspending command (ADR-0064 amendment 2026-08-08). The memory-hit half below is client-derived.
tiering_cold_resolves = 12772390
memory-hit split (client-derived, ADR-0071 D2):
  cold_frac = 67.5616% (cold_reads 1648603 · cold_resolves 3349450 — re-resolve ratio 2.03×)
  mem_hit p50_us = 751 · p99_us = 1055 · p999_us = 1055
  separation: mem_hit p99.9 1055 µs vs server cold p50 327 µs (client tail spread 304 µs — the truncation only separates the two modes while cold service exceeds it)
  NOT gate-eligible — cold fraction 67.6% > 50% — the hot set is not memory-resident in this row, so the truncation describes no useful population
tripwire sqes/submit = 1.7
```

## ycsb-c-zipfian

```
workload = c (zipfian)
ops = 7009934
errors = 0
nils = 0
ops_per_sec = 116829
combined_client p50_us = 495 · p99_us = 1663 · p999_us = 2367 · max_us = 7712
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 72.23%
stream_checksum = 0x9e449289010ebabf
tiering_cold_p50_us (worst cell) = 311
tiering_cold_p99_us (worst cell) = 1087
tiering_cold_p999_us (worst cell) = 24063
cold_read_qd_p99 (worst cell) = 5
coalesce_ratio_milli (worst cell) = 0
server ram-hit fields: WITHDRAWN — tiering_ram_hit_p50_us, tiering_ram_hit_p99_us, tiering_ram_hit_p999_us named-absent, server says `unmeasured-iteration-clock`; the reactor's per-iteration clock cannot time a non-suspending command (ADR-0064 amendment 2026-08-08). The memory-hit half below is client-derived.
tiering_cold_resolves = 15506923
memory-hit split (client-derived, ADR-0071 D2):
  cold_frac = 19.5045% (cold_reads 1367254 · cold_resolves 2734533 — re-resolve ratio 2.00×)
  mem_hit p50_us = 415 · p99_us = 831 · p999_us = 847
  separation: mem_hit p99.9 847 µs vs server cold p50 311 µs (client tail spread 432 µs — the truncation only separates the two modes while cold service exceeds it)
  NOT gate-eligible — separation check FAILED: derived memory-hit p99.9 847 µs >= server cold p50 311 µs — the two populations overlap and the quantile truncation cannot tell them apart (client tail spread 432 µs vs cold service 311 µs)
tripwire sqes/submit = 1.7
```

## ycsb-c-uniform

```
workload = c (uniform)
ops = 2597193
errors = 0
nils = 0
ops_per_sec = 43285
combined_client p50_us = 1375 · p99_us = 3903 · p999_us = 5119 · max_us = 10719
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 0.00%
stream_checksum = 0xc950110acd51ec75
tiering_cold_p50_us (worst cell) = 319
tiering_cold_p99_us (worst cell) = 1007
tiering_cold_p999_us (worst cell) = 23039
cold_read_qd_p99 (worst cell) = 5
coalesce_ratio_milli (worst cell) = 0
server ram-hit fields: WITHDRAWN — tiering_ram_hit_p50_us, tiering_ram_hit_p99_us, tiering_ram_hit_p999_us named-absent, server says `unmeasured-iteration-clock`; the reactor's per-iteration clock cannot time a non-suspending command (ADR-0064 amendment 2026-08-08). The memory-hit half below is client-derived.
tiering_cold_resolves = 19007716
memory-hit split (client-derived, ADR-0071 D2):
  cold_frac = 67.3952% (cold_reads 1750384 · cold_resolves 3500793 — re-resolve ratio 2.00×)
  mem_hit p50_us = 767 · p99_us = 1055 · p999_us = 1055
  separation: mem_hit p99.9 1055 µs vs server cold p50 319 µs (client tail spread 288 µs — the truncation only separates the two modes while cold service exceeds it)
  NOT gate-eligible — cold fraction 67.4% > 50% — the hot set is not memory-resident in this row, so the truncation describes no useful population
tripwire sqes/submit = 1.6
```

## ycsb-d-latest

```
workload = d (latest)
ops = 6509115
errors = 0
nils = 1693934
ops_per_sec = 108484
combined_client p50_us = 503 · p99_us = 1823 · p999_us = 23039 · max_us = 34568
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 72.24%
stream_checksum = 0x0ff3e3453e476f2e
tiering_cold_p50_us (worst cell) = 311
tiering_cold_p99_us (worst cell) = 975
tiering_cold_p999_us (worst cell) = 22015
cold_read_qd_p99 (worst cell) = 5
coalesce_ratio_milli (worst cell) = 0
server ram-hit fields: WITHDRAWN — tiering_ram_hit_p50_us, tiering_ram_hit_p99_us, tiering_ram_hit_p999_us named-absent, server says `unmeasured-iteration-clock`; the reactor's per-iteration clock cannot time a non-suspending command (ADR-0064 amendment 2026-08-08). The memory-hit half below is client-derived.
tiering_cold_resolves = 21968311
memory-hit split (client-derived, ADR-0071 D2):
  cold_frac = 22.7417% (cold_reads 1480284 · cold_resolves 2960595 — re-resolve ratio 2.00×)
  mem_hit p50_us = 407 · p99_us = 783 · p999_us = 799
  separation: mem_hit p99.9 799 µs vs server cold p50 311 µs (client tail spread 392 µs — the truncation only separates the two modes while cold service exceeds it)
  NOT gate-eligible — separation check FAILED: derived memory-hit p99.9 799 µs >= server cold p50 311 µs — the two populations overlap and the quantile truncation cannot tell them apart (client tail spread 392 µs vs cold service 311 µs)
tripwire sqes/submit = 1.6
```

## ycsb-e-zipfian

```
workload = e (zipfian)
ops = 33343
errors = 0
nils = 0
ops_per_sec = 556
combined_client p50_us = 14335 · p99_us = 30207 · p999_us = 36863 · max_us = 510846
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 0.00%
stream_checksum = 0x1cc7eb30115ffb55
tiering_cold_p50_us (worst cell) = 311
tiering_cold_p99_us (worst cell) = 1247
tiering_cold_p999_us (worst cell) = 25599
cold_read_qd_p99 (worst cell) = 5
coalesce_ratio_milli (worst cell) = 1
server ram-hit fields: WITHDRAWN — tiering_ram_hit_p50_us, tiering_ram_hit_p99_us, tiering_ram_hit_p999_us named-absent, server says `unmeasured-iteration-clock`; the reactor's per-iteration clock cannot time a non-suspending command (ADR-0064 amendment 2026-08-08). The memory-hit half below is client-derived.
tiering_cold_resolves = 23566946
memory-hit split (client-derived, ADR-0071 D2):
  cold_frac = 100.0000% (cold_reads 1594313 · cold_resolves 1598635 — re-resolve ratio 1.00×)
  mem_hit p50_us = 15 · p99_us = 15 · p999_us = 15
  separation: mem_hit p99.9 15 µs vs server cold p50 311 µs (client tail spread 0 µs — the truncation only separates the two modes while cold service exceeds it)
  NOT gate-eligible — 33343 ops < 100000 — too few for a p99.9 gate value
tripwire sqes/submit = 1.5
```

## ycsb-f-zipfian

```
workload = f (zipfian)
ops = 2646316
errors = 0
nils = 0
ops_per_sec = 44105
combined_client p50_us = 479 · p99_us = 27135 · p999_us = 163839 · max_us = 2459191
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 72.20%
stream_checksum = 0x2461d86105281791
tiering_cold_p50_us (worst cell) = 303
tiering_cold_p99_us (worst cell) = 1279
tiering_cold_p999_us (worst cell) = 26623
cold_read_qd_p99 (worst cell) = 5
coalesce_ratio_milli (worst cell) = 0
server ram-hit fields: WITHDRAWN — tiering_ram_hit_p50_us, tiering_ram_hit_p99_us, tiering_ram_hit_p999_us named-absent, server says `unmeasured-iteration-clock`; the reactor's per-iteration clock cannot time a non-suspending command (ADR-0064 amendment 2026-08-08). The memory-hit half below is client-derived.
tiering_cold_resolves = 24536867
memory-hit split (client-derived, ADR-0071 D2):
  cold_frac = 17.2024% (cold_reads 455231 · cold_resolves 969921 — re-resolve ratio 2.13×)
  mem_hit p50_us = 399 · p99_us = 879 · p999_us = 895
  separation: mem_hit p99.9 895 µs vs server cold p50 303 µs (client tail spread 496 µs — the truncation only separates the two modes while cold service exceeds it)
  NOT gate-eligible — separation check FAILED: derived memory-hit p99.9 895 µs >= server cold p50 303 µs — the two populations overlap and the quantile truncation cannot tell them apart (client tail spread 496 µs vs cold service 303 µs)
tripwire sqes/submit = 1.6
```

## ycsb-f-uniform

```
workload = f (uniform)
ops = 1674200
errors = 0
nils = 0
ops_per_sec = 27902
combined_client p50_us = 1663 · p99_us = 8703 · p999_us = 94207 · max_us = 480902
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 0.00%
stream_checksum = 0x720aa54f45427763
tiering_cold_p50_us (worst cell) = 303
tiering_cold_p99_us (worst cell) = 1215
tiering_cold_p999_us (worst cell) = 26623
cold_read_qd_p99 (worst cell) = 5
coalesce_ratio_milli (worst cell) = 0
server ram-hit fields: WITHDRAWN — tiering_ram_hit_p50_us, tiering_ram_hit_p99_us, tiering_ram_hit_p999_us named-absent, server says `unmeasured-iteration-clock`; the reactor's per-iteration clock cannot time a non-suspending command (ADR-0064 amendment 2026-08-08). The memory-hit half below is client-derived.
tiering_cold_resolves = 26939613
memory-hit split (client-derived, ADR-0071 D2):
  cold_frac = 66.7758% (cold_reads 1117961 · cold_resolves 2402746 — re-resolve ratio 2.15×)
  mem_hit p50_us = 911 · p99_us = 1279 · p999_us = 1279
  separation: mem_hit p99.9 1279 µs vs server cold p50 303 µs (client tail spread 368 µs — the truncation only separates the two modes while cold service exceeds it)
  NOT gate-eligible — cold fraction 66.8% > 50% — the hot set is not memory-resident in this row, so the truncation describes no useful population
tripwire sqes/submit = 1.5
```
