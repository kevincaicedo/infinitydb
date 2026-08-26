# S34 campaign M1b — the read rows on a filled, quiesced namespace (campaign M1's confound removed)

Written 2026-08-26 **before** the first leg. Campaign M1's read legs
(`.artifacts/m4.5/s34/campaign-M/m1-s35-*`) compared a FLUSH-class read
leg serving 4–7 % misses (its slower write legs populated fewer of the
200 k keys) against a FUA-class one serving none, with 4 × more
background work behind the FUA arm's write legs — the −5 % it read is
not the read path. `gate-run m4.5 --only-s35 --read-leg-fill` (harness
commit of this session) fills every key once (32 conns × P16, 1 KiB) and
applies the 40 s idle before the read leg, on both arms, so the read leg
compares the read path alone (`nils` = 0 on both by construction).

## Row

`gate-run m4.5 --only-s35 --reference-box --cells 4 --pin-start 0
--replicates 1 --duration 10 --leg-idle-s 40 --read-leg-fill --data-root
~/bench-data/s34/data-M` × 6 runs: flush, fua, fua, flush, flush, fua
(arms as campaign M: `--barrier-class flush --model-absent` vs
`--barrier-class fua` with the reference model at the data root).
Harness on 8,10,12,14; cells from 0. The engine binary carries the S43
default flip (250 µs on the FLUSH class) — the read leg does not seal
frames, and the write legs are disclosed beside it, not compared.

## Predeclared rule (plan S34 AC)

The read-leg ops/s median under fua within ± 2 % of flush (per-run
values disclosed; `nils` must read 0 on every leg or the leg is
invalid). The read leg's own replicate spread on the record is ± 1–5 %
(campaign K's base legs spanned 5.2 %): if the six legs' spread within
an arm exceeds the band, the clause is reported with the instrument
floor named and stays `Evidence-pending (instrument)` — never closed on
a number the row cannot resolve.
