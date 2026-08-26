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
