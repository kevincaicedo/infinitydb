# M0 gate-run report

date: 1781227231 (unix) · cells: 4 · replicates: 3 · duration: 10s
env-check: FAILED (overridden — NOT citation-grade)
tier: dev (non-binding)

notes:
- env-check FAILED and was overridden (--unsafe-env): not citation-grade
- dev-tier run: reference-box gates report measured values, non-binding verdicts
- fabric RTT measured at loop granularity (shared.now updates once per step)
- attribution: domains 1098908224 B vs VmRSS 1125752832 B (2.4% divergence)

| gate | threshold | measured | verdict |
|---|---|---|---|
| Pipelined GET/SET node throughput | >= 6000000 ops/s | 1294034.67 | FAIL (DEV-TIER, non-binding) |
| Unpipelined throughput vs Redis, 512 conns | >= 1.5 x Redis | 2.62 | PASS (DEV-TIER, non-binding) |
| io_uring SQEs per submit under pipelined load | >= 16 sqes/submit | 11.70 | FAIL |
| Fabric hop RTT p50 under load | < 2 us | 63.49 | FAIL (DEV-TIER, non-binding) |
| Cross-cell penalty, uniform random keys | <= 30 % vs all-local | 85.79 | FAIL (DEV-TIER, non-binding) |
| p99.9 latency (memtier, 8 threads) | < 3000 us | 3583.00 | FAIL (DEV-TIER, non-binding) |
| RSS @ 10M keys x (16 B, 64 B) | <= 1.1 x Redis | 0.61 | PASS (DEV-TIER, non-binding) |
| Reactor loop iteration p99.9 | < 500 us | 67.00 | PASS (DEV-TIER, non-binding) |
| Syscall CPU share under pipelined load | < 15 % | — | PENDING (tooling) |

## pipelined rep 0

```
ops = 12941875
errors = 0
elapsed_s = 10.001
ops_per_sec = 1294035
p50_us = 767
p99_us = 1503
p999_us = 5631
p9999_us = 7935
max_us = 10026
```

## pipelined rep 1

```
ops = 12821995
errors = 0
elapsed_s = 10.001
ops_per_sec = 1282013
p50_us = 799
p99_us = 1311
p999_us = 3583
p9999_us = 6143
max_us = 8650
```

## pipelined rep 2

```
ops = 13199164
errors = 0
elapsed_s = 10.001
ops_per_sec = 1319761
p50_us = 767
p99_us = 1279
p999_us = 3327
p9999_us = 5631
max_us = 7261
```
