# Campaign E verdict (2-slot pool, engine a2a96d3, 3 interleaved replicates, 256 MiB segments, 150 s legs, idle-state recovery boot binding)

| gate | measured (per-replicate median) | verdict |
|---|---|---|
| warmed zero-fill share (arm) ≤ 0.10 | **0.210** (0.210 / 0.196 / 0.210; baseline 1.012 / 1.023 / 0.999; campaign D's 1-slot arm 0.377) | FAIL — but **not** falsifier (a)'s > 0.3 |
| recycle deficit per cell ≤ 2 | **1** (1 / 1 / 1) | PASS |
| padding delta ≤ 3 pts | 0.0–0.2 (51.7 % both arms) | PASS — S39c's term untouched |
| always c32 p50 arm/base ≤ 1.05 | **0.96** (707 / 705 / 705 vs 731 / 731 / 735 µs) | PASS |
| always c32 p99 arm/base ≤ 1.10 | **0.54** (2.26 / 2.07 / 2.24 vs 4.16 / 5.22 / 2.84 ms) | PASS |
| read c64p16 arm/base ≥ 0.98 | **1.01** | PASS |
| recovery, idle-state boot, arm/base ≤ 1.05 | **1.04** (0.276 / 0.334 / 0.268 vs 0.262 / 0.321 / 0.259 s) | PASS — falsifier (c) closed |
| recovery, immediate boot (informational) | 1.38 (5.20 / 5.18 / 5.17 vs 3.78 / 4.09 / 3.63 s) | drive state, see below |
| rotations per cell ≥ 8 | 10 | PASS (validity) |
| host bytes per log byte (info) | **1.39** arm vs 2.20 baseline | — |
| device sectors per log byte (info) | **1.40** arm vs 2.20 baseline | — |

Throughput c32: arm 32.5 / 34.7 / 32.5 k vs base 32.0 / 31.4 / 31.9 k ops/s.
`recycle_pool_full` = **0 on every arm leg**; misses 16 per leg = 3 first-
generation per cell + **exactly 1 warmed miss per cell per leg**; recycled
29 / 32 / 30. Recycled-residue facts on the arm boots: 9 / 8 / 8 slacks.

## What the two campaigns say together

- The mechanism is real and costs nothing the row can see: zero-fill per
  log byte 1.01 → 0.38 (1 slot) → 0.21 (2 slots); accounted host bytes per
  log byte 2.20 → 1.56 → 1.39, the block device agreeing to 0.3 %; padding
  untouched; `always` p50 −3–4 %, **p99 halved** (the zero-fill no longer
  competes with the barrier); reads inside ±2 %; recovery +4 % on the
  page-cache-hot idle boot (the residue scan's +4 ms per 256 MiB, measured
  directly, is the whole of it).
- **The immediate-boot recovery ratio was drive state, not the rule:** the
  same image boots in 3.6–5.2 s right after the leg and in 0.26–0.33 s after
  the 40 s idle (device reads + the drive digesting the leg vs a warm page
  cache); the arm's immediate boots are slower because its leg ends with the
  drive in a different state (in-place overwrites of recycled extents), not
  because recovery does more — the idle boots are within 4 %.
- **The ≤ 0.1 hypothesis is not met at either bound, and the reason is not
  the bound:** `recycle_pool_full` is 0 in every arm leg, so truncation
  never overflows the pool — the pool is *empty* when the prealloc asks.
  The MAINTAIN prealloc runs at the instant of rotation, while the segment
  that could feed it is truncated only after the checkpoint that begins in
  the new active segment publishes; one warmed miss per cell per ~9
  rotations is the phase slip between the two cadences. A second slot
  halves the share (a candidate survives one slip) but cannot remove it.

## Disposition (predeclared rules, read as written)

D's rule sent the 2-slot pool as the next hypothesis; E's rule covered
"every binding gate green ⇒ default 2" and "> 0.3 ⇒ off" and **did not
cover 0.1 < share ≤ 0.3** — recorded as a gap in the predeclaration. The
disposition that follows the discipline: the recycling mechanism is
**Accepted as measured**; the zero-fill-share hypothesis (≤ 0.1) is
**Revised** — its number is `Evidence-pending` on the next lever, and the
gate threshold is **not** moved. The default **stays at 1 slot** (the
configuration every correctness row was run on; the A/B discipline does not
license a retune to 2 on a red share gate, and 2 slots buy 0.38 → 0.21 for
+256 MiB of pooled disk per cell that the feed-timing lever is expected to
deliver for free). Next hypothesis, ADR-first, its own A/B, **not a third arm
tonight**: defer the next-segment prealloc — when recycling is on and the
pool is empty at rotation, MAINTAIN waits (bounded: until the active segment
is a quarter full) for a pooled candidate before creating a fresh file; the
zero-fill pacing already finishes a fresh fill by the half-segment mark, so a
quarter-segment deferral costs the fresh path nothing it does not already
absorb, and a budgeted burst at most.
