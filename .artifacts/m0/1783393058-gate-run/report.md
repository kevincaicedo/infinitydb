# M0 gate-run report

date: 1783393058 (unix) · cells: 4 · replicates: 3 · duration: 10s
env-check: OK
tier: reference-box (binding)

notes:
- fabric RTT measured at loop granularity (shared.now updates once per step)
- comparator: dragonfly [0;32mv1.39.0-699862e5da7cb29bf5642a1da422c4c78cefae38[m · interleaved ABBA x 3 · persistence off both (ADR-0006 shape)
- attribution: domains 1098908480 B vs VmRSS 1129472000 B (2.7% divergence)

| gate | threshold | measured | verdict |
|---|---|---|---|
| Pipelined GET/SET node throughput | >= 6000000 ops/s | 2397551.75 | FAIL |
| Unpipelined throughput vs Redis, 512 conns | >= 1.5 x Redis | 2.71 | PASS |
| io_uring SQEs per submit under pipelined load | >= 16 sqes/submit | 18.63 | PASS |
| Fabric hop RTT p50 under load | < 2 us | 188.41 | FAIL |
| Cross-cell architecture vs Dragonfly, uniform random | >= 1.25 x Dragonfly | 1.37 | PASS |
| Cross-cell penalty, uniform random keys (informational; M1 S17 target) | <= 50 % vs all-local | 64.18 | FAIL (informational) |
| p99.9 latency (memtier, 8 threads) | < 3000 us | 1151.00 | PASS |
| RSS @ 10M keys x (16 B, 64 B) | <= 1.1 x Redis | 0.61 | PASS |
| Reactor loop iteration p99.9 | < 500 us | 223.00 | PASS |
| Syscall CPU share under pipelined load | < 15 % | — | PENDING (tooling) |

## pipelined rep 0

```
ops = 24301796
errors = 0
elapsed_s = 10.001
ops_per_sec = 2429864
p50_us = 407
p99_us = 815
p999_us = 1183
p9999_us = 10751
max_us = 11767
```

## pipelined rep 1

```
ops = 23978537
errors = 0
elapsed_s = 10.001
ops_per_sec = 2397552
p50_us = 423
p99_us = 783
p999_us = 991
p9999_us = 1471
max_us = 2815
```

## pipelined rep 2

```
ops = 22312377
errors = 0
elapsed_s = 10.001
ops_per_sec = 2230943
p50_us = 439
p99_us = 911
p999_us = 1151
p9999_us = 1503
max_us = 2157
```
