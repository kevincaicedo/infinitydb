# M2.5-S10 — M1 expiry-debt drain: root cause and disposition

date: 2026-07-07 · box: HomeLab (i7-13700KF, reference box per ADR-0022 D1) ·
kernel 7.0.0-27-generic · governor/EPP performance (env-check OK in-run) ·
tree clean at each measurement (revs named per leg)

## The carried failure

M1-S05 gate: 1M same-second expiry storm drains < 10 s with foreground
p99.9 < 2 ms in the same run. Carried disclosed failure (ADR-0022 D5):
11.34 s binding (`m1/1783228421`), reproduced in a tight 11.02–11.44 s band
across seven artifacts spanning M1 → M2.5 (1781312498 … 1783437942).

## Root cause: the band was the instrument, not the server

`inf-bench` m1 storm row (pre-fix `m1rows.rs`): fill 1M keys with
`PXAT = fill_start + 20 s`, run a **blocking 30 s** GET load "through the
storm", and only then start polling DBSIZE. The earliest observable drain
is therefore `fill_time + 30 s − 20 s ≈ 11.0–11.4 s` — exactly the historic
band, invariant to anything the server does. The `< 10 s` gate was
unmeasurable by this row as written; the drain itself completes during the
read window and was never observed.

Direct falsification on the pre-fix server (`4f9f113`, worktree build,
fully idle drain — the least favorable shape, no foreground traffic to keep
loops hot):

- 400k same-instant keys: **0 remaining at +1.0 s** after the deadline
  (2 replicates; `single-poll-legs.txt`).
- 1M same-instant keys (the gate shape): **0 remaining at +2.0 s**
  (2 replicates: +2019 ms, +2006 ms).
- Curve leg with 100 ms DBSIZE polling (`leg-baseline.txt`): 400k → 0 in
  645 ms, observed ~696k reaps/s. (Note: DBSIZE scatters to all cells and
  wakes them, so polled curves are an upper-bound shape — hence the
  single-poll protocol above for the honest idle bound.)

The pre-fix server clears the 1M gate shape with ≥ 5× margin. The
"~88.5k reaps/s structural clamp" inferred in earlier S10 notes was
arithmetic on the instrument floor (1M / 11.3 s), never an observation.

## Dispositions

- **H1 saturation-aware escalation (`0eaab11`) — Rejected, reverted.**
  Measured against the floor (11.32 s, `m1/1783402321`): no effect, and the
  pre-fix falsification above shows the defect it hypothesized (slow
  escalation ramp) does not produce a gate-relevant drain time.
- **H2 park veto under saturated backlog (`0286e05`) — Rejected, reverted.**
  Same: 11.28 s (`m1/1783402688`), 11.31 s (`m1/1783437942`, fresh
  stamp-verified binary — this run also eliminated the stale-binary
  alternative). Both hypotheses' mechanism story (drain gated on the park
  timeout at 64 fires/wake) is falsified by the ≥ 500k reaps/s idle drain
  measured pre-fix. Reverted per the L4 rule: no measured win, not merged.
- **Harness fix — Accepted (instrument correction, this commit):** the
  drain poller now runs concurrently with the read traffic on one
  persistent connection (5 ms cadence). The row measures the same quantity
  (deadline instant → DBSIZE 0 with reads running through the storm),
  gaining the ability to observe values below ~11 s.

## Binding gate rerun

With the corrected instrument (3 gate-run invocations = 3 replicates), see
the artifact paths in the review ledger entry; drain + the p99.9 co-gate
row cited there.

## Disclosures

- m1 gate-run reports do not record the server git rev in the header; this
  allowed two hypothesis "rejections" to be provisionally blamed on stale
  binaries before the floor was found. Tooling follow-up named in the
  ledger (report header should carry `infinityd --version`).
- The concurrent drain poller adds ~200 scattered DBSIZE ops/s during the
  read window — negligible against the foreground fleet; disclosed here.
- Read-only research subagents (file reads, no builds) were active on
  non-pinned cores during rerun windows; storm margin dwarfs any plausible
  interference.
