# M1 gate-run report

date: 1781312498 (unix) · cells: 4 · replicates: 1 · duration: 3s · storm: 100000 · subs: 64×8
env-check: FAILED (overridden — NOT citation-grade)
tier: dev (non-binding)

notes:
- env-check FAILED and was overridden (--unsafe-env): not citation-grade
- dev-tier run: reference-box gates report measured values, non-binding verdicts
- ttl-heavy: expired_active 576847 · expired_lazy 527 across cells
- eviction pressure: 17307769 evictions, used 163873696 B vs limit 67108864 B
- FLUSHALL command latency: 1.1 ms over 200000 keys
- pub/sub registry pressure: 512 subscriptions across 64 connections
- pub/sub deliveries drained by the fleet: 676325
- slow-subscriber: killed=true · client_output_buffer_limit_disconnections=1
- RSS fill leg skipped (--skip-fill)

| gate | threshold | measured | verdict |
|---|---|---|---|
| M0 gates re-pass within 5% | <= 5 % vs M0 baseline | — | PENDING (tooling) |
| RSS vs Redis, 10M x (16 B, 64 B) | <= 1 x Redis | — | PENDING (tooling) |
| Expiry storm: foreground p99.9 | < 2000 us | 1119.00 | PASS (DEV-TIER, non-binding) |
| Expiry storm: debt drains | < 10 s | 11.03 | FAIL (DEV-TIER, non-binding) |
| Eviction pressure: read p99.9 under write storm | < 2000 us | 1471.00 | PASS (DEV-TIER, non-binding) |
| Eviction bound: used memory vs maxmemory | <= 1.05 x maxmemory | 2.44 | FAIL (DEV-TIER, non-binding) |
| FLUSHALL under load: read p99 | < 2000 us | 391.00 | PASS (DEV-TIER, non-binding) |
| TTL-heavy mix p99.9 (feature-pressure row) | < 2000 us | 2175.00 | FAIL (informational) |
| allkeys-lfu hit rate vs Redis (zipfian) | <= 2 pp below Redis | — | PENDING (tooling) |
| Pub/sub fan-out p99 (100k subscriptions) | < 5 ms | 0.05 | PASS (DEV-TIER, non-binding) |
| KV p99.9 with pub/sub background traffic | < 2000 us | 703.00 | PASS (informational) |
| Slow subscriber dies at the output cap | >= 1 killed (bool) | 1.00 | PASS |
| 100% byte-diff green on declared-full | >= 1 green (bool) | — | PENDING (tooling) |
| 24h soak: zero crashes, RSS slope | < 0.5 %/24h | — | PENDING (tooling) |
| Docker image size | < 30 MB | — | PENDING (tooling) |
| Client smoke green x4 libraries | >= 4 libraries | — | PENDING (tooling) |

## baseline rep 0

```
ops = 12340980
errors = 0
elapsed_s = 3.001
ops_per_sec = 4112473
p50_us = 243
p99_us = 455
p999_us = 655
p9999_us = 4351
max_us = 4569
```

## ttl-heavy

```
ops = 9963011
errors = 0
elapsed_s = 3.001
ops_per_sec = 3319833
p50_us = 295
p99_us = 623
p999_us = 2175
p9999_us = 17919
max_us = 21933
```

## expiry-storm reads

```
ops = 134209733
errors = 0
elapsed_s = 30.001
ops_per_sec = 4473526
p50_us = 219
p99_us = 431
p999_us = 1119
p9999_us = 2111
max_us = 4011
```

## eviction-pressure

```
ops = 35500432
errors = 0
elapsed_s = 10.001
ops_per_sec = 3549757
p50_us = 279
p99_us = 559
p999_us = 1471
p9999_us = 2623
max_us = 4757
```

## flushall-under-load

```
ops = 47256771
errors = 0
elapsed_s = 10.001
ops_per_sec = 4725279
p50_us = 211
p99_us = 391
p999_us = 1007
p9999_us = 1791
max_us = 3931
```

## pubsub-background

```
ops = 11186241
errors = 0
elapsed_s = 3.001
ops_per_sec = 3727435
p50_us = 271
p99_us = 487
p999_us = 703
p9999_us = 4607
max_us = 10663
```
