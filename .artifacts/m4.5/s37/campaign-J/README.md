# S37 campaign J — step 1: the cold-overwrite ceiling (bench-diagnostics arm)

Rules written before the run (2026-08-22). Plan S37 step 1: "a
`bench-diagnostics`-feature blind-overwrite arm (compiled out of every
shipping build) measures the ceiling on leg E (64/256) and leg C; < 15 %
throughput and < 20 % p99 ⇒ `Rejected`, closed with the artifact."

## Shape

`gate-run m4.5 --only-s37 --reference-box --cells 4 --pin-start 0
--replicates 3 --duration 20 --s37-keys 1000000 --leg-idle-s 10
--infinityd-bin target/release-diag/infinityd` — a separate
`--features bench-diagnostics` build of the same commit (`e123e83`) for
**both** arms (A = the shipping path, B = `--blind-overwrite-ceiling`),
so the only difference is the flag. Per leg: fresh server, tiered
`always` namespace (MEM-BUDGET 128 MB/cell, DISK-BUDGET 10 GB, TIER-IO-MODE
direct), 1 M × 1 KiB fill (≈ 250 MB/cell — beyond the budget), then
100 % SET closed-loop pipeline 1 at 64 and 256 conns, 20 s each. ABBA.

Leg C of the diagnosis (the RAM-resident tiered row) is not run: it has
no cold candidates by construction, so the ceiling is 1.0 there
(`tiering_cold_resolves` ≈ 0 — the instrument cannot engage; disclosed).

## What is decided

- `s37:ceiling_ops_x_c256` (B ÷ A throughput) and
  `s37:ceiling_p99_gain_x_c256` (A ÷ B p99), per-replicate medians; the
  64-conn pair reported beside them.
- **Rule (the plan's):** `ceiling_ops_x < 1.15` **and** `p99_gain_x <
  1.20` at both 64 and 256 ⇒ step 2 (shadow-slot reconciliation, 3 d +
  ADR) is **`Rejected`** and S37 closes with this artifact; otherwise
  step 2 opens (ADR-first) with the ceiling as its upper bound — never
  as a target.
- Validity: `blind_share_arm_b_c256 ≥ 0.1` (B's instrument engaged on at
  least a tenth of its SETs) and A's `cold_resolve_share` ≈ B's blind
  share (the same share of SETs met a cold candidate).

B is **unsound** (the cold record is orphaned — two candidates per key
until the orphan's file retires; dead-byte accounting off): its numbers
are an upper bound on a build that never ships, and are never quoted as
a product figure.
