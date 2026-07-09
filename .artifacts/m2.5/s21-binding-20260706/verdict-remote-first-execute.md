# M2.5-S21 — remote-first EXECUTE ordering: **Rejected** (and the ADR-0027 discriminator answered)

- box: HomeLab i7-13700KF (designated reference box, ADR-0022 D1), 4 cells
  `--pin-start 4`, governor performance, turbo off, `perf_event_paranoid=-1`,
  env-check **OK** on every leg (tier: **reference-box, binding**), clean
  tree per leg (evidence committed between legs).
- lever: `--remote-first-execute` (infinityd → `LoopConfig.remote_first_execute`):
  a bounded `run_ready` slice + early FABRIC-OUT ahead of PARSE+EXECUTE, so
  reply-woken pumps' follow-on remote ops publish before the iteration's
  local bulk. Choreography pinned by `crates/inf-runtime/tests/reactor.rs`.
- legs: `legs.tsv` (this dir); source reports under `.artifacts/m0/`.

## Result — the lever loses every axis, zero replicate overlap

| arm | penalty vs all-local | natural ops/s | Dragonfly anchor |
|---|---|---|---|
| off (baseline, legs 2, 9) | **60.4 / 61.7 / 61.4 %** | 2.42–2.46 M | 1.44–1.48× |
| on (legs 5, 8) | **64.2 / 62.8 %** | 2.35–2.36 M | 1.36–1.37× |

Penalty +2.2 pp (median), natural throughput −4 %, anchor −0.08×. Same
shape as the early-flush rejection (`s21-campaign-20260706/`), larger
magnitude: publishing mid-iteration splits per-destination packs and rings
doorbells more often, and the batching it forfeits costs more than the
overlap it buys. Ships **off** (default `LoopConfig` unchanged, off-arm
byte-identical ordering — pinned by test).

## What this decides (ADR-0027 §Remaining #4)

Remote-first EXECUTE was the named behavioral discriminator between the two
surviving buckets of the 816 ns/op added remote cost: **overlap loss**
(a parked hop RTT) vs **per-op fabric CPU** (enqueue/dequeue, wakeup,
suspend/resume, reply handling). Issuing + publishing remote ops earlier
in the iteration did not reduce the penalty — it increased it. Overlap
loss is **not** the dominant term ⇒ the added cost is per-op fabric CPU.

Consequences:
- **Doorbell/batch shaping (ADR-0027 #5) is de-prioritized**: it attacks
  the same overlap-loss bucket this A/B just cleared.
- The perf cycle split (`s21-cycle-split-20260706/`, run in the same
  session) is the instrument that names the CPU components.

## Binding baseline (the row the ≤ 40 % staged gate reads)

- **Penalty 60.4 % binding** (5 replicates, natural 2.461 M vs all-local
  6.212 M ops/s), consistent with the prior binding 62.9 %
  (`.artifacts/m0/1783228176`) and dev-tier 57–59 %.
- Anchor **1.44× Dragonfly** (≥ 1.25 PASS), `sqes/submit` 18.06,
  loop p999 227 µs — tripwires green in-run.
- Note: binding all-local = **6.21 M ops/s > the 6 M pipelined node gate**
  — binding confirmation of the S09 shape finding (the pipelined gate is
  measured through the cross-cell penalty).
