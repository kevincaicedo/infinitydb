# inf-compare — competitive benchmark report

> **Tier:** DEV-TIER (non-citable, L10) — plumbing/relative numbers only
>
> - git tree is dirty
> - cpu governor is `powersave` (need `performance`)
> - `inf-bench env-check` failed

| | |
|---|---|
| Generated | unix 1781554490 |
| Git | `819ff22-dirty` |
| Host | 7.0.0-22-generic, 24 cores |
| CPU governor / EPP | `powersave` / `performance` |
| inf-bench env-check | FAIL (`target/release/inf-bench env-check` exit 1) |
| Mode | host |
| Generators | redis-benchmark |
| memtier | `memtier_benchmark v=255.255.255 sha=62413fd6:0 bits=64 libevent=2.1.12-stable openssl=OpenSSL 3.5.5 27 Jan 2026` |
| redis-benchmark | `redis-benchmark 8.0.5` |
| Parameters | duration=10s · threads=4 · clients=50 · value=64 B · keyspace=1000000 · pipeline=1, 16 · maxmemory=unset |

## Engines — published configs

| Engine | Mode | Version | Peak RSS (MiB) | Launch command |
|---|---|---|---:|---|
| redis | host | Redis server v=8.0.5 sha=00000000:0 malloc=jemalloc-5.3.0 bits=64 build=9729964261b8fc0f | 14.9 | `redis-server --port 7000 --save '' --appendonly no` |
| dragonfly | host | dragonfly v1.39.0-699862e5da7cb29bf5642a1da422c4c78cefae38 | 24.5 | `dragonfly --port 7001 --proactor_threads 4 --cache_mode --dbfilename '' --logtostderr --dir /tmp` |
| infinitydb | host | infinityd e1fc538 (git e1fc5383af95-dirty, x86_64-unknown-linux-gnu) | 71.5 | `target/release/infinityd --port 7002 --cells 4` |

## Notes & honesty

- **Non-citable run.** DEV-TIER numbers prove the harness and show relative shape only. A binding number needs `--reference-box` on a clean box (the M0-R2 standing obligation). Authoritative gate: `inf-bench env-check`.
- redis is single-threaded; dragonfly and infinitydb ran with 4 threads/cells. Each engine kept its own best config (recorded above), per master plan §22.
- GET rows were measured after a 5s sequential populate; redis-benchmark uses its own key format, so its GET cross-check reads against keys memtier didn't write (throughput-comparable, hit rate not).
- redis-benchmark is request-count based (`-n 1000000`) and reports only p50/p95/p99; p99.9 always comes from memtier. The two are compared on throughput, not latency.
- Pub/sub fan-out latency is **not** measured here — memtier/redis-benchmark don't set up subscribers. That row lives in `inf-bench gate-run m1` (delivery-acked).
- Raw memtier JSON + redis-benchmark CSV for every row are under `raw/`.
