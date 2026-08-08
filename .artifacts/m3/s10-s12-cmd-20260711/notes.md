# M3-S10/S11/S12 — command-level rows + program-cache evidence (2026-07-11)

Dev-tier (i7-13700KF, turbo off = 3.4 GHz, governor performance, pinned
`taskset -c 4`; env.txt). Execute-level harness (`benches/json_cmd.rs`):
parse → registry → handler → store → RESP bytes; no reactor/wire. NOT a
public claim (L10); S25 re-runs the wire-level gates on the reference box.

## Rows (criterion p50, 3 replicates — raw/r1..r3.txt)

| row (1 KiB gate shape)             | r1       | r2       | r3       |
|------------------------------------|----------|----------|----------|
| SET (plain, equal-size value)      | 53.5 ns  | 53.5 ns  | 53.5 ns  |
| JSON.SET (root)                    | 1.356 µs | 1.364 µs | 1.357 µs |
| GET (plain)                        | 40.4 ns  | 40.4 ns  | 40.4 ns  |
| JSON.GET $.child×4.id (depth-4)    | 1.517 µs | 1.520 µs | 1.530 µs |
| JSON.GET (root, full serialize)    | 2.750 µs | 2.830 µs | 2.755 µs |
| JSON.NUMINCRBY $.id (splice v1)    | 531 ns   | 544 ns   | 529 ns   |

## S10 §4.1 row — PASSES

- Hot-read hit rate **1.0000** (≈ 35 M hits / 4 misses per run, the 4 =
  one compile per distinct path; 0 evictions; 8 498 B counted).
- **Zero compiler symbols** in the read profile: `grep -cE
  'path::parse|program::encode|PathProgram::from_bytes|parse_ast'` over
  the full perf report = **0** (raw/perf-get-path-report.txt; 20 286
  samples). `ProgramCache::get_or_compile` (the hit path: hash + probe +
  memcmp + LRU touch) reads 2.0%.

## S11 denominator — the S05 decision point's input

JSON.SET dispatch-level delta over SET: 1.356 − 0.053 = **1.303 µs** ≈
the S05 slice-2 parse cost (1.264 µs) + ~40 ns dispatch/store. Under the
measured-M2.5 wire-SET anchor this reproduces the S05 projection:
**JSON.SET ≈ 58% of SET** vs the ≥ 70% gate. Safe-Rust parse levers are
exhausted (S05 slice 3); the named ADR question — unsafe emit core vs
evidence-based gate re-derivation — is now live for the S25 window with
its denominator confirmed at the dispatch level.

## Early warning — JSON.GET path-read cost (fired here, weeks before S25)

Path-read delta over GET: 1.517 − 0.040 = **1.477 µs**; against wire GET
p50 (≈ 1.7–1.8 µs M2.5 rows) the `JSON.GET ≤ 1.5× GET` gate projects to
≈ **1.8×** at dev tier — currently missing. The profile is decisive and
the levers were already named:

- **55% in the evaluator's Child entry walk** (`ObjIter::next` 41.2% +
  `read_value` 13.7%): the S09 ledger recorded "the evaluator's `Child`
  lookup walks entries with ordinals instead of `ObjRef::get`'s fused
  scan — a potential S10-time lever if read profiles show it". The read
  profile now shows it: a fused key scan that counts ordinals attacks
  the whole bucket (the S02 traversal slice cut the same shape 24–38%).
- **~7% allocator traffic** (`Matches` vectors, program clone, `Reply`
  tree, eval stacks): poolable per-cell scratch — same discipline as the
  parser's recycled buffers.
- `eval::run`/`resolve`/`read_op` ≈ 18%: shrinks with both of the above.

Disposition: recorded as the binding pre-S25 optimization slice (L4:
each lever its own A/B); not attempted in this story — S10–S12 are
correctness + surface, and the plan's early-warning discipline is
exactly this row existing now instead of in gate week.

## Informational

- JSON.NUMINCRBY at 531 ns on the v1 splice backend (freeze + eval +
  plan/apply + re-tier) — S16's in-place engine inherits this baseline
  for its ≤ 1.3× INCR budget row.
- JSON.GET root (full 1 KiB reply serialize) ≈ 2.75 µs — the serializer
  walk dominates; only the path row binds the S25 gate shape.
