# M0 gate-run report

date: 1783393275 (unix) · cells: 4 · replicates: 3 · duration: 10s
env-check: OK
tier: reference-box (binding)

notes:
- fabric RTT measured at loop granularity (shared.now updates once per step)
- comparator: dragonfly [0;32mv1.39.0-699862e5da7cb29bf5642a1da422c4c78cefae38[m · interleaved ABBA x 3 · persistence off both (ADR-0006 shape)
- attribution: domains 1098908480 B vs VmRSS 1129414656 B (2.7% divergence)

| gate | threshold | measured | verdict |
|---|---|---|---|
| Pipelined GET/SET node throughput | >= 6000000 ops/s | 2380869.90 | FAIL |
| Unpipelined throughput vs Redis, 512 conns | >= 1.5 x Redis | 2.72 | PASS |
| io_uring SQEs per submit under pipelined load | >= 16 sqes/submit | 17.31 | PASS |
| Fabric hop RTT p50 under load | < 2 us | 172.03 | FAIL |
| Cross-cell architecture vs Dragonfly, uniform random | >= 1.25 x Dragonfly | 1.36 | PASS |
| Cross-cell penalty, uniform random keys (informational; M1 S17 target) | <= 50 % vs all-local | 62.78 | FAIL (informational) |
| p99.9 latency (memtier, 8 threads) | < 3000 us | 1087.00 | PASS |
| RSS @ 10M keys x (16 B, 64 B) | <= 1.1 x Redis | 0.61 | PASS |
| Reactor loop iteration p99.9 | < 500 us | 215.00 | PASS |
| Syscall CPU share under pipelined load | < 15 % | — | PENDING (tooling) |

## pipelined rep 0

```
ops = 23888188
errors = 0
elapsed_s = 10.001
ops_per_sec = 2388547
p50_us = 423
p99_us = 815
p999_us = 1183
p9999_us = 10751
max_us = 11344
```

## pipelined rep 1

```
ops = 22650239
errors = 0
elapsed_s = 10.001
ops_per_sec = 2264745
p50_us = 447
p99_us = 895
p999_us = 1087
p9999_us = 1439
max_us = 2016
```

## pipelined rep 2

```
ops = 23811448
errors = 0
elapsed_s = 10.001
ops_per_sec = 2380870
p50_us = 423
p99_us = 815
p999_us = 1007
p9999_us = 1439
max_us = 2008
```
