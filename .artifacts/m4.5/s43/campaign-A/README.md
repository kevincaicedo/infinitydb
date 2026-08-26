# S43 campaign A — the FLUSH-class group hold (ADR-0092 D4), rules predeclared

Written 2026-08-25 **before** the first leg. Dev box = the reference box
(ADR-0022 D1); tier is what `env-check` says on the run's own header line.

## Row

`gate-run m4.5 --only-s35 --reference-box --cells 4 --pin-start 0
--barrier-class flush --replicates 1 --duration 10 --leg-idle-s 40
--data-root ~/bench-data/s43/data` — the S35 shape (4-cell c32 `always`
closed-loop leg, the c256 leg, the pipelined read leg, then a 1-cell c32
leg in the same replicate) on the **FLUSH class with the model absent**
(no `io-properties.toml` in the data root = the v6 "HEAD default" arm's
configuration; every spawn `--device-probe off` by the harness rule).

Arms: **base** = `--flush-group-window-us 0`; **arm** = `--flush-group-
window-us 250` (`GroupHoldConfig::ARM`). Three rounds, order alternated
(base/arm, arm/base, base/arm) so drive-state drift lands on both.
Exploratory 100 / 500 µs only if 250 reads inside the replicate spread.

## Predeclared rule (ADR-0092 D4)

On the 4-cell c32 leg, medians of the three per-arm reports:

- **H1** `acks_per_fsync` (the row's group column) arm ≥ 1.6 × base;
- **H2** ops/s arm ≥ 1.4 × base;
- **H3** p50 arm ≤ 0.75 × base;
- **H4** p99 arm ≤ 1.1 × base;
- **H5** `waits_group` (the leg's `frame_waits_group` delta) > 0 on
  every arm leg (the policy engaged) and = 0 on every base leg;
- **H6** the 1-cell c32 leg and the c256 leg: arm inside the base's
  replicate spread; **H7** read legs within ± 2 %.

**Falsifier:** ops/s below or p50 above the base at any of the three
shapes, or p99 > 1.1 × ⇒ `Rejected`, the knob and the policy removed.
A 32-conn win with a 256-conn loss = a shape result: off, recorded.

**Default rule:** every H clause on ≥ 2 of 3 rounds here is a
*dev-tier preview*; the binding form is 3–5 replicates on a quiet box
with `fstrim` (ADR-0092 D4) — this campaign decides whether that run is
worth scheduling, never the default.

Prediction on the record (from the v6 rows' arithmetic): base ≈ 4.5–5 k
ops/s at p50 ≈ 6 ms (p50 ÷ barrier ≈ 1.9); arm ≈ 8–9 k at ≈ 3.3 ms
(≈ 1.1–1.3 windows), `acks_per_fsync` ≈ 3.7 → ≈ 7–8.
