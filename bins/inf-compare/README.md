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

Zero dependencies: it only orchestrates external binaries
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

Reports land in ignored `.artifacts/compare/<unix-nanos>-compare/` (or `--out DIR`).
Keep them local; commit commands, configurations and result summaries, not
generated output. See [validation and reference hardware](../../docs/validation.md).

Local output:

```
report.md            # tier, per-leg configs, replicate summaries and individual samples
placement.txt        # server/generator logical CPU ranges, or unverified isolation
schedule.tsv         # actual leg order, replicate, scenario, artifact and status
raw/<leg>/*.memtier.json   # original generator JSON, one file per measured leg
raw/<leg>/*.redisbench.csv # original redis-benchmark CSV
raw/<leg>/*.info.*.txt     # INFO before/after; memory legs carry memory.tsv
logs/<leg>/config.txt     # exact command, version, mode and durability
logs/<leg>/<engine>.log   # stdout+stderr (host) / .container id (docker)
```

## Options (`inf-compare run`)

| Flag | Default | Meaning |
|---|---|---|
| `--engines` | all present | Comma list of `redis,dragonfly,infinitydb`. Default = those available on host (or with a local image under `--docker`). |
| `--generator` | `both` | `both` \| `memtier` \| `redis-benchmark`. |
| `--workload` | `all` | Single name, `all`, or a comma list (e.g. `set,get,memory`). See **Workloads**. |
| `--duration` | `30` | memtier `--test-time` seconds per row. |
| `--replicates` | `3` | Independent measurements per engine/workload/pipeline; 1–5 for development, 3–5 with `--reference-box`. |
| `--threads` | `4` | → infinityd `--cells`, dragonfly `--proactor_threads`, memtier `-t`. redis stays single-threaded. |
| `--clients` | `50` | Connections per generator thread. |
| `--pipeline` | `1,16` | Comma list → one row each. |
| `--data-size` | `64` | Value size in bytes. |
| `--keyspace` | `1000000` | Key space (memtier `--key-maximum`, redis-benchmark `-r`). |
| `--maxmemory-mb` | unset | Cap every engine (`allkeys-lru`); enables the `eviction` workload. |
| `--rb-requests` | `1000000` | redis-benchmark request count (`-n`). |
| `--crosscheck-threshold` | `25` | Flag a row when memtier and redis-benchmark throughput disagree by more than this %. |
| `--durability` | `none` | `none` or `everysec`. Everysec supports host-launched Redis and InfinityDB; Dragonfly is refused because no equivalent durable launch mode is verified. |
| `--data-root` | `.artifacts/compare-data` | Per-engine durable directories, wiped before each leg after launch configuration validation. |

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

## Replicates and order

For each workload/pipeline, the harness runs every eligible engine in each
replicate before moving to the next replicate. It rotates the requested order
one position per round: two engines produce `AB / BA / AB`, three produce
`ABC / BCA / CAB`. Unsupported engine/workload/generator combinations are
recorded as skipped and do not change the eligible engines' rotation. Memory
rows run once per engine/replicate; pipeline is not applicable.

Each leg has a fresh launched process and, for durable runs, a fresh data
directory. Attached servers receive FLUSHALL and the workload's preload but
retain process/cache state. Every raw file and log lives under a distinct leg
directory. A failed leg stops the run with a nonzero exit, preserves earlier
artifacts and the failed schedule entry, and produces no success report.
Owned servers are stopped and reaped on setup and measurement errors too.
Launched legs use distinct ports (`port-base + ordinal - 1`) to avoid
immediate address reuse during kernel socket cleanup; the full port span is
validated before launch. Attached endpoints stay unchanged.

Summaries report n, median, minimum, maximum and relative spread
`(max-min)/median*100` for each metric; zero medians or unrepresentable
percentages show `n/a`. Latencies are medians of per-run quantiles, never
pooled request percentiles. Optional observations disclose their own n.
Individual samples retain their replicate and execution ordinal. Replication
alone does not establish reference validity or authorize a public claim.
`--unsafe-env` cannot bypass the 3–5 reference replicate requirement.

For a short development smoke, use `--replicates 1`. Empty selections,
duplicate workloads/pipelines, zero pipelines and overflowing port ranges
refuse before artifacts or launches; at most 16 pipeline depths are accepted.

**Placement**

| Flag | Default | Meaning |
|---|---|---|
| `--docker` | off | Run servers in containers; the generator stays on the host. |
| `--attach` | — | `redis=host:port,dragonfly=host:port,…` — use running servers, skip launch/teardown. |
| `--port-base` | `7000` | Launched legs get `N + ordinal - 1`; the entire span must fit in a port. |
| `--pin-start` | — | First logical CPU of the process-wide server mask, width `--threads`; applies to every host engine. |
| `--load-pin-start` | — | First logical CPU of a disjoint generator mask; required together with `--pin-start`. |
| `--load-cpus` | `--threads` | Generator mask width, including memtier preload/memory fills and redis-benchmark. |

CPU pinning requires Linux host launches. Every engine is wrapped in `taskset`;
InfinityDB also pins its cells within that mask using `--pin-stride 1`; its
standalone default remains stride two. The harness checks the effective
mask before creating artifacts or launching servers, rejects overlap, unavailable
CPUs and trimmed masks (admitted CPU IDs: 0–65535), and fails if `taskset`
cannot run. Docker and attached
servers cannot request controlled placement. Unpinned development runs explicitly
report **CPU isolation unverified**. Logical CPU ranges do not prove physical-core
or SMT isolation; the reference environment must document those separately.

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
| `--reference-box` | Requires both CPU ranges on Linux host launches and a clean box (`inf-bench env-check` must pass). |
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
cargo run --release -p inf-compare -- run --reference-box --duration 60 --pipeline 1,16 \
  --threads 4 --pin-start 0 --load-pin-start 4 --load-cpus 4
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

## Decoder bounds and verification

The independent JSON reader and RESP framer are iterative, with at most 32
open containers including empty ones. JSON accepts at most 16 MiB and 256 Ki
value/key nodes. RESP accepts at most 1 MiB per reply, 64 Ki nodes and 4 KiB
header lines. Explicit errors distinguish malformed input and resource
limits from incomplete replies. Socket/file reads enforce byte budgets before
buffering; fragmented RESP is decoded incrementally. Bounds describe this
instrument's inputs, not InfinityDB's protocol limits.

Regression checks include exact limits, empty-container off-by-one cases,
malformed inputs, fragmented and pipelined replies, bounded file reads,
binary schedule/statistics checks, and fresh-process/error cleanup fixtures:

```bash
cargo test -p inf-compare
cd bins/inf-compare
cargo +nightly fuzz run compare_resp_frame -- -max_total_time=300
cargo +nightly fuzz run memtier_json_parse -- -max_total_time=300
```

Both fuzz targets are wired into `infinity-fuzz-nightly.yml` with authored
boundary seeds under `fuzz/seeds/`. The fuzz package alone depends on
libFuzzer; the shipped instrument remains dependency-free.
