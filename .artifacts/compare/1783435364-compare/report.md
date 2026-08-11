# inf-compare — competitive benchmark report

> **Tier:** DEV-TIER (non-citable) — `--reference-box --unsafe-env` overrode a non-clean box
>
> - git tree is dirty
> - `inf-bench env-check` failed

| | |
|---|---|
| Generated | unix 1783435364 |
| Git | `0286e05-dirty` |
| Host | 7.0.0-27-generic, 24 cores |
| CPU governor / EPP | `performance` / `performance` |
| inf-bench env-check | FAIL (`target/release/inf-bench env-check` exit 1) |
| Mode | host |
| Generators | memtier + redis-benchmark |
| memtier | `memtier_benchmark v=255.255.255 sha=62413fd6:0 bits=64 libevent=2.1.12-stable openssl=OpenSSL 3.5.5 27 Jan 2026` |
| redis-benchmark | `redis-benchmark 8.0.5` |
| Parameters | duration=10s · threads=12 · clients=50 · value=250 B · keyspace=1000000 · pipeline=1 · maxmemory=unset |

## Engines — published configs

| Engine | Mode | Version | Peak RSS (MiB) | Launch command |
|---|---|---|---:|---|
| redis | host | Redis server v=8.0.5 sha=00000000:0 malloc=jemalloc-5.3.0 bits=64 build=9729964261b8fc0f | 17.8 | `redis-server --port 7000 --save '' --appendonly no` |
| dragonfly | host | dragonfly v1.39.0-699862e5da7cb29bf5642a1da422c4c78cefae38 | 60.5 | `dragonfly --port 7001 --proactor_threads 12 --cache_mode --dbfilename '' --logtostderr --dir /tmp` |
| infinitydb | host | infinityd v0.2.0-alpha.1-21-g0eaab11 (git 0eaab11a52a7, x86_64-unknown-linux-gnu) | 253.9 | `target/release/infinityd --port 7002 --cells 12` |

## Results — memtier_benchmark

| Engine | Workload | Pipe | Throughput (ops/s) | avg (ms) | p50 (ms) | p99 (ms) | p99.9 (ms) | RSS (MiB) |
|---|---|---:|---:|---:|---:|---:|---:|---:|
| redis | mixed | 1 | 267606 | 2.241 | 2.127 | 4.159 | 5.055 | 17.6 |
| dragonfly | mixed | 1 | 1125502 | 0.532 | 0.447 | 2.911 | 4.383 | 44.8 |
| infinitydb | mixed | 1 | 1236884 | 0.484 | 0.479 | 0.935 | 3.471 | 253.9 |

## Notes & honesty

- **Non-citable run.** DEV-TIER numbers prove the harness and show relative shape only. A binding number needs `--reference-box` on a clean box (the M0-R2 standing obligation). Authoritative gate: `inf-bench env-check`.
- redis is single-threaded; dragonfly and infinitydb ran with 12 threads/cells. Each engine kept its own best config (recorded above), per master plan §22.
- GET rows were measured after a 5s sequential populate; redis-benchmark uses its own key format, so its GET cross-check reads against keys memtier didn't write (throughput-comparable, hit rate not).
- redis-benchmark is request-count based (`-n 1000000`) and reports only p50/p95/p99; p99.9 always comes from memtier. The two are compared on throughput, not latency.
- Pub/sub fan-out latency is **not** measured here — memtier/redis-benchmark don't set up subscribers. That row lives in `inf-bench gate-run m1` (delivery-acked).
- Raw memtier JSON + redis-benchmark CSV for every row are under `raw/`.
