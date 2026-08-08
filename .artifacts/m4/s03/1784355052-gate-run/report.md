# M4 gate-run report

date: 1784355052 (unix) · cells: 4 · duration: 8s · replicates: 3 · degenerate-case A/B (M4-S03; hard sub-gate, re-run at week-4 risk gate + S24)
env-check: FAILED (overridden — NOT citation-grade)
tier: dev (non-binding)

notes:
- env-check FAILED and was overridden (--unsafe-env): not citation-grade
- dev-tier run: reference-box gates report measured values, non-binding verdicts — the degenerate-case verdict binds on the reference box (week-4 risk gate + S24)
- p99.9 deltas are quantized by LogHistogram (32 sub-buckets/octave ≈ 3%): 0.0% = same bucket; any non-zero delta spans ≥ 1 bucket
- m4 binary target/release/infinityd: hash64:780f8d57dbc8ab4a (10538800 bytes)
- m3 baseline /tmp/claude-1000/-home-kcaicedo-Documents-Projects-databases/146c45b6-8d4c-463b-a1e1-345fa4ab8b4c/scratchpad/m3-baseline/target/release/infinityd: hash64:60afaf32c23bce09 (10479640 bytes) — pin this fingerprint across the week-4 and S24 re-runs; the commit it was built from is recorded in the ledger row (C15 lesson)
- pipelined 1:10 (M0 gate mix): m3 4288644 ops/s (spread 5.99%) vs m4 4271988 ops/s (spread 4.71%) — signed ops delta -0.39% · p999 815 → 1343 µs (+64.79%) · peak-RSS 189833216 → 189857792 B (+0.01%)
- unpipelined 512-conn (M0 gate mix): m3 1050378 ops/s (spread 1.35%) vs m4 1047061 ops/s (spread 4.15%) — signed ops delta -0.32% · p999 2495 → 2559 µs (+2.57%) · peak-RSS 134660096 → 135245824 B (+0.43%)
- ttl-heavy 1:1 writes (M1 gate mix): m3 3446626 ops/s (spread 5.31%) vs m4 3484483 ops/s (spread 5.37%) — signed ops delta +1.10% · p999 2559 → 2303 µs (-10.00%) · peak-RSS 269393920 → 264642560 B (-1.76%)

| gate | threshold | measured | verdict |
|---|---|---|---|
| Degenerate A/B: pipelined ops regression | <= 1 % vs M3 baseline | 0.39 | PASS (DEV-TIER, non-binding) |
| Degenerate A/B: pipelined p99.9 regression | <= 1 % vs M3 baseline (LogHistogram ~3% buckets: nonzero spans >= 1 bucket) | 64.79 | FAIL (DEV-TIER, non-binding) |
| Degenerate A/B: unpipelined ops regression | <= 1 % vs M3 baseline | 0.32 | PASS (DEV-TIER, non-binding) |
| Degenerate A/B: unpipelined p99.9 regression | <= 1 % vs M3 baseline | 2.57 | FAIL (DEV-TIER, non-binding) |
| Degenerate A/B: ttl-heavy ops regression | <= 1 % vs M3 baseline | 0.00 | PASS (DEV-TIER, non-binding) |
| Degenerate A/B: ttl-heavy p99.9 regression | <= 1 % vs M3 baseline | 0.00 | PASS (DEV-TIER, non-binding) |
| Degenerate A/B: peak-RSS regression (worst row) | <= 1 % vs M3 baseline | 0.43 | PASS (DEV-TIER, non-binding) |
| Memory-mode node constructs zero tiered tables | <= 0 tables | 0.00 | PASS |
| Tiering code-path counters identically zero | <= 0 counter sum | 0.00 | PASS |
| Memory-only rows append zero log records (M2 posture carried) | <= 0 records | 0.00 | PASS |

## pipelined 1:10 (M0 gate mix) m3-baseline rep 0

```
ops = 33144205
errors = 0
elapsed_s = 8.001
ops_per_sec = 4142546
p50_us = 239
p99_us = 471
p999_us = 1791
p9999_us = 8447
max_us = 10704
```

## pipelined 1:10 (M0 gate mix) m4 rep 0

```
ops = 34810047
errors = 0
elapsed_s = 8.001
ops_per_sec = 4350830
p50_us = 227
p99_us = 407
p999_us = 1343
p9999_us = 8703
max_us = 9188
```

## pipelined 1:10 (M0 gate mix) m4 rep 1

```
ops = 34179737
errors = 0
elapsed_s = 8.001
ops_per_sec = 4271988
p50_us = 231
p99_us = 431
p999_us = 1247
p9999_us = 2111
max_us = 3308
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 1

```
ops = 35200586
errors = 0
elapsed_s = 8.001
ops_per_sec = 4399482
p50_us = 227
p99_us = 391
p999_us = 815
p9999_us = 2239
max_us = 3800
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 2

```
ops = 34312877
errors = 0
elapsed_s = 8.001
ops_per_sec = 4288644
p50_us = 235
p99_us = 439
p999_us = 751
p9999_us = 1983
max_us = 4186
```

## pipelined 1:10 (M0 gate mix) m4 rep 2

```
ops = 33202845
errors = 0
elapsed_s = 8.001
ops_per_sec = 4149832
p50_us = 239
p99_us = 463
p999_us = 1471
p9999_us = 2431
max_us = 3990
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 0

```
ops = 5309074
errors = 0
elapsed_s = 5.005
ops_per_sec = 1060690
p50_us = 471
p99_us = 863
p999_us = 2495
p9999_us = 4607
max_us = 7666
```

## unpipelined 512-conn (M0 gate mix) m4 rep 0

```
ops = 5424415
errors = 0
elapsed_s = 5.006
ops_per_sec = 1083684
p50_us = 463
p99_us = 831
p999_us = 2559
p9999_us = 4607
max_us = 6805
```

## unpipelined 512-conn (M0 gate mix) m4 rep 1

```
ops = 5206857
errors = 0
elapsed_s = 5.006
ops_per_sec = 1040216
p50_us = 479
p99_us = 975
p999_us = 2751
p9999_us = 5375
max_us = 7555
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 1

```
ops = 5239396
errors = 0
elapsed_s = 5.006
ops_per_sec = 1046555
p50_us = 471
p99_us = 879
p999_us = 3135
p9999_us = 5247
max_us = 6006
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 2

```
ops = 5260127
errors = 0
elapsed_s = 5.008
ops_per_sec = 1050378
p50_us = 471
p99_us = 831
p999_us = 2303
p9999_us = 5119
max_us = 7749
```

## unpipelined 512-conn (M0 gate mix) m4 rep 2

```
ops = 5241748
errors = 0
elapsed_s = 5.006
ops_per_sec = 1047061
p50_us = 479
p99_us = 831
p999_us = 2239
p9999_us = 4735
max_us = 8162
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 0

```
ops = 28903904
errors = 0
elapsed_s = 8.001
ops_per_sec = 3612594
p50_us = 263
p99_us = 543
p999_us = 2559
p9999_us = 19455
max_us = 23440
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 0

```
ops = 27984100
errors = 0
elapsed_s = 8.001
ops_per_sec = 3497616
p50_us = 279
p99_us = 543
p999_us = 2303
p9999_us = 20991
max_us = 23505
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 1

```
ops = 26485916
errors = 0
elapsed_s = 8.001
ops_per_sec = 3310396
p50_us = 287
p99_us = 607
p999_us = 2623
p9999_us = 20479
max_us = 29919
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 1

```
ops = 27577542
errors = 0
elapsed_s = 8.001
ops_per_sec = 3446626
p50_us = 279
p99_us = 559
p999_us = 2623
p9999_us = 21503
max_us = 22771
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 2

```
ops = 27438584
errors = 0
elapsed_s = 8.001
ops_per_sec = 3429445
p50_us = 279
p99_us = 559
p999_us = 2175
p9999_us = 22015
max_us = 29055
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 2

```
ops = 27878896
errors = 0
elapsed_s = 8.001
ops_per_sec = 3484483
p50_us = 271
p99_us = 543
p999_us = 2111
p9999_us = 22015
max_us = 22900
```
