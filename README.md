# InfinityDB (`infinity/`)

One engine for the cache, the ledger, the document, the queue, and the vector.
This workspace is governed by [`docs/infinity-master-plan.md`](../docs/infinity-master-plan.md)
(design laws L1–L10) and the milestone plans under `../docs/milestones/`.

## Status: M0 — architecture skeleton

The thinnest end-to-end vertical: shard cells, fabric, RESP wire, minimal
store, bench harness, deterministic simulator. M0 ends in a **verdict**
(`docs/adr/0004-m0-verdict.md`), not a release; the STOP-gate campaign runs on
the Linux reference box.

**Platform tiers:** Linux x86-64/aarch64 + io_uring is the product tier.
macOS (kqueue) is a **development tier**: full correctness suite, explicitly
excluded from every performance gate. Items pending Linux validation are
tracked in [`../reviews/infinity-m0-skeleton.md`](../reviews/infinity-m0-skeleton.md).

## Layout

See master plan §20. Boundaries are mechanical: `scripts/check-dep-dag.sh`
fails CI on any internal dependency edge not allowed by `docs/dep-dag.toml`;
`#![forbid(unsafe_code)]` everywhere except the unsafe leaves
(`inf-simd`, `inf-alloc`, `inf-fabric`, `inf-runtime`), each carrying a
`SAFETY.md` inventory.

Frozen M0 interfaces: [`docs/interfaces-m0.md`](docs/interfaces-m0.md) —
changing one after M0 requires an ADR.

## Common commands

```bash
just check        # fmt + dep-DAG + deny-list + clippy + tests
just loom         # SPSC ring model checking
just compat       # byte-diff vs real redis-server
just sim-smoke    # deterministic simulator, trace-identity check
```
