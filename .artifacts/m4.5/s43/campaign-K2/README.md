# S43 campaign K2 — re-measurement of the ADR-0092 arm after batch 43 (batch 49, 2026-09-13)

Written **before** the first leg. Batch 43 (F-L01-04, ADR-0092 D1 rule 6
amendment) found that campaign K measured the arm with the hold inert
after every standalone episode; the ledger carried the fixed arm as
`Evidence-pending` since. This run is campaign K verbatim — same row,
same arms, same five alternated rounds, same predeclared rule — on the
committed batch-49 tree (`git=` on the log header). The rule below is
copied from campaign K's README unchanged; nothing is moved after the
fact (L4). Reading: the K row stands as the measurement of the code it
measured; K2 is the measurement of the shipped code.

---

# S43 campaign K — the binding run under the re-derived rule (ADR-0092, campaign-C amendment)

Written 2026-08-25 **before** the first leg. Reference box (ADR-0022 D1);
the tier is what `env-check` prints on each run's own header line.
Quiet-box rule: no compile, test, grep or edit runs on this box between
the campaign's header and footer lines (the S24 lesson — the operator's
own session activity moved same-binary A/A p99.9 by 2.4–5.9 %).

## Row and arms — campaign A's, unchanged

`gate-run m4.5 --only-s35 --reference-box --cells 4 --pin-start 0
--barrier-class flush --model-absent --replicates 1 --duration 10
--leg-idle-s 40 --data-root ~/bench-data/s43/data` — the S35 shape
(4-cell c32 `always` closed-loop leg, the c256 leg, the pipelined read
leg, then the 1-cell c32 leg) on the FLUSH class with the model absent
(every spawn `--device-probe off` by the harness rule — the report's own
note line proves the tier). Harness on cores 8,10,12,14; cells from 0.

Arms: **base** = `--flush-group-window-us 0`; **arm** = `--flush-group-
window-us 250`. **Five rounds, order alternated** (base/arm, arm/base,
base/arm, arm/base, base/arm) so drive-state drift lands on both.

Engine binary: the committed HEAD of the session (no cell-resident code
changed since `ada9a40`, campaign C's binary — the harness gained rows).
No `fstrim` (no sudo on this box) — disclosed; the 40 s idle is the
drive-state discipline available.

## The predeclared rule (ADR-0092 campaign-C amendment, verbatim)

Per replicate pair (arm ÷ base of the same round), then the count of
rounds on which every clause holds:

- **4-cell c32:** `acks_per_fsync` ≥ 1.4 ×, ops/s ≥ 1.2 ×, p50 ≤ 0.85 ×,
  p99 ≤ 1.0 ×;
- **1-cell c32:** `acks_per_fsync` ≥ 1.8 × and the arm's p50 ÷ barrier
  ≤ 1.3;
- **c256:** ops/s ≥ 1.0 × and p99 ≤ 1.1 ×;
- **reads:** ± 2 %;
- **engagement:** `waits_group` = 0 on every base leg, > 0 on every arm
  leg.

**Every clause on ≥ 4 of 5 rounds ⇒ the FLUSH class ships 250 µs** by
amendment (ADR-0092, the plan, `docs/compat-matrix.md` via its
generator). **Any median below its base ⇒ `Rejected`, knob removed.**
Neither ⇒ the default stays off, the record says which clause failed
on which rounds, and the exploratory ¾-target arm (ADR-0092) is the
next hypothesis, not a rider on this run.

The bars are the ones *derived from campaign C's readings*; they bind
this run and nothing else. Nothing here is moved after the fact (L4).

## Prediction on the record (from campaign C, not a claim)

4-cell c32 base ≈ 6.3–6.4 k ops/s at p50 ≈ 4.9 ms, group 4.3; arm ≈
7.9 k at ≈ 3.9 ms, group ≈ 6.4; 1-cell group 16 → 32, p50 ÷ barrier
≈ 1.1; c256 ≈ +12 %; reads ± 1 %.


## Result (written after the footer line, 2026-09-13 22:55; `campaign.log`, `s35-*/…/report.md`)

**Per round, arm ÷ base of the same round** (rounds: base-0/arm-0,
arm-1/base-1, base-2/arm-2, arm-3/base-3, base-4/arm-4; engine
`54bbf48`, `dirty=0`, env-check PASS on every leg, `performance`,
`no_turbo=1`, strays 0, no `fstrim` — no sudo, disclosed; 22:26–22:55):

- 4-cell c32: group **1.49** on every round (4.3 → 6.4; ≥ 1.4); ops/s
  **1.49 / 1.49 / 1.51 / 1.50 / 1.49** (6,364–6,418 → 9,451–9,624;
  ≥ 1.2); p50 **0.64–0.66** (4,927–4,991 → 3,151–3,271 µs; ≤ 0.85);
  p99 **0.71–0.73** (8,079–8,223 → 5,775–5,983 µs; ≤ 1.0).
- 1-cell c32: group **2.00 / 2.00 / 2.00 / 1.81 / 2.00** (16 → 32;
  round 3's base read 17.7; ≥ 1.8); arm p50 ÷ barrier **1.12–1.15**
  (≤ 1.3 — one window).
- c256: ops/s **1.28 / 1.37 / 1.28 / 4.08 / 1.36** (≥ 1.0 — round 3's
  base sat in the drive's bad mode: 16.6 k ops/s, p99 105 ms, barrier
  p99 59 ms); p99 **0.95 / 0.83 / 0.91 / 0.06 / 0.83** (≤ 1.1).
- reads: **0.990 / 0.992 / 0.983 / 0.850 / 0.980** (± 2 %) — round 3
  fails **as measured**: base-3's read leg ran over a keyspace its
  bad-mode c256 leg barely filled (nils 4.76 M vs 0.20–0.74 M on every
  other leg; 44 k frames vs 83–106 k), so its 1.85 M/s is a miss-heavy
  workload, not the read path. Median of per-round **0.983**, ratio of
  medians **0.983** — campaign K's median reading passes.
- engagement: `waits_group` **0** on every base leg; arm c32
  17.6–17.9 k, arm c256 17.4–17.5 k, arm 1c 9–43.

**Every clause on 4 of 5 rounds as measured** (round 3's read clause,
conditioned above), 5 of 5 on the median reading; **no median below
its base**. By the predeclared rule (≥ 4 of 5) **the 250 µs default
stands on the shipped code** — the fixed arm is measured, not
`Evidence-pending`. Against campaign K: base c32 6.36–6.42 k here vs
5.17–6.43 k (the drive stayed in its good state on every c32 leg), arm
9.45–9.62 k vs 7.82–9.58 k, ratios tighter (1.49–1.51 vs 1.23–2.03),
group 4.3 → 6.4 and 16 → 32 identical, `waits_group` 17.6–17.9 k vs
13.9–17.8 k. The batch-43 fix moved no reading on this row beyond the
instrument's spread: the episodes K carried inert were standalone-regime
(a barrier completing with nothing staged), and the S35 c32 closed loop
stages continuously. Prediction on the record (from K: base ≈ 6.4 k /
arm ≈ 7.8–9.6 k) — read 6.4 k / 9.5 k.
