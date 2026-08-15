# M4-S20 mixed-node coexistence audit

- date: 1786835097 (unix) · tier: **reference-box (binding)** · env-check: pass
- server: infinityd 6bd25b1 (git 6bd25b139d16, x86_64-unknown-linux-gnu) · 4 cells · pin-start 4
- loadgen cpus_allowed: 12-23 (server cells pinned from 4; a loadgen set overlapping the cell cpus invalidates the run — the C5 lesson)
- seed 0x51d02026 · 25s per leg · cache = default DB, node maxmemory 96mb allkeys-lru, 262144 keys × 512 B offered · documents 20000 × gate-1KiB on `audit-doc` · tiered 1310720 × 512 B = 10× 64mb on `audit-tier` (disk budget 4gb)
- generator placement: cache 8 conns × pipeline 16, documents 4 × 4, tiered 2 × 32. The cache and document legs run the identical connection counts solo and mixed; the tiered leg takes its queue depth from pipelining rather than threads, so the mixed leg adds 2 mostly-I/O-blocked generator threads rather than a second workload's worth of CPU (the C5 lesson — a mixed leg whose generator is crowded measures generator crowding)

## Legs

| leg | ops/s | p50 µs | p99 µs | p99.9 µs | errors | nil replies |
|---|---|---|---|---|---|---|
| cache solo | 1825172 | 67 | 111 | 131 | 0 | 7879017 |
| document solo | 388791 | 38 | 65 | 93 | 0 | 0 |
| tiered solo | 29506 | 2111 | 5759 | 7295 | 0 | 0 |
| cache mixed | 1066376 | 111 | 203 | 927 | 0 | 6528535 |
| document mixed | 173020 | 85 | 167 | 415 | 0 | 0 |
| tiered mixed | 35055 | 1727 | 5247 | 8063 | 0 | 0 |

Tiered fill: 1310720 keys at 295586 sets/s, 0 error replies (typed durable admission backpressure is a refusal, not a loss — the fill is checked on convergence).

## Isolation (solo → mixed, same campaign)

| namespace | ops/s Δ | p99 Δ | miss-rate solo → mixed |
|---|---|---|---|
| cache | -41.6% | +82.9% | 17.3% → 24.5% |
| documents | -55.5% | +156.9% | — |
| tiered | +18.8% | -8.9% | — |

Cold-read evidence for the isolation condition ("while the tiered ns serves cold reads at full QD"): **816099 cold reads issued during the mixed run** (687119 solo), cold-read queue depth p99 **2** (cap 64, ADR-0055 D2), tiered cold service p99 **0.4 ms**, 2067245 tail allocations, 75497472 B committed. A run that reached the mixed leg without cold reads fails rather than reports.

Gate `cache_isolation_p99` (≤ 10%, reference-box): measured +82.9% — FAIL

## Attribution (continuous, mixed run)

- sum(domains) vs RSS worst divergence: **2.1%** over 25 samples — computed on growth over the post-boot baseline (the M3 CI delta discipline: executable text/stacks are RSS no domain claims), RSS bracketing the ~1 s domain scrape, 32 MiB growth floor; 12 M2 domains + tiering committed/index, reserved VA excluded (not resident)
- baselines: RSS 81821696 B, domains 100873664 B (post-boot, pre-load)
- Gate `mixed_attribution` (≤ 10%, any tier): PASS
- peak RSS 349167616 B · final RSS 345620480 B · standing tiered reservation 536870912 B VA, 75497472 B committed
- page-cache disclosure: the tiered leg does real file I/O this run (75497472 B committed, 816099 cold reads in the mixed leg). S09 chose `Direct` (ADR-0054), so tier reads bypass the page cache and no file-cache term is claimed against RSS; `tiering_reserved_bytes` (536870912 B) is VA, not resident, and is excluded from the domain sum for that reason

## Saturation disposition (§19)

- cache generator: GENERATOR-LIMITED at 8 conns (+50% conns moved ops/s -14.8% — solo absolutes understate the server; deltas remain valid at fixed generator config)
- document generator: not probed this run — the doc leg's absolutes are context, not claims; its isolation *delta* is the audit quantity (fixed generator config both sides)
- tiered generator: not probed. This leg is device-bound by construction (uniform keys over 10× its memory budget on a Gen3 DRAM-less NVMe), so a connection probe would measure the drive, not the generator; its absolutes are context and its isolation delta is the audit quantity

## Findings

- **The original audit finding is CLOSED.** Per-namespace `MAXMEMORY` on named memory namespaces was registry-carried but unenforced when this audit first ran (2026-07-30): `Keyspace::evict_toward` rotated the numbered dbs only, so a named cache namespace never evicted and its growth evicted numbered-DB keys instead. **M4-S27 (ADR-0068) enforces it since 2026-08-06** — budgeted memory namespaces evict toward their own budget in structural isolation. The cache leg still runs on the default DB so its numbers stay comparable with the 2026-07-30 baseline; that is now a continuity choice, not a workaround.
- **The AC's "full QD" condition is not reachable on this box, and that is a measurement, not a shortfall.** This run reached `cold_read_qd_p99` **2** against the ADR-0055 D2 cap of 64. The cap was probed directly on 2026-08-15: quadrupling this harness's pipeline depth (32 → 128) left the depth unchanged, and an `ycsb` connection sweep moved it only 5 → 7 while throughput went 39.6k → 68.2k ops/s. The 32 h unified soak read 5. The cells drain the cold queue about as fast as a generator can fill it, so depth is set by device service time rather than by admission — reaching 64 would need a slower device or a lower cap, not a harder push. The isolation number below is therefore taken against a **continuously busy** cold path (816099 cold reads in the mixed leg), which is the strongest form of the condition this hardware admits, and the reader should not read it as the cap-saturated case.

## Named absent (debt-forward, honesty rules)

| row | why absent | rejoins |
|---|---|---|
| *(the tiered data leg is no longer absent — it ran this campaign; the row is kept here struck through so the audit's own history stays readable)* | — | — |
| topic workload | M7 owns topics | M7, per the plan's debt-forward note |
| collections workload | M5 owns collections | M5, per the plan's debt-forward note |
