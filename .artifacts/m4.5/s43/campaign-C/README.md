# S43 campaign C — the twice-corrected arm, same predeclared rule (ADR-0092 D4)

Written 2026-08-25 before the first leg. Campaign B (`../campaign-B/`)
engaged the hold and moved nothing; `round_target` ≈ 1.5 per cell showed
the K = 1 FLUSH cadence seals arrivals as plain frames while the slot is
busy, so the population is only visible at the barrier's completion and
a barrier completing with nothing pending issues a standalone fdatasync
the frame hold never saw. This engine (commit after `157a5ac`) measures
the target at `on_synced` (acked + assigned in flight), compares it to
the uncovered count, and holds the standalone path by the same rule.

Row, arms, order, rule H1–H7 and the falsifier: **campaign A's README,
unchanged**. Prediction (unchanged): arm `acks/fsync` ≈ 7–8, ops/s ≈
1.6–1.9 × base, p50 ≈ 0.55–0.65 ×, `waits_group` > 0 on arm legs only,
`round_target` ≈ 8 per cell (≈ 32 summed). If this campaign also reads
flat, the arm is `Rejected` as non-effective on this mechanism and the
knob removed (ADR-0092 D4's falsifier applies to a non-engaging arm as
much as to a losing one).
