# M0 gate-run report

date: 1786419995 (unix) · cells: 4 · replicates: 5 · duration: 10s
env-check: OK
tier: reference-box (binding)

notes:
- fabric RTT measured at loop granularity (shared.now updates once per step)
- comparator: dragonfly [0;32mv1.39.0-699862e5da7cb29bf5642a1da422c4c78cefae38[m · interleaved ABBA x 5 · persistence off both (ADR-0006 shape)
- attribution: domains 1098941248 B (document 32768 B) vs VmRSS 1130512384 B (2.8% divergence)

| gate | threshold | measured | verdict |
|---|---|---|---|
| Pipelined GET/SET node throughput | >= 6000000 ops/s | 2844019.30 | FAIL |
| Unpipelined throughput vs Redis, 512 conns | >= 1.5 x Redis | 2.82 | PASS |
| io_uring SQEs per submit under pipelined load | >= 16 sqes/submit | 14.51 | FAIL |
| Fabric hop RTT p50 under load | < 2 us | 200.70 | FAIL |
| Cross-cell architecture vs Dragonfly, uniform random | >= 1.25 x Dragonfly | 1.68 | PASS |
| Cross-cell penalty, uniform random keys (informational; M1 S17 target) | <= 50 % vs all-local | 63.17 | FAIL (informational) |
| p99.9 latency (memtier, 8 threads) | < 3000 us | 895.00 | PASS |
| RSS @ 10M keys x (16 B, 64 B) | <= 1.1 x Redis | 0.61 | PASS |
| Reactor loop iteration p99.9 | < 500 us | 215.00 | PASS |
| Syscall CPU share under pipelined load | < 15 % | — | PENDING (tooling) |

## pipelined rep 0

```
ops = 29201420
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2919756
p50_us = 343
p99_us = 623
p999_us = 895
p9999_us = 10495
max_us = 11174
```

## pipelined rep 1

```
ops = 28443996
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2844019
p50_us = 367
p99_us = 703
p999_us = 879
p9999_us = 1183
max_us = 1802
```

## pipelined rep 2

```
ops = 29284616
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2928143
p50_us = 343
p99_us = 623
p999_us = 831
p9999_us = 1183
max_us = 1632
```

## pipelined rep 3

```
ops = 26601689
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2659864
p50_us = 383
p99_us = 799
p999_us = 911
p9999_us = 1183
max_us = 1816
```

## pipelined rep 4

```
ops = 27922079
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2791897
p50_us = 351
p99_us = 703
p999_us = 895
p9999_us = 1151
max_us = 1963
```
