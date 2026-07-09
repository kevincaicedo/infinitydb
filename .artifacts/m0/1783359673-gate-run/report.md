# M0 gate-run report

date: 1783359673 (unix) · cells: 4 · replicates: 3 · duration: 10s
env-check: FAILED (overridden — NOT citation-grade)
tier: dev (non-binding)

notes:
- env-check FAILED and was overridden (--unsafe-env): not citation-grade
- dev-tier run: reference-box gates report measured values, non-binding verdicts
- fabric RTT measured at loop granularity (shared.now updates once per step)
- comparator: dragonfly [0;32mv1.39.0-699862e5da7cb29bf5642a1da422c4c78cefae38[m · interleaved ABBA x 3 · persistence off both (ADR-0006 shape)
- attribution: domains 1098908480 B vs VmRSS 1129369600 B (2.7% divergence)

| gate | threshold | measured | verdict |
|---|---|---|---|
| Pipelined GET/SET node throughput | >= 6000000 ops/s | 3609259.71 | FAIL (DEV-TIER, non-binding) |
| Unpipelined throughput vs Redis, 512 conns | >= 1.5 x Redis | 2.80 | PASS (DEV-TIER, non-binding) |
| io_uring SQEs per submit under pipelined load | >= 16 sqes/submit | 19.37 | PASS |
| Fabric hop RTT p50 under load | < 2 us | 139.26 | FAIL (DEV-TIER, non-binding) |
| Cross-cell architecture vs Dragonfly, uniform random | >= 1.25 x Dragonfly | 1.45 | PASS (DEV-TIER, non-binding) |
| Cross-cell penalty, uniform random keys (informational; M1 S17 target) | <= 50 % vs all-local | 58.75 | FAIL (informational) |
| p99.9 latency (memtier, 8 threads) | < 3000 us | 1567.00 | PASS (DEV-TIER, non-binding) |
| RSS @ 10M keys x (16 B, 64 B) | <= 1.1 x Redis | 0.61 | PASS (DEV-TIER, non-binding) |
| Reactor loop iteration p99.9 | < 500 us | 223.00 | PASS (DEV-TIER, non-binding) |
| Syscall CPU share under pipelined load | < 15 % | — | PENDING (tooling) |

## pipelined rep 0

```
ops = 38249340
errors = 0
elapsed_s = 10.001
ops_per_sec = 3824585
p50_us = 263
p99_us = 479
p999_us = 1695
p9999_us = 8191
max_us = 9124
```

## pipelined rep 1

```
ops = 35440386
errors = 0
elapsed_s = 10.001
ops_per_sec = 3543703
p50_us = 279
p99_us = 575
p999_us = 1343
p9999_us = 2303
max_us = 3462
```

## pipelined rep 2

```
ops = 36095550
errors = 0
elapsed_s = 10.001
ops_per_sec = 3609260
p50_us = 271
p99_us = 559
p999_us = 1567
p9999_us = 2303
max_us = 3427
```
