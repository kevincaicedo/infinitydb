# M1 gate-run report

date: 1784232097 (unix) · cells: 4 · replicates: 3 · duration: 10s · storm: 1000000 · subs: 512×50
env-check: FAILED (overridden — NOT citation-grade)
tier: dev (non-binding)

notes:
- env-check FAILED and was overridden (--unsafe-env): not citation-grade
- dev-tier run: reference-box gates report measured values, non-binding verdicts
- ttl-heavy: expired_active 1825022 · expired_lazy 591 across cells
- eviction pressure: 7545523 evictions; logical 251658176 B vs limit 268435456 B (resident incl. slack/buffers: 463543322 B)
- FLUSHALL command latency: 7.4 ms over 2000000 keys
- pub/sub registry pressure: 25600 subscriptions across 512 connections
- pub/sub deliveries drained by the fleet: 7294773
- slow-subscriber: killed=true · client_output_buffer_limit_disconnections=1

| gate | threshold | measured | verdict |
|---|---|---|---|
| M0 gates re-pass within 5% | <= 5 % vs M0 baseline | — | PENDING (tooling) |
| RSS vs Redis, 10M x (16 B, 64 B) | <= 1 x Redis | 0.61 | PASS (DEV-TIER, non-binding) |
| Expiry storm: foreground p99.9 | < 2000 us | 799.00 | PASS (DEV-TIER, non-binding) |
| Expiry storm: debt drains | < 10 s | 0.45 | PASS (DEV-TIER, non-binding) |
| Eviction pressure: read p99.9 under write storm | < 2000 us | 1055.00 | PASS (DEV-TIER, non-binding) |
| Eviction bound: used memory vs maxmemory | <= 1.05 x maxmemory | 0.94 | PASS (DEV-TIER, non-binding) |
| FLUSHALL under load: read p99 | < 2000 us | 575.00 | PASS (DEV-TIER, non-binding) |
| TTL-heavy mix p99.9 (feature-pressure row) | < 2000 us | 2303.00 | FAIL (informational) |
| allkeys-lfu hit rate vs Redis (zipfian) | <= 2 pp below Redis | — | PENDING (tooling) |
| Pub/sub fan-out p99 (100k subscriptions) | < 5 ms | 0.76 | PASS (DEV-TIER, non-binding) |
| KV p99.9 with pub/sub background traffic | < 2000 us | 2943.00 | FAIL (informational) |
| Slow subscriber dies at the output cap | >= 1 killed (bool) | 1.00 | PASS |
| 100% byte-diff green on declared-full | >= 1 green (bool) | — | PENDING (tooling) |
| 24h soak: zero crashes, RSS slope | < 0.5 %/24h | — | PENDING (tooling) |
| Docker image size | < 30 MB | — | PENDING (tooling) |
| Client smoke green x4 libraries | >= 4 libraries | — | PENDING (tooling) |

## baseline rep 0

```
ops = 30056138
errors = 0
elapsed_s = 10.001
ops_per_sec = 3005199
p50_us = 335
p99_us = 591
p999_us = 879
p9999_us = 10495
max_us = 11055
```

## baseline rep 1

```
ops = 29496675
errors = 0
elapsed_s = 10.001
ops_per_sec = 2949276
p50_us = 343
p99_us = 623
p999_us = 815
p9999_us = 1247
max_us = 2781
```

## baseline rep 2

```
ops = 28101339
errors = 0
elapsed_s = 10.001
ops_per_sec = 2809816
p50_us = 391
p99_us = 671
p999_us = 815
p9999_us = 1215
max_us = 3162
```

## ttl-heavy

```
ops = 25214167
errors = 0
elapsed_s = 10.001
ops_per_sec = 2521121
p50_us = 383
p99_us = 767
p999_us = 2303
p9999_us = 17919
max_us = 18747
```

## expiry-storm reads

```
ops = 92830781
errors = 0
elapsed_s = 30.001
ops_per_sec = 3094255
p50_us = 327
p99_us = 575
p999_us = 799
p9999_us = 1279
max_us = 3688
```

## eviction-pressure

```
ops = 23100350
errors = 0
elapsed_s = 10.001
ops_per_sec = 2309710
p50_us = 439
p99_us = 831
p999_us = 1055
p9999_us = 1439
max_us = 2514
```

## flushall-under-load

```
ops = 32310375
errors = 0
elapsed_s = 10.001
ops_per_sec = 3230579
p50_us = 335
p99_us = 575
p999_us = 735
p9999_us = 1727
max_us = 7519
```

## pubsub-background

```
ops = 10599878
errors = 0
elapsed_s = 10.002
ops_per_sec = 1059793
p50_us = 847
p99_us = 2047
p999_us = 2943
p9999_us = 6271
max_us = 7606
```
