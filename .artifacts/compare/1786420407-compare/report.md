# inf-compare — competitive benchmark report

> **Tier:** reference-box (binding, citation-grade)

| | |
|---|---|
| Generated | unix 1786420407 |
| Git | `587d0a0` |
| Host | 7.0.0-29-generic, 24 cores |
| CPU governor / EPP | `performance` / `performance` |
| inf-bench env-check | PASS (`target/release/inf-bench env-check` exit 0) |
| Mode | host |
| Generators | memtier + redis-benchmark |
| memtier | `memtier_benchmark v=255.255.255 sha=62413fd6:0 bits=64 libevent=2.1.12-stable openssl=OpenSSL 3.5.5 27 Jan 2026` |
| redis-benchmark | `redis-benchmark 8.0.5` |
| Parameters | duration=15s · threads=4 · clients=50 · value=64 B · keyspace=1000000 · pipeline=1, 16 · maxmemory=unset |

## Engines — published configs

| Engine | Mode | Version | Peak RSS (MiB) | Launch command |
|---|---|---|---:|---|
| redis | host | Redis server v=8.0.5 sha=00000000:0 malloc=jemalloc-5.3.0 bits=64 build=9729964261b8fc0f | 144.7 | `redis-server --port 7000 --save '' --appendonly no` |
| dragonfly | host | dragonfly v1.39.0-699862e5da7cb29bf5642a1da422c4c78cefae38 | 94.6 | `dragonfly --port 7001 --proactor_threads 4 --cache_mode --dbfilename '' --logtostderr --dir /tmp` |
| infinitydb | host | infinityd 02f870c (git 02f870cfc97d, x86_64-unknown-linux-gnu) | 168.4 | `target/release/infinityd --port 7002 --cells 4` |

## Results — memtier_benchmark

| Engine | Workload | Pipe | Throughput (ops/s) | avg (ms) | p50 (ms) | p99 (ms) | p99.9 (ms) | RSS (MiB) |
|---|---|---:|---:|---:|---:|---:|---:|---:|
| redis | set | 1 | 265389 | 0.753 | 0.727 | 1.439 | 1.767 | 128.6 |
| redis | set | 16 | 1808835 | 1.767 | 1.487 | 2.783 | 3.071 | 145.9 |
| redis | get | 1 | 281360 | 0.711 | 0.695 | 1.383 | 1.495 | 19.1 |
| redis | get | 16 | 2334134 | 1.368 | 1.207 | 2.335 | 2.495 | 19.4 |
| redis | mixed | 1 | 281870 | 0.709 | 0.695 | 1.383 | 1.479 | 18.3 |
| redis | mixed | 16 | 2210962 | 1.445 | 1.271 | 2.463 | 2.639 | 20.2 |
| dragonfly | set | 1 | 537162 | 0.372 | 0.375 | 0.535 | 1.415 | 86.1 |
| dragonfly | set | 16 | 1577364 | 2.026 | 1.983 | 3.631 | 4.671 | 92.9 |
| dragonfly | get | 1 | 583074 | 0.343 | 0.343 | 0.495 | 0.831 | 25.5 |
| dragonfly | get | 16 | 2049448 | 1.559 | 1.503 | 3.023 | 4.015 | 27.5 |
| dragonfly | mixed | 1 | 568381 | 0.352 | 0.343 | 0.527 | 2.175 | 26.1 |
| dragonfly | mixed | 16 | 1922162 | 1.662 | 1.591 | 3.199 | 4.223 | 28.1 |
| infinitydb | set | 1 | 761989 | 0.262 | 0.231 | 0.391 | 0.503 | 146.4 |
| infinitydb | set | 16 | 3009510 | 1.062 | 1.031 | 1.903 | 2.623 | 170.0 |
| infinitydb | get | 1 | 754222 | 0.265 | 0.239 | 0.415 | 0.623 | 97.8 |
| infinitydb | get | 16 | 3343814 | 0.955 | 0.935 | 1.807 | 2.431 | 97.9 |
| infinitydb | mixed | 1 | 782370 | 0.255 | 0.231 | 0.375 | 0.415 | 96.5 |
| infinitydb | mixed | 16 | 3353180 | 0.953 | 0.927 | 1.679 | 2.095 | 98.2 |

## Results — redis-benchmark

| Engine | Workload | Pipe | Throughput (req/s) | avg (ms) | p50 (ms) | p99 (ms) |
|---|---|---:|---:|---:|---:|---:|
| redis | set | 1 | 191424 | 0.136 | 0.135 | 0.191 |
| redis | set | 16 | 1074114 | 0.699 | 0.671 | 1.247 |
| redis | get | 1 | 191314 | 0.135 | 0.135 | 0.191 |
| redis | get | 16 | 2141328 | 0.315 | 0.295 | 0.575 |
| dragonfly | set | 1 | 179824 | 0.147 | 0.151 | 0.199 |
| dragonfly | set | 16 | 1522070 | 0.505 | 0.503 | 0.815 |
| dragonfly | get | 1 | 179533 | 0.146 | 0.151 | 0.199 |
| dragonfly | get | 16 | 2109704 | 0.353 | 0.351 | 0.503 |
| infinitydb | set | 1 | 189072 | 0.139 | 0.143 | 0.191 |
| infinitydb | set | 16 | 2049180 | 0.208 | 0.199 | 0.287 |
| infinitydb | get | 1 | 192012 | 0.136 | 0.135 | 0.183 |
| infinitydb | get | 16 | 2164502 | 0.196 | 0.191 | 0.287 |

## Cross-check — memtier vs redis-benchmark throughput

Independent-generator agreement on the same engine/workload. Flagged when the two disagree by more than 25%.

| Engine | Workload | Pipe | memtier (ops/s) | redis-bench (req/s) | Δ | |
|---|---|---:|---:|---:|---:|:--|
| redis | set | 1 | 265389 | 191424 | +38.6% | ⚠ diverges |
| redis | set | 16 | 1808835 | 1074114 | +68.4% | ⚠ diverges |
| redis | get | 1 | 281360 | 191314 | +47.1% | ⚠ diverges |
| redis | get | 16 | 2334134 | 2141328 | +9.0% | ok |
| dragonfly | set | 1 | 537162 | 179824 | +198.7% | ⚠ diverges |
| dragonfly | set | 16 | 1577364 | 1522070 | +3.6% | ok |
| dragonfly | get | 1 | 583074 | 179533 | +224.8% | ⚠ diverges |
| dragonfly | get | 16 | 2049448 | 2109704 | -2.9% | ok |
| infinitydb | set | 1 | 761989 | 189072 | +303.0% | ⚠ diverges |
| infinitydb | set | 16 | 3009510 | 2049180 | +46.9% | ⚠ diverges |
| infinitydb | get | 1 | 754222 | 192012 | +292.8% | ⚠ diverges |
| infinitydb | get | 16 | 3343814 | 2164502 | +54.5% | ⚠ diverges |

## Memory attribution — bytes/key

Fill the keyspace, then `(RSS_after − RSS_baseline) ÷ DBSIZE`. The L5 gate shape; the binding ≤ 1.0× Redis gate is `inf-bench gate-run m1` on the reference box.

| Engine | Keys | Value (B) | RSS baseline (MiB) | RSS after (MiB) | bytes/key |
|---|---:|---:|---:|---:|---:|
| redis | 13797 | 64 | 17.9 | 20.0 | 157.3 |
| dragonfly | 27150 | 64 | 27.1 | 27.5 | 15.8 |
| infinitydb | 41910 | 64 | 96.0 | 99.5 | 88.4 |

## Notes & honesty

- redis is single-threaded; dragonfly and infinitydb ran with 4 threads/cells. Each engine kept its own best config (recorded above), per master plan §22.
- GET rows were measured after a 5s sequential populate; redis-benchmark uses its own key format, so its GET cross-check reads against keys memtier didn't write (throughput-comparable, hit rate not).
- redis-benchmark is request-count based (`-n 1000000`) and reports only p50/p95/p99; p99.9 always comes from memtier. The two are compared on throughput, not latency.
- Pub/sub fan-out latency is **not** measured here — memtier/redis-benchmark don't set up subscribers. That row lives in `inf-bench gate-run m1` (delivery-acked).
- Raw memtier JSON + redis-benchmark CSV for every row are under `raw/`.
