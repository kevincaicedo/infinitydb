# InfinityDB

[![CI](https://github.com/kevincaicedo/infinitydb/actions/workflows/infinity-ci.yml/badge.svg)](https://github.com/kevincaicedo/infinitydb/actions/workflows/infinity-ci.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

InfinityDB is a multi-model database written in Rust. One engine serves an
in-memory cache, a durable key/value store and JSON documents, and it speaks
the Redis protocol, so existing Redis clients and tools work unchanged. It is
built from scratch on a **shared-nothing, thread-per-core** design: each
*cell* is one thread that owns a shard of the keyspace end to end (its
network I/O on Linux `io_uring`, its memory, its data and its log), so the
data plane has **no locks and no shared mutable state**. Durable namespaces
write to a per-cell append-only log. A durable namespace given a memory budget
keeps its hot data in RAM and tiers the rest to disk (strings only, for now).
The whole node runs inside a **deterministic simulator** that drives time
from a virtual clock, splits network reads at seeded points and injects disk
faults, so every failure replays from a seed.

> [!WARNING]
> **InfinityDB is alpha software and not ready for production.** Commands,
> flags, APIs and on-disk formats may change between versions. No release has
> been published yet, so build from source. The server has no authentication
> or TLS yet and listens on all interfaces: run it only on localhost or on a
> trusted network.

## Quickstart

On Linux (kernel 5.15 or newer), build the server and start it:

```bash
git clone https://github.com/kevincaicedo/infinitydb.git
cd infinitydb
cargo build --release -p infinityd
./target/release/infinityd --port 6379
```

The server runs in the foreground. If a Redis server already listens on
port 6379, InfinityDB refuses to start (`port 6379 is already owned by
another process`): stop that server, or start InfinityDB with `--port 6380`
and give every `redis-cli` below `-p 6380`. In another terminal, point any
Redis client at it:

```bash
redis-cli set hello world
redis-cli get hello
redis-cli JSON.SET user:1 '$' '{"name":"Ada"}'
redis-cli JSON.GET user:1 '$.name'
```

The four replies are `OK`, `"world"`, `OK` and `"[\"Ada\"]"`.

**Durable data.** Stop the first server (Ctrl-C), then start it again with a
data directory outside the clone (so it stays out of git and out of the
Docker build context):

```bash
./target/release/infinityd --port 6379 --data-dir ~/infinity-data
```

The first boot of a new directory spends 10 to 15 seconds measuring the
device before it accepts connections (`--device-probe off` skips it, with
conservative write settings). Meanwhile the server prints progress lines such
as `control: cell 0 not ready — in spawned`: that is the probe at work, not a
hang. The server is ready when it prints `control: recovery complete`.

In another terminal, create a durable namespace and write to it. `INF.NS USE`
selects a namespace for the connection (like `SELECT`), so the three commands
go through one `redis-cli` connection:

```bash
printf '%s\n' \
  'INF.NS CREATE orders MODE durable FSYNC everysec' \
  'INF.NS USE orders' \
  'SET order:1 shipped' | redis-cli
```

Each of the three commands answers `OK`.

Stop the server with Ctrl-C (a graceful stop) and restart it with the same
command. Then, in the other terminal, read the key back:

```bash
printf '%s\n' 'INF.NS USE orders' 'GET order:1' | redis-cli
```

It prints `OK`, then `"shipped"`: the write survived the restart. A command
sent before the restarted server has loaded its data answers `LOADING Redis
is loading the dataset in memory`; wait for the `control: recovery complete`
line, or send it again.

`SELECT 0..15` databases are always memory-only. With `MEM-BUDGET 1gb` on
`INF.NS CREATE`, a durable namespace keeps its hot data in memory and tiers
the rest to disk (string values without expiry, for now).

**Docker.** Build the image from this repository and run it:

```bash
docker build -t infinitydb:dev .
docker run --rm -p 127.0.0.1:6379:6379 \
  --security-opt seccomp=deploy/seccomp/infinitydb-seccomp.json \
  infinitydb:dev
```

Docker's default seccomp profile blocks `io_uring`; the bundled one allows it
([docs/deployment.md](docs/deployment.md) explains the options).

## Why InfinityDB

- **One engine for several data models.** Key/value and JSON documents
  today; secondary indexes and queries next; then streams and queues, vector
  search and in-database compute (WASM). All of them build on the same
  per-core log and share one protocol and one set of operations. Engines are
  feature-gated: the server library already builds without the document
  engine (CI checks the slim build's symbols), and a cache-only server build
  is the goal.
- **Predictable tail latency.** One owner per key, no locks, and batched
  syscalls, fsyncs and cross-core messages. Background work (expiry,
  eviction, flushing, compaction) is designed to run in budgeted slices and
  every queue to have a bound, so overload becomes backpressure or a clear
  error rather than a stall. The gaps still open (no hard cap on executor
  tasks yet, a pause while the key index grows) are listed in the
  architecture document.
- **Memory efficiency.** Compact records, per-cell memory attribution, JSON
  as a compact binary tape, per-namespace memory budgets, and tiering to
  disk for durable namespaces.
- **Correctness you can replay.** Deterministic simulation with torn and
  reordered writes, lost unsynced data and power cuts; planted-bug canaries
  (including an fsync that lies) prove the oracles catch what they should; a
  crash matrix, fuzzed decoders, Loom on the inter-cell ring, Miri on the
  allocator and fabric, and byte-for-byte diffs against Redis.
- **Honest compatibility.** Every deviation from Redis is written down, and
  no performance number is published without a reproducible measurement.

## Redis compatibility

InfinityDB speaks RESP2 and RESP3. Today it implements strings, keys,
expiry, all eight eviction policies, pub/sub, server introspection
(`INFO`, `CONFIG`, `CLIENT`, `COMMAND`) and RedisJSON-compatible `JSON.*`
commands. An oracle sends the same commands to real Redis 8.0.5 (and
RedisJSON for `JSON.*`) and to InfinityDB, both in-process and as a running
multi-cell server, and compares the replies byte for byte.

The generated [compatibility matrix](docs/compat-matrix.md) lists every
command's status and its documented deviations; CI fails if it is stale or if
a reply differs from Redis without a documented deviation. Hashes, lists,
sets, sorted sets, transactions, Lua, streams, AUTH/ACL/TLS and cluster mode
are not implemented yet; see the [roadmap](docs/roadmap.md).

## Architecture

A node runs N cells. Each cell is a thread (optionally pinned to a core) that
owns a range of the 16,384 hash slots and everything behind them: listener,
`io_uring` ring, parser, executor, store, log and memory. A command on a
numbered database whose keys all live on the accepting cell runs inline. A
command that may have to wait (for another cell's key, a durable
acknowledgment, a disk read) becomes a resumable state machine: remote work
travels to the owning cell over the *fabric*, a mesh of
single-producer/single-consumer rings, and the connection waits without
blocking the event loop. Durable writes are appended to the owning cell's
log, group-committed and acknowledged per the namespace's fsync policy;
checkpoints bound recovery time. [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)
explains the design in detail.

## Building from source

You need Linux (x86-64 or aarch64) with `io_uring`, kernel 5.15 or newer
(6.1 or newer recommended); macOS builds through `kqueue` for development
only. `rust-toolchain.toml` pins Rust 1.95, which `rustup` installs for you.
The development tasks use [`just`](https://github.com/casey/just), and the
test suite needs Redis 8.0.5 on `PATH` (or `INF_COMPAT_ORACLE_ADDR` pointing at
one) and Python 3.11 or newer.

- `cargo build --release -p infinityd` builds the server
  (`infinityd --help` prints its usage).
- `just check` runs format, lints, dependency rules, clippy and the test
  suite.
- `just compat` diffs replies byte for byte against a local Redis 8.0.5.
- `just sim-smoke` runs the deterministic simulator scenarios, each twice.
- `just loom` model-checks the inter-cell ring and checkpoint issuance.

[docs/validation.md](docs/validation.md) describes the full validation setup.

## Roadmap

The architecture, the cache core, durability and JSON documents are done.
The current milestone brings storage beyond RAM (working today for string
values), secondary indexes and queries, and the security basics that come
before the first public alpha. See [docs/roadmap.md](docs/roadmap.md).

## Project layout

```
crates/
  inf-foundation   shared types: ids, slot math, hashing, injected time
  inf-runtime      per-cell event loop and executor (io_uring, kqueue)
  inf-fabric       inter-cell SPSC ring mesh
  inf-alloc        buffer pools, arenas, memory accounting
  inf-simd         SIMD parsing, hash probes and CRC32C
  inf-wire         RESP protocol and command registry
  inf-store        records, hash index, expiry, eviction, namespaces, tiering,
                   ordered maps and secondary-index maintenance
  inf-log          durable log, checkpoints, tier files
  inf-server       command execution, pub/sub, durability wiring
  inf-doc          JSON documents
  inf-query        query compiler and predicate VM (not wired to the server yet)
  inf-probe        storage-device probe
  inf-stream  inf-vector  inf-compute  inf-replica   future engines (stubs)
  infinity-embedded   embedded library (stub)
bins/
  infinityd        the server
  inf              small CLI client
  inf-sim          deterministic simulator
  inf-bench        benchmark and exit-gate harness
  inf-compare      comparison harness against other servers
tests/             Redis compatibility suite, crash matrix, client smoke tests
deploy/            Docker seccomp profile (io_uring enabled),
                   client smoke-test image
docs/              architecture, deployment, compatibility, engineering style
scripts/           CI and mechanical-check scripts, simulator lanes
website/           project website
```

Crate dependencies are checked against [docs/dep-dag.toml](docs/dep-dag.toml)
in CI. `unsafe` code is confined to a few audited crates and modules, each
documented in a `SAFETY.md`; the compiler rejects it everywhere else.

## Contributing

Issues and pull requests are welcome; open an issue before starting a large
change. [CONTRIBUTING.md](CONTRIBUTING.md) covers setup, the design rules and
the review process. Run `just check` before sending a change.

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
