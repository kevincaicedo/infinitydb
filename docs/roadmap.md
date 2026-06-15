# InfinityDB Roadmap

InfinityDB is developed as a sequence of **milestones**, each a coherent,
shippable increment. This is the public, high-level view; detailed internal
milestone plans are referenced by ID (e.g. *M1-S07*) but are not part of this
repository.

> Milestones are scoped by capability, not by date. "Planned" means designed
> and on the train, not scheduled for a specific release date.

## The milestone train

### M0 — Architecture skeleton ✅ *Done*

The thinnest end-to-end vertical slice that proves the architecture: shard
cells, the inter-cell fabric, the RESP wire protocol, a minimal store, the
benchmark/gate harness, and the deterministic simulator. M0 ended in an
architecture *verdict*, not a public release.

### M1 — Cache core 🔵 *Current — `v0.1.0-alpha`*

Turns the skeleton into a usable, memory-only Redis-compatible cache:

- The full string / key-management / expiry / server-introspection command
  surface, byte-diff-verified against Redis 8.0.5.
- A hierarchical TTL timing wheel with budgeted active expiry that holds tail
  latency under same-instant expiry storms.
- An eviction engine with all eight Redis policies (CLOCK recency + Count-Min
  Sketch frequency) and per-namespace `maxmemory`.
- Namespaces v1 (`SELECT` + the `INF.NS` registry, memory mode).
- Pub/Sub over the fabric, with RESP3 push and per-connection output caps.
- Release engineering: Docker image, signed release artifacts, an SBOM, and a
  generated, CI-checked compatibility matrix.

This is the first public artifact. Performance and memory gates are validated
on a Linux reference box before any number is published.

### M2 — Durability 🔜 *Planned*

The log an append-only per-cell log, group commit, fsync
policy, per-cell log sequence numbers, checkpoints, crash-consistent recovery,
and durable namespaces. (L2).

### M3 — Collections *Planned*

Lists, sets, hashes, and sorted sets, plus keyspace notifications, sharded
pub/sub (`SSUBSCRIBE`), and `SLOWLOG`/`MONITOR`.

### M4 — Transactions & scripting *Planned*

`MULTI`/`EXEC`/`WATCH`, Lua scripting, and cross-cell atomicity guarantees.

### M5 — Streams, security, tracking *Planned*

Redis streams, `AUTH` and TLS, and client-side caching / tracking.

### M6 — JSON *Pending*

A JSON document type over the same engine. (SQL++ or custom language)

## Beyond the cache

The longer-term vision is *one engine* serving multiple data models — cache,
durable log, document, queue, and vector — behind a single protocol surface,
with first-party engines consuming versioned internal seams rather than
forking the core. The crate skeleton for these engines (`inf-stream`,
`inf-doc`, `inf-vector`, `inf-compute`, `inf-replica`) already exists in the
workspace and fills in as the milestones land.

## How to read status in this repo

- The [compatibility matrix](compat-matrix.md) is the source of truth for
  which commands work today.
- Performance and memory claims are evidence-gated: until the reference-box
  campaign lands, this alpha publishes no such numbers.
- CI status reflects the current state of `main`.
