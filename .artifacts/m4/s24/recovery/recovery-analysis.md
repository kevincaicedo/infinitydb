# M4 §7 recovery sub-gate, with tiering on (2026-08-15, binding env)

**Shape:** one durable node, 4 cells, a tiered namespace
(`MEM-BUDGET 1024mb`, `DISK-BUDGET 40gb`) filled to **10 GiB of user
data** (10,485,760 × 1024 B) in 110.9 s, stopped cleanly, then booted
again on the same data directory and timed to first `PONG`.

**Correction to `recovery.log`.** The harness's inline per-cell figure
(0.693 GB/s/cell) divides the whole data *directory* (16.39 GB) by the
boot time. That is wrong in both directions at once: the directory
includes tier files, which recovery does **not** replay — they are read on
demand — so it credits the boot with bytes it never touched. The boot log
reports what was actually replayed, and that is what the numbers below
use.

## What the boot actually did (`boot2.log`)

```
control: recovery 0/4 cells ready, 407239831/6278457156 bytes (6.5%), eta 73s
control: cell 0 recovered (4 segments, 1569488192 bytes, 2812633 records)
control: cell 1 recovered (4 segments, 1570059952 bytes, 2812237 records)
control: cell 2 recovered (4 segments, 1569892618 bytes, 2812069 records)
control: cell 3 recovered (4 segments, 1569016394 bytes, 2811913 records)
control: recovery complete — 4 cells serving (5906 ms)
```

| quantity | value |
|---|---|
| replayed, node | 6,278,457,156 B = **6.278 GB** |
| replayed, per cell | ~1.5695 GB (4 segments, ~2.812 M records) |
| boot wall, to first PONG | **5.906 s** |
| user data resident in the namespace | 10 GiB |

## The two gate rows

| row | threshold | measured | verdict |
|---|---|---|---|
| 10 GB node cold boot, tiering on | < 15 s | **5.906 s** | **PASS** (2.5× margin) |
| replay throughput per cell | ≥ 1 GB/s/cell | **0.266 GB/s/cell** | **FAIL** (3.8× short) |

Node-aggregate replay is 1.063 GB/s; per cell — the form the gate is
written in, and the right one, because recovery is a per-cell local
problem (L1) and all four cells ran concurrently for the whole 5.906 s —
it is 1.5695 GB / 5.906 s.

## Reading the failure

**Replay is record-bound, not device-bound.** Each cell replayed 2.812 M
records in 5.906 s = **476 k records/s/cell**, moving 1.57 GB. The S04
reader artifact measured raw sequential read on this class of box at
3.6 GiB/s, so recovery is running ~13× below what the device could
deliver. Adding device bandwidth would not move this row; the cost is per
record.

**This is the worst-case shape, and that is disclosed, not corrected
for.** `INFO persistence` immediately before the stop shows
`rdb_bgsave_in_progress:1` with `watermark_lag_lsn 36,830,234` — no
checkpoint had completed, so the boot replayed the full WAL tail with no
`.ick` prefix to skip. The `recovery_replay` bench calls this the
`tail-only` row and the steady-state `ick-tail` shape replays roughly
half as much.

**Page cache was warm** (dropping it needs `sudo`, which this session did
not take). A warm cache can only make the boot *faster*, so the FAIL is
conservative: a genuinely cold boot reads the same 6.28 GB from the
device and cannot beat this number.

## Owed

A steady-state (`ick-tail`) re-read with a completed checkpoint before the
stop, and a cold-cache run under `sudo sysctl vm.drop_caches=3`, would
bracket the row properly. Neither changes the direction: at 476 k
records/s/cell the per-cell gate needs ~3.8× more record throughput, not
a different cache state.
