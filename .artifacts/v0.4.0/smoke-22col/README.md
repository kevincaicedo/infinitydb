# 5 min format smoke — the 22-column sampler (2026-08-13)

Mechanics only, 64 MiB budget, `soak-unified.sh 0.08`. Not evidence for
any gate; it exists to prove the sampler/verdict contract end-to-end
after `live_bytes` was added (the shake bundle
`soak-unified-20260812-2328/` carries 21 columns and predates it).

Confirmed:
- header and rows both carry **22 columns**, values populated
  (`dead_bytes=305605608 live_bytes=686817280 compact_idle_pressure=0`);
- the verdict renders the ratio line —
  *"peak dead ratio 30.79% vs the 50.0% compaction trigger"* — and takes
  the below-trigger branch rather than the engine alarm, which is the
  correct reading for a 5 min run.

Note the scale effect in miniature: at a 64 MiB budget the run reached a
30.79% dead ratio in five minutes, because the trigger scales with the
live dataset (687 MB here vs ~22 GB at 2048 MiB). Same reason attempt 4
never got close.
