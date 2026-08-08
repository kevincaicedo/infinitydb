# M0 gate-run report

date: 1783698386 (unix) · cells: 4 · replicates: 5 · duration: 10s
env-check: OK
tier: reference-box (binding)

notes:
- fabric RTT measured at loop granularity (shared.now updates once per step)
- comparator: dragonfly [0;32mv1.39.0-699862e5da7cb29bf5642a1da422c4c78cefae38[m · interleaved ABBA x 5 · persistence off both (ADR-0006 shape)
- attribution: domains 1098908480 B vs VmRSS 1129771008 B (2.7% divergence)

| gate | threshold | measured | verdict |
|---|---|---|---|
| Pipelined GET/SET node throughput | >= 6000000 ops/s | 2835625.96 | FAIL |
| Unpipelined throughput vs Redis, 512 conns | >= 1.5 x Redis | 3.21 | PASS |
| io_uring SQEs per submit under pipelined load | >= 16 sqes/submit | 17.05 | PASS |
| Fabric hop RTT p50 under load | < 2 us | 172.03 | FAIL |
| Cross-cell architecture vs Dragonfly, uniform random | >= 1.25 x Dragonfly | 1.72 | PASS |
| Cross-cell penalty, uniform random keys (informational; M1 S17 target) | <= 50 % vs all-local | 64.32 | FAIL (informational) |
| p99.9 latency (memtier, 8 threads) | < 3000 us | 847.00 | PASS |
| RSS @ 10M keys x (16 B, 64 B) | <= 1.1 x Redis | 0.61 | PASS |
| Reactor loop iteration p99.9 | < 500 us | 191.00 | PASS |
| Syscall CPU share under pipelined load | < 15 % | — | PENDING (tooling) |

## pipelined rep 0

```
ops = 28073641
errors = 0
elapsed_s = 10.001
ops_per_sec = 2807045
p50_us = 351
p99_us = 671
p999_us = 943
p9999_us = 11775
max_us = 12708
```

## pipelined rep 1

```
ops = 28569978
errors = 0
elapsed_s = 10.001
ops_per_sec = 2856631
p50_us = 351
p99_us = 623
p999_us = 847
p9999_us = 1279
max_us = 4069
```

## pipelined rep 2

```
ops = 28923547
errors = 0
elapsed_s = 10.001
ops_per_sec = 2891964
p50_us = 351
p99_us = 591
p999_us = 799
p9999_us = 1535
max_us = 4200
```

## pipelined rep 3

```
ops = 28185923
errors = 0
elapsed_s = 10.001
ops_per_sec = 2818262
p50_us = 359
p99_us = 687
p999_us = 879
p9999_us = 1439
max_us = 4041
```

## pipelined rep 4

```
ops = 28359761
errors = 0
elapsed_s = 10.001
ops_per_sec = 2835626
p50_us = 359
p99_us = 639
p999_us = 815
p9999_us = 1215
max_us = 3329
```
