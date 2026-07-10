# M0 gate-run report

date: 1783652820 (unix) · cells: 4 · replicates: 3 · duration: 10s
env-check: OK
tier: reference-box (binding)

notes:
- fabric RTT measured at loop granularity (shared.now updates once per step)
- comparator: dragonfly [0;32mv1.39.0-699862e5da7cb29bf5642a1da422c4c78cefae38[m · interleaved ABBA x 3 · persistence off both (ADR-0006 shape)
- attribution: domains 1098908480 B vs VmRSS 1129701376 B (2.7% divergence)

| gate | threshold | measured | verdict |
|---|---|---|---|
| Pipelined GET/SET node throughput | >= 6000000 ops/s | 2923460.59 | FAIL |
| Unpipelined throughput vs Redis, 512 conns | >= 1.5 x Redis | 3.04 | PASS |
| io_uring SQEs per submit under pipelined load | >= 16 sqes/submit | 17.83 | PASS |
| Fabric hop RTT p50 under load | < 2 us | 167.94 | FAIL |
| Cross-cell architecture vs Dragonfly, uniform random | >= 1.25 x Dragonfly | 1.74 | PASS |
| Cross-cell penalty, uniform random keys (informational; M1 S17 target) | <= 50 % vs all-local | 55.88 | FAIL (informational) |
| p99.9 latency (memtier, 8 threads) | < 3000 us | 831.00 | PASS |
| RSS @ 10M keys x (16 B, 64 B) | <= 1.1 x Redis | 0.61 | PASS |
| Reactor loop iteration p99.9 | < 500 us | 183.00 | PASS |
| Syscall CPU share under pipelined load | < 15 % | — | PENDING (tooling) |

## pipelined rep 0

```
ops = 29237965
errors = 0
elapsed_s = 10.001
ops_per_sec = 2923461
p50_us = 335
p99_us = 655
p999_us = 863
p9999_us = 11007
max_us = 11641
```

## pipelined rep 1

```
ops = 29055381
errors = 0
elapsed_s = 10.001
ops_per_sec = 2905160
p50_us = 343
p99_us = 639
p999_us = 831
p9999_us = 1215
max_us = 1984
```

## pipelined rep 2

```
ops = 29266654
errors = 0
elapsed_s = 10.001
ops_per_sec = 2926334
p50_us = 343
p99_us = 623
p999_us = 799
p9999_us = 1119
max_us = 1662
```
