# M4 gate-run report

date: 1784424976 (unix) · cells: 4 · duration: 10s · replicates: 5 · degenerate-case A/B (M4-S03; hard sub-gate, re-run at week-4 risk gate + S24)
env-check: OK
tier: reference-box (binding)

notes:
- p99.9 deltas are quantized by LogHistogram (32 sub-buckets/octave ≈ 3%): 0.0% = same bucket; any non-zero delta spans ≥ 1 bucket
- m4 binary target/release/infinityd: hash64:1240b1a264960461 (10593368 bytes)
- m3 baseline target/release/infinityd: hash64:1240b1a264960461 (10593368 bytes) — pin this fingerprint across the week-4 and S24 re-runs; the commit it was built from is recorded in the ledger row (C15 lesson)
- server cells pinned: --pin-start 4 (same cpu set both legs)
- pipelined 1:10 (M0 gate mix): m3 2942059 ops/s (spread 1.49%) vs m4 2919775 ops/s (spread 3.04%) — signed ops delta -0.76% · p999 639 → 687 µs (+7.51%) · peak-RSS 188510208 → 188710912 B (+0.11%)
- unpipelined 512-conn (M0 gate mix): m3 718350 ops/s (spread 0.72%) vs m4 724912 ops/s (spread 2.64%) — signed ops delta +0.91% · p999 1279 → 1311 µs (+2.50%) · peak-RSS 119324672 → 119402496 B (+0.07%)
- ttl-heavy 1:1 writes (M1 gate mix): m3 2342790 ops/s (spread 47.59%) vs m4 2448879 ops/s (spread 4.97%) — signed ops delta +4.53% · p999 12287 → 4095 µs (-66.67%) · peak-RSS 236814336 → 237744128 B (+0.39%)

| gate | threshold | measured | verdict |
|---|---|---|---|
| Degenerate A/B: pipelined ops regression | <= 1 % vs M3 baseline | 0.76 | PASS |
| Degenerate A/B: pipelined p99.9 regression | <= 1 % vs M3 baseline (LogHistogram ~3% buckets: nonzero spans >= 1 bucket) | 7.51 | FAIL |
| Degenerate A/B: unpipelined ops regression | <= 1 % vs M3 baseline | 0.00 | PASS |
| Degenerate A/B: unpipelined p99.9 regression | <= 1 % vs M3 baseline | 2.50 | FAIL |
| Degenerate A/B: ttl-heavy ops regression | <= 1 % vs M3 baseline | 0.00 | PASS |
| Degenerate A/B: ttl-heavy p99.9 regression | <= 1 % vs M3 baseline | 0.00 | PASS |
| Degenerate A/B: peak-RSS regression (worst row) | <= 1 % vs M3 baseline | 0.39 | PASS |
| Memory-mode node constructs zero tiered tables | <= 0 tables | 0.00 | PASS |
| Tiering code-path counters identically zero | <= 0 counter sum | 0.00 | PASS |
| Memory-only rows append zero log records (M2 posture carried) | <= 0 records | 0.00 | PASS |

## pipelined 1:10 (M0 gate mix) m3-baseline rep 0

```
ops = 29194924
errors = 0
elapsed_s = 10.001
ops_per_sec = 2919163
p50_us = 359
p99_us = 623
p999_us = 735
p9999_us = 10495
max_us = 11428
```

## pipelined 1:10 (M0 gate mix) m4 rep 0

```
ops = 29266281
errors = 0
elapsed_s = 10.001
ops_per_sec = 2926265
p50_us = 343
p99_us = 607
p999_us = 687
p9999_us = 10751
max_us = 11452
```

## pipelined 1:10 (M0 gate mix) m4 rep 1

```
ops = 28424069
errors = 0
elapsed_s = 10.001
ops_per_sec = 2842092
p50_us = 367
p99_us = 655
p999_us = 735
p9999_us = 863
max_us = 3783
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 1

```
ops = 29091892
errors = 0
elapsed_s = 10.001
ops_per_sec = 2908881
p50_us = 343
p99_us = 639
p999_us = 719
p9999_us = 847
max_us = 4126
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 2

```
ops = 29434717
errors = 0
elapsed_s = 10.001
ops_per_sec = 2943118
p50_us = 351
p99_us = 543
p999_us = 623
p9999_us = 703
max_us = 2265
```

## pipelined 1:10 (M0 gate mix) m4 rep 2

```
ops = 29083494
errors = 0
elapsed_s = 10.001
ops_per_sec = 2908049
p50_us = 351
p99_us = 607
p999_us = 719
p9999_us = 847
max_us = 3774
```

## pipelined 1:10 (M0 gate mix) m4 rep 3

```
ops = 29201310
errors = 0
elapsed_s = 10.001
ops_per_sec = 2919775
p50_us = 351
p99_us = 575
p999_us = 655
p9999_us = 767
max_us = 2783
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 3

```
ops = 29531073
errors = 0
elapsed_s = 10.001
ops_per_sec = 2952724
p50_us = 351
p99_us = 527
p999_us = 607
p9999_us = 671
max_us = 2731
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 4

```
ops = 29424073
errors = 0
elapsed_s = 10.001
ops_per_sec = 2942059
p50_us = 351
p99_us = 559
p999_us = 639
p9999_us = 783
max_us = 3823
```

## pipelined 1:10 (M0 gate mix) m4 rep 4

```
ops = 29313054
errors = 0
elapsed_s = 10.001
ops_per_sec = 2930960
p50_us = 351
p99_us = 559
p999_us = 639
p9999_us = 783
max_us = 4628
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 0

```
ops = 3618253
errors = 0
elapsed_s = 5.007
ops_per_sec = 722596
p50_us = 687
p99_us = 1119
p999_us = 1343
p9999_us = 3455
max_us = 4375
```

## unpipelined 512-conn (M0 gate mix) m4 rep 0

```
ops = 3633178
errors = 0
elapsed_s = 5.008
ops_per_sec = 725524
p50_us = 687
p99_us = 1151
p999_us = 1311
p9999_us = 3391
max_us = 4334
```

## unpipelined 512-conn (M0 gate mix) m4 rep 1

```
ops = 3595423
errors = 0
elapsed_s = 5.008
ops_per_sec = 717979
p50_us = 703
p99_us = 1151
p999_us = 1311
p9999_us = 2175
max_us = 4202
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 1

```
ops = 3596433
errors = 0
elapsed_s = 5.007
ops_per_sec = 718217
p50_us = 703
p99_us = 1151
p999_us = 1279
p9999_us = 1631
max_us = 3822
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 2

```
ops = 3596977
errors = 0
elapsed_s = 5.007
ops_per_sec = 718350
p50_us = 703
p99_us = 1151
p999_us = 1279
p9999_us = 1663
max_us = 4461
```

## unpipelined 512-conn (M0 gate mix) m4 rep 2

```
ops = 3592468
errors = 0
elapsed_s = 5.008
ops_per_sec = 717405
p50_us = 703
p99_us = 1151
p999_us = 1343
p9999_us = 2239
max_us = 6415
```

## unpipelined 512-conn (M0 gate mix) m4 rep 3

```
ops = 3629745
errors = 0
elapsed_s = 5.007
ops_per_sec = 724912
p50_us = 687
p99_us = 1119
p999_us = 1215
p9999_us = 1727
max_us = 4436
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 3

```
ops = 3618254
errors = 0
elapsed_s = 5.008
ops_per_sec = 722538
p50_us = 703
p99_us = 1119
p999_us = 1247
p9999_us = 1599
max_us = 3851
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 4

```
ops = 3592506
errors = 0
elapsed_s = 5.007
ops_per_sec = 717428
p50_us = 703
p99_us = 1151
p999_us = 1311
p9999_us = 2687
max_us = 5157
```

## unpipelined 512-conn (M0 gate mix) m4 rep 4

```
ops = 3688267
errors = 0
elapsed_s = 5.008
ops_per_sec = 736516
p50_us = 687
p99_us = 1119
p999_us = 1247
p9999_us = 1791
max_us = 4586
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 0

```
ops = 13165690
errors = 0
elapsed_s = 10.002
ops_per_sec = 1316367
p50_us = 783
p99_us = 1535
p999_us = 6527
p9999_us = 8191
max_us = 9411
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 0

```
ops = 24870059
errors = 0
elapsed_s = 10.001
ops_per_sec = 2486727
p50_us = 399
p99_us = 655
p999_us = 4095
p9999_us = 16383
max_us = 18110
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 1

```
ops = 23652605
errors = 0
elapsed_s = 10.001
ops_per_sec = 2364942
p50_us = 415
p99_us = 783
p999_us = 2431
p9999_us = 16895
max_us = 42594
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 1

```
ops = 24161429
errors = 0
elapsed_s = 10.001
ops_per_sec = 2415801
p50_us = 399
p99_us = 671
p999_us = 11263
p9999_us = 14847
max_us = 18035
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 2

```
ops = 24316311
errors = 0
elapsed_s = 10.001
ops_per_sec = 2431310
p50_us = 399
p99_us = 639
p999_us = 13055
p9999_us = 15103
max_us = 15710
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 2

```
ops = 24491532
errors = 0
elapsed_s = 10.001
ops_per_sec = 2448879
p50_us = 399
p99_us = 703
p999_us = 4095
p9999_us = 17919
max_us = 21833
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 3

```
ops = 24556360
errors = 0
elapsed_s = 10.001
ops_per_sec = 2455349
p50_us = 399
p99_us = 687
p999_us = 2495
p9999_us = 17407
max_us = 30080
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 3

```
ops = 23431037
errors = 0
elapsed_s = 10.001
ops_per_sec = 2342790
p50_us = 407
p99_us = 751
p999_us = 12287
p9999_us = 15103
max_us = 22754
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 4

```
ops = 23429182
errors = 0
elapsed_s = 10.001
ops_per_sec = 2342661
p50_us = 407
p99_us = 767
p999_us = 12543
p9999_us = 14847
max_us = 16314
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 4

```
ops = 24180943
errors = 0
elapsed_s = 10.001
ops_per_sec = 2417802
p50_us = 407
p99_us = 671
p999_us = 4351
p9999_us = 19455
max_us = 34534
```
