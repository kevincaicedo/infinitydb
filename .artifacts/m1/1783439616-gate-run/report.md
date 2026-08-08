# M1 gate-run report

date: 1783439616 (unix) · cells: 4 · replicates: 3 · duration: 10s · storm: 1000000 · subs: 512×50
env-check: OK
tier: reference-box (binding)

notes:
- ttl-heavy: expired_active 1824357 · expired_lazy 526 across cells
- eviction pressure: 7350952 evictions; logical 251658176 B vs limit 268435456 B (resident incl. slack/buffers: 463504320 B)
- FLUSHALL command latency: 6.9 ms over 2000000 keys
- pub/sub registry pressure: 25600 subscriptions across 512 connections
- pub/sub deliveries drained by the fleet: 8133107
- slow-subscriber: killed=true · client_output_buffer_limit_disconnections=1

| gate | threshold | measured | verdict |
|---|---|---|---|
| M0 gates re-pass within 5% | <= 5 % vs M0 baseline | — | PENDING (tooling) |
| RSS vs Redis, 10M x (16 B, 64 B) | <= 1 x Redis | 0.61 | PASS |
| Expiry storm: foreground p99.9 | < 2000 us | 863.00 | PASS |
| Expiry storm: debt drains | < 10 s | 0.44 | PASS |
| Eviction pressure: read p99.9 under write storm | < 2000 us | 1119.00 | PASS |
| Eviction bound: used memory vs maxmemory | <= 1.05 x maxmemory | 0.94 | PASS |
| FLUSHALL under load: read p99 | < 2000 us | 623.00 | PASS |
| TTL-heavy mix p99.9 (feature-pressure row) | < 2000 us | 1631.00 | PASS (informational) |
| allkeys-lfu hit rate vs Redis (zipfian) | <= 2 pp below Redis | — | PENDING (tooling) |
| Pub/sub fan-out p99 (100k subscriptions) | < 5 ms | 0.65 | PASS |
| KV p99.9 with pub/sub background traffic | < 2000 us | 3007.00 | FAIL (informational) |
| Slow subscriber dies at the output cap | >= 1 killed (bool) | 1.00 | PASS |
| 100% byte-diff green on declared-full | >= 1 green (bool) | — | PENDING (tooling) |
| 24h soak: zero crashes, RSS slope | < 0.5 %/24h | — | PENDING (tooling) |
| Docker image size | < 30 MB | — | PENDING (tooling) |
| Client smoke green x4 libraries | >= 4 libraries | — | PENDING (tooling) |

## baseline rep 0

```
ops = 29936543
errors = 0
elapsed_s = 10.001
ops_per_sec = 2993338
p50_us = 327
p99_us = 623
p999_us = 847
p9999_us = 10495
max_us = 10996
```

## baseline rep 1

```
ops = 29712633
errors = 0
elapsed_s = 10.001
ops_per_sec = 2970942
p50_us = 335
p99_us = 639
p999_us = 847
p9999_us = 1247
max_us = 1897
```

## baseline rep 2

```
ops = 29024976
errors = 0
elapsed_s = 10.001
ops_per_sec = 2902140
p50_us = 343
p99_us = 623
p999_us = 831
p9999_us = 1183
max_us = 2024
```

## ttl-heavy

```
ops = 24817266
errors = 0
elapsed_s = 10.001
ops_per_sec = 2481452
p50_us = 391
p99_us = 783
p999_us = 1631
p9999_us = 14847
max_us = 15734
```

## expiry-storm reads

```
ops = 89249942
errors = 0
elapsed_s = 30.001
ops_per_sec = 2974884
p50_us = 335
p99_us = 655
p999_us = 863
p9999_us = 1279
max_us = 4159
```

## eviction-pressure

```
ops = 22569147
errors = 0
elapsed_s = 10.001
ops_per_sec = 2256671
p50_us = 439
p99_us = 863
p999_us = 1119
p9999_us = 1439
max_us = 2432
```

## flushall-under-load

```
ops = 31549936
errors = 0
elapsed_s = 10.001
ops_per_sec = 3154593
p50_us = 319
p99_us = 623
p999_us = 847
p9999_us = 1535
max_us = 6721
```

## pubsub-background

```
ops = 9644607
errors = 0
elapsed_s = 10.002
ops_per_sec = 964294
p50_us = 1055
p99_us = 2175
p999_us = 3007
p9999_us = 10239
max_us = 11210
```
