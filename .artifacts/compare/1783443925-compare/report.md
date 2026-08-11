# inf-compare — competitive benchmark report

> **Tier:** DEV-TIER (non-citable, L10) — plumbing/relative numbers only

| | |
|---|---|
| Generated | unix 1783443925 |
| Git | `95fe3f5` |
| Host | 7.0.0-27-generic, 24 cores |
| CPU governor / EPP | `performance` / `performance` |
| inf-bench env-check | PASS (`target/release/inf-bench env-check` exit 0) |
| Mode | host |
| Generators | memtier + redis-benchmark |
| memtier | `memtier_benchmark v=255.255.255 sha=62413fd6:0 bits=64 libevent=2.1.12-stable openssl=OpenSSL 3.5.5 27 Jan 2026` |
| redis-benchmark | `redis-benchmark 8.0.5` |
| Parameters | duration=30s · threads=4 · clients=50 · value=64 B · keyspace=1000000 · pipeline=1, 16 · maxmemory=unset |

## Engines — published configs

| Engine | Mode | Version | Peak RSS (MiB) | Launch command |
|---|---|---|---:|---|
| infinitydb | host | infinityd v0.2.0-alpha.1-26-gfe797e1 (git fe797e17cdf9, x86_64-unknown-linux-gnu) | 198.5 | `target/release/infinityd --port 7000 --cells 4` |
| redis | host | Redis server v=8.0.5 sha=00000000:0 malloc=jemalloc-5.3.0 bits=64 build=9729964261b8fc0f | 160.1 | `redis-server --port 7001 --save '' --appendonly no` |

## Results — memtier_benchmark

| Engine | Workload | Pipe | Throughput (ops/s) | avg (ms) | p50 (ms) | p99 (ms) | p99.9 (ms) | RSS (MiB) |
|---|---|---:|---:|---:|---:|---:|---:|---:|
| infinitydb | set | 1 | 742251 | 0.269 | 0.239 | 0.423 | 0.607 | 150.9 |
| infinitydb | set | 16 | 3014913 | 1.060 | 1.023 | 1.903 | 2.559 | 200.4 |
| infinitydb | mixed | 1 | 767513 | 0.260 | 0.231 | 0.399 | 0.575 | 96.6 |
| infinitydb | mixed | 16 | 3340082 | 0.956 | 0.919 | 1.807 | 2.287 | 100.1 |
| infinitydb | get | 1 | 761616 | 0.262 | 0.231 | 0.407 | 0.583 | 97.4 |
| infinitydb | get | 16 | 3407658 | 0.937 | 0.903 | 1.783 | 2.367 | 97.3 |
| infinitydb | incr | 1 | 747179 | 0.267 | 0.247 | 0.415 | 0.447 | 119.6 |
| infinitydb | incr | 16 | 3165369 | 1.009 | 0.983 | 1.807 | 2.671 | 145.1 |
| infinitydb | mset | 1 | 745648 | 0.268 | 0.231 | 0.407 | 0.591 | 123.0 |
| infinitydb | mset | 16 | 2767478 | 1.155 | 1.119 | 2.063 | 3.103 | 145.0 |
| infinitydb | ttl | 1 | 775315 | 0.258 | 0.231 | 0.383 | 0.471 | 113.1 |
| infinitydb | ttl | 16 | 3315016 | 0.964 | 0.935 | 1.791 | 2.367 | 114.9 |
| redis | set | 1 | 268814 | 0.744 | 0.727 | 1.439 | 1.655 | 133.6 |
| redis | set | 16 | 1768294 | 1.807 | 1.527 | 2.863 | 3.375 | 161.0 |
| redis | mixed | 1 | 273546 | 0.731 | 0.711 | 1.407 | 1.599 | 18.9 |
| redis | mixed | 16 | 2158703 | 1.480 | 1.295 | 2.511 | 3.087 | 22.6 |
| redis | get | 1 | 275097 | 0.727 | 0.703 | 1.391 | 1.679 | 19.6 |
| redis | get | 16 | 2311725 | 1.382 | 1.215 | 2.351 | 2.863 | 19.5 |
| redis | incr | 1 | 271389 | 0.737 | 0.711 | 1.415 | 1.727 | 74.2 |
| redis | incr | 16 | 1980825 | 1.613 | 1.399 | 2.735 | 3.423 | 84.6 |
| redis | mset | 1 | 260224 | 0.768 | 0.727 | 1.455 | 1.775 | 24.8 |
| redis | mset | 16 | 1703942 | 1.876 | 1.703 | 2.911 | 3.727 | 54.9 |
| redis | ttl | 1 | 273689 | 0.730 | 0.711 | 1.415 | 1.607 | 18.5 |
| redis | ttl | 16 | 2070239 | 1.543 | 1.335 | 2.559 | 3.183 | 19.2 |

## Results — redis-benchmark

| Engine | Workload | Pipe | Throughput (req/s) | avg (ms) | p50 (ms) | p99 (ms) |
|---|---|---:|---:|---:|---:|---:|
| infinitydb | set | 1 | 181785 | 0.144 | 0.143 | 0.207 |
| infinitydb | set | 16 | 1769912 | 0.241 | 0.223 | 0.343 |
| infinitydb | get | 1 | 191828 | 0.136 | 0.135 | 0.191 |
| infinitydb | get | 16 | 2197802 | 0.193 | 0.191 | 0.247 |
| infinitydb | incr | 1 | 189394 | 0.138 | 0.135 | 0.191 |
| infinitydb | incr | 16 | 2024292 | 0.211 | 0.199 | 0.271 |
| redis | set | 1 | 189934 | 0.137 | 0.135 | 0.215 |
| redis | set | 16 | 1019368 | 0.738 | 0.695 | 1.231 |
| redis | get | 1 | 170358 | 0.151 | 0.151 | 0.215 |
| redis | get | 16 | 2314815 | 0.299 | 0.287 | 0.447 |
| redis | incr | 1 | 187688 | 0.138 | 0.135 | 0.215 |
| redis | incr | 16 | 1305483 | 0.569 | 0.535 | 1.047 |

## Cross-check — memtier vs redis-benchmark throughput

Independent-generator agreement on the same engine/workload. Flagged when the two disagree by more than 25%.

| Engine | Workload | Pipe | memtier (ops/s) | redis-bench (req/s) | Δ | |
|---|---|---:|---:|---:|---:|:--|
| infinitydb | set | 1 | 742251 | 181785 | +308.3% | ⚠ diverges |
| infinitydb | set | 16 | 3014913 | 1769912 | +70.3% | ⚠ diverges |
| infinitydb | get | 1 | 761616 | 191828 | +297.0% | ⚠ diverges |
| infinitydb | get | 16 | 3407658 | 2197802 | +55.0% | ⚠ diverges |
| infinitydb | incr | 1 | 747179 | 189394 | +294.5% | ⚠ diverges |
| infinitydb | incr | 16 | 3165369 | 2024292 | +56.4% | ⚠ diverges |
| redis | set | 1 | 268814 | 189934 | +41.5% | ⚠ diverges |
| redis | set | 16 | 1768294 | 1019368 | +73.5% | ⚠ diverges |
| redis | get | 1 | 275097 | 170358 | +61.5% | ⚠ diverges |
| redis | get | 16 | 2311725 | 2314815 | -0.1% | ok |
| redis | incr | 1 | 271389 | 187688 | +44.6% | ⚠ diverges |
| redis | incr | 16 | 1980825 | 1305483 | +51.7% | ⚠ diverges |

## Memory attribution — bytes/key

Fill the keyspace, then `(RSS_after − RSS_baseline) ÷ DBSIZE`. The L5 gate shape; the binding ≤ 1.0× Redis gate is `inf-bench gate-run m1` on the reference box.

| Engine | Keys | Value (B) | RSS baseline (MiB) | RSS after (MiB) | bytes/key |
|---|---:|---:|---:|---:|---:|
| infinitydb | 43529 | 64 | 112.7 | 116.4 | 88.1 |
| redis | 13460 | 64 | 18.4 | 20.7 | 178.6 |

## Notes & honesty

- **Non-citable run.** DEV-TIER numbers prove the harness and show relative shape only. A binding number needs `--reference-box` on a clean box (the M0-R2 standing obligation). Authoritative gate: `inf-bench env-check`.
- redis is single-threaded; dragonfly and infinitydb ran with 4 threads/cells. Each engine kept its own best config (recorded above), per master plan §22.
- GET rows were measured after a 5s sequential populate; redis-benchmark uses its own key format, so its GET cross-check reads against keys memtier didn't write (throughput-comparable, hit rate not).
- redis-benchmark is request-count based (`-n 1000000`) and reports only p50/p95/p99; p99.9 always comes from memtier. The two are compared on throughput, not latency.
- Pub/sub fan-out latency is **not** measured here — memtier/redis-benchmark don't set up subscribers. That row lives in `inf-bench gate-run m1` (delivery-acked).
- Raw memtier JSON + redis-benchmark CSV for every row are under `raw/`.
