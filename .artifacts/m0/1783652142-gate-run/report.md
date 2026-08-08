# M0 gate-run report

date: 1783652142 (unix) · cells: 4 · replicates: 3 · duration: 10s
env-check: OK
tier: reference-box (binding)

notes:
- fabric RTT measured at loop granularity (shared.now updates once per step)
- comparator: dragonfly [0;32mv1.39.0-699862e5da7cb29bf5642a1da422c4c78cefae38[m · interleaved ABBA x 3 · persistence off both (ADR-0006 shape)
- attribution: domains 1098908480 B vs VmRSS 1129697280 B (2.7% divergence)

| gate | threshold | measured | verdict |
|---|---|---|---|
| Pipelined GET/SET node throughput | >= 6000000 ops/s | 2880674.11 | FAIL |
| Unpipelined throughput vs Redis, 512 conns | >= 1.5 x Redis | 2.85 | PASS |
| io_uring SQEs per submit under pipelined load | >= 16 sqes/submit | 16.01 | PASS |
| Fabric hop RTT p50 under load | < 2 us | 172.03 | FAIL |
| Cross-cell architecture vs Dragonfly, uniform random | >= 1.25 x Dragonfly | 1.67 | PASS |
| Cross-cell penalty, uniform random keys (informational; M1 S17 target) | <= 50 % vs all-local | 63.88 | FAIL (informational) |
| p99.9 latency (memtier, 8 threads) | < 3000 us | 863.00 | PASS |
| RSS @ 10M keys x (16 B, 64 B) | <= 1.1 x Redis | 0.61 | PASS |
| Reactor loop iteration p99.9 | < 500 us | 195.00 | PASS |
| Syscall CPU share under pipelined load | < 15 % | — | PENDING (tooling) |

## pipelined rep 0

```
ops = 29035720
errors = 0
elapsed_s = 10.001
ops_per_sec = 2903196
p50_us = 335
p99_us = 671
p999_us = 895
p9999_us = 10495
max_us = 11040
```

## pipelined rep 1

```
ops = 28809888
errors = 0
elapsed_s = 10.001
ops_per_sec = 2880674
p50_us = 343
p99_us = 655
p999_us = 863
p9999_us = 1247
max_us = 1646
```

## pipelined rep 2

```
ops = 28651836
errors = 0
elapsed_s = 10.001
ops_per_sec = 2864856
p50_us = 359
p99_us = 639
p999_us = 847
p9999_us = 1215
max_us = 1792
```
