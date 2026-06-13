# M1 gate-run report

date: 1781312636 (unix) · cells: 4 · replicates: 1 · duration: 3s · storm: 100000 · subs: 64×8
env-check: FAILED (overridden — NOT citation-grade)
tier: dev (non-binding)

notes:
- env-check FAILED and was overridden (--unsafe-env): not citation-grade
- dev-tier run: reference-box gates report measured values, non-binding verdicts
- ttl-heavy: expired_active 570063 · expired_lazy 133 across cells
- eviction pressure: 17351321 evictions; logical 62913344 B vs limit 67108864 B (resident incl. slack/buffers: 163873696 B)
- FLUSHALL command latency: 0.9 ms over 200000 keys
- pub/sub registry pressure: 512 subscriptions across 64 connections
- pub/sub deliveries drained by the fleet: 478698
- slow-subscriber: killed=true · client_output_buffer_limit_disconnections=1
- RSS fill leg skipped (--skip-fill)

| gate | threshold | measured | verdict |
|---|---|---|---|
| M0 gates re-pass within 5% | <= 5 % vs M0 baseline | — | PENDING (tooling) |
| RSS vs Redis, 10M x (16 B, 64 B) | <= 1 x Redis | — | PENDING (tooling) |
| Expiry storm: foreground p99.9 | < 2000 us | 1151.00 | PASS (DEV-TIER, non-binding) |
| Expiry storm: debt drains | < 10 s | 11.02 | FAIL (DEV-TIER, non-binding) |
| Eviction pressure: read p99.9 under write storm | < 2000 us | 1631.00 | PASS (DEV-TIER, non-binding) |
| Eviction bound: used memory vs maxmemory | <= 1.05 x maxmemory | 0.94 | PASS (DEV-TIER, non-binding) |
| FLUSHALL under load: read p99 | < 2000 us | 399.00 | PASS (DEV-TIER, non-binding) |
| TTL-heavy mix p99.9 (feature-pressure row) | < 2000 us | 2367.00 | FAIL (informational) |
| allkeys-lfu hit rate vs Redis (zipfian) | <= 2 pp below Redis | — | PENDING (tooling) |
| Pub/sub fan-out p99 (100k subscriptions) | < 5 ms | 0.05 | PASS (DEV-TIER, non-binding) |
| KV p99.9 with pub/sub background traffic | < 2000 us | 2015.00 | FAIL (informational) |
| Slow subscriber dies at the output cap | >= 1 killed (bool) | 1.00 | PASS |
| 100% byte-diff green on declared-full | >= 1 green (bool) | — | PENDING (tooling) |
| 24h soak: zero crashes, RSS slope | < 0.5 %/24h | — | PENDING (tooling) |
| Docker image size | < 30 MB | — | PENDING (tooling) |
| Client smoke green x4 libraries | >= 4 libraries | — | PENDING (tooling) |

## baseline rep 0

```
ops = 12619360
errors = 0
elapsed_s = 3.001
ops_per_sec = 4205140
p50_us = 239
p99_us = 423
p999_us = 575
p9999_us = 4223
max_us = 4944
```

## ttl-heavy

```
ops = 10223326
errors = 0
elapsed_s = 3.001
ops_per_sec = 3406894
p50_us = 287
p99_us = 607
p999_us = 2367
p9999_us = 8703
max_us = 10372
```

## expiry-storm reads

```
ops = 131638252
errors = 0
elapsed_s = 30.001
ops_per_sec = 4387836
p50_us = 223
p99_us = 455
p999_us = 1151
p9999_us = 2111
max_us = 3616
```

## eviction-pressure

```
ops = 35290898
errors = 0
elapsed_s = 10.001
ops_per_sec = 3528794
p50_us = 279
p99_us = 559
p999_us = 1631
p9999_us = 2687
max_us = 4811
```

## flushall-under-load

```
ops = 47513405
errors = 0
elapsed_s = 10.001
ops_per_sec = 4750957
p50_us = 211
p99_us = 399
p999_us = 1023
p9999_us = 2015
max_us = 3746
```

## pubsub-background

```
ops = 10399655
errors = 0
elapsed_s = 3.001
ops_per_sec = 3465369
p50_us = 279
p99_us = 607
p999_us = 2015
p9999_us = 4863
max_us = 5284
```
