# inf-compare — competitive benchmark report

> **Tier:** reference-box (binding, citation-grade)

| | |
|---|---|
| Generated | unix 1787440571 |
| Git | `70065ff` |
| Host | 7.0.0-30-generic, 4 cores |
| CPU governor / EPP | `performance` / `performance` |
| inf-bench env-check | PASS (`target/release/inf-bench env-check` exit 0) |
| Mode | host |
| Generators | memtier |
| memtier | `memtier_benchmark v=255.255.255 sha=62413fd6:0 bits=64 libevent=2.1.12-stable openssl=OpenSSL 3.5.5 27 Jan 2026` |
| Parameters | duration=60s · threads=4 · clients=8 · value=1024 B · keyspace=1000000 · pipeline=1 · maxmemory=unset |
| Load shape | offered 100000 ops/s (memtier --rate-limiting 3125 per connection × 32 connections) · durability=everysec · data root `/home/kcaicedo/bench-data/s40/data-corrected` · device `nvme0n1` |

## Engines — published configs

| Engine | Mode | Version | Peak RSS (MiB) | Launch command |
|---|---|---|---:|---|
| infinitydb | host | infinityd 70065ff (git 70065fffeaad, x86_64-unknown-linux-gnu) | 302.2 | `target/release/infinityd --port 7400 --cells 4 --pin-start 0 --data-dir /home/kcaicedo/bench-data/s40/data-corrected/infinitydb --conn-default-ns cmp` |
| redis | host | taskset from util-linux 2.41.3 | 352.0 | `taskset -c 0-3 redis-server --port 7401 --save '' --appendonly yes --appendfsync everysec --dir /home/kcaicedo/bench-data/s40/data-corrected/redis` |

## Results — memtier_benchmark

| Engine | Workload | Pipe | Throughput (ops/s) | achieved/offered | avg (ms) | p50 (ms) | p99 (ms) | p99.9 (ms) | max (ms) | server CPU (%) | device MiB written | RSS (MiB) |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| infinitydb | set | 1 | 98785 | 0.99 | 0.059 | 0.055 | 0.127 | 0.591 | 9.663 | 100 | 7720.2 | 302.2 |
| redis | set | 1 | 96560 | 0.97 | 0.166 | 0.143 | 0.287 | 3.887 | 524.287 | 69 | 8294.7 | 241.2 |

### Persistence and stall deltas

| Engine | Workload | Pipe | Before/after summary |
|---|---|---:|---|
| infinitydb | set | 1 | parks +0; ckpt_bytes +182673408; stall_p99_us 895; stall_p999_us 22527 |
| redis | set | 1 | aof_rewrites +42; aof_delayed_fsync +0; aof_last_status ok; child_cpu_s +10.424 |

## Notes & honesty

- redis command execution is single-threaded, but its process tree was allowed the same 4 CPUs as InfinityDB's cells so AOF rewrite children did not contend with the command thread on one pinned CPU. Each engine's config is recorded above.
- GET rows were measured after a 5s sequential populate; redis-benchmark uses its own key format, so its GET cross-check reads against keys memtier didn't write (throughput-comparable, hit rate not).
- redis-benchmark is request-count based (`-n 1000000`) and reports only p50/p95/p99; p99.9 always comes from memtier. The two are compared on throughput, not latency.
- Pub/sub fan-out latency is **not** measured here — memtier/redis-benchmark don't set up subscribers. That row lives in `inf-bench gate-run m1` (delivery-acked).
- **Offered-rate row (M4.5-S40).** memtier paces each connection at `--rate-limiting` = rate ÷ connections; `achieved/offered` below 0.90 means the generator (or the server) could not hold the rate and the latency columns are not an offered-rate measurement. `max (ms)` is memtier's worst request; server CPU covers the host process plus Redis's completed AOF-child CPU and live descendants; device MiB written is the block device's sectors-written delta (journal and metadata included, NAND amplification not). Raw INFO before/after each row is under `raw/`.
- **Durability everysec.** redis ran `--appendonly yes --appendfsync everysec` (its AOF under the data root); infinitydb ran `--data-dir` with every connection starting in an `FSYNC everysec` namespace (`--conn-default-ns cmp`, proven by a probe key before the row) — the same ≤ 1 s power-loss window on both sides, each engine's own mechanism, both on the same device.
- Raw memtier JSON + redis-benchmark CSV for every row are under `raw/`.
