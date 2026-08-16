# M4 gate-run report

date: 1786843290 (unix) · cells: 4 · duration: 10s · replicates: 6 · degenerate-case A/B (M4-S03; hard sub-gate, re-run at week-4 risk gate + S24)
env-check: OK
tier: reference-box (binding)

notes:
- p99.9 deltas are quantized by LogHistogram (32 sub-buckets/octave ≈ 3%): 0.0% = same bucket; any non-zero delta spans ≥ 1 bucket
- m4 binary /home/kcaicedo/.cache/inf-campaign/v0.4.0-bin/infinityd-6bd25b1: hash64:1010d6649adf52a6 (12723224 bytes)
- m3 baseline /home/kcaicedo/.cache/inf-campaign/v0.4.0-bin/infinityd-6bd25b1: hash64:1010d6649adf52a6 (12723224 bytes) — pin this fingerprint across the week-4 and S24 re-runs; the commit it was built from is recorded in the ledger row (C15 lesson)
- server cells pinned: --pin-start 4 (same cpu set both legs)
- slot crossover active (week-4 instrument fix): servers respawn per replicate and the binary↔slot assignment alternates; legs run in spawn order so slot + load-order bias cancels in the leg medians over an even replicate count
- pipelined 1:10 (M0 gate mix): m3 3005134 ops/s (spread 5.79%) vs m4 3000821 ops/s (spread 6.54%) — signed ops delta -0.14% · p999 655 → 671 µs (+2.44%) · peak-RSS 188059648 → 188006400 B (-0.03%)
- unpipelined 512-conn (M0 gate mix): m3 783722 ops/s (spread 0.35%) vs m4 784925 ops/s (spread 0.95%) — signed ops delta +0.15% · p999 1279 → 1183 µs (-7.51%) · peak-RSS 119599104 → 119640064 B (+0.03%)
- ttl-heavy 1:1 writes (M1 gate mix): m3 2550042 ops/s (spread 4.42%) vs m4 2554367 ops/s (spread 2.51%) — signed ops delta +0.17% · p999 4479 → 4607 µs (+2.86%) · peak-RSS 239595520 → 240115712 B (+0.22%)

| gate | threshold | measured | verdict |
|---|---|---|---|
| Degenerate A/B: pipelined ops regression | <= 1 % vs M3 baseline | 0.14 | PASS |
| Degenerate A/B: pipelined p99.9 regression | <= 1 % vs M3 baseline (LogHistogram ~3% buckets: nonzero spans >= 1 bucket) | 2.44 | FAIL |
| Degenerate A/B: unpipelined ops regression | <= 1 % vs M3 baseline | 0.00 | PASS |
| Degenerate A/B: unpipelined p99.9 regression | <= 1 % vs M3 baseline | 0.00 | PASS |
| Degenerate A/B: ttl-heavy ops regression | <= 1 % vs M3 baseline | 0.00 | PASS |
| Degenerate A/B: ttl-heavy p99.9 regression | <= 1 % vs M3 baseline | 2.86 | FAIL |
| Degenerate A/B: peak-RSS regression (worst row) | <= 1 % vs M3 baseline | 0.22 | PASS |
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
ops = 30062427
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3005876
p50_us = 359
p99_us = 559
p999_us = 671
p9999_us = 10495
max_us = 11090
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 0

```
ops = 30054470
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3005134
p50_us = 343
p99_us = 559
p999_us = 639
p9999_us = 10495
max_us = 11199
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 1

```
ops = 29966388
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2996298
p50_us = 359
p99_us = 575
p999_us = 687
p9999_us = 10495
max_us = 11537
```

## pipelined 1:10 (M0 gate mix) m4 rep 1

```
ops = 28184935
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2818171
p50_us = 351
p99_us = 687
p999_us = 767
p9999_us = 10495
max_us = 11044
```

## pipelined 1:10 (M0 gate mix) m4 rep 2

```
ops = 30012031
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3000821
p50_us = 351
p99_us = 591
p999_us = 687
p9999_us = 10495
max_us = 11159
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 2

```
ops = 30158922
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3015507
p50_us = 335
p99_us = 559
p999_us = 623
p9999_us = 10495
max_us = 11258
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 3

```
ops = 28536752
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2853358
p50_us = 367
p99_us = 607
p999_us = 687
p9999_us = 10751
max_us = 11287
```

## pipelined 1:10 (M0 gate mix) m4 rep 3

```
ops = 29935669
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2993220
p50_us = 343
p99_us = 543
p999_us = 623
p9999_us = 10495
max_us = 21212
```

## pipelined 1:10 (M0 gate mix) m4 rep 4

```
ops = 30148891
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3014547
p50_us = 343
p99_us = 527
p999_us = 607
p9999_us = 10495
max_us = 11038
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 4

```
ops = 30020966
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3001688
p50_us = 335
p99_us = 575
p999_us = 655
p9999_us = 10495
max_us = 11050
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 5

```
ops = 30275524
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3027205
p50_us = 335
p99_us = 527
p999_us = 607
p9999_us = 10751
max_us = 11410
```

## pipelined 1:10 (M0 gate mix) m4 rep 5

```
ops = 29934083
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2993056
p50_us = 335
p99_us = 607
p999_us = 671
p9999_us = 10495
max_us = 11286
```

## unpipelined 512-conn (M0 gate mix) m4 rep 0

```
ops = 3897807
errors = 0
busy_retryable = 0
elapsed_s = 5.007
ops_per_sec = 778425
p50_us = 639
p99_us = 1055
p999_us = 1279
p9999_us = 3327
max_us = 4375
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 0

```
ops = 3921383
errors = 0
busy_retryable = 0
elapsed_s = 5.008
ops_per_sec = 783026
p50_us = 639
p99_us = 1055
p999_us = 1311
p9999_us = 3327
max_us = 4189
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 1

```
ops = 3927051
errors = 0
busy_retryable = 0
elapsed_s = 5.008
ops_per_sec = 784198
p50_us = 639
p99_us = 1023
p999_us = 1183
p9999_us = 3327
max_us = 4106
```

## unpipelined 512-conn (M0 gate mix) m4 rep 1

```
ops = 3930419
errors = 0
busy_retryable = 0
elapsed_s = 5.007
ops_per_sec = 784925
p50_us = 639
p99_us = 1023
p999_us = 1183
p9999_us = 3391
max_us = 4209
```

## unpipelined 512-conn (M0 gate mix) m4 rep 2

```
ops = 3927212
errors = 0
busy_retryable = 0
elapsed_s = 5.008
ops_per_sec = 784234
p50_us = 639
p99_us = 1055
p999_us = 1183
p9999_us = 3327
max_us = 4387
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 2

```
ops = 3926180
errors = 0
busy_retryable = 0
elapsed_s = 5.009
ops_per_sec = 783776
p50_us = 639
p99_us = 1055
p999_us = 1279
p9999_us = 3455
max_us = 4259
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 3

```
ops = 3924145
errors = 0
busy_retryable = 0
elapsed_s = 5.007
ops_per_sec = 783722
p50_us = 639
p99_us = 1055
p999_us = 1215
p9999_us = 3391
max_us = 5127
```

## unpipelined 512-conn (M0 gate mix) m4 rep 3

```
ops = 3927898
errors = 0
busy_retryable = 0
elapsed_s = 5.007
ops_per_sec = 784416
p50_us = 639
p99_us = 1055
p999_us = 1215
p9999_us = 3391
max_us = 4217
```

## unpipelined 512-conn (M0 gate mix) m4 rep 4

```
ops = 3935791
errors = 0
busy_retryable = 0
elapsed_s = 5.008
ops_per_sec = 785879
p50_us = 639
p99_us = 1055
p999_us = 1183
p9999_us = 3327
max_us = 4236
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 4

```
ops = 3914573
errors = 0
busy_retryable = 0
elapsed_s = 5.009
ops_per_sec = 781432
p50_us = 639
p99_us = 1055
p999_us = 1279
p9999_us = 3455
max_us = 4567
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 5

```
ops = 3923135
errors = 0
busy_retryable = 0
elapsed_s = 5.008
ops_per_sec = 783442
p50_us = 639
p99_us = 1055
p999_us = 1183
p9999_us = 3327
max_us = 5524
```

## unpipelined 512-conn (M0 gate mix) m4 rep 5

```
ops = 3932282
errors = 0
busy_retryable = 0
elapsed_s = 5.007
ops_per_sec = 785280
p50_us = 639
p99_us = 1055
p999_us = 1183
p9999_us = 3391
max_us = 4540
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 0

```
ops = 25699091
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2569546
p50_us = 375
p99_us = 671
p999_us = 5247
p9999_us = 18943
max_us = 19409
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 0

```
ops = 25503718
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2550042
p50_us = 383
p99_us = 623
p999_us = 4223
p9999_us = 16895
max_us = 17680
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 1

```
ops = 25936698
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2593381
p50_us = 383
p99_us = 623
p999_us = 3327
p9999_us = 18431
max_us = 19318
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 1

```
ops = 25094484
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2509121
p50_us = 383
p99_us = 687
p999_us = 4607
p9999_us = 17919
max_us = 18488
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 2

```
ops = 25549403
errors = 0
busy_retryable = 0
elapsed_s = 10.002
ops_per_sec = 2554367
p50_us = 391
p99_us = 655
p999_us = 3071
p9999_us = 19455
max_us = 20669
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 2

```
ops = 25214790
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2521195
p50_us = 383
p99_us = 687
p999_us = 4735
p9999_us = 17919
max_us = 18784
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 3

```
ops = 25747809
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2574461
p50_us = 383
p99_us = 687
p999_us = 2815
p9999_us = 17919
max_us = 18551
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 3

```
ops = 25162336
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2515905
p50_us = 391
p99_us = 655
p999_us = 4479
p9999_us = 15615
max_us = 16840
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 4

```
ops = 25736602
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2573358
p50_us = 383
p99_us = 623
p999_us = 2815
p9999_us = 17919
max_us = 18671
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 4

```
ops = 24809040
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2480552
p50_us = 391
p99_us = 735
p999_us = 5119
p9999_us = 18431
max_us = 19154
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 5

```
ops = 25432571
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2542915
p50_us = 383
p99_us = 703
p999_us = 4479
p9999_us = 19455
max_us = 19903
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 5

```
ops = 25447985
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2544460
p50_us = 391
p99_us = 607
p999_us = 4607
p9999_us = 15359
max_us = 17877
```
