# M4-S20 mixed-node coexistence audit

- date: 1786833990 (unix) · tier: **reference-box (binding)** · env-check: pass
- server: infinityd 6bd25b1 (git 6bd25b139d16, x86_64-unknown-linux-gnu) · 4 cells · pin-start 4
- loadgen cpus_allowed: 12-23 (server cells pinned from 4; a loadgen set overlapping the cell cpus invalidates the run — the C5 lesson)
- seed 0x51d02026 · 25s per leg · cache = default DB, node maxmemory 96mb allkeys-lru, 262144 keys × 512 B offered · documents 20000 × gate-1KiB on `audit-doc` · tiered 1310720 × 512 B = 10× 64mb on `audit-tier` (disk budget 4gb)
- generator placement: cache 8 conns × pipeline 16, documents 4 × 4, tiered 2 × 32. The cache and document legs run the identical connection counts solo and mixed; the tiered leg takes its queue depth from pipelining rather than threads, so the mixed leg adds 2 mostly-I/O-blocked generator threads rather than a second workload's worth of CPU (the C5 lesson — a mixed leg whose generator is crowded measures generator crowding)

## Legs

| leg | ops/s | p50 µs | p99 µs | p99.9 µs | errors | nil replies |
|---|---|---|---|---|---|---|
| cache solo | 1822258 | 67 | 111 | 131 | 0 | 7867278 |
| document solo | 387471 | 38 | 65 | 91 | 0 | 0 |
| tiered solo | 37727 | 1471 | 5503 | 40959 | 0 | 0 |
| cache mixed | 1071817 | 109 | 187 | 4223 | 0 | 6563797 |
| document mixed | 167252 | 87 | 163 | 3967 | 0 | 0 |
| tiered mixed | 34045 | 1727 | 6143 | 9983 | 0 | 0 |

Tiered fill: 1310720 keys at 552764 sets/s, 0 error replies (typed durable admission backpressure is a refusal, not a loss — the fill is checked on convergence).

## Isolation (solo → mixed, same campaign)

| namespace | ops/s Δ | p99 Δ | miss-rate solo → mixed |
|---|---|---|---|
| cache | -41.2% | +68.5% | 17.3% → 24.5% |
| documents | -56.8% | +150.8% | — |
| tiered | -9.8% | +11.6% | — |

Cold-read evidence for the isolation condition ("while the tiered ns serves cold reads at full QD"): **831193 cold reads issued during the mixed run** (881035 solo), cold-read queue depth p99 **3** (cap 64, ADR-0055 D2), tiered cold service p99 **0.4 ms**, 2438329 tail allocations, 75497472 B committed. A run that reached the mixed leg without cold reads fails rather than reports.

Gate `cache_isolation_p99` (≤ 10%, reference-box): measured +68.5% — FAIL

## Attribution (continuous, mixed run)

- sum(domains) vs RSS worst divergence: **6.5%** over 25 samples — computed on growth over the post-boot baseline (the M3 CI delta discipline: executable text/stacks are RSS no domain claims), RSS bracketing the ~1 s domain scrape, 32 MiB growth floor; 12 M2 domains + tiering committed/index, reserved VA excluded (not resident)
- baselines: RSS 81891328 B, domains 100873664 B (post-boot, pre-load)
- Gate `mixed_attribution` (≤ 10%, any tier): PASS
- peak RSS 384266240 B · final RSS 384794624 B · standing tiered reservation 536870912 B VA, 75497472 B committed
- page-cache disclosure: the tiered leg does real file I/O this run (75497472 B committed, 831193 cold reads in the mixed leg). S09 chose `Direct` (ADR-0054), so tier reads bypass the page cache and no file-cache term is claimed against RSS; `tiering_reserved_bytes` (536870912 B) is VA, not resident, and is excluded from the domain sum for that reason

## Saturation disposition (§19)

- cache generator: GENERATOR-LIMITED at 8 conns (+50% conns moved ops/s +10.9% — solo absolutes understate the server; deltas remain valid at fixed generator config)
- document generator: not probed this run — the doc leg's absolutes are context, not claims; its isolation *delta* is the audit quantity (fixed generator config both sides)
- tiered generator: not probed. This leg is device-bound by construction (uniform keys over 10× its memory budget on a Gen3 DRAM-less NVMe), so a connection probe would measure the drive, not the generator; its absolutes are context and its isolation delta is the audit quantity

## Findings

- **Per-namespace `MAXMEMORY` on named memory namespaces is unenforced**: the registry carries it (M1) but the eviction sweep rotates the numbered dbs only (`Keyspace::evict_toward`), so a named cache namespace never evicts. This audit therefore runs the cache leg on the default DB under node-level `CONFIG SET maxmemory` (the proven M1 machinery). Recorded for the plan: per-namespace eviction enforcement needs an owner before a multi-cache node is honest.

## Named absent (debt-forward, honesty rules)

| row | why absent | rejoins |
|---|---|---|
| *(the tiered data leg is no longer absent — it ran this campaign; the row is kept here struck through so the audit's own history stays readable)* | — | — |
| topic workload | M7 owns topics | M7, per the plan's debt-forward note |
| collections workload | M5 owns collections | M5, per the plan's debt-forward note |
