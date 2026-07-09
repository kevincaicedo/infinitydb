# M0 gate-run report

date: 1783393428 (unix) · cells: 4 · replicates: 3 · duration: 10s
env-check: OK
tier: reference-box (binding)

notes:
- fabric RTT measured at loop granularity (shared.now updates once per step)
- comparator: dragonfly [0;32mv1.39.0-699862e5da7cb29bf5642a1da422c4c78cefae38[m · interleaved ABBA x 3 · persistence off both (ADR-0006 shape)
- attribution: domains 1098908480 B vs VmRSS 1129426944 B (2.7% divergence)

| gate | threshold | measured | verdict |
|---|---|---|---|
| Pipelined GET/SET node throughput | >= 6000000 ops/s | 2499136.94 | FAIL |
| Unpipelined throughput vs Redis, 512 conns | >= 1.5 x Redis | 2.66 | PASS |
| io_uring SQEs per submit under pipelined load | >= 16 sqes/submit | 16.78 | PASS |
| Fabric hop RTT p50 under load | < 2 us | 196.61 | FAIL |
| Cross-cell architecture vs Dragonfly, uniform random | >= 1.25 x Dragonfly | 1.48 | PASS |
| Cross-cell penalty, uniform random keys (informational; M1 S17 target) | <= 50 % vs all-local | 61.44 | FAIL (informational) |
| p99.9 latency (memtier, 8 threads) | < 3000 us | 943.00 | PASS |
| RSS @ 10M keys x (16 B, 64 B) | <= 1.1 x Redis | 0.61 | PASS |
| Reactor loop iteration p99.9 | < 500 us | 235.00 | PASS |
| Syscall CPU share under pipelined load | < 15 % | — | PENDING (tooling) |

## pipelined rep 0

```
ops = 26297391
errors = 0
elapsed_s = 10.001
ops_per_sec = 2629459
p50_us = 375
p99_us = 703
p999_us = 959
p9999_us = 10495
max_us = 12198
```

## pipelined rep 1

```
ops = 24994319
errors = 0
elapsed_s = 10.001
ops_per_sec = 2499137
p50_us = 399
p99_us = 719
p999_us = 943
p9999_us = 1343
max_us = 2374
```

## pipelined rep 2

```
ops = 24929263
errors = 0
elapsed_s = 10.001
ops_per_sec = 2492639
p50_us = 407
p99_us = 719
p999_us = 943
p9999_us = 1311
max_us = 1754
```
