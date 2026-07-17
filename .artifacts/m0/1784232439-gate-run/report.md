# M0 gate-run report

date: 1784232439 (unix) · cells: 4 · replicates: 3 · duration: 10s
env-check: FAILED (overridden — NOT citation-grade)
tier: dev (non-binding)

notes:
- env-check FAILED and was overridden (--unsafe-env): not citation-grade
- dev-tier run: reference-box gates report measured values, non-binding verdicts
- fabric RTT measured at loop granularity (shared.now updates once per step)
- comparator: dragonfly [0;32mv1.39.0-699862e5da7cb29bf5642a1da422c4c78cefae38[m · interleaved ABBA x 3 · persistence off both (ADR-0006 shape)
- attribution: domains 1098941248 B (document 32768 B) vs VmRSS 1129918464 B (2.7% divergence)

| gate | threshold | measured | verdict |
|---|---|---|---|
| Pipelined GET/SET node throughput | >= 6000000 ops/s | 2923451.19 | FAIL (DEV-TIER, non-binding) |
| Unpipelined throughput vs Redis, 512 conns | >= 1.5 x Redis | 3.14 | PASS (DEV-TIER, non-binding) |
| io_uring SQEs per submit under pipelined load | >= 16 sqes/submit | 18.16 | PASS |
| Fabric hop RTT p50 under load | < 2 us | 180.22 | FAIL (DEV-TIER, non-binding) |
| Cross-cell architecture vs Dragonfly, uniform random | >= 1.25 x Dragonfly | 1.74 | PASS (DEV-TIER, non-binding) |
| Cross-cell penalty, uniform random keys (informational; M1 S17 target) | <= 50 % vs all-local | 63.92 | FAIL (informational) |
| p99.9 latency (memtier, 8 threads) | < 3000 us | 863.00 | PASS (DEV-TIER, non-binding) |
| RSS @ 10M keys x (16 B, 64 B) | <= 1.1 x Redis | 0.61 | PASS (DEV-TIER, non-binding) |
| Reactor loop iteration p99.9 | < 500 us | 207.00 | PASS (DEV-TIER, non-binding) |
| Syscall CPU share under pipelined load | < 15 % | — | PENDING (tooling) |

## pipelined rep 0

```
ops = 29237812
errors = 0
elapsed_s = 10.001
ops_per_sec = 2923451
p50_us = 343
p99_us = 623
p999_us = 895
p9999_us = 10751
max_us = 21569
```

## pipelined rep 1

```
ops = 28720527
errors = 0
elapsed_s = 10.001
ops_per_sec = 2871690
p50_us = 343
p99_us = 687
p999_us = 863
p9999_us = 1247
max_us = 3363
```

## pipelined rep 2

```
ops = 29935923
errors = 0
elapsed_s = 10.001
ops_per_sec = 2993247
p50_us = 335
p99_us = 591
p999_us = 783
p9999_us = 1279
max_us = 3674
```
