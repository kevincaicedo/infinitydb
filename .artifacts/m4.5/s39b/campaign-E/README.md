# S39b campaign E — the 2-slot pool (falsifier (a)'s named next hypothesis), rules written before the run

Campaign D's first pair read the arm's warmed zero-fill share at **0.326** with
`recycle_misses` 5 per cell against 6 recycles — the 1-slot pool starves at
this shape (ADR-0090 D6 falsifier (a): "> 0.3 → the slot starves → the bound
is wrong, not the mechanism: a 2-slot pool is the next hypothesis, by
amendment, not a silent retune"). This campaign runs that hypothesis the same
night, same box, same rules as D, with the arm at `--segment-recycle-slots 2`
and the same `--no-segment-recycle` baseline (interleaved, 3 replicates).

Predeclared: the same binding gates as D. If the 2-slot arm passes every
binding gate ⇒ the default becomes `recycle_slots = 2` by a fourth ADR-0090
amendment carrying both campaigns' artifacts (disk attribution ≤ 2 × segment
per cell, disclosed); if the 2-slot arm also reads > 0.3 ⇒ the bound is not
the mechanism's limit either — recycling ships default **off** with both rows
recorded and the pool's feed timing (truncation phase vs prealloc) becomes the
next story, never a third arm the same night. The 1-slot row's other gates are
reported as measured regardless.

## Amendment before the run (engine `a2a96d3`, rules added before E started)

- E runs on `a2a96d3` (D ran on `16b4dd5`): the diff touches counters
  (`recycle_pool_full`), the accept path (`--conn-default-ns`, off here), the
  harness and docs — nothing on the write, truncation or recovery paths.
  Each campaign's baseline is its own same-binary control.
- Recovery is timed twice per leg: immediately after the read leg
  (informational) and after the 40 s drive-state idle (**binding**, the
  ≤ 1.05 gate) — campaign D's 1.25 on the immediate boot was attributed to
  drive state, not the reader (residue scan +4 ms per 256 MiB, unlink
  0.1 ms; `campaign-D/VERDICT.md`), and the instrument change is made before
  this run, not after reading it.
