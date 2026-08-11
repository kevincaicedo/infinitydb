# inf-compare — competitive benchmark report

> **Tier:** reference-box (binding, citation-grade)

| | |
|---|---|
| Generated | unix 1782968021 |
| Git | `ce3cf5f` |
| Host | 7.0.0-27-generic, 24 cores |
| CPU governor / EPP | `performance` / `performance` |
| inf-bench env-check | PASS (`target/release/inf-bench env-check` exit 0) |
| Mode | host |
| Generators | memtier + redis-benchmark |
| memtier | `memtier_benchmark v=255.255.255 sha=62413fd6:0 bits=64 libevent=2.1.12-stable openssl=OpenSSL 3.5.5 27 Jan 2026` |
| redis-benchmark | `redis-benchmark 8.0.5` |
| Parameters | duration=10s · threads=8 · clients=50 · value=64 B · keyspace=1000000 · pipeline=1 · maxmemory=unset |

## Engines — published configs

| Engine | Mode | Version | Peak RSS (MiB) | Launch command |
|---|---|---|---:|---|
| redis | host | Redis server v=8.0.5 sha=00000000:0 malloc=jemalloc-5.3.0 bits=64 build=9729964261b8fc0f | 17.6 | `redis-server --port 7000 --save '' --appendonly no` |
| dragonfly | host | dragonfly v1.39.0-699862e5da7cb29bf5642a1da422c4c78cefae38 | 46.5 | `dragonfly --port 7001 --proactor_threads 8 --cache_mode --dbfilename '' --logtostderr --dir /tmp` |
| infinitydb | host | infinityd ce3cf5f (git ce3cf5fc5313, x86_64-unknown-linux-gnu) | 156.4 | `target/release/infinityd --port 7002 --cells 8` |

## Results — memtier_benchmark

| Engine | Workload | Pipe | Throughput (ops/s) | avg (ms) | p50 (ms) | p99 (ms) | p99.9 (ms) | RSS (MiB) |
|---|---|---:|---:|---:|---:|---:|---:|---:|
| redis | mixed | 1 | 254931 | 1.569 | 1.615 | 2.831 | 3.423 | 17.6 |
| dragonfly | mixed | 1 | 1025867 | 0.389 | 0.351 | 0.807 | 0.895 | 35.9 |
| infinitydb | mixed | 1 | 903722 | 0.442 | 0.391 | 0.919 | 0.975 | 156.4 |

## Notes & honesty

- redis is single-threaded; dragonfly and infinitydb ran with 8 threads/cells. Each engine kept its own best config (recorded above), per master plan §22.
- GET rows were measured after a 5s sequential populate; redis-benchmark uses its own key format, so its GET cross-check reads against keys memtier didn't write (throughput-comparable, hit rate not).
- redis-benchmark is request-count based (`-n 1000000`) and reports only p50/p95/p99; p99.9 always comes from memtier. The two are compared on throughput, not latency.
- Pub/sub fan-out latency is **not** measured here — memtier/redis-benchmark don't set up subscribers. That row lives in `inf-bench gate-run m1` (delivery-acked).
- Raw memtier JSON + redis-benchmark CSV for every row are under `raw/`.
