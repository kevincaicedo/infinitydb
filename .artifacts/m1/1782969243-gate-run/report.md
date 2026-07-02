# M1 gate-run report

date: 1782969243 (unix) · cells: 4 · replicates: 3 · duration: 10s · storm: 1000000 · subs: 512×50
env-check: OK
tier: reference-box (binding)

notes:
- ttl-heavy: expired_active 1834610 · expired_lazy 500 across cells
- eviction pressure: 6037770 evictions; logical 251658176 B vs limit 268435456 B (resident incl. slack/buffers: 463504288 B)
- FLUSHALL command latency: 8.0 ms over 2000000 keys
- pub/sub registry pressure: 25600 subscriptions across 512 connections
- pub/sub deliveries drained by the fleet: 7709096
- slow-subscriber: killed=true · client_output_buffer_limit_disconnections=1

| gate | threshold | measured | verdict |
|---|---|---|---|
| M0 gates re-pass within 5% | <= 5 % vs M0 baseline | — | PENDING (tooling) |
| RSS vs Redis, 10M x (16 B, 64 B) | <= 1 x Redis | 0.61 | PASS |
| Expiry storm: foreground p99.9 | < 2000 us | 1007.00 | PASS |
| Expiry storm: debt drains | < 10 s | 11.40 | FAIL |
| Eviction pressure: read p99.9 under write storm | < 2000 us | 1343.00 | PASS |
| Eviction bound: used memory vs maxmemory | <= 1.05 x maxmemory | 0.94 | PASS |
| FLUSHALL under load: read p99 | < 2000 us | 751.00 | PASS |
| TTL-heavy mix p99.9 (feature-pressure row) | < 2000 us | 2495.00 | FAIL (informational) |
| allkeys-lfu hit rate vs Redis (zipfian) | <= 2 pp below Redis | — | PENDING (tooling) |
| Pub/sub fan-out p99 (100k subscriptions) | < 5 ms | 0.68 | PASS |
| KV p99.9 with pub/sub background traffic | < 2000 us | 2879.00 | FAIL (informational) |
| Slow subscriber dies at the output cap | >= 1 killed (bool) | 1.00 | PASS |
| 100% byte-diff green on declared-full | >= 1 green (bool) | — | PENDING (tooling) |
| 24h soak: zero crashes, RSS slope | < 0.5 %/24h | — | PENDING (tooling) |
| Docker image size | < 30 MB | — | PENDING (tooling) |
| Client smoke green x4 libraries | >= 4 libraries | — | PENDING (tooling) |

## baseline rep 0

```
ops = 22134242
errors = 0
elapsed_s = 10.001
ops_per_sec = 2213186
p50_us = 439
p99_us = 911
p999_us = 1247
p9999_us = 11007
max_us = 11622
```

## baseline rep 1

```
ops = 22788906
errors = 0
elapsed_s = 10.001
ops_per_sec = 2278626
p50_us = 431
p99_us = 863
p999_us = 1055
p9999_us = 1375
max_us = 1890
```

## baseline rep 2

```
ops = 22518237
errors = 0
elapsed_s = 10.001
ops_per_sec = 2251569
p50_us = 431
p99_us = 879
p999_us = 1119
p9999_us = 1343
max_us = 1903
```

## ttl-heavy

```
ops = 20242024
errors = 0
elapsed_s = 10.001
ops_per_sec = 2023986
p50_us = 471
p99_us = 975
p999_us = 2495
p9999_us = 12799
max_us = 13581
```

## expiry-storm reads

```
ops = 72576637
errors = 0
elapsed_s = 30.001
ops_per_sec = 2419116
p50_us = 407
p99_us = 815
p999_us = 1007
p9999_us = 1215
max_us = 2357
```

## eviction-pressure

```
ops = 18657901
errors = 0
elapsed_s = 10.001
ops_per_sec = 1865537
p50_us = 527
p99_us = 1023
p999_us = 1343
p9999_us = 1663
max_us = 2537
```

## flushall-under-load

```
ops = 25739073
errors = 0
elapsed_s = 10.001
ops_per_sec = 2573547
p50_us = 375
p99_us = 751
p999_us = 959
p9999_us = 1279
max_us = 8087
```

## pubsub-background

```
ops = 9524387
errors = 0
elapsed_s = 10.004
ops_per_sec = 952026
p50_us = 1055
p99_us = 2239
p999_us = 2879
p9999_us = 6527
max_us = 7439
```
