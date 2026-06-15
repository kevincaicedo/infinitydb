# InfinityDB

**A Redis-compatible, shared-nothing key/value engine written in Rust.**

[![CI](https://github.com/kevincaicedo/infinitydb/actions/workflows/infinity-ci.yml/badge.svg)](https://github.com/kevincaicedo/infinitydb/actions/workflows/infinity-ci.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

InfinityDB speaks the Redis wire protocol (RESP2/RESP3), so existing Redis
clients and tools work unchanged. Underneath, it is a from-scratch engine
built on a **thread-per-core, shared-nothing** architecture: each core owns a
shard of the keyspace end to end — its own network I/O (Linux `io_uring`),
event loop, memory, and data — with **no locks and no shared mutable state on
the data plane**.

The long-term goal is *one engine* for several data models — cache, durable
log, document, queue, and vector — behind a single protocol surface. The
current public alpha implements the **cache core**.

> [!WARNING]
> **Status: `v0.1.0-alpha` — early alpha.** In-memory only (no persistence
> yet), single-node, no authentication or TLS. Not production-ready.

---

## Table of contents

- [What works today](#what-works-today)
- [Quickstart](#quickstart)
- [Why InfinityDB](#why-infinitydb)
- [Redis compatibility](#redis-compatibility)
- [Architecture](#architecture)
- [Building from source](#building-from-source)
- [Roadmap](#roadmap)
- [Evidence & claims](#evidence--claims)
- [Project layout](#project-layout)
- [Contributing](#contributing)
- [License](#license)

## What works today

The `v0.1.0-alpha` cache core (milestone **M1**) implements:

- **Strings & keys** — the full string family, key management (`RENAME`,
  `COPY`, `TOUCH`, `UNLINK`, `SCAN`, `KEYS`, `RANDOMKEY`, `DBSIZE`, …),
  `FLUSHDB`/`FLUSHALL`.
- **Expiry** — `EXPIRE`/`PEXPIRE`/`EXPIREAT`/…, `SET` with
  `EX/PX/EXAT/PXAT/KEEPTTL`, a hierarchical timing wheel, and budgeted active
  expiry that holds tail latency under same-instant expiry storms.
- **Eviction** — all 8 Redis policies (`noeviction`, `allkeys-lru`,
  `volatile-lru`, `allkeys-lfu`, `volatile-lfu`, `allkeys-random`,
  `volatile-random`, `volatile-ttl`) with per-namespace `maxmemory`.
- **Pub/Sub** — `SUBSCRIBE`/`PSUBSCRIBE`/`PUBLISH`/`PUBSUB`, RESP3 push
  frames, and per-connection output-buffer limits.
- **Namespaces v1** — `SELECT 0..15` plus the `INF.NS` namespace registry
  (memory mode).
- **Introspection** — `INFO`, a `CONFIG GET/SET` subset, `CLIENT`,
  `COMMAND`, and `DEBUG` subsets.

Every command declared `full` is **byte-diff-verified against Redis 8.0.5** —
see the generated [compatibility matrix](docs/compat-matrix.md).

## Quickstart

### Docker

```bash
docker run --rm -p 6379:6379 \
  --security-opt seccomp=deploy/seccomp/infinitydb-seccomp.json \
  ghcr.io/kevincaicedo/infinitydb:v0.1.0-alpha
```

Then point any Redis client at it:

```bash
redis-cli -p 6379 ping            # PONG
redis-cli -p 6379 set hello world # OK
redis-cli -p 6379 get hello       # "world"
```

> [!IMPORTANT]
> **The `--security-opt seccomp=…` flag is required.** InfinityDB uses Linux
> `io_uring`, which Docker's *default* seccomp profile blocks. The bundled
> profile re-enables `io_uring` while still denying the dangerous
> container-escape syscalls. See [docs/deployment.md](docs/deployment.md) for
> the full explanation and alternatives.

### Prebuilt binaries

Static (musl) Linux binaries for `x86-64` and `aarch64` are attached to each
[release](https://github.com/kevincaicedo/infinitydb/releases):

```bash
tar xzf infinitydb-v0.1.0-alpha-linux-x86_64.tar.gz
./infinityd --port 6379
```

## Why InfinityDB

Most Redis-compatible servers either reuse Redis's single-threaded core or add
locks to share data across threads. InfinityDB takes a different path:

- **Thread-per-core, shared-nothing (L1).** One core = one *cell* = one shard,
  owning its data with no cross-core locks, atomics, or shared mutable state.
  A request that lands on the owning core never synchronizes with another core.
- **`io_uring`-native I/O.** The network path is built on `io_uring` from the
  ground up (batched submission, multishot accept/recv, provided buffers) —
  not bolted onto an epoll loop.
- **The log is the database (L2, planned for M2).** Durable state is designed
  as an append-only per-cell log; indexes are rebuildable projections.
- **Memory is a first-class budget (L5).** Per-domain memory attribution
  (records, index, timing wheel, eviction structures) is built.

## Redis compatibility

InfinityDB targets behavioral equivalence with Redis 8.0.5 for the commands it
declares `full`. The [compatibility matrix](docs/compat-matrix.md) is a
**generated artifact** listing every command's status (`full`, `partial`,
`extension`, `internal`), with documented deviations. It is regenerated and
byte-diff-checked in CI on every change, so it can never silently drift from
the implementation.

Deviations are documented — e.g. opaque `SCAN` cursors,
`KEYS`/`SCAN` ordering, and per-cell `FLUSHALL` semantics are listed
explicitly.

## Architecture

A one-paragraph version: a client connection is accepted by one **cell**
(a core + its `io_uring` reactor + executor + store). Commands route to the
cell that owns the key's slot; cross-cell work travels over an internal
**fabric** of single-producer/single-consumer rings, never shared memory.
Background work (expiry, eviction, incremental rehash) runs as budgeted
`MAINTAIN` slices so it cannot starve foreground latency.

See **[docs/architecture.md](docs/architecture.md)** for diagrams and the full
walk-through, and **[docs/interfaces-m0.md](docs/interfaces-m0.md)** for the
frozen internal seams.

## Building from source

Requires Rust **1.95+** and (for the runtime) **Linux with `io_uring`**
(kernel 5.15+; 6.1+ recommended). macOS builds for development via
`kqueue`, but is not a performance target.

```bash
# from the repository root
just check        # fmt + dependency-DAG law + cell deny-list + clippy + tests
cargo build --release -p infinityd
./target/release/infinityd --port 6379
```

Other developer commands:

```bash
just compat       # byte-diff vs a local redis-server
just sim-smoke    # deterministic simulator, trace-identity check
just loom         # concurrency model-check of the SPSC ring
```

## Roadmap

InfinityDB ships on a milestone train. High level:

| Milestone | Theme | Status |
|---|---|---|
| **M0** | Architecture skeleton (cells, fabric, RESP wire, simulator) | Done |
| **M1** | **Cache core** — strings/keys/expiry/eviction/pub-sub | Current |
| M2 | Durability — log spine, group commit, recovery | Planned |
| M3 | Collections — lists, sets, hashes, sorted sets | Planned |
| M4 | Transactions & scripting — `MULTI`/`EXEC`, Lua | Planned |
| M5 | Streams, `AUTH`/TLS, client tracking | Planned |
| M6 | JSON Document SQL++ | Pending |

See **[docs/roadmap.md](docs/roadmap.md)** for details.

## Evidence & claims

InfinityDB follows a strict claim discipline:

- **Correctness claims** (Redis byte-compatibility, deterministic-simulation
  results, TTL correctness) are backed by tests in this repository and CI.
- **Performance and memory claims** require reproducible benchmarks on a
  pinned Linux reference box with controlled CPU governor and thermals.

Until that reference-box evidence exists, this alpha publishes **no**
throughput, latency, or memory-ratio numbers.

## Project layout

```
crates/
  inf-foundation   core types shared across the workspace
  inf-simd  inf-alloc  inf-fabric  inf-runtime   unsafe leaves (each has SAFETY.md)
  inf-wire         RESP protocol (no knowledge of records)
  inf-log          append-only log spine (durability; M2)
  inf-store        records, index, TTL wheel, eviction, namespaces
  inf-server       command execution, pub/sub, client registry
  inf-stream inf-doc inf-vector inf-compute inf-replica   future engines
  infinity-embedded   embedded API
bins/
  infinityd        the server
  inf              CLI client
  inf-bench        benchmark + gate-run harness
  inf-sim          deterministic simulator
tests/compat       Redis byte-diff compatibility suite + matrix generator
deploy/seccomp     Docker seccomp profile (io_uring-enabled, hardened)
docs/              public documentation (this folder)
```

Internal dependency edges are mechanically enforced: `scripts/check-dep-dag.sh`
fails CI on any edge not allowed by `docs/dep-dag.toml`, and `#![forbid(unsafe_code)]`
holds everywhere except the four unsafe leaves, each carrying a `SAFETY.md`.

## Contributing

This is an early alpha under active development; interfaces change between
milestones. Issues and discussion are welcome. See
**[CONTRIBUTING.md](CONTRIBUTING.md)** for development setup, the design laws,
and the unsafe-code and evidence rules. Before sending a change, run
`just check` from the repository root — CI runs the same ladder (fmt,
dependency-DAG law, cell deny-list, clippy `-D warnings`, the full test suite,
Miri on the unsafe leaves, and Loom on the SPSC ring).

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
