# M1 gate-run report

date: 1786420253 (unix) · cells: 4 · replicates: 3 · duration: 10s · storm: 1000000 · subs: 512×50
env-check: OK
tier: reference-box (binding)

notes:
- ttl-heavy: expired_active 1826549 · expired_lazy 581 across cells
- eviction pressure: 7462916 evictions; logical 251658176 B vs limit 268435456 B (resident incl. slack/buffers: 463537088 B)
- FLUSHALL command latency: 7.2 ms over 2000000 keys
- pub/sub registry pressure: 25600 subscriptions across 512 connections
- pub/sub deliveries drained by the fleet: 5482496
- slow-subscriber: killed=true · client_output_buffer_limit_disconnections=1

| gate | threshold | measured | verdict |
|---|---|---|---|
| M0 gates re-pass within 5% | <= 5 % vs M0 baseline | — | PENDING (tooling) |
| RSS vs Redis, 10M x (16 B, 64 B) | <= 1 x Redis | 0.61 | PASS |
| Expiry storm: foreground p99.9 | < 2000 us | 831.00 | PASS |
| Expiry storm: debt drains | < 10 s | 0.52 | PASS |
| Eviction pressure: read p99.9 under write storm | < 2000 us | 1119.00 | PASS |
| Eviction bound: used memory vs maxmemory | <= 1.05 x maxmemory | 0.94 | PASS |
| FLUSHALL under load: read p99 | < 2000 us | 639.00 | PASS |
| TTL-heavy mix p99.9 (feature-pressure row) | < 2000 us | 1439.00 | PASS (informational) |
| allkeys-lfu hit rate vs Redis (zipfian) | <= 2 pp below Redis | 0.00 | PASS |
| Pub/sub fan-out p99 (100k subscriptions) | < 5 ms | 0.79 | PASS |
| KV p99.9 with pub/sub background traffic | < 2000 us | 2687.00 | FAIL (informational) |
| Slow subscriber dies at the output cap | >= 1 killed (bool) | 1.00 | PASS |
| 100% byte-diff green on declared-full | >= 1 green (bool) | — | PENDING (tooling) |
| 24h soak: zero crashes, RSS slope | < 0.5 %/24h | — | PENDING (tooling) |
| Docker image size | < 10 MB | — | PENDING (tooling) |
| Client smoke green x4 libraries | >= 4 libraries | — | PENDING (tooling) |

## baseline rep 0

```
ops = 28154106
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2815039
p50_us = 351
p99_us = 687
p999_us = 975
p9999_us = 10495
max_us = 11671
```

## baseline rep 1

```
ops = 28199136
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2819585
p50_us = 383
p99_us = 735
p999_us = 847
p9999_us = 1151
max_us = 1671
```

## baseline rep 2

```
ops = 28792969
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2878963
p50_us = 343
p99_us = 655
p999_us = 847
p9999_us = 1087
max_us = 1897
```

## ttl-heavy

```
ops = 25305285
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2530217
p50_us = 383
p99_us = 735
p999_us = 1439
p9999_us = 15615
max_us = 16478
```

## expiry-storm reads

```
ops = 90884232
errors = 0
busy_retryable = 0
elapsed_s = 30.001
ops_per_sec = 3029344
p50_us = 327
p99_us = 639
p999_us = 831
p9999_us = 1151
max_us = 2727
```

## eviction-pressure

```
ops = 22907664
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2290449
p50_us = 431
p99_us = 815
p999_us = 1119
p9999_us = 1535
max_us = 2216
```

## flushall-under-load

```
ops = 30547675
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3054437
p50_us = 319
p99_us = 639
p999_us = 831
p9999_us = 1279
max_us = 7239
```

## pubsub-background

```
ops = 12539791
errors = 0
busy_retryable = 0
elapsed_s = 10.002
ops_per_sec = 1253742
p50_us = 815
p99_us = 1823
p999_us = 2687
p9999_us = 5631
max_us = 6287
```
