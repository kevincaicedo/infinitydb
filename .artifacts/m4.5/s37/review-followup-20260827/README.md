# M4.5-S37 review follow-up (2026-08-27) — DST sweeps on the amended tree

The review of engine `00657b6` found four defects in shadow-slot
reconciliation (ADR-0093); this directory holds the DST rows of the
amended proof set (ADR-0093 §Amendment A7). Both scenarios run with the
`hash64` collision oracle — real colliding keys through the real paths.

Tree: the S37 review-follow-up commit (see `reviews/infinity-m4.5-indexes-query.md`,
entry "review of `00657b6`"). Binary: `cargo build --release --bin inf-sim`.

## m4-tiered (node level, both arms)

    inf-sim --scenario m4-tiered --seed 0x5EED0000 --sweep 1000 --shard I/8 --out m4-tiered

Result: 1 000 seeds, **0 violations**; 750 arm seeds; 41 995 tickets /
41 984 same-key; phase 6c (crafted colliding pairs through the plane):
6 000 tickets, 3 000 `Collision` verdicts, `GET`/`DBSIZE`/`SCAN`/`DEL`
expectations exact on both arms; the knob witnessed on every cell.
Manifest counters `shadow_*` (other than the `collide_*` ones) are
one-cell `INFO` scrapes — coverage, not totals.

## m4-recovery (store level, chained lives)

    inf-sim --scenario m4-recovery --seed 0x5EED0000 --sweep 1000 --shard I/8 --out m4-recovery

Result: 1 000 seeds, **0 violations**; 78 676 tickets opened, 17 405 open
across a cut, 21 279 re-formed by recovery, 61 232 same-key and 182
`Collision` verdicts (each checked against the decoded keys), 154 184 ops
on the colliding pairs, 83 rebuilt slots settled at boot by full key,
4 000 `DBSIZE`-drain exactness checks.

Per-shard manifests and per-seed result lines are beside this file;
`shard-*.log` is each shard's stdout/stderr.
