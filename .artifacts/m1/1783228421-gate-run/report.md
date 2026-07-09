# M1 gate-run report

date: 1783228421 (unix) · cells: 4 · replicates: 3 · duration: 10s · storm: 1000000 · subs: 512×50
env-check: OK
tier: reference-box (binding)

notes:
- ttl-heavy: expired_active 1824230 · expired_lazy 493 across cells
- eviction pressure: 6883439 evictions; logical 251658176 B vs limit 268435456 B (resident incl. slack/buffers: 463504320 B)
- FLUSHALL command latency: 8.8 ms over 2000000 keys
- pub/sub registry pressure: 25600 subscriptions across 512 connections
- pub/sub deliveries drained by the fleet: 5169006
- slow-subscriber: killed=true · client_output_buffer_limit_disconnections=1

| gate | threshold | measured | verdict |
|---|---|---|---|
| M0 gates re-pass within 5% | <= 5 % vs M0 baseline | — | PENDING (tooling) |
| RSS vs Redis, 10M x (16 B, 64 B) | <= 1 x Redis | 0.61 | PASS |
| Expiry storm: foreground p99.9 | < 2000 us | 927.00 | PASS |
| Expiry storm: debt drains | < 10 s | 11.34 | FAIL |
| Eviction pressure: read p99.9 under write storm | < 2000 us | 1183.00 | PASS |
| Eviction bound: used memory vs maxmemory | <= 1.05 x maxmemory | 0.94 | PASS |
| FLUSHALL under load: read p99 | < 2000 us | 655.00 | PASS |
| TTL-heavy mix p99.9 (feature-pressure row) | < 2000 us | 3263.00 | FAIL (informational) |
| allkeys-lfu hit rate vs Redis (zipfian) | <= 2 pp below Redis | — | PENDING (tooling) |
| Pub/sub fan-out p99 (100k subscriptions) | < 5 ms | 0.69 | PASS |
| KV p99.9 with pub/sub background traffic | < 2000 us | 2879.00 | FAIL (informational) |
| Slow subscriber dies at the output cap | >= 1 killed (bool) | 1.00 | PASS |
| 100% byte-diff green on declared-full | >= 1 green (bool) | — | PENDING (tooling) |
| 24h soak: zero crashes, RSS slope | < 0.5 %/24h | — | PENDING (tooling) |
| Docker image size | < 30 MB | — | PENDING (tooling) |
| Client smoke green x4 libraries | >= 4 libraries | — | PENDING (tooling) |

## baseline rep 0

```
ops = 25648191
errors = 0
elapsed_s = 10.001
ops_per_sec = 2564467
p50_us = 383
p99_us = 751
p999_us = 1087
p9999_us = 10751
max_us = 11404
```

## baseline rep 1

```
ops = 25116453
errors = 0
elapsed_s = 10.001
ops_per_sec = 2511295
p50_us = 399
p99_us = 687
p999_us = 927
p9999_us = 1407
max_us = 2963
```

## baseline rep 2

```
ops = 23146860
errors = 0
elapsed_s = 10.001
ops_per_sec = 2314367
p50_us = 439
p99_us = 895
p999_us = 1055
p9999_us = 1407
max_us = 2018
```

## ttl-heavy

```
ops = 22108003
errors = 0
elapsed_s = 10.001
ops_per_sec = 2210527
p50_us = 439
p99_us = 847
p999_us = 3263
p9999_us = 16383
max_us = 17701
```

## expiry-storm reads

```
ops = 79997365
errors = 0
elapsed_s = 30.001
ops_per_sec = 2666472
p50_us = 375
p99_us = 719
p999_us = 927
p9999_us = 1311
max_us = 2682
```

## eviction-pressure

```
ops = 21184660
errors = 0
elapsed_s = 10.001
ops_per_sec = 2118159
p50_us = 471
p99_us = 911
p999_us = 1183
p9999_us = 1599
max_us = 2571
```

## flushall-under-load

```
ops = 29146139
errors = 0
elapsed_s = 10.001
ops_per_sec = 2914293
p50_us = 343
p99_us = 655
p999_us = 847
p9999_us = 1375
max_us = 8639
```

## pubsub-background

```
ops = 11569962
errors = 0
elapsed_s = 10.002
ops_per_sec = 1156769
p50_us = 863
p99_us = 2111
p999_us = 2879
p9999_us = 6015
max_us = 7353
```
