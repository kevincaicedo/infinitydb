# M0 gate-run report

date: 1783391461 (unix) · cells: 4 · replicates: 5 · duration: 10s
env-check: OK
tier: reference-box (binding)

notes:
- fabric RTT measured at loop granularity (shared.now updates once per step)
- comparator: dragonfly [0;32mv1.39.0-699862e5da7cb29bf5642a1da422c4c78cefae38[m · interleaved ABBA x 5 · persistence off both (ADR-0006 shape)
- attribution: domains 1098908480 B vs VmRSS 1129345024 B (2.7% divergence)

| gate | threshold | measured | verdict |
|---|---|---|---|
| Pipelined GET/SET node throughput | >= 6000000 ops/s | 2514316.73 | FAIL |
| Unpipelined throughput vs Redis, 512 conns | >= 1.5 x Redis | 2.76 | PASS |
| io_uring SQEs per submit under pipelined load | >= 16 sqes/submit | 18.06 | PASS |
| Fabric hop RTT p50 under load | < 2 us | 217.09 | FAIL |
| Cross-cell architecture vs Dragonfly, uniform random | >= 1.25 x Dragonfly | 1.44 | PASS |
| Cross-cell penalty, uniform random keys (informational; M1 S17 target) | <= 50 % vs all-local | 60.38 | FAIL (informational) |
| p99.9 latency (memtier, 8 threads) | < 3000 us | 975.00 | PASS |
| RSS @ 10M keys x (16 B, 64 B) | <= 1.1 x Redis | 0.61 | PASS |
| Reactor loop iteration p99.9 | < 500 us | 227.00 | PASS |
| Syscall CPU share under pipelined load | < 15 % | — | PENDING (tooling) |

## pipelined rep 0

```
ops = 25188584
errors = 0
elapsed_s = 10.001
ops_per_sec = 2518580
p50_us = 399
p99_us = 767
p999_us = 1055
p9999_us = 10495
max_us = 11211
```

## pipelined rep 1

```
ops = 25084293
errors = 0
elapsed_s = 10.001
ops_per_sec = 2508150
p50_us = 399
p99_us = 735
p999_us = 959
p9999_us = 1343
max_us = 2030
```

## pipelined rep 2

```
ops = 25171826
errors = 0
elapsed_s = 10.001
ops_per_sec = 2516885
p50_us = 399
p99_us = 735
p999_us = 959
p9999_us = 1375
max_us = 1849
```

## pipelined rep 3

```
ops = 24711086
errors = 0
elapsed_s = 10.001
ops_per_sec = 2470852
p50_us = 407
p99_us = 751
p999_us = 975
p9999_us = 1343
max_us = 1796
```

## pipelined rep 4

```
ops = 25146237
errors = 0
elapsed_s = 10.001
ops_per_sec = 2514317
p50_us = 399
p99_us = 735
p999_us = 1023
p9999_us = 1471
max_us = 2383
```
