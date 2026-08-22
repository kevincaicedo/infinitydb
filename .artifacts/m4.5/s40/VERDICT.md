# S40 verdict — InfinityDB vs Redis 8.0.5 at the S27 D5 offered rate, everysec, one session

Engine `9c31a18`, redis-server 8.0.5 (host), `memtier_benchmark` v255
(2026-01 build), reference box (env-check PASS, tier banner "binding,
citation-grade" on every report), same NVMe (`nvme0n1p3`, ext4), no `fstrim`
possible (disclosed; the box had just run S39b's two campaigns — ~30 GB of
writes — and each S40 invocation writes another ~15 GB). 100 000 offered
ops/s (32 connections × 3 125/s, memtier `--rate-limiting`), 100 % SET, 1 KiB
values, 1 M keys, 60 s per engine, engines one at a time in-run, order
alternated R-I / I-R / R-I, 40 s idle before each invocation.

| invocation | engine | achieved/offered | p50 ms | p99 ms | p99.9 ms | **max ms** | server CPU % | device MiB written |
|---|---|---|---|---|---|---|---|---|
| 1 (R-I) | redis | 0.97 | 0.143 | 3.151 | 4.799 | **66.0** | 51 | 7 758 |
| 1 (R-I) | infinitydb | 0.98 | 0.063 | 0.127 | 0.319 | **342.0** | 105 | 7 600 |
| 2 (I-R) | infinitydb | 0.91 | 0.063 | 0.135 | 0.327 | **1 036.3** | 103 | 6 961 |
| 2 (I-R) | redis | 0.98 | 0.143 | 3.167 | 4.351 | **53.2** | 49 | 7 957 |
| 3 (R-I) | redis | 0.96 | 0.143 | 3.167 | 5.023 | **602.1** | 50 | 7 767 |
| 3 (R-I) | infinitydb | 0.99 | 0.055 | 0.127 | 0.423 | **23.4** | 108 | 7 584 |

## Readings (per the predeclared rules)

1. **Every invocation orders the engines the same way on p50, p99 and
   p99.9** — the comparative sentence is allowed for those columns:
   InfinityDB's everysec write p50 is 2.3–2.6 × lower, p99 23–25 × lower,
   p99.9 10–15 × lower than Redis 8.0.5's at the same offered rate and loss
   window on the same device in the same session, on an independent
   generator. Redis is single-threaded (one of its four cores busy, 49–51 %
   of one core); InfinityDB ran four cells (103–108 % of one core in total).
   Device bytes: InfinityDB wrote 2–12 % fewer bytes than Redis's AOF.
2. **`max` orders the engines differently in every invocation — no
   comparative sentence on it.** Redis: 66 / 53 / 602 ms (its log shows
   an AOF rewrite — a fork over a 1 GB dataset — roughly every second at
   the default `auto-aof-rewrite-*`; the default config is the comparator's
   own, as the 08-20 tri-bench ran it). InfinityDB: 342 / 1 036 / 23 ms.
3. **The S27 D5 bar (`max ≤ 50 ms` at everysec, offered rate) does NOT hold
   on this generator for either engine in this session**: InfinityDB red in
   2 of 3 invocations, Redis red in 3 of 3. S36's in-house row (2026-08-21)
   read 3.5–6.3 ms on the same bar. **This is a finding, not a claim**: the
   two instruments differ in pipeline (16 with latency from the intended
   send vs memtier's pipeline 1 per-connection pacing), dataset (200 k vs
   1 M keys — 4 × the checkpoint), defaults (K auto = 3 + fill vs K = 1 /
   fill off), and drive state (two campaigns earlier the same night vs a
   rebooted box). Invocation 2's 0.91 achieved/offered (inside the 0.90
   rule) says the stalls cost ~5 s of offered work. `inf-compare` does not
   scrape INFO, so the stall is not attributed here (`log_write_stall_
   p99_us`, `log_admission_parked_total`, checkpoint/MANIFEST phase are the
   first three reads). Nothing about the D5 wording may be quoted from this
   row; the claim-ledger row (C42) records both engines' numbers and the
   discrepancy.

## What is not in this row

No closed-loop row (the S27 D5 wording is an offered-rate wording); no
`--fstrim`; no INFO scrape; a single session.
