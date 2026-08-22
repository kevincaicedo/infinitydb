# S40 corrected campaign — fair Redis CPU, complete observability, C42 re-evaluation

Rules written before the run. The original S40 reports are historical only:
Redis and its AOF children were pinned to one CPU, child CPU was omitted, INFO
was absent, and an unrelated Redis process existed. C42 is withdrawn.

Binding row: Redis 8.0.5 and InfinityDB run one at a time, in-run, at 100,000
offered `everysec` SET/s, 1 KiB values, 1M-key space, 32 connections, pipeline
1, 60 seconds. Redis is allowed CPUs 0-3, so forked AOF rewrite children do not
compete with its command thread on a single CPU; InfinityDB uses four cells.
The generator uses CPUs 8,10,12,14. The order alternates R-I / I-R / R-I across
three invocations with a 40-second idle before each.

The harness must refuse if any unrelated `redis-server` exists before launch.
For every engine row it stores raw INFO before and after. Redis CPU is parent
plus completed AOF-child and live-descendant CPU; the report includes AOF
rewrite/delayed-fsync deltas. InfinityDB reports admission-park, checkpoint-byte
and write-stall histogram facts. Device sectors written and achieved/offered
remain visible.

Validity: all six production legs must achieve at least 0.90 of offered rate,
have complete INFO snapshots, and run on the clean binding tier. A comparative
latency sentence may return to C42 only if all three paired repetitions order
the engines the same way on every quoted percentile; quote medians and ranges,
not the best run. The `max <= 50 ms` D5 question is evaluated separately and is
not rescued by percentile wins.

After the production rows, one Redis-only
`--redis-no-auto-rewrite` run quantifies automatic rewrite cost. It is explicitly
non-production, cannot enter C42, and must be labelled diagnostic in the report.

Outcome: all six production legs were valid and every pair ordered p50, p99 and
p99.9 the same way. The corrected median Redis/InfinityDB ratios are 2.60x,
2.06x and 3.32x respectively. C42 may return only with these narrowed figures;
the old ratios stay withdrawn. InfinityDB exceeded the separate 50 ms maximum
bar in one of three legs, so D5 remains open. See `VERDICT.md`.
