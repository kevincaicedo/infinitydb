# inf-compare

The InfinityDB **competitive** benchmark harness: it drives the industry-standard
load generators (`memtier_benchmark` + `redis-benchmark`) against **redis**,
**dragonfly**, and **infinitydb** on one box — host processes, docker
containers, or already-running servers — and renders a single markdown report
with throughput, latency (p50/p99/p99.9), RSS, and bytes/key.

It is the competitor-anchored complement to [`inf-bench`](../inf-bench/README.md).
`inf-bench` is the *in-house* loadgen + exit-gate harness; `inf-compare` is the
*independent generator* cross-check the master plan §22 requires: _"same box,
same workload files, configs published. No comparison ships from a run the
competitor wasn't in."_

Zero external crates, like `inf-bench`: it only orchestrates external binaries
and parses their output with a hand-rolled JSON reader (`src/json.rs`), so it
shares neither code nor a dependency surface with the system under test.

## Quick start

```bash
just benchmark                       # all present engines, all workloads, both generators
just benchmark --workload mixed --duration 10 --pipeline 1
cargo run --release -p inf-compare -- run [options]
cargo run --release -p inf-compare -- list-workloads
cargo run --release -p inf-compare -- help
```

Reports land in ignored `.artifacts/compare/<unix>-compare/` (or `--out DIR`).
Keep them local; commit commands, configurations and result summaries, not
generated output. See [validation and reference hardware](../../docs/validation.md).

Local output:

```
report.md            # tier banner, published configs, result tables, honesty notes
raw/<engine>-<workload>-p<pipe>.memtier.json   # memtier JSON each row was parsed from
raw/<engine>-<workload>-p<pipe>.redisbench.csv # redis-benchmark CSV
logs/<engine>.log    # the engine's stdout+stderr (host) / .container id (docker)
```

## Options (`inf-compare run`)

| Flag | Default | Meaning |
|---|---|---|
| `--engines` | all present | Comma list of `redis,dragonfly,infinitydb`. Default = those available on host (or with a local image under `--docker`). |
| `--generator` | `both` | `both` \| `memtier` \| `redis-benchmark`. |
| `--workload` | `all` | Single name, `all`, or a comma list (e.g. `set,get,memory`). See **Workloads**. |
| `--duration` | `30` | memtier `--test-time` seconds per row. |
| `--threads` | `4` | → infinityd `--cells`, dragonfly `--proactor_threads`, memtier `-t`. redis stays single-threaded. |
| `--clients` | `50` | Connections per generator thread. |
| `--pipeline` | `1,16` | Comma list → one row each. |
| `--data-size` | `64` | Value size in bytes. |
| `--keyspace` | `1000000` | Key space (memtier `--key-maximum`, redis-benchmark `-r`). |
| `--maxmemory-mb` | unset | Cap every engine (`allkeys-lru`); enables the `eviction` workload. |
| `--rb-requests` | `1000000` | redis-benchmark request count (`-n`). |
| `--crosscheck-threshold` | `25` | Flag a row when memtier and redis-benchmark throughput disagree by more than this %. |
| `--durability` | `none` | `none` or `everysec`. Everysec supports host-launched Redis and InfinityDB; Dragonfly is refused because no equivalent durable launch mode is verified. |
| `--data-root` | `.artifacts/compare-data` | Per-engine durable directories, wiped only after launch configuration validation. |

For an every-second durable comparison, select the supported engines:

```bash
just benchmark --engines redis,infinitydb --durability everysec --workload set --pipeline 1
```

Dragonfly's [official AOF documentation](https://www.dragonflydb.io/docs/managing-dragonfly/aof)
states AOF is unsupported (checked 2026-09-16); the installed 1.39.0 binary
and source at 1.39.0/1.40.2 provide no matching every-second fsync mode.
Scheduling snapshots does not establish equivalent
durability. An explicit or automatically selected Dragonfly causes an
`everysec` run to fail before any engine launches or data directory is
prepared; it is never silently omitted. Dragonfly remains available under
`--durability none`. Docker and attached servers are refused for `everysec`.
The report lists durability per engine; attached servers are marked
`unverified (attached)` because the harness does not configure them.

**Placement**

| Flag | Default | Meaning |
|---|---|---|
| `--docker` | off | Run servers in containers; the generator stays on the host. |
| `--attach` | — | `redis=host:port,dragonfly=host:port,…` — use running servers, skip launch/teardown. |
| `--port-base` | `7000` | Launched engines get `N, N+1, …`. |
| `--pin-start` | — | `taskset` base core for host launches (fairness); infinityd uses its own `--pin-start`. |

**Docker images** (with `--docker`)

| Flag | Default |
|---|---|
| `--redis-image` | `redis:8.0.5` |
| `--dragonfly-image` | `docker.dragonflydb.io/dragonflydb/dragonfly` |
| `--infinitydb-image` | `infinitydb:dev` (build with `just docker-build`) |
| `--seccomp` | `deploy/seccomp/infinitydb-seccomp.json` |

**Evidence**

| Flag | Meaning |
|---|---|
| `--out DIR` | Artifacts root (default `.artifacts/compare`). |
| `--reference-box` | Bind the numbers — requires a clean box (`inf-bench env-check` must pass). |
| `--unsafe-env` | Proceed on a non-clean box; stamps the run non-citable. |

## Workloads (gated to the M1 string surface)

`redis-benchmark`'s default `-t` set fires `lpush/sadd/hset/zadd/lrange`, none of
which M1 implements (collections are M3) — so every workload here names
string-family commands only.

| Workload | Driver | redis-benchmark cross-check | In `all` |
|---|---|---|---|
| `set` | memtier ratio 1:0 | `-t set` | yes |
| `mixed` | memtier ratio 1:10 | — (no ratio mode) | yes |
| `get` | memtier ratio 0:1 (populated first) | `-t get` | yes |
| `incr` | memtier `--command "INCR …"` | `-t incr` | yes |
| `mset` | memtier `--command "MSET …"` | — (rb MSET writes 10 keys/op) | yes |
| `ttl` | memtier ratio 1:10 + `--expiry-range 1-5` | — (rb can't attach a TTL) | yes |
| `memory` | fill + `DBSIZE` + RSS delta → **bytes/key** | — | yes |
| `eviction` | memtier ratio 1:0 vs `--maxmemory-mb` | `-t set` | **opt-in** |

## Modes & metrics

- **host** — spawn the binary as a child; RSS from `/proc/<pid>/status` (peak `VmHWM` + current `VmRSS`).
- **docker** — `docker run -d` a container; RSS from `docker stats` (no separate peak). infinitydb runs with the io_uring **seccomp profile** because its only Linux backend is io_uring and Docker's default seccomp denies it.
- **attach** — talk to an already-running server; RSS is `n/a` (no owned PID/container).

Throughput + p50/p99/**p99.9** come from memtier (`ALL STATS / Totals`, never a
per-second bucket). redis-benchmark is request-count based and reports only
p50/p95/p99, so it feeds the **cross-check** (throughput agreement), not a
co-equal latency table. Each engine's exact launch command is published in the
report (the "configs published" requirement).

## Examples

```bash
# Full host sweep: all three engines, both generators, every workload.
cargo run --release -p inf-compare -- run \
  --engines redis,dragonfly,infinitydb --generator both --workload all \
  --duration 30 --threads 4 --clients 50 --pipeline 1,16

# Memory-only comparison (bytes/key) at two value sizes.
cargo run --release -p inf-compare -- run --workload memory --data-size 64 --keyspace 1000000

# Eviction pressure under a 512 MB cap.
cargo run --release -p inf-compare -- run --workload eviction --maxmemory-mb 512 --keyspace 50000000

# Servers in containers; generator on the host.
cargo run --release -p inf-compare -- run --docker --engines redis,infinitydb

# Benchmark a server you started yourself, plus a launched infinitydb.
cargo run --release -p inf-compare -- run \
  --engines redis,infinitydb --attach redis=127.0.0.1:6379

# Reference-box, citation-grade (refuses unless the box is clean).
cargo run --release -p inf-compare -- run --reference-box --duration 60 --pipeline 1,16
```

## Tier honesty (L10)

The report leads with a tier banner so a number can never be quoted without its
context. A run is **DEV-TIER (non-citable)** unless `--reference-box` is given on
a clean box. `inf-compare` records governor and EPP for every Linux sysfs
`cpuN`, including sparse CPU IDs. Each reading must exist and equal
`performance`; missing/empty/unreadable policy files, including on offline
CPUs, refuse reference admission. CPU enumeration must also succeed and
find at least one CPU. The report names each CPU and its reading.

A built `target/release/inf-bench` (or `target/debug/inf-bench` when release
is absent), resolved from the working directory, must also execute
`env-check` successfully. An absent, unexecutable or failing checker refuses
reference admission. This authoritative check also covers thermal state
and the git tree. Run from the workspace root after building `inf-bench`.
A `--reference-box` run on
a non-clean box is **refused** unless `--unsafe-env` is passed, which stamps the
result non-citable. macOS is dev-tier only and cannot run dragonfly (Linux-only).

## What this tool deliberately is not

It does not measure pub/sub fan-out latency: memtier/redis-benchmark do not set
up subscribers, so that row stays with `inf-bench gate-run m1` (delivery-acked).
It is not a replacement for `inf-bench` — it is the external cross-check that
sits beside it.
