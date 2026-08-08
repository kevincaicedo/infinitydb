# inf-compare — competitive benchmark report

> **Tier:** reference-box (binding, citation-grade)

| | |
|---|---|
| Generated | unix 1783652925 |
| Git | `bc0a4a7` |
| Host | 7.0.0-27-generic, 24 cores |
| CPU governor / EPP | `performance` / `performance` |
| inf-bench env-check | PASS (`target/release/inf-bench env-check` exit 0) |
| Mode | host |
| Generators | memtier + redis-benchmark |
| memtier | `memtier_benchmark v=255.255.255 sha=62413fd6:0 bits=64 libevent=2.1.12-stable openssl=OpenSSL 3.5.5 27 Jan 2026` |
| redis-benchmark | `redis-benchmark 8.0.5` |
| Parameters | duration=30s · threads=4 · clients=50 · value=64 B · keyspace=1000000 · pipeline=1, 16 · maxmemory=1024 MB |

## Engines — published configs

| Engine | Mode | Version | Peak RSS (MiB) | Launch command |
|---|---|---|---:|---|
| redis | host | Redis server v=8.0.5 sha=00000000:0 malloc=jemalloc-5.3.0 bits=64 build=9729964261b8fc0f | 159.5 | `redis-server --port 7000 --save '' --appendonly no --maxmemory 1073741824 --maxmemory-policy allkeys-lru` |
| dragonfly | host | dragonfly v1.39.0-699862e5da7cb29bf5642a1da422c4c78cefae38 | 120.3 | `dragonfly --port 7001 --proactor_threads 4 --cache_mode --dbfilename '' --maxmemory 1073741824 --logtostderr --dir /tmp` |
| infinitydb | host | infinityd v0.2.0-alpha.1-26-gfe797e1 (git fe797e17cdf9, x86_64-unknown-linux-gnu) | 196.9 | `target/release/infinityd --port 7002 --cells 4` |

## Results — memtier_benchmark

| Engine | Workload | Pipe | Throughput (ops/s) | avg (ms) | p50 (ms) | p99 (ms) | p99.9 (ms) | RSS (MiB) |
|---|---|---:|---:|---:|---:|---:|---:|---:|
| redis | set | 1 | 269749 | 0.741 | 0.719 | 1.439 | 1.639 | 133.5 |
| redis | set | 16 | 1752596 | 1.824 | 1.535 | 2.863 | 3.231 | 160.6 |
| redis | mixed | 1 | 274251 | 0.729 | 0.711 | 1.399 | 1.599 | 18.9 |
| redis | mixed | 16 | 2134988 | 1.496 | 1.311 | 2.543 | 2.735 | 22.6 |
| redis | get | 1 | 279762 | 0.715 | 0.695 | 1.383 | 1.591 | 19.6 |
| redis | get | 16 | 2253507 | 1.417 | 1.247 | 2.399 | 2.671 | 19.6 |
| redis | incr | 1 | 267576 | 0.747 | 0.719 | 1.431 | 1.783 | 83.5 |
| redis | incr | 16 | 1891395 | 1.689 | 1.463 | 2.863 | 3.551 | 99.5 |
| redis | mset | 1 | 264723 | 0.755 | 0.735 | 1.463 | 1.775 | 24.9 |
| redis | mset | 16 | 1707673 | 1.872 | 1.591 | 2.815 | 3.647 | 55.0 |
| redis | ttl | 1 | 273612 | 0.731 | 0.719 | 1.415 | 1.527 | 18.6 |
| redis | ttl | 16 | 2041836 | 1.564 | 1.351 | 2.607 | 2.879 | 19.2 |
| dragonfly | set | 1 | 540488 | 0.370 | 0.375 | 0.519 | 0.695 | 88.5 |
| dragonfly | set | 16 | 1575173 | 2.029 | 1.999 | 3.423 | 4.351 | 118.9 |
| dragonfly | mixed | 1 | 582447 | 0.343 | 0.351 | 0.487 | 0.623 | 26.2 |
| dragonfly | mixed | 16 | 1940233 | 1.646 | 1.583 | 3.199 | 4.223 | 28.6 |
| dragonfly | get | 1 | 586733 | 0.341 | 0.343 | 0.495 | 0.735 | 26.8 |
| dragonfly | get | 16 | 2050676 | 1.558 | 1.503 | 3.023 | 3.983 | 29.1 |
| dragonfly | incr | 1 | 563627 | 0.355 | 0.359 | 0.503 | 0.775 | 60.6 |
| dragonfly | incr | 16 | 1813820 | 1.762 | 1.719 | 3.199 | 4.319 | 90.1 |
| dragonfly | mset | 1 | 541388 | 0.369 | 0.375 | 0.527 | 0.903 | 32.9 |
| dragonfly | mset | 16 | 1582635 | 2.019 | 1.983 | 3.487 | 4.479 | 54.0 |
| dragonfly | ttl | 1 | 580761 | 0.344 | 0.351 | 0.503 | 0.719 | 26.5 |
| dragonfly | ttl | 16 | 1860032 | 1.718 | 1.639 | 3.295 | 4.223 | 27.9 |
| infinitydb | set | 1 | 737356 | 0.271 | 0.247 | 0.415 | 0.463 | 150.9 |
| infinitydb | set | 16 | 3045250 | 1.049 | 1.031 | 1.839 | 2.527 | 198.9 |
| infinitydb | mixed | 1 | 752683 | 0.265 | 0.239 | 0.415 | 0.463 | 96.7 |
| infinitydb | mixed | 16 | 3376471 | 0.946 | 0.927 | 1.639 | 2.079 | 100.1 |
| infinitydb | get | 1 | 750573 | 0.266 | 0.247 | 0.415 | 0.455 | 97.5 |
| infinitydb | get | 16 | 3457980 | 0.924 | 0.903 | 1.623 | 2.303 | 97.7 |
| infinitydb | incr | 1 | 767766 | 0.260 | 0.231 | 0.391 | 0.455 | 119.8 |
| infinitydb | incr | 16 | 3169825 | 1.008 | 0.983 | 1.759 | 2.719 | 145.3 |
| infinitydb | mset | 1 | 744188 | 0.268 | 0.239 | 0.407 | 0.599 | 115.5 |
| infinitydb | mset | 16 | 2785057 | 1.147 | 1.127 | 1.975 | 3.151 | 138.9 |
| infinitydb | ttl | 1 | 766601 | 0.261 | 0.231 | 0.399 | 0.583 | 107.1 |
| infinitydb | ttl | 16 | 3305089 | 0.966 | 0.943 | 1.751 | 2.383 | 110.4 |

## Results — redis-benchmark

| Engine | Workload | Pipe | Throughput (req/s) | avg (ms) | p50 (ms) | p99 (ms) |
|---|---|---:|---:|---:|---:|---:|
| redis | set | 1 | 189502 | 0.138 | 0.135 | 0.271 |
| redis | set | 16 | 1027749 | 0.728 | 0.703 | 1.199 |
| redis | get | 1 | 189430 | 0.136 | 0.135 | 0.199 |
| redis | get | 16 | 2202643 | 0.314 | 0.295 | 0.599 |
| redis | incr | 1 | 190150 | 0.137 | 0.135 | 0.207 |
| redis | incr | 16 | 1218027 | 0.613 | 0.583 | 1.023 |
| dragonfly | set | 1 | 177904 | 0.148 | 0.151 | 0.199 |
| dragonfly | set | 16 | 1512859 | 0.507 | 0.503 | 0.839 |
| dragonfly | get | 1 | 180115 | 0.146 | 0.151 | 0.207 |
| dragonfly | get | 16 | 2114165 | 0.353 | 0.351 | 0.503 |
| dragonfly | incr | 1 | 180701 | 0.146 | 0.151 | 0.207 |
| dragonfly | incr | 16 | 1497006 | 0.514 | 0.495 | 1.063 |
| infinitydb | set | 1 | 187723 | 0.140 | 0.143 | 0.191 |
| infinitydb | set | 16 | 1897533 | 0.224 | 0.207 | 0.255 |
| infinitydb | get | 1 | 189825 | 0.137 | 0.143 | 0.191 |
| infinitydb | get | 16 | 2202643 | 0.193 | 0.191 | 0.247 |
| infinitydb | incr | 1 | 190840 | 0.137 | 0.135 | 0.207 |
| infinitydb | incr | 16 | 2036660 | 0.206 | 0.191 | 0.247 |

## Cross-check — memtier vs redis-benchmark throughput

Independent-generator agreement on the same engine/workload. Flagged when the two disagree by more than 25%.

| Engine | Workload | Pipe | memtier (ops/s) | redis-bench (req/s) | Δ | |
|---|---|---:|---:|---:|---:|:--|
| redis | set | 1 | 269749 | 189502 | +42.3% | ⚠ diverges |
| redis | set | 16 | 1752596 | 1027749 | +70.5% | ⚠ diverges |
| redis | get | 1 | 279762 | 189430 | +47.7% | ⚠ diverges |
| redis | get | 16 | 2253507 | 2202643 | +2.3% | ok |
| redis | incr | 1 | 267576 | 190150 | +40.7% | ⚠ diverges |
| redis | incr | 16 | 1891395 | 1218027 | +55.3% | ⚠ diverges |
| dragonfly | set | 1 | 540488 | 177904 | +203.8% | ⚠ diverges |
| dragonfly | set | 16 | 1575173 | 1512859 | +4.1% | ok |
| dragonfly | get | 1 | 586733 | 180115 | +225.8% | ⚠ diverges |
| dragonfly | get | 16 | 2050676 | 2114165 | -3.0% | ok |
| dragonfly | incr | 1 | 563627 | 180701 | +211.9% | ⚠ diverges |
| dragonfly | incr | 16 | 1813820 | 1497006 | +21.2% | ok |
| infinitydb | set | 1 | 737356 | 187723 | +292.8% | ⚠ diverges |
| infinitydb | set | 16 | 3045250 | 1897533 | +60.5% | ⚠ diverges |
| infinitydb | get | 1 | 750573 | 189825 | +295.4% | ⚠ diverges |
| infinitydb | get | 16 | 3457980 | 2202643 | +57.0% | ⚠ diverges |
| infinitydb | incr | 1 | 767766 | 190840 | +302.3% | ⚠ diverges |
| infinitydb | incr | 16 | 3169825 | 2036660 | +55.6% | ⚠ diverges |

## Memory attribution — bytes/key

Fill the keyspace, then `(RSS_after − RSS_baseline) ÷ DBSIZE`. The L5 gate shape; the binding ≤ 1.0× Redis gate is `inf-bench gate-run m1` on the reference box.

| Engine | Keys | Value (B) | RSS baseline (MiB) | RSS after (MiB) | bytes/key |
|---|---:|---:|---:|---:|---:|
| redis | 13282 | 64 | 18.9 | 20.4 | 118.1 |
| dragonfly | 28363 | 64 | 27.0 | 27.7 | 28.2 |
| infinitydb | 42028 | 64 | 108.4 | 111.9 | 88.1 |

## Notes & honesty

- redis is single-threaded; dragonfly and infinitydb ran with 4 threads/cells. Each engine kept its own best config (recorded above), per master plan §22.
- GET rows were measured after a 5s sequential populate; redis-benchmark uses its own key format, so its GET cross-check reads against keys memtier didn't write (throughput-comparable, hit rate not).
- redis-benchmark is request-count based (`-n 1000000`) and reports only p50/p95/p99; p99.9 always comes from memtier. The two are compared on throughput, not latency.
- Pub/sub fan-out latency is **not** measured here — memtier/redis-benchmark don't set up subscribers. That row lives in `inf-bench gate-run m1` (delivery-acked).
- Raw memtier JSON + redis-benchmark CSV for every row are under `raw/`.
