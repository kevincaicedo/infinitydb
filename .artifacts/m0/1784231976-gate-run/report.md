# M0 gate-run report

date: 1784231976 (unix) · cells: 4 · replicates: 3 · duration: 10s
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
| Pipelined GET/SET node throughput | >= 6000000 ops/s | 2764904.60 | FAIL (DEV-TIER, non-binding) |
| Unpipelined throughput vs Redis, 512 conns | >= 1.5 x Redis | 2.97 | PASS (DEV-TIER, non-binding) |
| io_uring SQEs per submit under pipelined load | >= 16 sqes/submit | 14.22 | FAIL |
| Fabric hop RTT p50 under load | < 2 us | 188.41 | FAIL (DEV-TIER, non-binding) |
| Cross-cell architecture vs Dragonfly, uniform random | >= 1.25 x Dragonfly | 1.63 | PASS (DEV-TIER, non-binding) |
| Cross-cell penalty, uniform random keys (informational; M1 S17 target) | <= 50 % vs all-local | 64.36 | FAIL (informational) |
| p99.9 latency (memtier, 8 threads) | < 3000 us | 879.00 | PASS (DEV-TIER, non-binding) |
| RSS @ 10M keys x (16 B, 64 B) | <= 1.1 x Redis | 0.61 | PASS (DEV-TIER, non-binding) |
| Reactor loop iteration p99.9 | < 500 us | 211.00 | PASS (DEV-TIER, non-binding) |
| Syscall CPU share under pipelined load | < 15 % | — | PENDING (tooling) |

## pipelined rep 0

```
ops = 27653218
errors = 0
elapsed_s = 10.002
ops_per_sec = 2764905
p50_us = 359
p99_us = 703
p999_us = 1007
p9999_us = 10495
max_us = 10999
```

## pipelined rep 1

```
ops = 28019311
errors = 0
elapsed_s = 10.001
ops_per_sec = 2801612
p50_us = 359
p99_us = 655
p999_us = 847
p9999_us = 1215
max_us = 1689
```

## pipelined rep 2

```
ops = 27645256
errors = 0
elapsed_s = 10.001
ops_per_sec = 2764167
p50_us = 367
p99_us = 655
p999_us = 879
p9999_us = 1311
max_us = 2303
```
