# M0 gate-run report

date: 1784233268 (unix) · cells: 4 · replicates: 3 · duration: 10s
env-check: FAILED (overridden — NOT citation-grade)
tier: dev (non-binding)

notes:
- env-check FAILED and was overridden (--unsafe-env): not citation-grade
- dev-tier run: reference-box gates report measured values, non-binding verdicts
- fabric RTT measured at loop granularity (shared.now updates once per step)
- comparator: dragonfly [0;32mv1.39.0-699862e5da7cb29bf5642a1da422c4c78cefae38[m · interleaved ABBA x 3 · persistence off both (ADR-0006 shape)
- attribution: domains 1098941248 B (document 32768 B) vs VmRSS 1129951232 B (2.7% divergence)

| gate | threshold | measured | verdict |
|---|---|---|---|
| Pipelined GET/SET node throughput | >= 6000000 ops/s | 2917919.76 | FAIL (DEV-TIER, non-binding) |
| Unpipelined throughput vs Redis, 512 conns | >= 1.5 x Redis | 3.09 | PASS (DEV-TIER, non-binding) |
| io_uring SQEs per submit under pipelined load | >= 16 sqes/submit | 19.84 | PASS |
| Fabric hop RTT p50 under load | < 2 us | 176.13 | FAIL (DEV-TIER, non-binding) |
| Cross-cell architecture vs Dragonfly, uniform random | >= 1.25 x Dragonfly | 1.69 | PASS (DEV-TIER, non-binding) |
| Cross-cell penalty, uniform random keys (informational; M1 S17 target) | <= 50 % vs all-local | 64.36 | FAIL (informational) |
| p99.9 latency (memtier, 8 threads) | < 3000 us | 847.00 | PASS (DEV-TIER, non-binding) |
| RSS @ 10M keys x (16 B, 64 B) | <= 1.1 x Redis | 0.61 | PASS (DEV-TIER, non-binding) |
| Reactor loop iteration p99.9 | < 500 us | 195.00 | PASS (DEV-TIER, non-binding) |
| Syscall CPU share under pipelined load | < 15 % | — | PENDING (tooling) |

## pipelined rep 0

```
ops = 29401712
errors = 0
elapsed_s = 10.001
ops_per_sec = 2939765
p50_us = 335
p99_us = 607
p999_us = 911
p9999_us = 10751
max_us = 11233
```

## pipelined rep 1

```
ops = 28983915
errors = 0
elapsed_s = 10.001
ops_per_sec = 2898072
p50_us = 351
p99_us = 639
p999_us = 799
p9999_us = 1119
max_us = 3417
```

## pipelined rep 2

```
ops = 29182505
errors = 0
elapsed_s = 10.001
ops_per_sec = 2917920
p50_us = 343
p99_us = 607
p999_us = 847
p9999_us = 1279
max_us = 2192
```
