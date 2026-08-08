# M1 gate-run report

date: 1783322385 (unix) · cells: 4 · replicates: 3 · duration: 10s · storm: 1000000 · subs: 512×50
env-check: FAILED (overridden — NOT citation-grade)
tier: dev (non-binding)

notes:
- env-check FAILED and was overridden (--unsafe-env): not citation-grade
- dev-tier run: reference-box gates report measured values, non-binding verdicts
- ttl-heavy: expired_active 1831674 · expired_lazy 505 across cells
- eviction pressure: 6059785 evictions; logical 251658176 B vs limit 268435456 B (resident incl. slack/buffers: 463504320 B)
- FLUSHALL command latency: 8.6 ms over 2000000 keys
- pub/sub registry pressure: 25600 subscriptions across 512 connections
- pub/sub deliveries drained by the fleet: 5181027
- slow-subscriber: killed=true · client_output_buffer_limit_disconnections=1

| gate | threshold | measured | verdict |
|---|---|---|---|
| M0 gates re-pass within 5% | <= 5 % vs M0 baseline | — | PENDING (tooling) |
| RSS vs Redis, 10M x (16 B, 64 B) | <= 1 x Redis | 0.61 | PASS (DEV-TIER, non-binding) |
| Expiry storm: foreground p99.9 | < 2000 us | 1119.00 | PASS (DEV-TIER, non-binding) |
| Expiry storm: debt drains | < 10 s | 11.44 | FAIL (DEV-TIER, non-binding) |
| Eviction pressure: read p99.9 under write storm | < 2000 us | 1375.00 | PASS (DEV-TIER, non-binding) |
| Eviction bound: used memory vs maxmemory | <= 1.05 x maxmemory | 0.94 | PASS (DEV-TIER, non-binding) |
| FLUSHALL under load: read p99 | < 2000 us | 831.00 | PASS (DEV-TIER, non-binding) |
| TTL-heavy mix p99.9 (feature-pressure row) | < 2000 us | 3711.00 | FAIL (informational) |
| allkeys-lfu hit rate vs Redis (zipfian) | <= 2 pp below Redis | — | PENDING (tooling) |
| Pub/sub fan-out p99 (100k subscriptions) | < 5 ms | 1.45 | PASS (DEV-TIER, non-binding) |
| KV p99.9 with pub/sub background traffic | < 2000 us | 3007.00 | FAIL (informational) |
| Slow subscriber dies at the output cap | >= 1 killed (bool) | 1.00 | PASS |
| 100% byte-diff green on declared-full | >= 1 green (bool) | — | PENDING (tooling) |
| 24h soak: zero crashes, RSS slope | < 0.5 %/24h | — | PENDING (tooling) |
| Docker image size | < 30 MB | — | PENDING (tooling) |
| Client smoke green x4 libraries | >= 4 libraries | — | PENDING (tooling) |

## baseline rep 0

```
ops = 22406900
errors = 0
elapsed_s = 10.001
ops_per_sec = 2240418
p50_us = 431
p99_us = 895
p999_us = 1183
p9999_us = 11007
max_us = 11746
```

## baseline rep 1

```
ops = 21458366
errors = 0
elapsed_s = 10.001
ops_per_sec = 2145591
p50_us = 463
p99_us = 927
p999_us = 1183
p9999_us = 1535
max_us = 3322
```

## baseline rep 2

```
ops = 22089220
errors = 0
elapsed_s = 10.001
ops_per_sec = 2208634
p50_us = 447
p99_us = 879
p999_us = 1119
p9999_us = 1343
max_us = 2824
```

## ttl-heavy

```
ops = 18906221
errors = 0
elapsed_s = 10.001
ops_per_sec = 1890405
p50_us = 503
p99_us = 1087
p999_us = 3711
p9999_us = 14847
max_us = 15452
```

## expiry-storm reads

```
ops = 70016573
errors = 0
elapsed_s = 30.001
ops_per_sec = 2333799
p50_us = 423
p99_us = 863
p999_us = 1119
p9999_us = 1439
max_us = 3344
```

## eviction-pressure

```
ops = 18680126
errors = 0
elapsed_s = 10.001
ops_per_sec = 1867769
p50_us = 527
p99_us = 1055
p999_us = 1375
p9999_us = 1695
max_us = 3019
```

## flushall-under-load

```
ops = 24232649
errors = 0
elapsed_s = 10.001
ops_per_sec = 2422990
p50_us = 399
p99_us = 831
p999_us = 1087
p9999_us = 1439
max_us = 8773
```

## pubsub-background

```
ops = 10950968
errors = 0
elapsed_s = 10.002
ops_per_sec = 1094925
p50_us = 911
p99_us = 2111
p999_us = 3007
p9999_us = 6655
max_us = 11566
```
