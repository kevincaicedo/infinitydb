# 30 min instrument shake — NOT an endurance run (2026-08-12 23:28 → 00:07)

Purpose: prove the two ADR-amended changes work at the **real 2048 MiB
config** before spending another box-night — (1) the tiered leg's key
stream advances per leg, (2) the sampler records the reclaim series.
`soak-unified.sh 0.5` with `SOAK_LEG_SECS=240`.

**Non-citation by construction.** A 0.5 h run is dominated by the fill
and warm-up, and the verdict says so itself: *"steady-state window not
applicable (run 0.5 h <= 2x warm-up 8 h)"*. Its RSS/accounted slope
FAILs (+398%/24 h, +305%/24 h) are the fill filling — they are not
endurance readings and must never be quoted as any.

## What it proved

1. **Seeds advance and are logged.** 8 legs, each a pure function of the
   master seed and its index:
   `486541350, 3140977111, 5795412872, 8449848633, 11104284394,
   13758720155, 16413155916, 19067591677`.
2. **The reclaim series populates.** `dead_bytes` 0 → **641,611,744**
   over 8 legs; `live_bytes` 5,494,538,240 per cell (~22 GB node-wide);
   `compact_idle_pressure` 0.
3. **The fix does what it was meant to.** Dead space now accumulates
   steadily across legs — 57.9 MB after leg 0, 393 MB by leg 5, 641 MB
   by leg 7 (~25.7 MB/min node-wide) — where the 20260811 run's
   fixed-seed steady window was flat for 22 h (+0.23 GB disk, 219 flush
   slices). Fresh key streams keep displacing cold records.

## What it corrected — a defect in the change itself

The first cut of the discriminator used an **absolute** dead-byte
threshold (64 MiB) and consequently stamped this healthy run
`ENGINE-SIDE … escalate`. That was wrong, and the shake is what caught
it. **Compaction triggers on a RATIO**, not a byte count:
`dead / (dead + live) >= COMPACTION-DEAD-RATIO`, default **50%**
(`TierSpec::compaction_dead_ratio_pct`, clamped 50..=100 by ADR-0059 D1).
This run's 641 MB of dead is **2.83%** of live — compaction was never
eligible, and staying idle was correct behaviour.

Fixed: `live_bytes` joins the sampler as the denominator, the trigger
percentage is passed to the verdict (`SOAK_DEAD_RATIO_PCT`, default 50,
mirroring the store default), and the discriminator now compares the
**peak dead ratio** against the trigger — peak, because compaction that
did run would have pulled the ratio back down. Re-validated on four
fixtures: this run's shape (2.83% → WORKLOAD/CONFIG), past-trigger with
idle compaction (53.19% → ENGINE-SIDE), healthy (ratio high *and*
compaction advancing → PASS), and the real 20260811 bundle (old format,
verdict unchanged).

## What it explains about attempt 4

The trigger scales with the live dataset, and the dataset scales with
the memory budget:

| run | live/cell | dead needed to trigger | dead reached |
|---|---|---|---|
| attempt 2, 1024 MiB | 2.7 GB | ~2.7 GB | 54.9 GB → 6.5 M slices |
| attempt 4, 2048 MiB | ~5.5 GB | **~5.5 GB** | never sampled; writes flat → 0 |

Doubling the budget doubled the backlog required before compaction can
start, while the repeated key stream cut backlog *production* to
near-zero. Both had to hold for a 30 h zero, and both did.

## Projection for attempt 5 (state it as a projection)

At the measured 25.7 MB/min node-wide, the ~22 GB node backlog is
reached in ~14 h; the rate should *rise* as the tail rolls over (this
run is 25 min old and its dead/user ratio is 1.7% against attempt 2's
24 h average of 43.6%), so onset is likely earlier. Either way it lands
inside the 8–32 h measured window with hours to spare.

**Early-abort criterion for attempt 5** — the thing this instrument was
built for: by the end of the 8 h warm-up, node-wide `dead_bytes` should
be ~10 GB and climbing. If it is flat, kill the run then instead of
discovering it at hour 30.
