# InfinityDB Roadmap

InfinityDB is alpha software. It is built as a sequence of milestones, each
scoped by capability rather than by date: a milestone is done when its
features work from the wire and its exit gates pass, not when a calendar says
so. An exit gate is a pass/fail check the milestone must meet: a
correctness test, a simulation or crash sweep, or a measurement against a
stated budget. No release has been published yet; the first public alpha
is cut inside M4, after trust and safety and before JSON documents beyond
RAM (M4's last phase). Milestones after the current one may be reordered.

Status: **Done** (built, tested, and its exit gates run), **In progress**, or
**Planned** (scoped, not started).

## M0: Architecture skeleton (Done)

- Shard cells, each a thread with its own `io_uring` event loop (`kqueue` on
  macOS for development).
- The fabric: a mesh of single-producer/single-consumer rings between cells,
  with credit-based flow control.
- The RESP wire protocol, perfect-hash command dispatch and a minimal store.
- The benchmark harness that measures each milestone's exit gates, and the
  deterministic simulator.
- Ended in an architecture verdict, measured against other servers, rather
  than a release.

## M1: Cache core (Done)

- The string, key, expiry and server command families, diffed byte for byte
  against Redis.
- A hierarchical timer wheel with budgeted active expiry, so a mass expiry
  runs in bounded slices.
- All eight Redis eviction policies (CLOCK recency plus a Count-Min Sketch for
  frequency).
- Namespaces (`SELECT` and the `INF.NS` registry) and pub/sub across cells
  with RESP3 push and per-connection output limits.
- The generated compatibility matrix checked in CI, nightly simulation runs,
  and a release pipeline (static binaries, a container image, an SBOM), not
  yet used for a release.

## M2: Durability (Done)

- A per-cell append-only log: checksummed log frames, segments, group commit
  and a MANIFEST.
- Durable namespaces with `always` (acknowledged once the write is durable
  on the device) or `everysec` (at most about one second of loss on power
  failure, like Redis).
- Fuzzy checkpoints, log truncation and parallel recovery; `INF.CKPT` and
  `BGSAVE`.
- A crash matrix and simulated disk faults in the deterministic simulator
  (torn and reordered writes, lost unsynced data, power cuts), with
  planted-bug canaries, including an fsync that lies, that the oracles must
  catch.
- A hardening pass: group-commit and replay performance, cross-cell overhead,
  a comparison harness against Redis, Redis Stack and Dragonfly, fuzzing and
  soundness audits.

## M3: JSON documents (Done)

- JSON as a first-class value, stored as a compact binary tape.
- A SIMD JSON parser and a JSONPath subset compiled to cached path programs.
- RedisJSON-compatible `JSON.*` commands, diffed against RedisJSON, with
  in-place path updates.
- Durable documents through delta log records, crash-atomic updates and
  per-document memory accounting.
- JSONPath filter expressions (`?(@...)`) moved to M4, where the query engine
  lands.

## M4: Beyond RAM, indexes and query, first public alpha (In progress)

- **Tiered storage (done for string values):** a per-cell hybrid-log address
  space, cold reads through `io_uring` with direct I/O, compaction, large
  values as blob extents, and per-namespace memory and disk budgets.
- **Write path (done):** write barriers chosen by a device probe at first
  boot, a bounded pipeline of log-frame writes, a device I/O budget, log
  segment recycling, backpressure instead of errors under load, and a
  graceful stop that keeps every acknowledged write.
- **Secondary indexes and a PartiQL subset (in progress):** the ordered index,
  index maintenance and backfill, the predicate VM and the query compiler are
  built and tested; the command surface (index DDL, queries, `EXPLAIN`,
  JSONPath filters) comes next, after the work that bounds and reserves
  capacity across the engine.
- **Trust and safety (planned):** bind-address control with a protected
  default, `AUTH`/`requirepass`, an end-to-end request envelope (output,
  request-size and work limits with typed refusal causes), and published
  format and support matrices. The first public alpha release follows.
- **JSON documents beyond RAM (planned):** cold document reads and updates,
  expiry on tiered namespaces, indexes over cold documents, and queries that
  touch a single cell (a tenant's keys co-located by hash tag).

## M5: Data types (Planned)

- Hashes, lists, sets and sorted sets (sorted sets reuse M4's ordered index).
- Bitmaps, bitfields and HyperLogLog (may be cut).
- Size-adaptive encodings, the `SCAN` family for every type, `OBJECT` and
  `MEMORY USAGE`.
- Keyspace notifications, sharded pub/sub, `SLOWLOG` and `MONITOR`.

## M6: Transactions and scripting (Planned)

- `MULTI`/`EXEC`/`WATCH`/`DISCARD`: inline on one cell, and across cells
  through deterministic, ordered lock acquisition.
- A native all-or-nothing transaction command and conditional writes on a
  revision.
- Lua scripting (`EVAL`) with declared keys and a `FUNCTION` subset.
- Single-cell transactions come first; transactions across cells may first
  ship as a typed refusal.

## M7: Streams and queues, first beta (Planned)

- Topics built on the same per-cell log, and the full Redis Streams surface.
- Queue features: offset consumption, idempotent producers, dead-letter
  queues, delayed delivery and retention.
- Client tracking, multi-user ACLs, TLS and RDB import.
- An agent-memory profile: freshness barriers, fenced worker claims,
  per-namespace users and work budgets, and adapters for Python and
  TypeScript agent frameworks.
- Backup, restore and point-in-time recovery, including restore into a
  different cell count.

## M8: Vector search (Planned)

- Redis-compatible vector sets and native vector collections.
- Per-cell HNSW with SQ8 quantization and rescoring; k-NN gathered across
  cells.
- Filter predicates, in-memory or tiered residency, and embeddings kept in
  step with document revisions.

## M9: Replication and high availability (Planned)

- Per-cell log shipping, with state digests to verify replicas.
- `WAIT`, `WAITAOF` and read-from-replica with session tokens.
- Two HA profiles: an asynchronous replica with a stated recovery point, and
  strong HA over a quorum-replicated log.
- A Raft control plane for metadata only, fenced failover, migration from a
  live Redis replica, and a Jepsen-style fault harness.

## M10: Compute and enterprise, release candidate (Planned)

- WASM reducers that run on the cell that owns the data, with fuel and memory
  limits, scoped capabilities and transactional invocation.
- Module, call and trigger commands for those reducers.
- An embedded library mode and a shared-memory IPC transport.
- Full ACLs, an audit log, and documented extension seams.

## M11: Hardening for 1.0 (Planned)

- No new features: long mixed-workload soaks, broader fuzzing, and a full
  performance re-baseline with every public claim measured again.
- A frozen compatibility matrix and frozen extension seams: the 1.0 contract.
- An upgrade and downgrade path, a documentation site and a third-party
  security audit.

InfinityDB 1.0 is a single-node and replicated database with a stated failure
model per HA profile, serving cache, durable key/value, JSON documents (beyond
RAM, with expiry and indexes), streams and queues, and vector search over
RESP, with WASM reducers and embedded modes, in slim or full builds.

## Beyond 1.0

Committed direction, not scheduled: cluster mode with slot migration, a
native extension SDK against versioned seams, live queries, protocol gateways
(a DynamoDB-compatible HTTP API first, then HTTP/JSON, WebSocket and the Kafka
wire protocol), a Kubernetes operator, compacted topics and multi-region
asynchronous replication.

Before 1.0, InfinityDB is not a SQL database (no query planner or joins),
not a wire-compatible clone of DynamoDB, MongoDB or Kafka, not multi-region,
and not a host for Redis `.so` modules; the module APIs it supports, such as
`JSON.*`, are native.

## Reading the current state

- The [compatibility matrix](compat-matrix.md) is the source of truth for
  which commands work today.
- No performance or memory numbers are published until a reproducible
  measurement backs them.
- CI status reflects the `main` branch.
