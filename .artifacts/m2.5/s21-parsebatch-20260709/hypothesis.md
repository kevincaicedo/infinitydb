# M2.5-S21 next slice — parse-batch staged prefetch (hypothesis before code)

Date: 2026-07-09 · owner: Kevin Caicedo · box: designated (i7-13700KF),
governor performance verified, perf_event_paranoid=-1.

## Lever

The ADR-0029 second lever / ADR-0030 D3 named opening: adopt the
fabric-apply staged-prefetch shape (ADR-0030 lever 3) on the **client
parse loop's local fast path** — the batch ADR-0005's demoted pipeline
never had exists there naturally (`cmds_per_iter` 185 local; the recv
buffer at the P=16 gate shape yields ~16-command sub-batches per
connection buffer, at or above the core's ~10–16 outstanding-miss MLP
width, so per-buffer staging captures the hiding).

Shape: stage (flat argv copy via `stage_argv_block` + `hash_key` +
phase-1 `prefetch`) → one `probe_prefetch` pass over the staged batch →
execute in parse order. Barriers flush the stage (defer-to-pump,
LOADING error, protocol error, QUIT); correctness never depends on
stage-time hints (execute reads `conn_cx` live: SELECT mid-batch only
mis-hints the prefetch db, never the execution db).

## Budget (from `.artifacts/m2.5/s09-s21-cycle-split-20260706/`)

- Local leg today: 2,122 cyc/op; store bucket **848** (10.2 LLC
  misses/op, `resolve_hashed` 40% self — the walk is scalar).
- Fabric-apply precedent: the same shape on a ~39-op drain batch cut
  the owner-side store bucket **~69%** (739 → 227 cyc/op-mix) and won
  +13.3% dev / +7.3% binding on the natural row.
- Added cost: flat copy of ~32–96 B argv (~20–40 cyc) + stage
  bookkeeping ≤ ~100 cyc/op — the fabric A/B already proved this trade
  in-kind.
- **Target: store 848 → ≤ 400 cyc/op ⇒ all-local ≥ +15% binding**
  (6.6–6.8M → ≥ 7.6M @ 4 cells). Natural leg: the origin-local quarter
  gains; absolute natural must not regress; anchor ≥ 1.25× intact.
- Honesty: this lever serves the ADR-0029 carried ≥ 6M row (S19 re-read,
  incl. the 8-cell spec shape) more than the ≤ 40% penalty ratio — the
  ratio may even *rise* while both absolute rows improve (all-local
  grows faster). The penalty's own lever (de-async dispatch, ~580
  cyc/op-mix) is **carried, not attempted in this slice** — one lever,
  one A/B (L4).

## Method

Dev-tier ABAB sanity (zero-overlap required to proceed) → binding ABA
gate-run m0 `--reference-box`, n=3/leg, anchor in-run, evidence
committed per leg; plus one 8-cell all-local leg per arm (the ADR-0029
spec shape). Flag `--parse-batch-prefetch` / `--no-parse-batch-prefetch`
(gate-run known-flags list extended in the same commit — the S21
refused-leg lesson). Ships default-on only on an Accepted binding
verdict (ADR-0030 D2 precedent); off arm retained.

## Acceptance

All-local ≥ +10% binding, zero arm overlap; natural row within noise or
better; anchor ≥ 1.25× in-run; reply-byte semantics pinned by an e2e
test (parse-batch on/off byte-identical across SELECT/QUIT/DEBUG
SLEEP/LOADING/pipeline interleavings). Losing A/B ⇒ recorded, not
merged (M0-S14 rule).
