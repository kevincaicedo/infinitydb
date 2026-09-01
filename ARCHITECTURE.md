# InfinityDB Architecture

This document is a technical overview of the internals of InfinityDB: a
problem statement, an overview of how the system works, and the motivation
behind the major design decisions. It is the first document a new engineer
should read.

Where to go deeper: the architectural authority with laws, gates, and the
roadmap is [`docs/infinity-master-plan.md`](../docs/infinity-master-plan.md)
(repo root); the intra-node walkthrough (life of a request, MAINTAIN
classes) is [`docs/architecture.md`](docs/architecture.md); the frozen
internal interfaces live in `docs/interfaces-m0.md` / `docs/interfaces-m2.md`;
the engineering style is [`docs/INFINITY_STYLE.md`](docs/INFINITY_STYLE.md).

## Problem Statement

Modern applications run a zoo: Redis for caching and real-time structures,
DynamoDB-class stores for durable documents, Kafka/RabbitMQ for queues and
events, a vector database for AI retrieval — plus the glue, the double
writes, and the cross-system consistency bugs between them. Each system is
operated, secured, monitored, and paid for separately, and the data they
share is copied between them over networks.

InfinityDB is one engine for those workloads: a **key/value engine kernel**
that is fully Redis-compatible on the wire (keep `redis-cli`, keep every
Redis SDK), durable and persistent like DynamoDB, a NoSQL document store
(JSON documents as first-class values, typed secondary indexes, a
planner-free query subset), a durable log/queue system, and — inside the
1.0 roadmap — vector search and in-database WASM compute.

The product thesis in one sentence:
application state, events, and application logic living in one engine, fully open source, with two freedoms it is not SQL (documents under designed keys),
and it is not RAM-bound (tiered storage makes beyond-RAM a per-namespace
configuration).

Crucially, **workloads are consequences, not targets**. The engine is a
small kernel — per-core cells, a log spine, versioned extension seams — and
each workload exists because it is a *projection* of that kernel. Anything
that would require new kernel semantics rather than a new engine on
existing seams is out of scope until a law amendment says otherwise.

## Overview

One InfinityDB node is one process, `infinityd`. It pins one **shard cell**
per data core. Each cell is a complete miniature database: an io_uring
reactor, a SIMD RESP parser, a single-threaded command executor, a slice of
the keyspace with its index and record arena, a log writer, and an
allocator. One additional control thread runs the slow plane: config,
topology, stats aggregation, checkpoint scheduling.

The keyspace is partitioned by the same 16,384-slot space as Redis Cluster,
assigned to cells in contiguous ranges. The cell count is therefore part of
what a data directory *means*: it is recorded in `topology.toml` at the
directory's first boot, and a boot whose `--cells` disagrees is a typed
refusal — resizing is an explicit re-shard, never a flag edit (ADR-0095).
Cells share **nothing** on the data
plane: no locks, no shared atomics, no cross-core cache-line traffic. When
a command on cell A needs a key owned by cell B, it crosses the **fabric**
— a full mesh of lock-free SPSC ring pairs, polled cooperatively, batched,
never waking per message — as a typed fabric op, and the command future
suspends until the reply arrives.

Durability is the **log spine**: every durable fact is an append to a
per-cell, per-namespace segmented log, group-committed once per reactor
iteration with an fsync policy chosen per namespace (`always` /
`everysec` / `memory`). The durable barrier comes in two classes, chosen
per segment and per frame (ADR-0086): the **FLUSH class** — a buffered
frame write with a linked `fdatasync`, ending in a device-wide cache
flush every cell queues on — and the **FUA class** — a 4 KiB-aligned
frame written `RWF_DSYNC` on a pre-zeroed `O_DIRECT` segment, durable at
its own completion, independent of the other cells. The device decides:
the probe runs at the **first boot of a data directory** (`--device-probe
auto`, the default — ≈ 10 s, once; ADR-0091) and writes
`io-properties.toml`, whose identity block binds the model to the
filesystem + device it measured (a moved directory is re-probed); the
class is the probe's per-device verdict (`fua` where a write-through
beats a FLUSH by the rule, `flush` otherwise); `--device-probe off` is
the named dev tier (no file ⇒ FLUSH, loudly — never silently the slow
class in production); `INFO persistence` reports both the configured
class (`io_class_configured`) and the active segment's (`barrier_class`,
which reads `flush` on a fresh cell until its class-upgrade rotation).
The same model carries the device's cold sequential read rate, and the
checkpoint interval's replay term is that rate divided across the cells
(ADR-0088 D4 as amended) — a boot's replay budget is the device's, not
a constant. A frame takes the FUA class only when it extends
the durable prefix — a FUA write persists itself, never the un-barriered
frames before it. Frames ride a **bounded pipeline**
(ADR-0087): the staging domain is a ring of K + 1 frame buffers, up to K
sealed frames are in flight at once, the ledger advances its written and
durable watermarks over completion-ordered prefixes, and a frame whose
due barrier cannot yet be honest — a linked fdatasync with earlier writes
still in flight, a rotation's seal — waits one write latency rather than
claiming coverage it does not have. The pipeline depth K is
**class-derived** (ADR-0087's fourth amendment, 2026-08-22:
`--frames-in-flight auto` resolves to 3 under the FUA class and 1 under
FLUSH, both with 4 MiB buffers, after the barrier class is known and
before the ring is sized; an explicit K overrides either). Where an
`always` namespace exists, a FUA-class next segment is pre-zeroed before
it takes frames; a covered, pre-zeroed segment below the MANIFEST floor
is **recycled** into the next one by rename instead of unlinked
(ADR-0090; a bounded one-slot pool per cell, `--no-segment-recycle`
off), so that second write is paid once per generation — the reader
proves the recycled file's previous-life frames inert by their own
segment stamp, never a hole, never data. Because the segment that feeds
the pool is truncated a checkpoint *after* the rotation that needs it,
the prealloc **waits, bounded to a quarter segment**, for the pool
before creating a fresh file (ADR-0090 D9; `--recycle-wait off|quarter|
eighth`), and the fallback's zero-fill paces from its own origin. Background I/O
(zero-fill, tier flush, checkpoints, compaction, backfill) spends a
per-cell share of a **measured device budget** (ADR-0088; the seal
pacer that ADR proposed is an A/B arm, off). On aligned segments a
**barrier-less** frame may hold until it fills 16 KiB or 1 ms elapses
(ADR-0089's fill policy — on by default, `--fill-window-us 0` is the
per-iteration cadence); a frame that carries a barrier is never held by
the fill policy. On the FLUSH class a due frame may instead wait, bounded
to a window, for the round of clients it just acked to re-arrive — the
K = 1 alternation otherwise carries half the population per barrier
(ADR-0092's group hold — `--flush-group-window-us`, 250 µs by default
since its binding A/B of 2026-08-26, `0` the off arm; inert on the FUA
class). Everything else — the hash index, document trees,
secondary indexes, stream offsets, vector graphs — is a rebuildable
projection over that log. Checkpoints are fuzzy snapshots streamed by the
owning cell in budgeted background slices (no fork, no stop-the-world)
as 4 KiB-aligned `O_DIRECT` blocks — no page-cache lump the kernel repays
at once — at an interval **derived** from the last checkpoint's size and
bounded by the recovery gate in the same expression (ADR-0088);
recovery is checkpoint + parallel per-cell log replay. There is no global
LSN: each cell replays independently, and cross-cell atomicity is resolved
by transaction decision records.

A **namespace** binds semantics to keys: durability class, eviction policy,
memory budget, retention. One node simultaneously serves a `memory`
namespace as a cache, an `always` namespace as a ledger, and a `topic`
namespace as a queue — that composition is the point of the product.

Above the kernel, capability arrives as **engines attached to versioned
seams** (command registry, record types, index projections, fabric ops,
triggers, transports): the document engine (`inf-doc`), the query engine
(`inf-query`), streams (`inf-stream`), vectors (`inf-vector`), compute
(`inf-compute`). Engines are feature-gated crates — a cache-only build
compiles none of them.

## Design Decisions

### Gates before features

InfinityDB is a full restart of a failed predecessor (Vortex), and its
post-mortem is constitutional (master plan §2). Vortex optimized components
brilliantly and lost the system: a shared keyspace made I/O batching
impossible, memory unattributable, and persistence globally entangled — and
no end-to-end gate existed to catch it before feature buildout. So
InfinityDB's milestone train runs on **STOP gates**: hard, numeric,
end-to-end thresholds measured on a designated reference box. A failed gate
halts feature work; the fix happens at the architecture layer. Milestone 0
was nothing but this: an end-to-end skeleton proving the architecture
before any feature existed.

### One core, one shard, one owner

Every key, document, topic partition, and log segment is owned by exactly
one core. This is the decision from which the others cascade: persistence,
expiry, eviction, memory accounting, and replication become *local,
single-threaded problems*; NUMA locality holds by construction; data
structures need no `Sync` bounds and no lock-aware designs.

The known cost is hot keys: a single hot key serializes on one core, where
a shared keyspace lets N cores read it. We state this honestly and
mitigate in order: a single cell sustains millions of pipelined ops/s on
one key; RESP3 client-side caching (M7) is the protocol-native fix;
read-leases are post-1.0 research.

The cross-cell fabric is held to the numbers production thread-per-core
systems achieve (sub-microsecond batched hops), because Vortex's
shared-keyspace decision was "proven" by a broken fabric experiment —
internal results that contradict published production behavior are presumed
implementation artifacts until shown otherwise.

### The log is the database

One primitive — the per-cell append-only segmented log — provides Redis
AOF-class durability, DynamoDB-class commit semantics, Kafka-class topics,
replication, CDC, and point-in-time recovery. A cache is the log with
durability off. A queue is the log read forward by consumer groups. A
replica is the log shipped to a follower. Indexes are projections over it,
rebuildable by replay. This is why the feature set can be broad while the
trusted computing base stays small: there is one durability mechanism to
make correct, not five.

### Batch every boundary

Syscalls, fabric hops, fsyncs, and cache misses are paid per *batch*, never
per operation. The reactor produces every SQE of an iteration into a single
`io_uring_enter`; the log writer seals one batch frame per iteration;
`always`-mode writers share one group-commit barrier (one fsync, or one
write-through frame); index probes are prefetched a batch at a time. The always-on tripwire metrics
(`sqes/submit`, `cmds/iteration`, grouping ratio) have CI gates — a
benchmark run with red tripwires is invalid for claims by definition,
because it means the architecture wasn't exercising the thing that makes it
fast.

### Every command is a resumable state machine

Commands compile to `!Send` futures on a minimal cell-local executor — no
work stealing, no atomic wakers, no global runtime. The local fast path
completes synchronously inside `poll` and pays essentially nothing for the
capability; suspension is reserved for the moments that need it: a remote
key (fabric hop), a cold read (NVMe), a durability-gated ack (fsync
watermark), a blocking op (`BLPOP`). One scheduler model covers what would
otherwise be five ad-hoc mechanisms, and background work (expiry, eviction,
checkpoints, compaction, index backfill) runs in deficit-weighted budgeted
slices so p99.9 is protected by construction. Two budgets govern those
slices: CPU in work units (the deficit scheduler) and the **device** in
bytes and ops (ADR-0088) — every cell holds a static share of a measured
device model (`inf probe-device`), foreground classes are metered and
never deferred, background classes (zero-fill, tier flush, checkpoint,
compaction and shadow-reconciliation reads — in that priority) are
granted bounded, work-conserving deficits on the injected clock and told
"not this slice" otherwise; never a queue, never a client-visible
refusal. The tiered write path's one remaining foreground device read —
verifying a cold candidate before a plain `SET` overwrites it — has a
measured off-critical-path form (M4.5-S37, ADR-0093, shipped as an arm
behind `tiered-shadow-overwrite`, default off): the new record wins at
once by the index's own probe order, the candidate stays slotted as a
*shadow* whose winner is pinned in RAM by the release ceiling, and a
MAINTAIN reconciler reads and verifies it later with the same full-key
comparison and the same exact death — nothing is ever removed on hash
evidence, every bound falls back to the synchronous verify, and the
shadow set is a projection of the index that recovery rebuilds. A
ticket is **ambiguous until that read** (ADR-0093 as amended,
2026-08-27): the twin may be the key's old record or a different key
with the same 64 bits, so every answer derived from a ticket is exact
or waits — `DBSIZE` verifies the unverified tickets under an admission
fence before it counts, `SCAN` names the twin like any cold slot, one
cold address carries one ticket, and the recovery rebuild reads the
slots it cannot pair by construction (two RAM keys with one hash, or
pairs beyond the cap) and settles them by full key before the cell
serves; reconciliation is a *verify* (the read, legal under a checkpoint
walk) and a *settle* (read-free, never under a walk).

### Determinism is a feature

Time, randomness, disk, network, and fabric effects are injected behind
runtime traits. Consequently the **entire node** — all cells, fabric, log,
recovery — runs deterministically on one thread inside the simulator
(`inf-sim`), FoundationDB/TigerBeetle-style: simulated disks tear writes and
lie about fsync, power cuts land at chosen LSNs, and every failure is a
seed that replays byte-identically. Invariant oracles (linearizability per
key, durability-watermark honesty, index equivalence, accounting
reconciliation) run continuously inside the sim, and a nightly fleet burns
millions of simulated seconds. Determinism also gives replicas byte-exact
log apply and makes `EXPLAIN` truthful — there is no planner and no
nondeterministic execution to surprise anyone.

The simulator's authority is guarded mechanically: cell code cannot name
`tokio`, locks, `thread::sleep`, ambient clocks, or ambient randomness —
a denylist script enforces it.

### Memory is the product

Vortex shipped 2–3× Redis memory and could not say where it went; that
failure mode is now impossible to hide. Records are variable-size and
packed; the index costs ~8 B/slot; every allocation belongs to a named
per-cell domain counted at the allocation site (no atomics), and
`sum(domains)` diverging from RSS by more than 10% fails CI. Bytes/key and
RSS-vs-Redis are release gates, not dashboards.

### A kernel with seams, not a monolith

Every engine capability registers through a versioned seam: commands enter
via the command registry with typed metadata; new value types register
record-type ids with typed encode/decode and a memory domain; indexes
register as log projections with build-from-replay, checkpoint-sidecar, and
maintenance-slice hooks; cross-cell verbs register fabric opcodes. First-
party engines are **required** to use these seams — the Linux-driver
discipline: drivers prove the driver API — and a mechanical dep-DAG check
makes bypassing them a build failure. This is what makes slim builds real
(a cache-only binary contains zero document/query/vector code) and what the
post-1.0 native extension SDK will stabilize.

The compute plane is the same idea aimed at users: WASM reducers (M10) run
inside the process on the core that owns their data — fuel-metered,
memory-capped, capability-scoped, deterministic — moving code to data as an
*option* alongside the wire protocol, never a replacement for it.

### Redis compatibility as the wedge, honesty as the policy

The wire protocol is RESP2/RESP3, and compatibility is declared per command
in a generated, CI-enforced matrix (`full` / `partial` with documented
deviations / `absent`) that is diffed byte-for-byte against a pinned real
Redis in CI. Compatibility is the adoption wedge — keep your clients, adopt
one namespace at a time — not the identity. Pre-1.0 there is exactly one
wire protocol; new capabilities arrive as `INF.*` commands (namespaces,
indexes, the PartiQL subset) inside it. Wire-protocol gateways (DynamoDB
HTTP, native HTTP/JSON, Kafka) are post-1.0 adapters that must not add
data-plane semantics — the "kernel test".

### Queries without a planner

Typed secondary indexes and a PartiQL subset ship pre-1.0 (M4.5) under a
planner fence: a statement compiles to a straight-line access program —
exactly one access path (primary key, one declared index range, or an
explicitly consented scan), an optional predicate-VM residual filter, and a
cursor. No cost model, no joins, no plan search; anything outside the
subset is rejected with a documented error, and `EXPLAIN` prints the one
program that will run. Access paths are keys and declared indexes — the
DynamoDB discipline, kept by construction rather than restraint.

### Claims follow evidence

Every public number lives in a claim ledger as `Allowed`, `Narrowed`,
`Rejected`, or `Evidence-pending`, tied to artifacts from the designated
reference box: 3–5 replicates, clean tree, recorded governor/thermals,
competitors in the same run, flamegraph and memory attribution attached,
tripwires green. Development-box numbers can never back a public claim. The
comparative instrument is `inf-compare` — a zero-dependency harness driving
industry-standard load generators against Redis, Dragonfly, and InfinityDB
on one box with published configs. Optimizations are hypotheses until an
end-to-end A/B settles them, and a losing A/B is recorded and not merged.

### Rust

The requirements — no GC pauses on a p99.9-gated data plane, precise
layout, zero-cost abstraction over injected effects, a type system strong
enough to make invalid states unrepresentable — narrow the field to Rust
and Zig. Rust wins for this project: temporal safety matters even in a
single-threaded cell (suspension custody — what may be held across a yield
— is exactly the class of bug lifetimes catch), typestate and newtypes turn
our invariants into compile errors, `!Send` futures encode "this never
leaves its core" in the signature, and the ecosystem supplies audited
foundations for the edges (wasmtime for reducers, mlua for scripting)
without touching the zero-dependency core. Stable toolchain, pinned
version; `unsafe` confined to audited leaf crates under Miri and Loom,
plus one module-scoped emit region (`inf_doc::emit`, ADR-0049). The index hash is SipHash-1-3 under a per-data-directory secret (ADR-0094; `hash64` is a digest, not a key hash), so no client-chosen key set can lengthen a probe chain or forge 64-bit "exact" evidence. The secret's identity is named by every `MANIFEST` (epoch 3) and compared before any checkpoint loads — a replaced secret is a typed boot refusal, never a boot with invisible keys; a data directory has one owning process (`LOCK`, taken before anything else is touched) and the secret file is created `0600` and refused otherwise (ADR-0094 D6–D9).

## References

The lineage this design stands on:

- **TigerBeetle** — deterministic simulation, static limits, assertion
  discipline, TIGER_STYLE (our INFINITY_STYLE's parent), and the courage to
  say "zero technical debt" out loud.
- **FoundationDB** — deterministic simulation testing as the centerpiece of
  correctness.
- **Seastar / ScyllaDB** — shard-per-core, scheduling groups, reactor
  discipline.
- **DragonflyDB** — shared-nothing Redis semantics at production scale;
  VLL-style cross-shard transactions.
- **Microsoft FASTER (SIGMOD '18)** — the hybrid-log record store behind
  tiered storage.
- **Redpanda** — thread-per-core Kafka-model storage.
- **simdjson** — SIMD parsing techniques (via the ported Vortex parser).
- **SpacetimeDB** — the in-database reducer model and the idea that state,
  events, and logic belong together.
- **ScyllaDB Alternator** — the DynamoDB-gateway precedent.
- **Kafka** — segment/retention/consumer-group storage model.
- `docs/vortex-master-plan.md` and the Vortex artifacts — the measured
  post-mortem this architecture answers, root cause by root cause.
