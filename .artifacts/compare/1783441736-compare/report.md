# inf-compare — competitive benchmark report

> **Tier:** DEV-TIER (non-citable, L10) — plumbing/relative numbers only

| | |
|---|---|
| Generated | unix 1783441736 |
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
| infinitydb | host | infinityd v0.2.0-alpha.1-26-gfe797e1 (git fe797e17cdf9, x86_64-unknown-linux-gnu) | 197.3 | `target/release/infinityd --port 7000 --cells 4` |

## Results — memtier_benchmark

| Engine | Workload | Pipe | Throughput (ops/s) | avg (ms) | p50 (ms) | p99 (ms) | p99.9 (ms) | RSS (MiB) |
|---|---|---:|---:|---:|---:|---:|---:|---:|
| infinitydb | set | 1 | 741071 | 0.270 | 0.239 | 0.423 | 0.511 | 151.0 |
| infinitydb | set | 16 | 3085936 | 1.035 | 0.999 | 1.911 | 2.607 | 199.6 |
| infinitydb | mixed | 1 | 752403 | 0.266 | 0.239 | 0.415 | 0.471 | 96.5 |
| infinitydb | mixed | 16 | 3425281 | 0.932 | 0.911 | 1.623 | 1.999 | 100.0 |
| infinitydb | get | 1 | 749014 | 0.267 | 0.247 | 0.415 | 0.567 | 97.4 |
| infinitydb | get | 16 | 3417791 | 0.935 | 0.895 | 1.767 | 2.351 | 97.3 |
| infinitydb | incr | 1 | 769012 | 0.260 | 0.231 | 0.383 | 0.599 | 119.6 |
| infinitydb | incr | 16 | 3202192 | 0.998 | 0.967 | 1.783 | 2.703 | 145.1 |
| infinitydb | mset | 1 | 735692 | 0.272 | 0.239 | 0.415 | 0.463 | 122.6 |
| infinitydb | mset | 16 | 2712636 | 1.178 | 1.135 | 2.191 | 3.151 | 145.1 |
| infinitydb | ttl | 1 | 764863 | 0.261 | 0.231 | 0.407 | 0.471 | 113.2 |
| infinitydb | ttl | 16 | 3332277 | 0.959 | 0.935 | 1.711 | 2.399 | 114.6 |

## Results — redis-benchmark

| Engine | Workload | Pipe | Throughput (req/s) | avg (ms) | p50 (ms) | p99 (ms) |
|---|---|---:|---:|---:|---:|---:|
| infinitydb | set | 1 | 184775 | 0.142 | 0.143 | 0.215 |
| infinitydb | set | 16 | 1915709 | 0.221 | 0.199 | 0.271 |
| infinitydb | get | 1 | 174125 | 0.149 | 0.151 | 0.207 |
| infinitydb | get | 16 | 2252252 | 0.189 | 0.191 | 0.231 |
| infinitydb | incr | 1 | 189036 | 0.138 | 0.135 | 0.191 |
| infinitydb | incr | 16 | 1912046 | 0.222 | 0.207 | 0.263 |

## Cross-check — memtier vs redis-benchmark throughput

Independent-generator agreement on the same engine/workload. Flagged when the two disagree by more than 25%.

| Engine | Workload | Pipe | memtier (ops/s) | redis-bench (req/s) | Δ | |
|---|---|---:|---:|---:|---:|:--|
| infinitydb | set | 1 | 741071 | 184775 | +301.1% | ⚠ diverges |
| infinitydb | set | 16 | 3085936 | 1915709 | +61.1% | ⚠ diverges |
| infinitydb | get | 1 | 749014 | 174125 | +330.2% | ⚠ diverges |
| infinitydb | get | 16 | 3417791 | 2252252 | +51.7% | ⚠ diverges |
| infinitydb | incr | 1 | 769012 | 189036 | +306.8% | ⚠ diverges |
| infinitydb | incr | 16 | 3202192 | 1912046 | +67.5% | ⚠ diverges |

## Memory attribution — bytes/key

Fill the keyspace, then `(RSS_after − RSS_baseline) ÷ DBSIZE`. The L5 gate shape; the binding ≤ 1.0× Redis gate is `inf-bench gate-run m1` on the reference box.

| Engine | Keys | Value (B) | RSS baseline (MiB) | RSS after (MiB) | bytes/key |
|---|---:|---:|---:|---:|---:|
| infinitydb | 41483 | 64 | 112.8 | 116.3 | 88.1 |

## Notes & honesty

- **Non-citable run.** DEV-TIER numbers prove the harness and show relative shape only. A binding number needs `--reference-box` on a clean box (the M0-R2 standing obligation). Authoritative gate: `inf-bench env-check`.
- redis is single-threaded; dragonfly and infinitydb ran with 4 threads/cells. Each engine kept its own best config (recorded above), per master plan §22.
- GET rows were measured after a 5s sequential populate; redis-benchmark uses its own key format, so its GET cross-check reads against keys memtier didn't write (throughput-comparable, hit rate not).
- redis-benchmark is request-count based (`-n 1000000`) and reports only p50/p95/p99; p99.9 always comes from memtier. The two are compared on throughput, not latency.
- Pub/sub fan-out latency is **not** measured here — memtier/redis-benchmark don't set up subscribers. That row lives in `inf-bench gate-run m1` (delivery-acked).
- Raw memtier JSON + redis-benchmark CSV for every row are under `raw/`.
