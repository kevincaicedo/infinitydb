# M0 gate-run report

date: 1786841977 (unix) · cells: 4 · replicates: 15 · duration: 10s
env-check: OK
tier: reference-box (binding)

notes:
- fabric RTT measured at loop granularity (shared.now updates once per step)
- comparator: dragonfly [0;32mv1.39.0-699862e5da7cb29bf5642a1da422c4c78cefae38[m · interleaved ABBA x 15 · persistence off both (ADR-0006 shape)
- attribution: domains 1098941248 B (document 32768 B) vs VmRSS 1129963520 B (2.7% divergence)

| gate | threshold | measured | verdict |
|---|---|---|---|
| Pipelined GET/SET node throughput | >= 6000000 ops/s | 2843086.39 | FAIL |
| Unpipelined throughput vs Redis, 512 conns | >= 1.5 x Redis | 2.80 | PASS |
| io_uring SQEs per submit under pipelined load | >= 16 sqes/submit | 16.40 | PASS |
| Fabric hop RTT p50 under load | < 2 us | 176.13 | FAIL |
| Cross-cell architecture vs Dragonfly, uniform random | >= 1.25 x Dragonfly | 1.65 | PASS |
| Cross-cell penalty, uniform random keys (informational; M1 S17 target) | <= 50 % vs all-local | 64.64 | FAIL (informational) |
| p99.9 latency (memtier, 8 threads) | < 3000 us | 879.00 | PASS |
| RSS @ 10M keys x (16 B, 64 B) | <= 1.1 x Redis | 0.61 | PASS |
| Reactor loop iteration p99.9 | < 500 us | 195.00 | PASS |
| Syscall CPU share under pipelined load | < 15 % | — | PENDING (tooling) |

## pipelined rep 0

```
ops = 28151872
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2814830
p50_us = 351
p99_us = 671
p999_us = 943
p9999_us = 10495
max_us = 11191
```

## pipelined rep 1

```
ops = 28128558
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2812522
p50_us = 359
p99_us = 655
p999_us = 863
p9999_us = 1247
max_us = 1564
```

## pipelined rep 2

```
ops = 28433893
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2843086
p50_us = 351
p99_us = 655
p999_us = 911
p9999_us = 1279
max_us = 1704
```

## pipelined rep 3

```
ops = 27396218
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2739342
p50_us = 359
p99_us = 719
p999_us = 943
p9999_us = 1247
max_us = 1584
```

## pipelined rep 4

```
ops = 27557342
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2755428
p50_us = 375
p99_us = 671
p999_us = 911
p9999_us = 1375
max_us = 1777
```

## pipelined rep 5

```
ops = 28463723
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2846034
p50_us = 351
p99_us = 671
p999_us = 879
p9999_us = 1087
max_us = 1668
```

## pipelined rep 6

```
ops = 28568140
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2856504
p50_us = 351
p99_us = 655
p999_us = 879
p9999_us = 1279
max_us = 1685
```

## pipelined rep 7

```
ops = 29340555
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2933696
p50_us = 343
p99_us = 623
p999_us = 879
p9999_us = 1279
max_us = 1721
```

## pipelined rep 8

```
ops = 28536406
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2853331
p50_us = 351
p99_us = 687
p999_us = 879
p9999_us = 1119
max_us = 2019
```

## pipelined rep 9

```
ops = 28405367
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2840205
p50_us = 351
p99_us = 671
p999_us = 863
p9999_us = 1151
max_us = 1538
```

## pipelined rep 10

```
ops = 28710835
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2870775
p50_us = 343
p99_us = 671
p999_us = 911
p9999_us = 1247
max_us = 1642
```

## pipelined rep 11

```
ops = 28865507
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2886200
p50_us = 343
p99_us = 655
p999_us = 879
p9999_us = 1215
max_us = 1956
```

## pipelined rep 12

```
ops = 28105454
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2810187
p50_us = 359
p99_us = 703
p999_us = 895
p9999_us = 1119
max_us = 1782
```

## pipelined rep 13

```
ops = 29035397
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2903194
p50_us = 343
p99_us = 639
p999_us = 879
p9999_us = 1279
max_us = 1564
```

## pipelined rep 14

```
ops = 27714712
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2771114
p50_us = 359
p99_us = 687
p999_us = 927
p9999_us = 1311
max_us = 2170
```
