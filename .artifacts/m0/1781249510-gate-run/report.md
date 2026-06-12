# M0 gate-run report

date: 1781249510 (unix) · cells: 4 · replicates: 3 · duration: 10s
env-check: FAILED (overridden — NOT citation-grade)
tier: dev (non-binding)

notes:
- env-check FAILED and was overridden (--unsafe-env): not citation-grade
- dev-tier run: reference-box gates report measured values, non-binding verdicts
- fabric RTT measured at loop granularity (shared.now updates once per step)
- attribution: domains 1098908224 B vs VmRSS 1126850560 B (2.5% divergence)

| gate | threshold | measured | verdict |
|---|---|---|---|
| Pipelined GET/SET node throughput | >= 6000000 ops/s | 3701454.15 | FAIL (DEV-TIER, non-binding) |
| Unpipelined throughput vs Redis, 512 conns | >= 1.5 x Redis | 2.83 | PASS (DEV-TIER, non-binding) |
| io_uring SQEs per submit under pipelined load | >= 16 sqes/submit | 13.33 | FAIL |
| Fabric hop RTT p50 under load | < 2 us | 143.36 | FAIL (DEV-TIER, non-binding) |
| Cross-cell penalty, uniform random keys | <= 30 % vs all-local | 56.63 | FAIL (DEV-TIER, non-binding) |
| p99.9 latency (memtier, 8 threads) | < 3000 us | 1471.00 | PASS (DEV-TIER, non-binding) |
| RSS @ 10M keys x (16 B, 64 B) | <= 1.1 x Redis | 0.61 | PASS (DEV-TIER, non-binding) |
| Reactor loop iteration p99.9 | < 500 us | 231.00 | PASS (DEV-TIER, non-binding) |
| Syscall CPU share under pipelined load | < 15 % | — | PENDING (tooling) |

## pipelined rep 0

```
ops = 38187358
errors = 0
elapsed_s = 10.001
ops_per_sec = 3818385
p50_us = 255
p99_us = 511
p999_us = 1503
p9999_us = 8447
max_us = 10500
```

## pipelined rep 1

```
ops = 37018067
errors = 0
elapsed_s = 10.001
ops_per_sec = 3701454
p50_us = 271
p99_us = 495
p999_us = 1311
p9999_us = 2239
max_us = 3545
```

## pipelined rep 2

```
ops = 35124340
errors = 0
elapsed_s = 10.001
ops_per_sec = 3512096
p50_us = 279
p99_us = 575
p999_us = 1471
p9999_us = 2687
max_us = 4265
```
