# M1 gate-run report

date: 1783699242 (unix) · cells: 4 · replicates: 3 · duration: 10s · storm: 1000000 · subs: 512×50
env-check: OK
tier: reference-box (binding)

notes:
- ttl-heavy: expired_active 1825360 · expired_lazy 563 across cells
- eviction pressure: 7177240 evictions; logical 251658176 B vs limit 268435456 B (resident incl. slack/buffers: 463504320 B)
- FLUSHALL command latency: 7.7 ms over 2000000 keys
- pub/sub registry pressure: 25600 subscriptions across 512 connections
- pub/sub deliveries drained by the fleet: 4336128
- slow-subscriber: killed=true · client_output_buffer_limit_disconnections=1

| gate | threshold | measured | verdict |
|---|---|---|---|
| M0 gates re-pass within 5% | <= 5 % vs M0 baseline | — | PENDING (tooling) |
| RSS vs Redis, 10M x (16 B, 64 B) | <= 1 x Redis | 0.61 | PASS |
| Expiry storm: foreground p99.9 | < 2000 us | 911.00 | PASS |
| Expiry storm: debt drains | < 10 s | 0.70 | PASS |
| Eviction pressure: read p99.9 under write storm | < 2000 us | 1215.00 | PASS |
| Eviction bound: used memory vs maxmemory | <= 1.05 x maxmemory | 0.94 | PASS |
| FLUSHALL under load: read p99 | < 2000 us | 607.00 | PASS |
| TTL-heavy mix p99.9 (feature-pressure row) | < 2000 us | 2111.00 | FAIL (informational) |
| allkeys-lfu hit rate vs Redis (zipfian) | <= 2 pp below Redis | 0.00 | PASS |
| Pub/sub fan-out p99 (100k subscriptions) | < 5 ms | 0.75 | PASS |
| KV p99.9 with pub/sub background traffic | < 2000 us | 2879.00 | FAIL (informational) |
| Slow subscriber dies at the output cap | >= 1 killed (bool) | 1.00 | PASS |
| 100% byte-diff green on declared-full | >= 1 green (bool) | — | PENDING (tooling) |
| 24h soak: zero crashes, RSS slope | < 0.5 %/24h | — | PENDING (tooling) |
| Docker image size | < 30 MB | — | PENDING (tooling) |
| Client smoke green x4 libraries | >= 4 libraries | — | PENDING (tooling) |

## baseline rep 0

```
ops = 28118385
errors = 0
elapsed_s = 10.001
ops_per_sec = 2811530
p50_us = 351
p99_us = 671
p999_us = 975
p9999_us = 11007
max_us = 12000
```

## baseline rep 1

```
ops = 28347759
errors = 0
elapsed_s = 10.001
ops_per_sec = 2834396
p50_us = 351
p99_us = 655
p999_us = 863
p9999_us = 1279
max_us = 2109
```

## baseline rep 2

```
ops = 27870828
errors = 0
elapsed_s = 10.001
ops_per_sec = 2786751
p50_us = 359
p99_us = 671
p999_us = 879
p9999_us = 1279
max_us = 3835
```

## ttl-heavy

```
ops = 24732283
errors = 0
elapsed_s = 10.001
ops_per_sec = 2472901
p50_us = 391
p99_us = 767
p999_us = 2111
p9999_us = 17919
max_us = 19745
```

## expiry-storm reads

```
ops = 86213659
errors = 0
elapsed_s = 30.001
ops_per_sec = 2873670
p50_us = 359
p99_us = 703
p999_us = 911
p9999_us = 1279
max_us = 3148
```

## eviction-pressure

```
ops = 22069516
errors = 0
elapsed_s = 10.001
ops_per_sec = 2206663
p50_us = 447
p99_us = 895
p999_us = 1215
p9999_us = 1727
max_us = 2435
```

## flushall-under-load

```
ops = 30980502
errors = 0
elapsed_s = 10.001
ops_per_sec = 3097673
p50_us = 319
p99_us = 607
p999_us = 783
p9999_us = 1183
max_us = 7784
```

## pubsub-background

```
ops = 11309620
errors = 0
elapsed_s = 10.002
ops_per_sec = 1130765
p50_us = 863
p99_us = 2015
p999_us = 2879
p9999_us = 6911
max_us = 9069
```
