# inf-compare — competitive benchmark report

> **Tier:** DEV-TIER (non-citable, L10) — plumbing/relative numbers only
>
> - git tree is dirty
> - cpu governor is `powersave` (need `performance`)
> - `inf-bench env-check` failed

| | |
|---|---|
| Generated | unix 1781554719 |
| Git | `819ff22-dirty` |
| Host | 7.0.0-22-generic, 24 cores |
| CPU governor / EPP | `powersave` / `performance` |
| inf-bench env-check | FAIL (`target/release/inf-bench env-check` exit 1) |
| Mode | host |
| Generators | memtier + redis-benchmark |
| memtier | `memtier_benchmark v=255.255.255 sha=62413fd6:0 bits=64 libevent=2.1.12-stable openssl=OpenSSL 3.5.5 27 Jan 2026` |
| redis-benchmark | `redis-benchmark 8.0.5` |
| Parameters | duration=30s · threads=4 · clients=50 · value=64 B · keyspace=1000000 · pipeline=1, 16 · maxmemory=unset |

## Engines — published configs

| Engine | Mode | Version | Peak RSS (MiB) | Launch command |
|---|---|---|---:|---|
| redis | host | Redis server v=8.0.5 sha=00000000:0 malloc=jemalloc-5.3.0 bits=64 build=9729964261b8fc0f | 171.8 | `redis-server --port 7000 --save '' --appendonly no` |
| dragonfly | host | dragonfly v1.39.0-699862e5da7cb29bf5642a1da422c4c78cefae38 | 143.2 | `dragonfly --port 7001 --proactor_threads 4 --cache_mode --dbfilename '' --logtostderr --dir /tmp` |
| infinitydb | host | infinityd e1fc538 (git e1fc5383af95-dirty, x86_64-unknown-linux-gnu) | 204.4 | `target/release/infinityd --port 7002 --cells 4` |

## Results — memtier_benchmark

| Engine | Workload | Pipe | Throughput (ops/s) | avg (ms) | p50 (ms) | p99 (ms) | p99.9 (ms) | RSS (MiB) |
|---|---|---:|---:|---:|---:|---:|---:|---:|
| redis | set | 1 | 417003 | 0.479 | 0.463 | 0.927 | 1.079 | 134.5 |
| redis | set | 16 | 2726242 | 1.172 | 1.015 | 1.895 | 2.255 | 173.1 |
| redis | mixed | 1 | 427452 | 0.468 | 0.455 | 0.903 | 1.047 | 19.4 |
| redis | mixed | 16 | 3357457 | 0.951 | 0.839 | 1.631 | 1.895 | 25.2 |
| redis | get | 1 | 432398 | 0.462 | 0.447 | 0.895 | 1.031 | 20.2 |
| redis | get | 16 | 3588467 | 0.890 | 0.791 | 1.527 | 1.863 | 20.3 |
| redis | incr | 1 | 423094 | 0.473 | 0.455 | 0.911 | 1.087 | 75.4 |
| redis | incr | 16 | 3063344 | 1.043 | 0.903 | 1.759 | 2.207 | 91.6 |
| redis | mset | 1 | 404974 | 0.494 | 0.471 | 0.935 | 1.119 | 28.2 |
| redis | mset | 16 | 2618706 | 1.220 | 1.079 | 1.911 | 2.447 | 72.3 |
| redis | ttl | 1 | 432014 | 0.463 | 0.455 | 0.903 | 0.951 | 18.8 |
| redis | ttl | 16 | 3245549 | 0.984 | 0.855 | 1.647 | 1.999 | 19.8 |
| dragonfly | set | 1 | 821249 | 0.243 | 0.247 | 0.359 | 0.895 | 95.4 |
| dragonfly | set | 16 | 2451856 | 1.303 | 1.279 | 2.255 | 2.863 | 141.8 |
| dragonfly | mixed | 1 | 883954 | 0.226 | 0.231 | 0.343 | 0.767 | 29.9 |
| dragonfly | mixed | 16 | 3005037 | 1.063 | 1.015 | 2.143 | 3.727 | 33.8 |
| dragonfly | get | 1 | 884749 | 0.226 | 0.231 | 0.351 | 0.919 | 30.4 |
| dragonfly | get | 16 | 3149076 | 1.014 | 0.975 | 2.047 | 2.959 | 32.5 |
| dragonfly | incr | 1 | 840371 | 0.238 | 0.239 | 0.375 | 0.911 | 65.1 |
| dragonfly | incr | 16 | 2824887 | 1.131 | 1.103 | 2.111 | 2.799 | 98.2 |
| dragonfly | mset | 1 | 810015 | 0.247 | 0.247 | 0.383 | 1.063 | 43.6 |
| dragonfly | mset | 16 | 2435136 | 1.313 | 1.287 | 2.383 | 3.551 | 64.7 |
| dragonfly | ttl | 1 | 868025 | 0.230 | 0.231 | 0.367 | 0.807 | 29.6 |
| dragonfly | ttl | 16 | 2959719 | 1.079 | 1.039 | 2.127 | 2.847 | 32.3 |
| infinitydb | set | 1 | 1123507 | 0.178 | 0.167 | 0.295 | 0.775 | 153.2 |
| infinitydb | set | 16 | 4082935 | 0.783 | 0.751 | 1.511 | 3.183 | 206.2 |
| infinitydb | mixed | 1 | 1130383 | 0.177 | 0.159 | 0.287 | 0.775 | 94.7 |
| infinitydb | mixed | 16 | 4601039 | 0.694 | 0.663 | 1.351 | 2.911 | 99.2 |
| infinitydb | get | 1 | 1131173 | 0.177 | 0.167 | 0.295 | 0.807 | 95.8 |
| infinitydb | get | 16 | 4633861 | 0.689 | 0.671 | 1.303 | 3.023 | 95.5 |
| infinitydb | incr | 1 | 1151461 | 0.174 | 0.159 | 0.279 | 0.807 | 118.8 |
| infinitydb | incr | 16 | 4228200 | 0.756 | 0.727 | 1.455 | 3.231 | 145.2 |
| infinitydb | mset | 1 | 1102851 | 0.181 | 0.167 | 0.295 | 0.871 | 124.9 |
| infinitydb | mset | 16 | 3765822 | 0.849 | 0.815 | 1.575 | 3.583 | 150.4 |
| infinitydb | ttl | 1 | 1115266 | 0.179 | 0.167 | 0.303 | 0.799 | 110.5 |
| infinitydb | ttl | 16 | 4506513 | 0.709 | 0.671 | 1.471 | 3.263 | 133.2 |

## Results — redis-benchmark

| Engine | Workload | Pipe | Throughput (req/s) | avg (ms) | p50 (ms) | p99 (ms) |
|---|---|---:|---:|---:|---:|---:|
| redis | set | 1 | 300120 | 0.088 | 0.087 | 0.159 |
| redis | set | 16 | 1420454 | 0.535 | 0.535 | 0.871 |
| redis | get | 1 | 265816 | 0.097 | 0.095 | 0.151 |
| redis | get | 16 | 3378378 | 0.203 | 0.199 | 0.351 |
| redis | incr | 1 | 297885 | 0.088 | 0.087 | 0.151 |
| redis | incr | 16 | 1901141 | 0.390 | 0.375 | 0.679 |
| dragonfly | set | 1 | 258131 | 0.102 | 0.103 | 0.151 |
| dragonfly | set | 16 | 2247191 | 0.342 | 0.335 | 0.567 |
| dragonfly | get | 1 | 259605 | 0.101 | 0.103 | 0.151 |
| dragonfly | get | 16 | 3048780 | 0.231 | 0.223 | 0.383 |
| dragonfly | incr | 1 | 259471 | 0.101 | 0.103 | 0.151 |
| dragonfly | incr | 16 | 2267574 | 0.340 | 0.327 | 0.727 |
| infinitydb | set | 1 | 284981 | 0.093 | 0.095 | 0.143 |
| infinitydb | set | 16 | 3003003 | 0.144 | 0.135 | 0.191 |
| infinitydb | get | 1 | 291545 | 0.090 | 0.095 | 0.151 |
| infinitydb | get | 16 | 3115265 | 0.137 | 0.135 | 0.183 |
| infinitydb | incr | 1 | 283849 | 0.093 | 0.095 | 0.143 |
| infinitydb | incr | 16 | 3058104 | 0.142 | 0.135 | 0.215 |

## Cross-check — memtier vs redis-benchmark throughput

Independent-generator agreement on the same engine/workload. Flagged when the two disagree by more than 25%.

| Engine | Workload | Pipe | memtier (ops/s) | redis-bench (req/s) | Δ | |
|---|---|---:|---:|---:|---:|:--|
| redis | set | 1 | 417003 | 300120 | +38.9% | ⚠ diverges |
| redis | set | 16 | 2726242 | 1420454 | +91.9% | ⚠ diverges |
| redis | get | 1 | 432398 | 265816 | +62.7% | ⚠ diverges |
| redis | get | 16 | 3588467 | 3378378 | +6.2% | ok |
| redis | incr | 1 | 423094 | 297885 | +42.0% | ⚠ diverges |
| redis | incr | 16 | 3063344 | 1901141 | +61.1% | ⚠ diverges |
| dragonfly | set | 1 | 821249 | 258131 | +218.2% | ⚠ diverges |
| dragonfly | set | 16 | 2451856 | 2247191 | +9.1% | ok |
| dragonfly | get | 1 | 884749 | 259605 | +240.8% | ⚠ diverges |
| dragonfly | get | 16 | 3149076 | 3048780 | +3.3% | ok |
| dragonfly | incr | 1 | 840371 | 259471 | +223.9% | ⚠ diverges |
| dragonfly | incr | 16 | 2824887 | 2267574 | +24.6% | ok |
| infinitydb | set | 1 | 1123507 | 284981 | +294.2% | ⚠ diverges |
| infinitydb | set | 16 | 4082935 | 3003003 | +36.0% | ⚠ diverges |
| infinitydb | get | 1 | 1131173 | 291545 | +288.0% | ⚠ diverges |
| infinitydb | get | 16 | 4633861 | 3115265 | +48.7% | ⚠ diverges |
| infinitydb | incr | 1 | 1151461 | 283849 | +305.7% | ⚠ diverges |
| infinitydb | incr | 16 | 4228200 | 3058104 | +38.3% | ⚠ diverges |

## Memory attribution — bytes/key

Fill the keyspace, then `(RSS_after − RSS_baseline) ÷ DBSIZE`. The L5 gate shape; the binding ≤ 1.0× Redis gate is `inf-bench gate-run m1` on the reference box.

| Engine | Keys | Value (B) | RSS baseline (MiB) | RSS after (MiB) | bytes/key |
|---|---:|---:|---:|---:|---:|
| redis | 21199 | 64 | 18.6 | 21.9 | 166.0 |
| dragonfly | 40908 | 64 | 31.3 | 32.9 | 39.9 |
| infinitydb | 66332 | 64 | 115.4 | 121.0 | 88.2 |

## Notes & honesty

- **Non-citable run.** DEV-TIER numbers prove the harness and show relative shape only. A binding number needs `--reference-box` on a clean box (the M0-R2 standing obligation). Authoritative gate: `inf-bench env-check`.
- redis is single-threaded; dragonfly and infinitydb ran with 4 threads/cells. Each engine kept its own best config (recorded above), per master plan §22.
- GET rows were measured after a 5s sequential populate; redis-benchmark uses its own key format, so its GET cross-check reads against keys memtier didn't write (throughput-comparable, hit rate not).
- redis-benchmark is request-count based (`-n 1000000`) and reports only p50/p95/p99; p99.9 always comes from memtier. The two are compared on throughput, not latency.
- Pub/sub fan-out latency is **not** measured here — memtier/redis-benchmark don't set up subscribers. That row lives in `inf-bench gate-run m1` (delivery-acked).
- Raw memtier JSON + redis-benchmark CSV for every row are under `raw/`.
