# M0 gate-run report

date: 1786421612 (unix) · cells: 4 · replicates: 5 · duration: 10s
env-check: OK
tier: reference-box (binding)

notes:
- fabric RTT measured at loop granularity (shared.now updates once per step)
- comparator: dragonfly [0;32mv1.39.0-699862e5da7cb29bf5642a1da422c4c78cefae38[m · interleaved ABBA x 5 · persistence off both (ADR-0006 shape)
- attribution: domains 1098941248 B (document 32768 B) vs VmRSS 1129975808 B (2.7% divergence)

| gate | threshold | measured | verdict |
|---|---|---|---|
| Pipelined GET/SET node throughput | >= 6000000 ops/s | 2864837.64 | FAIL |
| Unpipelined throughput vs Redis, 512 conns | >= 1.5 x Redis | 2.72 | PASS |
| io_uring SQEs per submit under pipelined load | >= 16 sqes/submit | 17.07 | PASS |
| Fabric hop RTT p50 under load | < 2 us | 192.51 | FAIL |
| Cross-cell architecture vs Dragonfly, uniform random | >= 1.25 x Dragonfly | 1.68 | PASS |
| Cross-cell penalty, uniform random keys (informational; M1 S17 target) | <= 50 % vs all-local | 64.67 | FAIL (informational) |
| p99.9 latency (memtier, 8 threads) | < 3000 us | 863.00 | PASS |
| RSS @ 10M keys x (16 B, 64 B) | <= 1.1 x Redis | 0.61 | PASS |
| Reactor loop iteration p99.9 | < 500 us | 219.00 | PASS |
| Syscall CPU share under pipelined load | < 15 % | — | PENDING (tooling) |

## pipelined rep 0

```
ops = 28511626
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2850826
p50_us = 351
p99_us = 687
p999_us = 975
p9999_us = 10495
max_us = 10849
```

## pipelined rep 1

```
ops = 28675471
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2867149
p50_us = 343
p99_us = 671
p999_us = 863
p9999_us = 1151
max_us = 1657
```

## pipelined rep 2

```
ops = 28651769
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2864838
p50_us = 351
p99_us = 655
p999_us = 863
p9999_us = 1247
max_us = 1756
```

## pipelined rep 3

```
ops = 27220133
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2721645
p50_us = 383
p99_us = 751
p999_us = 879
p9999_us = 1151
max_us = 2040
```

## pipelined rep 4

```
ops = 28797812
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2879469
p50_us = 343
p99_us = 655
p999_us = 863
p9999_us = 1183
max_us = 1865
```
