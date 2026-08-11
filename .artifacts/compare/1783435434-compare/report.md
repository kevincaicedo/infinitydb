# inf-compare — competitive benchmark report

> **Tier:** DEV-TIER (non-citable) — `--reference-box --unsafe-env` overrode a non-clean box
>
> - git tree is dirty
> - `inf-bench env-check` failed

| | |
|---|---|
| Generated | unix 1783435434 |
| Git | `0286e05-dirty` |
| Host | 7.0.0-27-generic, 24 cores |
| CPU governor / EPP | `performance` / `performance` |
| inf-bench env-check | FAIL (`target/release/inf-bench env-check` exit 1) |
| Mode | host |
| Generators | memtier + redis-benchmark |
| memtier | `memtier_benchmark v=255.255.255 sha=62413fd6:0 bits=64 libevent=2.1.12-stable openssl=OpenSSL 3.5.5 27 Jan 2026` |
| redis-benchmark | `redis-benchmark 8.0.5` |
| Parameters | duration=10s · threads=16 · clients=50 · value=250 B · keyspace=1000000 · pipeline=1 · maxmemory=unset |

## Engines — published configs

| Engine | Mode | Version | Peak RSS (MiB) | Launch command |
|---|---|---|---:|---|
| redis | host | Redis server v=8.0.5 sha=00000000:0 malloc=jemalloc-5.3.0 bits=64 build=9729964261b8fc0f | 19.0 | `redis-server --port 7000 --save '' --appendonly no` |
| dragonfly | host | dragonfly v1.39.0-699862e5da7cb29bf5642a1da422c4c78cefae38 | 73.1 | `dragonfly --port 7001 --proactor_threads 16 --cache_mode --dbfilename '' --logtostderr --dir /tmp` |
| infinitydb | host | infinityd v0.2.0-alpha.1-21-g0eaab11 (git 0eaab11a52a7, x86_64-unknown-linux-gnu) | 354.8 | `target/release/infinityd --port 7002 --cells 16` |

## Results — memtier_benchmark

| Engine | Workload | Pipe | Throughput (ops/s) | avg (ms) | p50 (ms) | p99 (ms) | p99.9 (ms) | RSS (MiB) |
|---|---|---:|---:|---:|---:|---:|---:|---:|
| redis | mixed | 1 | 260442 | 3.071 | 2.879 | 4.255 | 6.815 | 18.1 |
| dragonfly | mixed | 1 | 1138563 | 0.702 | 0.399 | 3.423 | 5.119 | 51.5 |
| infinitydb | mixed | 1 | 1085529 | 0.736 | 0.327 | 5.087 | 7.359 | 354.8 |

## Notes & honesty

- **Non-citable run.** DEV-TIER numbers prove the harness and show relative shape only. A binding number needs `--reference-box` on a clean box (the M0-R2 standing obligation). Authoritative gate: `inf-bench env-check`.
- redis is single-threaded; dragonfly and infinitydb ran with 16 threads/cells. Each engine kept its own best config (recorded above), per master plan §22.
- GET rows were measured after a 5s sequential populate; redis-benchmark uses its own key format, so its GET cross-check reads against keys memtier didn't write (throughput-comparable, hit rate not).
- redis-benchmark is request-count based (`-n 1000000`) and reports only p50/p95/p99; p99.9 always comes from memtier. The two are compared on throughput, not latency.
- Pub/sub fan-out latency is **not** measured here — memtier/redis-benchmark don't set up subscribers. That row lives in `inf-bench gate-run m1` (delivery-acked).
- Raw memtier JSON + redis-benchmark CSV for every row are under `raw/`.
