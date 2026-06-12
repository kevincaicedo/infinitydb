# M0 gate-run report

date: 1781227039 (unix) · cells: 4 · replicates: 3 · duration: 10s
env-check: FAILED (overridden — NOT citation-grade)
tier: dev (non-binding)

notes:
- env-check FAILED and was overridden (--unsafe-env): not citation-grade
- dev-tier run: reference-box gates report measured values, non-binding verdicts
- fabric RTT measured at loop granularity (shared.now updates once per step)
- attribution: domains 1098908224 B vs VmRSS 1125773312 B (2.4% divergence)

| gate | threshold | measured | verdict |
|---|---|---|---|
| Pipelined GET/SET node throughput | >= 6000000 ops/s | 1238953.33 | FAIL (DEV-TIER, non-binding) |
| Unpipelined throughput vs Redis, 512 conns | >= 1.5 x Redis | 2.64 | PASS (DEV-TIER, non-binding) |
| io_uring SQEs per submit under pipelined load | >= 16 sqes/submit | 3.78 | FAIL |
| Fabric hop RTT p50 under load | < 2 us | 71.68 | FAIL (DEV-TIER, non-binding) |
| Cross-cell penalty, uniform random keys | <= 30 % vs all-local | 85.33 | FAIL (DEV-TIER, non-binding) |
| p99.9 latency (memtier, 8 threads) | < 3000 us | 3903.00 | FAIL (DEV-TIER, non-binding) |
| RSS @ 10M keys x (16 B, 64 B) | <= 1.1 x Redis | 0.61 | PASS (DEV-TIER, non-binding) |
| Reactor loop iteration p99.9 | < 500 us | 67.00 | PASS (DEV-TIER, non-binding) |
| Syscall CPU share under pipelined load | < 15 % | — | PENDING (tooling) |

## pipelined rep 0

```
ops = 13169667
errors = 0
elapsed_s = 10.001
ops_per_sec = 1316818
p50_us = 767
p99_us = 1375
p999_us = 4351
p9999_us = 8191
max_us = 12500
```

## pipelined rep 1

```
ops = 11915119
errors = 0
elapsed_s = 10.001
ops_per_sec = 1191381
p50_us = 847
p99_us = 1535
p999_us = 3647
p9999_us = 6783
max_us = 10749
```

## pipelined rep 2

```
ops = 12390934
errors = 0
elapsed_s = 10.001
ops_per_sec = 1238953
p50_us = 815
p99_us = 1471
p999_us = 3903
p9999_us = 6655
max_us = 9990
```
