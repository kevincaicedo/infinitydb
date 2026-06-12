# M0 gate-run report

date: 1781244111 (unix) · cells: 4 · replicates: 3 · duration: 10s
env-check: FAILED (overridden — NOT citation-grade)
tier: dev (non-binding)

notes:
- env-check FAILED and was overridden (--unsafe-env): not citation-grade
- dev-tier run: reference-box gates report measured values, non-binding verdicts
- fabric RTT measured at loop granularity (shared.now updates once per step)
- attribution: domains 1098908224 B vs VmRSS 1126100992 B (2.4% divergence)

| gate | threshold | measured | verdict |
|---|---|---|---|
| Pipelined GET/SET node throughput | >= 6000000 ops/s | 2868738.11 | FAIL (DEV-TIER, non-binding) |
| Unpipelined throughput vs Redis, 512 conns | >= 1.5 x Redis | 2.72 | PASS (DEV-TIER, non-binding) |
| io_uring SQEs per submit under pipelined load | >= 16 sqes/submit | 3.81 | FAIL |
| Fabric hop RTT p50 under load | < 2 us | 184.32 | FAIL (DEV-TIER, non-binding) |
| Cross-cell penalty, uniform random keys | <= 30 % vs all-local | 67.18 | FAIL (DEV-TIER, non-binding) |
| p99.9 latency (memtier, 8 threads) | < 3000 us | 1887.00 | PASS (DEV-TIER, non-binding) |
| RSS @ 10M keys x (16 B, 64 B) | <= 1.1 x Redis | 0.61 | PASS (DEV-TIER, non-binding) |
| Reactor loop iteration p99.9 | < 500 us | 191.00 | PASS (DEV-TIER, non-binding) |
| Syscall CPU share under pipelined load | < 15 % | — | PENDING (tooling) |

## pipelined rep 0

```
ops = 28689622
errors = 0
elapsed_s = 10.001
ops_per_sec = 2868738
p50_us = 327
p99_us = 1407
p999_us = 1887
p9999_us = 8703
max_us = 10143
```

## pipelined rep 1

```
ops = 27553255
errors = 0
elapsed_s = 10.001
ops_per_sec = 2755059
p50_us = 359
p99_us = 975
p999_us = 1791
p9999_us = 1951
max_us = 2495
```

## pipelined rep 2

```
ops = 29190548
errors = 0
elapsed_s = 10.001
ops_per_sec = 2918830
p50_us = 335
p99_us = 943
p999_us = 1887
p9999_us = 3071
max_us = 4394
```
