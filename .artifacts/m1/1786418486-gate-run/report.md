# M1 gate-run report

date: 1786418486 (unix) · cells: 4 · replicates: 3 · duration: 10s · storm: 1000000 · subs: 512×50
env-check: OK
tier: reference-box (binding)

notes:
- ttl-heavy: expired_active 1810565 · expired_lazy 563 across cells
- eviction pressure: 4939039 evictions; logical 251658176 B vs limit 268435456 B (resident incl. slack/buffers: 463537088 B)
- FLUSHALL command latency: 8.9 ms over 2000000 keys
- pub/sub registry pressure: 25600 subscriptions across 512 connections
- pub/sub deliveries drained by the fleet: 2560512
- slow-subscriber: killed=true · client_output_buffer_limit_disconnections=1

| gate | threshold | measured | verdict |
|---|---|---|---|
| M0 gates re-pass within 5% | <= 5 % vs M0 baseline | — | PENDING (tooling) |
| RSS vs Redis, 10M x (16 B, 64 B) | <= 1 x Redis | 0.61 | PASS |
| Expiry storm: foreground p99.9 | < 2000 us | 991.00 | PASS |
| Expiry storm: debt drains | < 10 s | 0.69 | PASS |
| Eviction pressure: read p99.9 under write storm | < 2000 us | 1279.00 | PASS |
| Eviction bound: used memory vs maxmemory | <= 1.05 x maxmemory | 0.94 | PASS |
| FLUSHALL under load: read p99 | < 2000 us | 847.00 | PASS |
| TTL-heavy mix p99.9 (feature-pressure row) | < 2000 us | 4607.00 | FAIL (informational) |
| allkeys-lfu hit rate vs Redis (zipfian) | <= 2 pp below Redis | — | PENDING (tooling) |
| Pub/sub fan-out p99 (100k subscriptions) | < 5 ms | 1.07 | PASS |
| KV p99.9 with pub/sub background traffic | < 2000 us | 4095.00 | FAIL (informational) |
| Slow subscriber dies at the output cap | >= 1 killed (bool) | 1.00 | PASS |
| 100% byte-diff green on declared-full | >= 1 green (bool) | — | PENDING (tooling) |
| 24h soak: zero crashes, RSS slope | < 0.5 %/24h | — | PENDING (tooling) |
| Docker image size | < 10 MB | — | PENDING (tooling) |
| Client smoke green x4 libraries | >= 4 libraries | — | PENDING (tooling) |

## baseline rep 0

```
ops = 20818634
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2081552
p50_us = 479
p99_us = 863
p999_us = 1023
p9999_us = 6783
max_us = 9786
```

## baseline rep 1

```
ops = 19847578
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 1984470
p50_us = 511
p99_us = 895
p999_us = 1087
p9999_us = 1183
max_us = 3170
```

## baseline rep 2

```
ops = 19935203
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 1993272
p50_us = 511
p99_us = 879
p999_us = 991
p9999_us = 1151
max_us = 3585
```

## ttl-heavy

```
ops = 17217366
errors = 0
busy_retryable = 0
elapsed_s = 10.002
ops_per_sec = 1721475
p50_us = 575
p99_us = 1055
p999_us = 4607
p9999_us = 14079
max_us = 14747
```

## expiry-storm reads

```
ops = 60320354
errors = 0
busy_retryable = 0
elapsed_s = 30.001
ops_per_sec = 2010598
p50_us = 503
p99_us = 863
p999_us = 991
p9999_us = 1151
max_us = 2571
```

## eviction-pressure

```
ops = 15455376
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 1545347
p50_us = 671
p99_us = 1119
p999_us = 1279
p9999_us = 1471
max_us = 2628
```

## flushall-under-load

```
ops = 21252925
errors = 0
busy_retryable = 0
elapsed_s = 10.002
ops_per_sec = 2124954
p50_us = 479
p99_us = 847
p999_us = 991
p9999_us = 1215
max_us = 8850
```

## pubsub-background

```
ops = 8472387
errors = 0
busy_retryable = 0
elapsed_s = 10.003
ops_per_sec = 847021
p50_us = 1215
p99_us = 2239
p999_us = 4095
p9999_us = 10495
max_us = 11853
```
