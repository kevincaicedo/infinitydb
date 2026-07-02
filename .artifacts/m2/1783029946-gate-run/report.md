# M2 gate-run report

date: 1783029946 (unix) · cells: 4 · replicates: 7 · duration: 10s
env-check: OK
tier: dev (non-binding)

notes:
- dev-tier run: reference-box gates report measured values, non-binding verdicts
- p99.9 deltas are quantized by LogHistogram (32 sub-buckets/octave ≈ 3%): 0.0% = same bucket; any non-zero delta spans ≥ 1 bucket
- M2 leg runs the as-shipped node assembly (no durable plane configured — infinityd durable wiring lands with the release stories); the zero-record assert on a durable-enabled node is the node_e2e mixed-class test
- server cells pinned: --pin-start 4 (same cpu set both legs)
- pipelined 1:10 (M0 gate mix): m1 2524999 ops/s (spread 10.39%) vs m2 2531672 ops/s (spread 6.45%) — signed ops delta +0.26% · p999 815 → 751 µs (-7.85%)
- unpipelined 512-conn (M0 gate mix): m1 742806 ops/s (spread 2.72%) vs m2 738477 ops/s (spread 2.73%) — signed ops delta -0.58% · p999 1215 → 1247 µs (+2.63%)
- ttl-heavy 1:1 writes (M1 gate mix): m1 2232235 ops/s (spread 34.78%) vs m2 2234624 ops/s (spread 4.61%) — signed ops delta +0.11% · p999 3839 → 4031 µs (+5.00%)

| gate | threshold | measured | verdict |
|---|---|---|---|
| Zero-cost A/B: pipelined ops regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | 0.00 | PASS (DEV-TIER, non-binding) |
| Zero-cost A/B: pipelined p99.9 regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | 0.00 | PASS (DEV-TIER, non-binding) |
| Zero-cost A/B: unpipelined 512-conn ops regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | 0.58 | PASS (DEV-TIER, non-binding) |
| Zero-cost A/B: unpipelined p99.9 regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | 2.63 | FAIL (DEV-TIER, non-binding) |
| Zero-cost A/B: ttl-heavy write-mix ops regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | 0.00 | PASS (DEV-TIER, non-binding) |
| Zero-cost A/B: ttl-heavy p99.9 regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | 5.00 | FAIL (DEV-TIER, non-binding) |
| Memory-only rows append zero log records | <= 0 records | 0.00 | PASS |
| everysec penalty vs memory mode | < 10 % | — | PENDING (tooling) |
| always grouped writes | >= 300000 w/s | — | PENDING (tooling) |
| Replay throughput per cell | >= 1 GB/s/cell | — | PENDING (tooling) |
| 10 GB node cold boot | < 15 s | — | PENDING (tooling) |
| DST durability oracle: 10k seeds | <= 0 violations | — | PENDING (tooling) |
| Crash matrix green in CI | <= 0 failures | — | PENDING (tooling) |
| Checkpoint under full load: foreground p99.9 (anti-BGREWRITEAOF) | < 2000 us | — | PENDING (tooling) |
| M0/M1 gates re-pass | <= 5 % vs M1 artifact | — | PENDING (tooling) |
| One log write per iteration | <= 1 writes/iter | — | PENDING (tooling) |
| acks/fsync grouping ratio above floor | >= 2 acks per fsync | — | PENDING (tooling) |
| sum(domains) vs RSS divergence (with log domains) | <= 10 % | — | PENDING (tooling) |

## pipelined 1:10 (M0 gate mix) m1-baseline rep 0

```
ops = 23184567
errors = 0
elapsed_s = 10.001
ops_per_sec = 2318164
p50_us = 439
p99_us = 767
p999_us = 895
p9999_us = 10751
max_us = 11945
```

## pipelined 1:10 (M0 gate mix) m2 rep 0

```
ops = 26174445
errors = 0
elapsed_s = 10.001
ops_per_sec = 2617102
p50_us = 383
p99_us = 719
p999_us = 799
p9999_us = 10495
max_us = 10985
```

## pipelined 1:10 (M0 gate mix) m2 rep 1

```
ops = 25319996
errors = 0
elapsed_s = 10.001
ops_per_sec = 2531672
p50_us = 407
p99_us = 639
p999_us = 735
p9999_us = 815
max_us = 1719
```

## pipelined 1:10 (M0 gate mix) m1-baseline rep 1

```
ops = 25253811
errors = 0
elapsed_s = 10.002
ops_per_sec = 2524999
p50_us = 399
p99_us = 735
p999_us = 831
p9999_us = 943
max_us = 2054
```

## pipelined 1:10 (M0 gate mix) m1-baseline rep 2

```
ops = 24749546
errors = 0
elapsed_s = 10.001
ops_per_sec = 2474635
p50_us = 407
p99_us = 735
p999_us = 831
p9999_us = 927
max_us = 4011
```

## pipelined 1:10 (M0 gate mix) m2 rep 2

```
ops = 25301179
errors = 0
elapsed_s = 10.001
ops_per_sec = 2529797
p50_us = 407
p99_us = 655
p999_us = 735
p9999_us = 831
max_us = 1948
```

## pipelined 1:10 (M0 gate mix) m2 rep 3

```
ops = 24842726
errors = 0
elapsed_s = 10.001
ops_per_sec = 2483952
p50_us = 415
p99_us = 751
p999_us = 863
p9999_us = 959
max_us = 3767
```

## pipelined 1:10 (M0 gate mix) m1-baseline rep 3

```
ops = 25808671
errors = 0
elapsed_s = 10.001
ops_per_sec = 2580576
p50_us = 399
p99_us = 607
p999_us = 687
p9999_us = 767
max_us = 2017
```

## pipelined 1:10 (M0 gate mix) m1-baseline rep 4

```
ops = 25457246
errors = 0
elapsed_s = 10.001
ops_per_sec = 2545435
p50_us = 399
p99_us = 687
p999_us = 767
p9999_us = 863
max_us = 3152
```

## pipelined 1:10 (M0 gate mix) m2 rep 4

```
ops = 25326940
errors = 0
elapsed_s = 10.001
ops_per_sec = 2532405
p50_us = 399
p99_us = 687
p999_us = 751
p9999_us = 879
max_us = 4059
```

## pipelined 1:10 (M0 gate mix) m2 rep 5

```
ops = 25420047
errors = 0
elapsed_s = 10.001
ops_per_sec = 2541717
p50_us = 407
p99_us = 623
p999_us = 703
p9999_us = 783
max_us = 1490
```

## pipelined 1:10 (M0 gate mix) m1-baseline rep 5

```
ops = 24036543
errors = 0
elapsed_s = 10.001
ops_per_sec = 2403354
p50_us = 423
p99_us = 735
p999_us = 815
p9999_us = 943
max_us = 3470
```

## pipelined 1:10 (M0 gate mix) m1-baseline rep 6

```
ops = 25268708
errors = 0
elapsed_s = 10.001
ops_per_sec = 2526558
p50_us = 415
p99_us = 703
p999_us = 815
p9999_us = 895
max_us = 3514
```

## pipelined 1:10 (M0 gate mix) m2 rep 6

```
ops = 24541537
errors = 0
elapsed_s = 10.001
ops_per_sec = 2453813
p50_us = 431
p99_us = 735
p999_us = 879
p9999_us = 975
max_us = 2566
```

## unpipelined 512-conn (M0 gate mix) m1-baseline rep 0

```
ops = 3758182
errors = 0
elapsed_s = 5.008
ops_per_sec = 750498
p50_us = 655
p99_us = 1183
p999_us = 1471
p9999_us = 3391
max_us = 6245
```

## unpipelined 512-conn (M0 gate mix) m2 rep 0

```
ops = 3732016
errors = 0
elapsed_s = 5.008
ops_per_sec = 745218
p50_us = 671
p99_us = 1119
p999_us = 1311
p9999_us = 3391
max_us = 4437
```

## unpipelined 512-conn (M0 gate mix) m2 rep 1

```
ops = 3695684
errors = 0
elapsed_s = 5.008
ops_per_sec = 738006
p50_us = 687
p99_us = 1119
p999_us = 1183
p9999_us = 1695
max_us = 3362
```

## unpipelined 512-conn (M0 gate mix) m1-baseline rep 1

```
ops = 3710701
errors = 0
elapsed_s = 5.008
ops_per_sec = 741018
p50_us = 671
p99_us = 1119
p999_us = 1279
p9999_us = 1791
max_us = 4241
```

## unpipelined 512-conn (M0 gate mix) m1-baseline rep 2

```
ops = 3719342
errors = 0
elapsed_s = 5.007
ops_per_sec = 742806
p50_us = 671
p99_us = 1087
p999_us = 1183
p9999_us = 1503
max_us = 4131
```

## unpipelined 512-conn (M0 gate mix) m2 rep 2

```
ops = 3698018
errors = 0
elapsed_s = 5.008
ops_per_sec = 738477
p50_us = 687
p99_us = 1087
p999_us = 1247
p9999_us = 1727
max_us = 4394
```

## unpipelined 512-conn (M0 gate mix) m2 rep 3

```
ops = 3704668
errors = 0
elapsed_s = 5.009
ops_per_sec = 739547
p50_us = 671
p99_us = 1119
p999_us = 1247
p9999_us = 1599
max_us = 3992
```

## unpipelined 512-conn (M0 gate mix) m1-baseline rep 3

```
ops = 3732650
errors = 0
elapsed_s = 5.007
ops_per_sec = 745456
p50_us = 671
p99_us = 1119
p999_us = 1183
p9999_us = 1663
max_us = 4826
```

## unpipelined 512-conn (M0 gate mix) m1-baseline rep 4

```
ops = 3734224
errors = 0
elapsed_s = 5.010
ops_per_sec = 745386
p50_us = 671
p99_us = 1119
p999_us = 1183
p9999_us = 1471
max_us = 4731
```

## unpipelined 512-conn (M0 gate mix) m2 rep 4

```
ops = 3697386
errors = 0
elapsed_s = 5.008
ops_per_sec = 738332
p50_us = 687
p99_us = 1087
p999_us = 1215
p9999_us = 1599
max_us = 4272
```

## unpipelined 512-conn (M0 gate mix) m2 rep 5

```
ops = 3699105
errors = 0
elapsed_s = 5.007
ops_per_sec = 738783
p50_us = 687
p99_us = 1087
p999_us = 1183
p9999_us = 1631
max_us = 4152
```

## unpipelined 512-conn (M0 gate mix) m1-baseline rep 5

```
ops = 3657480
errors = 0
elapsed_s = 5.008
ops_per_sec = 730394
p50_us = 687
p99_us = 1151
p999_us = 1215
p9999_us = 1535
max_us = 4155
```

## unpipelined 512-conn (M0 gate mix) m1-baseline rep 6

```
ops = 3656813
errors = 0
elapsed_s = 5.007
ops_per_sec = 730280
p50_us = 687
p99_us = 1119
p999_us = 1279
p9999_us = 1695
max_us = 4135
```

## unpipelined 512-conn (M0 gate mix) m2 rep 6

```
ops = 3630542
errors = 0
elapsed_s = 5.007
ops_per_sec = 725077
p50_us = 687
p99_us = 1151
p999_us = 1343
p9999_us = 1791
max_us = 4169
```

## ttl-heavy 1:1 writes (M1 gate mix) m1-baseline rep 0

```
ops = 15174140
errors = 0
elapsed_s = 10.001
ops_per_sec = 1517210
p50_us = 799
p99_us = 1023
p999_us = 7423
p9999_us = 10751
max_us = 15044
```

## ttl-heavy 1:1 writes (M1 gate mix) m2 rep 0

```
ops = 22273249
errors = 0
elapsed_s = 10.001
ops_per_sec = 2227029
p50_us = 439
p99_us = 799
p999_us = 4735
p9999_us = 13311
max_us = 14222
```

## ttl-heavy 1:1 writes (M1 gate mix) m2 rep 1

```
ops = 22507868
errors = 0
elapsed_s = 10.001
ops_per_sec = 2250513
p50_us = 439
p99_us = 783
p999_us = 4223
p9999_us = 14079
max_us = 15081
```

## ttl-heavy 1:1 writes (M1 gate mix) m1-baseline rep 1

```
ops = 22497905
errors = 0
elapsed_s = 10.001
ops_per_sec = 2249529
p50_us = 439
p99_us = 735
p999_us = 6527
p9999_us = 17407
max_us = 19236
```

## ttl-heavy 1:1 writes (M1 gate mix) m1-baseline rep 2

```
ops = 22937855
errors = 0
elapsed_s = 10.001
ops_per_sec = 2293484
p50_us = 431
p99_us = 671
p999_us = 3455
p9999_us = 15615
max_us = 15996
```

## ttl-heavy 1:1 writes (M1 gate mix) m2 rep 2

```
ops = 21834332
errors = 0
elapsed_s = 10.001
ops_per_sec = 2183180
p50_us = 431
p99_us = 863
p999_us = 4031
p9999_us = 16127
max_us = 18356
```

## ttl-heavy 1:1 writes (M1 gate mix) m2 rep 3

```
ops = 22864420
errors = 0
elapsed_s = 10.001
ops_per_sec = 2286197
p50_us = 439
p99_us = 703
p999_us = 2175
p9999_us = 14335
max_us = 19644
```

## ttl-heavy 1:1 writes (M1 gate mix) m1-baseline rep 3

```
ops = 22324635
errors = 0
elapsed_s = 10.001
ops_per_sec = 2232235
p50_us = 447
p99_us = 751
p999_us = 3839
p9999_us = 16895
max_us = 18479
```

## ttl-heavy 1:1 writes (M1 gate mix) m1-baseline rep 4

```
ops = 22187009
errors = 0
elapsed_s = 10.001
ops_per_sec = 2218442
p50_us = 463
p99_us = 751
p999_us = 3455
p9999_us = 15359
max_us = 19066
```

## ttl-heavy 1:1 writes (M1 gate mix) m2 rep 4

```
ops = 22358504
errors = 0
elapsed_s = 10.001
ops_per_sec = 2235582
p50_us = 439
p99_us = 751
p999_us = 3711
p9999_us = 16127
max_us = 19984
```

## ttl-heavy 1:1 writes (M1 gate mix) m2 rep 5

```
ops = 21897929
errors = 0
elapsed_s = 10.001
ops_per_sec = 2189494
p50_us = 455
p99_us = 815
p999_us = 1695
p9999_us = 13823
max_us = 14538
```

## ttl-heavy 1:1 writes (M1 gate mix) m1-baseline rep 5

```
ops = 21663613
errors = 0
elapsed_s = 10.001
ops_per_sec = 2166137
p50_us = 455
p99_us = 863
p999_us = 3903
p9999_us = 16895
max_us = 18252
```

## ttl-heavy 1:1 writes (M1 gate mix) m1-baseline rep 6

```
ops = 22643347
errors = 0
elapsed_s = 10.001
ops_per_sec = 2264087
p50_us = 431
p99_us = 751
p999_us = 3391
p9999_us = 15615
max_us = 21965
```

## ttl-heavy 1:1 writes (M1 gate mix) m2 rep 6

```
ops = 22348635
errors = 0
elapsed_s = 10.001
ops_per_sec = 2234624
p50_us = 439
p99_us = 767
p999_us = 4031
p9999_us = 16895
max_us = 23325
```
