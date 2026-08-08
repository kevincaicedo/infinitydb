# M4-S05 mutation budget bench env snapshot
2026-07-18
git: b8b21c552cd0be230cd10a229da4095538590b11 (dirty files: 7 — S04/S05/S06 work in progress, disclosed)
kernel: 7.0.0-28-generic
cpu: 13th Gen Intel(R) Core(TM) i7-13700KF
governor(cpu4): performance
epp(cpu4): performance
pinned: taskset -c 4
tier: dev box (linux-devbox profile) — informational, non-citable (L10)
harness: benches/mutation.rs — custom (the `store`/`resolver` precedent);
isolates the address-path delta with identical record work on both sides.
The command-level SET row joins the campaign when tiered namespaces are
command-wired (S22/S24).

## medians of 3 replicates (tiered − arena, ns/op)

| row | cache-hot (32K) | miss-bound (1M) | §4.1 verdict (dev-tier) |
|-----|-----------------|-----------------|-------------------------|
| in-place rewrite (exact-fit SET lane) | +0.01 | +0.01 | PASS — indistinguishable |
| scalar patch (JSON.NUMINCRBY lane)    | +0.09 | +0.17 | PASS — indistinguishable |
| relocate (size-changing SET lane)     | +27.68 | +37.29 | recorded finding, see below |

Absolute costs (median rep): in-place 4.7/14.5 ns · patch 3.0/12.6 ns ·
relocate arena 16.1/55.1 ns vs tiered 43.8/92.1 ns.

## Relocation-lane finding (root cause + pre-named A/B)

Copy-to-tail always writes fresh ring bytes; after the first ring cycle
those pages were decommitted via `MADV_DONTNEED` + `PROT_NONE`
(ADR-0052 D3 — the honest-RSS choice), so every recycled 4 KiB page
soft-faults on first touch: ≈1–2 µs per fault amortized over ~32 records
per page ≈ +30–40 ns/op — the measured delta. The M3 arena baseline
recycles freed allocations from its free lists (resident pages, no
refault), a fundamentally different memory lifecycle.

This is the exact trade ADR-0052 D3 pre-named. A/B candidates owned by
S22 (with this artifact as the baseline):
1. `MADV_FREE` decommit (pages stay resident until global reclaim — RSS
   honesty cost, rejected as default in the ADR).
2. `MADV_POPULATE_WRITE` at commit time — moves the fault cost into the
   batched commit slice (L3: pay per batch, not per op) with no RSS
   honesty cost. Hypothesis: removes most of the delta.

The §4.1 SET/NUMINCRBY budget rows are the in-place lanes — PASS at dev
tier. The relocation lane is S06's copy-to-tail mechanism working as
designed (the hot/cold filter); its end-to-end cost is gated by the S22
YCSB rows and the week-4 risk gate, not by this micro row.
