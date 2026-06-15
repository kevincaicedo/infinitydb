# InfinityDB Architecture

This document explains how InfinityDB is put together, from a single request
down to the shared-nothing cell model. It is conceptual overview;
the frozen internal interfaces live in [interfaces-m0.md](interfaces-m0.md).

- [Thread-per-core, shared-nothing](#the-big-idea-thread-per-core-shared-nothing)
- [A cell, end to end](#a-cell-end-to-end)
- [The life of a request](#the-life-of-a-request)
- [Routing and the fabric](#routing-and-the-fabric)
- [Background work: the MAINTAIN classes](#background-work-the-maintain-classes)
- [Storage: records, index, TTL, eviction](#storage-records-index-ttl-eviction)
- [Crate layering and the dependency law](#crate-layering-and-the-dependency-law)
- [Design laws](#design-laws)

---

## Thread-per-core, shared-nothing

A traditional multi-threaded server shares its data between threads and guards
it with locks. Every shared access is a potential contention point, a cache
line bouncing between cores, a tail-latency spike under load.

InfinityDB removes the sharing instead of guarding it. The keyspace is split
into **slots**; each CPU core runs one **cell** that exclusively owns a range
of slots — its data, its index, its memory, *and its network sockets*. A cell
is a complete miniature database. Because nothing on the data plane is shared
between cells, there are **no locks, no shared atomics, and no cross-core
cache-line ping-pong** on the hot path.

```
        clients (any Redis client / redis-cli)
            │   RESP2 / RESP3 over TCP
            ▼
   ┌─────────────────────────────────────────────────────────────┐
   │  infinityd process                                          │
   │                                                             │
   │   core 0          core 1          core 2          core 3    │
   │  ┌────────┐      ┌────────┐      ┌────────┐      ┌────────┐ │
   │  │ CELL 0 │      │ CELL 1 │      │ CELL 2 │      │ CELL 3 │ │
   │  │ slots  │      │ slots  │      │ slots  │      │ slots  │ │
   │  │ A..    │      │ ..B    │      │ ..C    │      │ ..D    │ │
   │  └───┬────┘      └───┬────┘      └───┬────┘      └───┬────┘ │
   │      └───────────────┴──────┬────────┴───────────────┘      │
   │                      fabric (SPSC rings)                    │
   │            cross-cell hops only — never shared memory       │
   └─────────────────────────────────────────────────────────────┘
```

The OS spreads incoming connections across cells using `SO_REUSEPORT`, so each
cell accepts and serves its own clients on its own `io_uring` instance.

## A cell, end to end

Every cell is the same vertical stack, owned by one thread pinned to one core:

```
   ┌──────────────────────── CELL k (core k) ────────────────────────┐
   │                                                                  │
   │   io_uring reactor                                               │
   │     • multishot accept / recv      • provided buffer pool        │
   │     • batched submit / complete    • timers (the timing wheel)   │
   │                       │                                          │
   │                       ▼                                          │
   │   executor  ── polls command state machines (no per-op malloc)   │
   │                       │                                          │
   │                       ▼                                          │
   │   wire (RESP)  ── parse request argv / encode reply              │
   │                       │                                          │
   │                       ▼                                          │
   │   store                                                          │
   │     • records + hash index (incremental rehash)                  │
   │     • TTL timing wheel (ms / s / min tiers)                      │
   │     • eviction (CLOCK + Count-Min Sketch, 8 policies)            │
   │     • namespaces (per-db arena / index / wheel)                  │
   │                                                                  │
   │   MAINTAIN  ── budgeted background slices (expiry/evict/rehash)  │
   └──────────────────────────────────────────────────────────────────┘
```

Key properties:

- **No data-plane allocation on the fast path.** The common request completes
  on its first poll without allocating a task slot or touching the global
  allocator.
- **Layer isolation.** `inf-runtime` is the only crate that names `io_uring`
  or `kqueue`. `inf-store` never sees a socket; `inf-wire` never sees a
  record; `inf-log` never knows RESP or command semantics.

## The life of a request

A simple `GET key` whose key lives on the accepting cell:

```
client ──"GET key"──▶ io_uring recv (cell k)
                         │
                         ▼
                    RESP parse → argv ["GET","key"]
                         │
                         ▼
                    command metadata + slot(key)
                         │
              slot owned by cell k?  ── yes ──▶ store.get(key)
                         │ no                        │
                         ▼                           ▼
                  fabric hop to owner         encode RESP reply
                  (see next section)                 │
                                                     ▼
                                          io_uring send ──▶ client
```

Every command enters through the same kernel: command metadata, key/slot
routing, the store, and (as they come online) durability effects and
ACL/compat hooks. No command is allowed to tunnel around this path.

Commands are modeled as **resumable state machines**: the local fast path pays
essentially nothing for the ability to suspend, but when a command must wait
(a cross-cell hop, a blocked read), it suspends cleanly instead of blocking the
core.

## Routing and the fabric

When a command touches a key owned by another cell, the work crosses the
**fabric** — a mesh of single-producer/single-consumer (SPSC) ring buffers,
one directed pair per cell-to-cell link. The fabric is the *only* way cells
communicate; there is no shared mutable memory between them.

```
   CELL 1 (accepting)                         CELL 2 (owns slot)
   ┌────────────────┐                         ┌────────────────┐
   │ parse "GET k"  │                         │                │
   │ slot(k)=cell 2 │   ──── SPSC ring ───▶   │ store.get(k)   │
   │ suspend cmd    │                         │ build reply    │
   │                │   ◀─── SPSC ring ────   │                │
   │ resume + send  │                         │                │
   └────────────────┘                         └────────────────┘
```

Everything that crosses a boundary is **batched** — syscalls, fabric hops,
fsyncs, cache misses — because a boundary crossed once per item is the usual
source of latency cliffs. Backpressure is explicit and bounded with credits and
budgets; the fabric never grows an unbounded queue.

Pub/Sub rides this same fabric: a channel is owned by its slot's cell, each
subscriber's state lives on that subscriber's own cell, and a `PUBLISH`
fans out as one fabric message per subscriber-bearing cell (not one per
subscriber) — with no global subscriber table.

## Background work: the MAINTAIN classes

Expiry, eviction, and incremental rehash all need CPU, but none of them may be
allowed to stall a foreground request. They run as **budgeted `MAINTAIN`
slices** interleaved with request processing on the same single-threaded cell:

```
   reactor loop (one cell):
     └── serve foreground requests ──┐
                                     ├─▶ MAINTAIN budget available?
     ┌───────────────────────────────┘        │ yes
     │                                          ▼
     │   expiry slice   (timing-wheel ticks, bounded fires per slice)
     │   eviction slice (CLOCK sweep + sketch decay, bounded steps)
     │   rehash slice   (incremental table growth)
     └── back to foreground ◀───────────────────────────────────────
```

Each slice has a hard budget (keys and/or steps). For example, a storm of one
million keys all expiring in the same millisecond is drained over many bounded
slices, with reads serviced in between — the storm cannot ride a single slice
and cliff tail latency. Background debt is tracked and the budget escalates
under pressure while foreground latency stays protected.

## Storage: records, index, TTL, eviction

Within a cell, the store is itself several cooperating pieces:

- **Records + hash index.** Keys map to records through a hash index that
  rehashes incrementally; `SCAN` returns a stable, opaque cursor that survives
  rehashing (every key present for the whole scan is returned at least once).
- **TTL timing wheel.** A hierarchical wheel (millisecond / second / minute
  tiers) holds compact `{key-hash, deadline}` entries — 16 bytes each, with no
  key copies — validated at fire time so a stale entry is a counted no-op, never
  a misfire. A lazy check on read is the backstop.
- **Eviction.** Recency is a CLOCK reference counter packed into spare record
  flag bits (zero extra bytes per key); frequency is an 8 KiB Count-Min Sketch
  with Morris counters, allocated only under an LFU policy. All eight Redis
  policies live in one module; `maxmemory` pressure is checked on the write
  path with a single branch on a cached flag.
- **Namespaces.** `SELECT 0..15` and the `INF.NS` registry give each namespace
  its own arena, index, and wheel — isolation by construction, with per-namespace
  memory accounting.

## Crate layering and the dependency law

InfinityDB is split into crates with a strict, *mechanically enforced*
dependency direction (lower layers never depend on higher ones):

```
            infinityd / inf / inf-bench / inf-sim       (binaries)
                           │
                      inf-server                        (commands, pub/sub)
              ┌────────────┼───────────────┐
          inf-store    inf-wire        inf-log          (engine layer)
              │            │               │
            ┌─┴────────────┴───────────────┘
    inf-runtime  inf-fabric  inf-alloc  inf-simd        (unsafe leaves)
            └──────────────┬───────────────┘
                    inf-foundation                      (core types)
```

`scripts/check-dep-dag.sh` fails CI on any internal edge not declared in
[dep-dag.toml](dep-dag.toml). Safe Rust is the default: `#![forbid(unsafe_code)]`
holds everywhere except the four unsafe leaves (`inf-simd`, `inf-alloc`,
`inf-fabric`, `inf-runtime`), each of which carries a `SAFETY.md` inventory and
is checked with Miri in CI.

## Design laws

The architecture is governed by a small set of non-negotiable laws. The ones
most visible in the code:

| Law | Statement |
|---|---|
| L1 | One core, one shard, one owner — no shared mutable data-plane state. |
| L2 | The log is the database; indexes are rebuildable projections (M2). |
| L3 | Batch every boundary: syscalls, fabric hops, fsyncs, cache misses. |
| L5 | Memory is the product — bytes/key, buffers, and RSS are tracked and gated. |
| L6 | Every command is a resumable state machine; the fast path pays nothing for it. |
| L7 | Determinism is a feature — time, randomness, and I/O are injected for simulation. |
| L8 | Compatibility is staged and honest, declared per command with documented deviations. |
| L9 | Safety is layered — unsafe code is isolated, audited, and fuzzed/model-checked. |
| L10 | Claims follow evidence — public numbers require citation-grade artifacts. |

Determinism (L7) deserves a note: every source of nondeterminism — the clock,
randomness, disk, network, and fabric scheduling — is injected, so the entire
system can run inside a deterministic simulator (`inf-sim`). The same seed
produces byte-identical execution traces, which turns a class of concurrency
bugs into reproducible, replayable failures.
