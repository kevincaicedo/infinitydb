# inf-compare — competitive benchmark report

> **Tier:** reference-box (binding, citation-grade)

| | |
|---|---|
| Generated | unix 1784245083 |
| Git | `9c4e5f4` |
| Host | 7.0.0-27-generic, 24 cores |
| CPU governor / EPP | `performance` / `performance` |
| inf-bench env-check | PASS (`target/release/inf-bench env-check` exit 0) |
| Mode | host |
| Generators | memtier + redis-benchmark |
| memtier | `memtier_benchmark v=255.255.255 sha=62413fd6:0 bits=64 libevent=2.1.12-stable openssl=OpenSSL 3.5.5 27 Jan 2026` |
| redis-benchmark | `redis-benchmark 8.0.5` |
| Parameters | duration=10s · threads=8 · clients=50 · value=250 B · keyspace=1000000 · pipeline=1 · maxmemory=unset |

## Engines — published configs

| Engine | Mode | Version | Peak RSS (MiB) | Launch command |
|---|---|---|---:|---|
| infinitydb | host | infinityd 8391a38 (git 8391a38fb3b6-dirty, x86_64-unknown-linux-gnu) | 162.0 | `target/release/infinityd --port 7000 --cells 8` |
| dragonfly | host | dragonfly v1.39.0-699862e5da7cb29bf5642a1da422c4c78cefae38 | 46.9 | `dragonfly --port 7001 --proactor_threads 8 --cache_mode --dbfilename '' --logtostderr --dir /tmp` |

## Results — memtier_benchmark

| Engine | Workload | Pipe | Throughput (ops/s) | avg (ms) | p50 (ms) | p99 (ms) | p99.9 (ms) | RSS (MiB) |
|---|---|---:|---:|---:|---:|---:|---:|---:|
| infinitydb | mixed | 1 | 876484 | 0.456 | 0.415 | 0.935 | 0.991 | 162.0 |
| dragonfly | mixed | 1 | 1045930 | 0.382 | 0.391 | 0.759 | 1.935 | 36.4 |

## Notes & honesty

- redis is single-threaded; dragonfly and infinitydb ran with 8 threads/cells. Each engine kept its own best config (recorded above), per master plan §22.
- GET rows were measured after a 5s sequential populate; redis-benchmark uses its own key format, so its GET cross-check reads against keys memtier didn't write (throughput-comparable, hit rate not).
- redis-benchmark is request-count based (`-n 1000000`) and reports only p50/p95/p99; p99.9 always comes from memtier. The two are compared on throughput, not latency.
- Pub/sub fan-out latency is **not** measured here — memtier/redis-benchmark don't set up subscribers. That row lives in `inf-bench gate-run m1` (delivery-acked).
- Raw memtier JSON + redis-benchmark CSV for every row are under `raw/`.
