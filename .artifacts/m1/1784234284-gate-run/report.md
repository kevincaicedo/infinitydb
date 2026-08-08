# M1 gate-run report

date: 1784234284 (unix) · cells: 4 · replicates: 3 · duration: 10s · storm: 1000000 · subs: 512×50
env-check: FAILED (overridden — NOT citation-grade)
tier: dev (non-binding)

notes:
- env-check FAILED and was overridden (--unsafe-env): not citation-grade
- dev-tier run: reference-box gates report measured values, non-binding verdicts
- ttl-heavy: expired_active 1829950 · expired_lazy 596 across cells
- eviction pressure: 7460623 evictions; logical 251658176 B vs limit 268435456 B (resident incl. slack/buffers: 463537088 B)
- FLUSHALL command latency: 8.0 ms over 2000000 keys
- pub/sub registry pressure: 25600 subscriptions across 512 connections
- pub/sub deliveries drained by the fleet: 5178870
- slow-subscriber: killed=true · client_output_buffer_limit_disconnections=1

| gate | threshold | measured | verdict |
|---|---|---|---|
| M0 gates re-pass within 5% | <= 5 % vs M0 baseline | — | PENDING (tooling) |
| RSS vs Redis, 10M x (16 B, 64 B) | <= 1 x Redis | 0.61 | PASS (DEV-TIER, non-binding) |
| Expiry storm: foreground p99.9 | < 2000 us | 863.00 | PASS (DEV-TIER, non-binding) |
| Expiry storm: debt drains | < 10 s | 0.48 | PASS (DEV-TIER, non-binding) |
| Eviction pressure: read p99.9 under write storm | < 2000 us | 1119.00 | PASS (DEV-TIER, non-binding) |
| Eviction bound: used memory vs maxmemory | <= 1.05 x maxmemory | 0.94 | PASS (DEV-TIER, non-binding) |
| FLUSHALL under load: read p99 | < 2000 us | 623.00 | PASS (DEV-TIER, non-binding) |
| TTL-heavy mix p99.9 (feature-pressure row) | < 2000 us | 1247.00 | PASS (informational) |
| allkeys-lfu hit rate vs Redis (zipfian) | <= 2 pp below Redis | — | PENDING (tooling) |
| Pub/sub fan-out p99 (100k subscriptions) | < 5 ms | 0.56 | PASS (DEV-TIER, non-binding) |
| KV p99.9 with pub/sub background traffic | < 2000 us | 2815.00 | FAIL (informational) |
| Slow subscriber dies at the output cap | >= 1 killed (bool) | 1.00 | PASS |
| 100% byte-diff green on declared-full | >= 1 green (bool) | — | PENDING (tooling) |
| 24h soak: zero crashes, RSS slope | < 0.5 %/24h | — | PENDING (tooling) |
| Docker image size | < 30 MB | — | PENDING (tooling) |
| Client smoke green x4 libraries | >= 4 libraries | — | PENDING (tooling) |

## baseline rep 0

```
ops = 29150184
errors = 0
elapsed_s = 10.001
ops_per_sec = 2914674
p50_us = 343
p99_us = 639
p999_us = 911
p9999_us = 10751
max_us = 11361
```

## baseline rep 1

```
ops = 28536425
errors = 0
elapsed_s = 10.001
ops_per_sec = 2853343
p50_us = 351
p99_us = 655
p999_us = 895
p9999_us = 1247
max_us = 1516
```

## baseline rep 2

```
ops = 27382431
errors = 0
elapsed_s = 10.001
ops_per_sec = 2737893
p50_us = 359
p99_us = 703
p999_us = 975
p9999_us = 1439
max_us = 4200
```

## ttl-heavy

```
ops = 24304764
errors = 0
elapsed_s = 10.001
ops_per_sec = 2430215
p50_us = 399
p99_us = 783
p999_us = 1247
p9999_us = 14079
max_us = 15177
```

## expiry-storm reads

```
ops = 88129284
errors = 0
elapsed_s = 30.001
ops_per_sec = 2937524
p50_us = 335
p99_us = 655
p999_us = 863
p9999_us = 1279
max_us = 4007
```

## eviction-pressure

```
ops = 23033263
errors = 0
elapsed_s = 10.001
ops_per_sec = 2303018
p50_us = 439
p99_us = 799
p999_us = 1119
p9999_us = 1599
max_us = 3410
```

## flushall-under-load

```
ops = 31171412
errors = 0
elapsed_s = 10.001
ops_per_sec = 3116798
p50_us = 319
p99_us = 623
p999_us = 831
p9999_us = 1247
max_us = 7919
```

## pubsub-background

```
ops = 12034485
errors = 0
elapsed_s = 10.002
ops_per_sec = 1203250
p50_us = 815
p99_us = 1983
p999_us = 2815
p9999_us = 5759
max_us = 6513
```
