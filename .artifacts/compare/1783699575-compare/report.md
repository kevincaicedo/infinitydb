# inf-compare — competitive benchmark report

> **Tier:** reference-box (binding, citation-grade)

| | |
|---|---|
| Generated | unix 1783699575 |
| Git | `98864f4` |
| Host | 7.0.0-27-generic, 24 cores |
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
| redis | host | Redis server v=8.0.5 sha=00000000:0 malloc=jemalloc-5.3.0 bits=64 build=9729964261b8fc0f | 143.9 | `redis-server --port 7000 --save '' --appendonly no` |
| dragonfly | host | dragonfly v1.39.0-699862e5da7cb29bf5642a1da422c4c78cefae38 | 93.9 | `dragonfly --port 7001 --proactor_threads 4 --cache_mode --dbfilename '' --logtostderr --dir /tmp` |
| infinitydb | host | infinityd 515c87a (git 515c87a00168, x86_64-unknown-linux-gnu) | 167.1 | `target/release/infinityd --port 7002 --cells 4` |

## Results — memtier_benchmark

| Engine | Workload | Pipe | Throughput (ops/s) | avg (ms) | p50 (ms) | p99 (ms) | p99.9 (ms) | RSS (MiB) |
|---|---|---:|---:|---:|---:|---:|---:|---:|
| redis | set | 1 | 267052 | 0.749 | 0.711 | 1.423 | 1.711 | 128.8 |
| redis | set | 16 | 1790743 | 1.784 | 1.599 | 2.815 | 3.407 | 145.8 |
| redis | get | 1 | 269725 | 0.741 | 0.695 | 1.399 | 1.695 | 19.3 |
| redis | get | 16 | 2292089 | 1.393 | 1.223 | 2.367 | 2.863 | 19.4 |
| redis | mixed | 1 | 274308 | 0.729 | 0.695 | 1.391 | 1.631 | 18.9 |
| redis | mixed | 16 | 2188404 | 1.459 | 1.279 | 2.431 | 2.959 | 20.2 |
| dragonfly | set | 1 | 533482 | 0.375 | 0.375 | 0.567 | 1.327 | 86.1 |
| dragonfly | set | 16 | 1554681 | 2.056 | 2.015 | 3.583 | 4.543 | 92.9 |
| dragonfly | get | 1 | 586603 | 0.341 | 0.343 | 0.511 | 1.015 | 25.5 |
| dragonfly | get | 16 | 2037402 | 1.568 | 1.511 | 3.055 | 4.047 | 27.5 |
| dragonfly | mixed | 1 | 576697 | 0.346 | 0.351 | 0.519 | 1.039 | 26.0 |
| dragonfly | mixed | 16 | 1911196 | 1.671 | 1.607 | 3.247 | 4.319 | 27.6 |
| infinitydb | set | 1 | 735185 | 0.272 | 0.247 | 0.423 | 0.535 | 146.2 |
| infinitydb | set | 16 | 3116238 | 1.025 | 0.983 | 1.935 | 2.783 | 168.9 |
| infinitydb | get | 1 | 741202 | 0.270 | 0.255 | 0.423 | 0.599 | 97.2 |
| infinitydb | get | 16 | 3486766 | 0.916 | 0.879 | 1.759 | 2.367 | 97.2 |
| infinitydb | mixed | 1 | 721624 | 0.277 | 0.263 | 0.439 | 0.631 | 96.0 |
| infinitydb | mixed | 16 | 3448639 | 0.926 | 0.879 | 1.775 | 2.287 | 97.9 |

## Results — redis-benchmark

| Engine | Workload | Pipe | Throughput (req/s) | avg (ms) | p50 (ms) | p99 (ms) |
|---|---|---:|---:|---:|---:|---:|
| redis | set | 1 | 189681 | 0.141 | 0.135 | 0.263 |
| redis | set | 16 | 970874 | 0.773 | 0.735 | 1.391 |
| redis | get | 1 | 179662 | 0.145 | 0.143 | 0.263 |
| redis | get | 16 | 2277904 | 0.292 | 0.287 | 0.487 |
| dragonfly | set | 1 | 167757 | 0.158 | 0.159 | 0.231 |
| dragonfly | set | 16 | 1490313 | 0.512 | 0.503 | 0.871 |
| dragonfly | get | 1 | 179372 | 0.147 | 0.151 | 0.215 |
| dragonfly | get | 16 | 2096436 | 0.354 | 0.351 | 0.519 |
| infinitydb | set | 1 | 185943 | 0.141 | 0.143 | 0.215 |
| infinitydb | set | 16 | 2040816 | 0.211 | 0.207 | 0.287 |
| infinitydb | get | 1 | 189681 | 0.138 | 0.143 | 0.207 |
| infinitydb | get | 16 | 2074689 | 0.203 | 0.199 | 0.319 |

## Cross-check — memtier vs redis-benchmark throughput

Independent-generator agreement on the same engine/workload. Flagged when the two disagree by more than 25%.

| Engine | Workload | Pipe | memtier (ops/s) | redis-bench (req/s) | Δ | |
|---|---|---:|---:|---:|---:|:--|
| redis | set | 1 | 267052 | 189681 | +40.8% | ⚠ diverges |
| redis | set | 16 | 1790743 | 970874 | +84.4% | ⚠ diverges |
| redis | get | 1 | 269725 | 179662 | +50.1% | ⚠ diverges |
| redis | get | 16 | 2292089 | 2277904 | +0.6% | ok |
| dragonfly | set | 1 | 533482 | 167757 | +218.0% | ⚠ diverges |
| dragonfly | set | 16 | 1554681 | 1490313 | +4.3% | ok |
| dragonfly | get | 1 | 586603 | 179372 | +227.0% | ⚠ diverges |
| dragonfly | get | 16 | 2037402 | 2096436 | -2.8% | ok |
| infinitydb | set | 1 | 735185 | 185943 | +295.4% | ⚠ diverges |
| infinitydb | set | 16 | 3116238 | 2040816 | +52.7% | ⚠ diverges |
| infinitydb | get | 1 | 741202 | 189681 | +290.8% | ⚠ diverges |
| infinitydb | get | 16 | 3486766 | 2074689 | +68.1% | ⚠ diverges |

## Memory attribution — bytes/key

Fill the keyspace, then `(RSS_after − RSS_baseline) ÷ DBSIZE`. The L5 gate shape; the binding ≤ 1.0× Redis gate is `inf-bench gate-run m1` on the reference box.

| Engine | Keys | Value (B) | RSS baseline (MiB) | RSS after (MiB) | bytes/key |
|---|---:|---:|---:|---:|---:|
| redis | 13728 | 64 | 18.1 | 20.2 | 164.1 |
| dragonfly | 27403 | 64 | 26.5 | 27.4 | 36.3 |
| infinitydb | 42682 | 64 | 95.5 | 99.1 | 88.3 |

## Notes & honesty

- redis is single-threaded; dragonfly and infinitydb ran with 4 threads/cells. Each engine kept its own best config (recorded above), per master plan §22.
- GET rows were measured after a 5s sequential populate; redis-benchmark uses its own key format, so its GET cross-check reads against keys memtier didn't write (throughput-comparable, hit rate not).
- redis-benchmark is request-count based (`-n 1000000`) and reports only p50/p95/p99; p99.9 always comes from memtier. The two are compared on throughput, not latency.
- Pub/sub fan-out latency is **not** measured here — memtier/redis-benchmark don't set up subscribers. That row lives in `inf-bench gate-run m1` (delivery-acked).
- Raw memtier JSON + redis-benchmark CSV for every row are under `raw/`.
