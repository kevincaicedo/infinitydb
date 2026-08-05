# M4 gate-run report

date: 1785479546 (unix) · cells: 4 · duration: 10s · replicates: 6 · degenerate-case A/B (M4-S03; hard sub-gate, re-run at week-4 risk gate + S24)
env-check: FAILED (overridden — NOT citation-grade)
tier: dev (non-binding)

notes:
- env-check FAILED and was overridden (--unsafe-env): not citation-grade
- dev-tier run: reference-box gates report measured values, non-binding verdicts — the degenerate-case verdict binds on the reference box (week-4 risk gate + S24)
- p99.9 deltas are quantized by LogHistogram (32 sub-buckets/octave ≈ 3%): 0.0% = same bucket; any non-zero delta spans ≥ 1 bucket
- m4 binary target/release/infinityd: hash64:7a460e51973ec4c9 (10979968 bytes)
- m3 baseline target/release/infinityd: hash64:7a460e51973ec4c9 (10979968 bytes) — pin this fingerprint across the week-4 and S24 re-runs; the commit it was built from is recorded in the ledger row (C15 lesson)
- slot crossover active (week-4 instrument fix): servers respawn per replicate and the binary↔slot assignment alternates; legs run in spawn order so slot + load-order bias cancels in the leg medians over an even replicate count
- pipelined 1:10 (M0 gate mix): m3 4466445 ops/s (spread 6.68%) vs m4 4498649 ops/s (spread 22.54%) — signed ops delta +0.72% · p999 1151 → 1119 µs (-2.78%) · peak-RSS 190881792 → 190914560 B (+0.02%)
- unpipelined 512-conn (M0 gate mix): m3 1145696 ops/s (spread 3.41%) vs m4 1143861 ops/s (spread 0.55%) — signed ops delta -0.16% · p999 2687 → 2559 µs (-4.76%) · peak-RSS 134668288 → 134561792 B (-0.08%)
- ttl-heavy 1:1 writes (M1 gate mix): m3 3669435 ops/s (spread 5.62%) vs m4 3675111 ops/s (spread 3.07%) — signed ops delta +0.15% · p999 2239 → 2175 µs (-2.86%) · peak-RSS 269381632 → 270688256 B (+0.49%)

| gate | threshold | measured | verdict |
|---|---|---|---|
| Degenerate A/B: pipelined ops regression | <= 1 % vs M3 baseline | 0.00 | PASS (DEV-TIER, non-binding) |
| Degenerate A/B: pipelined p99.9 regression | <= 1 % vs M3 baseline (LogHistogram ~3% buckets: nonzero spans >= 1 bucket) | 0.00 | PASS (DEV-TIER, non-binding) |
| Degenerate A/B: unpipelined ops regression | <= 1 % vs M3 baseline | 0.16 | PASS (DEV-TIER, non-binding) |
| Degenerate A/B: unpipelined p99.9 regression | <= 1 % vs M3 baseline | 0.00 | PASS (DEV-TIER, non-binding) |
| Degenerate A/B: ttl-heavy ops regression | <= 1 % vs M3 baseline | 0.00 | PASS (DEV-TIER, non-binding) |
| Degenerate A/B: ttl-heavy p99.9 regression | <= 1 % vs M3 baseline | 0.00 | PASS (DEV-TIER, non-binding) |
| Degenerate A/B: peak-RSS regression (worst row) | <= 1 % vs M3 baseline | 0.49 | PASS (DEV-TIER, non-binding) |
| Memory-mode node constructs zero tiered tables | <= 0 tables | 0.00 | PASS |
| Tiering code-path counters identically zero | <= 0 counter sum | 0.00 | PASS |
| Write amplification, worst tiered namespace | < 3 x user bytes (wal + flush) | — | PENDING (tooling) |
| Memory-only rows append zero log records (M2 posture carried) | <= 0 records | 0.00 | PASS |
| Mixed-node attribution divergence (M4-S20) | <= 10 pct, worst continuous sample | — | PENDING (tooling) |
| Cache-namespace p99 isolation under the mixed node (M4-S20) | <= 10 pct vs same-campaign solo baseline | — | PENDING (tooling) |

## write amplification by row

Per namespace, worst first — never a node-wide blend (M4-S16).

| row | write amplification |
|---|---|
| pipelined 1:10 (M0 gate mix) | n/a (no tiered namespace on the node — memory-mode row) · blob: n/a (no blob activity) |
| unpipelined 512-conn (M0 gate mix) | n/a (no tiered namespace on the node — memory-mode row) · blob: n/a (no blob activity) |
| ttl-heavy 1:1 writes (M1 gate mix) | n/a (no tiered namespace on the node — memory-mode row) · blob: n/a (no blob activity) |

## pipelined 1:10 (M0 gate mix) m4 rep 0

```
ops = 44989934
errors = 0
elapsed_s = 10.001
ops_per_sec = 4498649
p50_us = 219
p99_us = 415
p999_us = 1119
p9999_us = 4479
max_us = 9482
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 0

```
ops = 45627700
errors = 0
elapsed_s = 10.001
ops_per_sec = 4562391
p50_us = 219
p99_us = 391
p999_us = 1023
p9999_us = 4223
max_us = 8739
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 1

```
ops = 44610636
errors = 0
elapsed_s = 10.001
ops_per_sec = 4460697
p50_us = 215
p99_us = 439
p999_us = 1151
p9999_us = 4223
max_us = 9475
```

## pipelined 1:10 (M0 gate mix) m4 rep 1

```
ops = 44853967
errors = 0
elapsed_s = 10.001
ops_per_sec = 4485083
p50_us = 215
p99_us = 423
p999_us = 1119
p9999_us = 4479
max_us = 9014
```

## pipelined 1:10 (M0 gate mix) m4 rep 2

```
ops = 45784244
errors = 0
elapsed_s = 10.001
ops_per_sec = 4578097
p50_us = 219
p99_us = 367
p999_us = 959
p9999_us = 4095
max_us = 9277
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 2

```
ops = 42644410
errors = 0
elapsed_s = 10.001
ops_per_sec = 4264102
p50_us = 255
p99_us = 447
p999_us = 1183
p9999_us = 4607
max_us = 8705
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 3

```
ops = 44668830
errors = 0
elapsed_s = 10.001
ops_per_sec = 4466445
p50_us = 219
p99_us = 431
p999_us = 1151
p9999_us = 4223
max_us = 9134
```

## pipelined 1:10 (M0 gate mix) m4 rep 3

```
ops = 35642931
errors = 0
elapsed_s = 10.001
ops_per_sec = 3563989
p50_us = 335
p99_us = 511
p999_us = 1567
p9999_us = 8063
max_us = 8833
```

## pipelined 1:10 (M0 gate mix) m4 rep 4

```
ops = 45655074
errors = 0
elapsed_s = 10.001
ops_per_sec = 4565124
p50_us = 219
p99_us = 383
p999_us = 991
p9999_us = 4351
max_us = 9030
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 4

```
ops = 44445004
errors = 0
elapsed_s = 10.001
ops_per_sec = 4444016
p50_us = 223
p99_us = 415
p999_us = 1247
p9999_us = 4351
max_us = 8730
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 5

```
ops = 44885927
errors = 0
elapsed_s = 10.001
ops_per_sec = 4488214
p50_us = 223
p99_us = 399
p999_us = 1151
p9999_us = 4351
max_us = 9072
```

## pipelined 1:10 (M0 gate mix) m4 rep 5

```
ops = 41557984
errors = 0
elapsed_s = 10.001
ops_per_sec = 4155475
p50_us = 243
p99_us = 479
p999_us = 1471
p9999_us = 7807
max_us = 8908
```

## unpipelined 512-conn (M0 gate mix) m4 rep 0

```
ops = 5712187
errors = 0
elapsed_s = 5.006
ops_per_sec = 1141100
p50_us = 439
p99_us = 783
p999_us = 2559
p9999_us = 4735
max_us = 6119
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 0

```
ops = 5765578
errors = 0
elapsed_s = 5.005
ops_per_sec = 1152026
p50_us = 431
p99_us = 783
p999_us = 2751
p9999_us = 4607
max_us = 5393
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 1

```
ops = 5766511
errors = 0
elapsed_s = 5.005
ops_per_sec = 1152224
p50_us = 431
p99_us = 767
p999_us = 2559
p9999_us = 4607
max_us = 5322
```

## unpipelined 512-conn (M0 gate mix) m4 rep 1

```
ops = 5719860
errors = 0
elapsed_s = 5.006
ops_per_sec = 1142666
p50_us = 439
p99_us = 767
p999_us = 2367
p9999_us = 4991
max_us = 7044
```

## unpipelined 512-conn (M0 gate mix) m4 rep 2

```
ops = 5732293
errors = 0
elapsed_s = 5.005
ops_per_sec = 1145293
p50_us = 431
p99_us = 783
p999_us = 2559
p9999_us = 4735
max_us = 5690
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 2

```
ops = 5691267
errors = 0
elapsed_s = 5.005
ops_per_sec = 1137117
p50_us = 439
p99_us = 767
p999_us = 2559
p9999_us = 4863
max_us = 8980
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 3

```
ops = 5658932
errors = 0
elapsed_s = 5.005
ops_per_sec = 1130648
p50_us = 447
p99_us = 783
p999_us = 2751
p9999_us = 4991
max_us = 7375
```

## unpipelined 512-conn (M0 gate mix) m4 rep 3

```
ops = 5724862
errors = 0
elapsed_s = 5.005
ops_per_sec = 1143861
p50_us = 439
p99_us = 767
p999_us = 2751
p9999_us = 4991
max_us = 7873
```

## unpipelined 512-conn (M0 gate mix) m4 rep 4

```
ops = 5740789
errors = 0
elapsed_s = 5.006
ops_per_sec = 1146810
p50_us = 431
p99_us = 783
p999_us = 2751
p9999_us = 4991
max_us = 6519
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 4

```
ops = 5571612
errors = 0
elapsed_s = 5.005
ops_per_sec = 1113141
p50_us = 455
p99_us = 783
p999_us = 2175
p9999_us = 4607
max_us = 5635
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 5

```
ops = 5735253
errors = 0
elapsed_s = 5.006
ops_per_sec = 1145696
p50_us = 439
p99_us = 767
p999_us = 2687
p9999_us = 6143
max_us = 11048
```

## unpipelined 512-conn (M0 gate mix) m4 rep 5

```
ops = 5707995
errors = 0
elapsed_s = 5.005
ops_per_sec = 1140512
p50_us = 439
p99_us = 767
p999_us = 2111
p9999_us = 4607
max_us = 5315
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 0

```
ops = 36615786
errors = 0
elapsed_s = 10.001
ops_per_sec = 3661311
p50_us = 263
p99_us = 559
p999_us = 2239
p9999_us = 19967
max_us = 21008
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 0

```
ops = 36697424
errors = 0
elapsed_s = 10.001
ops_per_sec = 3669435
p50_us = 263
p99_us = 543
p999_us = 2047
p9999_us = 17407
max_us = 18431
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 1

```
ops = 36106750
errors = 0
elapsed_s = 10.001
ops_per_sec = 3610312
p50_us = 263
p99_us = 559
p999_us = 2239
p9999_us = 18943
max_us = 33790
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 1

```
ops = 37606475
errors = 0
elapsed_s = 10.001
ops_per_sec = 3760330
p50_us = 255
p99_us = 503
p999_us = 2015
p9999_us = 17919
max_us = 33107
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 2

```
ops = 37157319
errors = 0
elapsed_s = 10.001
ops_per_sec = 3715379
p50_us = 255
p99_us = 527
p999_us = 2175
p9999_us = 18943
max_us = 50968
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 2

```
ops = 35465114
errors = 0
elapsed_s = 10.001
ops_per_sec = 3546149
p50_us = 271
p99_us = 559
p999_us = 2239
p9999_us = 19967
max_us = 21086
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 3

```
ops = 35220116
errors = 0
elapsed_s = 10.001
ops_per_sec = 3521714
p50_us = 271
p99_us = 591
p999_us = 3071
p9999_us = 18431
max_us = 36156
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 3

```
ops = 36478846
errors = 0
elapsed_s = 10.001
ops_per_sec = 3647547
p50_us = 263
p99_us = 527
p999_us = 2015
p9999_us = 19967
max_us = 21554
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 4

```
ops = 36754313
errors = 0
elapsed_s = 10.001
ops_per_sec = 3675111
p50_us = 255
p99_us = 543
p999_us = 2111
p9999_us = 19455
max_us = 21278
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 4

```
ops = 36970956
errors = 0
elapsed_s = 10.001
ops_per_sec = 3696803
p50_us = 263
p99_us = 527
p999_us = 2047
p9999_us = 17919
max_us = 19448
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 5

```
ops = 37282464
errors = 0
elapsed_s = 10.001
ops_per_sec = 3727909
p50_us = 255
p99_us = 495
p999_us = 2111
p9999_us = 19455
max_us = 44869
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 5

```
ops = 36727638
errors = 0
elapsed_s = 10.001
ops_per_sec = 3672476
p50_us = 263
p99_us = 559
p999_us = 2175
p9999_us = 18943
max_us = 21127
```
