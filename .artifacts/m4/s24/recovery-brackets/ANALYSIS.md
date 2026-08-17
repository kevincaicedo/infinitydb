# Recovery brackets (2026-08-16) — the gate is not measuring what its name says

Three legs, one box, one binary (`infinityd-6bd25b1`). Two run in the same
session minutes apart; the third is the 2026-08-15 campaign point.

| leg | when | shape | cache | boot wall | replayed | records |
|---|---|---|---|---|---|---|
| tail-only | 2026-08-15 | no checkpoint | warm | **5.906 s** | 6.278 GB | 11,248,852 |
| **tail-only** | **2026-08-16** | no checkpoint | warm | **13.415 s** | 4.435 GB | 11,457,788 |
| **ick-tail** | **2026-08-16** | checkpoint completed | warm | **12.308 s** | 3.364 GB | 10,611,896 |

Artifacts: `recovery-brackets/` (ick-tail), `recovery-brackets-tail-only/`,
and `recovery/` (the 08-15 point). Leg B (cold cache) is **not yet run** —
it needs `sudo`.

## Finding 1 — the boot shape is not the variable. The drive is.

The 2026-08-15 point (5.906 s) and today's **identical shape** (13.415 s)
differ by **2.27×**. Same binary, same box, same 10 GiB dataset, same
`--cells 4`, warm cache both times. The only thing that changed is the
day, and the drive shows it independently: the fill took **326–346 s
today versus 110.9 s on 08-15**, a 3× slowdown on the same work.

This is the F20/F29 drive-state effect reaching the recovery gate. It has
a direct consequence: **the 5.906 s figure in ledger C38a is not
reproducible.** The `< 15 s` gate still passes today at 13.4 s, but the
margin collapses from **2.5× to 1.11×**.

**A correction to an earlier reading in this same session.** Comparing
today's ick-tail (12.3 s) against 08-15's tail-only (5.9 s) suggested the
steady-state shape was ~2× *slower* and that C38a's "this is the
worst-case shape" disclosure was falsified. **That was wrong** — it was a
cross-day comparison confounded by drive state. The same-session control
settles it: ick-tail **12.308 s** vs tail-only **13.415 s**, so the
checkpoint shape is slightly *faster*, exactly as C38a's disclosure
predicts (it replays less). **C38a's disclosure stands.** The control leg
existed precisely to catch this, and it did.

## Finding 2 — recovery is dominated by `Phase::Start`, not by record replay

Every leg's boot log carries per-phase progress. `Phase::Start` is defined
in `crates/inf-server/src/recover.rs:173` as:

> *Dirs, MANIFEST, scan, floor checks, checkpoint open + presize — bounded
> by directory-entry counts, one step.*

The logs show the cells sitting in that phase for nearly the whole boot:

```
control: cell 0 not ready — in start (0/0 bytes) for 11s      <- ick-tail
control: cell 0 not ready — in checkpoint (76448907/839519793 bytes) for 12s
control: recovery complete — 4 cells serving (12308 ms)
```

```
control: cell 0 not ready — in start (0/0 bytes) for 12s      <- tail-only, today
control: cell 0 not ready — in checkpoint (282061956/1108522765 bytes) for 13s
control: recovery complete — 4 cells serving (13415 ms)
```

The 08-15 leg has **zero** `in start` lines — it was already replaying by
the time the 5 s reporting threshold hit.

Splitting each boot at the observed phase transition:

| leg | `Start` | replay | GB/s/cell over **replay** | GB/s/cell over **whole boot** |
|---|---|---|---|---|
| tail-only 08-15 | ~4.5 s | ~1.4 s | ~1.12 | 0.266 |
| tail-only today | ~12.5 s | ~0.9 s | ~1.21 | 0.083 |
| ick-tail today | ~11.5 s | ~0.8 s | ~1.04 | 0.068 |

**Record replay moves 3.4–6.3 GB in about one second in every leg.** The
whole-boot figure varies 4× across legs only because `Start` does.

### What this means for the gate

The §7 row is *"Recovery with tiering on: **replay throughput** per cell
≥ 1 GB/s"*. Measured against the phase the row is named after, all three
legs sit at **~1.0–1.2 GB/s/cell** — at or above the bar. The published
**0.266 GB/s/cell FAIL divides replayed bytes by total boot time**, most
of which is `Start` doing directory and manifest I/O that replays no
records at all.

**This is a measurement-definition question, and it is the owner's to
settle, not an agent's.** Two honest options:

- **(a)** The row means *end-to-end boot throughput*. Then the number is
  right, the name is wrong, and it should be renamed — and the optimization
  target is `Start`, not the decoder.
- **(b)** The row means *replay throughput*. Then it must be measured on
  the replay phase, and on this evidence it is close to passing — but the
  gate would then say nothing about how long a node actually takes to come
  back, which is what an operator cares about.

Either way, **the current pairing of "replay throughput" as a name with
whole-boot arithmetic as a method is not defensible** and should not ship
unchanged.

### Precision caveat, stated rather than buried

The `Start`/replay split is read off progress lines emitted at **1-second
granularity**, so each phase boundary carries ±1 s. On a replay phase of
~1 s that is a large *relative* uncertainty: the per-leg replay rates above
are order-of-magnitude, **not** precise figures, and none of them should be
quoted as a claim. What is robust is the qualitative split — the phase
labels are explicit in the log, and `Start` is measured in seconds while
replay finishes inside one reporting interval in all three legs.

**The first thing any follow-up should do is timestamp the phase
transitions directly** (`Recovery::phase_code` already exists; it needs
per-transition timing, not a poll). Until then no precise per-phase number
is claimable.

## Finding 3 — the progress reporter is actively misleading during `Start`

Throughout the phase that dominates the boot, the node reports:

```
control: recovery 0/4 cells ready, 0/0 bytes (100.0%), eta 0s
```

**`100.0%` complete and `eta 0s`, for 12 of a 13-second boot.** The
percentage is `bytes_done/bytes_total` with both terms zero, and the ETA
follows it. An operator watching a slow restart is told it is finished.
This is a real observability defect independent of the performance
question, and it is cheap to fix (report the phase, and suppress a
percentage when the denominator is zero).

## Consequences to carry

1. **C38a** — 5.906 s is not reproducible; today the same shape reads
   13.415 s. The gate passes both times; the *margin* is the disclosure.
2. **C38b / F28** — "record-bound, not device-bound" is **mis-targeted**.
   The boot is `Start`-bound, and `Start` is directory/metadata-I/O bound,
   which *is* device-state sensitive — hence the 2.27× day-over-day swing.
3. **C20** — the ick-tail (steady-state) shape now has a same-session
   measurement: 12.308 s, ~0.068 GB/s/cell whole-boot. Its ≥ 1 GB/s
   narrowed wording is not supported on the whole-boot basis.
4. **M4.5-S21** — currently scoped as "recovery record-decode pipeline".
   On this evidence a decoder optimization targets **~10%** of a boot.
   Re-scope: instrument the phases first, then optimize what dominates.
5. **Leg B (cold cache) is now verdict-relevant.** With the warm-cache
   boot at 13.4 s and the gate at 15 s, there is 1.6 s of headroom. A cold
   `Start` — which is exactly the metadata I/O a cold cache punishes —
   could cross it. This was previously assessed as "won't change
   direction"; that assessment was made before `Start` was identified and
   **no longer holds**.
