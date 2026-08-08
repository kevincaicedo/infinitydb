# M2 gate-run report

date: 1783026749 (unix) · cells: 4 · replicates: 5 · duration: 10s
env-check: OK
tier: dev (non-binding)

notes:
- dev-tier run: reference-box gates report measured values, non-binding verdicts
- p99.9 deltas are quantized by LogHistogram (32 sub-buckets/octave ≈ 3%): 0.0% = same bucket; any non-zero delta spans ≥ 1 bucket
- M2 leg runs the as-shipped node assembly (no durable plane configured — infinityd durable wiring lands with the release stories); the zero-record assert on a durable-enabled node is the node_e2e mixed-class test
- pipelined 1:10 (M0 gate mix): m1 2570498 ops/s (spread 5.32%) vs m2 2443533 ops/s (spread 10.98%) · p999 975 → 975 µs
- unpipelined 512-conn (M0 gate mix): m1 740698 ops/s (spread 2.49%) vs m2 740029 ops/s (spread 3.35%) · p999 1599 → 1567 µs
- ttl-heavy 1:1 writes (M1 gate mix): m1 2247665 ops/s (spread 4.40%) vs m2 2265092 ops/s (spread 2.18%) · p999 11263 → 5247 µs

| gate | threshold | measured | verdict |
|---|---|---|---|
| Zero-cost A/B: pipelined ops delta | <= 1 % vs M1 build | 4.94 | FAIL (DEV-TIER, non-binding) |
| Zero-cost A/B: pipelined p99.9 delta | <= 1 % vs M1 build | 0.00 | PASS (DEV-TIER, non-binding) |
| Zero-cost A/B: unpipelined 512-conn ops delta | <= 1 % vs M1 build | 0.09 | PASS (DEV-TIER, non-binding) |
| Zero-cost A/B: unpipelined p99.9 delta | <= 1 % vs M1 build | 2.00 | FAIL (DEV-TIER, non-binding) |
| Zero-cost A/B: ttl-heavy write-mix ops delta | <= 1 % vs M1 build | 0.78 | PASS (DEV-TIER, non-binding) |
| Zero-cost A/B: ttl-heavy p99.9 delta | <= 1 % vs M1 build | 53.41 | FAIL (DEV-TIER, non-binding) |
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
ops = 26370208
errors = 0
elapsed_s = 10.001
ops_per_sec = 2636712
p50_us = 375
p99_us = 719
p999_us = 975
p9999_us = 10495
max_us = 11247
```

## pipelined 1:10 (M0 gate mix) m2 rep 0

```
ops = 26449398
errors = 0
elapsed_s = 10.001
ops_per_sec = 2644620
p50_us = 399
p99_us = 719
p999_us = 879
p9999_us = 10239
max_us = 10691
```

## pipelined 1:10 (M0 gate mix) m2 rep 1

```
ops = 23766946
errors = 0
elapsed_s = 10.001
ops_per_sec = 2376415
p50_us = 439
p99_us = 735
p999_us = 975
p9999_us = 1503
max_us = 2861
```

## pipelined 1:10 (M0 gate mix) m1-baseline rep 1

```
ops = 25718321
errors = 0
elapsed_s = 10.001
ops_per_sec = 2571517
p50_us = 391
p99_us = 703
p999_us = 927
p9999_us = 1311
max_us = 2387
```

## pipelined 1:10 (M0 gate mix) m1-baseline rep 2

```
ops = 25707922
errors = 0
elapsed_s = 10.001
ops_per_sec = 2570498
p50_us = 391
p99_us = 671
p999_us = 895
p9999_us = 1439
max_us = 2562
```

## pipelined 1:10 (M0 gate mix) m2 rep 2

```
ops = 25145343
errors = 0
elapsed_s = 10.001
ops_per_sec = 2514251
p50_us = 399
p99_us = 703
p999_us = 975
p9999_us = 1471
max_us = 2281
```

## pipelined 1:10 (M0 gate mix) m2 rep 3

```
ops = 24366229
errors = 0
elapsed_s = 10.001
ops_per_sec = 2436358
p50_us = 407
p99_us = 767
p999_us = 1007
p9999_us = 1375
max_us = 1877
```

## pipelined 1:10 (M0 gate mix) m1-baseline rep 3

```
ops = 25001904
errors = 0
elapsed_s = 10.001
ops_per_sec = 2499908
p50_us = 391
p99_us = 783
p999_us = 1023
p9999_us = 1471
max_us = 1861
```

## pipelined 1:10 (M0 gate mix) m1-baseline rep 4

```
ops = 25505633
errors = 0
elapsed_s = 10.001
ops_per_sec = 2550275
p50_us = 391
p99_us = 719
p999_us = 1007
p9999_us = 1471
max_us = 1906
```

## pipelined 1:10 (M0 gate mix) m2 rep 4

```
ops = 24438078
errors = 0
elapsed_s = 10.001
ops_per_sec = 2443533
p50_us = 415
p99_us = 735
p999_us = 991
p9999_us = 1503
max_us = 2304
```

## unpipelined 512-conn (M0 gate mix) m1-baseline rep 0

```
ops = 3743789
errors = 0
elapsed_s = 5.007
ops_per_sec = 747736
p50_us = 671
p99_us = 1183
p999_us = 1631
p9999_us = 3455
max_us = 5191
```

## unpipelined 512-conn (M0 gate mix) m2 rep 0

```
ops = 3744069
errors = 0
elapsed_s = 5.007
ops_per_sec = 747720
p50_us = 671
p99_us = 1151
p999_us = 1567
p9999_us = 3519
max_us = 4582
```

## unpipelined 512-conn (M0 gate mix) m2 rep 1

```
ops = 3706226
errors = 0
elapsed_s = 5.008
ops_per_sec = 740029
p50_us = 671
p99_us = 1183
p999_us = 1567
p9999_us = 1983
max_us = 3761
```

## unpipelined 512-conn (M0 gate mix) m1-baseline rep 1

```
ops = 3708808
errors = 0
elapsed_s = 5.007
ops_per_sec = 740698
p50_us = 671
p99_us = 1183
p999_us = 1567
p9999_us = 1919
max_us = 4602
```

## unpipelined 512-conn (M0 gate mix) m1-baseline rep 2

```
ops = 3652222
errors = 0
elapsed_s = 5.008
ops_per_sec = 729281
p50_us = 687
p99_us = 1343
p999_us = 1599
p9999_us = 1919
max_us = 4318
```

## unpipelined 512-conn (M0 gate mix) m2 rep 2

```
ops = 3638581
errors = 0
elapsed_s = 5.007
ops_per_sec = 726664
p50_us = 687
p99_us = 1471
p999_us = 1631
p9999_us = 1983
max_us = 4568
```

## unpipelined 512-conn (M0 gate mix) m2 rep 3

```
ops = 3620354
errors = 0
elapsed_s = 5.008
ops_per_sec = 722957
p50_us = 687
p99_us = 1503
p999_us = 1631
p9999_us = 1983
max_us = 4157
```

## unpipelined 512-conn (M0 gate mix) m1-baseline rep 3

```
ops = 3686478
errors = 0
elapsed_s = 5.008
ops_per_sec = 736156
p50_us = 671
p99_us = 1375
p999_us = 1599
p9999_us = 1919
max_us = 3975
```

## unpipelined 512-conn (M0 gate mix) m1-baseline rep 4

```
ops = 3712757
errors = 0
elapsed_s = 5.009
ops_per_sec = 741202
p50_us = 671
p99_us = 1151
p999_us = 1535
p9999_us = 2239
max_us = 4482
```

## unpipelined 512-conn (M0 gate mix) m2 rep 4

```
ops = 3712453
errors = 0
elapsed_s = 5.008
ops_per_sec = 741272
p50_us = 671
p99_us = 1183
p999_us = 1567
p9999_us = 1887
max_us = 3898
```

## ttl-heavy 1:1 writes (M1 gate mix) m1-baseline rep 0

```
ops = 22136415
errors = 0
elapsed_s = 10.001
ops_per_sec = 2213386
p50_us = 431
p99_us = 863
p999_us = 10239
p9999_us = 12031
max_us = 12972
```

## ttl-heavy 1:1 writes (M1 gate mix) m2 rep 0

```
ops = 22653552
errors = 0
elapsed_s = 10.001
ops_per_sec = 2265092
p50_us = 423
p99_us = 831
p999_us = 5247
p9999_us = 16895
max_us = 17196
```

## ttl-heavy 1:1 writes (M1 gate mix) m2 rep 1

```
ops = 22600347
errors = 0
elapsed_s = 10.001
ops_per_sec = 2259770
p50_us = 431
p99_us = 783
p999_us = 5375
p9999_us = 17919
max_us = 18562
```

## ttl-heavy 1:1 writes (M1 gate mix) m1-baseline rep 1

```
ops = 22680952
errors = 0
elapsed_s = 10.001
ops_per_sec = 2267839
p50_us = 431
p99_us = 831
p999_us = 11007
p9999_us = 12799
max_us = 13620
```

## ttl-heavy 1:1 writes (M1 gate mix) m1-baseline rep 2

```
ops = 21767258
errors = 0
elapsed_s = 10.001
ops_per_sec = 2176434
p50_us = 439
p99_us = 863
p999_us = 11519
p9999_us = 12543
max_us = 13093
```

## ttl-heavy 1:1 writes (M1 gate mix) m2 rep 2

```
ops = 22764897
errors = 0
elapsed_s = 10.001
ops_per_sec = 2276223
p50_us = 431
p99_us = 799
p999_us = 4863
p9999_us = 17407
max_us = 18176
```

## ttl-heavy 1:1 writes (M1 gate mix) m2 rep 3

```
ops = 22838102
errors = 0
elapsed_s = 10.001
ops_per_sec = 2283513
p50_us = 423
p99_us = 751
p999_us = 5375
p9999_us = 17919
max_us = 18907
```

## ttl-heavy 1:1 writes (M1 gate mix) m1-baseline rep 3

```
ops = 22756059
errors = 0
elapsed_s = 10.001
ops_per_sec = 2275324
p50_us = 423
p99_us = 767
p999_us = 11263
p9999_us = 12799
max_us = 13169
```

## ttl-heavy 1:1 writes (M1 gate mix) m1-baseline rep 4

```
ops = 22479679
errors = 0
elapsed_s = 10.001
ops_per_sec = 2247665
p50_us = 423
p99_us = 815
p999_us = 11519
p9999_us = 12543
max_us = 13306
```

## ttl-heavy 1:1 writes (M1 gate mix) m2 rep 4

```
ops = 22344668
errors = 0
elapsed_s = 10.001
ops_per_sec = 2234189
p50_us = 431
p99_us = 863
p999_us = 4991
p9999_us = 17407
max_us = 18400
```
