# M4 gate-run report

date: 1786416839 (unix) · cells: 4 · duration: 10s · replicates: 6 · degenerate-case A/B (M4-S03; hard sub-gate, re-run at week-4 risk gate + S24)
env-check: OK
tier: reference-box (binding)

notes:
- p99.9 deltas are quantized by LogHistogram (32 sub-buckets/octave ≈ 3%): 0.0% = same bucket; any non-zero delta spans ≥ 1 bucket
- m4 binary /home/kcaicedo/.cache/inf-campaign/v0.4.0-bin/infinityd-m4: hash64:2b50b03e54378a6f (12722784 bytes)
- m3 baseline /home/kcaicedo/.cache/inf-campaign/v0.4.0-bin/infinityd-m4: hash64:2b50b03e54378a6f (12722784 bytes) — pin this fingerprint across the week-4 and S24 re-runs; the commit it was built from is recorded in the ledger row (C15 lesson)
- server cells pinned: --pin-start 4 (same cpu set both legs)
- slot crossover active (week-4 instrument fix): servers respawn per replicate and the binary↔slot assignment alternates; legs run in spawn order so slot + load-order bias cancels in the leg medians over an even replicate count
- pipelined 1:10 (M0 gate mix): m3 3154201 ops/s (spread 5.21%) vs m4 3148415 ops/s (spread 2.12%) — signed ops delta -0.18% · p999 687 → 671 µs (-2.33%) · peak-RSS 188555264 → 188571648 B (+0.01%)
- unpipelined 512-conn (M0 gate mix): m3 797138 ops/s (spread 1.07%) vs m4 793614 ops/s (spread 0.41%) — signed ops delta -0.44% · p999 1311 → 1343 µs (+2.44%) · peak-RSS 119898112 → 119820288 B (-0.06%)
- ttl-heavy 1:1 writes (M1 gate mix): m3 2683404 ops/s (spread 4.66%) vs m4 2674701 ops/s (spread 8.40%) — signed ops delta -0.32% · p999 4351 → 4607 µs (+5.88%) · peak-RSS 243392512 → 242528256 B (-0.36%)

| gate | threshold | measured | verdict |
|---|---|---|---|
| Degenerate A/B: pipelined ops regression | <= 1 % vs M3 baseline | 0.18 | PASS |
| Degenerate A/B: pipelined p99.9 regression | <= 1 % vs M3 baseline (LogHistogram ~3% buckets: nonzero spans >= 1 bucket) | 0.00 | PASS |
| Degenerate A/B: unpipelined ops regression | <= 1 % vs M3 baseline | 0.44 | PASS |
| Degenerate A/B: unpipelined p99.9 regression | <= 1 % vs M3 baseline | 2.44 | FAIL |
| Degenerate A/B: ttl-heavy ops regression | <= 1 % vs M3 baseline | 0.32 | PASS |
| Degenerate A/B: ttl-heavy p99.9 regression | <= 1 % vs M3 baseline | 5.88 | FAIL |
| Degenerate A/B: peak-RSS regression (worst row) | <= 1 % vs M3 baseline | 0.01 | PASS |
| Memory-mode node constructs zero tiered tables | <= 0 tables | 0.00 | PASS |
| Tiering code-path counters identically zero | <= 0 counter sum | 0.00 | PASS |
| Write amplification, worst tiered namespace | < 3 x user bytes (wal + flush) | — | PENDING (tooling) |
| Memory-only rows append zero log records (M2 posture carried) | <= 0 records | 0.00 | PASS |
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
| pipelined 1:10 (M0 gate mix) | n/a (no tiered namespace on the node — memory-mode row) · blob: n/a (no blob activity) |
| unpipelined 512-conn (M0 gate mix) | n/a (no tiered namespace on the node — memory-mode row) · blob: n/a (no blob activity) |
| ttl-heavy 1:1 writes (M1 gate mix) | n/a (no tiered namespace on the node — memory-mode row) · blob: n/a (no blob activity) |

## pipelined 1:10 (M0 gate mix) m4 rep 0

```
ops = 31048770
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3104476
p50_us = 327
p99_us = 527
p999_us = 655
p9999_us = 10751
max_us = 11934
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 0

```
ops = 30393279
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3038986
p50_us = 327
p99_us = 575
p999_us = 687
p9999_us = 10239
max_us = 11757
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 1

```
ops = 31379227
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3137478
p50_us = 319
p99_us = 575
p999_us = 687
p9999_us = 10495
max_us = 10889
```

## pipelined 1:10 (M0 gate mix) m4 rep 1

```
ops = 31715131
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3171113
p50_us = 319
p99_us = 527
p999_us = 623
p9999_us = 10495
max_us = 10808
```

## pipelined 1:10 (M0 gate mix) m4 rep 2

```
ops = 31599700
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3159510
p50_us = 319
p99_us = 527
p999_us = 623
p9999_us = 10239
max_us = 10900
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 2

```
ops = 31345107
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3134073
p50_us = 319
p99_us = 575
p999_us = 687
p9999_us = 10495
max_us = 11666
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 3

```
ops = 31636860
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3163232
p50_us = 319
p99_us = 559
p999_us = 671
p9999_us = 10239
max_us = 11310
```

## pipelined 1:10 (M0 gate mix) m4 rep 3

```
ops = 31299282
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3129513
p50_us = 311
p99_us = 591
p999_us = 719
p9999_us = 10239
max_us = 11478
```

## pipelined 1:10 (M0 gate mix) m4 rep 4

```
ops = 31309024
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3130472
p50_us = 319
p99_us = 559
p999_us = 671
p9999_us = 5759
max_us = 11958
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 4

```
ops = 32035762
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3203187
p50_us = 319
p99_us = 503
p999_us = 607
p9999_us = 10239
max_us = 11060
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 5

```
ops = 31546205
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3154201
p50_us = 319
p99_us = 559
p999_us = 687
p9999_us = 10751
max_us = 11235
```

## pipelined 1:10 (M0 gate mix) m4 rep 5

```
ops = 31488493
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3148415
p50_us = 319
p99_us = 559
p999_us = 671
p9999_us = 10495
max_us = 10970
```

## unpipelined 512-conn (M0 gate mix) m4 rep 0

```
ops = 3979737
errors = 0
busy_retryable = 0
elapsed_s = 5.010
ops_per_sec = 794424
p50_us = 623
p99_us = 1023
p999_us = 1215
p9999_us = 3263
max_us = 4080
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 0

```
ops = 3996608
errors = 0
busy_retryable = 0
elapsed_s = 5.008
ops_per_sec = 798104
p50_us = 623
p99_us = 1007
p999_us = 1279
p9999_us = 3327
max_us = 4368
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 1

```
ops = 4005362
errors = 0
busy_retryable = 0
elapsed_s = 5.009
ops_per_sec = 799597
p50_us = 623
p99_us = 1007
p999_us = 1183
p9999_us = 3327
max_us = 6175
```

## unpipelined 512-conn (M0 gate mix) m4 rep 1

```
ops = 3974012
errors = 0
busy_retryable = 0
elapsed_s = 5.007
ops_per_sec = 793614
p50_us = 623
p99_us = 1023
p999_us = 1375
p9999_us = 3199
max_us = 4114
```

## unpipelined 512-conn (M0 gate mix) m4 rep 2

```
ops = 3971901
errors = 0
busy_retryable = 0
elapsed_s = 5.009
ops_per_sec = 792987
p50_us = 623
p99_us = 1023
p999_us = 1343
p9999_us = 3263
max_us = 4175
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 2

```
ops = 3974547
errors = 0
busy_retryable = 0
elapsed_s = 5.008
ops_per_sec = 793619
p50_us = 623
p99_us = 1023
p999_us = 1279
p9999_us = 3199
max_us = 4080
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 3

```
ops = 3991671
errors = 0
busy_retryable = 0
elapsed_s = 5.008
ops_per_sec = 797138
p50_us = 623
p99_us = 1023
p999_us = 1439
p9999_us = 3327
max_us = 4228
```

## unpipelined 512-conn (M0 gate mix) m4 rep 3

```
ops = 3966168
errors = 0
busy_retryable = 0
elapsed_s = 5.009
ops_per_sec = 791744
p50_us = 623
p99_us = 1055
p999_us = 1535
p9999_us = 3263
max_us = 4758
```

## unpipelined 512-conn (M0 gate mix) m4 rep 4

```
ops = 3968001
errors = 0
busy_retryable = 0
elapsed_s = 5.009
ops_per_sec = 792102
p50_us = 623
p99_us = 1023
p999_us = 1279
p9999_us = 3263
max_us = 4022
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 4

```
ops = 3961438
errors = 0
busy_retryable = 0
elapsed_s = 5.008
ops_per_sec = 791055
p50_us = 623
p99_us = 1023
p999_us = 1343
p9999_us = 3263
max_us = 4117
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 5

```
ops = 3980805
errors = 0
busy_retryable = 0
elapsed_s = 5.008
ops_per_sec = 794827
p50_us = 623
p99_us = 1023
p999_us = 1311
p9999_us = 3263
max_us = 4298
```

## unpipelined 512-conn (M0 gate mix) m4 rep 5

```
ops = 3982742
errors = 0
busy_retryable = 0
elapsed_s = 5.010
ops_per_sec = 794993
p50_us = 623
p99_us = 1007
p999_us = 1247
p9999_us = 3199
max_us = 4147
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 0

```
ops = 27394323
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2739046
p50_us = 359
p99_us = 623
p999_us = 3199
p9999_us = 20479
max_us = 20963
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 0

```
ops = 26225477
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2622180
p50_us = 367
p99_us = 687
p999_us = 4095
p9999_us = 16127
max_us = 17058
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 1

```
ops = 27052178
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2704860
p50_us = 359
p99_us = 639
p999_us = 3199
p9999_us = 18943
max_us = 19927
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 1

```
ops = 25151248
errors = 0
busy_retryable = 0
elapsed_s = 10.003
ops_per_sec = 2514418
p50_us = 399
p99_us = 703
p999_us = 4607
p9999_us = 18943
max_us = 19535
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 2

```
ops = 26751028
errors = 0
busy_retryable = 0
elapsed_s = 10.002
ops_per_sec = 2674701
p50_us = 367
p99_us = 639
p999_us = 4735
p9999_us = 20991
max_us = 21652
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 2

```
ops = 26837197
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2683404
p50_us = 367
p99_us = 607
p999_us = 4351
p9999_us = 16895
max_us = 19171
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 3

```
ops = 25925288
errors = 0
busy_retryable = 0
elapsed_s = 10.002
ops_per_sec = 2592137
p50_us = 367
p99_us = 735
p999_us = 4863
p9999_us = 21503
max_us = 22670
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 3

```
ops = 26092318
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2608894
p50_us = 375
p99_us = 735
p999_us = 5503
p9999_us = 17919
max_us = 30964
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 4

```
ops = 27257005
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2725326
p50_us = 359
p99_us = 623
p999_us = 2431
p9999_us = 19455
max_us = 20406
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 4

```
ops = 26519043
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2651585
p50_us = 367
p99_us = 607
p999_us = 4991
p9999_us = 21503
max_us = 21957
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 5

```
ops = 27176268
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2717234
p50_us = 359
p99_us = 687
p999_us = 991
p9999_us = 15359
max_us = 16025
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 5

```
ops = 25842513
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2583955
p50_us = 375
p99_us = 735
p999_us = 4479
p9999_us = 18943
max_us = 20490
```
