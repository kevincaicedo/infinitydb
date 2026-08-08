# M0 gate-run report

date: 1783392817 (unix) · cells: 4 · replicates: 3 · duration: 10s
env-check: OK
tier: reference-box (binding)

notes:
- fabric RTT measured at loop granularity (shared.now updates once per step)
- comparator: dragonfly [0;32mv1.39.0-699862e5da7cb29bf5642a1da422c4c78cefae38[m · interleaved ABBA x 3 · persistence off both (ADR-0006 shape)
- attribution: domains 1098908480 B vs VmRSS 1129435136 B (2.7% divergence)

| gate | threshold | measured | verdict |
|---|---|---|---|
| Pipelined GET/SET node throughput | >= 6000000 ops/s | 2499775.37 | FAIL |
| Unpipelined throughput vs Redis, 512 conns | >= 1.5 x Redis | 2.91 | PASS |
| io_uring SQEs per submit under pipelined load | >= 16 sqes/submit | 16.73 | PASS |
| Fabric hop RTT p50 under load | < 2 us | 208.90 | FAIL |
| Cross-cell architecture vs Dragonfly, uniform random | >= 1.25 x Dragonfly | 1.45 | PASS |
| Cross-cell penalty, uniform random keys (informational; M1 S17 target) | <= 50 % vs all-local | 61.72 | FAIL (informational) |
| p99.9 latency (memtier, 8 threads) | < 3000 us | 943.00 | PASS |
| RSS @ 10M keys x (16 B, 64 B) | <= 1.1 x Redis | 0.61 | PASS |
| Reactor loop iteration p99.9 | < 500 us | 239.00 | PASS |
| Syscall CPU share under pipelined load | < 15 % | — | PENDING (tooling) |

## pipelined rep 0

```
ops = 25595601
errors = 0
elapsed_s = 10.001
ops_per_sec = 2559250
p50_us = 391
p99_us = 719
p999_us = 975
p9999_us = 11007
max_us = 11560
```

## pipelined rep 1

```
ops = 25000458
errors = 0
elapsed_s = 10.001
ops_per_sec = 2499775
p50_us = 399
p99_us = 735
p999_us = 943
p9999_us = 1343
max_us = 1970
```

## pipelined rep 2

```
ops = 24797226
errors = 0
elapsed_s = 10.001
ops_per_sec = 2479459
p50_us = 407
p99_us = 719
p999_us = 943
p9999_us = 1503
max_us = 4067
```
