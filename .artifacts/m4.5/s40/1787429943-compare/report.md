# inf-compare — competitive benchmark report

> **Tier:** reference-box (binding, citation-grade)

| | |
|---|---|
| Generated | unix 1787429943 |
| Git | `9c31a18` |
| Host | 7.0.0-30-generic, 4 cores |
| CPU governor / EPP | `performance` / `performance` |
| inf-bench env-check | PASS (`target/release/inf-bench env-check` exit 0) |
| Mode | host |
| Generators | memtier |
| memtier | `memtier_benchmark v=255.255.255 sha=62413fd6:0 bits=64 libevent=2.1.12-stable openssl=OpenSSL 3.5.5 27 Jan 2026` |
| Parameters | duration=60s · threads=4 · clients=8 · value=1024 B · keyspace=1000000 · pipeline=1 · maxmemory=unset |
| Load shape | offered 100000 ops/s (memtier --rate-limiting 3125 per connection × 32 connections) · durability=everysec · data root `/home/kcaicedo/bench-data/s40/data` · device `nvme0n1` |

## Engines — published configs

| Engine | Mode | Version | Peak RSS (MiB) | Launch command |
|---|---|---|---:|---|
| infinitydb | host | infinityd 9c31a18 (git 9c31a1859041, x86_64-unknown-linux-gnu) | 343.1 | `target/release/infinityd --port 7400 --cells 4 --pin-start 0 --data-dir /home/kcaicedo/bench-data/s40/data/infinitydb --conn-default-ns cmp` |
| redis | host | taskset from util-linux 2.41.3 | 257.7 | `taskset -c 0 redis-server --port 7401 --save '' --appendonly yes --appendfsync everysec --dir /home/kcaicedo/bench-data/s40/data/redis` |

## Results — memtier_benchmark

| Engine | Workload | Pipe | Throughput (ops/s) | achieved/offered | avg (ms) | p50 (ms) | p99 (ms) | p99.9 (ms) | max (ms) | server CPU (%) | device MiB written | RSS (MiB) |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| infinitydb | set | 1 | 91433 | 0.91 | 0.089 | 0.063 | 0.135 | 0.327 | 1036.287 | 103 | 6960.7 | 343.1 |
| redis | set | 1 | 97833 | 0.98 | 0.195 | 0.143 | 3.167 | 4.351 | 53.247 | 49 | 7957.4 | 243.7 |

## Notes & honesty

- redis is single-threaded; dragonfly and infinitydb ran with 4 threads/cells. Each engine kept its own best config (recorded above), per master plan §22.
- GET rows were measured after a 5s sequential populate; redis-benchmark uses its own key format, so its GET cross-check reads against keys memtier didn't write (throughput-comparable, hit rate not).
- redis-benchmark is request-count based (`-n 1000000`) and reports only p50/p95/p99; p99.9 always comes from memtier. The two are compared on throughput, not latency.
- Pub/sub fan-out latency is **not** measured here — memtier/redis-benchmark don't set up subscribers. That row lives in `inf-bench gate-run m1` (delivery-acked).
- **Offered-rate row (M4.5-S40).** memtier paces each connection at `--rate-limiting` = rate ÷ connections; `achieved/offered` below 0.90 means the generator (or the server) could not hold the rate and the latency columns are not an offered-rate measurement. `max (ms)` is memtier's worst request; server CPU is the engine process's utime+stime over the row's wall time (host launches only); device MiB written is the block device's sectors-written delta across the row (journal and metadata included, NAND amplification not).
- **Durability everysec.** redis ran `--appendonly yes --appendfsync everysec` (its AOF under the data root); infinitydb ran `--data-dir` with every connection starting in an `FSYNC everysec` namespace (`--conn-default-ns cmp`, proven by a probe key before the row) — the same ≤ 1 s power-loss window on both sides, each engine's own mechanism, both on the same device.
- Raw memtier JSON + redis-benchmark CSV for every row are under `raw/`.
