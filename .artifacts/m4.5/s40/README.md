# S40 — the comparator at the S27 D5 offered rate (rules written before the run)

Engine `9c31a18` (= `a2a96d3` + campaign-E artifacts only; clean tree; env-check OK), redis-server **8.0.5** on the
host (the compat oracle's version; `--appendonly yes --appendfsync everysec
--save ''`, AOF under `~/bench-data/s40/data/redis`), infinityd at the shipped
defaults (`io-properties.toml` from the S35 probe ⇒ FUA class, K auto = 3 × 4
MiB, fill 1 ms / 16 KiB, recycling 1 slot; `--data-dir ~/bench-data/s40/data/
infinitydb --conn-default-ns cmp`, `INF.NS CREATE cmp MODE durable FSYNC
everysec` before any generator connection, proven by a probe key). Both data
dirs on nvme0n1p3 (ext4), wiped per launch. Generator: `memtier_benchmark`
(v255 build of 2026-01) on cores 8,10,12,14; servers on 0–6 (`--pin-start 0`:
redis `taskset -c 0-3`, infinityd cells pinned 0,2,4,6 — disclosed: redis is
single-threaded and uses one of its four cores).

Row: `inf-compare run --engines redis,infinitydb --generator memtier
--workload set --pipeline 1 --threads 4 --clients 8 --data-size 1024 --keyspace
1000000 --duration 60 --rate 100000 --durability everysec --probe-file … 
--device-stat nvme0n1 --pin-start 0 --reference-box` — 32 connections each
paced at 3 125 ops/s (100 000 offered, the S27 D5 comparator-matched rate of
ADR-0081 D5 / S36), 1 KiB values, 100 % SET, 60 s per engine, engines run one
at a time in-run (redis first, then infinitydb, the order alternated across
the three invocations: R-I, I-R, R-I), a 40 s idle before each invocation.

Predeclared: the row reports, per engine, achieved/offered (a value below
0.90 invalidates that engine's latency columns — generator or server short),
p50 / p99 / p99.9 / max, server CPU, device MiB written. The S27 D5 bar
(`max ≤ 50 ms at everysec`) is read on infinitydb's rows as a re-measurement
of S36's figure on an *independent* generator; the redis rows are the
comparator context ADR-0081 D5 was written against, with the same loss window.
No ranking claim is predeclared: the claim-ledger row quotes both engines'
numbers side by side with the tier and the generator named, and a comparative
sentence only if every one of the three invocations orders the two engines
the same way on the quoted column. A leg with achieved/offered < 0.90 on
either engine is disclosed and excluded from the quote.
