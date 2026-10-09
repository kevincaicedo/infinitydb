# InfinityDB Architecture

This document explains how InfinityDB works inside: the problem it addresses,
the principles that shape it, and a walk through the system from a client's
bytes to the disk and back. It describes the engine as it exists in this
repository today. Anything designed but not built yet is collected in
[Not built yet](#not-built-yet) at the end and is not presented as working
anywhere else.

InfinityDB is **alpha** software. Formats, flags and command behaviour can
still change from one commit to the next.

Companion documents:

- [INFINITY_STYLE.md](INFINITY_STYLE.md): the engineering rules the code
  follows.
- [interfaces-m0.md](interfaces-m0.md) and [interfaces-m2.md](interfaces-m2.md):
  the frozen internal seams (runtime, fabric, store, log).
- [The index and query interfaces](interfaces-m4.5.md): the ordered-index,
  path-program and query seams (draft).
- [compat-matrix.md](compat-matrix.md): Redis compatibility, command by
  command, with the deviations recorded for each command.
- [deployment.md](deployment.md), [validation.md](validation.md) and
  [roadmap.md](roadmap.md).

## Contents

- [The problem](#the-problem)
- [Overview](#overview)
- [Vocabulary](#vocabulary)
- [Design principles](#design-principles)
- [Inside a cell](#inside-a-cell)
- [The life of a request](#the-life-of-a-request)
- [The fabric](#the-fabric)
- [The wire and the command table](#the-wire-and-the-command-table)
- [Storage inside a cell](#storage-inside-a-cell)
- [Durability](#durability)
- [Tiered storage: datasets larger than RAM](#tiered-storage-datasets-larger-than-ram)
- [JSON documents](#json-documents)
- [Secondary indexes and queries](#secondary-indexes-and-queries)
- [Commands that span cells](#commands-that-span-cells)
- [Pub/sub](#pubsub)
- [Background work: MAINTAIN](#background-work-maintain)
- [Limits and backpressure](#limits-and-backpressure)
- [Observability](#observability)
- [Safety](#safety)
- [How we know it works](#how-we-know-it-works)
- [Crate map](#crate-map)
- [Not built yet](#not-built-yet)
- [References](#references)

## The problem

Many applications run several data systems side by side: Redis for caching
and real-time state, a durable key/value or document store for entities, a
log or queue for events, and increasingly a vector index for retrieval. Each
system has its own operations, security model, memory behaviour and failure
modes. The data moves between them over the network, and every boundary adds
double writes and consistency bugs.

InfinityDB is one engine for these workloads. It is a multi-model database
written in Rust, built around key/value storage:

- **A Redis-compatible server.** RESP2 and RESP3 on the wire, so `redis-cli`
  and existing Redis client libraries work without changes.
- **Durability chosen per namespace.** A namespace can be memory-only, or
  durable on a write-ahead log with `everysec` or `always` fsync semantics
  and periodic checkpoints.
- **Datasets larger than RAM, per namespace.** A durable namespace can be
  given a memory budget; records beyond it live in tier files on disk.
- **JSON documents as first-class values.** A compact binary document format
  and a RedisJSON-compatible `JSON.*` command family.
- **Secondary indexes and a small query language.** Typed indexes over
  document fields and a planner-free PartiQL subset. The engine is built;
  its commands are not exposed yet.
- **Ahead:** streams and queues, vector search, replication, and
  in-database compute ([roadmap](roadmap.md)).

The idea that makes one engine possible is that these workloads are all
views of one primitive: a per-core, append-only, segmented log.

- A cache is the same store with the log switched off: memory namespaces
  never write to it, so they pay nothing for it.
- A durable key/value store is the log, plus an in-memory index, plus
  checkpoints so recovery does not replay from the beginning of time.
- A document store is the same thing with a value type that can apply a
  path edit and log it as a small delta.
- A queue is the log read forward by consumers (not built yet).
- A replica is the log shipped to another node (not built yet).

So the core stays small: one execution model, one durability mechanism, one
memory accounting scheme. Each workload beyond key/value is an engine
attached to that core, not a separate system bolted on beside it.

It helps to say what InfinityDB is not. It is not a SQL database: there are
no joins and no cost-based planner, and data is reached by key or by a
declared index. It is not a Redis module host: the module APIs it adopts,
such as `JSON.*`, are implemented natively. And today it is a single-node
system.

## Overview

One InfinityDB node is one process, `infinityd`.

```
              clients: redis-cli, any Redis client library
                  │   RESP2 / RESP3 over TCP
                  │   (every cell has its own SO_REUSEPORT listener)
 ┌────────────────┼─────────────────────────────────────────────────┐
 │ infinityd      ▼                                                 │
 │  ┌─────────────────┐   ┌─────────────────┐   ┌─────────────────┐ │
 │  │ cell 0          │   │ cell 1          │   │ cell N-1        │ │
 │  │ slots 0 ..      │   │ slots ..        │   │ slots .. 16383  │ │
 │  │                 │   │                 │   │                 │ │
 │  │ io_uring ring   │   │ io_uring ring   │   │ io_uring ring   │ │
 │  │ buffer pool     │   │ buffer pool     │   │ buffer pool     │ │
 │  │ RESP parser     │   │ ...             │   │ ...             │ │
 │  │ executor        │   │                 │   │                 │ │
 │  │ store + index   │   │                 │   │                 │ │
 │  │ log + ckpt      │   │                 │   │                 │ │
 │  └────────┬────────┘   └────────┬────────┘   └────────┬────────┘ │
 │           └──── fabric: one SPSC ring per directed ───┘          │
 │                 cell pair, no shared data structures             │
 │                                                                  │
 │  control thread: catalog writer, file deletions (--data-dir)     │
 └──────────────────────────────────────────────────────────────────┘

 data directory (shard-N/ is cell N's directory):
   LOCK  META  key-hash.toml  topology.toml  io-properties.toml
   shard-0/MANIFEST  shard-0/log/seg-NNNNNN.ilog  shard-0/ckpt/ckpt-NNNNNN.ick
   shard-0/ns-N/cold/{tier-NNNNNN.itier, blob-NNNNNN.iblob}
   shard-1/...
```

**Cells.** The node runs N cell threads (`--cells`, default 4). A cell is a
complete miniature database: it owns a listener, an io_uring instance, a
buffer pool, a command executor, a slice of the keyspace with its index and
memory, and that slice's log, checkpoints and tier files. Cells share no
mutable data. The intended deployment is one cell per core, pinned with
`--pin-start` and `--pin-stride`; pinning is off by default.

**Slots.** A key maps to one of 16,384 slots with the same function Redis
Cluster uses: CRC16 of the key, or of its `{hash tag}` if it has one. Each
cell owns a contiguous range of slots. Because the cell count decides which
cell owns which key on disk, it is part of what a data directory means: it is
recorded in `topology.toml` on first boot, and a later boot with a different
`--cells` is refused.

**The fabric.** When a command on one cell needs a key owned by another, it
crosses the fabric: a mesh of single-producer, single-consumer rings, one per
directed pair of cells. The fabric is the only way cells talk to each other.

**The control thread.** With a data directory, one extra thread persists the
node catalog (namespace and index definitions). Catalog changes are rare and
slow, so the file writes and fsyncs live off the data plane. Namespace and
index ids come from shared atomic counters that the cell issuing the DDL
bumps. The control thread also deletes truncated log segments and stale
checkpoint files on behalf of the cells, because freeing a large file's
pages is slow in the kernel. The main thread samples process gauges (RSS,
CPU) and waits for the cells to exit.

**Boot.** `main` parses flags (there is no config file), takes the data
directory's `LOCK`, loads or creates the key-hash secret, checks the
topology, loads the catalog, resolves the storage device's properties, and
only then starts the cell threads. The cells recover their logs in parallel,
and the node answers `-LOADING`, on every cell, until all of them have
finished. No cell accepts connections until every cell has reserved its
caches.

## Vocabulary

| Term | Meaning |
|---|---|
| cell | One thread that owns a slot range and everything that serves it. |
| slot | One of 16,384 hash buckets of the keyspace; each is owned by one cell. A qualified slot (an index slot, a ring slot, a wheel slot, a task slot) is a position in that structure and unrelated. |
| fabric | The ring mesh cells use to send each other work and replies. |
| shard | On disk, a cell's directory: `shard-N/` belongs to cell N. |
| pump | A per-connection future that runs the commands that must wait. |
| gate | A typed wait point: a fabric reply, an I/O completion, the durable watermark. |
| exit gate | A pass/fail measurement a milestone must meet; unrelated to the wait-point gates. |
| MAINTAIN | The loop step that runs budgeted background work. |
| namespace | A named keyspace with its own durability, eviction and memory settings. |
| record | One key and its value, packed into a cell's arena. |
| frame | A log frame: the records staged since the last seal, written with one write. A loop iteration seals at most one. |
| message | One fabric operation between cells. |
| LSN | A record's position in its cell's log: (segment, offset). |
| durable watermark | The highest LSN known to be durable on the device. Tiered storage has watermarks of its own, described there. |
| checkpoint | A snapshot of a cell's state, written as a log prefix. |
| MANIFEST | The per-cell file naming the current checkpoint and live log segments. |
| tier file | An on-disk file holding records a tiered namespace moved out of RAM. |

## Design principles

Each principle below is a consequence of the problem. The project's rule is
that each one is enforced by the strongest mechanism available: a type,
then a lint, then a generated table, then a self-tested script, and only
then code review.

### One core, one shard, one owner

Locks and shared atomics cost contention, cache-line transfers and tail
latency. Worse, sharing turns every subsystem into a global one: persistence,
expiry, eviction and memory accounting all have to coordinate across
threads. Give every key exactly one owning cell and all of those become
local, single-threaded problems.

This shows up directly in the types. A cell's store is an `Rc<RefCell<...>>`,
not an `Arc<Mutex<...>>`. Command futures are `!Send`, and the executor's
wakers use no atomic instructions. Cell code may not name locks, channels,
`tokio`, `sleep`, thread spawning, or the ambient clock or randomness; a
denylist script and clippy's `disallowed-types` / `disallowed-methods`
reject them at build time.

A few things do cross threads, and each is small and off the per-key path:

- the fabric ring indices (release/acquire, never `SeqCst` on the ring), one
  doorbell flag per directed cell pair, and one park flag per cell with an
  `eventfd` to wake a parked peer;
- per-cell slots on shared boards (memory gauges, boot recovery and
  `-LOADING` progress, checkpoint publications, index readiness), each
  written by its own cell and read by the others, plus the process gauges
  the main thread publishes;
- a few multi-writer control counters that any cell may bump: the namespace
  id, index id and catalog epoch allocators (`fetch_add`), each cell's
  checkpoint request slot (`fetch_max`), the boot countdown every cell
  decrements once its caches are reserved, and the node-wide stop and
  quiesce counters;
- the node-wide DDL ticket, taken by compare-and-swap, so one namespace DDL
  program runs at a time;
- a bounded channel (256 messages) to the control thread. A catalog persist
  request is a blocking send, so a full queue briefly blocks the cell issuing
  the DDL; a file deletion is a non-blocking send that is retried on the next
  MAINTAIN slice when the queue is full.

There is no lock anywhere on the data plane.

The cost is stated plainly: a single hot key lives on one core and cannot be
spread across several.

### The log is the database

Every durable fact is a log append first. The in-memory hash index, document
trees and secondary indexes are projections: they are rebuilt from the log,
or loaded from a checkpoint that is itself written as a prefix of the log.
Recovery therefore has one vocabulary. Loading a checkpoint and replaying the
log tail both go through the same idempotent upsert, so there is one
recovery path to get right, not two.

### Batch every boundary

The useful work of a pipelined `GET` is small: hash the key, probe the index,
read the record, copy the reply. Everything else is overhead, and the
expensive overheads are boundaries: system calls, messages between cores,
fsyncs and cache misses. InfinityDB pays each boundary once per batch:

- one `io_uring_enter` per loop iteration, carrying every operation the
  iteration queued;
- at most one log frame per iteration, and one durability barrier covering
  every durable write that is due;
- fabric messages packed many to a ring slot and published in batches: the
  replies a drain produced right after that drain, everything else at the
  end of the iteration;
- index lines prefetched for a whole batch of parsed commands before any of
  them executes.

Batching is also measured. The loop exports its batching ratios (SQEs per
submit, CQEs per reap, commands per iteration, fabric messages per batch) as
always-on tripwires in `INFO tripwires`. A benchmark whose tripwires show the
batching did not happen is not a valid measurement.

### Commands suspend only when they must

A command on a numbered database (`SELECT 0` to `15`) whose keys all live on
the connection's cell executes on the spot, inside the parse step: no
future, no task, no allocation of a task slot. A command that may have to
wait (for another cell, for a durable acknowledgment, for log space, for a
disk read) runs in a future on the cell's executor and suspends on a typed
gate when it must.
So the fast path pays nothing for the ability to suspend, and one mechanism
covers every kind of waiting.

The rule that makes suspension safe: nothing borrowed before a suspension is
trusted after it. No buffer lease, borrow of the store, index position or
address is held across an `await`. A resumed command looks up what it needs
again.

### Determinism is a feature

Time, randomness, network and disk are injected. The loop takes a `Clock`,
randomness comes from seeded `SplitMix64` streams, the network and the
io_uring backend sit behind a `BackendDriver` trait, and every log and tier
file effect goes through a `SegmentFs` trait. Where iteration order could
reach output, cell code uses `BTreeMap`s or a fixed hasher, never a
per-process random seed.

The payoff is the simulator: the whole node, all cells included, runs on one
thread with a virtual clock, simulated sockets and a simulated disk that
loses, tears and reorders unsynced writes. The same seed gives the same
execution, byte for byte, so every failure is a seed you can replay. See
[How we know it works](#how-we-know-it-works).

### Memory is the product

For a cache, memory per key is the cost of the product. Records are packed
with an 8-byte header. The index slot is 8 bytes and holds no keys. The
eviction clock bits live in spare bits of the record's flags byte, so they
cost nothing. The bytes a cell holds are attributed, at the allocation site,
to named and counted domains (records, index, TTL wheel, documents,
secondary indexes, buffers, and so on), and `INFO memory` reports them. The
exit-gate harness fails a run where the sum of the domains drifts from the
process RSS by more than 10%.

### Put a limit on everything

The rule is that every queue, ring, pool, batch, reply and retry has a bound,
and every bound has a defined behaviour when it is crossed: a typed refusal,
pacing, or a continuation. Pressure is pushed back to the client over TCP, by
disarming the connection's receive, instead of being absorbed by a queue that
grows. The main bounds, and the known gaps, are listed in
[Limits and backpressure](#limits-and-backpressure).

### Seams, not forks

The kernel is runtime, fabric, wire, log and store, built on the shared
foundation, simd and alloc crates; `inf-server` assembles them into the cell
plane. Engines attach through versioned seams: the command table, record
type tags, log record types, and index projections. The document engine is
the first engine and the exception today: the store links it directly,
behind the store's own `doc` feature, so that records can hold documents.
That feature is switched on by `inf-server`'s `doc` feature, on by default.
Building the `inf-server` library with `--no-default-features` gives a slim
build with no document or path code at all (CI checks its symbols); the
`JSON.*` rows stay in the command table and answer unknown-command. The
`infinityd` binary has no such switch yet and always includes the document
engine. Crate
dependencies are a checked table (`docs/dep-dag.toml`), and CI rejects an
edge that is not in it.

### Compatibility is staged and honest

Redis compatibility is how people adopt InfinityDB, not the whole of what it
is. Compatibility is declared per command in
[compat-matrix.md](compat-matrix.md), which is generated from the command
table and a test corpus diffed byte for byte against a real `redis-server`
(8.0.5) and, for `JSON.*`, a pinned RedisJSON. The matrix records the
differences the corpus finds and the ones a row's note declares; it does not
show that no other difference exists. CI fails if the matrix is stale or a
corpus reply differs from its oracle with no recorded deviation for the
case.

### Claims follow evidence

This document contains no performance numbers, on purpose. Performance
claims need a reproducible measurement on known hardware, with a control
run. Optimizations land with an A/B measurement or stay behind a flag; a
losing A/B is recorded and not merged. `inf-bench` runs the project's own
exit gates and `inf-compare` drives standard load generators against Redis,
Redis Stack, Dragonfly and InfinityDB on one machine.

### Why Rust

The data plane has latency goals at high percentiles, so no garbage
collector. It needs precise control over layout and allocation, and
zero-cost abstraction over the injected effects. Beyond that, the type system
carries a lot of the design: `!Send` futures say "this never leaves its core"
in the signature, lifetimes catch what may not be held across a suspension,
and newtypes and enums make invalid states hard to write. `unsafe` is
confined to a few audited modules (see [Safety](#safety)).

## Inside a cell

### The loop

Every cell runs the same loop forever. One iteration:

```
 SUBMIT+REAP  one io_uring_enter: submit every op queued last iteration,
              harvest completions (poll while busy, park when idle)
 DISPATCH     route each completion; fire due timers
 FABRIC-IN    drain messages from other cells (bounded); publish the
              replies the drain produced at once
 PARSE+EXEC   parse received bytes; run local commands now; queue the
              rest on their connection's pump; resume woken futures
              (bounded)
 MAINTAIN     budgeted background slices: expiry, eviction, checkpoints,
              tiering, recovery, housekeeping
 LOG          seal the log records staged since the last seal into at
              most one frame and queue its write (or hold the frame
              open for a later iteration)
 RESPOND      queue a send for each connection with pending output
 FABRIC-OUT   publish staged fabric messages, ring doorbells
 IDLE         keep spinning, or park until I/O or a doorbell
```

The network and storage operations these steps produce (accept, receive,
send, close, log frame writes, segment zero-fill, checkpoint writes, tier
flush writes, fdatasync, tier reads) are not issued where they are produced.
They are pushed onto a queue and ride the single submit at the top of the
next iteration.

Some file work does not go through the ring. Creating and renaming files,
small metadata writes such as a new MANIFEST, and the data writes of a large
value's blob extent are synchronous calls through the injected filesystem
seam; they happen in MAINTAIN, or, for blob extents, on the command path,
bounded by the value's size. Their fsyncs still ride the ring. Deleting
truncated log segments and stale checkpoint files is handed to the control
thread through its bounded queue (a full queue keeps the path and retries on
the next slice). Tier files and blob files are unlinked inline, in bounded
MAINTAIN slices. Boot recovery reads the checkpoint and the log with
blocking I/O, in budgeted steps, while the node still answers `-LOADING`.

**Idle.** After work stops, the loop spins for 64 iterations, then parks in
the kernel with a timeout (500 µs when the node has more than one cell, 5 ms
with one). Parking must not lose a wakeup from another cell, so there is a
handshake. The parking cell sets its park flag, issues a full fence, and
checks its doorbells again; if one is set, it does not park. A cell
publishing to a peer sets the doorbell, fences, and writes the peer's
`eventfd` only if the peer's park flag is set. A cell that is still
recovering never parks.

**Budgets.** The loop has three scheduling groups with a deficit scheduler
weighted 8 : 1 : 1: foreground (commands), maintenance (expiry, eviction,
housekeeping) and checkpoint (so a long checkpoint cannot starve expiry, nor
the reverse). Foreground work is charged but not gated: what one iteration
can parse is bounded by the receive buffers delivered to it. Maintenance and
checkpoint slices are gated by their deficits. At most 1,024 resumed futures
run per iteration; the rest wait for the next one. A second budget, for the
storage device, is described in
[Background work](#background-work-maintain).

### The backend: io_uring, kqueue, and the simulator

The loop talks to the operating system through one trait, `BackendDriver`.
Pushing an operation never makes a system call. `submit_and_reap` is called
once per iteration and returns completions. The operation set is small:
arm accept, arm or disarm receive, send, close, log write (with a durability
barrier), fdatasync, and tier read. The contract is completion-shaped even on
a readiness-based backend. Only `inf-runtime` names io_uring or kqueue.

**io_uring (Linux, the production backend).** The ring is created with
`SINGLE_ISSUER` and `DEFER_TASKRUN`, falling back one flag at a time on
kernels that reject them. On kernels that support them (6.0 and later) it
uses multishot accept and multishot receive over a kernel-provided buffer
group. On older 5.15-class kernels it runs a degraded mode with one-shot
operations and the same observable behaviour. At most half of the cell's
buffer pool is handed to the kernel for receives, so the send path can always
get a buffer. No operation uses registered (fixed) buffers: at boot the
receive pool is registered once, only to probe the capability; network I/O
uses plain sends and receives, and cold-tier reads are plain positional
reads. Accepted sockets get `TCP_NODELAY`.

**kqueue (macOS, development only).** A readiness-to-completion adapter so
the whole stack builds and tests on a laptop. It is never used for
performance numbers. Because macOS does not spread connections across a
`SO_REUSEPORT` group, macOS nodes hand accepted sockets to other cells over
the fabric (`--accept-handoff`, on by default there).

**The simulator's driver.** `inf-sim` provides a third implementation with
in-memory sockets, seeded fault injection and a virtual clock, and drives the
real cell code through it.

### Buffers

Each cell has a fixed pool of network buffers (`--buffers` 4096 of
`--buf-size` 4096 bytes by default, so 16 MiB per cell). The pool never
grows. Running out is backpressure: the kernel reports that it had data but
no buffer, receiving pauses, and it resumes as buffers come back.

A received buffer is parsed in place. A request that lies entirely inside one
buffer is parsed with no copy; a request that spans buffers is copied into
the connection's accumulator. A small local command (at most 16 arguments
and 512 bytes of arguments) is then copied once, flat, into the cell's stage
buffer so it can run in a prefetched batch (step 4 of the walkthrough
below); a larger one runs inline from the parsed slices. A command handed to
the connection's pump is copied into an owned command that the pump keeps
while it waits. Replies are written into the connection's output buffer
and, at RESPOND, copied into one pool buffer and sent. A connection has at
most one send in flight, so a large reply drains over several iterations.

## The life of a request

### A GET on the connection's own cell

1. The previous iteration's `io_uring_enter` harvests a receive completion
   carrying the client's bytes in a pool buffer.
2. PARSE feeds the buffer to the connection's parser, which yields a
   request's arguments as slices borrowed from the buffer.
3. The command name is looked up in the command table (a compile-time
   perfect hash). Its metadata says where the keys are, so routing computes
   the key's slot and owner. The owner is this cell, so the command runs now.
4. For an ordinary command like this one, the cell copies its arguments
   flat into a small per-cell stage buffer, hashes the key and prefetches
   the index lines it will probe, then moves on to the next request in the
   buffer. At the end of the buffer the whole staged batch executes in
   order, with its index lines already on their way into cache. Only a
   command of at most 16 arguments and 512 bytes of arguments is staged. A
   larger one, `HELLO`, `SELECT`, `INF.NS`, `QUIT`, `DEBUG`, and any
   command the table does not know are barriers: the staged batch executes
   first, then the command runs inline. `--no-parse-batch-prefetch` turns
   staging off.
5. `GET` probes the index, compares the full key in the record, and writes
   the RESP reply into the connection's output.
6. RESPOND copies the output into a pool buffer and queues a send. The next
   iteration's single submit sends it.

No future is created and no task is scheduled.

### A GET for a key on another cell

```
 cell A (holds the connection)                cell B (owns the slot)
 ─────────────────────────────                ──────────────────────
 PARSE      slot is B's: hand the command
            to the connection's pump
            pump: encode Apply{token, argv},
            take a credit, wait on gate[token]
 FABRIC-OUT publish, ring B's doorbell ──────▶ FABRIC-IN  drain, prefetch,
            (wake B if it is parked)                      execute GET,
                                                          reply{token, bytes},
                                               publish at once
 FABRIC-IN  reply: return the credit,  ◀──────
            complete gate[token]
 PARSE+EXEC pump resumes, appends the
            reply bytes to the output
 RESPOND    send
```

The owner cell executes the command exactly as if a local client had sent it
and returns raw RESP bytes, so the reply is identical either way. The owner
publishes replies during FABRIC-IN instead of waiting for its own FABRIC-OUT
step, which shortens the round trip. The origin cell resumes the pump in the
same iteration that received the reply.

### Pipelines and the pump

Clients pipeline, and replies must come back in request order. Each
connection has at most one pump: a future that takes the connection's
deferred commands in order, dispatches them, and emits their replies in
order. Once a connection has deferred a command, the commands behind it in
the same stream are deferred too, so a local fast-path reply can never
overtake an earlier remote one.

The pump dispatches ahead of its replies within a window: at most 32 remote
operations in flight and 256 replies pending per connection. If a connection
has 1,024 commands queued, the cell disarms its receive, so TCP flow control
pushes back on the client; receiving resumes when the queue drains to 64.

Besides remote keys, a few kinds of command always use the pump even when
every key is local: commands on a named namespace (their acknowledgments may
wait on durability and their admission may pause), pub/sub, namespace
changes, checkpoint requests, and, on a node with more than one cell,
whole-keyspace commands such as `SCAN` and `DBSIZE`.

### A durable SET

Take `SET k v` on a connection bound to a namespace created with
`INF.NS CREATE orders MODE durable FSYNC always`, with `k` owned by the same
cell.

```
 iteration n    PARSE+EXEC  pump: admission (admit / wait for log space /
                            refuse) → execute: the in-memory store is updated
                            → stage the log record in the staging ring
                            → reply held on the ack gate for its sequence
                LOG         seal the staged records into one frame and
                            queue its write with a durability barrier
                            (a pacing or grouping hold may defer this to
                            a later iteration)
 next iter.     SUBMIT      one io_uring_enter carries the write and its
                            barrier (linked fdatasync, or an RWF_DSYNC write)
 later          REAP        barrier completes → the durable watermark
                            advances → the gate releases every waiting reply
                            it now covers → pump resumes → +OK is sent
```

With `FSYNC everysec` the reply does not wait for the gate: it is sent as
soon as the command has executed, and the log is synced by a one-second
timer. If `k` is owned by another cell, the command travels as a namespace
apply, the owner does the steps above, and the owner holds back its fabric
reply until its own durable watermark covers the write.

**Visibility and acknowledgment are different events.** The store is updated
when the command executes, before the log record is durable. The `always`
acknowledgment waits for durability, but another connection reading the same
key can see the new value before the writer is acknowledged. If the node
crashes before the barrier completes, that value may be lost; the writer was
never acknowledged, so the durability promise holds, but the reader saw a
value that did not survive.

## The fabric

The fabric is how cells cooperate without sharing memory.

**Rings.** Each directed cell pair has one single-producer, single-consumer
ring: a fixed power-of-two array of 64-byte slots (4,096 slots in
`infinityd`). Producer and consumer each own a free-running index on its own
cache line and cache the other side's index, so the common case touches only
the local line. A batch is published with one release store and consumed with
one release store. The ring is the only `unsafe` code in `inf-fabric`; it is
model-checked with Loom and tested under Miri.

**Messages and packing.** A message is an encoded operation with an 8-byte
header. Messages up to 62 bytes sit inline in a slot; larger ones spill to
the heap. Operations toward a destination are packed into one open buffer that
seals into a single slot at 2 KiB or 64 operations, so one slot carries many
operations and one heap allocation is shared across the pack. Requests are
published, and doorbells rung, at FABRIC-OUT; replies go out as soon as
FABRIC-IN has produced them.

**Operations.** The operations client traffic uses are `Apply` (run this
command on the owner), `ApplyNs` (the same for a named namespace), `Batch`
(one level of nesting), `Reply`, and `AdoptConn` (take over an accepted
socket). Every data operation carries a token (16 bits of origin cell, 48
bits of sequence) and is answered by exactly one `Reply`. The decoder is
total, bounded and fuzzed.

**Credits.** Each cell may have 1,024 unanswered data operations toward each
peer. Sending consumes a credit; draining the reply returns it. A sender out
of credits parks until a credit returns. Replies need no credit, and each
ring holds at least twice the credit count, so a reply can always be sent and
draining never waits for space: there is no cycle in which two cells wait for
each other.

**Draining.** FABRIC-IN drains about 1,024 messages per iteration,
round-robin over peers from a rotating starting point. Slots are consumed
in chunks of 8 and the budget is checked between chunks, so a drain can
overshoot it by one chunk (8 slots of up to 64 messages each). The mesh's
drain function never blocks and never sends; that unconditional progress is
half of the deadlock-freedom argument. FABRIC-IN then publishes the replies
it produced as soon as the drain ends.

## The wire and the command table

**Parsing.** The parser accepts RESP arrays and inline commands. It is a
lending iterator: a parsed request borrows from the buffer and cannot outlive
the parse step. Integer lengths are parsed with SWAR (several digits per
machine word) and CRLF is checked where the length says it must be; a SIMD
CRLF search is used only for inline commands. Limits are enforced from the
length line, before any bytes are buffered: a bulk string may be up to
`proto-max-bulk-len` (16 MiB by default), a whole request up to that plus
64 KiB, and a request may have at most 1,024 arguments. A protocol error gets
`-ERR Protocol error: ...` and the connection is closed. `HELLO` switches a
connection between RESP2 and RESP3.

**The command table.** The whole wire surface is one static table of 91
rows. Each row carries the command's name, arity, flags (`READONLY`,
`WRITE`, `ADMIN`, `FAST`, `DENYOOM`, `LOADING`, `INTERNAL`) and a key
specification in Redis's first/last/step form. Lookup is a perfect hash
computed at compile time: fold the name into two words, two multiplies, one
probe, one compare. A collision in the hash fails the build.

The table is the single source of truth: routing reads the key specification
to find the owner, the executor reads the flags, `COMMAND` renders the table,
and the compatibility matrix is generated from it. There is no second
dispatcher and no hand-kept list of "commands that do X".

Execution follows one order for every command: look up, reject internal
commands from clients, check arity, check the namespace is available, apply
the out-of-memory check to `DENYOOM` commands, apply the RESP2 subscriber
restriction, then execute against the selected store. Writes then refresh
the cell's memory-pressure flag.

## Storage inside a cell

The store (`inf-store`) never sees a socket or a RESP byte and never opens a
file. Time is passed in. It is a single-threaded library that one cell owns.

### Records

A record is a key and a value packed into the cell's arena behind an 8-byte
header:

```
 [0]      type:4 | flags:4     flags: TTL, RAW, 2-bit CLOCK reference
 [1]      key length: u8       keys up to 255 bytes
 [2..5]   value length: u24    values up to 16 MiB − 1 inline
 [5..8]   version: u24         per-key mutation counter
 [8..13]  expire_at_ms: u40    present only when the TTL flag is set
 [..]     key bytes, then value bytes, no padding
```

Record types are strings, JSON documents, and string extents (a 24-byte
reference to a large value stored out of line in a tiered namespace).

### The arena

Records live in a per-cell arena: size-class slabs carved out of anonymous
`mmap` chunks (2 MiB by default), with a free list per class; an allocation
larger than a quarter chunk gets its own mapping. An arena address packs
chunk and offset into 48 bits, exactly the width the index slot reserves.
The arena counts requested bytes, mapped bytes and the slack between them.
When a budget is exhausted, allocation returns "no memory" and the command
gets an out-of-memory error; nothing panics under memory pressure.

### The hash index

The index is an open-addressing table in the Swiss-table style. Each index
slot has a one-byte control entry holding 7 bits of the hash, and control
bytes are compared 16 at a time with SIMD. The index slot itself is 8 bytes:
a 48-bit record address, 15 more fingerprint bits and a used bit. The table
holds no keys and no values; a match is confirmed by comparing the full key
in the record, which the batch prefetch has usually already brought into
cache. The load factor, counting tombstones, stays at or below 85%.

Growth doubles the table and re-places every entry in one step, on the
foreground write path. That is a known pause, proportional to the table size,
and it is counted in `index_grows`. Incremental growth is not built.

The index hash is SipHash-1-3 keyed with a secret. The secret is generated
once per data directory, stored in `key-hash.toml` with mode `0600`, and its
identity is recorded in every MANIFEST, so a boot with the wrong secret is
refused before any data is read. The reason is hash flooding: with a public
hash function, a client could choose keys that all land in one probe chain.
Routing keys to cells uses a different hash, CRC16, so that slot assignment
matches Redis Cluster.

### Expiry

Expiry runs on a per-store hierarchical timing wheel: four tiers of 512
slots at 1 ms, 512 ms, about 4.4 minutes and about 37 hours per slot, which
covers about 2.2 years; later deadlines wait on an overflow list. A node is
16 bytes (the key's hash and its deadline) and holds no pointer to the
record. A membership table maps each key hash with a deadline to its one
node, so a key never holds more than one: a later deadline keeps the node
(it fires early and re-files itself), an earlier one moves it, and a cleared
deadline or a death removes it in constant time. Removal copies the next
node of the slot list into the removed one's place; a removed last node
stays linked as a tombstone until its slot drains, and tombstones are
bounded. Because a hash is not a key, a node is removed only after the store
has checked that no other record with the same hash still has a deadline,
using the same bounded enumeration the secondary indexes use; when that
enumeration cannot finish within its bounds, the node is removed and the
sweep below takes the records over.

When a node fires, the store enumerates the records sharing its hash, removes
every one whose deadline has passed, and re-files the node at the earliest
remaining deadline, strictly after the current millisecond. The schedule
changes only at two places: the record write, when the deadline changed, and
the record death. A command never touches it directly, and a rewrite that
keeps its deadline pays nothing.

A key the node budget refuses — the per-store bound is 2²⁴ − 1 nodes, the
width of the node link — is not left to lazy expiry. The expiry sweep owes it
a visit: MAINTAIN slices walk the index table in passes under a shared slot
budget, removing expired records and scheduling live ones. A pass that saw no
new refusal and no table rebuild while it ran proves every record with a
deadline has a node, and the sweep goes idle.

Active expiry runs in MAINTAIN slices under the maintenance budget, capped at
4,096 expirations per slice and rotated across databases so that one expiry
storm cannot starve the others. Expired keys are also removed lazily when
read. The log stores absolute Unix-millisecond deadlines, so a restart keeps
the right expiry times.

### Eviction and maxmemory

`maxmemory` is a node-wide budget, and each cell enforces its share
(`maxmemory / cells`); cells own equal slot ranges, so this needs no shared
state. The write path pays one branch on a cached "over limit" flag. When
over the limit, a write may evict up to 512 keys inline and then answers with
Redis's out-of-memory error; MAINTAIN then evicts down to a lower target
(`limit − limit/16`) in budgeted slices.

All eight Redis policies are supported. Recency uses CLOCK, with a 2-bit
reference counter in the record header: a lookup saturates it to 3, a write
sets it to 1, and the eviction hand decrements it, so a record at 0 is a
victim, at zero extra bytes per record. Reads earn more than writes, which
lets the sweep tell a read-hot set from write churn. Frequency
uses a 4 × 2,048 Count-Min Sketch of one-byte Morris counters (8 KiB per
store), allocated only while an LFU policy is active and halved periodically
to age. Its randomness comes from a seeded generator, so eviction is
deterministic in the simulator.

### Namespaces

A namespace binds semantics to a set of keys:

- **Numbered databases.** `SELECT 0` to `15` work as in Redis. They are always
  memory-only (even on a node with a data directory) and share the node's
  `maxmemory`.
- **Named namespaces.**
  `INF.NS CREATE name [MODE memory|durable] [FSYNC always|everysec]
  [EVICTION policy] [MAXMEMORY bytes] [MEM-BUDGET bytes ...]`.
  A memory namespace can have its own `MAXMEMORY` and eviction policy; it
  then evicts only its own keys and cannot disturb the others. A durable
  namespace is logged and never evicts (evicting without logging a delete
  would bring the key back on replay). A durable namespace with a
  `MEM-BUDGET` is tiered; see
  [Tiered storage](#tiered-storage-datasets-larger-than-ram).

A connection binds to a named namespace with `INF.NS USE`, or starts bound to
one if the node was started with `--conn-default-ns`. Namespace ids are
allocated once and never reused, because log records refer to namespaces by
id. On a node with a data directory, creating a namespace persists it first
and applies it second: the control thread writes the new catalog (write a
new file, fsync, rename, fsync the directory), and only then does every cell
learn about the namespace.

## Durability

Only named durable namespaces write to the log. Memory namespaces and the
numbered databases never reach it, so they pay nothing for durability.

### Records, frames and LSNs

A log record is a varint length, a type, flags, a namespace id and a payload.
Its types include string post-images (the full new value), deletes, absolute
expiry times, namespace operations, checkpoint-begin markers, document deltas
and full documents, and markers used by tiered namespaces. Strings are logged
as their new value, never as the operation, so replay is a blind upsert and
never depends on what was there before. Unknown record types and
non-canonical encodings are refused.

The log is per cell. An LSN is a (segment, offset) pair within one cell's
log; there is no global LSN, and each cell recovers independently.

The records staged since the last seal go into one frame, written with one
write. A loop iteration seals at most one frame; a frame may also be held
open across several iterations (see the holds under
[Two kinds of barrier](#two-kinds-of-barrier)), in which case it carries
records from all of them.

```
 header   magic · frame length · record count · first LSN
          · epoch (log life) · seq (frame number in this life)
          · covered LSN (durable watermark when the frame was sealed)
 body     records
 trailer  CRC32C over header and body
 padding  zeros up to the next 4 KiB boundary (O_DIRECT segments only)
```

The epoch, sequence number and covered LSN matter at recovery. They let the
reader tell the torn tail of a crash (never acknowledged, safe to cut) from
data the device lost after acknowledging it (a device fault, which stops the
node). More on this under [Recovery](#recovery).

### Staging and the frame pipeline

During EXECUTE, a durable write stages its record into a staging ring of
whole-frame buffers (4 MiB each by default, `--log-staging-mib`). The buffers
are allocated once and the append path allocates nothing. At LOG, unless a
hold applies, the current buffer seals into a frame and is leased to the
in-flight write until its completion returns.

Up to K frames may be in flight at once, so the ring has K + 1 buffers. K is
derived from the barrier class described below: 3 for FUA and 1 for FLUSH,
at most 8 (`--frames-in-flight` overrides it). Completions can arrive in any
order, so a frame counts as written only when every frame before it is
written too. At most 16 frames may wait for earlier ones in this way; when
that window is full, the next frame waits.

When staging is full, durable writes wait for space instead of failing
(mainstream Redis clients do not retry `-BUSY`). A write whose log record can
never fit one staging buffer, less the frame header and trailer, is refused
up front with the non-retryable `ERR write exceeds durable log staging
capacity`. Admission uses a conservative estimate of the record's size; a
JSON write is checked against its exact encoded size when it executes, and
gets `ERR document too large for durable log staging`.

### Group commit and the durable watermark

Each iteration writes at most one frame, and one barrier covers every durable
write that is due. The durable watermark advances only when a barrier
completes, and only over the longest prefix of the log in which every earlier
barrier has also completed; a later completion alone proves nothing about the
bytes before it.

- `FSYNC always`: the acknowledgment waits on the durable-watermark gate
  until the durable watermark covers the write's record.
- `FSYNC everysec`: the acknowledgment is sent as soon as the write executes;
  a one-second timer makes sure a barrier runs. As with Redis's
  `appendfsync everysec`, a power loss can lose up to about one second of
  acknowledged writes.

### Two kinds of barrier

Storage devices differ widely in what a durability barrier costs, so
InfinityDB supports two kinds and lets the device decide.

- **FLUSH class.** A buffered frame write, linked in io_uring (`IO_LINK`) to
  an `fdatasync`. On many devices this ends in a device-wide cache flush,
  which every cell sharing the device queues behind.
- **FUA class.** Segments are opened with `O_DIRECT`, frames are aligned to
  4 KiB, and each segment is zero-filled in MAINTAIN before it is used. A
  frame that carries a due barrier is then written with `RWF_DSYNC` and is
  durable when its own write completes, without waiting on the other cells.

A frame takes the FUA path only when all of these hold; otherwise it takes
the linked `fdatasync`, exactly as on the FLUSH class:

- the segment it lands in is `O_DIRECT` and already zero-filled;
- its padded length is at most 256 KiB by default (the device probe can
  set a different limit in `io-properties.toml`), because past some size a
  device's forced-unit-access write stops being cheaper than a flush;
- everything before it in the log is already durable or covered by an
  earlier pending barrier. A FUA write makes only its own bytes durable,
  while `fdatasync` covers the whole file, so a frame behind un-barriered
  bytes needs the `fdatasync` that covers the gap.

The first condition means a FUA node does not always run FUA-class. A fresh
cell starts on a segment that has not been zero-filled, writes FLUSH-class
frames, and moves to FUA when the log rotates onto the first zero-filled
segment (a class-upgrade rotation). And zero-fill is background work under
the device budget: if it falls behind, rotation takes the next segment
un-zeroed, and that segment is written FLUSH-class too. `INFO persistence`
shows both the class the node was configured for (`io_class_configured`)
and the class the cell is running now (`barrier_class`), with a count of
un-zeroed rotations (`rotations_unzeroed`).

On the first boot of a data directory, `infinityd` (with `--device-probe
auto`, the default) measures both barrier kinds on the directory's device
and writes the result, together with the device's read and write rates and
its identity, to `io-properties.toml`. A later boot on a different device
probes again. With `--device-probe off` and no file, the node uses the FLUSH
class and runs without a device model. `inf probe-device` runs the same probe
by hand.

Two further policies trade a very small delay for much bigger batches. On
an `O_DIRECT` segment, a frame with no barrier due may stay open for up to
1 ms or until it holds 16 KiB (`--fill-window-us`, `--fill-target-kib`);
this never delays an `always` acknowledgment. And on the FLUSH class, a
frame with a barrier due may wait up to 250 µs for the next round of writes
from the clients it just acknowledged (`--flush-group-window-us`), so one
barrier covers more of them.

These two are policy holds. A frame is also held for correctness: while a
due barrier cannot be issued behind frames still in flight, while the
reorder window is full, while a rotation waits for the old segment's writes
to finish, or while the next segment is not ready. An optional frame-seal
pacer (`--seal-pace`, off by default) can also hold a frame queued behind
frames in flight for at most one barrier window at the device's measured
barrier rate. Each hold is counted, and every held frame keeps collecting
records until it seals.

### Segments

The log is a sequence of segment files (`seg-NNNNNN.ilog`, 256 MiB by
default). The next segment is created and preallocated, and for the FUA
class zero-filled, in MAINTAIN slices well before the current one fills, so
rotation on the write path is a pointer swap. A pre-zeroed segment below the
checkpoint floor may be recycled as the next segment by renaming it instead
of deleting it (one pooled segment per cell by default), which saves writing
the zeros again. Every frame records the segment and offset it was written
for, so recovery recognizes frames left over from a recycled file's previous
life and never mistakes them for data.

Running out of space is handled in two ways, and an operator should plan
for the second. If creating or preallocating the next segment fails with
`ENOSPC`, the cell refuses new durable writes with a typed error
(`ERR durable write refused: log storage exhausted (NOSPACE)`) while memory
namespaces keep working. But preallocation only sets the file's length; it
does not reserve blocks, so on a real filesystem that early warning is
best-effort. A full device usually shows up later, as a failed frame write
or zero-fill write, and a failed log write stops the node like any other.

An `fsync` failure also stops the node: after a failed `fsync` the kernel's
page cache can no longer be trusted to say what reached the disk. This
applies to every barrier that protects acknowledged data (log frames,
zero-fill, tier files, blob extents), no caller may catch that error and
continue, and a script in CI checks it. Two operations are different
because they have a safe fallback: a failed checkpoint write or sync
abandons that checkpoint, and a failed MANIFEST swap keeps the previous
recovery unit and retries after a backoff. In both cases the old checkpoint
and the log are still valid.

### Checkpoints

Without checkpoints, recovery would replay the whole log. A checkpoint lets
recovery load a snapshot and replay only the log after it.

A checkpoint is fuzzy and taken by the owning cell, in MAINTAIN slices under
the checkpoint budget. There is no `fork()` and no stop-the-world pause.
First the cell stages a checkpoint-begin marker in the log; the marker's LSN
is where replay will start. Then the cell walks its stores, writing records
into CRC-protected sections of a `ckpt-NNNNNN.ick.new` file with ordinary log
record encodings. A checkpoint is really a prefix of the log written out as
state, and loading it uses the same upsert as replay. Records that change
during the walk are also in the log after the begin marker, so replaying from
the marker fixes them. A footer carries per-namespace counts and a digest
chained over every section's CRC. When the file is synced it is renamed to
`ckpt-NNNNNN.ick` and the directory is synced.

The next checkpoint starts once the log has grown to about twice the size of
the last checkpoint (with a floor), capped so that replaying the log stays
inside a fixed recovery-time budget. Checkpoint writes are paced by the
device budget, or at a fixed rate when there is no device model. An
I/O error during a checkpoint abandons that checkpoint, not the process: the
previous checkpoint and the log are still valid. `INF.CKPT [CELL k] [WAIT]`
and `BGSAVE` request one.

### MANIFEST and truncation

Each cell has a `MANIFEST` naming its current recovery unit: the checkpoint,
its begin LSN, the live segments, the key-hash identity and, for tiered
namespaces, the tier files and how far each is durable. It is replaced
atomically: write a new file, fsync it, rename it over the old one, fsync the
directory. A reader sees the old unit or the new one, never a mix. The
checkpoint's begin LSN is the truncation floor: segments below it are fully
covered and can be deleted or recycled.

### Recovery

Each cell recovers its own log, in parallel with the other cells. Recovery is
a resumable state machine stepped in MAINTAIN slices. The `-LOADING` state
is node-wide: until every cell has finished, every cell answers `-LOADING` to
commands not allowed during loading, including a cell whose own recovery is
already done.

1. Read the MANIFEST. Without one (a fresh cell), replay the whole log. With
   one, the named checkpoint must load and the floor segment must exist;
   anything less stops the boot, never a silent full replay.
2. Load the checkpoint, verifying every section CRC before applying it and
   the footer's digest and counts at the end.
3. Replay the log from the begin LSN, frame by frame. Each frame is validated
   (magic, length, CRC, recorded position, epoch and sequence continuity)
   before any of its records is applied.
4. After each segment, scan the bytes past its last valid frame. A valid frame
   beyond a gap means a write was lost below later data. If a later frame's
   covered LSN says the lost range had been acknowledged as durable, the
   device lost acknowledged data and the boot stops. Otherwise the gap is an
   unacknowledged torn tail: the cell continues from the gap, and existing
   bytes are never rewritten.
5. Remove leftovers of interrupted operations (segments below the floor,
   unnamed checkpoint files) and reopen the log under a new epoch, so bytes
   from the previous life can never be mistaken for new frames.

### Stopping

`SIGTERM` or `SIGINT` starts a graceful stop. Every cell stops accepting,
answers every command it has already read, and closes its connections. Once
every cell is quiet, each durable cell writes a stop checkpoint
(`--shutdown-checkpoint`, on by default) and a final sync, and the process
exits 0 when all cells have drained. Every acknowledged write of every class
is then in the image the next boot recovers, and that boot replays nothing.
If a second signal arrives, or the stop takes longer than
`--shutdown-timeout-ms` (10 s by default; the process exits with code 1 and
names the stuck phase), the next boot falls back to crash recovery. There is
no `SHUTDOWN` command.

## Tiered storage: datasets larger than RAM

A durable namespace created with a `MEM-BUDGET` is tiered: its records may
live on disk, and only the recently written part and a working set stay in
RAM. The design follows the hybrid log of Microsoft's FASTER.

### The hybrid log

Each (cell, tiered namespace) pair has a logical address space. Records are
appended at monotonically increasing 48-bit addresses, and four watermarks
(positions in this address space, separate from the log's durable
watermark) divide it:

```
  ◀─ older                                                       newer ─▶
  ┌──────────────┬────────────────────────┬───────────────┬────────────────┐
  │ on disk only │ in RAM and on disk     │ in RAM only,  │ in RAM,        │
  │ (cold)       │ (flushed, read-only)   │ read-only,    │ mutable        │
  │              │                        │ not flushed   │                │
  └──────────────┴────────────────────────┴───────────────┴────────────────┘
                 ▲ head                   ▲ flushed       ▲ ro_boundary    ▲ tail
```

Records in the mutable region are updated in place. A record below the
read-only boundary is never changed; updating it writes a new copy at the
tail. That rule is enforced by the API (a mutable view below the boundary is
refused) because an in-place write there would silently diverge from a copy
already on disk. RAM pages are released only below the flushed watermark,
also enforced by the API. The RAM part is a fixed ring of reserved virtual
memory whose pages are committed and released as the watermarks move.

The index has the same shape as the memory index; its 48-bit field holds a
logical address instead of an arena address. It also keeps each entry's full
64-bit hash beside the table, so growing the table never has to read a cold
record from disk.

### Pressure: demote, don't evict

A cache namespace answers memory pressure by evicting. A tiered namespace
answers by demoting, in three budgeted MAINTAIN steps: seal the mutable
region down to its target share of the budget (25% by default), flush sealed
bytes to tier files, and release RAM pages below the flushed watermark.

Tier files hold one contiguous range of addresses each: a 4 KiB header, then
4 KiB tier frames (4,092 data bytes and a CRC32C each; these are unrelated to
log frames), and a 4 KiB footer once sealed. Finding a byte is arithmetic,
with no per-record directory. Flush writes go through the loop's driver, and
the flushed watermark advances only when the round's final barrier
completes.

### Cold reads

When a lookup lands on a cold address, the store does no I/O. It returns the
address as a candidate and the command suspends:

1. The read joins a bounded per-class queue (foreground or maintenance).
2. Once per iteration the queues drain into `O_DIRECT` reads from an aligned
   buffer pool (plain positional reads; the pool is not registered with
   io_uring), up to a per-cell queue-depth cap, 3 : 1 in favour of
   foreground, merging adjacent ranges of the same file.
3. When the read completes, the command resumes, compares the full key, and
   looks the key up again through the index, because anything may have
   changed while it waited. The fingerprint can match the wrong key with
   probability about 2⁻²²; then the command retries with that address
   excluded.

If the queue is full, the command gets `BUSY cold-read queue saturated`. A
disk error on a cold read gets a typed error reply.

A verified cold read may promote the record: its image is appended at the
tail, so the next read is served from RAM. A small direct-mapped filter
promotes only on a second read soon after the first, so a key touched once
by a sweep stays cold while a key read again comes back into RAM. Promotion
is best-effort; when it cannot happen it is skipped and counted, never
waited for.

### Compaction and large values

Overwrites and deletes leave dead bytes in tier files. Each file keeps exact
live and dead byte counts (no scanning). When a file is at least half dead,
compaction copies its live records forward to the tail, a slice at a time,
repoints the index, and retires the file after the next MANIFEST swap and
once no read still holds it. Compaction only ever allocates space; it never
waits for space, so it cannot deadlock the flush it is part of.

Values at or above the namespace's `BLOB-THRESHOLD` are written out of line to
blob extent files, synchronously on the command path, and the record holds a
24-byte reference. An extent is durable before the record that points to it
can be acknowledged, and it is reclaimed only after the record that killed it
is durable and no reader holds it.

Tier flush and compaction spend the cell's device budget, described
under [Background work](#background-work-maintain).

### Recovery and limits

Tiered recovery does not scan the cold data. The MANIFEST names the tier files
and how far each is durable; unsealed files are cut back to that length. The
checkpoint holds address references for records already in tier files and
full images for the rest. The log tail then replays on top, and records whose
older copy was displaced are handled by a log marker, without reading the old
copy. So the tiered index is rebuilt from the checkpoint, not by reading cold
data.

Today tiered namespaces support the string family only. They have no key
expiry (the `EXPIRE` family and `SET` expiry options are refused, and `TTL`
answers -1), no JSON documents and no secondary indexes. Their errors are
typed: `DISKFULL` when the namespace's disk budget or the device is full,
`STALLED` when a write waited longer than `TAIL-STALL-TIMEOUT` for flush
progress, and `BUSY` when the cold-read queue is full.

## JSON documents

Documents are ordinary records with type JSON. They are durable, evictable
and versioned like any other value.

**The format.** A document is stored as an `idoc` tape: a compact binary
encoding with one-byte tags, small integers and short strings inline, 64-bit
integers and doubles, and objects and arrays with a 24-bit length, so any
subtree can be skipped in O(1). Each value has exactly one encoding and the
validator rejects any other, so two equal documents are byte-identical,
which lets replay and tests compare raw bytes. A document of up to 512 bytes
is stored inside its record; a larger one lives in a per-cell document arena
and the record points to it. Nesting depth is capped at 128 containers, and
a document's body at 16,777,192 bytes: the 16 MiB − 1 record value less the
document's value prefix and header, so every stored document fits one record
and one full-image log record. A write whose result would cross either bound
is refused before anything changes.

**Parsing.** The JSON parser uses simdjson's approach: SIMD classification of
64-byte blocks, then a streaming pass that writes the canonical tape directly,
with no intermediate tree.

**Paths.** A JSONPath expression (the subset in
[jsonpath-subset.md](jsonpath-subset.md)) compiles to a small bytecode
program, cached per cell in a bounded LRU. The compiled bytes are the
program, so the same bytes can be cached and written to the log.

**Mutations.** A mutation is planned against the whole match set first: every
edit is validated (types, integer range, the size of the result) before any
output exists. Then a single pass writes the new tape. There is no rollback
path because nothing partial is ever built. The mutation is a pure function
of (document, program, operation), which is what makes replaying it safe.

**Logging.** A document write is logged as a small delta (the path program
and the operation) instead of the whole document. After 64 deltas, or once
the deltas since the last full image add up to the document's own size, the
next write logs a full image instead, which bounds how much replay has to
apply to rebuild any document. A lineage number ties each delta to one
incarnation of the key, so a delta can never apply to a document that was
deleted and recreated in between.

**Replies.** Reply size is not bounded by document size (`$..*`, repeated
paths and formatting options can multiply it), so serialization runs against
an explicit reply budget (128 MiB by default) and answers `ERR reply too
large` beyond it.

The `JSON.*` family has 22 commands, checked against RedisJSON by the
compatibility oracle.

## Secondary indexes and queries

The index and query engine is **built and tested as a library but not yet
reachable from the wire**: there is no index DDL or query command in the
command table yet, and the server does not link the query crate. What exists:

- **Ordered maps.** Each secondary index is a per-cell B+-tree of
  (typed key, document reference) pairs in `memcmp` order: 8-byte numeric
  keys stored as flat arrays, or variable-length keys up to 1 KiB, with SIMD
  leaf search. Cursors never pin a node; they remember their last pair and
  seek again when the tree has changed.
- **Typed keys.** UTF-8 strings, 64-bit integers, doubles and booleans encode
  so that byte order equals value order. One coercion table decides what a
  document value becomes as an index key, and both index maintenance and
  query evaluation use it.
- **Maintenance in the owning cell.** A write to an indexed namespace
  evaluates the index paths on the old document, applies the mutation, then
  on the new document, and applies the difference to the trees, all inside
  the same step on the same thread, so no observer sees the document and its
  index disagree.
- **Backfill and persistence.** A new index is filled by a budgeted MAINTAIN
  walk. Converged trees are saved inside checkpoints; at boot an index is
  either loaded from the checkpoint or rebuilt, and a damaged saved index can
  never stop a boot.
- **Queries.** A predicate VM evaluates filter programs over documents: an
  iterative bytecode interpreter with a fuel limit that allocates nothing. The
  PartiQL subset compiler turns a statement into exactly one access step (a
  primary-key get, one index range, or a scan), an optional filter, and a page
  bound. There is no cost model and no plan search; the type has room for
  only one access step. The grammar and every rejection message are in
  [partiql-subset.md](partiql-subset.md).

## Commands that span cells

Commands whose keys live on several cells run as small programs on the pump:

- `DEL`, `UNLINK`, `EXISTS` and `TOUCH` count local keys directly and send one
  operation per remote key, then sum the results. If any leg fails, the reply
  is that error, never a partial sum.
- `MGET` and `JSON.MGET` gather values from each owner in key order.
- `MSET` applies its local pairs, then sends one leg per remote pair and
  replies `OK` when all legs succeed. `MSETNX` across cells checks, then sets.
- `RENAME` and `RENAMENX` across two owners read a snapshot of the source,
  put it on the target, and then remove the source only if it is unchanged.
  An interruption can leave both copies, never neither.
- `COPY` across two owners reads a snapshot of the source and puts it on the
  target; the source is never touched. (`COPY` within a named namespace is
  refused.)
- `DBSIZE`, `KEYS`, `SCAN`, `RANDOMKEY`, `FLUSHDB`, `FLUSHALL` and
  `CONFIG SET` go to every cell. A `SCAN` cursor carries the cell number in its
  top 16 bits and a per-cell cursor below it; each cell's walk returns every
  key present for the whole scan at least once, even while the table grows.

Multi-key writes that span cells are not atomic across cells today: another
client can observe some legs applied and others not. On a named namespace, a
command whose keys live on more than one cell is refused, except
`JSON.MGET`.

## Pub/sub

A channel is owned by the cell that owns `slot(channel)`. Subscriber state
lives on the subscriber's own cell. When a cell gains its first subscriber to
a channel, or loses its last, it tells the channel's owner. `PUBLISH` goes to
the owner, which sends one fabric message per cell that has subscribers, not
one per subscriber. Pattern subscriptions are kept on the subscriber's cell
too, and every cell holds a copy of the pattern index: when a cell gains its
first subscriber to a pattern, or loses its last, it tells every cell. So any
owner can find the cells with matching pattern subscribers without a global
table. A `SUBSCRIBE` is confirmed only after these notifications have been
acknowledged, so once a client sees its confirmation, a `PUBLISH` from any
cell reaches it. Delivery appends complete RESP messages directly to the
subscriber's output, and `client-output-buffer-limit` disconnects
subscribers that fall too far behind. The registries are `BTreeMap`s, so
delivery order never depends on a hash seed.

## Background work: MAINTAIN

Background work runs in MAINTAIN, in this order each iteration, and each
part is bounded:

1. Graceful-stop progress.
2. Recovery steps, while the cell is booting.
3. Memory statistics publication.
4. Active expiry, under the maintenance budget.
5. Secondary-index backfill, capped per slice by documents and by steps.
6. Pending `CLIENT KILL` and configuration changes (`CONFIG SET` fans a new
   configuration to every cell, and each applies it here).
7. Eviction, under the maintenance budget.
8. Durable-log upkeep: segment preallocation and zero-fill, the checkpoint
   slice under the checkpoint budget, MANIFEST and truncation.
9. Tiered upkeep: demotion, flush, page release, compaction, extent
   reclamation, retired-file cleanup.
10. Wakeups from the control thread (catalog persisted, checkpoint published).
11. Connection sweep: idle `timeout`, output-buffer limits.

Two budgets govern this work. The CPU budget is the deficit scheduler from
[the loop](#the-loop). The device budget is each cell's static share of the
device model measured at boot. Foreground classes (log frames, blob writes,
foreground cold reads) are metered but never deferred. Background classes
(zero-fill, tier flush, checkpoint, maintenance reads) hold byte and
operation credit refilled from the injected clock, each capped at about
50 ms of its share, with what a full class cannot hold kept in a shared pool. A
background producer offers its next block and gets one of two answers: issue
it now, or "not this slice" — keep it and offer it again next time. Nothing
queues or waits on the budget. A block larger than its class's cap could
never be covered by waiting, so once the class has rested a whole refill at
its cap the budget issues it anyway and the class owes the excess, repaid
before its credit grows again; a class with one producer on the cell
therefore always progresses, within a bound its share sets. A tier round
offers only when it has something to flush. A checkpoint floor keeps
checkpoints from starving under sustained writes, which would otherwise make
recovery grow without bound. Zero-fill has no floor: when it falls behind,
the cost is a segment written FLUSH-class, not a correctness problem. With
no device model (the probe was off and there is no `io-properties.toml`),
background I/O is not budgeted and checkpoints are paced at a fixed rate.

## Limits and backpressure

| Bound | Default | When crossed |
|---|---|---|
| Connections per cell | `maxclients / cells` (10,000 node-wide) | `-ERR max number of clients reached`, close |
| Accept file descriptors | the process limit | accept pauses until a close frees a descriptor, retry timer 100 ms |
| Network buffers per cell | 4,096 × 4 KiB | receive pauses until buffers return |
| Bulk string / request / arguments | 16 MiB / 16 MiB + 64 KiB / 1,024 | protocol error, connection closed |
| Key / inline value | 255 B / 16 MiB − 1 | typed error |
| Queued commands per connection | 1,024 (resume at 64) | stop receiving; TCP pushes back |
| Remote ops / pending replies per connection | 32 / 256 | pump stops dispatching until replies arrive |
| Fabric credits per destination | 1,024 | sender waits for a credit |
| Fabric drain per iteration | about 1,024 messages (checked between 8-slot chunks, so it can overshoot by one chunk) | the rest next iteration |
| Resumed futures per iteration | 1,024 | the rest next iteration |
| Expirations per slice | 4,096 | the rest next slice |
| Memory | `maxmemory` share; namespace `MAXMEMORY` | `DENYOOM` commands get `-OOM`, after at most 512 inline evictions |
| Log staging | (K + 1) × 4 MiB | durable writes wait; oversized records refused |
| Log space | the device | segment creation failing with `ENOSPC`: durable writes refused (`NOSPACE`), memory namespaces unaffected; a failed frame or zero-fill write (the usual case, since segments are sparse): the node stops |
| Output buffer | `client-output-buffer-limit`: normal clients unlimited (as in Redis); subscribers 32 MiB hard, or 8 MiB for 60 s | client disconnected |
| Cold-read queue | `COLD-READ-QD` (64) and overflow cap | `BUSY` |
| Document depth / body size / reply | 128 containers / 16,777,192 bytes / 128 MiB | typed error; nothing changes |

Two things are unbounded by default:

- **The executor's task slab.** It reserves 1,024 task slots but can grow
  beyond them. Reserved capacity for tasks and gates is designed and not yet
  built.
- **A normal client's output buffer.** As in Redis, the default
  `client-output-buffer-limit` puts no cap on clients that are not
  subscribed. A client that pipelines many requests and never reads its
  replies grows its output buffer until it reads or disconnects. Set a
  `normal` limit with `CONFIG SET client-output-buffer-limit` to bound it.

## Observability

`INFO` has `server`, `clients`, `memory`, `persistence`, `tiering`, `stats`,
`replication`, `cpu`, `tripwires`, `loophist` and `keyspace` sections.
Mind the scope: on a node with a data directory, `memory` and `keyspace` are
totals over every cell (without one they describe the serving cell), while
`persistence`, `tiering` and `tripwires` always describe the cell that
served the `INFO` command. Multiply before comparing those to the process as
a whole.

The tripwires are always on, and turning them off is only possible in an
A/B build that proves they cost nothing: SQEs per submit, CQEs per reap,
commands per iteration, fabric messages per batch, the loop iteration's
99.9th-percentile time, and the memory domains. `INFO loophist` returns a
per-cell histogram of iteration times. At boot the node prints the key-hash
source, topology, device properties, barrier class, frames in flight,
backend capabilities and listening port.

## Safety

**Unsafe code.** Crates default to `#![forbid(unsafe_code)]`. The exceptions
keep `#![deny(unsafe_code)]` at the crate root and allow it only in named
modules, each listed with its safety argument in the crate's `SAFETY.md`:
`inf-runtime` (the io_uring and kqueue backends; the driver, whose stable
byte views hand log, checkpoint, tier and cold-read buffers to the kernel
beyond the borrow that made them; the executor and its wakers; thread affinity;
signals; sockets; the cold-read buffers), `inf-fabric` (the ring only),
`inf-alloc` (arena, region, aligned buffers), `inf-simd` (CRC32C,
CRLF search, group probes, JSON classification, UTF-8), one module each in
`inf-doc` (tape emit), `inf-server` (log byte views) and `inf-probe`, and
two in the simulator. A script refuses a crate root without the attribute
and any `allow` that is not on a whole module. The ring is checked with
Loom; the allocator and fabric with Miri. The executor's wakers are
deliberately not thread-safe: they use no atomics (a CI check inspects the
waker path for atomic instructions), and command futures are `!Send`, which
keeps them on their cell's thread.

**Untrusted bytes.** Client requests, fabric messages and every on-disk
format (log frames, segments, checkpoints, MANIFEST, catalog, tier files,
blob extents, documents, path and query programs) go through a decoder that
is iterative (no recursion), bounded and total, and each has a fuzz target.
A value is checked once where it enters and becomes a type; inner code does
not check it again.

**Failure policy.** Protocol errors, admission pressure and recoverable I/O
errors return typed errors. An `fsync` failure on a barrier that protects
acknowledged data, a log write failure, or evidence that the device lost
acknowledged data stops the node. A failed checkpoint is abandoned, and a
failed MANIFEST swap keeps the previous recovery unit, without stopping the
node (see [Segments](#segments)).

**Network exposure.** `infinityd` listens on all IPv4 interfaces and has no
authentication and no TLS yet. Do not expose it to untrusted networks.

## How we know it works

### Deterministic simulation

`inf-sim` runs a whole node (every cell, the fabric, the wire, the store, the
log, recovery) on one thread. It uses the real server and loop code, not a
model of them, over:

- a simulated network with in-memory sockets that split received bytes at
  seeded random points, which exercises every parser resume path;
- a simulated disk that keeps, for every file, what the OS would show and
  what would survive a power cut. Unsynced data is lost on a cut, directory
  operations survive only in a seeded prefix, and pending writes are cut into
  sectors that each survive a coin flip and land in a seeded order, so writes
  tear and reorder;
- a virtual clock, seeded randomness, and a key-hash secret derived from the
  seed;
- named fault points in the storage code, compiled only into test and
  simulator builds (a check keeps them out of shipping builds).

Oracles watch every run: every key's replies replayed against a model store in
apply order; an independent model written from Redis's documented semantics;
pub/sub delivery (in order, no loss, no duplicates); memory accounting and key
contents at quiescence; no acknowledged `always` write lost across crashes;
after seeded power cuts, every live key of a tiered namespace serving its
exact bytes and every deleted key gone; document replay equivalence; index
backfill and saved-index equivalence. Oracles have canaries, planted bugs
compiled in only for the canary run that must turn them red
(`just sim-canaries`). `--verify-determinism` runs a seed twice and compares
the traces and a hash of the final state, disk included.

Every CI run executes every scenario once (`just sim-smoke`); sweeps run
thousands of seeds per scenario (`just durable-sweep` and friends); a nightly
fleet runs fresh seeds and files an issue with the scenario and seed of any
failure.

### Compatibility oracle

`just compat` diffs raw reply bytes, RESP2 and RESP3, against a real
`redis-server` 8.0.5 and a pinned Redis Stack for `JSON.*`, over a corpus of
commands and edge cases. It runs both the in-process executor and a spawned
four-cell durable `infinityd` over TCP. The compatibility matrix is generated
from the same run, and CI fails if the committed matrix is stale.

### Crash matrix

`tests/crash-matrix` kills the process at named fault points on the
durability path and checks that the point fired, that recovery reaches the
same state as a reference replay of the surviving log, that the recovery
outcome is the expected one, that no acknowledged `always` write was lost,
and that recovering twice gives the same result. Its rows are data files
anyone can review.

### Fuzzing, Loom and Miri

Every decoder has a cargo-fuzz target. CI runs a short fuzz of the RESP
parser on every change. A nightly job fuzzes the wire, fabric, log frame,
segment, checkpoint, MANIFEST, tier-file, blob-extent and document decoders
for hours each. The catalog, query-program, index-key and index-sidecar
targets, and a few server-side ones, exist but are not in the nightly set
yet. Loom
model-checks the fabric ring; Miri runs the allocator and fabric tests under
strict provenance.

### Mechanical checks

`just check` runs formatting, clippy with warnings as errors, and the tests,
plus scripts for the rules a compiler cannot see: the crate dependency table,
the cell denylist and clock ban, fault-point and `fsync` fail-stop rules,
the panic policy and the inventory of release assertions, `SAFETY.md`
inventories and unsafe crate roots, test-only features kept out of shipping
builds, file length and line width, and lint ratchets. Each script is
self-tested: a planted violation must make it fail. `just check` also
tests the slim `inf-server` library build, and CI builds it and checks that
its symbols contain no document or query code.

### Exit gates

`inf-bench gate-run` measures the project's exit gates and refuses to run
on an unsuitable machine (dirty tree, wrong CPU governor, thermal
throttling) unless told the result is not citable. `inf-compare` produces
comparison reports against Redis, Redis Stack and Dragonfly with standard
load generators. See [validation.md](validation.md).

## Crate map

| Crate | Role | Never sees |
|---|---|---|
| `inf-foundation` | ids, slots, CRC16 and hash tags, keyed hashing, clocks, randomness, histograms, fault points | anything above it |
| `inf-simd` | SIMD kernels: CRC32C, CRLF search, group probes, JSON classification | |
| `inf-alloc` | buffer pools, arenas, regions, aligned buffers, accounting | |
| `inf-runtime` | the loop, executor, gates, timers, scheduler, backends, device budget, cold reads | records, RESP |
| `inf-fabric` | SPSC rings, mesh, credits, doorbells, codec | records, storage |
| `inf-wire` | RESP parser and writer, the command table | records, storage, sockets |
| `inf-log` | log records, frames, staging, group commit, segments, checkpoints, MANIFEST, tier files, blobs | sockets, RESP, keyspace semantics |
| `inf-store` | records, index, TTL wheel, eviction, namespaces, tiering, ordered maps, index maintenance | sockets, RESP, files |
| `inf-doc` | the document format, parser, paths, mutations | RESP, records, logs, sockets |
| `inf-query` | predicate VM and PartiQL subset compiler (not linked by the server yet) | RESP, sockets, logs |
| `inf-server` | the cell plane: dispatch, pumps, cross-cell programs, pub/sub, durability and tier plumbing, INFO/CONFIG | |
| `inf-probe` | the boot-time device probe | |
| `infinityd` | the node binary | |
| `inf` | a small CLI, including `inf probe-device` | |
| `inf-sim` | the deterministic simulator | |
| `inf-bench`, `inf-compare` | exit-gate harness and comparison harness | |
| `inf-stream`, `inf-vector`, `inf-compute`, `inf-replica`, `infinity-embedded` | empty placeholders that reserve their dependency edges | |

## Not built yet

These parts are designed or planned but do not exist in the code today. The
order and grouping are in [roadmap.md](roadmap.md).

- **Index and query commands.** Index DDL, the query command, `EXPLAIN`, and
  JSONPath filter expressions (`?(...)`). The engine underneath is built.
- **Tiered namespaces beyond strings.** Expiry, JSON documents and secondary
  indexes on tiered namespaces.
- **Data types.** Hashes, lists, sets, sorted sets, bitmaps, HyperLogLog.
- **Transactions and scripting.** `MULTI`/`EXEC`/`WATCH`, Lua and `FUNCTION`,
  and atomic writes across cells.
- **Streams and queues.** `MODE topic` is reserved and refused today.
- **Vectors, in-database compute, embedded mode, replication and high
  availability.** Their crates exist only as empty placeholders.
- **Security.** A bind address option, authentication and ACLs, TLS.
- **Topology.** Clustering, resharding, and changing the cell count of an
  existing data directory.
- **Named-namespace gaps.** On a node with any durable namespace,
  `FLUSHALL` is refused. On a named namespace, `FLUSHDB` and `COPY` are
  refused, and so is a multi-key command whose keys live on more than one
  cell, except `JSON.MGET`.
- **Bounds still open.** A hard cap on executor tasks, a foreground budget
  that bounds the commands one iteration parses and runs inline, and
  incremental index growth to replace stop-and-copy.

## References

The work this design stands on:

- **TigerBeetle**: deterministic simulation, explicit limits, assertion
  discipline, and
  [TigerStyle](https://github.com/tigerbeetle/tigerbeetle/blob/main/docs/TIGER_STYLE.md),
  from which [INFINITY_STYLE.md](INFINITY_STYLE.md) descends.
- **FoundationDB**: deterministic simulation testing as the centre of
  correctness work
  ([talk](https://www.youtube.com/watch?v=OJb8A6h9jQQ)).
- **Seastar and ScyllaDB**: shard-per-core, scheduling groups, and the I/O
  scheduler that the device budget is modelled on.
- **Dragonfly**: shared-nothing Redis semantics at scale.
- **FASTER** (Chandramouli et al., SIGMOD 2018): the hybrid log behind tiered
  storage.
- **Redpanda**: thread-per-core storage for a log.
- **simdjson** (Langdale and Lemire): SIMD JSON parsing.
- **Swiss tables** (Abseil) and **hashbrown**: the index layout.
- **SipHash** (Aumasson and Bernstein): keyed hashing against hash flooding.
- **Count-Min Sketch** (Cormode and Muthukrishnan) and Morris counters: the
  LFU frequency estimate.
- Pillai et al., "All File Systems Are Not Created Equal" (OSDI 2014): the
  crash behaviour the simulated disk models.
- Rebello et al.,
  ["Can Applications Recover from fsync Failures?"](https://www.usenix.org/system/files/atc20-rebello.pdf)
  (USENIX ATC 2020): why an `fsync` failure stops the node.
- **Redis** and **RedisJSON**: the behaviour the compatibility oracle checks
  against.
