# M3-S01/S02 — idoc v1 first measurements (2026-07-10)

**Tier: DEV** (i7-13700KF, pinned P-cores, performance governor — per the
linux-devbox profile). No public claim cites this directory; the S25
reference-box campaign re-runs the gate-grade rows (L10).

## Density (tests/density.rs, seed 0x1D0C2026, corpus-shape stand-in)
See density.txt. Aggregate: **1.108× msgpack** (budget ≤ 1.15) ·
**0.764× JSON text** (budget ≤ 0.85). Per-shape worst cases, visible by
design: small-200B 0.905× text (tiny docs near parity — 8 B header ≈ 4%);
wide-array 1.119× msgpack (fixarray 1 B vs our u24 container lens — the
ADR-0036 D3 compaction-pass fallback stays reserved, not triggered).

## Traversal criterion (benches/traverse.rs, adversarial key placement:
## target key 7th of 8 per level, long string before it)
- depth4_leaf_fetch/tape:  **218.4 ns** (budget ≤ 200 ns — **9% over** on
  this placement; §4.1 budget is Evidence-pending toward S25)
- depth4_leaf_fetch/arena: 197.3 ns
- A/B history: fused key scan in ObjRef::get **Accepted** (−7.0%);
  inline value-skip **Rejected** (+4.6%, branch bloat — reverted, noted
  in code).
- Named levers if S25 confirms the miss on the real S20 gate shape:
  realistic key placement (this row is worst-case by construction),
  S20 batch prefetch (the budget's miss-bound regime), SIMD key scan in
  the S05 kernel family. Context: the end-to-end gate (JSON.GET ≤ 1.5×
  GET) carries ~600–800 ns of headroom; 18 ns is ~3% of it.

## Round-trip AC
PROPTEST_CASES=1000000 release run of tests/roundtrip.rs: all 3
properties green at 10⁶ cases each (tape identity; arena identity +
freeze(morph(t)) == t + accounting reconciliation; fragment ≡ body).
