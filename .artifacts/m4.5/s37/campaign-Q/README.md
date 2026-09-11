# S37 campaign Q — the ticketed-`DEL` RSS/tail row (ADR-0093 A13; batch 30)

Written 2026-09-11 **before** the first leg. The evidence batch 29 named
and left `Evidence-pending`: the memory and the tail of a `DEL` whose
winner carries an open ticket (the walk: one Foreground twin read, the
twin's marker, the delete — `TwinScratch` on the stack, a cursor, zero
heap by construction).

## Row

`gate-run m4.5 --only-s37 --s37-ticketed-del --cells 4 --pin-start 0
--replicates 3 --s37-keys 1000000 --s37-del-keys 12288 --s37-del-cycles 4
--leg-idle-s 5 --device-probe off --data-root ~/bench-data/s37/data-Q`
on the **shipping** binary (engine tree = batch 30, the F-L07-02 fix in;
`infinityd` rebuilt 2026-09-11 18:24), campaign N's `io-properties.toml`
at the data root. Per leg: fresh server, tiered `always` namespace
(MEM-BUDGET 128 MB/cell, DISK-BUDGET 10 GB, TIER-IO-MODE direct), 1 M ×
1 KiB fill (≈ 250 MB/cell — beyond the budget, ≈ 90 % of the oldest keys
cold), **then** the arm's CONFIG keys, then four cycles on fresh key
windows of 12 288 keys (keys 0–49 151, the oldest — the most likely
cold): SET the window (64 conns, pipeline 1; on B every cold key opens a
ticket — 3 072 per cell, under `SHADOW_TICKETS_CAP` 4 096 and ≈ 3 MiB of
the 16 MiB pin cap), then DEL the window (64 conns, pipeline 1; on B
every ticketed winner walks its ticket), VmRSS of the server sampled
every 20 ms through the DEL pass. Two arms, ABBA:

- **A** = `tiered-shadow-overwrite no` — the shipping path: the SET pays
  the synchronous verifying read, the DEL is a RAM delete.
- **B** = `tiered-shadow-overwrite yes` + `tiered-shadow-reconcile no` —
  the ticket opens at the SET and **stays open** (the reconciler is
  paused, ADR-0093 A8) until its DEL walks it. The pause isolates the
  walk from the reconciler's cadence; it is not the D9 campaign's shape
  (that one runs the reconciler and decides the default — this row
  records the walk).

## What is read

Per cycle (raw line): SETs, tickets opened (per SET), fallbacks, DELs,
`tiering_shadow_forced_by_delete` (ticketed DELs — the coverage; A must
read 0, B near the cold share), `tiering_shadow_delete_run_refused`
(must be 0 — one same-key twin per winner), `tiering_shadow_pending`
after the DEL pass (must be 0 — every ticket drained), Foreground reads,
DEL ops/s, p50, p99, p99.9, max, VmRSS before / peak during / after the
DEL pass. Per leg: medians over the cycles (growth = the worst cycle).
Per arm: medians over the three replicates; B ÷ A ratios.

## Predeclared reading (no decision — the row records)

- **Memory (the A13 bound):** the walk allocates nothing per DEL, so B's
  RSS growth through a pass of ~12 k ticketed DELs must not scale with
  the DEL count: `growth_B − growth_A ≤ 8 MiB` (the sampler's noise
  floor: allocator arenas and the page cache of the twin reads), and B's
  `end − base` within 8 MiB of A's. Above that the bound is *not*
  demonstrated and the row says so (`Evidence-pending`, with the number).
- **Tail (recorded, no bar):** each ticketed DEL pays a Foreground twin
  read by design (D3); B's p50/p99/p99.9 against A's are the cost of
  that read on this device at 64 conns, disclosed with `cold_read`-class
  latencies. A p99.9 above 50 ms names a stall, not a mechanism.
- **Validity first:** B's ticketed share ≥ 0.7 of its DELs, refused = 0,
  pending after every pass = 0, A's forced = 0. A row that misses any of
  these is vacuous and is re-run, not read.

## Tier

Dev box, `--unsafe-env --allow-dirty`: governor `powersave` (the
reference-tier setup script needs sudo — not available to this session),
thermal-throttle counters non-zero since boot, tree dirty (batch 30's
own changes). **Non-citable dev tier**; the reference-tier rerun is the
same command with `--reference-box` after `setup-infinity-benchmark-env.sh`
on a committed tree. The memory reading is a difference of two arms on
one box in one run and does not depend on the governor; the tail
absolutes do.
