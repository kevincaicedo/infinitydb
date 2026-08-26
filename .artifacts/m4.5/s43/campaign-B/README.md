# S43 campaign B — the corrected arm, same predeclared rule (ADR-0092 D4)

Written 2026-08-25 before the first leg. Campaign A (`../campaign-A/`)
ran the arm at `ab261e0` and found it **non-engaging**: D1 rule 5
compared the pending count to the last frame's records, and in the
steady alternation both are n / 2. `157a5ac` sets the round target to the
population a completion reveals (the frame's records + the records
pending at its `LogWritten`) and adds `waits_group` / `round_target` to
the S35 leg line so H5 is readable.

Row, arms, order, and the rule H1–H7 + falsifier: **exactly campaign A's
README** (unchanged, on purpose). Engine `157a5ac`. Prediction on the
record (unchanged): base ≈ 6.4 k ops/s at p50 ≈ 4.9 ms, `acks/fsync`
4.3 (campaign A's base); arm `acks/fsync` ≈ 7–8, ops/s ≈ 1.6–1.9 ×, p50
≈ 0.55–0.65 ×, `waits_group` > 0 on every arm leg and 0 on every base
leg, `round_target` ≈ 8 × 4 cells.
