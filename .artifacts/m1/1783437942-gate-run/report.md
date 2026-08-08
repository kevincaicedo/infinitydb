# M1 gate-run report

date: 1783437942 (unix) · cells: 4 · replicates: 3 · duration: 10s · storm: 1000000 · subs: 512×50
env-check: OK
tier: reference-box (binding)

notes:
- ttl-heavy: expired_active 1824128 · expired_lazy 571 across cells
- eviction pressure: 7350897 evictions; logical 251658176 B vs limit 268435456 B (resident incl. slack/buffers: 463504320 B)
- FLUSHALL command latency: 6.1 ms over 2000000 keys
- pub/sub registry pressure: 25600 subscriptions across 512 connections
- pub/sub deliveries drained by the fleet: 7937264
- slow-subscriber: killed=true · client_output_buffer_limit_disconnections=1

| gate | threshold | measured | verdict |
|---|---|---|---|
| M0 gates re-pass within 5% | <= 5 % vs M0 baseline | — | PENDING (tooling) |
| RSS vs Redis, 10M x (16 B, 64 B) | <= 1 x Redis | 0.61 | PASS |
| Expiry storm: foreground p99.9 | < 2000 us | 799.00 | PASS |
| Expiry storm: debt drains | < 10 s | 11.31 | FAIL |
| Eviction pressure: read p99.9 under write storm | < 2000 us | 1151.00 | PASS |
| Eviction bound: used memory vs maxmemory | <= 1.05 x maxmemory | 0.94 | PASS |
| FLUSHALL under load: read p99 | < 2000 us | 623.00 | PASS |
| TTL-heavy mix p99.9 (feature-pressure row) | < 2000 us | 2239.00 | FAIL (informational) |
| allkeys-lfu hit rate vs Redis (zipfian) | <= 2 pp below Redis | — | PENDING (tooling) |
| Pub/sub fan-out p99 (100k subscriptions) | < 5 ms | 0.68 | PASS |
| KV p99.9 with pub/sub background traffic | < 2000 us | 3007.00 | FAIL (informational) |
| Slow subscriber dies at the output cap | >= 1 killed (bool) | 1.00 | PASS |
| 100% byte-diff green on declared-full | >= 1 green (bool) | — | PENDING (tooling) |
| 24h soak: zero crashes, RSS slope | < 0.5 %/24h | — | PENDING (tooling) |
| Docker image size | < 30 MB | — | PENDING (tooling) |
| Client smoke green x4 libraries | >= 4 libraries | — | PENDING (tooling) |

## baseline rep 0

```
ops = 30203415
errors = 0
elapsed_s = 10.001
ops_per_sec = 3019986
p50_us = 327
p99_us = 607
p999_us = 847
p9999_us = 10239
max_us = 11274
```

## baseline rep 1

```
ops = 28728730
errors = 0
elapsed_s = 10.001
ops_per_sec = 2872549
p50_us = 343
p99_us = 655
p999_us = 895
p9999_us = 1279
max_us = 1695
```

## baseline rep 2

```
ops = 28395855
errors = 0
elapsed_s = 10.001
ops_per_sec = 2839191
p50_us = 343
p99_us = 655
p999_us = 863
p9999_us = 1279
max_us = 2021
```

## ttl-heavy

```
ops = 25043626
errors = 0
elapsed_s = 10.001
ops_per_sec = 2504025
p50_us = 391
p99_us = 767
p999_us = 2239
p9999_us = 18431
max_us = 19920
```

## expiry-storm reads

```
ops = 93287305
errors = 0
elapsed_s = 30.001
ops_per_sec = 3109464
p50_us = 319
p99_us = 591
p999_us = 799
p9999_us = 1151
max_us = 2007
```

## eviction-pressure

```
ops = 22542180
errors = 0
elapsed_s = 10.001
ops_per_sec = 2253922
p50_us = 439
p99_us = 847
p999_us = 1151
p9999_us = 1599
max_us = 2103
```

## flushall-under-load

```
ops = 31751807
errors = 0
elapsed_s = 10.001
ops_per_sec = 3174823
p50_us = 311
p99_us = 623
p999_us = 799
p9999_us = 1215
max_us = 5941
```

## pubsub-background

```
ops = 9615612
errors = 0
elapsed_s = 10.002
ops_per_sec = 961356
p50_us = 1087
p99_us = 2239
p999_us = 3007
p9999_us = 6527
max_us = 7437
```
