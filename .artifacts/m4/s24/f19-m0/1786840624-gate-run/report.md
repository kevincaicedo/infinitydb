# M0 gate-run report

date: 1786840624 (unix) · cells: 4 · replicates: 15 · duration: 10s
env-check: OK
tier: reference-box (binding)

notes:
- fabric RTT measured at loop granularity (shared.now updates once per step)
- comparator: dragonfly [0;32mv1.39.0-699862e5da7cb29bf5642a1da422c4c78cefae38[m · interleaved ABBA x 15 · persistence off both (ADR-0006 shape)
- attribution: domains 1098941248 B (document 32768 B) vs VmRSS 1130455040 B (2.8% divergence)

| gate | threshold | measured | verdict |
|---|---|---|---|
| Pipelined GET/SET node throughput | >= 6000000 ops/s | 2839586.80 | FAIL |
| Unpipelined throughput vs Redis, 512 conns | >= 1.5 x Redis | 2.82 | PASS |
| io_uring SQEs per submit under pipelined load | >= 16 sqes/submit | 16.49 | PASS |
| Fabric hop RTT p50 under load | < 2 us | 172.03 | FAIL |
| Cross-cell architecture vs Dragonfly, uniform random | >= 1.25 x Dragonfly | 1.68 | PASS |
| Cross-cell penalty, uniform random keys (informational; M1 S17 target) | <= 50 % vs all-local | 64.32 | FAIL (informational) |
| p99.9 latency (memtier, 8 threads) | < 3000 us | 879.00 | PASS |
| RSS @ 10M keys x (16 B, 64 B) | <= 1.1 x Redis | 0.61 | PASS |
| Reactor loop iteration p99.9 | < 500 us | 195.00 | PASS |
| Syscall CPU share under pipelined load | < 15 % | — | PENDING (tooling) |

## pipelined rep 0

```
ops = 28998536
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2899467
p50_us = 343
p99_us = 655
p999_us = 927
p9999_us = 10495
max_us = 11120
```

## pipelined rep 1

```
ops = 27932791
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2792957
p50_us = 351
p99_us = 687
p999_us = 895
p9999_us = 1247
max_us = 2453
```

## pipelined rep 2

```
ops = 28673996
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2867033
p50_us = 351
p99_us = 639
p999_us = 863
p9999_us = 1279
max_us = 2001
```

## pipelined rep 3

```
ops = 28315370
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2831154
p50_us = 359
p99_us = 703
p999_us = 879
p9999_us = 1215
max_us = 2250
```

## pipelined rep 4

```
ops = 27561092
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2755765
p50_us = 359
p99_us = 703
p999_us = 895
p9999_us = 1151
max_us = 1688
```

## pipelined rep 5

```
ops = 28696801
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2869336
p50_us = 351
p99_us = 671
p999_us = 879
p9999_us = 1183
max_us = 3145
```

## pipelined rep 6

```
ops = 27600069
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2759678
p50_us = 351
p99_us = 687
p999_us = 895
p9999_us = 1119
max_us = 1727
```

## pipelined rep 7

```
ops = 28864143
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2886096
p50_us = 343
p99_us = 655
p999_us = 927
p9999_us = 1311
max_us = 1916
```

## pipelined rep 8

```
ops = 28731964
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2872814
p50_us = 343
p99_us = 655
p999_us = 847
p9999_us = 1119
max_us = 2433
```

## pipelined rep 9

```
ops = 28383490
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2838070
p50_us = 367
p99_us = 719
p999_us = 879
p9999_us = 1119
max_us = 1535
```

## pipelined rep 10

```
ops = 27251698
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2724881
p50_us = 359
p99_us = 703
p999_us = 911
p9999_us = 1183
max_us = 2021
```

## pipelined rep 11

```
ops = 28812029
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2880888
p50_us = 343
p99_us = 655
p999_us = 863
p9999_us = 1215
max_us = 1562
```

## pipelined rep 12

```
ops = 28398881
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2839587
p50_us = 359
p99_us = 703
p999_us = 863
p9999_us = 1119
max_us = 2057
```

## pipelined rep 13

```
ops = 28083950
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2808087
p50_us = 351
p99_us = 687
p999_us = 911
p9999_us = 1183
max_us = 2240
```

## pipelined rep 14

```
ops = 29133780
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2912973
p50_us = 343
p99_us = 623
p999_us = 831
p9999_us = 1247
max_us = 1847
```
