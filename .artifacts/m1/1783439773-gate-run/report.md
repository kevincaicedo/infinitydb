# M1 gate-run report

date: 1783439773 (unix) · cells: 4 · replicates: 3 · duration: 10s · storm: 1000000 · subs: 512×50
env-check: OK
tier: reference-box (binding)

notes:
- ttl-heavy: expired_active 1825523 · expired_lazy 518 across cells
- eviction pressure: 7570310 evictions; logical 251658176 B vs limit 268435456 B (resident incl. slack/buffers: 463504320 B)
- FLUSHALL command latency: 7.5 ms over 2000000 keys
- pub/sub registry pressure: 25600 subscriptions across 512 connections
- pub/sub deliveries drained by the fleet: 5528310
- slow-subscriber: killed=true · client_output_buffer_limit_disconnections=1

| gate | threshold | measured | verdict |
|---|---|---|---|
| M0 gates re-pass within 5% | <= 5 % vs M0 baseline | — | PENDING (tooling) |
| RSS vs Redis, 10M x (16 B, 64 B) | <= 1 x Redis | 0.61 | PASS |
| Expiry storm: foreground p99.9 | < 2000 us | 831.00 | PASS |
| Expiry storm: debt drains | < 10 s | 0.48 | PASS |
| Eviction pressure: read p99.9 under write storm | < 2000 us | 1119.00 | PASS |
| Eviction bound: used memory vs maxmemory | <= 1.05 x maxmemory | 0.94 | PASS |
| FLUSHALL under load: read p99 | < 2000 us | 591.00 | PASS |
| TTL-heavy mix p99.9 (feature-pressure row) | < 2000 us | 1343.00 | PASS (informational) |
| allkeys-lfu hit rate vs Redis (zipfian) | <= 2 pp below Redis | — | PENDING (tooling) |
| Pub/sub fan-out p99 (100k subscriptions) | < 5 ms | 0.67 | PASS |
| KV p99.9 with pub/sub background traffic | < 2000 us | 2687.00 | FAIL (informational) |
| Slow subscriber dies at the output cap | >= 1 killed (bool) | 1.00 | PASS |
| 100% byte-diff green on declared-full | >= 1 green (bool) | — | PENDING (tooling) |
| 24h soak: zero crashes, RSS slope | < 0.5 %/24h | — | PENDING (tooling) |
| Docker image size | < 30 MB | — | PENDING (tooling) |
| Client smoke green x4 libraries | >= 4 libraries | — | PENDING (tooling) |

## baseline rep 0

```
ops = 29042362
errors = 0
elapsed_s = 10.001
ops_per_sec = 2903912
p50_us = 343
p99_us = 639
p999_us = 895
p9999_us = 10495
max_us = 12017
```

## baseline rep 1

```
ops = 28008058
errors = 0
elapsed_s = 10.001
ops_per_sec = 2800418
p50_us = 351
p99_us = 687
p999_us = 895
p9999_us = 1279
max_us = 1794
```

## baseline rep 2

```
ops = 27921683
errors = 0
elapsed_s = 10.001
ops_per_sec = 2791856
p50_us = 359
p99_us = 703
p999_us = 847
p9999_us = 1055
max_us = 1651
```

## ttl-heavy

```
ops = 25778953
errors = 0
elapsed_s = 10.001
ops_per_sec = 2577544
p50_us = 375
p99_us = 735
p999_us = 1343
p9999_us = 15359
max_us = 16324
```

## expiry-storm reads

```
ops = 93625356
errors = 0
elapsed_s = 30.001
ops_per_sec = 3120735
p50_us = 319
p99_us = 607
p999_us = 831
p9999_us = 1247
max_us = 3267
```

## eviction-pressure

```
ops = 23215266
errors = 0
elapsed_s = 10.001
ops_per_sec = 2321255
p50_us = 431
p99_us = 831
p999_us = 1119
p9999_us = 1599
max_us = 2443
```

## flushall-under-load

```
ops = 32196315
errors = 0
elapsed_s = 10.001
ops_per_sec = 3219232
p50_us = 311
p99_us = 591
p999_us = 751
p9999_us = 1183
max_us = 7508
```

## pubsub-background

```
ops = 12156364
errors = 0
elapsed_s = 10.002
ops_per_sec = 1215350
p50_us = 815
p99_us = 1919
p999_us = 2687
p9999_us = 7167
max_us = 8698
```
