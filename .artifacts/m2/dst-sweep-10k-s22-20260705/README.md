# M2-S22 release DST sweep — 10,000 seeds, 0 violations (§6 `dst_sweep` gate)

Date 2026-07-05 · kernel 7.0.0-27-generic · governor `performance` · git `4cc278b` (clean; regenerated after the S22 group-commit fix — one durability fsync in flight)
Box: HomeLab i7-13700KF (the user-designated M2 reference box; sweep is CPU-only — box class is immaterial to this gate).

## Command (reproducible per seed — L7)

```
for i in 0 1 2 3 4 5 6 7; do
  ./target/release/inf-sim --scenario m2-durable --sweep 10000 --seed 0xD5EE0000 \
    --shard "$i/8" --out <this dir> &
done; wait
```

## Result

- **10,000 seeds · 0 durability-oracle violations** (the §6 gate value).
- 133 legal taxonomy refusals (1.33%) — reorder physics producing interior-corruption
  states; every refusal survival-audited clean against the surviving image
  (ADR-0021; §8.2 binds survival, not serving). Identical seed-for-seed to the
  S19 dev sweep (`dst-sweep-10k-20260703`) — the sweep is deterministic.
- Any seed replays byte-identically: `inf-sim --scenario m2-durable --seed <seed>`.

Files: `manifest-shard-{0..7}.txt` (per-shard summary), `results-shard-{0..7}.txt`
(10,000 per-seed result lines).
