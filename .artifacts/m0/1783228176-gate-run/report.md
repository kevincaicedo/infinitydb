# M0 gate-run report

date: 1783228176 (unix) · cells: 4 · replicates: 3 · duration: 10s
env-check: OK
tier: reference-box (binding)

notes:
- fabric RTT measured at loop granularity (shared.now updates once per step)
- attribution: domains 1098908480 B vs VmRSS 1129422848 B (2.7% divergence)

| gate | threshold | measured | verdict |
|---|---|---|---|
| Pipelined GET/SET node throughput | >= 6000000 ops/s | 2459719.29 | FAIL |
| Unpipelined throughput vs Redis, 512 conns | >= 1.5 x Redis | 2.75 | PASS |
| io_uring SQEs per submit under pipelined load | >= 16 sqes/submit | 16.77 | PASS |
| Fabric hop RTT p50 under load | < 2 us | 200.70 | FAIL |
| Cross-cell architecture vs Dragonfly, uniform random | >= 1.25 x Dragonfly | — | PENDING (tooling) |
| Cross-cell penalty, uniform random keys (informational; M1 S17 target) | <= 50 % vs all-local | 62.93 | FAIL (informational) |
| p99.9 latency (memtier, 8 threads) | < 3000 us | 1023.00 | PASS |
| RSS @ 10M keys x (16 B, 64 B) | <= 1.1 x Redis | 0.61 | PASS |
| Reactor loop iteration p99.9 | < 500 us | 235.00 | PASS |
| Syscall CPU share under pipelined load | < 15 % | — | PENDING (tooling) |

## pipelined rep 0

```
ops = 25556456
errors = 0
elapsed_s = 10.001
ops_per_sec = 2555323
p50_us = 391
p99_us = 719
p999_us = 1087
p9999_us = 11263
max_us = 12057
```

## pipelined rep 1

```
ops = 24599875
errors = 0
elapsed_s = 10.001
ops_per_sec = 2459719
p50_us = 407
p99_us = 735
p999_us = 975
p9999_us = 1471
max_us = 2148
```

## pipelined rep 2

```
ops = 24308026
errors = 0
elapsed_s = 10.001
ops_per_sec = 2430504
p50_us = 415
p99_us = 751
p999_us = 1023
p9999_us = 1407
max_us = 2197
```
