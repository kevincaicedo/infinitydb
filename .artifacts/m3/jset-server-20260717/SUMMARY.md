# M3-S25 server-side json_set slice — verdict (dev tier, 2026-07-17)

Environment: `env.txt`. Baseline binary = HEAD (`infinityd-base`),
lever binary = HEAD + L1 (`infinityd-l1`). Nothing citable (dev tier,
L10). Companion: `PROFILE.md` (the opening attribution).

## What the profile established (PROFILE.md)

- Generator saturation ruled out (thread/cpuset matrix); the §19
  validity concern raised by the +0.02pp stall is retired — the stall
  was cross-leg noise, and the wire delta decomposes as
  **parse ≈ 797 ns + doc plumbing ≈ 61 ns + diffuse ≈ 140 ns** on a
  measured 1.000 µs/op-cell JSON.SET−SET difference.
- Shared wire/dispatch machinery is per-op equal between lanes; there
  is no hidden server-side bottleneck. The one structural asymmetry:
  every JSON.SET overwrite paid doc-arena alloc + copy + old-blob free
  + release accounting, where SET reuses its record slot in place.

## L1 — in-place tape-blob overwrite (landed)

Same-size-class root overwrite of an existing tape-blob document now
rewrites the blob bytes in place (`resize_in_place`, record written
through the carrying path before blob bytes change — the ADR-0037 D3
refusal ordering preserved; class-local resize is reversible on abort).
No format, threshold, or seam change; `write_record_carrying` became
`pub(crate)`.

**Mechanism proof (perf, jset leg, l1 binary):** `DocStore::release`
eliminated (0.29% → absent), `json_write_value` 1.05% → 0.28%, new
costs `resize_in_place` 0.25% + `payload_of` 0.14% → **net ≈ −0.67pp
of cell time ≈ −21 ns/op**. Instructions/op flat (30.1k → 30.4k, the
guard replaces the freed work), so the win is churn/locality, not
instruction count.

**Wire A/B (ABAB, 4 fresh-server arms, per-lane discarded warm-up leg,
3 reps each — `SUMMARY-ab.txt`):**

| Arm | SET ops/s | JSON.SET ops/s | ratio |
|---|---|---|---|
| base (6 reps) | 1,832,977 (rsd 1.9%) | 1,226,508 (rsd 5.1%) | 0.6691 |
| L1 (6 reps) | 1,772,404 (rsd 3.1%) | 1,257,221 (rsd 2.8%) | **0.7093** |

**Honest bounds:** jset +2.5% / set −3.3% across binaries; the set-side
move is layout-class noise (L1 never executes on the SET path; the
counters session `pstat-*` shows set instructions/op −1.4% *and*
throughput +2.2% on l1 — session-scale noise exceeds the effect). The
symbol-level −21 ns/op (+0.7% jset) is the defensible physical effect;
the A/B's ratio crossing is partly favorable noise. Conclusion: **the
dev-tier gate now reads on the 0.70 line (0.67–0.71 across sessions),
inside the cross-session drift band. The reference-box campaign owns
the verdict (L10) — no dev number is citable either way.**

## Rejected / not pursued (recorded)

- **L2 root-program fast slot:** rejected before code — the cache's MRU
  head path already serves `$` with one length/memcmp; the RefCell
  borrow is the only remaining cost (~2–3 ns). No experiment justified.
- **Parse-into-blob copy elision:** output length unknown before parse;
  would replumb the emit region for a ≤ 40 ns bound. Not priced.

## Also fixed in this slice

- `ns::tests::duplicate_id_is_a_caller_bug` gated `debug_assertions`
  (pre-existing: a should-panic on a debug_assert fails under
  `--release` test runs; found running the lib suite in release).
- New test: `doc_records::same_class_blob_overwrite_rewrites_in_place`
  (in-place path: accounting exact, one version bump, TTL kept,
  cross-class fallback relocates and releases).

## Validation

- `cargo test -p inf-store` (lib + doc_records + doc_storm +
  doc_attribution + mutation_error_atomicity + doc_durability) green.
- `just check` green; `cargo deny check` green.
- Not re-run, with reasons: parse/emit goldens + differential (parser
  untouched); RSS legs (placement/residency untouched; blob reuse can
  only reduce arena churn); DST campaign tier (record/replay semantics
  untouched — the store commit path is byte-identical in outcome, and
  the doc suites assert exactness).

## Disclosures

- Collapsed jset reps (−6…−15% for a full 10 s leg) appear ~1 in 6
  across both binaries; server logs clean; desktop session live on the
  box. Watch on the reference box; if it reproduces there, it becomes
  its own investigation.
- Cross-binary layout noise band (±3%) documented in earlier slices
  applies to every wire number above.
