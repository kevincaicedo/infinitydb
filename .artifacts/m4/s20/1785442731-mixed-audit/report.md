# M4-S20 mixed-node coexistence audit

- date: 1785442731 (unix) · tier: **dev (non-binding)** · env-check: FAILED (recorded, non-citable)
- server: infinityd 147c33a (git 147c33aca509, x86_64-unknown-linux-gnu) · 4 cells · pin-start 4
- loadgen cpus_allowed: 12-23 (server cells pinned from 4; a loadgen set overlapping the cell cpus invalidates the run — the C5 lesson)
- seed 0x51d02026 · 25s per leg · cache = default DB, node maxmemory 96mb allkeys-lru, 262144 keys × 512 B offered · documents 20000 × gate-1KiB on `audit-doc`

## Legs

| leg | ops/s | p50 µs | p99 µs | p99.9 µs | errors | nil replies |
|---|---|---|---|---|---|---|
| cache solo | 1566999 | 81 | 135 | 155 | 0 | 6766618 |
| document solo | 350821 | 43 | 73 | 101 | 0 | 0 |
| cache mixed | 1073638 | 111 | 211 | 243 | 0 | 6572782 |
| document mixed | 166639 | 83 | 199 | 235 | 0 | 0 |

## Isolation (solo → mixed, same campaign)

| namespace | ops/s Δ | p99 Δ | miss-rate solo → mixed |
|---|---|---|---|
| cache | -31.5% | +56.3% | 17.3% → 24.5% |
| documents | -52.5% | +172.6% | — |

Gate `cache_isolation_p99` (≤ 10%, reference-box): measured +56.3% — FAIL (DEV-TIER, non-binding)

## Attribution (continuous, mixed run)

- sum(domains) vs RSS worst divergence: **9.4%** over 25 samples — computed on growth over the post-boot baseline (the M3 CI delta discipline: executable text/stacks are RSS no domain claims), RSS bracketing the ~1 s domain scrape, 32 MiB growth floor; 12 M2 domains + tiering committed/index, reserved VA excluded (not resident)
- baselines: RSS 77135872 B, domains 100873664 B (post-boot, pre-load)
- Gate `mixed_attribution` (≤ 10%, any tier): PASS
- peak RSS 217837568 B · final RSS 217837568 B · standing tiered reservation 67108864 B VA, 0 B committed
- page-cache disclosure: no tiered data plane exists on this node (D8), so no file-cache term applies; the S09 cgroup series joins the S24 re-audit with real tier I/O

## Saturation disposition (§19)

- cache generator: GENERATOR-LIMITED at 8 conns (+50% conns moved ops/s +14.8% — solo absolutes understate the server; deltas remain valid at fixed generator config)
- document generator: not probed this run — the doc leg's absolutes are context, not claims; its isolation *delta* is the audit quantity (fixed generator config both sides)

## Findings

- **Per-namespace `MAXMEMORY` on named memory namespaces is unenforced**: the registry carries it (M1) but the eviction sweep rotates the numbered dbs only (`Keyspace::evict_toward`), so a named cache namespace never evicts. This audit therefore runs the cache leg on the default DB under node-level `CONFIG SET maxmemory` (the proven M1 machinery). Recorded for the plan: per-namespace eviction enforcement needs an owner before a multi-cache node is honest.

## Named absent (debt-forward, honesty rules)

| row | why absent | rejoins |
|---|---|---|
| tiered data leg (YCSB 10× RAM, full-QD cold reads) | the data plane is behind the ADR-0062 D8 `USE` refusal (verified live this run: 4 standing tables, zero tail allocs) and the S22 harness is unbuilt | command wiring + S22 → the S24 campaign re-audit |
| topic workload | M7 owns topics | M7, per the plan's debt-forward note |
| collections workload | M5 owns collections | M5, per the plan's debt-forward note |
