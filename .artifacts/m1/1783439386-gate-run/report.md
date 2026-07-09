# M1 gate-run report

date: 1783439386 (unix) · cells: 4 · replicates: 3 · duration: 10s · storm: 1000000 · subs: 512×50
env-check: OK
tier: reference-box (binding)

notes:
- ttl-heavy: expired_active 1821724 · expired_lazy 571 across cells
- eviction pressure: 7518872 evictions; logical 251658176 B vs limit 268435456 B (resident incl. slack/buffers: 463504320 B)
- FLUSHALL command latency: 7.0 ms over 2000000 keys
- pub/sub registry pressure: 25600 subscriptions across 512 connections
- pub/sub deliveries drained by the fleet: 5529403
- slow-subscriber: killed=true · client_output_buffer_limit_disconnections=1

| gate | threshold | measured | verdict |
|---|---|---|---|
| M0 gates re-pass within 5% | <= 5 % vs M0 baseline | — | PENDING (tooling) |
| RSS vs Redis, 10M x (16 B, 64 B) | <= 1 x Redis | 0.61 | PASS |
| Expiry storm: foreground p99.9 | < 2000 us | 799.00 | PASS |
| Expiry storm: debt drains | < 10 s | 0.49 | PASS |
| Eviction pressure: read p99.9 under write storm | < 2000 us | 1151.00 | PASS |
| Eviction bound: used memory vs maxmemory | <= 1.05 x maxmemory | 0.94 | PASS |
| FLUSHALL under load: read p99 | < 2000 us | 575.00 | PASS |
| TTL-heavy mix p99.9 (feature-pressure row) | < 2000 us | 1247.00 | PASS (informational) |
| allkeys-lfu hit rate vs Redis (zipfian) | <= 2 pp below Redis | — | PENDING (tooling) |
| Pub/sub fan-out p99 (100k subscriptions) | < 5 ms | 0.72 | PASS |
| KV p99.9 with pub/sub background traffic | < 2000 us | 2751.00 | FAIL (informational) |
| Slow subscriber dies at the output cap | >= 1 killed (bool) | 1.00 | PASS |
| 100% byte-diff green on declared-full | >= 1 green (bool) | — | PENDING (tooling) |
| 24h soak: zero crashes, RSS slope | < 0.5 %/24h | — | PENDING (tooling) |
| Docker image size | < 30 MB | — | PENDING (tooling) |
| Client smoke green x4 libraries | >= 4 libraries | — | PENDING (tooling) |

## baseline rep 0

```
ops = 27666677
errors = 0
elapsed_s = 10.001
ops_per_sec = 2766371
p50_us = 351
p99_us = 687
p999_us = 943
p9999_us = 10751
max_us = 13064
```

## baseline rep 1

```
ops = 29807990
errors = 0
elapsed_s = 10.001
ops_per_sec = 2980464
p50_us = 335
p99_us = 607
p999_us = 799
p9999_us = 1215
max_us = 2678
```

## baseline rep 2

```
ops = 28808363
errors = 0
elapsed_s = 10.001
ops_per_sec = 2880549
p50_us = 343
p99_us = 671
p999_us = 895
p9999_us = 1375
max_us = 2448
```

## ttl-heavy

```
ops = 25459515
errors = 0
elapsed_s = 10.001
ops_per_sec = 2545619
p50_us = 383
p99_us = 735
p999_us = 1247
p9999_us = 14079
max_us = 15003
```

## expiry-storm reads

```
ops = 92719502
errors = 0
elapsed_s = 30.001
ops_per_sec = 3090542
p50_us = 327
p99_us = 607
p999_us = 799
p9999_us = 1183
max_us = 2251
```

## eviction-pressure

```
ops = 23092810
errors = 0
elapsed_s = 10.001
ops_per_sec = 2308962
p50_us = 431
p99_us = 815
p999_us = 1151
p9999_us = 1535
max_us = 3133
```

## flushall-under-load

```
ops = 32340637
errors = 0
elapsed_s = 10.001
ops_per_sec = 3233718
p50_us = 311
p99_us = 575
p999_us = 767
p9999_us = 1247
max_us = 6877
```

## pubsub-background

```
ops = 12281987
errors = 0
elapsed_s = 10.001
ops_per_sec = 1228032
p50_us = 815
p99_us = 2047
p999_us = 2751
p9999_us = 5887
max_us = 6977
```
