# M0 gate-run report

date: 1783359278 (unix) · cells: 4 · replicates: 3 · duration: 10s
env-check: FAILED (overridden — NOT citation-grade)
tier: dev (non-binding)

notes:
- env-check FAILED and was overridden (--unsafe-env): not citation-grade
- dev-tier run: reference-box gates report measured values, non-binding verdicts
- fabric RTT measured at loop granularity (shared.now updates once per step)
- comparator: dragonfly [0;32mv1.39.0-699862e5da7cb29bf5642a1da422c4c78cefae38[m · interleaved ABBA x 3 · persistence off both (ADR-0006 shape)
- attribution: domains 1098908480 B vs VmRSS 1129304064 B (2.7% divergence)

| gate | threshold | measured | verdict |
|---|---|---|---|
| Pipelined GET/SET node throughput | >= 6000000 ops/s | 3599002.06 | FAIL (DEV-TIER, non-binding) |
| Unpipelined throughput vs Redis, 512 conns | >= 1.5 x Redis | 2.81 | PASS (DEV-TIER, non-binding) |
| io_uring SQEs per submit under pipelined load | >= 16 sqes/submit | 19.12 | PASS |
| Fabric hop RTT p50 under load | < 2 us | 143.36 | FAIL (DEV-TIER, non-binding) |
| Cross-cell architecture vs Dragonfly, uniform random | >= 1.25 x Dragonfly | 1.46 | PASS (DEV-TIER, non-binding) |
| Cross-cell penalty, uniform random keys (informational; M1 S17 target) | <= 50 % vs all-local | 59.51 | FAIL (informational) |
| p99.9 latency (memtier, 8 threads) | < 3000 us | 1535.00 | PASS (DEV-TIER, non-binding) |
| RSS @ 10M keys x (16 B, 64 B) | <= 1.1 x Redis | 0.61 | PASS (DEV-TIER, non-binding) |
| Reactor loop iteration p99.9 | < 500 us | 251.00 | PASS (DEV-TIER, non-binding) |
| Syscall CPU share under pipelined load | < 15 % | — | PENDING (tooling) |

## pipelined rep 0

```
ops = 37266758
errors = 0
elapsed_s = 10.001
ops_per_sec = 3726391
p50_us = 263
p99_us = 511
p999_us = 1695
p9999_us = 8447
max_us = 8978
```

## pipelined rep 1

```
ops = 35765733
errors = 0
elapsed_s = 10.001
ops_per_sec = 3576279
p50_us = 279
p99_us = 543
p999_us = 1407
p9999_us = 2239
max_us = 3208
```

## pipelined rep 2

```
ops = 35993205
errors = 0
elapsed_s = 10.001
ops_per_sec = 3599002
p50_us = 279
p99_us = 543
p999_us = 1535
p9999_us = 2303
max_us = 3749
```
