# inf-compare — competitive benchmark report

> **Tier:** DEV-TIER (non-citable, L10) — plumbing/relative numbers only
>
> - git tree is dirty
> - `inf-bench env-check` failed

| | |
|---|---|
| Generated | unix 1783435574 |
| Git | `0286e05-dirty` |
| Host | 7.0.0-27-generic, 24 cores |
| CPU governor / EPP | `performance` / `performance` |
| inf-bench env-check | FAIL (`target/release/inf-bench env-check` exit 1) |
| Mode | host |
| Generators | memtier + redis-benchmark |
| memtier | `memtier_benchmark v=255.255.255 sha=62413fd6:0 bits=64 libevent=2.1.12-stable openssl=OpenSSL 3.5.5 27 Jan 2026` |
| redis-benchmark | `redis-benchmark 8.0.5` |
| Parameters | duration=30s · threads=8 · clients=50 · value=64 B · keyspace=1000000 · pipeline=1, 16 · maxmemory=unset |

## Engines — published configs

| Engine | Mode | Version | Peak RSS (MiB) | Launch command |
|---|---|---|---:|---|
| infinitydb | host | infinityd v0.2.0-alpha.1-21-g0eaab11 (git 0eaab11a52a7, x86_64-unknown-linux-gnu) | 171.3 | `target/release/infinityd --port 7000 --cells 8` |
| dragonfly | host | dragonfly v1.39.0-699862e5da7cb29bf5642a1da422c4c78cefae38 | 80.4 | `dragonfly --port 7001 --proactor_threads 8 --cache_mode --dbfilename '' --logtostderr --dir /tmp` |

## Results — memtier_benchmark

| Engine | Workload | Pipe | Throughput (ops/s) | avg (ms) | p50 (ms) | p99 (ms) | p99.9 (ms) | RSS (MiB) |
|---|---|---:|---:|---:|---:|---:|---:|---:|
| infinitydb | mixed | 1 | 1058199 | 0.378 | 0.327 | 0.887 | 1.039 | 161.4 |
| infinitydb | mixed | 16 | 5548320 | 1.150 | 1.103 | 2.207 | 3.103 | 171.3 |
| dragonfly | mixed | 1 | 1070154 | 0.373 | 0.375 | 0.783 | 0.887 | 37.6 |
| dragonfly | mixed | 16 | 3234487 | 1.974 | 1.855 | 4.319 | 5.567 | 43.3 |

## Notes & honesty

- **Non-citable run.** DEV-TIER numbers prove the harness and show relative shape only. A binding number needs `--reference-box` on a clean box (the M0-R2 standing obligation). Authoritative gate: `inf-bench env-check`.
- redis is single-threaded; dragonfly and infinitydb ran with 8 threads/cells. Each engine kept its own best config (recorded above), per master plan §22.
- GET rows were measured after a 5s sequential populate; redis-benchmark uses its own key format, so its GET cross-check reads against keys memtier didn't write (throughput-comparable, hit rate not).
- redis-benchmark is request-count based (`-n 1000000`) and reports only p50/p95/p99; p99.9 always comes from memtier. The two are compared on throughput, not latency.
- Pub/sub fan-out latency is **not** measured here — memtier/redis-benchmark don't set up subscribers. That row lives in `inf-bench gate-run m1` (delivery-acked).
- Raw memtier JSON + redis-benchmark CSV for every row are under `raw/`.
