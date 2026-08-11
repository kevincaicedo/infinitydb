# M0 gate-run report

date: 1786418362 (unix) · cells: 4 · replicates: 5 · duration: 10s
env-check: OK
tier: reference-box (binding)

notes:
- fabric RTT measured at loop granularity (shared.now updates once per step)
- comparator: dragonfly [0;32mv1.39.0-699862e5da7cb29bf5642a1da422c4c78cefae38[m · interleaved ABBA x 5 · persistence off both (ADR-0006 shape)
- attribution: domains 1098941248 B (document 32768 B) vs VmRSS 1130496000 B (2.8% divergence)

| gate | threshold | measured | verdict |
|---|---|---|---|
| Pipelined GET/SET node throughput | >= 6000000 ops/s | 1924569.03 | FAIL |
| Unpipelined throughput vs Redis, 512 conns | >= 1.5 x Redis | 2.01 | PASS |
| io_uring SQEs per submit under pipelined load | >= 16 sqes/submit | 16.84 | PASS |
| Fabric hop RTT p50 under load | < 2 us | 270.33 | FAIL |
| Cross-cell architecture vs Dragonfly, uniform random | >= 1.25 x Dragonfly | 1.10 | FAIL |
| Cross-cell penalty, uniform random keys (informational; M1 S17 target) | <= 50 % vs all-local | 61.43 | FAIL (informational) |
| p99.9 latency (memtier, 8 threads) | < 3000 us | 1055.00 | PASS |
| RSS @ 10M keys x (16 B, 64 B) | <= 1.1 x Redis | 0.61 | PASS |
| Reactor loop iteration p99.9 | < 500 us | 215.00 | PASS |
| Syscall CPU share under pipelined load | < 15 % | — | PENDING (tooling) |

## pipelined rep 0

```
ops = 20270743
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2026772
p50_us = 503
p99_us = 863
p999_us = 1119
p9999_us = 7167
max_us = 11372
```

## pipelined rep 1

```
ops = 18560757
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 1855800
p50_us = 559
p99_us = 975
p999_us = 1183
p9999_us = 1279
max_us = 3347
```

## pipelined rep 2

```
ops = 19248491
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 1924569
p50_us = 527
p99_us = 927
p999_us = 1055
p9999_us = 1183
max_us = 2416
```

## pipelined rep 3

```
ops = 19460220
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 1945739
p50_us = 527
p99_us = 879
p999_us = 1007
p9999_us = 14335
max_us = 16563
```

## pipelined rep 4

```
ops = 19071700
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 1906942
p50_us = 527
p99_us = 927
p999_us = 1055
p9999_us = 1183
max_us = 3195
```
