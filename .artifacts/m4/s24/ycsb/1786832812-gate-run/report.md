# M4 gate-run report

date: 1786832812 (unix) · cells: 4 · conns: 8 · pipeline: 1 · duration: 60s · dataset: 10× budget · ycsb rows: 10 (M4-S22)
env-check: OK
tier: reference-box (binding)

notes:
- harness: inf-bench ycsb — memtier cannot drive a 10× RAM zipfian tier workload (§6)
- E-adaptation: cursor-scan slices (SCAN <cursor> COUNT 1..=100, pipeline 1) — the keyspace is unordered, no ordered range-scan exists (documented deviation)
- value shape: single constant-byte value of --value-size (default 1 KiB ≈ YCSB's 10×100 B fields); D's `latest` uses a per-connection frontier estimate
- dataset: 20971520 keys × 1024 B = 20480 MiB = 10× the 2048 MiB memory budget · seed 0x1d0c2026 · θ 0.99
- mode: TIERED — the D8 refusal is lifted; rows run against the tiered namespace
- zipf self-check: top-1% share measured 72.26% vs analytic 71.87% (θ=0.99, n=20971520, 2000000 draws)
- seed verification: 100000 ops regenerated identically (checksum 0xab8d5646b6e63542)
- loader: 20971520 keys in 306.5s (68434 sets/s, 1 passes), DBSIZE == keys asserted
- hot-set instrument role: tiered leg at conns=8 pipeline=1 value_size=1024 (ADR-0071 D6 — both legs of the comparison must share this config; the comparison refuses on a mismatch)
- saturation (ycsb-a-zipfian): GENERATOR-LIMITED at 8 conns (+50% moved ops/s +25.1%) — absolutes understate the server; deltas remain valid at fixed generator config
- hot-set gate: 3 row(s) compared against the RAM-resident reference leg (worst per percentile binds); 7 row(s) excluded and named in the section

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
| Hot set at memory speed: memory-hit p50 delta | <= 10 pct vs RAM-resident reference leg, memory-hit split | -12.00 | PASS |
| Hot set at memory speed: memory-hit p99 delta | <= 10 pct vs RAM-resident reference leg, memory-hit split | 328.21 | FAIL |
| Hot set at memory speed: memory-hit p99.9 delta | <= 10 pct vs RAM-resident reference leg, memory-hit split (LogHistogram ~3% buckets) | 40.29 | FAIL |
| Cold reads: p99 < 1.5 ms on NVMe under loaded zipfian rows | < 1.5 ms, cold-read split histogram, worst loaded row | 1.44 | PASS |
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
| ycsb-a-zipfian | 1.894× worst of 4 namespace(s) · blob: n/a (no blob activity) |
| ycsb-a-uniform | 1.878× worst of 4 namespace(s) · blob: n/a (no blob activity) |
| ycsb-b-zipfian | 1.872× worst of 4 namespace(s) · blob: n/a (no blob activity) |
| ycsb-b-uniform | 1.870× worst of 4 namespace(s) · blob: n/a (no blob activity) |
| ycsb-c-zipfian | 1.870× worst of 4 namespace(s) · blob: n/a (no blob activity) |
| ycsb-c-uniform | 1.870× worst of 4 namespace(s) · blob: n/a (no blob activity) |
| ycsb-d-latest | 1.871× worst of 4 namespace(s) · blob: n/a (no blob activity) |
| ycsb-e-zipfian | 1.871× worst of 4 namespace(s) · blob: n/a (no blob activity) |
| ycsb-f-zipfian | 1.831× worst of 4 namespace(s) · blob: n/a (no blob activity) |
| ycsb-f-uniform | 1.812× worst of 4 namespace(s) · blob: n/a (no blob activity) |

## loader fill

```
ops = 20971520
errors = 0
busy_retryable = 0
elapsed_s = 306.451
ops_per_sec = 68434
p50_us = 151
p99_us = 27135
p999_us = 286719
p9999_us = 1277951
max_us = 4834882
```

## ycsb-a-zipfian

```
workload = a (zipfian)
ops = 2392674
errors = 0
nils = 0
ops_per_sec = 39878
combined_client p50_us = 31 · p99_us = 863 · p999_us = 18943 · max_us = 204294
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 72.20%
stream_checksum = 0x8c3ef67297e4fe2f
tiering_cold_p50_us (worst cell) = 235
tiering_cold_p99_us (worst cell) = 1439
tiering_cold_p999_us (worst cell) = 24063
cold_read_qd_p99 (worst cell) = 4
coalesce_ratio_milli (worst cell) = 0
server ram-hit fields: WITHDRAWN — tiering_ram_hit_p50_us, tiering_ram_hit_p99_us, tiering_ram_hit_p999_us named-absent, server says `unmeasured-iteration-clock`; the reactor's per-iteration clock cannot time a non-suspending command (ADR-0064 amendment 2026-08-08). The memory-hit half below is client-derived.
tiering_cold_resolves = 1994180
memory-hit split (client-derived, ADR-0071 D2):
  cold_frac = 33.3253% (cold_reads 797365 · cold_resolves 1994042 — re-resolve ratio 2.50×)
  mem_hit p50_us = 21 · p99_us = 191 · p999_us = 195
  separation: mem_hit p99.9 195 µs vs server cold p50 235 µs (client tail spread 174 µs — the truncation only separates the two modes while cold service exceeds it)
  gate-eligible (separation check passed)
tripwire sqes/submit = 2.2
```

## ycsb-a-uniform

```
workload = a (uniform)
ops = 1242979
errors = 0
nils = 0
ops_per_sec = 20716
combined_client p50_us = 303 · p99_us = 1119 · p999_us = 23551 · max_us = 188077
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 0.00%
stream_checksum = 0x024065573cfc203f
tiering_cold_p50_us (worst cell) = 255
tiering_cold_p99_us (worst cell) = 1183
tiering_cold_p999_us (worst cell) = 24063
cold_read_qd_p99 (worst cell) = 5
coalesce_ratio_milli (worst cell) = 0
server ram-hit fields: WITHDRAWN — tiering_ram_hit_p50_us, tiering_ram_hit_p99_us, tiering_ram_hit_p999_us named-absent, server says `unmeasured-iteration-clock`; the reactor's per-iteration clock cannot time a non-suspending command (ADR-0064 amendment 2026-08-08). The memory-hit half below is client-derived.
tiering_cold_resolves = 5877026
memory-hit split (client-derived, ADR-0071 D2):
  cold_frac = 91.6044% (cold_reads 1138624 · cold_resolves 2846797 — re-resolve ratio 2.50×)
  mem_hit p50_us = 19 · p99_us = 87 · p999_us = 99
  separation: mem_hit p99.9 99 µs vs server cold p50 255 µs (client tail spread 80 µs — the truncation only separates the two modes while cold service exceeds it)
  NOT gate-eligible — cold fraction 91.6% > 50% — the hot set is not memory-resident in this row, so the truncation describes no useful population
tripwire sqes/submit = 1.9
```

## ycsb-b-zipfian

```
workload = b (zipfian)
ops = 4785164
errors = 0
nils = 0
ops_per_sec = 79752
combined_client p50_us = 25 · p99_us = 559 · p999_us = 1343 · max_us = 92140
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 72.22%
stream_checksum = 0x6416878bce9aebbc
tiering_cold_p50_us (worst cell) = 243
tiering_cold_p99_us (worst cell) = 863
tiering_cold_p999_us (worst cell) = 23039
cold_read_qd_p99 (worst cell) = 4
coalesce_ratio_milli (worst cell) = 0
server ram-hit fields: WITHDRAWN — tiering_ram_hit_p50_us, tiering_ram_hit_p99_us, tiering_ram_hit_p999_us named-absent, server says `unmeasured-iteration-clock`; the reactor's per-iteration clock cannot time a non-suspending command (ADR-0064 amendment 2026-08-08). The memory-hit half below is client-derived.
tiering_cold_resolves = 7834325
memory-hit split (client-derived, ADR-0071 D2):
  cold_frac = 20.0992% (cold_reads 961781 · cold_resolves 1957299 — re-resolve ratio 2.04×)
  mem_hit p50_us = 22 · p99_us = 191 · p999_us = 195
  separation: mem_hit p99.9 195 µs vs server cold p50 243 µs (client tail spread 173 µs — the truncation only separates the two modes while cold service exceeds it)
  gate-eligible (separation check passed)
tripwire sqes/submit = 1.7
```

## ycsb-b-uniform

```
workload = b (uniform)
ops = 2035279
errors = 0
nils = 0
ops_per_sec = 33921
combined_client p50_us = 223 · p99_us = 815 · p999_us = 1695 · max_us = 26830
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 0.00%
stream_checksum = 0x83f4e2b649ce1961
tiering_cold_p50_us (worst cell) = 247
tiering_cold_p99_us (worst cell) = 751
tiering_cold_p999_us (worst cell) = 19455
cold_read_qd_p99 (worst cell) = 4
coalesce_ratio_milli (worst cell) = 0
server ram-hit fields: WITHDRAWN — tiering_ram_hit_p50_us, tiering_ram_hit_p99_us, tiering_ram_hit_p999_us named-absent, server says `unmeasured-iteration-clock`; the reactor's per-iteration clock cannot time a non-suspending command (ADR-0064 amendment 2026-08-08). The memory-hit half below is client-derived.
tiering_cold_resolves = 10501752
memory-hit split (client-derived, ADR-0071 D2):
  cold_frac = 64.6011% (cold_reads 1314812 · cold_resolves 2667427 — re-resolve ratio 2.03×)
  mem_hit p50_us = 19 · p99_us = 143 · p999_us = 143
  separation: mem_hit p99.9 143 µs vs server cold p50 247 µs (client tail spread 124 µs — the truncation only separates the two modes while cold service exceeds it)
  NOT gate-eligible — cold fraction 64.6% > 50% — the hot set is not memory-resident in this row, so the truncation describes no useful population
tripwire sqes/submit = 1.6
```

## ycsb-c-zipfian

```
workload = c (zipfian)
ops = 5627279
errors = 0
nils = 0
ops_per_sec = 93787
combined_client p50_us = 21 · p99_us = 527 · p999_us = 751 · max_us = 3844
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 72.22%
stream_checksum = 0x3d81bffd54ed9c20
tiering_cold_p50_us (worst cell) = 243
tiering_cold_p99_us (worst cell) = 703
tiering_cold_p999_us (worst cell) = 17407
cold_read_qd_p99 (worst cell) = 4
coalesce_ratio_milli (worst cell) = 0
server ram-hit fields: WITHDRAWN — tiering_ram_hit_p50_us, tiering_ram_hit_p99_us, tiering_ram_hit_p999_us named-absent, server says `unmeasured-iteration-clock`; the reactor's per-iteration clock cannot time a non-suspending command (ADR-0064 amendment 2026-08-08). The memory-hit half below is client-derived.
tiering_cold_resolves = 12776987
memory-hit split (client-derived, ADR-0071 D2):
  cold_frac = 20.2160% (cold_reads 1137610 · cold_resolves 2275235 — re-resolve ratio 2.00×)
  mem_hit p50_us = 20 · p99_us = 167 · p999_us = 175
  separation: mem_hit p99.9 175 µs vs server cold p50 243 µs (client tail spread 155 µs — the truncation only separates the two modes while cold service exceeds it)
  gate-eligible (separation check passed)
tripwire sqes/submit = 1.5
```

## ycsb-c-uniform

```
workload = c (uniform)
ops = 2002794
errors = 0
nils = 0
ops_per_sec = 33380
combined_client p50_us = 227 · p99_us = 847 · p999_us = 1183 · max_us = 12867
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 0.00%
stream_checksum = 0xe9f82b57a30bcf7b
tiering_cold_p50_us (worst cell) = 243
tiering_cold_p99_us (worst cell) = 671
tiering_cold_p999_us (worst cell) = 15615
cold_read_qd_p99 (worst cell) = 4
coalesce_ratio_milli (worst cell) = 0
server ram-hit fields: WITHDRAWN — tiering_ram_hit_p50_us, tiering_ram_hit_p99_us, tiering_ram_hit_p999_us named-absent, server says `unmeasured-iteration-clock`; the reactor's per-iteration clock cannot time a non-suspending command (ADR-0064 amendment 2026-08-08). The memory-hit half below is client-derived.
tiering_cold_resolves = 15281469
memory-hit split (client-derived, ADR-0071 D2):
  cold_frac = 62.5242% (cold_reads 1252231 · cold_resolves 2504482 — re-resolve ratio 2.00×)
  mem_hit p50_us = 19 · p99_us = 163 · p999_us = 163
  separation: mem_hit p99.9 163 µs vs server cold p50 243 µs (client tail spread 144 µs — the truncation only separates the two modes while cold service exceeds it)
  NOT gate-eligible — cold fraction 62.5% > 50% — the hot set is not memory-resident in this row, so the truncation describes no useful population
tripwire sqes/submit = 1.5
```

## ycsb-d-latest

```
workload = d (latest)
ops = 4048121
errors = 0
nils = 978473
ops_per_sec = 67468
combined_client p50_us = 22 · p99_us = 607 · p999_us = 1279 · max_us = 26711
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 72.22%
stream_checksum = 0x012f3abfb55292a2
tiering_cold_p50_us (worst cell) = 239
tiering_cold_p99_us (worst cell) = 671
tiering_cold_p999_us (worst cell) = 12031
cold_read_qd_p99 (worst cell) = 4
coalesce_ratio_milli (worst cell) = 0
server ram-hit fields: WITHDRAWN — tiering_ram_hit_p50_us, tiering_ram_hit_p99_us, tiering_ram_hit_p999_us named-absent, server says `unmeasured-iteration-clock`; the reactor's per-iteration clock cannot time a non-suspending command (ADR-0064 amendment 2026-08-08). The memory-hit half below is client-derived.
tiering_cold_resolves = 17671786
memory-hit split (client-derived, ADR-0071 D2):
  cold_frac = 29.5235% (cold_reads 1195147 · cold_resolves 2390317 — re-resolve ratio 2.00×)
  mem_hit p50_us = 19 · p99_us = 163 · p999_us = 167
  separation: mem_hit p99.9 167 µs vs server cold p50 239 µs (client tail spread 148 µs — the truncation only separates the two modes while cold service exceeds it)
  gate-eligible (separation check passed)
tripwire sqes/submit = 1.5
```

## ycsb-e-zipfian

```
workload = e (zipfian)
ops = 34114
errors = 0
nils = 0
ops_per_sec = 568
combined_client p50_us = 14079 · p99_us = 30207 · p999_us = 43007 · max_us = 56107
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 0.00%
stream_checksum = 0x65dd7183346373af
tiering_cold_p50_us (worst cell) = 239
tiering_cold_p99_us (worst cell) = 1087
tiering_cold_p999_us (worst cell) = 24575
cold_read_qd_p99 (worst cell) = 4
coalesce_ratio_milli (worst cell) = 1
server ram-hit fields: WITHDRAWN — tiering_ram_hit_p50_us, tiering_ram_hit_p99_us, tiering_ram_hit_p999_us named-absent, server says `unmeasured-iteration-clock`; the reactor's per-iteration clock cannot time a non-suspending command (ADR-0064 amendment 2026-08-08). The memory-hit half below is client-derived.
tiering_cold_resolves = 19321543
memory-hit split (client-derived, ADR-0071 D2):
  cold_frac = 100.0000% (cold_reads 1644859 · cold_resolves 1649757 — re-resolve ratio 1.00×)
  mem_hit p50_us = 14 · p99_us = 14 · p999_us = 14
  separation: mem_hit p99.9 14 µs vs server cold p50 239 µs (client tail spread 0 µs — the truncation only separates the two modes while cold service exceeds it)
  NOT gate-eligible — 34114 ops < 100000 — too few for a p99.9 gate value
tripwire sqes/submit = 1.4
```

## ycsb-f-zipfian

```
workload = f (zipfian)
ops = 2788626
errors = 0
nils = 0
ops_per_sec = 46477
combined_client p50_us = 40 · p99_us = 655 · p999_us = 34815 · max_us = 405795
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 72.18%
stream_checksum = 0xb859c47a5531c58c
tiering_cold_p50_us (worst cell) = 239
tiering_cold_p99_us (worst cell) = 1183
tiering_cold_p999_us (worst cell) = 25599
cold_read_qd_p99 (worst cell) = 4
coalesce_ratio_milli (worst cell) = 1
server ram-hit fields: WITHDRAWN — tiering_ram_hit_p50_us, tiering_ram_hit_p99_us, tiering_ram_hit_p999_us named-absent, server says `unmeasured-iteration-clock`; the reactor's per-iteration clock cannot time a non-suspending command (ADR-0064 amendment 2026-08-08). The memory-hit half below is client-derived.
tiering_cold_resolves = 20472943
memory-hit split (client-derived, ADR-0071 D2):
  cold_frac = 19.1183% (cold_reads 533139 · cold_resolves 1151400 — re-resolve ratio 2.16×)
  mem_hit p50_us = 33 · p99_us = 147 · p999_us = 155
  separation: mem_hit p99.9 155 µs vs server cold p50 239 µs (client tail spread 122 µs — the truncation only separates the two modes while cold service exceeds it)
  gate-eligible (separation check passed)
tripwire sqes/submit = 1.4
```

## ycsb-f-uniform

```
workload = f (uniform)
ops = 1588962
errors = 0
nils = 0
ops_per_sec = 26482
combined_client p50_us = 215 · p99_us = 1215 · p999_us = 24063 · max_us = 178706
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 0.00%
stream_checksum = 0xf871c635b9bffe76
tiering_cold_p50_us (worst cell) = 239
tiering_cold_p99_us (worst cell) = 1087
tiering_cold_p999_us (worst cell) = 25087
cold_read_qd_p99 (worst cell) = 4
coalesce_ratio_milli (worst cell) = 0
server ram-hit fields: WITHDRAWN — tiering_ram_hit_p50_us, tiering_ram_hit_p99_us, tiering_ram_hit_p999_us named-absent, server says `unmeasured-iteration-clock`; the reactor's per-iteration clock cannot time a non-suspending command (ADR-0064 amendment 2026-08-08). The memory-hit half below is client-derived.
tiering_cold_resolves = 22787597
memory-hit split (client-derived, ADR-0071 D2):
  cold_frac = 67.5789% (cold_reads 1073803 · cold_resolves 2314654 — re-resolve ratio 2.16×)
  mem_hit p50_us = 37 · p99_us = 51 · p999_us = 51
  separation: mem_hit p99.9 51 µs vs server cold p50 239 µs (client tail spread 14 µs — the truncation only separates the two modes while cold service exceeds it)
  NOT gate-eligible — cold fraction 67.6% > 50% — the hot set is not memory-resident in this row, so the truncation describes no useful population
tripwire sqes/submit = 1.4
```

## hot-set gate: tiered vs RAM-resident reference leg

```
reference leg: .artifacts/m4/s24/ycsb-ref/1786831834-gate-run/
this leg: conns=8 pipeline=1 value_size=1024
reference leg config: conns=8 pipeline=1 value_size=1024

| row | percentile | reference µs | tiered µs | delta |
|---|---|---|---|---|
| ycsb-a-zipfian | p50 | 27 | 21 | -22.22% |
| ycsb-a-zipfian | p99 | 155 | 191 | +23.23% |
| ycsb-a-zipfian | p99.9 | 211 | 195 | -7.58% |
| ycsb-b-zipfian | p50 | 25 | 22 | -12.00% |
| ycsb-b-zipfian | p99 | 46 | 191 | +315.22% |
| ycsb-b-zipfian | p99.9 | 139 | 195 | +40.29% |
| ycsb-c-zipfian | p50 | 25 | 20 | -20.00% |
| ycsb-c-zipfian | p99 | 39 | 167 | +328.21% |
| ycsb-c-zipfian | p99.9 | 131 | 175 | +33.59% |

excluded rows (named, not dropped):
- ycsb-a-uniform (tiered leg: cold fraction 91.6% > 50% — the hot set is not memory-resident in this row, so the truncation describes no useful population)
- ycsb-b-uniform (tiered leg: cold fraction 64.6% > 50% — the hot set is not memory-resident in this row, so the truncation describes no useful population)
- ycsb-c-uniform (tiered leg: cold fraction 62.5% > 50% — the hot set is not memory-resident in this row, so the truncation describes no useful population)
- ycsb-d-latest (reference leg ruled it ineligible)
- ycsb-e-zipfian (tiered leg: 34114 ops < 100000 — too few for a p99.9 gate value)
- ycsb-f-zipfian (reference leg ruled it ineligible)
- ycsb-f-uniform (tiered leg: cold fraction 67.6% > 50% — the hot set is not memory-resident in this row, so the truncation describes no useful population)
```
