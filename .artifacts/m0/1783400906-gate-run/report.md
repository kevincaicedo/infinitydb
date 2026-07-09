# M0 gate-run report

date: 1783400906 (unix) · cells: 4 · replicates: 3 · duration: 10s
env-check: OK
tier: reference-box (binding)

notes:
- fabric RTT measured at loop granularity (shared.now updates once per step)
- comparator: dragonfly [0;32mv1.39.0-699862e5da7cb29bf5642a1da422c4c78cefae38[m · interleaved ABBA x 3 · persistence off both (ADR-0006 shape)
- attribution: domains 1098908480 B vs VmRSS 1129492480 B (2.7% divergence)

| gate | threshold | measured | verdict |
|---|---|---|---|
| Pipelined GET/SET node throughput | >= 6000000 ops/s | 2744740.80 | FAIL |
| Unpipelined throughput vs Redis, 512 conns | >= 1.5 x Redis | 2.85 | PASS |
| io_uring SQEs per submit under pipelined load | >= 16 sqes/submit | 17.40 | PASS |
| Fabric hop RTT p50 under load | < 2 us | 200.70 | FAIL |
| Cross-cell architecture vs Dragonfly, uniform random | >= 1.25 x Dragonfly | 1.61 | PASS |
| Cross-cell penalty, uniform random keys (informational; M1 S17 target) | <= 50 % vs all-local | 58.79 | FAIL (informational) |
| p99.9 latency (memtier, 8 threads) | < 3000 us | 943.00 | PASS |
| RSS @ 10M keys x (16 B, 64 B) | <= 1.1 x Redis | 0.61 | PASS |
| Reactor loop iteration p99.9 | < 500 us | 215.00 | PASS |
| Syscall CPU share under pipelined load | < 15 % | — | PENDING (tooling) |

## pipelined rep 0

```
ops = 28221551
errors = 0
elapsed_s = 10.001
ops_per_sec = 2821828
p50_us = 367
p99_us = 719
p999_us = 943
p9999_us = 11007
max_us = 11509
```

## pipelined rep 1

```
ops = 27451122
errors = 0
elapsed_s = 10.001
ops_per_sec = 2744741
p50_us = 367
p99_us = 671
p999_us = 879
p9999_us = 1311
max_us = 1772
```

## pipelined rep 2

```
ops = 26943142
errors = 0
elapsed_s = 10.001
ops_per_sec = 2694006
p50_us = 367
p99_us = 703
p999_us = 975
p9999_us = 1439
max_us = 2406
```
