# M0 gate-run report

date: 1783698588 (unix) · cells: 8 · replicates: 3 · duration: 10s
env-check: OK
tier: reference-box (binding)

notes:
- fabric RTT measured at loop granularity (shared.now updates once per step)
- comparator: dragonfly [0;32mv1.39.0-699862e5da7cb29bf5642a1da422c4c78cefae38[m · interleaved ABBA x 3 · persistence off both (ADR-0006 shape)
- attribution: domains 1174406784 B vs VmRSS 1215176704 B (3.4% divergence)

| gate | threshold | measured | verdict |
|---|---|---|---|
| Pipelined GET/SET node throughput | >= 6000000 ops/s | 3956579.32 | FAIL |
| Unpipelined throughput vs Redis, 512 conns | >= 1.5 x Redis | 4.21 | PASS |
| io_uring SQEs per submit under pipelined load | >= 16 sqes/submit | 7.93 | FAIL |
| Fabric hop RTT p50 under load | < 2 us | 118.78 | FAIL |
| Cross-cell architecture vs Dragonfly, uniform random | >= 1.25 x Dragonfly | 1.41 | PASS |
| Cross-cell penalty, uniform random keys (informational; M1 S17 target) | <= 50 % vs all-local | 64.69 | FAIL (informational) |
| p99.9 latency (memtier, 8 threads) | < 3000 us | 927.00 | PASS |
| RSS @ 10M keys x (16 B, 64 B) | <= 1.1 x Redis | 0.66 | PASS |
| Reactor loop iteration p99.9 | < 500 us | 121.00 | PASS |
| Syscall CPU share under pipelined load | < 15 % | — | PENDING (tooling) |

## pipelined rep 0

```
ops = 40242062
errors = 0
elapsed_s = 10.001
ops_per_sec = 4023648
p50_us = 243
p99_us = 479
p999_us = 959
p9999_us = 5247
max_us = 6497
```

## pipelined rep 1

```
ops = 39570863
errors = 0
elapsed_s = 10.001
ops_per_sec = 3956579
p50_us = 243
p99_us = 527
p999_us = 863
p9999_us = 2431
max_us = 4878
```

## pipelined rep 2

```
ops = 39166560
errors = 0
elapsed_s = 10.001
ops_per_sec = 3916242
p50_us = 251
p99_us = 527
p999_us = 927
p9999_us = 2495
max_us = 6575
```
