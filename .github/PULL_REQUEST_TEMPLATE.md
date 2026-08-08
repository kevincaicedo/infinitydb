<!-- InfinityDB PR checklist (M2.5-S23). The full lifecycle is
     CONTRIBUTING.md + docs/INFINITY_STYLE.md (normative). Deviations are
     review rejects, not style preferences. -->

## What & why

<!-- One paragraph: the change, and the invariant/story/issue it serves. -->

## Author checklist

- [ ] `just check` green locally (fmt, dep-DAG, cell deny-list, fault-point
      + fsync greps, panic policy, safety inventory, clippy `-D warnings`,
      workspace tests)
- [ ] Tests land **with** the change (the DST scenario / fuzz target /
      regression test that guards it — not in a follow-up)
- [ ] Layer checks for touched areas: `just loom` (inf-fabric) ·
      `just compat` (reply bytes) · `just sim-smoke` (determinism) ·
      Miri (unsafe leaves) · fuzz smoke (decoders)
- [ ] **Performance work (L4):** hypothesis + target metric + workload stated
      *before* the change; A/B artifact attached (3–5 replicates, environment
      named); a losing A/B is recorded and the code **not merged**
- [ ] **Correctness-only** label if shipping without perf acceptance
- [ ] Frozen seam / dep-DAG edge / format change → the ADR merged **first**
- [ ] Unsafe touched → `// SAFETY:` on every block, crate `SAFETY.md`
      inventory updated (script-checked), Miri/Loom run

## Reviewer checklist

- [ ] **INFINITY_STYLE conformance affirmed** (`docs/INFINITY_STYLE.md`):
      invalid states unrepresentable · panics only for violated internal
      invariants · no hot-path allocation/dispatch/locks without an A/B
      artifact · bounded queues & explicit backpressure · decoders
      iterative + depth/size-bounded + fuzzed
- [ ] Evidence discipline holds (L10): no number or "faster/slower" claim
      in code, docs, or the PR description without its artifact
- [ ] Crate fences respected (dep-DAG green is necessary, not sufficient —
      check the *semantic* boundary: e.g. `inf-store` sees no sockets,
      `inf-log` knows no RESP)
