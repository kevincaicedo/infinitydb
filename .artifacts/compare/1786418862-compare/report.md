# inf-compare — competitive benchmark report

> **Tier:** reference-box (binding, citation-grade)

| | |
|---|---|
| Generated | unix 1786418862 |
| Git | `9ab2818` |
| Host | 7.0.0-29-generic, 12 cores |
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
| redis | host | taskset from util-linux 2.41.3 | 160.1 | `taskset -c 4 redis-server --port 7000 --save '' --appendonly no` |
| dragonfly | host | taskset from util-linux 2.41.3 | 123.1 | `taskset -c 4-7 dragonfly --port 7001 --proactor_threads 4 --cache_mode --dbfilename '' --logtostderr --dir /tmp` |
| infinitydb | host | infinityd 02f870c (git 02f870cfc97d, x86_64-unknown-linux-gnu) | 204.6 | `target/release/infinityd --port 7002 --cells 4 --pin-start 4` |

## Results — memtier_benchmark

| Engine | Workload | Pipe | Throughput (ops/s) | avg (ms) | p50 (ms) | p99 (ms) | p99.9 (ms) | RSS (MiB) |
|---|---|---:|---:|---:|---:|---:|---:|---:|
| redis | set | 1 | 274346 | 0.729 | 0.679 | 1.375 | 1.671 | 133.0 |
| redis | set | 16 | 1704770 | 1.874 | 1.895 | 2.863 | 4.191 | 160.2 |
| redis | get | 1 | 287456 | 0.695 | 0.655 | 1.327 | 1.551 | 19.0 |
| redis | get | 16 | 2318984 | 1.376 | 1.367 | 2.111 | 3.055 | 19.0 |
| redis | mixed | 1 | 285855 | 0.699 | 0.663 | 1.335 | 1.503 | 18.4 |
| redis | mixed | 16 | 2168194 | 1.472 | 1.463 | 2.255 | 2.799 | 22.2 |
| dragonfly | set | 1 | 528556 | 0.378 | 0.407 | 0.679 | 0.871 | 88.7 |
| dragonfly | set | 16 | 1589487 | 2.010 | 1.975 | 3.311 | 4.671 | 120.8 |
| dragonfly | get | 1 | 550091 | 0.363 | 0.303 | 0.879 | 0.927 | 26.6 |
| dragonfly | get | 16 | 2070279 | 1.542 | 1.487 | 2.847 | 4.255 | 28.2 |
| dragonfly | mixed | 1 | 547165 | 0.365 | 0.303 | 0.871 | 0.919 | 26.6 |
| dragonfly | mixed | 16 | 1956028 | 1.632 | 1.575 | 2.991 | 3.967 | 28.7 |
| infinitydb | set | 1 | 542749 | 0.368 | 0.255 | 0.919 | 0.951 | 150.6 |
| infinitydb | set | 16 | 3561540 | 0.896 | 0.847 | 1.839 | 2.639 | 205.7 |
| infinitydb | get | 1 | 551963 | 0.362 | 0.247 | 0.639 | 0.943 | 97.2 |
| infinitydb | get | 16 | 3956834 | 0.806 | 0.759 | 1.663 | 1.999 | 97.2 |
| infinitydb | mixed | 1 | 556129 | 0.359 | 0.247 | 0.879 | 0.935 | 96.5 |
| infinitydb | mixed | 16 | 3892965 | 0.819 | 0.775 | 1.687 | 1.823 | 101.8 |

## Results — redis-benchmark

| Engine | Workload | Pipe | Throughput (req/s) | avg (ms) | p50 (ms) | p99 (ms) |
|---|---|---:|---:|---:|---:|---:|
| redis | set | 1 | 187582 | 0.138 | 0.135 | 0.199 |
| redis | set | 16 | 1063830 | 0.703 | 0.679 | 1.119 |
| redis | get | 1 | 183891 | 0.140 | 0.143 | 0.199 |
| redis | get | 16 | 2288330 | 0.294 | 0.287 | 0.423 |
| dragonfly | set | 1 | 183016 | 0.144 | 0.143 | 0.199 |
| dragonfly | set | 16 | 1529052 | 0.502 | 0.495 | 0.831 |
| dragonfly | get | 1 | 183621 | 0.143 | 0.143 | 0.191 |
| dragonfly | get | 16 | 2123142 | 0.347 | 0.351 | 0.495 |
| infinitydb | set | 1 | 190259 | 0.138 | 0.135 | 0.183 |
| infinitydb | set | 16 | 1972386 | 0.218 | 0.207 | 0.399 |
| infinitydb | get | 1 | 189825 | 0.137 | 0.135 | 0.191 |
| infinitydb | get | 16 | 2192982 | 0.193 | 0.191 | 0.335 |

## Cross-check — memtier vs redis-benchmark throughput

Independent-generator agreement on the same engine/workload. Flagged when the two disagree by more than 25%.

| Engine | Workload | Pipe | memtier (ops/s) | redis-bench (req/s) | Δ | |
|---|---|---:|---:|---:|---:|:--|
| redis | set | 1 | 274346 | 187582 | +46.3% | ⚠ diverges |
| redis | set | 16 | 1704770 | 1063830 | +60.2% | ⚠ diverges |
| redis | get | 1 | 287456 | 183891 | +56.3% | ⚠ diverges |
| redis | get | 16 | 2318984 | 2288330 | +1.3% | ok |
| dragonfly | set | 1 | 528556 | 183016 | +188.8% | ⚠ diverges |
| dragonfly | set | 16 | 1589487 | 1529052 | +4.0% | ok |
| dragonfly | get | 1 | 550091 | 183621 | +199.6% | ⚠ diverges |
| dragonfly | get | 16 | 2070279 | 2123142 | -2.5% | ok |
| infinitydb | set | 1 | 542749 | 190259 | +185.3% | ⚠ diverges |
| infinitydb | set | 16 | 3561540 | 1972386 | +80.6% | ⚠ diverges |
| infinitydb | get | 1 | 551963 | 189825 | +190.8% | ⚠ diverges |
| infinitydb | get | 16 | 3956834 | 2192982 | +80.4% | ⚠ diverges |

## Memory attribution — bytes/key

Fill the keyspace, then `(RSS_after − RSS_baseline) ÷ DBSIZE`. The L5 gate shape; the binding ≤ 1.0× Redis gate is `inf-bench gate-run m1` on the reference box.

| Engine | Keys | Value (B) | RSS baseline (MiB) | RSS after (MiB) | bytes/key |
|---|---:|---:|---:|---:|---:|
| redis | 14005 | 64 | 17.6 | 20.1 | 183.7 |
| dragonfly | 27136 | 64 | 26.7 | 28.2 | 58.0 |
| infinitydb | 39425 | 64 | 95.6 | 98.9 | 88.1 |

## Notes & honesty

- redis is single-threaded; dragonfly and infinitydb ran with 4 threads/cells. Each engine kept its own best config (recorded above), per master plan §22.
- GET rows were measured after a 5s sequential populate; redis-benchmark uses its own key format, so its GET cross-check reads against keys memtier didn't write (throughput-comparable, hit rate not).
- redis-benchmark is request-count based (`-n 1000000`) and reports only p50/p95/p99; p99.9 always comes from memtier. The two are compared on throughput, not latency.
- Pub/sub fan-out latency is **not** measured here — memtier/redis-benchmark don't set up subscribers. That row lives in `inf-bench gate-run m1` (delivery-acked).
- Raw memtier JSON + redis-benchmark CSV for every row are under `raw/`.
