# M4 gate-run report

date: 1786416367 (unix) · cells: 4 · duration: 10s · replicates: 6 · degenerate-case A/B (M4-S03; hard sub-gate, re-run at week-4 risk gate + S24)
env-check: OK
tier: reference-box (binding)

notes:
- p99.9 deltas are quantized by LogHistogram (32 sub-buckets/octave ≈ 3%): 0.0% = same bucket; any non-zero delta spans ≥ 1 bucket
- m4 binary /home/kcaicedo/.cache/inf-campaign/v0.4.0-bin/infinityd-m4: hash64:2b50b03e54378a6f (12722784 bytes)
- m3 baseline /home/kcaicedo/.cache/inf-campaign/v0.4.0-bin/infinityd-m3-a1ebcb9: hash64:60afaf32c23bce09 (10479640 bytes) — pin this fingerprint across the week-4 and S24 re-runs; the commit it was built from is recorded in the ledger row (C15 lesson)
- server cells pinned: --pin-start 4 (same cpu set both legs)
- slot crossover active (week-4 instrument fix): servers respawn per replicate and the binary↔slot assignment alternates; legs run in spawn order so slot + load-order bias cancels in the leg medians over an even replicate count
- pipelined 1:10 (M0 gate mix): m3 3126957 ops/s (spread 2.07%) vs m4 3136711 ops/s (spread 2.27%) — signed ops delta +0.31% · p999 671 → 703 µs (+4.77%) · peak-RSS 188063744 → 188383232 B (+0.17%)
- unpipelined 512-conn (M0 gate mix): m3 790251 ops/s (spread 2.22%) vs m4 792290 ops/s (spread 2.33%) — signed ops delta +0.26% · p999 1343 → 1407 µs (+4.77%) · peak-RSS 119427072 → 119713792 B (+0.24%)
- ttl-heavy 1:1 writes (M1 gate mix): m3 2679021 ops/s (spread 5.18%) vs m4 2715656 ops/s (spread 11.30%) — signed ops delta +1.37% · p999 5119 → 4351 µs (-15.00%) · peak-RSS 242503680 → 244117504 B (+0.67%)

| gate | threshold | measured | verdict |
|---|---|---|---|
| Degenerate A/B: pipelined ops regression | <= 1 % vs M3 baseline | 0.00 | PASS |
| Degenerate A/B: pipelined p99.9 regression | <= 1 % vs M3 baseline (LogHistogram ~3% buckets: nonzero spans >= 1 bucket) | 4.77 | FAIL |
| Degenerate A/B: unpipelined ops regression | <= 1 % vs M3 baseline | 0.00 | PASS |
| Degenerate A/B: unpipelined p99.9 regression | <= 1 % vs M3 baseline | 4.77 | FAIL |
| Degenerate A/B: ttl-heavy ops regression | <= 1 % vs M3 baseline | 0.00 | PASS |
| Degenerate A/B: ttl-heavy p99.9 regression | <= 1 % vs M3 baseline | 0.00 | PASS |
| Degenerate A/B: peak-RSS regression (worst row) | <= 1 % vs M3 baseline | 0.67 | PASS |
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
ops = 31371554
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3136711
p50_us = 343
p99_us = 591
p999_us = 703
p9999_us = 10495
max_us = 11392
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 0

```
ops = 31370088
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3136583
p50_us = 319
p99_us = 543
p999_us = 655
p9999_us = 10239
max_us = 11297
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 1

```
ops = 31160800
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3115732
p50_us = 319
p99_us = 559
p999_us = 655
p9999_us = 10495
max_us = 11586
```

## pipelined 1:10 (M0 gate mix) m4 rep 1

```
ops = 31007727
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3100387
p50_us = 335
p99_us = 607
p999_us = 703
p9999_us = 11007
max_us = 11965
```

## pipelined 1:10 (M0 gate mix) m4 rep 2

```
ops = 31042262
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3103836
p50_us = 335
p99_us = 607
p999_us = 719
p9999_us = 10495
max_us = 11537
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 2

```
ops = 31131063
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3112710
p50_us = 343
p99_us = 591
p999_us = 687
p9999_us = 10239
max_us = 11397
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 3

```
ops = 31026498
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3102213
p50_us = 327
p99_us = 559
p999_us = 671
p9999_us = 10239
max_us = 11109
```

## pipelined 1:10 (M0 gate mix) m4 rep 3

```
ops = 31445136
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3144101
p50_us = 319
p99_us = 543
p999_us = 655
p9999_us = 10239
max_us = 11072
```

## pipelined 1:10 (M0 gate mix) m4 rep 4

```
ops = 30854241
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3085015
p50_us = 319
p99_us = 607
p999_us = 719
p9999_us = 10239
max_us = 11372
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 4

```
ops = 31273936
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3126957
p50_us = 319
p99_us = 591
p999_us = 703
p9999_us = 10239
max_us = 10776
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 5

```
ops = 31673413
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3166977
p50_us = 319
p99_us = 527
p999_us = 623
p9999_us = 10239
max_us = 10642
```

## pipelined 1:10 (M0 gate mix) m4 rep 5

```
ops = 31567063
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3156316
p50_us = 319
p99_us = 559
p999_us = 671
p9999_us = 10239
max_us = 10923
```

## unpipelined 512-conn (M0 gate mix) m4 rep 0

```
ops = 3964411
errors = 0
busy_retryable = 0
elapsed_s = 5.008
ops_per_sec = 791613
p50_us = 623
p99_us = 1055
p999_us = 1503
p9999_us = 3327
max_us = 5052
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 0

```
ops = 3982435
errors = 0
busy_retryable = 0
elapsed_s = 5.010
ops_per_sec = 794907
p50_us = 623
p99_us = 1023
p999_us = 1375
p9999_us = 4351
max_us = 9338
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 1

```
ops = 3894281
errors = 0
busy_retryable = 0
elapsed_s = 5.009
ops_per_sec = 777399
p50_us = 655
p99_us = 1087
p999_us = 1471
p9999_us = 3199
max_us = 4454
```

## unpipelined 512-conn (M0 gate mix) m4 rep 1

```
ops = 3885855
errors = 0
busy_retryable = 0
elapsed_s = 5.010
ops_per_sec = 775587
p50_us = 639
p99_us = 1087
p999_us = 1407
p9999_us = 3263
max_us = 5061
```

## unpipelined 512-conn (M0 gate mix) m4 rep 2

```
ops = 3977607
errors = 0
busy_retryable = 0
elapsed_s = 5.009
ops_per_sec = 794056
p50_us = 623
p99_us = 1023
p999_us = 1407
p9999_us = 3263
max_us = 4148
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 2

```
ops = 3958954
errors = 0
busy_retryable = 0
elapsed_s = 5.010
ops_per_sec = 790230
p50_us = 623
p99_us = 1055
p999_us = 1343
p9999_us = 3327
max_us = 4670
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 3

```
ops = 3964431
errors = 0
busy_retryable = 0
elapsed_s = 5.008
ops_per_sec = 791592
p50_us = 623
p99_us = 1023
p999_us = 1343
p9999_us = 3263
max_us = 4102
```

## unpipelined 512-conn (M0 gate mix) m4 rep 3

```
ops = 3973562
errors = 0
busy_retryable = 0
elapsed_s = 5.010
ops_per_sec = 793145
p50_us = 623
p99_us = 1023
p999_us = 1247
p9999_us = 3135
max_us = 4052
```

## unpipelined 512-conn (M0 gate mix) m4 rep 4

```
ops = 3902420
errors = 0
busy_retryable = 0
elapsed_s = 5.008
ops_per_sec = 779168
p50_us = 639
p99_us = 1055
p999_us = 1503
p9999_us = 3839
max_us = 6030
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 4

```
ops = 3899776
errors = 0
busy_retryable = 0
elapsed_s = 5.010
ops_per_sec = 778438
p50_us = 639
p99_us = 1055
p999_us = 1215
p9999_us = 3199
max_us = 4120
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 5

```
ops = 3958744
errors = 0
busy_retryable = 0
elapsed_s = 5.009
ops_per_sec = 790251
p50_us = 623
p99_us = 1023
p999_us = 1247
p9999_us = 3327
max_us = 4277
```

## unpipelined 512-conn (M0 gate mix) m4 rep 5

```
ops = 3967854
errors = 0
busy_retryable = 0
elapsed_s = 5.008
ops_per_sec = 792290
p50_us = 623
p99_us = 1023
p999_us = 1247
p9999_us = 3263
max_us = 3998
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 0

```
ops = 27596533
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2759326
p50_us = 359
p99_us = 575
p999_us = 1599
p9999_us = 17919
max_us = 18303
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 0

```
ops = 26044715
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2604110
p50_us = 367
p99_us = 671
p999_us = 5119
p9999_us = 19455
max_us = 21036
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 1

```
ops = 27113509
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2710986
p50_us = 359
p99_us = 639
p999_us = 2111
p9999_us = 16895
max_us = 17898
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 1

```
ops = 26160441
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2615665
p50_us = 375
p99_us = 655
p999_us = 4351
p9999_us = 15871
max_us = 23303
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 2

```
ops = 27160264
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2715656
p50_us = 359
p99_us = 607
p999_us = 3519
p9999_us = 20479
max_us = 21530
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 2

```
ops = 26420748
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2641728
p50_us = 367
p99_us = 655
p999_us = 5119
p9999_us = 20479
max_us = 22271
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 3

```
ops = 27237161
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2723402
p50_us = 359
p99_us = 639
p999_us = 1247
p9999_us = 16383
max_us = 17415
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 3

```
ops = 25515451
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2551227
p50_us = 383
p99_us = 687
p999_us = 5247
p9999_us = 20479
max_us = 21828
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 4

```
ops = 27458306
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2745461
p50_us = 359
p99_us = 591
p999_us = 1887
p9999_us = 17407
max_us = 18225
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 4

```
ops = 25848550
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2584509
p50_us = 383
p99_us = 671
p999_us = 5119
p9999_us = 18943
max_us = 19943
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 5

```
ops = 26793723
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2679021
p50_us = 367
p99_us = 671
p999_us = 3647
p9999_us = 17919
max_us = 18620
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 5

```
ops = 24527423
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2452431
p50_us = 399
p99_us = 719
p999_us = 5759
p9999_us = 17919
max_us = 21195
```
