# M4.5-S08 — predicate VM eval budget + allocation-free artifacts

**Tier: dev — non-citable.** i7-13700KF (hybrid; bench pinned
`taskset -c 4`, a P-core), governor `performance`, EPP `performance`,
load average < 0.5 at run time. `cargo bench -p inf-query` (criterion,
100 samples/row); 3 replicates (`predicate-eval-rep{1,2,3}.txt`),
spread < 0.5% on every row. Working tree of the S08 session (see the
ledger entry for the commit). The §4.1 budget is **≥ 5M predicate
evals/s/core on the 1 KiB corpus shape (≤ 200 ns/eval),
allocation-free**. Docs are the shared `inf-bench` corpus shapes
(`gate-1KiB`, `medium-2KiB`); the timed loop includes root-cursor
derivation and the full fuel accounting.

| row (median of medians) | rep1 | rep2 | rep3 | evals/s | budget verdict |
|---|---|---|---|---|---|
| residual_two_leaf (`score >= 0.5 AND kind = 'gate'`) — **the budget row** | 177.44 | 177.55 | 177.46 | **5.63M** | ✓ ≤ 200 ns |
| residual_short_circuit (first conjunct false, sibling skipped) | 104.61 | 104.62 | 104.59 | 9.56M | ✓ |
| residual_in_list (3 members, hit on last) | 88.84 | 88.85 | 88.76 | 11.3M | ✓ |
| residual_deep_path (`$.child.child.child.score`, depth-4 chain) | 459.38 | 459.08 | 460.69 | 2.18M | disclosed¹ |
| residual_multi_match (`$.items[*].qty > 90`, 12 elements, 2 KiB doc) | 1716.6 | 1716.5 | 1721.2 | 583k | disclosed¹ |

¹ The budget names the 1 KiB corpus's *typical residual* (the two-leaf
row). Depth-4 chains pay four sequential object scans (tape entry
decode + value skip — `ObjIter::next` is the cost, not VM overhead)
and a 12-element wildcard pays one node visit per element by design
(fuel counts the walk — the S08 pitfall row); both scale with the work
they do, not a fixed regression.

**Allocation-free evidence** (`perf-eval-profile.txt`): pinned perf
record over the 1 KiB rows — zero allocator symbols in the eval loop
(top symbols: the VM loop, `ObjIter::next`, the two `read_op`s,
`read_value`, `relation`). The standing regression gate is
`crates/inf-query/tests/predicate_alloc.rs`: thread-local
`CountingAllocator` delta over 50,000 evals (5 predicate shapes incl.
wildcard multi-match) == 0 after warm-up.

**Optimization history (L4):** first cut measured 281 ns on the budget
row; the profile attributed ~18% to the general walk's per-eval frame
fill (`VisitFrames::new`). Disposition **Revised**: `eval_visit` now
routes `Child`/`Index` chains through the established ADR-0043 D1
simple-step lane (no frames, no per-item op re-decode; identical
node accounting, pinned by the `path_eval` congruence proptests) and
constructs frames only for general walks — 281 → 177 ns with no
semantic change. No losing A/Bs to record; no alternative designs were
benched against each other.
