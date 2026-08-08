# M2.5-S21 final named lever — de-async dispatch (hypothesis before code)

Date: 2026-07-10 · owner: Kevin Caicedo · box: designated (i7-13700KF),
governor performance verified, turbo off, perf_event_paranoid=-1.

## Lever

The ADR-0030 D4 / ADR-0033 D4 carried lever: the pump's dispatch side
pays full async state-machine machinery per deferred command — the
construction + nested poll of the compiled `dispatch_one` → `send_apply`
futures (three `async fn` layers; a ~256 B argv-view array plus the
program-arm union state written into the pump future per command) —
although the send path suspends only on fabric-credit exhaustion, which
is rare at gate loads. The genuine per-op suspension is the *front-reply*
gate on the emit side, deliberately untouched here. The 2026-07-06 cycle
split attributed ~839 cyc/remote-op to plane pump/dispatch machinery;
the ADR-0030 levers cut the residual to ~580 cyc/op-mix.

Shape: `--deasync-dispatch` — the pump's dispatch phase first attempts
`dispatch_one_fast`, a plain synchronous function covering exactly the
hot arms:

- (a) single-owner remote command via sync `try_send_apply` (token draw
  + stage in one fabric borrow; the waiter registers only after a
  successful stage — the ADR-0030 publication-ordering argument holds:
  the peer cannot observe the op before FABRIC-OUT publishes it, so no
  reply can precede registration. A `NoCredit` first attempt abandons
  the drawn token — a skipped monotonic value, never registered, never
  paired (RTT queue only sees successful sends) — and falls back.)
- (b) the local mirror arm (local execute / conn-state / malformed) —
  extracted as one shared sync fn so both paths run literally the same
  code;
- (c) the restricted-subscriber early reply (already sync).

Everything else — plane pub/sub, INF.NS DDL, ckpt surface, named-ns
dispatch (including every `PendingReply::Durable` producer), scatter,
split/gather/fan multi-key arms, two-owner moves, argv wider than the
inline array, and any `NoCredit` — falls back to the unchanged async
`dispatch_one`. Off arm = today's shipped path exactly.

## Budget (from `s09-s21-cycle-split-20260706/` + ADR-0030/0033 results)

- Natural mix today (post ADR-0030+0033): ~4,280 cyc/op-mix dev;
  binding natural 2.88–2.96M @ 4 cells, all-local 7.98–8.22M, penalty
  ratio ~64%, anchor 1.67–1.74×.
- Addressable: the dispatch-side share of the ~580 cyc/op-mix plane
  bucket (future construction, nested poll dispatch, state moves,
  icache footprint of the giant poll fn). Estimate 250–400 cyc/op-mix
  removable on the covered arms — 100% of the natural-leg remote mix is
  single-owner GET/SET.
- Not addressed (named residuals): pump emit side (gate poll,
  `render_outcome`, per-reply conn write), gate map ops (3/op, already
  IntHasher), codec+mesh (~244), kernel (fixed-rate — amortizes).
- **Target: natural ≥ +4% binding, zero arm overlap** (expected +5–9%);
  all-local unchanged within noise (the pump is off that path); anchor
  ≥ 1.25× intact (expect improvement ∝ natural). Dev-tier perf
  re-attribution: the plane-machinery bucket (`pump`/`dispatch_one`/
  `send_apply` self cycles) shrinks ≥ 40% on the natural leg.
- Honesty: even a full win cannot reach the ≤ 40% staged gate by itself
  (that needs natural ≈ 4.9M at today's all-local). This is the last
  *named* lever — after this A/B the gate's disposition goes to its ADR
  either way; it does not silently close (ADR-0030 D4).

## Method

Dev-tier ABAB sanity (zero overlap required to proceed) → binding ABAB
`gate-run m0 --reference-box`, n=3/leg, anchor in-run, evidence
committed per leg, zero box work during legs (the ADR-0033 hygiene
lesson), `touch bins/infinityd/build.rs` before the campaign build (the
version-stamp lesson). The flag lands in gate-run's known-flags list in
the same commit (the ADR-0027 refused-leg lesson). Ships default-on only
on an Accepted binding verdict; the off arm is retained.

## Acceptance

Natural ≥ +4% binding with zero arm overlap; all-local within noise;
anchor ≥ 1.25× in-run; reply-byte semantics pinned by an e2e
(`deasync_dispatch_matches_pump_semantics`: remote/local interleave,
SELECT/HELLO conn-state barrier mid-queue, split DEL / MGET fallback
arms interleaved with fast-arm ops, scatter DBSIZE, expiry-on-read on
the remote path, QUIT). Losing A/B ⇒ recorded, not merged (M0-S14 rule).
