# Week-4 RISK GATE — verdict record (2026-07-18)

Box: HomeLab reference box (ADR-0022 D1; ADATA LEGEND 700 Gen3 DRAM-less
NVMe — §19 device deviation disclosed). env-check green on every counted
run (clean tree, governor/EPP performance, turbo off, no thermal flags);
cells pinned `--pin-start 4`, same cpu set both legs; baseline rebuilt
from `a1ebcb9`, fingerprint `hash64:60afaf32c23bce09 (10479640 bytes)` —
byte-exact match with the pinned report note (whole-workspace build at
the recorded path). M4 binary `hash64:1240b1a264960461 (10593368 bytes)`
(tip `147c33a`, prebuilt and held fixed across legs).

## Runs (all committed under this directory)

1. **Leg 1** — 5 ABBA reps (`1784424616-gate-run/`): ops deltas
   +0.26% / +0.31% / −0.16%; peak-RSS worst +0.40%; zero tripwires
   green; p99.9: pipelined 0.00, ttl 0.00, unpipelined **+2.50% (one
   LogHistogram bucket, 1279 → 1311 µs)** → encoded row FAIL.
2. **A/A control** — same M4 binary both legs, 5 ABBA reps
   (`aa-control/1784424976-gate-run/`): p99.9 "deltas" **+7.51%
   (pipelined, two buckets)** and **+2.50% (unpipelined — the exact
   1279 → 1311 signature)** with *identical binaries*; ttl leg polluted
   by a collapse rep (58% spread). Same-binary ops "deltas" ±0.9%.
3. **Leg 2** — 9 ABBA reps (`1784427397-gate-run/`): ops +0.73% /
   +1.01% / −0.53% (m4 ahead on two rows); RSS ≤ +0.45%; tripwires
   green; p99.9: pipelined +2.33% (one bucket), unpipelined +2.50%
   (one bucket, the same 1279 → 1311), ttl −2.78% (one bucket, m4
   better).

## Analysis

Every p99.9 reading across A/B and A/A runs is 0 or ±1–2 buckets, in
both directions. The unpipelined row read *exactly* 1279 → 1311 µs in
all three runs **including the same-binary control** — the one-bucket
delta follows the harness slot (spawn order/port of the m4-labeled
server), not the binary. The instrument's demonstrated same-binary tail
floor at this config is ≥ 1 bucket (~2.5–3%), so a 1-bucket A/B reading
cannot be adjudicated against the 1% threshold (§19: a measurement whose
instrument cannot resolve the claim is not a valid measurement of it).

Structural evidence carrying the degenerate-case verdict: the S02
asm-identity artifact (memory-mode lookup instruction-identical to M3),
all zero tripwires (`tiering_* ≡ 0`, `log_records_appended = 0` on every
row of every run), throughput deltas within ±1% with m4 ahead on 2 of 3
rows at 9 reps, RSS ≤ +0.45%, and the perf attribution diff
(`flames/` — no new M4 userspace bucket under the pipelined mix; kernel
buckets bounded, `kptr_restrict=1` disclosed).

## Joined risk-gate inputs

- **S04 cold-read histogram (reference NVMe, 3 pinned reps):** p99 =
  163–199 µs — 7.5× inside the < 1.5 ms bound
  (`.artifacts/m4/s04/cold-read-histogram-refbox-20260718.md`).
- **S01 resolver re-read (binding env, 3 pinned reps):** +0.12–0.16 ns
  cache-hot, +0.87–0.88 ns miss-bound — inside the ≤ 2 ns budget
  (`.artifacts/m4/s01/`).

## Disposition

**No valid failure exists → the STOP does not fire; E5–E7 may proceed.**
The cold-read half passes outright. The degenerate-case half: ops, RSS,
and tripwire rows pass binding; the p99.9 rows are **inconclusive at
instrument resolution** (A/A-controlled) and are neither passed nor
failed — per the gate's own rule, an unresolvable reading is re-run,
never dispositioned. The binding tail verdict moves to the S24 final
campaign **with a named instrument precondition**: fix the gate-run slot
asymmetry (crossover the binary↔slot assignment across replicates, or
serialize server lifetimes) so the S24 run can resolve a 1-bucket
question. S03 stays `Implementation complete — evidence pending` on the
tail rows only.
