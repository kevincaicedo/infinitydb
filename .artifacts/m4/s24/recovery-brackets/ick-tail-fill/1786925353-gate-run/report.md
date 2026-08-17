# M4 gate-run report

date: 1786925353 (unix) · cells: 4 · conns: 8 · pipeline: 8 · duration: 20s · dataset: 10× budget · ycsb rows: 10 (M4-S22)
env-check: OK
tier: dev (non-binding)

notes:
- harness: inf-bench ycsb — memtier cannot drive a 10× RAM zipfian tier workload (§6)
- E-adaptation: cursor-scan slices (SCAN <cursor> COUNT 1..=100, pipeline 1) — the keyspace is unordered, no ordered range-scan exists (documented deviation)
- value shape: single constant-byte value of --value-size (default 1 KiB ≈ YCSB's 10×100 B fields); D's `latest` uses a per-connection frontier estimate
- dataset: 10485760 keys × 1024 B = 10240 MiB = 10× the 1024 MiB memory budget · seed 0x1d0c2026 · θ 0.99
- mode: TIERED — the D8 refusal is lifted; rows run against the tiered namespace
- zipf self-check: top-1% share measured 71.23% vs analytic 70.81% (θ=0.99, n=10485760, 2000000 draws)
- loader: 10485760 keys in 124.7s (84070 sets/s, 1 passes), DBSIZE == keys asserted
- hot-set instrument role: tiered leg at conns=8 pipeline=8 value_size=1024 (ADR-0071 D6 — both legs of the comparison must share this config; the comparison refuses on a mismatch)
- saturation (ycsb-a-zipfian): GENERATOR-LIMITED at 8 conns (+50% moved ops/s +17.8%) — absolutes understate the server; deltas remain valid at fixed generator config
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
| Cold reads: p99 < 1.5 ms on NVMe under loaded zipfian rows | < 1.5 ms, cold-read split histogram, worst loaded row | 1.34 | PASS (DEV-TIER, non-binding) |
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
| ycsb-a-zipfian | 1.898× worst of 4 namespace(s) · blob: n/a (no blob activity) |
| ycsb-a-uniform | 1.884× worst of 4 namespace(s) · blob: n/a (no blob activity) |
| ycsb-b-zipfian | 1.877× worst of 4 namespace(s) · blob: n/a (no blob activity) |
| ycsb-b-uniform | 1.876× worst of 4 namespace(s) · blob: n/a (no blob activity) |
| ycsb-c-zipfian | 1.876× worst of 4 namespace(s) · blob: n/a (no blob activity) |
| ycsb-c-uniform | 1.876× worst of 4 namespace(s) · blob: n/a (no blob activity) |
| ycsb-d-latest | 1.877× worst of 4 namespace(s) · blob: n/a (no blob activity) |
| ycsb-e-zipfian | 1.877× worst of 4 namespace(s) · blob: n/a (no blob activity) |
| ycsb-f-zipfian | 1.832× worst of 4 namespace(s) · blob: n/a (no blob activity) |
| ycsb-f-uniform | 1.822× worst of 4 namespace(s) · blob: n/a (no blob activity) |

## loader fill

```
ops = 10485760
errors = 0
busy_retryable = 0
elapsed_s = 124.726
ops_per_sec = 84070
p50_us = 143
p99_us = 9215
p999_us = 221183
p9999_us = 1507327
max_us = 3818476
```

## ycsb-a-zipfian

```
workload = a (zipfian)
ops = 1237422
errors = 0
nils = 0
ops_per_sec = 61868
combined_client p50_us = 671 · p99_us = 5375 · p999_us = 64511 · max_us = 234623
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 71.17%
stream_checksum = 0x6768211016979ace
tiering_cold_p50_us (worst cell) = 271
tiering_cold_p99_us (worst cell) = 1343
tiering_cold_p999_us (worst cell) = 25087
cold_read_qd_p99 (worst cell) = 5
coalesce_ratio_milli (worst cell) = 0
server ram-hit fields: WITHDRAWN — tiering_ram_hit_p50_us, tiering_ram_hit_p99_us, tiering_ram_hit_p999_us named-absent, server says `unmeasured-iteration-clock`; the reactor's per-iteration clock cannot time a non-suspending command (ADR-0064 amendment 2026-08-08). The memory-hit half below is client-derived.
tiering_cold_resolves = 1086682
memory-hit split (client-derived, ADR-0071 D2):
  cold_frac = 35.1136% (cold_reads 434503 · cold_resolves 1086622 — re-resolve ratio 2.50×)
  mem_hit p50_us = 495 · p99_us = 815 · p999_us = 831
  separation: mem_hit p99.9 831 µs vs server cold p50 271 µs (client tail spread 336 µs — the truncation only separates the two modes while cold service exceeds it)
  NOT gate-eligible — separation check FAILED: derived memory-hit p99.9 831 µs >= server cold p50 271 µs — the two populations overlap and the quantile truncation cannot tell them apart (client tail spread 336 µs vs cold service 271 µs)
tripwire sqes/submit = 2.5
```

## ycsb-a-uniform

```
workload = a (uniform)
ops = 335152
errors = 0
nils = 0
ops_per_sec = 16755
combined_client p50_us = 1567 · p99_us = 60415 · p999_us = 425983 · max_us = 975095
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 0.00%
stream_checksum = 0xee6051c3342e4ab3
tiering_cold_p50_us (worst cell) = 271
tiering_cold_p99_us (worst cell) = 1119
tiering_cold_p999_us (worst cell) = 58367
cold_read_qd_p99 (worst cell) = 5
coalesce_ratio_milli (worst cell) = 0
server ram-hit fields: WITHDRAWN — tiering_ram_hit_p50_us, tiering_ram_hit_p99_us, tiering_ram_hit_p999_us named-absent, server says `unmeasured-iteration-clock`; the reactor's per-iteration clock cannot time a non-suspending command (ADR-0064 amendment 2026-08-08). The memory-hit half below is client-derived.
tiering_cold_resolves = 2421787
memory-hit split (client-derived, ADR-0071 D2):
  cold_frac = 94.9029% (cold_reads 318069 · cold_resolves 795516 — re-resolve ratio 2.50×)
  mem_hit p50_us = 447 · p99_us = 543 · p999_us = 543
  separation: mem_hit p99.9 543 µs vs server cold p50 271 µs (client tail spread 96 µs — the truncation only separates the two modes while cold service exceeds it)
  NOT gate-eligible — cold fraction 94.9% > 50% — the hot set is not memory-resident in this row, so the truncation describes no useful population
tripwire sqes/submit = 2.2
```

## ycsb-b-zipfian

```
workload = b (zipfian)
ops = 2584581
errors = 0
nils = 0
ops_per_sec = 129223
combined_client p50_us = 439 · p99_us = 1631 · p999_us = 3071 · max_us = 6604
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 71.18%
stream_checksum = 0x4ded21cbed2cd13f
tiering_cold_p50_us (worst cell) = 263
tiering_cold_p99_us (worst cell) = 911
tiering_cold_p999_us (worst cell) = 28671
cold_read_qd_p99 (worst cell) = 5
coalesce_ratio_milli (worst cell) = 0
server ram-hit fields: WITHDRAWN — tiering_ram_hit_p50_us, tiering_ram_hit_p99_us, tiering_ram_hit_p999_us named-absent, server says `unmeasured-iteration-clock`; the reactor's per-iteration clock cannot time a non-suspending command (ADR-0064 amendment 2026-08-08). The memory-hit half below is client-derived.
tiering_cold_resolves = 3551554
memory-hit split (client-derived, ADR-0071 D2):
  cold_frac = 21.4747% (cold_reads 555032 · cold_resolves 1129767 — re-resolve ratio 2.04×)
  mem_hit p50_us = 351 · p99_us = 719 · p999_us = 735
  separation: mem_hit p99.9 735 µs vs server cold p50 263 µs (client tail spread 384 µs — the truncation only separates the two modes while cold service exceeds it)
  NOT gate-eligible — separation check FAILED: derived memory-hit p99.9 735 µs >= server cold p50 263 µs — the two populations overlap and the quantile truncation cannot tell them apart (client tail spread 384 µs vs cold service 263 µs)
tripwire sqes/submit = 2.0
```

## ycsb-b-uniform

```
workload = b (uniform)
ops = 788870
errors = 0
nils = 0
ops_per_sec = 39439
combined_client p50_us = 1311 · p99_us = 4991 · p999_us = 29695 · max_us = 273469
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 0.00%
stream_checksum = 0x0b034702452e8f01
tiering_cold_p50_us (worst cell) = 271
tiering_cold_p99_us (worst cell) = 927
tiering_cold_p999_us (worst cell) = 24063
cold_read_qd_p99 (worst cell) = 5
coalesce_ratio_milli (worst cell) = 0
server ram-hit fields: WITHDRAWN — tiering_ram_hit_p50_us, tiering_ram_hit_p99_us, tiering_ram_hit_p999_us named-absent, server says `unmeasured-iteration-clock`; the reactor's per-iteration clock cannot time a non-suspending command (ADR-0064 amendment 2026-08-08). The memory-hit half below is client-derived.
tiering_cold_resolves = 4794177
memory-hit split (client-derived, ADR-0071 D2):
  cold_frac = 77.3394% (cold_reads 610107 · cold_resolves 1242623 — re-resolve ratio 2.04×)
  mem_hit p50_us = 607 · p99_us = 815 · p999_us = 831
  separation: mem_hit p99.9 831 µs vs server cold p50 271 µs (client tail spread 224 µs — the truncation only separates the two modes while cold service exceeds it)
  NOT gate-eligible — cold fraction 77.3% > 50% — the hot set is not memory-resident in this row, so the truncation describes no useful population
tripwire sqes/submit = 1.8
```

## ycsb-c-zipfian

```
workload = c (zipfian)
ops = 2537482
errors = 0
nils = 0
ops_per_sec = 126869
combined_client p50_us = 455 · p99_us = 1535 · p999_us = 2047 · max_us = 4450
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 71.18%
stream_checksum = 0x5f3d7038cd7af326
tiering_cold_p50_us (worst cell) = 263
tiering_cold_p99_us (worst cell) = 863
tiering_cold_p999_us (worst cell) = 18943
cold_read_qd_p99 (worst cell) = 5
coalesce_ratio_milli (worst cell) = 0
server ram-hit fields: WITHDRAWN — tiering_ram_hit_p50_us, tiering_ram_hit_p99_us, tiering_ram_hit_p999_us named-absent, server says `unmeasured-iteration-clock`; the reactor's per-iteration clock cannot time a non-suspending command (ADR-0064 amendment 2026-08-08). The memory-hit half below is client-derived.
tiering_cold_resolves = 5835252
memory-hit split (client-derived, ADR-0071 D2):
  cold_frac = 20.5138% (cold_reads 520534 · cold_resolves 1041075 — re-resolve ratio 2.00×)
  mem_hit p50_us = 375 · p99_us = 751 · p999_us = 751
  separation: mem_hit p99.9 751 µs vs server cold p50 263 µs (client tail spread 376 µs — the truncation only separates the two modes while cold service exceeds it)
  NOT gate-eligible — separation check FAILED: derived memory-hit p99.9 751 µs >= server cold p50 263 µs — the two populations overlap and the quantile truncation cannot tell them apart (client tail spread 376 µs vs cold service 263 µs)
tripwire sqes/submit = 1.8
```

## ycsb-c-uniform

```
workload = c (uniform)
ops = 817824
errors = 0
nils = 0
ops_per_sec = 40886
combined_client p50_us = 1407 · p99_us = 4095 · p999_us = 5247 · max_us = 7783
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 0.00%
stream_checksum = 0x6200c74b94960b94
tiering_cold_p50_us (worst cell) = 263
tiering_cold_p99_us (worst cell) = 847
tiering_cold_p999_us (worst cell) = 16127
cold_read_qd_p99 (worst cell) = 5
coalesce_ratio_milli (worst cell) = 0
server ram-hit fields: WITHDRAWN — tiering_ram_hit_p50_us, tiering_ram_hit_p99_us, tiering_ram_hit_p999_us named-absent, server says `unmeasured-iteration-clock`; the reactor's per-iteration clock cannot time a non-suspending command (ADR-0064 amendment 2026-08-08). The memory-hit half below is client-derived.
tiering_cold_resolves = 7065715
memory-hit split (client-derived, ADR-0071 D2):
  cold_frac = 75.2277% (cold_reads 615230 · cold_resolves 1230463 — re-resolve ratio 2.00×)
  mem_hit p50_us = 703 · p99_us = 943 · p999_us = 943
  separation: mem_hit p99.9 943 µs vs server cold p50 263 µs (client tail spread 240 µs — the truncation only separates the two modes while cold service exceeds it)
  NOT gate-eligible — cold fraction 75.2% > 50% — the hot set is not memory-resident in this row, so the truncation describes no useful population
tripwire sqes/submit = 1.7
```

## ycsb-d-latest

```
workload = d (latest)
ops = 2022308
errors = 0
nils = 502481
ops_per_sec = 101111
combined_client p50_us = 559 · p99_us = 1887 · p999_us = 5503 · max_us = 28286
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 71.23%
stream_checksum = 0xe981338a34fd84de
tiering_cold_p50_us (worst cell) = 255
tiering_cold_p99_us (worst cell) = 815
tiering_cold_p999_us (worst cell) = 11263
cold_read_qd_p99 (worst cell) = 5
coalesce_ratio_milli (worst cell) = 0
server ram-hit fields: WITHDRAWN — tiering_ram_hit_p50_us, tiering_ram_hit_p99_us, tiering_ram_hit_p999_us named-absent, server says `unmeasured-iteration-clock`; the reactor's per-iteration clock cannot time a non-suspending command (ADR-0064 amendment 2026-08-08). The memory-hit half below is client-derived.
tiering_cold_resolves = 8338324
memory-hit split (client-derived, ADR-0071 D2):
  cold_frac = 31.4641% (cold_reads 636302 · cold_resolves 1272609 — re-resolve ratio 2.00×)
  mem_hit p50_us = 423 · p99_us = 719 · p999_us = 735
  separation: mem_hit p99.9 735 µs vs server cold p50 255 µs (client tail spread 312 µs — the truncation only separates the two modes while cold service exceeds it)
  NOT gate-eligible — separation check FAILED: derived memory-hit p99.9 735 µs >= server cold p50 255 µs — the two populations overlap and the quantile truncation cannot tell them apart (client tail spread 312 µs vs cold service 255 µs)
tripwire sqes/submit = 1.6
```

## ycsb-e-zipfian

```
workload = e (zipfian)
ops = 11980
errors = 0
nils = 0
ops_per_sec = 599
combined_client p50_us = 12799 · p99_us = 27647 · p999_us = 278527 · max_us = 940617
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 0.00%
stream_checksum = 0x0bd8fe5c16bb1a88
tiering_cold_p50_us (worst cell) = 255
tiering_cold_p99_us (worst cell) = 991
tiering_cold_p999_us (worst cell) = 23551
cold_read_qd_p99 (worst cell) = 5
coalesce_ratio_milli (worst cell) = 0
server ram-hit fields: WITHDRAWN — tiering_ram_hit_p50_us, tiering_ram_hit_p99_us, tiering_ram_hit_p999_us named-absent, server says `unmeasured-iteration-clock`; the reactor's per-iteration clock cannot time a non-suspending command (ADR-0064 amendment 2026-08-08). The memory-hit half below is client-derived.
tiering_cold_resolves = 8935352
memory-hit split (client-derived, ADR-0071 D2):
  cold_frac = 100.0000% (cold_reads 595848 · cold_resolves 597028 — re-resolve ratio 1.00×)
  mem_hit p50_us = 15 · p99_us = 15 · p999_us = 15
  separation: mem_hit p99.9 15 µs vs server cold p50 255 µs (client tail spread 0 µs — the truncation only separates the two modes while cold service exceeds it)
  NOT gate-eligible — 11980 ops < 100000 — too few for a p99.9 gate value
tripwire sqes/submit = 1.6
```

## ycsb-f-zipfian

```
workload = f (zipfian)
ops = 1754366
errors = 0
nils = 0
ops_per_sec = 87713
combined_client p50_us = 479 · p99_us = 2367 · p999_us = 49151 · max_us = 188165
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 71.18%
stream_checksum = 0x300c7a820e06578d
tiering_cold_p50_us (worst cell) = 251
tiering_cold_p99_us (worst cell) = 975
tiering_cold_p999_us (worst cell) = 23551
cold_read_qd_p99 (worst cell) = 5
coalesce_ratio_milli (worst cell) = 0
server ram-hit fields: WITHDRAWN — tiering_ram_hit_p50_us, tiering_ram_hit_p99_us, tiering_ram_hit_p999_us named-absent, server says `unmeasured-iteration-clock`; the reactor's per-iteration clock cannot time a non-suspending command (ADR-0064 amendment 2026-08-08). The memory-hit half below is client-derived.
tiering_cold_resolves = 9804421
memory-hit split (client-derived, ADR-0071 D2):
  cold_frac = 22.5619% (cold_reads 395819 · cold_resolves 869069 — re-resolve ratio 2.20×)
  mem_hit p50_us = 383 · p99_us = 799 · p999_us = 815
  separation: mem_hit p99.9 815 µs vs server cold p50 251 µs (client tail spread 432 µs — the truncation only separates the two modes while cold service exceeds it)
  NOT gate-eligible — separation check FAILED: derived memory-hit p99.9 815 µs >= server cold p50 251 µs — the two populations overlap and the quantile truncation cannot tell them apart (client tail spread 432 µs vs cold service 251 µs)
tripwire sqes/submit = 1.6
```

## ycsb-f-uniform

```
workload = f (uniform)
ops = 521018
errors = 0
nils = 0
ops_per_sec = 26049
combined_client p50_us = 1535 · p99_us = 29695 · p999_us = 98303 · max_us = 293951
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 0.00%
stream_checksum = 0x5b5934243d804664
tiering_cold_p50_us (worst cell) = 255
tiering_cold_p99_us (worst cell) = 975
tiering_cold_p999_us (worst cell) = 24063
cold_read_qd_p99 (worst cell) = 5
coalesce_ratio_milli (worst cell) = 0
server ram-hit fields: WITHDRAWN — tiering_ram_hit_p50_us, tiering_ram_hit_p99_us, tiering_ram_hit_p999_us named-absent, server says `unmeasured-iteration-clock`; the reactor's per-iteration clock cannot time a non-suspending command (ADR-0064 amendment 2026-08-08). The memory-hit half below is client-derived.
tiering_cold_resolves = 10878386
memory-hit split (client-derived, ADR-0071 D2):
  cold_frac = 92.4438% (cold_reads 481649 · cold_resolves 1073965 — re-resolve ratio 2.23×)
  mem_hit p50_us = 391 · p99_us = 527 · p999_us = 527
  separation: mem_hit p99.9 527 µs vs server cold p50 255 µs (client tail spread 136 µs — the truncation only separates the two modes while cold service exceeds it)
  NOT gate-eligible — cold fraction 92.4% > 50% — the hot set is not memory-resident in this row, so the truncation describes no useful population
tripwire sqes/submit = 1.6
```
