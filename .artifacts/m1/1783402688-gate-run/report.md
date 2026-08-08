# M1 gate-run report

date: 1783402688 (unix) · cells: 4 · replicates: 3 · duration: 10s · storm: 1000000 · subs: 512×50
env-check: OK
tier: reference-box (binding)

notes:
- ttl-heavy: expired_active 1820374 · expired_lazy 483 across cells
- eviction pressure: 7668585 evictions; logical 251658176 B vs limit 268435456 B (resident incl. slack/buffers: 463504320 B)
- FLUSHALL command latency: 8.2 ms over 2000000 keys
- pub/sub registry pressure: 25600 subscriptions across 512 connections
- pub/sub deliveries drained by the fleet: 5220347
- slow-subscriber: killed=true · client_output_buffer_limit_disconnections=1

| gate | threshold | measured | verdict |
|---|---|---|---|
| M0 gates re-pass within 5% | <= 5 % vs M0 baseline | — | PENDING (tooling) |
| RSS vs Redis, 10M x (16 B, 64 B) | <= 1 x Redis | 0.61 | PASS |
| Expiry storm: foreground p99.9 | < 2000 us | 799.00 | PASS |
| Expiry storm: debt drains | < 10 s | 11.28 | FAIL |
| Eviction pressure: read p99.9 under write storm | < 2000 us | 1119.00 | PASS |
| Eviction bound: used memory vs maxmemory | <= 1.05 x maxmemory | 0.94 | PASS |
| FLUSHALL under load: read p99 | < 2000 us | 575.00 | PASS |
| TTL-heavy mix p99.9 (feature-pressure row) | < 2000 us | 1215.00 | PASS (informational) |
| allkeys-lfu hit rate vs Redis (zipfian) | <= 2 pp below Redis | — | PENDING (tooling) |
| Pub/sub fan-out p99 (100k subscriptions) | < 5 ms | 0.67 | PASS |
| KV p99.9 with pub/sub background traffic | < 2000 us | 2623.00 | FAIL (informational) |
| Slow subscriber dies at the output cap | >= 1 killed (bool) | 1.00 | PASS |
| 100% byte-diff green on declared-full | >= 1 green (bool) | — | PENDING (tooling) |
| 24h soak: zero crashes, RSS slope | < 0.5 %/24h | — | PENDING (tooling) |
| Docker image size | < 30 MB | — | PENDING (tooling) |
| Client smoke green x4 libraries | >= 4 libraries | — | PENDING (tooling) |

## baseline rep 0

```
ops = 29560984
errors = 0
elapsed_s = 10.001
ops_per_sec = 2955757
p50_us = 335
p99_us = 639
p999_us = 895
p9999_us = 10751
max_us = 11466
```

## baseline rep 1

```
ops = 28640674
errors = 0
elapsed_s = 10.001
ops_per_sec = 2863691
p50_us = 351
p99_us = 687
p999_us = 879
p9999_us = 1279
max_us = 3806
```

## baseline rep 2

```
ops = 29581692
errors = 0
elapsed_s = 10.001
ops_per_sec = 2957797
p50_us = 351
p99_us = 671
p999_us = 831
p9999_us = 1087
max_us = 2994
```

## ttl-heavy

```
ops = 25791533
errors = 0
elapsed_s = 10.001
ops_per_sec = 2578801
p50_us = 383
p99_us = 735
p999_us = 1215
p9999_us = 14079
max_us = 14604
```

## expiry-storm reads

```
ops = 93153756
errors = 0
elapsed_s = 30.001
ops_per_sec = 3105015
p50_us = 319
p99_us = 607
p999_us = 799
p9999_us = 1119
max_us = 2276
```

## eviction-pressure

```
ops = 23473492
errors = 0
elapsed_s = 10.001
ops_per_sec = 2347011
p50_us = 423
p99_us = 815
p999_us = 1119
p9999_us = 1567
max_us = 2293
```

## flushall-under-load

```
ops = 32409556
errors = 0
elapsed_s = 10.001
ops_per_sec = 3240627
p50_us = 311
p99_us = 575
p999_us = 735
p9999_us = 1151
max_us = 8048
```

## pubsub-background

```
ops = 12366365
errors = 0
elapsed_s = 10.002
ops_per_sec = 1236414
p50_us = 799
p99_us = 1823
p999_us = 2623
p9999_us = 5887
max_us = 8230
```
