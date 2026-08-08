# M4 gate-run report

date: 1784424616 (unix) · cells: 4 · duration: 10s · replicates: 5 · degenerate-case A/B (M4-S03; hard sub-gate, re-run at week-4 risk gate + S24)
env-check: OK
tier: reference-box (binding)

notes:
- p99.9 deltas are quantized by LogHistogram (32 sub-buckets/octave ≈ 3%): 0.0% = same bucket; any non-zero delta spans ≥ 1 bucket
- m4 binary target/release/infinityd: hash64:1240b1a264960461 (10593368 bytes)
- m3 baseline /tmp/claude-1000/-home-kcaicedo-Documents-Projects-databases/146c45b6-8d4c-463b-a1e1-345fa4ab8b4c/scratchpad/m3-baseline/target/release/infinityd: hash64:60afaf32c23bce09 (10479640 bytes) — pin this fingerprint across the week-4 and S24 re-runs; the commit it was built from is recorded in the ledger row (C15 lesson)
- server cells pinned: --pin-start 4 (same cpu set both legs)
- pipelined 1:10 (M0 gate mix): m3 2883205 ops/s (spread 2.18%) vs m4 2890736 ops/s (spread 1.98%) — signed ops delta +0.26% · p999 671 → 671 µs (+0.00%) · peak-RSS 188018688 → 188518400 B (+0.27%)
- unpipelined 512-conn (M0 gate mix): m3 734130 ops/s (spread 5.34%) vs m4 736409 ops/s (spread 1.23%) — signed ops delta +0.31% · p999 1279 → 1311 µs (+2.50%) · peak-RSS 120246272 → 120532992 B (+0.24%)
- ttl-heavy 1:1 writes (M1 gate mix): m3 2425874 ops/s (spread 2.09%) vs m4 2421935 ops/s (spread 6.71%) — signed ops delta -0.16% · p999 4607 → 4607 µs (+0.00%) · peak-RSS 237137920 → 238092288 B (+0.40%)

| gate | threshold | measured | verdict |
|---|---|---|---|
| Degenerate A/B: pipelined ops regression | <= 1 % vs M3 baseline | 0.00 | PASS |
| Degenerate A/B: pipelined p99.9 regression | <= 1 % vs M3 baseline (LogHistogram ~3% buckets: nonzero spans >= 1 bucket) | 0.00 | PASS |
| Degenerate A/B: unpipelined ops regression | <= 1 % vs M3 baseline | 0.00 | PASS |
| Degenerate A/B: unpipelined p99.9 regression | <= 1 % vs M3 baseline | 2.50 | FAIL |
| Degenerate A/B: ttl-heavy ops regression | <= 1 % vs M3 baseline | 0.16 | PASS |
| Degenerate A/B: ttl-heavy p99.9 regression | <= 1 % vs M3 baseline | 0.00 | PASS |
| Degenerate A/B: peak-RSS regression (worst row) | <= 1 % vs M3 baseline | 0.40 | PASS |
| Memory-mode node constructs zero tiered tables | <= 0 tables | 0.00 | PASS |
| Tiering code-path counters identically zero | <= 0 counter sum | 0.00 | PASS |
| Memory-only rows append zero log records (M2 posture carried) | <= 0 records | 0.00 | PASS |

## pipelined 1:10 (M0 gate mix) m3-baseline rep 0

```
ops = 29158607
errors = 0
elapsed_s = 10.001
ops_per_sec = 2915535
p50_us = 343
p99_us = 591
p999_us = 703
p9999_us = 10751
max_us = 11811
```

## pipelined 1:10 (M0 gate mix) m4 rep 0

```
ops = 29086876
errors = 0
elapsed_s = 10.001
ops_per_sec = 2908303
p50_us = 351
p99_us = 607
p999_us = 687
p9999_us = 10495
max_us = 10982
```

## pipelined 1:10 (M0 gate mix) m4 rep 1

```
ops = 28978917
errors = 0
elapsed_s = 10.001
ops_per_sec = 2897517
p50_us = 351
p99_us = 591
p999_us = 671
p9999_us = 799
max_us = 2996
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 1

```
ops = 28835055
errors = 0
elapsed_s = 10.001
ops_per_sec = 2883205
p50_us = 359
p99_us = 559
p999_us = 639
p9999_us = 767
max_us = 3323
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 2

```
ops = 28763273
errors = 0
elapsed_s = 10.001
ops_per_sec = 2876014
p50_us = 359
p99_us = 559
p999_us = 655
p9999_us = 815
max_us = 3727
```

## pipelined 1:10 (M0 gate mix) m4 rep 2

```
ops = 28513354
errors = 0
elapsed_s = 10.001
ops_per_sec = 2851045
p50_us = 359
p99_us = 655
p999_us = 751
p9999_us = 895
max_us = 4118
```

## pipelined 1:10 (M0 gate mix) m4 rep 3

```
ops = 28907835
errors = 0
elapsed_s = 10.001
ops_per_sec = 2890455
p50_us = 351
p99_us = 575
p999_us = 655
p9999_us = 751
max_us = 3739
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 3

```
ops = 28849791
errors = 0
elapsed_s = 10.001
ops_per_sec = 2884680
p50_us = 351
p99_us = 591
p999_us = 671
p9999_us = 799
max_us = 3340
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 4

```
ops = 28529018
errors = 0
elapsed_s = 10.001
ops_per_sec = 2852585
p50_us = 351
p99_us = 655
p999_us = 719
p9999_us = 815
max_us = 1747
```

## pipelined 1:10 (M0 gate mix) m4 rep 4

```
ops = 28910437
errors = 0
elapsed_s = 10.001
ops_per_sec = 2890736
p50_us = 351
p99_us = 575
p999_us = 655
p9999_us = 751
max_us = 2481
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 0

```
ops = 3751188
errors = 0
elapsed_s = 5.007
ops_per_sec = 749116
p50_us = 671
p99_us = 1247
p999_us = 1407
p9999_us = 3455
max_us = 4235
```

## unpipelined 512-conn (M0 gate mix) m4 rep 0

```
ops = 3698037
errors = 0
elapsed_s = 5.008
ops_per_sec = 738384
p50_us = 687
p99_us = 1119
p999_us = 1311
p9999_us = 3455
max_us = 5252
```

## unpipelined 512-conn (M0 gate mix) m4 rep 1

```
ops = 3663886
errors = 0
elapsed_s = 5.008
ops_per_sec = 731603
p50_us = 687
p99_us = 1151
p999_us = 1247
p9999_us = 1503
max_us = 3616
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 1

```
ops = 3600979
errors = 0
elapsed_s = 5.008
ops_per_sec = 719089
p50_us = 703
p99_us = 1151
p999_us = 1247
p9999_us = 1631
max_us = 3937
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 2

```
ops = 3555172
errors = 0
elapsed_s = 5.008
ops_per_sec = 709939
p50_us = 703
p99_us = 1183
p999_us = 1343
p9999_us = 2239
max_us = 4931
```

## unpipelined 512-conn (M0 gate mix) m4 rep 2

```
ops = 3651914
errors = 0
elapsed_s = 5.007
ops_per_sec = 729321
p50_us = 703
p99_us = 1151
p999_us = 1311
p9999_us = 1663
max_us = 4229
```

## unpipelined 512-conn (M0 gate mix) m4 rep 3

```
ops = 3687514
errors = 0
elapsed_s = 5.007
ops_per_sec = 736409
p50_us = 687
p99_us = 1151
p999_us = 1311
p9999_us = 1503
max_us = 3945
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 3

```
ops = 3676729
errors = 0
elapsed_s = 5.008
ops_per_sec = 734130
p50_us = 687
p99_us = 1151
p999_us = 1279
p9999_us = 2303
max_us = 4584
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 4

```
ops = 3676733
errors = 0
elapsed_s = 5.008
ops_per_sec = 734199
p50_us = 687
p99_us = 1183
p999_us = 1279
p9999_us = 1535
max_us = 4217
```

## unpipelined 512-conn (M0 gate mix) m4 rep 4

```
ops = 3691558
errors = 0
elapsed_s = 5.007
ops_per_sec = 737247
p50_us = 687
p99_us = 1119
p999_us = 1183
p9999_us = 1471
max_us = 4011
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 0

```
ops = 24261384
errors = 0
elapsed_s = 10.001
ops_per_sec = 2425874
p50_us = 399
p99_us = 751
p999_us = 4607
p9999_us = 17407
max_us = 18674
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 0

```
ops = 23395576
errors = 0
elapsed_s = 10.001
ops_per_sec = 2339229
p50_us = 423
p99_us = 751
p999_us = 5375
p9999_us = 16895
max_us = 17477
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 1

```
ops = 25020494
errors = 0
elapsed_s = 10.001
ops_per_sec = 2501769
p50_us = 391
p99_us = 639
p999_us = 2623
p9999_us = 18431
max_us = 18761
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 1

```
ops = 24139991
errors = 0
elapsed_s = 10.001
ops_per_sec = 2413714
p50_us = 407
p99_us = 687
p999_us = 4735
p9999_us = 17919
max_us = 20057
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 2

```
ops = 24647285
errors = 0
elapsed_s = 10.001
ops_per_sec = 2464380
p50_us = 399
p99_us = 687
p999_us = 2175
p9999_us = 18431
max_us = 18693
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 2

```
ops = 24222007
errors = 0
elapsed_s = 10.001
ops_per_sec = 2421935
p50_us = 399
p99_us = 719
p999_us = 4607
p9999_us = 18431
max_us = 22419
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 3

```
ops = 24494639
errors = 0
elapsed_s = 10.001
ops_per_sec = 2449127
p50_us = 399
p99_us = 735
p999_us = 4479
p9999_us = 18431
max_us = 28056
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 3

```
ops = 24501596
errors = 0
elapsed_s = 10.001
ops_per_sec = 2449884
p50_us = 399
p99_us = 655
p999_us = 4735
p9999_us = 18431
max_us = 20573
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 4

```
ops = 24240879
errors = 0
elapsed_s = 10.001
ops_per_sec = 2423789
p50_us = 407
p99_us = 735
p999_us = 2495
p9999_us = 18431
max_us = 19001
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 4

```
ops = 23514036
errors = 0
elapsed_s = 10.001
ops_per_sec = 2351093
p50_us = 431
p99_us = 719
p999_us = 4607
p9999_us = 17919
max_us = 23906
```
