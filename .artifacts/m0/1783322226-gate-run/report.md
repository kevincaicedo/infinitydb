# M0 gate-run report

date: 1783322226 (unix) · cells: 4 · replicates: 3 · duration: 10s
env-check: FAILED (overridden — NOT citation-grade)
tier: dev (non-binding)

notes:
- env-check FAILED and was overridden (--unsafe-env): not citation-grade
- dev-tier run: reference-box gates report measured values, non-binding verdicts
- fabric RTT measured at loop granularity (shared.now updates once per step)
- comparator: dragonfly [0;32mv1.39.0-699862e5da7cb29bf5642a1da422c4c78cefae38[m · interleaved ABBA x 3 · persistence off both (ADR-0006 shape)
- attribution: domains 1098908480 B vs VmRSS 1129459712 B (2.7% divergence)

| gate | threshold | measured | verdict |
|---|---|---|---|
| Pipelined GET/SET node throughput | >= 6000000 ops/s | 2202570.61 | FAIL (DEV-TIER, non-binding) |
| Unpipelined throughput vs Redis, 512 conns | >= 1.5 x Redis | 2.60 | PASS (DEV-TIER, non-binding) |
| io_uring SQEs per submit under pipelined load | >= 16 sqes/submit | 15.87 | FAIL |
| Fabric hop RTT p50 under load | < 2 us | 233.47 | FAIL (DEV-TIER, non-binding) |
| Cross-cell architecture vs Dragonfly, uniform random | >= 1.25 x Dragonfly | 1.31 | PASS (DEV-TIER, non-binding) |
| Cross-cell penalty, uniform random keys (informational; M1 S17 target) | <= 50 % vs all-local | 57.22 | FAIL (informational) |
| p99.9 latency (memtier, 8 threads) | < 3000 us | 1151.00 | PASS (DEV-TIER, non-binding) |
| RSS @ 10M keys x (16 B, 64 B) | <= 1.1 x Redis | 0.61 | PASS (DEV-TIER, non-binding) |
| Reactor loop iteration p99.9 | < 500 us | 279.00 | PASS (DEV-TIER, non-binding) |
| Syscall CPU share under pipelined load | < 15 % | — | PENDING (tooling) |

## pipelined rep 0

```
ops = 22817700
errors = 0
elapsed_s = 10.001
ops_per_sec = 2281474
p50_us = 423
p99_us = 863
p999_us = 1151
p9999_us = 11263
max_us = 22518
```

## pipelined rep 1

```
ops = 22028153
errors = 0
elapsed_s = 10.001
ops_per_sec = 2202571
p50_us = 447
p99_us = 879
p999_us = 1119
p9999_us = 1407
max_us = 2661
```

## pipelined rep 2

```
ops = 21147985
errors = 0
elapsed_s = 10.002
ops_per_sec = 2114468
p50_us = 455
p99_us = 943
p999_us = 1247
p9999_us = 1503
max_us = 3561
```
