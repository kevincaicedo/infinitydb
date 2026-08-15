# M4 gate-run report

date: 1786831834 (unix) · cells: 4 · conns: 8 · pipeline: 1 · duration: 60s · dataset: 1× budget · ycsb rows: 10 (M4-S22)
env-check: OK
tier: reference-box (binding)

notes:
- harness: inf-bench ycsb — memtier cannot drive a 10× RAM zipfian tier workload (§6)
- E-adaptation: cursor-scan slices (SCAN <cursor> COUNT 1..=100, pipeline 1) — the keyspace is unordered, no ordered range-scan exists (documented deviation)
- value shape: single constant-byte value of --value-size (default 1 KiB ≈ YCSB's 10×100 B fields); D's `latest` uses a per-connection frontier estimate
- dataset: 2097152 keys × 1024 B = 2048 MiB = 1× the 2048 MiB memory budget · seed 0x1d0c2026 · θ 0.99
- mode: TIERED — the D8 refusal is lifted; rows run against the tiered namespace
- zipf self-check: top-1% share measured 68.48% vs analytic 67.95% (θ=0.99, n=2097152, 2000000 draws)
- loader: 2097152 keys in 3.4s (608378 sets/s, 1 passes), DBSIZE == keys asserted
- hot-set instrument role: reference (RAM-resident) leg at conns=8 pipeline=1 value_size=1024 (ADR-0071 D6 — both legs of the comparison must share this config; the comparison refuses on a mismatch)
- saturation (ycsb-a-zipfian): GENERATOR-LIMITED at 8 conns (+50% moved ops/s +29.6%) — absolutes understate the server; deltas remain valid at fixed generator config
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
| Write amplification, worst tiered namespace | < 3 x user bytes (wal + flush) | 1.10 | PASS |
| Memory-only rows append zero log records (M2 posture carried) | <= 0 records | — | PENDING (tooling) |
| Mixed-node attribution divergence (M4-S20) | <= 10 pct, worst continuous sample | — | PENDING (tooling) |
| Cache-namespace p99 isolation under the mixed node (M4-S20) | <= 10 pct vs same-campaign solo baseline | — | PENDING (tooling) |
| Hot set at memory speed: memory-hit p50 delta | <= 10 pct vs RAM-resident reference leg, memory-hit split | — | PENDING (tooling) |
| Hot set at memory speed: memory-hit p99 delta | <= 10 pct vs RAM-resident reference leg, memory-hit split | — | PENDING (tooling) |
| Hot set at memory speed: memory-hit p99.9 delta | <= 10 pct vs RAM-resident reference leg, memory-hit split (LogHistogram ~3% buckets) | — | PENDING (tooling) |
| Cold reads: p99 < 1.5 ms on NVMe under loaded zipfian rows | < 1.5 ms, cold-read split histogram, worst loaded row | 36.86 | FAIL |
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
| ycsb-a-zipfian | 1.023× worst of 4 namespace(s) · blob: n/a (no blob activity) |
| ycsb-a-uniform | 1.026× worst of 4 namespace(s) · blob: n/a (no blob activity) |
| ycsb-b-zipfian | 1.026× worst of 4 namespace(s) · blob: n/a (no blob activity) |
| ycsb-b-uniform | 1.026× worst of 4 namespace(s) · blob: n/a (no blob activity) |
| ycsb-c-zipfian | 1.026× worst of 4 namespace(s) · blob: n/a (no blob activity) |
| ycsb-c-uniform | 1.026× worst of 4 namespace(s) · blob: n/a (no blob activity) |
| ycsb-d-latest | 1.079× worst of 4 namespace(s) · blob: n/a (no blob activity) |
| ycsb-e-zipfian | 1.079× worst of 4 namespace(s) · blob: n/a (no blob activity) |
| ycsb-f-zipfian | 1.084× worst of 4 namespace(s) · blob: n/a (no blob activity) |
| ycsb-f-uniform | 1.105× worst of 4 namespace(s) · blob: n/a (no blob activity) |

## loader fill

```
ops = 2097152
errors = 0
busy_retryable = 0
elapsed_s = 3.447
ops_per_sec = 608378
p50_us = 125
p99_us = 359
p999_us = 2815
p9999_us = 22015
max_us = 391592
```

## ycsb-a-zipfian

```
workload = a (zipfian)
ops = 10420779
errors = 0
nils = 0
ops_per_sec = 173679
combined_client p50_us = 27 · p99_us = 167 · p999_us = 2431 · max_us = 441056
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 68.44%
stream_checksum = 0xee101aec015b8945
tiering_cold_p50_us (worst cell) = 131
tiering_cold_p99_us (worst cell) = 36863
tiering_cold_p999_us (worst cell) = 200703
cold_read_qd_p99 (worst cell) = 3
coalesce_ratio_milli (worst cell) = 0
server ram-hit fields: WITHDRAWN — tiering_ram_hit_p50_us, tiering_ram_hit_p99_us, tiering_ram_hit_p999_us named-absent, server says `unmeasured-iteration-clock`; the reactor's per-iteration clock cannot time a non-suspending command (ADR-0064 amendment 2026-08-08). The memory-hit half below is client-derived.
tiering_cold_resolves = 63325
memory-hit split (client-derived, ADR-0071 D2):
  cold_frac = 0.2429% (cold_reads 25317 · cold_resolves 63325 — re-resolve ratio 2.50×)
  mem_hit p50_us = 27 · p99_us = 155 · p999_us = 211
  separation: not applicable — this is the reference (RAM-resident) leg, which has no cold mode to separate from (ADR-0071 D6); it is checked against the 1% RAM-residency bound instead
  gate-eligible (separation check passed)
tripwire sqes/submit = 1.7
```

## ycsb-a-uniform

```
workload = a (uniform)
ops = 6237034
errors = 0
nils = 0
ops_per_sec = 103950
combined_client p50_us = 30 · p99_us = 215 · p999_us = 15359 · max_us = 246634
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 0.00%
stream_checksum = 0xde7ca3af05339713
tiering_cold_p50_us (worst cell) = 139
tiering_cold_p99_us (worst cell) = 29183
tiering_cold_p999_us (worst cell) = 81919
cold_read_qd_p99 (worst cell) = 3
coalesce_ratio_milli (worst cell) = 0
server ram-hit fields: WITHDRAWN — tiering_ram_hit_p50_us, tiering_ram_hit_p99_us, tiering_ram_hit_p999_us named-absent, server says `unmeasured-iteration-clock`; the reactor's per-iteration clock cannot time a non-suspending command (ADR-0064 amendment 2026-08-08). The memory-hit half below is client-derived.
tiering_cold_resolves = 382722
memory-hit split (client-derived, ADR-0071 D2):
  cold_frac = 1.8642% (cold_reads 116273 · cold_resolves 290837 — re-resolve ratio 2.50×)
  mem_hit p50_us = 29 · p99_us = 155 · p999_us = 175
  separation: not applicable — this is the reference (RAM-resident) leg, which has no cold mode to separate from (ADR-0071 D6); it is checked against the 1% RAM-residency bound instead
  NOT gate-eligible — reference leg cold fraction 1.864% > 1% — this leg is not RAM-resident, so its percentiles do not describe memory speed and cannot be the hot-set reference (raise the memory budget or lower the dataset)
tripwire sqes/submit = 1.8
```

## ycsb-b-zipfian

```
workload = b (zipfian)
ops = 17182287
errors = 0
nils = 0
ops_per_sec = 286370
combined_client p50_us = 25 · p99_us = 89 · p999_us = 215 · max_us = 95199
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 68.44%
stream_checksum = 0xbc980ffbb3b79ca0
tiering_cold_p50_us (worst cell) = 125
tiering_cold_p99_us (worst cell) = 23551
tiering_cold_p999_us (worst cell) = 65535
cold_read_qd_p99 (worst cell) = 2
coalesce_ratio_milli (worst cell) = 0
server ram-hit fields: WITHDRAWN — tiering_ram_hit_p50_us, tiering_ram_hit_p99_us, tiering_ram_hit_p999_us named-absent, server says `unmeasured-iteration-clock`; the reactor's per-iteration clock cannot time a non-suspending command (ADR-0064 amendment 2026-08-08). The memory-hit half below is client-derived.
tiering_cold_resolves = 540581
memory-hit split (client-derived, ADR-0071 D2):
  cold_frac = 0.4483% (cold_reads 77020 · cold_resolves 157859 — re-resolve ratio 2.05×)
  mem_hit p50_us = 25 · p99_us = 46 · p999_us = 139
  separation: not applicable — this is the reference (RAM-resident) leg, which has no cold mode to separate from (ADR-0071 D6); it is checked against the 1% RAM-residency bound instead
  gate-eligible (separation check passed)
tripwire sqes/submit = 1.6
```

## ycsb-b-uniform

```
workload = b (uniform)
ops = 12996033
errors = 0
nils = 0
ops_per_sec = 216600
combined_client p50_us = 27 · p99_us = 159 · p999_us = 287 · max_us = 167021
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 0.00%
stream_checksum = 0xefbe603d84c730c5
tiering_cold_p50_us (worst cell) = 123
tiering_cold_p99_us (worst cell) = 18943
tiering_cold_p999_us (worst cell) = 60415
cold_read_qd_p99 (worst cell) = 2
coalesce_ratio_milli (worst cell) = 0
server ram-hit fields: WITHDRAWN — tiering_ram_hit_p50_us, tiering_ram_hit_p99_us, tiering_ram_hit_p999_us named-absent, server says `unmeasured-iteration-clock`; the reactor's per-iteration clock cannot time a non-suspending command (ADR-0064 amendment 2026-08-08). The memory-hit half below is client-derived.
tiering_cold_resolves = 1017843
memory-hit split (client-derived, ADR-0071 D2):
  cold_frac = 1.7973% (cold_reads 233581 · cold_resolves 477262 — re-resolve ratio 2.04×)
  mem_hit p50_us = 26 · p99_us = 107 · p999_us = 139
  separation: not applicable — this is the reference (RAM-resident) leg, which has no cold mode to separate from (ADR-0071 D6); it is checked against the 1% RAM-residency bound instead
  NOT gate-eligible — reference leg cold fraction 1.797% > 1% — this leg is not RAM-resident, so its percentiles do not describe memory speed and cannot be the hot-set reference (raise the memory budget or lower the dataset)
tripwire sqes/submit = 1.6
```

## ycsb-c-zipfian

```
workload = c (zipfian)
ops = 18397311
errors = 0
nils = 0
ops_per_sec = 306621
combined_client p50_us = 25 · p99_us = 43 · p999_us = 211 · max_us = 3929
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 68.44%
stream_checksum = 0xab2ad658c8f89a66
tiering_cold_p50_us (worst cell) = 125
tiering_cold_p99_us (worst cell) = 16895
tiering_cold_p999_us (worst cell) = 59391
cold_read_qd_p99 (worst cell) = 2
coalesce_ratio_milli (worst cell) = 0
server ram-hit fields: WITHDRAWN — tiering_ram_hit_p50_us, tiering_ram_hit_p99_us, tiering_ram_hit_p999_us named-absent, server says `unmeasured-iteration-clock`; the reactor's per-iteration clock cannot time a non-suspending command (ADR-0064 amendment 2026-08-08). The memory-hit half below is client-derived.
tiering_cold_resolves = 1207667
memory-hit split (client-derived, ADR-0071 D2):
  cold_frac = 0.5159% (cold_reads 94912 · cold_resolves 189824 — re-resolve ratio 2.00×)
  mem_hit p50_us = 25 · p99_us = 39 · p999_us = 131
  separation: not applicable — this is the reference (RAM-resident) leg, which has no cold mode to separate from (ADR-0071 D6); it is checked against the 1% RAM-residency bound instead
  gate-eligible (separation check passed)
tripwire sqes/submit = 1.5
```

## ycsb-c-uniform

```
workload = c (uniform)
ops = 15278825
errors = 0
nils = 0
ops_per_sec = 254646
combined_client p50_us = 26 · p99_us = 187 · p999_us = 271 · max_us = 3034
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 0.00%
stream_checksum = 0x028acd57944db038
tiering_cold_p50_us (worst cell) = 135
tiering_cold_p99_us (worst cell) = 12287
tiering_cold_p999_us (worst cell) = 50175
cold_read_qd_p99 (worst cell) = 2
coalesce_ratio_milli (worst cell) = 0
server ram-hit fields: WITHDRAWN — tiering_ram_hit_p50_us, tiering_ram_hit_p99_us, tiering_ram_hit_p999_us named-absent, server says `unmeasured-iteration-clock`; the reactor's per-iteration clock cannot time a non-suspending command (ADR-0064 amendment 2026-08-08). The memory-hit half below is client-derived.
tiering_cold_resolves = 1775693
memory-hit split (client-derived, ADR-0071 D2):
  cold_frac = 1.8589% (cold_reads 284013 · cold_resolves 568026 — re-resolve ratio 2.00×)
  mem_hit p50_us = 26 · p99_us = 123 · p999_us = 155
  separation: not applicable — this is the reference (RAM-resident) leg, which has no cold mode to separate from (ADR-0071 D6); it is checked against the 1% RAM-residency bound instead
  NOT gate-eligible — reference leg cold fraction 1.859% > 1% — this leg is not RAM-resident, so its percentiles do not describe memory speed and cannot be the hot-set reference (raise the memory budget or lower the dataset)
tripwire sqes/submit = 1.5
```

## ycsb-d-latest

```
workload = d (latest)
ops = 15510328
errors = 0
nils = 4914033
ops_per_sec = 258504
combined_client p50_us = 24 · p99_us = 151 · p999_us = 319 · max_us = 49122
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 68.46%
stream_checksum = 0x716b7699faafdb97
tiering_cold_p50_us (worst cell) = 125
tiering_cold_p99_us (worst cell) = 7551
tiering_cold_p999_us (worst cell) = 40959
cold_read_qd_p99 (worst cell) = 2
coalesce_ratio_milli (worst cell) = 0
server ram-hit fields: WITHDRAWN — tiering_ram_hit_p50_us, tiering_ram_hit_p99_us, tiering_ram_hit_p999_us named-absent, server says `unmeasured-iteration-clock`; the reactor's per-iteration clock cannot time a non-suspending command (ADR-0064 amendment 2026-08-08). The memory-hit half below is client-derived.
tiering_cold_resolves = 2366601
memory-hit split (client-derived, ADR-0071 D2):
  cold_frac = 1.9048% (cold_reads 295443 · cold_resolves 590908 — re-resolve ratio 2.00×)
  mem_hit p50_us = 24 · p99_us = 107 · p999_us = 135
  separation: not applicable — this is the reference (RAM-resident) leg, which has no cold mode to separate from (ADR-0071 D6); it is checked against the 1% RAM-residency bound instead
  NOT gate-eligible — reference leg cold fraction 1.905% > 1% — this leg is not RAM-resident, so its percentiles do not describe memory speed and cannot be the hot-set reference (raise the memory budget or lower the dataset)
tripwire sqes/submit = 1.5
```

## ycsb-e-zipfian

```
workload = e (zipfian)
ops = 171163
errors = 0
nils = 0
ops_per_sec = 2853
combined_client p50_us = 2751 · p99_us = 6783 · p999_us = 8703 · max_us = 139586
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 0.00%
stream_checksum = 0x123a17638fb48fc5
tiering_cold_p50_us (worst cell) = 139
tiering_cold_p99_us (worst cell) = 7551
tiering_cold_p999_us (worst cell) = 40959
cold_read_qd_p99 (worst cell) = 5
coalesce_ratio_milli (worst cell) = 0
server ram-hit fields: WITHDRAWN — tiering_ram_hit_p50_us, tiering_ram_hit_p99_us, tiering_ram_hit_p999_us named-absent, server says `unmeasured-iteration-clock`; the reactor's per-iteration clock cannot time a non-suspending command (ADR-0064 amendment 2026-08-08). The memory-hit half below is client-derived.
tiering_cold_resolves = 5033473
memory-hit split (client-derived, ADR-0071 D2):
  cold_frac = 100.0000% (cold_reads 2666593 · cold_resolves 2666872 — re-resolve ratio 1.00×)
  mem_hit p50_us = 11 · p99_us = 11 · p999_us = 11
  separation: not applicable — this is the reference (RAM-resident) leg, which has no cold mode to separate from (ADR-0071 D6); it is checked against the 1% RAM-residency bound instead
  NOT gate-eligible — reference leg cold fraction 100.000% > 1% — this leg is not RAM-resident, so its percentiles do not describe memory speed and cannot be the hot-set reference (raise the memory budget or lower the dataset)
tripwire sqes/submit = 1.5
```

## ycsb-f-zipfian

```
workload = f (zipfian)
ops = 5701980
errors = 0
nils = 0
ops_per_sec = 95033
combined_client p50_us = 37 · p99_us = 391 · p999_us = 3647 · max_us = 313217
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 68.43%
stream_checksum = 0xf55148f724e7d7e9
tiering_cold_p50_us (worst cell) = 131
tiering_cold_p99_us (worst cell) = 6399
tiering_cold_p999_us (worst cell) = 38911
cold_read_qd_p99 (worst cell) = 5
coalesce_ratio_milli (worst cell) = 0
server ram-hit fields: WITHDRAWN — tiering_ram_hit_p50_us, tiering_ram_hit_p99_us, tiering_ram_hit_p999_us named-absent, server says `unmeasured-iteration-clock`; the reactor's per-iteration clock cannot time a non-suspending command (ADR-0064 amendment 2026-08-08). The memory-hit half below is client-derived.
tiering_cold_resolves = 6613511
memory-hit split (client-derived, ADR-0071 D2):
  cold_frac = 11.8789% (cold_reads 677333 · cold_resolves 1580038 — re-resolve ratio 2.33×)
  mem_hit p50_us = 33 · p99_us = 111 · p999_us = 121
  separation: not applicable — this is the reference (RAM-resident) leg, which has no cold mode to separate from (ADR-0071 D6); it is checked against the 1% RAM-residency bound instead
  NOT gate-eligible — reference leg cold fraction 11.879% > 1% — this leg is not RAM-resident, so its percentiles do not describe memory speed and cannot be the hot-set reference (raise the memory budget or lower the dataset)
tripwire sqes/submit = 1.5
```

## ycsb-f-uniform

```
workload = f (uniform)
ops = 2214345
errors = 0
nils = 0
ops_per_sec = 36906
combined_client p50_us = 58 · p99_us = 927 · p999_us = 16895 · max_us = 544702
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 0.00%
stream_checksum = 0x79573c1ac0985694
tiering_cold_p50_us (worst cell) = 139
tiering_cold_p99_us (worst cell) = 5887
tiering_cold_p999_us (worst cell) = 25599
cold_read_qd_p99 (worst cell) = 5
coalesce_ratio_milli (worst cell) = 0
server ram-hit fields: WITHDRAWN — tiering_ram_hit_p50_us, tiering_ram_hit_p99_us, tiering_ram_hit_p999_us named-absent, server says `unmeasured-iteration-clock`; the reactor's per-iteration clock cannot time a non-suspending command (ADR-0064 amendment 2026-08-08). The memory-hit half below is client-derived.
tiering_cold_resolves = 9693568
memory-hit split (client-derived, ADR-0071 D2):
  cold_frac = 59.6498% (cold_reads 1320853 · cold_resolves 3080057 — re-resolve ratio 2.33×)
  mem_hit p50_us = 23 · p99_us = 40 · p999_us = 41
  separation: not applicable — this is the reference (RAM-resident) leg, which has no cold mode to separate from (ADR-0071 D6); it is checked against the 1% RAM-residency bound instead
  NOT gate-eligible — reference leg cold fraction 59.650% > 1% — this leg is not RAM-resident, so its percentiles do not describe memory speed and cannot be the hot-set reference (raise the memory budget or lower the dataset)
tripwire sqes/submit = 1.5
```
