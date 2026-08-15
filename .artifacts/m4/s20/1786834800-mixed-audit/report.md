# M4-S20 mixed-node coexistence audit

- date: 1786834800 (unix) · tier: **reference-box (binding)** · env-check: pass
- server: infinityd 6bd25b1 (git 6bd25b139d16, x86_64-unknown-linux-gnu) · 4 cells · pin-start 4
- loadgen cpus_allowed: 12-23 (server cells pinned from 4; a loadgen set overlapping the cell cpus invalidates the run — the C5 lesson)
- seed 0x51d02026 · 25s per leg · cache = default DB, node maxmemory 96mb allkeys-lru, 262144 keys × 512 B offered · documents 20000 × gate-1KiB on `audit-doc` · tiered 1310720 × 512 B = 10× 64mb on `audit-tier` (disk budget 4gb)
- generator placement: cache 8 conns × pipeline 16, documents 4 × 4, tiered 2 × 128. The cache and document legs run the identical connection counts solo and mixed; the tiered leg takes its queue depth from pipelining rather than threads, so the mixed leg adds 2 mostly-I/O-blocked generator threads rather than a second workload's worth of CPU (the C5 lesson — a mixed leg whose generator is crowded measures generator crowding)

## Legs

| leg | ops/s | p50 µs | p99 µs | p99.9 µs | errors | nil replies |
|---|---|---|---|---|---|---|
| cache solo | 1637508 | 75 | 115 | 135 | 0 | 7068775 |
| document solo | 391378 | 38 | 65 | 93 | 0 | 0 |
| tiered solo | 43429 | 5631 | 11519 | 20991 | 0 | 0 |
| cache mixed | 1096040 | 103 | 183 | 4351 | 0 | 6711947 |
| document mixed | 146148 | 97 | 179 | 4351 | 0 | 0 |
| tiered mixed | 36147 | 6527 | 17407 | 24575 | 0 | 0 |

Tiered fill: 1310720 keys at 554744 sets/s, 0 error replies (typed durable admission backpressure is a refusal, not a loss — the fill is checked on convergence).

## Isolation (solo → mixed, same campaign)

| namespace | ops/s Δ | p99 Δ | miss-rate solo → mixed |
|---|---|---|---|
| cache | -33.1% | +59.1% | 17.3% → 24.5% |
| documents | -62.7% | +175.4% | — |
| tiered | -16.8% | +51.1% | — |

Cold-read evidence for the isolation condition ("while the tiered ns serves cold reads at full QD"): **908792 cold reads issued during the mixed run** (1011706 solo), cold-read queue depth p99 **3** (cap 64, ADR-0055 D2), tiered cold service p99 **0.5 ms**, 2734248 tail allocations, 75497472 B committed. A run that reached the mixed leg without cold reads fails rather than reports.

Gate `cache_isolation_p99` (≤ 10%, reference-box): measured +59.1% — FAIL

## Attribution (continuous, mixed run)

- sum(domains) vs RSS worst divergence: **22.3%** over 25 samples — computed on growth over the post-boot baseline (the M3 CI delta discipline: executable text/stacks are RSS no domain claims), RSS bracketing the ~1 s domain scrape, 32 MiB growth floor; 12 M2 domains + tiering committed/index, reserved VA excluded (not resident)
- baselines: RSS 81891328 B, domains 100873664 B (post-boot, pre-load)
- Gate `mixed_attribution` (≤ 10%, any tier): FAIL
- peak RSS 424243200 B · final RSS 422768640 B · standing tiered reservation 536870912 B VA, 75497472 B committed
- page-cache disclosure: the tiered leg does real file I/O this run (75497472 B committed, 908792 cold reads in the mixed leg). S09 chose `Direct` (ADR-0054), so tier reads bypass the page cache and no file-cache term is claimed against RSS; `tiering_reserved_bytes` (536870912 B) is VA, not resident, and is excluded from the domain sum for that reason

## Saturation disposition (§19)

- cache generator: GENERATOR-LIMITED at 8 conns (+50% conns moved ops/s +11.8% — solo absolutes understate the server; deltas remain valid at fixed generator config)
- document generator: not probed this run — the doc leg's absolutes are context, not claims; its isolation *delta* is the audit quantity (fixed generator config both sides)
- tiered generator: not probed. This leg is device-bound by construction (uniform keys over 10× its memory budget on a Gen3 DRAM-less NVMe), so a connection probe would measure the drive, not the generator; its absolutes are context and its isolation delta is the audit quantity

## Findings

- **The original audit finding is CLOSED.** Per-namespace `MAXMEMORY` on named memory namespaces was registry-carried but unenforced when this audit first ran (2026-07-30): `Keyspace::evict_toward` rotated the numbered dbs only, so a named cache namespace never evicted and its growth evicted numbered-DB keys instead. **M4-S27 (ADR-0068) enforces it since 2026-08-06** — budgeted memory namespaces evict toward their own budget in structural isolation. The cache leg still runs on the default DB so its numbers stay comparable with the 2026-07-30 baseline; that is now a continuity choice, not a workaround.
- **Cold-read queue depth reached p99 3 against the ADR-0055 D2 cap of 64.** The AC's condition is that the cache namespace holds its latency *while the tiered namespace serves cold reads at full QD*, so the depth is part of the claim, not a footnote: a shallow queue would make the isolation number look better than the profile it is supposed to describe.

## Named absent (debt-forward, honesty rules)

| row | why absent | rejoins |
|---|---|---|
| *(the tiered data leg is no longer absent — it ran this campaign; the row is kept here struck through so the audit's own history stays readable)* | — | — |
| topic workload | M7 owns topics | M7, per the plan's debt-forward note |
| collections workload | M5 owns collections | M5, per the plan's debt-forward note |
