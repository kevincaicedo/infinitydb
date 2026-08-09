# RSS attribution — soak-unified-20260807-0227 (added 2026-08-08)

The stamp-time reading "process RSS 2.07 GB vs `used_memory` 633 MB with
~1.1 GiB unattributed" is resolved: **there is no unattributed gigabyte.**
`INFO`'s Memory section is a **node-scope fold** (summed across the 4
cells' published gauges), while the Tiering, Persistence, and Tripwires
sections are **cell-scope** (the serving cell only). The tier region and
tiered index therefore appear in `info-end.txt` at 1/4 of their node-wide
size. Multiplying the cell-scope terms by 4 closes the gap.

## Attribution table (node-wide, end of run; RSS 2,073,911,296 B)

| Component | Bytes | In `used_memory`? | Evidence |
|---|---:|---|---|
| Record arenas (4 cells × 117,440,512) | 469,762,048 | yes | `store.rs` slab chunks — high-water, never unmapped |
| Doc arenas (4 × 20,971,520) | 83,886,080 | yes | INFO fold = exactly 4× the cell value |
| Wire buffer pools (4 × 4096 × 4096 B) | 67,108,864 | yes | `bins/infinityd/src/main.rs` defaults; zeroed at boot → resident |
| KV index + wheel + doc scratch + conn state (×4) | ~11,860,415 | yes | tripwires ×4 |
| **`used_memory` subtotal** | **632,617,407** | | reconciles bottom-up to 633.5 MB (cells symmetric to 0.15%) |
| Tier region committed (4 × 270,532,608) | 1,082,130,432 | **no — cell-scope in INFO** | page math exact: net 258 pages × 1 MiB/cell = mutable window 250‰ × 1 GiB + slack. Every cell materializes the FULL namespace budget (no per-cell split) |
| Tiered index sidecar (4 × 71,303,168) | 285,212,672 | **no — cell-scope in INFO** | ~27 B/key × 2.62 M keys/cell |
| Log staging (4 × 2 × 4 MiB) | 33,554,432 | no | fixed per cell |
| Checkpoint double-buffers (4 × 2,097,256) | 8,389,024 | no | `ckpt_buffer_bytes` ×4 |
| Cold-read AlignedPool (4 × 64 × 16 KiB) | 4,194,304 | no | QD cap 64 × 4-frame buffers |
| Fabric SPSC rings (12 × 4096 × 64 B + packs) | ~3,178,000 | no | mesh cells×(cells−1) |
| io_uring SQ/CQ/SQE mappings | ~1,654,784 | no | measured via smaps on a probe instance |
| Binary text + libc (file-backed, mostly shared) | ~4,600,000 | no | measured |
| Thread stacks + TLS (6 user threads) | ~2,300,000 | no | measured |
| **Model total** | **≈ 2,057.5 MB** | | |
| **Residual vs RSS** | **≈ 16.4 MB (0.8%)** | | |

Also note: `tiering_reserved_bytes 2,147,483,648` is per-cell VA
reservation (8 GiB node-wide) — reserved, not resident; correctly absent
from RSS beyond the committed window.

## The residual, and the warm-up grower hypothesis

The ~16 MB residual is malloc-backed anon, dominated by the per-cell
**reply and command buffer pools** (`plane.rs` recycle pools: up to
4096 × 4096 B each per cell, worst case 128 MB node-wide) — unattributed
in INFO today. Their high-water-fill behavior (they grow only when a
concurrency spike holds that many buffers at once — parked replies behind
gated durable acks, connection bursts at the 300 s leg restarts) matches
the observed warm-up shape: +20.6 MB in h0–8 with decaying increments,
then plateau. **This is a hypothesis, not a ledger-grade root cause**,
until the pool gauges exist — `reply_pool_bytes`/`cmd_pool_bytes`/
`cold_pool_bytes` INFO fields are being added before the attempt-3 soak
(readiness F15), which will let the next run adjudicate it directly.

Rejected alternatives: region commit churn is ring-bounded (net pages
constant; decommit immediate); record-arena high-water is inside
`used_memory`, which was flat ±0.1%; tiered index is static post-fill;
staging/ckpt buffers are fixed-capacity.

## `mem_fragmentation_ratio 3.28` explained

The field is `rss / used_memory` verbatim — Redis-compat shape, **not
allocator fragmentation**. On a tiered node the numerator carries
~1.42 GiB of deliberate, fully-designed resident memory (tier ring
windows, tiered index, staging, pools) that the denominator excludes.
Recomputed against the full model above, the true overhead ratio is
**~1.008**. Reading 3.28 as fragmentation on a tiered node is a
scope-trap misread; a doc note / scope-corrected companion field is
queued with the F15 instrumentation.

## Cgroup file cache

Not captured this run (the F9/F11 gap this analysis closes going
forward). O_DIRECT tier I/O (ADR-0054, verified at open, no silent
fallback) should keep tier reads/writes out of the page cache, but this
artifact cannot prove it — no file-cache series exists. The attempt-3
sampler captures smaps_rollup + the server cgroup's `memory.stat`
hourly (`attribution.log`), with the shared-scope caveat recorded
in-band; expected `file` content is log segments only (bounded by
`segs_live` × 64 MiB, sawtoothing with truncation).
