# inf-sim

The **deterministic simulator** for InfinityDB (milestone M0-S20, master plan
§17.1) — the project's main tool for finding concurrency and correctness bugs
and making them *reproducible*.

## What it is

`inf-sim` runs the **entire node** — N cells, the fabric, the wire protocol,
the store, and the command plane — single-threaded, with **time and entropy
injected**. It does not use a forked or mocked data plane: it drives the *real*
`ServerPlane` / `CellLoop` over a `SimDriver` whose "network" is in-memory
per-cell byte queues with seeded chunk boundaries (so the RESP parser's
resumability is exercised on every run).

The key property:

> **Same seed ⇒ byte-identical event trace.** Every failure is a single seed
> you can replay verbatim.

This turns the usual nightmare of "a flaky concurrency bug that reproduces once
a week" into "seed `0xC0FFEE` fails — here is the exact trace."

## How it works

```
   ┌──────────────────────────── inf-sim ────────────────────────────┐
   │                                                                  │
   │  seeded scheduler ── advances a VirtualClock in seeded steps     │
   │        │              and picks which cell/client runs next      │
   │        ▼                                                          │
   │  simulated clients ── issue a seeded SET/GET/EXPIRE/PUBLISH mix   │
   │        │              over in-memory byte queues (seeded splits)  │
   │        ▼                                                          │
   │  REAL node: N cells · fabric · wire · store · command plane      │
   │        │                                                          │
   │        ▼                                                          │
   │  oracles observe every apply point and the trace recorder logs   │
   │  (cell, origin, argv, reply) — the bytes that must be identical   │
   └──────────────────────────────────────────────────────────────────┘
```

Because the clock is virtual, the simulator compresses time: a scenario can
fire millions of TTL deadlines across "48 simulated hours" in seconds of wall
clock.

### Oracles (what it checks)

Every run arms these invariant checks:

- **Per-key linearizability.** Every apply point is replayed against a single
  in-memory model `Keyspace`, in apply order (a true total order on one
  thread); the model's reply must equal the observed reply byte-for-byte. TTL
  semantics ride the same replay (same injected `now`), so an early or ghost
  expiry diverges a later read.
- **Pub/Sub delivery.** Subscribers confirm before any publisher starts
  (confirmed ⇒ reachable); every `PUBLISH` reply must equal the planned
  receiver count; every delivered frame carries a per-`(channel, publisher)`
  sequence exactly one past the last (per-publisher FIFO — no loss, dup, or
  reorder); at quiescence each subscriber received exactly the published count.
- **Accounting reconciliation.** At quiescence both engines drain
  expired-but-unreaped entries at one instant, then the per-cell live-record
  sum must equal the model's, pub/sub registries must be empty, and no
  server-side connection may leak.

A **stall detector** fails the run if the scheduler makes no progress within a
window (the lost-wakeup tripwire).

## Usage

```bash
# from the repository root
cargo run --release --bin inf-sim -- --scenario m0-smoke --seed 0xC0FFEE

# verify determinism: run twice, byte-compare the traces
cargo run --release --bin inf-sim -- --scenario m1-cache --seed 0xCAFE --verify-determinism

# the just shortcut
just sim-smoke
```

### Flags

```
inf-sim --scenario m0-smoke|m1-cache [--seed N|0xN] [--verify-determinism]
        [--plant lost-wakeup] [--cells N] [--connections N] [--commands N]
        [--key-space N] [--trace-out FILE]
```

| Flag | Meaning |
|---|---|
| `--scenario` | `m0-smoke` (KV + cross-cell mix) or `m1-cache` (adds TTL traffic + cross-cell pub/sub fan-out). |
| `--seed` | Seed (decimal or `0x` hex). The seed *is* the reproduction. |
| `--verify-determinism` | Run the scenario twice and assert the traces are byte-identical. |
| `--plant lost-wakeup` | Inject a known bug to prove the oracle/stall detector catches it (a self-test). |
| `--cells` / `--connections` / `--commands` / `--key-space` | Override scenario size. |
| `--trace-out FILE` | Write the raw event trace to a file (for offline diffing). |

### Output

```
inf-sim: scenario m1-cache seed 0xcafe: 60000 commands, 57296 apply events,
         4801 steps, trace 2421040 bytes, hash 0xbf1d3c77cbc77158
inf-sim: sim_seconds=... published=... delivered=...
inf-sim: determinism verified — second run trace byte-identical
```

The process exits non-zero on any oracle violation, a stall, or a determinism
mismatch — and prints the first few violations.

## Scenarios

| Scenario | Shape |
|---|---|
| `m0-smoke` | 3 cells, 100 connections, 100k mixed commands incl. cross-cell. The original M0 acceptance scenario. |
| `m1-cache` | 3 cells, 80 connections, 60k commands, plus TTL traffic and cross-cell pub/sub (channel + pattern subscribers), with the delivery + accounting oracles armed. |

## The seed corpus & nightly fleet

`seeds/corpus.txt` is the **fossil record** of every bug the simulator has ever
caught. Format is `<scenario> <seed>` per line. The contract:

- The corpus replays green on every merge to `main` (the `corpus` test in
  `cargo test -p inf-sim`).
- The nightly fleet replays the corpus at full size, plus a batch of fresh
  run-id-derived seeds and seeded long-running expiry campaigns
  (>1M simulated seconds/night).
- **When a fresh seed fails, the fix lands *with that seed appended* to the
  corpus** — so the bug can never silently come back.

## Why a custom simulator?

Deterministic simulation testing (DST) is how InfinityDB makes its
shared-nothing concurrency trustworthy. Injecting all nondeterminism — clock,
randomness, network chunking, scheduler order — means the test suite explores
real interleavings and any failure is a replayable artifact, not a Heisenbug.
This is the same discipline used by FoundationDB and TigerBeetle.
