# M0 gate-run report

date: 1784235296 (unix) · cells: 4 · replicates: 3 · duration: 10s
env-check: OK
tier: dev (non-binding)

notes:
- dev-tier run: reference-box gates report measured values, non-binding verdicts
- fabric RTT measured at loop granularity (shared.now updates once per step)
- comparator: dragonfly [0;32mv1.39.0-699862e5da7cb29bf5642a1da422c4c78cefae38[m · interleaved ABBA x 3 · persistence off both (ADR-0006 shape)
- attribution: domains 1098908480 B vs VmRSS 1129652224 B (2.7% divergence)

| gate | threshold | measured | verdict |
|---|---|---|---|
| Pipelined GET/SET node throughput | >= 6000000 ops/s | 2898552.70 | FAIL (DEV-TIER, non-binding) |
| Unpipelined throughput vs Redis, 512 conns | >= 1.5 x Redis | 2.91 | PASS (DEV-TIER, non-binding) |
| io_uring SQEs per submit under pipelined load | >= 16 sqes/submit | 17.49 | PASS |
| Fabric hop RTT p50 under load | < 2 us | 172.03 | FAIL (DEV-TIER, non-binding) |
| Cross-cell architecture vs Dragonfly, uniform random | >= 1.25 x Dragonfly | 1.68 | PASS (DEV-TIER, non-binding) |
| Cross-cell penalty, uniform random keys (informational; M1 S17 target) | <= 50 % vs all-local | 64.75 | FAIL (informational) |
| p99.9 latency (memtier, 8 threads) | < 3000 us | 847.00 | PASS (DEV-TIER, non-binding) |
| RSS @ 10M keys x (16 B, 64 B) | <= 1.1 x Redis | 0.61 | PASS (DEV-TIER, non-binding) |
| Reactor loop iteration p99.9 | < 500 us | 191.00 | PASS (DEV-TIER, non-binding) |
| Syscall CPU share under pipelined load | < 15 % | — | PENDING (tooling) |

## pipelined rep 0

```
ops = 29319022
errors = 0
elapsed_s = 10.001
ops_per_sec = 2931559
p50_us = 335
p99_us = 623
p999_us = 863
p9999_us = 10495
max_us = 11017
```

## pipelined rep 1

```
ops = 28472547
errors = 0
elapsed_s = 10.001
ops_per_sec = 2846933
p50_us = 351
p99_us = 639
p999_us = 847
p9999_us = 1119
max_us = 1739
```

## pipelined rep 2

```
ops = 28988827
errors = 0
elapsed_s = 10.001
ops_per_sec = 2898553
p50_us = 343
p99_us = 639
p999_us = 847
p9999_us = 1119
max_us = 1850
```
