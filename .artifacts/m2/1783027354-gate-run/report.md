# M2 gate-run report

date: 1783027354 (unix) · cells: 4 · replicates: 7 · duration: 10s
env-check: OK
tier: dev (non-binding)

notes:
- dev-tier run: reference-box gates report measured values, non-binding verdicts
- p99.9 deltas are quantized by LogHistogram (32 sub-buckets/octave ≈ 3%): 0.0% = same bucket; any non-zero delta spans ≥ 1 bucket
- M2 leg runs the as-shipped node assembly (no durable plane configured — infinityd durable wiring lands with the release stories); the zero-record assert on a durable-enabled node is the node_e2e mixed-class test
- server cells pinned: --pin-start 4 (same cpu set both legs)
- pipelined 1:10 (M0 gate mix): m1 2482201 ops/s (spread 57.05%) vs m2 2496575 ops/s (spread 7.70%) · p999 831 → 783 µs
- unpipelined 512-conn (M0 gate mix): m1 746446 ops/s (spread 2.39%) vs m2 755556 ops/s (spread 1.75%) · p999 1311 → 1215 µs
- ttl-heavy 1:1 writes (M1 gate mix): m1 2210922 ops/s (spread 35.55%) vs m2 2235186 ops/s (spread 11.98%) · p999 11775 → 4479 µs

| gate | threshold | measured | verdict |
|---|---|---|---|
| Zero-cost A/B: pipelined ops delta | <= 1 % vs M1 build | 0.58 | PASS (DEV-TIER, non-binding) |
| Zero-cost A/B: pipelined p99.9 delta | <= 1 % vs M1 build | 5.78 | FAIL (DEV-TIER, non-binding) |
| Zero-cost A/B: unpipelined 512-conn ops delta | <= 1 % vs M1 build | 1.22 | FAIL (DEV-TIER, non-binding) |
| Zero-cost A/B: unpipelined p99.9 delta | <= 1 % vs M1 build | 7.32 | FAIL (DEV-TIER, non-binding) |
| Zero-cost A/B: ttl-heavy write-mix ops delta | <= 1 % vs M1 build | 1.10 | FAIL (DEV-TIER, non-binding) |
| Zero-cost A/B: ttl-heavy p99.9 delta | <= 1 % vs M1 build | 61.96 | FAIL (DEV-TIER, non-binding) |
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
ops = 11018160
errors = 0
elapsed_s = 10.001
ops_per_sec = 1101673
p50_us = 863
p99_us = 1375
p999_us = 2879
p9999_us = 5887
max_us = 6271
```

## pipelined 1:10 (M0 gate mix) m2 rep 0

```
ops = 25375290
errors = 0
elapsed_s = 10.001
ops_per_sec = 2537239
p50_us = 407
p99_us = 687
p999_us = 783
p9999_us = 10239
max_us = 11867
```

## pipelined 1:10 (M0 gate mix) m2 rep 1

```
ops = 24969104
errors = 0
elapsed_s = 10.001
ops_per_sec = 2496575
p50_us = 407
p99_us = 655
p999_us = 751
p9999_us = 895
max_us = 2009
```

## pipelined 1:10 (M0 gate mix) m1-baseline rep 1

```
ops = 24824681
errors = 0
elapsed_s = 10.001
ops_per_sec = 2482201
p50_us = 407
p99_us = 751
p999_us = 831
p9999_us = 10495
max_us = 20752
```

## pipelined 1:10 (M0 gate mix) m1-baseline rep 2

```
ops = 25103048
errors = 0
elapsed_s = 10.001
ops_per_sec = 2510027
p50_us = 407
p99_us = 687
p999_us = 767
p9999_us = 911
max_us = 3197
```

## pipelined 1:10 (M0 gate mix) m2 rep 2

```
ops = 24066267
errors = 0
elapsed_s = 10.001
ops_per_sec = 2406321
p50_us = 431
p99_us = 735
p999_us = 815
p9999_us = 927
max_us = 1844
```

## pipelined 1:10 (M0 gate mix) m2 rep 3

```
ops = 24518903
errors = 0
elapsed_s = 10.001
ops_per_sec = 2451576
p50_us = 415
p99_us = 719
p999_us = 863
p9999_us = 959
max_us = 3409
```

## pipelined 1:10 (M0 gate mix) m1-baseline rep 3

```
ops = 25180241
errors = 0
elapsed_s = 10.001
ops_per_sec = 2517693
p50_us = 407
p99_us = 655
p999_us = 735
p9999_us = 847
max_us = 4231
```

## pipelined 1:10 (M0 gate mix) m1-baseline rep 4

```
ops = 24824181
errors = 0
elapsed_s = 10.001
ops_per_sec = 2482146
p50_us = 431
p99_us = 703
p999_us = 847
p9999_us = 943
max_us = 3276
```

## pipelined 1:10 (M0 gate mix) m2 rep 4

```
ops = 25048938
errors = 0
elapsed_s = 10.001
ops_per_sec = 2504606
p50_us = 407
p99_us = 687
p999_us = 751
p9999_us = 863
max_us = 3257
```

## pipelined 1:10 (M0 gate mix) m2 rep 5

```
ops = 24969458
errors = 0
elapsed_s = 10.001
ops_per_sec = 2496653
p50_us = 407
p99_us = 687
p999_us = 767
p9999_us = 879
max_us = 3607
```

## pipelined 1:10 (M0 gate mix) m1-baseline rep 5

```
ops = 24389586
errors = 0
elapsed_s = 10.001
ops_per_sec = 2438647
p50_us = 423
p99_us = 735
p999_us = 831
p9999_us = 927
max_us = 2083
```

## pipelined 1:10 (M0 gate mix) m1-baseline rep 6

```
ops = 24886626
errors = 0
elapsed_s = 10.001
ops_per_sec = 2488332
p50_us = 407
p99_us = 735
p999_us = 815
p9999_us = 927
max_us = 2953
```

## pipelined 1:10 (M0 gate mix) m2 rep 6

```
ops = 23452284
errors = 0
elapsed_s = 10.001
ops_per_sec = 2344977
p50_us = 463
p99_us = 735
p999_us = 879
p9999_us = 991
max_us = 3442
```

## unpipelined 512-conn (M0 gate mix) m1-baseline rep 0

```
ops = 3694759
errors = 0
elapsed_s = 5.008
ops_per_sec = 737841
p50_us = 719
p99_us = 1215
p999_us = 1471
p9999_us = 3391
max_us = 5208
```

## unpipelined 512-conn (M0 gate mix) m2 rep 0

```
ops = 3745684
errors = 0
elapsed_s = 5.007
ops_per_sec = 748024
p50_us = 671
p99_us = 1119
p999_us = 1375
p9999_us = 3391
max_us = 4306
```

## unpipelined 512-conn (M0 gate mix) m2 rep 1

```
ops = 3729219
errors = 0
elapsed_s = 5.007
ops_per_sec = 744784
p50_us = 687
p99_us = 1087
p999_us = 1215
p9999_us = 1599
max_us = 4277
```

## unpipelined 512-conn (M0 gate mix) m1-baseline rep 1

```
ops = 3683509
errors = 0
elapsed_s = 5.008
ops_per_sec = 735567
p50_us = 719
p99_us = 1183
p999_us = 1343
p9999_us = 1631
max_us = 4391
```

## unpipelined 512-conn (M0 gate mix) m1-baseline rep 2

```
ops = 3706597
errors = 0
elapsed_s = 5.007
ops_per_sec = 740248
p50_us = 719
p99_us = 1183
p999_us = 1375
p9999_us = 1695
max_us = 4457
```

## unpipelined 512-conn (M0 gate mix) m2 rep 2

```
ops = 3759873
errors = 0
elapsed_s = 5.009
ops_per_sec = 750556
p50_us = 671
p99_us = 1087
p999_us = 1247
p9999_us = 1695
max_us = 4238
```

## unpipelined 512-conn (M0 gate mix) m2 rep 3

```
ops = 3790424
errors = 0
elapsed_s = 5.009
ops_per_sec = 756786
p50_us = 655
p99_us = 1087
p999_us = 1183
p9999_us = 1567
max_us = 4184
```

## unpipelined 512-conn (M0 gate mix) m1-baseline rep 3

```
ops = 3737793
errors = 0
elapsed_s = 5.007
ops_per_sec = 746446
p50_us = 719
p99_us = 1151
p999_us = 1311
p9999_us = 1695
max_us = 4301
```

## unpipelined 512-conn (M0 gate mix) m1-baseline rep 4

```
ops = 3772312
errors = 0
elapsed_s = 5.007
ops_per_sec = 753395
p50_us = 735
p99_us = 1119
p999_us = 1279
p9999_us = 1567
max_us = 4344
```

## unpipelined 512-conn (M0 gate mix) m2 rep 4

```
ops = 3795847
errors = 0
elapsed_s = 5.008
ops_per_sec = 757988
p50_us = 671
p99_us = 1055
p999_us = 1151
p9999_us = 1439
max_us = 4171
```

## unpipelined 512-conn (M0 gate mix) m2 rep 5

```
ops = 3786203
errors = 0
elapsed_s = 5.007
ops_per_sec = 756151
p50_us = 655
p99_us = 1087
p999_us = 1215
p9999_us = 1599
max_us = 3878
```

## unpipelined 512-conn (M0 gate mix) m1-baseline rep 5

```
ops = 3750491
errors = 0
elapsed_s = 5.007
ops_per_sec = 749071
p50_us = 703
p99_us = 1151
p999_us = 1311
p9999_us = 1599
max_us = 4424
```

## unpipelined 512-conn (M0 gate mix) m1-baseline rep 6

```
ops = 3743796
errors = 0
elapsed_s = 5.007
ops_per_sec = 747715
p50_us = 719
p99_us = 1151
p999_us = 1311
p9999_us = 1695
max_us = 4257
```

## unpipelined 512-conn (M0 gate mix) m2 rep 6

```
ops = 3783285
errors = 0
elapsed_s = 5.007
ops_per_sec = 755556
p50_us = 671
p99_us = 1087
p999_us = 1215
p9999_us = 1535
max_us = 4344
```

## ttl-heavy 1:1 writes (M1 gate mix) m1-baseline rep 0

```
ops = 14731114
errors = 0
elapsed_s = 10.001
ops_per_sec = 1472949
p50_us = 703
p99_us = 1119
p999_us = 7039
p9999_us = 8703
max_us = 9757
```

## ttl-heavy 1:1 writes (M1 gate mix) m2 rep 0

```
ops = 22549809
errors = 0
elapsed_s = 10.001
ops_per_sec = 2254735
p50_us = 431
p99_us = 783
p999_us = 4479
p9999_us = 13055
max_us = 13774
```

## ttl-heavy 1:1 writes (M1 gate mix) m2 rep 1

```
ops = 19980940
errors = 0
elapsed_s = 10.001
ops_per_sec = 1997848
p50_us = 511
p99_us = 863
p999_us = 11263
p9999_us = 12543
max_us = 17581
```

## ttl-heavy 1:1 writes (M1 gate mix) m1-baseline rep 1

```
ops = 21396861
errors = 0
elapsed_s = 10.001
ops_per_sec = 2139416
p50_us = 479
p99_us = 767
p999_us = 10495
p9999_us = 12799
max_us = 24432
```

## ttl-heavy 1:1 writes (M1 gate mix) m1-baseline rep 2

```
ops = 22591321
errors = 0
elapsed_s = 10.001
ops_per_sec = 2258860
p50_us = 431
p99_us = 671
p999_us = 11775
p9999_us = 13311
max_us = 14212
```

## ttl-heavy 1:1 writes (M1 gate mix) m2 rep 2

```
ops = 22478696
errors = 0
elapsed_s = 10.001
ops_per_sec = 2247609
p50_us = 439
p99_us = 719
p999_us = 4479
p9999_us = 14591
max_us = 20762
```

## ttl-heavy 1:1 writes (M1 gate mix) m2 rep 3

```
ops = 22355248
errors = 0
elapsed_s = 10.002
ops_per_sec = 2235186
p50_us = 439
p99_us = 799
p999_us = 2623
p9999_us = 14847
max_us = 17641
```

## ttl-heavy 1:1 writes (M1 gate mix) m1-baseline rep 3

```
ops = 21012524
errors = 0
elapsed_s = 10.001
ops_per_sec = 2101015
p50_us = 487
p99_us = 847
p999_us = 11007
p9999_us = 15615
max_us = 27099
```

## ttl-heavy 1:1 writes (M1 gate mix) m1-baseline rep 4

```
ops = 22376127
errors = 0
elapsed_s = 10.001
ops_per_sec = 2237328
p50_us = 439
p99_us = 703
p999_us = 12031
p9999_us = 13311
max_us = 14412
```

## ttl-heavy 1:1 writes (M1 gate mix) m2 rep 4

```
ops = 22206397
errors = 0
elapsed_s = 10.001
ops_per_sec = 2220350
p50_us = 447
p99_us = 719
p999_us = 4479
p9999_us = 15615
max_us = 20747
```

## ttl-heavy 1:1 writes (M1 gate mix) m2 rep 5

```
ops = 22657672
errors = 0
elapsed_s = 10.001
ops_per_sec = 2265530
p50_us = 439
p99_us = 687
p999_us = 2175
p9999_us = 14591
max_us = 15333
```

## ttl-heavy 1:1 writes (M1 gate mix) m1-baseline rep 5

```
ops = 22111862
errors = 0
elapsed_s = 10.001
ops_per_sec = 2210922
p50_us = 447
p99_us = 751
p999_us = 11775
p9999_us = 15615
max_us = 27512
```

## ttl-heavy 1:1 writes (M1 gate mix) m1-baseline rep 6

```
ops = 22333135
errors = 0
elapsed_s = 10.001
ops_per_sec = 2233018
p50_us = 439
p99_us = 719
p999_us = 12031
p9999_us = 13567
max_us = 14070
```

## ttl-heavy 1:1 writes (M1 gate mix) m2 rep 6

```
ops = 21919191
errors = 0
elapsed_s = 10.001
ops_per_sec = 2191646
p50_us = 439
p99_us = 831
p999_us = 4479
p9999_us = 15103
max_us = 21211
```
