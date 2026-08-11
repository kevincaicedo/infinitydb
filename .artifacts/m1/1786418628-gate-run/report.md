# M1 gate-run report

date: 1786418628 (unix) · cells: 4 · replicates: 3 · duration: 10s · storm: 1000000 · subs: 512×50
env-check: OK
tier: reference-box (binding)

notes:
- ttl-heavy: expired_active 1804921 · expired_lazy 1220 across cells
- eviction pressure: 5027368 evictions; logical 251658176 B vs limit 268435456 B (resident incl. slack/buffers: 463537088 B)
- FLUSHALL command latency: 8.6 ms over 2000000 keys
- pub/sub registry pressure: 25600 subscriptions across 512 connections
- pub/sub deliveries drained by the fleet: 3473408
- slow-subscriber: killed=true · client_output_buffer_limit_disconnections=1

| gate | threshold | measured | verdict |
|---|---|---|---|
| M0 gates re-pass within 5% | <= 5 % vs M0 baseline | — | PENDING (tooling) |
| RSS vs Redis, 10M x (16 B, 64 B) | <= 1 x Redis | 0.61 | PASS |
| Expiry storm: foreground p99.9 | < 2000 us | 1055.00 | PASS |
| Expiry storm: debt drains | < 10 s | 0.80 | PASS |
| Eviction pressure: read p99.9 under write storm | < 2000 us | 1407.00 | PASS |
| Eviction bound: used memory vs maxmemory | <= 1.05 x maxmemory | 0.94 | PASS |
| FLUSHALL under load: read p99 | < 2000 us | 879.00 | PASS |
| TTL-heavy mix p99.9 (feature-pressure row) | < 2000 us | 4735.00 | FAIL (informational) |
| allkeys-lfu hit rate vs Redis (zipfian) | <= 2 pp below Redis | 0.00 | PASS |
| Pub/sub fan-out p99 (100k subscriptions) | < 5 ms | 1.35 | PASS |
| KV p99.9 with pub/sub background traffic | < 2000 us | 4479.00 | FAIL (informational) |
| Slow subscriber dies at the output cap | >= 1 killed (bool) | 1.00 | PASS |
| 100% byte-diff green on declared-full | >= 1 green (bool) | — | PENDING (tooling) |
| 24h soak: zero crashes, RSS slope | < 0.5 %/24h | — | PENDING (tooling) |
| Docker image size | < 10 MB | — | PENDING (tooling) |
| Client smoke green x4 libraries | >= 4 libraries | — | PENDING (tooling) |

## baseline rep 0

```
ops = 19993192
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 1999073
p50_us = 503
p99_us = 863
p999_us = 991
p9999_us = 8063
max_us = 9668
```

## baseline rep 1

```
ops = 19569115
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 1956627
p50_us = 543
p99_us = 831
p999_us = 927
p9999_us = 1151
max_us = 4352
```

## baseline rep 2

```
ops = 19155917
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 1915360
p50_us = 527
p99_us = 927
p999_us = 1023
p9999_us = 1183
max_us = 2051
```

## ttl-heavy

```
ops = 16750048
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 1674776
p50_us = 607
p99_us = 1119
p999_us = 4735
p9999_us = 12799
max_us = 13303
```

## expiry-storm reads

```
ops = 59910088
errors = 0
busy_retryable = 0
elapsed_s = 30.001
ops_per_sec = 1996918
p50_us = 511
p99_us = 911
p999_us = 1055
p9999_us = 1247
max_us = 2013
```

## eviction-pressure

```
ops = 15663195
errors = 0
busy_retryable = 0
elapsed_s = 10.002
ops_per_sec = 1566084
p50_us = 655
p99_us = 1215
p999_us = 1407
p9999_us = 1695
max_us = 3523
```

## flushall-under-load

```
ops = 21330182
errors = 0
busy_retryable = 0
elapsed_s = 10.002
ops_per_sec = 2132668
p50_us = 471
p99_us = 879
p999_us = 1087
p9999_us = 1375
max_us = 8741
```

## pubsub-background

```
ops = 7516641
errors = 0
busy_retryable = 0
elapsed_s = 10.002
ops_per_sec = 751479
p50_us = 1439
p99_us = 2623
p999_us = 4479
p9999_us = 10239
max_us = 11720
```
