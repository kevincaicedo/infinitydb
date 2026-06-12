# M0 gate-run report

date: 1781253562 (unix) · cells: 4 · replicates: 3 · duration: 10s
env-check: FAILED (overridden — NOT citation-grade)
tier: dev (non-binding)

notes:
- env-check FAILED and was overridden (--unsafe-env): not citation-grade
- dev-tier run: reference-box gates report measured values, non-binding verdicts
- fabric RTT measured at loop granularity (shared.now updates once per step)
- attribution: domains 1098908224 B vs VmRSS 1126854656 B (2.5% divergence)

| gate | threshold | measured | verdict |
|---|---|---|---|
| Pipelined GET/SET node throughput | >= 6000000 ops/s | 3896788.06 | FAIL (DEV-TIER, non-binding) |
| Unpipelined throughput vs Redis, 512 conns | >= 1.5 x Redis | 2.86 | PASS (DEV-TIER, non-binding) |
| io_uring SQEs per submit under pipelined load | >= 16 sqes/submit | 17.16 | PASS |
| Fabric hop RTT p50 under load | < 2 us | 143.36 | FAIL (DEV-TIER, non-binding) |
| Cross-cell architecture vs Dragonfly, uniform random | >= 1.25 x Dragonfly | — | PENDING (tooling) |
| Cross-cell penalty, uniform random keys (informational; M1 S17 target) | <= 50 % vs all-local | 56.17 | FAIL (informational) |
| p99.9 latency (memtier, 8 threads) | < 3000 us | 1055.00 | PASS (DEV-TIER, non-binding) |
| RSS @ 10M keys x (16 B, 64 B) | <= 1.1 x Redis | 0.61 | PASS (DEV-TIER, non-binding) |
| Reactor loop iteration p99.9 | < 500 us | 227.00 | PASS (DEV-TIER, non-binding) |
| Syscall CPU share under pipelined load | < 15 % | — | PENDING (tooling) |

## pipelined rep 0

```
ops = 41508242
errors = 0
elapsed_s = 10.001
ops_per_sec = 4150491
p50_us = 239
p99_us = 439
p999_us = 991
p9999_us = 4223
max_us = 8563
```

## pipelined rep 1

```
ops = 38970843
errors = 0
elapsed_s = 10.001
ops_per_sec = 3896788
p50_us = 255
p99_us = 495
p999_us = 1119
p9999_us = 2239
max_us = 3694
```

## pipelined rep 2

```
ops = 38497196
errors = 0
elapsed_s = 10.001
ops_per_sec = 3849401
p50_us = 263
p99_us = 495
p999_us = 1055
p9999_us = 1983
max_us = 3662
```
