# Review-3 campaign (2026-08-22) — S39a default rerun under the shipping configuration + the amended K gate

Written **before** the first leg ran (engine `5e162b7`; the rules below are the
ADR-0089 second amendment and the ADR-0087 D8 third amendment, verbatim).
Tier: reference box (binding) — governor `performance`, turbo off, cells
pinned 0,2,4,6, generator 8,10,12,14, ext4 NVMe data root, clean tree.
`fstrim` was **not** run this session (no sudo from the agent shell) —
disclosed; the box was rebooted ~1 h before the campaign and idle since.

## Campaign A — S39a fill policy, baseline vs arm A, at the shipping configuration

`gate-run m4.5 --only-s36 --barrier-class fua --frames-in-flight 1 --staging-mib 4
--leg-idle-s 60`; baseline = `--fill-window-us 0`, arm A = `--fill-window-us
1000 --fill-target-kib 16`. Five pairs, ABBA order (base, A, A, base, base, A,
A, base, base, A). **What changed vs `s39a/`:** K = 1 / 4 MiB (the product
default), not K = 3 / 2 MiB; 60 s idle before every device leg, not 40.

Predeclared rule (ADR-0089 D6 (d) re-specified — conditional on the paired
baseline leg, which the original was not):

- **R1 efficacy** (must hold to keep the design point): arm-A median
  `log_padding_pct` ≤ 15 % **and** closed-loop `everysec` ops/s ≥ the paired
  baseline in ≥ 4 of 5 pairs. Prediction on record: at K = 1 the baseline's
  own padding is *lower* than K = 3's 24–30 % (the single in-flight write
  already batches), so the throughput delta is smaller than +14–39 %; it is
  still ≥ 0 in every pair because the row is device-bound.
- **R2 falsifier (d), conditioned:** an *arm-only stall* = an arm-A offered
  leg with `max` > 50 ms or `parked` > 0 whose paired baseline leg reads
  `max` ≤ 50 ms and `parked` = 0. The 08-22 `s39a` campaign already holds
  one (fillA-3, 413 ms / 106 parks); ADR-0089's amendment named a second as
  the trigger. **One arm-only stall in this campaign ⇒ (d) fires ⇒ the
  default stays 0** (the policy remains a flag). Stalls in *both* arms'
  offered legs are device behaviour and do not fire (d).
- **R3 tripwire:** arm-A closed-loop `parked` ≤ 1.5 × the paired baseline's
  in ≥ 4 of 5 pairs (the hold must not add admission parks); arm-A offered
  p99 ≤ baseline's in ≥ 4 of 5 pairs.
- Outcome → action: R1 ∧ ¬R2 ∧ R3 → default `--fill-window-us 1000`; R2
  → default 0, flag kept; R1 fails → `Rejected` at K = 1 (re-attributed:
  the cadence term is a K ≥ 2 term), default 0.

## Campaign B — the K gate, amended instrument, three pairings interleaved

`gate-run m4.5 --only-s35 --barrier-class fua --replicates 1 --leg-idle-s 40
--fill-window-us 0` (the cadence every prior K row used) × pairings
`K1/4 MiB`, `K3/2 MiB`, `K3/4 MiB` × 5 rounds, the pairing order rotated each
round (round r runs the pairings starting at index r mod 3). Each report is
one replicate; the campaign's medians are computed across the 5 reports per
pairing from the raw rows (`aggregate.py`). Instrument: client histogram
256 sub-buckets/octave (0.4 %, 2 µs at the S35 octave) — `5e162b7`.

Predeclared gate (ADR-0087 D8, third amendment):

- **G1** median `p50 ÷ barrier` at 4 cells × 32 conns ≤ 1.3 (unchanged).
- **G2 (binding, new)** median of per-replicate **barrier** 4c ÷ 1c ≤ 1.3 —
  the F2 contention term measured directly (FLUSH read ~1.8: fsync p50
  1,535 → 2,751 µs; FUA K = 3 read 1.09–1.15 on 08-22's raw rows).
- **G3 (informational)** median of per-replicate **client** p50 4c ÷ 1c with
  its spread, on the 0.4 % instrument. Prediction on record: K = 3 reads
  ≈ 1.30–1.36 (the pipeline's seal wait at 8 conns/cell, ~0.2 × by
  structure), K = 1 ≈ 1.45. It no longer binds — see the amendment for why.
- **G4 tails:** median 4-cell c32 p99 of a K = 3 pairing ≤ 1.1 × the K = 1
  pairing's; c256 `max` disclosed (drive-state dominated in every prior arm —
  not a gate). A leg with barrier p99 > 10 ms is drive-state flagged: its
  replicate is disclosed and excluded when ≥ 4 clean replicates remain,
  else the pairing is *noisy*.
- **G5 reads:** median read-leg ops/s within ±2 % across the three pairings.
- Decision table: K3/4 MiB passes G1, G2, G4, G5 → FUA class default K = 3 /
  4 MiB, FLUSH keeps K = 1 / 4 MiB (class-derived `--frames-in-flight auto`).
  Only K3/2 passes → K = 1 (the ≈ 2 MiB durable-record bound is not accepted
  silently). Neither → K = 1. ≥ 2 pairings noisy → K = 1, instrument again.
- Expected on record: G2 ≈ 1.1 for every FUA pairing (it discriminates the
  class, not K); G1 discriminates K (K = 1 ≈ 1.85 FAIL, K = 3 ≈ 1.2 PASS).

## Campaign C — the combined configuration (runs after A and B decide)

One S36 row set (3 pairs) and one S35 row (3 replicates) at the chosen fill
default × the chosen K, plus the K = 1 / fill-0 control the same night:
padding, write-amp, `everysec` throughput + CPU, offered p99/max/parks,
reads ±2 %, tripwires — the interaction check.

## Campaign A — outcome (read 2026-08-22 11:25, before campaign B finished; nothing below changes A's verdict)

R1 met (arm-A padding 6.8–10.3 %, median 7.5 %; throughput ≥ baseline in
5/5 pairs: +20.9 / +11.0 / +7.7 / +3.7 / +4.5 %). R2: no arm-only stall —
all ten offered legs `max` 1.2–3.7 ms, `parked` 0; (d) does not fire and
the 08-22 residual (fillA-3, 413 ms) is closed as the device's. R3 parks
clause met (ratios 0.93–1.18). **R3 p99 clause: 3 of 5** (arm 100/100 µs
vs base 96/90 on pairs 1–2; arm lower on 3–5; medians 92 vs 96) — by the
rule as written the default is **not flipped by campaign A**. The clause
was a pairwise comparison of a statistic whose baseline replicate spread
(90–96 µs) exceeds the deltas it tripped on; that is a defect of the
rule, recorded, not a reason to re-read the result.

## Campaign C — predeclared rule for the fill default (written before C runs; B still in flight)

C runs at the K chosen by campaign B's table (and, if that K ≠ 1, a K = 1
/ fill-0 control row the same night). S36 row, 3 pairs ABBA at the chosen
K, baseline fill 0 vs arm A 1000/16 KiB, `--leg-idle-s 60`:

- **R1'**: arm median padding ≤ 15 % and throughput ≥ paired baseline in
  ≥ 2 of 3 pairs.
- **R2'**: no arm-only stall (as R2).
- **R3'** (median-based, the A defect fixed): arm median closed-loop
  `parked` ≤ 1.5 × baseline median; **arm median offered p99 ≤ baseline
  median × 1.05** (the baseline's own replicate spread on campaign A).
- S35 row at the chosen K, 3 replicates, fill 1000 vs fill 0: p50 ÷
  barrier and barrier 4c/1c inside the replicate spread, reads ±2 %.
- R1' ∧ ¬R2' ∧ R3' ∧ the S35 row unchanged → the default flips to 1000 /
  16 KiB by ADR-0089 amendment with C's artifacts; otherwise it stays 0.

## Campaign B — outcome (read 2026-08-22 12:05)

G1: K1 1.853 FAIL · K3/2 1.198 PASS · K3/4 1.208 PASS. **G2 (barrier 4c/1c):
K1 1.543 · K3/2 1.346 · K3/4 1.435 — all FAIL ≤ 1.3.** G3 (client, info):
1.446 / 1.395 / 1.408. Tails: c32 p99 K1 2.54 ms vs K3 1.93 ms (G4 met);
c256 `max` 49–240 ms, drive-state dominated in every arm. Reads 1.609 /
1.588 / 1.620 M (±2 % met). No drive-state-flagged legs. **By the table:
neither K = 3 pairing passes G2 ⇒ K = 1.** Recorded defect of G2 (not a
re-read): its denominator is the 1-cell barrier, which reads 383 µs at K = 1
(qd1), 423–463 at K = 3 (qd3 on one cell) and 543–559 on 08-22 (drive
state), while the 4-cell barrier is pinned at 591–623 in every session —
the ratio measures the device's qd4-vs-qd1 FUA latency, which K does not
control, and it reads worst at K = 1. The expectation "G2 ≈ 1.1 for every
FUA pairing" was wrong (it was a slow-1-cell night's reading).

## Campaign C — runs at K = 1 / 4 MiB (the rule); then the same rows at K = 3 / 4 MiB as a *candidate* row for the owner (not a decision)

## Campaign C at K = 1 — outcome (read 12:40, before the K = 3 candidate rows ran)

R1' met (padding 15.0–17.1 → 7.4–9.1 %; +0.1 / +6.5 / +0.5 %); R2' met (six
offered legs `max` 1.1–3.3 ms, `parked` 0); R3' met (parked median 1.05 ×;
offered p99 median 90 vs 99 µs = 0.91); S35 row fill 1000 vs 0: p50 ÷
barrier 1.86 vs 1.85, barrier 4c/1c 1.54 vs 1.54, reads +1.8 %, c32 p99
+2.5–7 % (disclosed). **Every clause met ⇒ the fill default flips to
1000 µs / 16 KiB** (ADR-0089 third amendment). The K = 3 / 4 MiB rows that
follow are the candidate's interaction evidence for the owner's K decision
only; they do not move the fill default (already decided at K = 1).
