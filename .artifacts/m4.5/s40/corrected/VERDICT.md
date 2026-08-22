# Corrected S40 verdict

The production comparison is valid and all three interleaved pairs agree on
the latency ordering. C42 may return in narrowed form using the corrected
ratios below. The old 2.3-2.6x / 23-25x / 10-15x sentence remains withdrawn.

All six production legs achieved at least 0.96 of the 100,000 offered writes/s.
Redis and its AOF children were allowed CPUs 0-3; server CPU covers the process
tree; no unrelated Redis process existed; and raw before/after INFO exists for
every row.

| metric | InfinityDB, 3 reps | Redis 8.0.5, 3 reps | corrected Redis / InfinityDB |
|---|---:|---:|---:|
| achieved ops/s | 98,459-98,787 (median 98,785) | 96,560-98,677 (median 97,113) | offered-rate row, not a throughput claim |
| p50 | 0.055 ms in all reps | 0.143 ms in all reps | 2.60x lower |
| p99 | 0.127-0.151 ms (median 0.143) | 0.287-0.295 ms (median 0.295) | 1.95-2.26x, median 2.06x lower |
| p99.9 | 0.591-1.079 ms (median 0.679) | 1.079-3.887 ms (median 3.583) | 1.59-6.58x, median 3.32x lower |
| max | 9.663 / 13.695 / 103.423 ms | 56.575 / 144.383 / 524.287 ms | informational; not a percentile claim |
| process-tree CPU | 100-117% (median 101%) | 69-70% (median 69%) | disclose topology: 4 cells vs Redis parent+AOF children |
| device MiB written | 7,675.9-7,827.7 | 8,155.6-8,651.2 | paired median InfinityDB/Redis 0.93x |

Redis completed 40 / 42 / 44 automatic AOF rewrites, with 9.894 / 10.424 /
11.646 seconds of child CPU. InfinityDB reported zero admission parks in every
row; checkpoint output was 138-183 MiB. Its client max exceeded the S27 50 ms
bar once (103.423 ms), so the D5 max claim remains open even though p50 through
p99.9 ordered consistently.

The Redis-only `auto-aof-rewrite-percentage 0` diagnostic is non-production:
98,505 ops/s, p50 0.143 ms, p99 0.287 ms, p99.9 1.871 ms, max 42.239 ms, 50%
CPU, 5,938 MiB written, and zero rewrites/child CPU. Relative to the production
Redis medians, disabling rewrites changed little at p50/p99 but reduced CPU,
device writes and worst-tail exposure. It cannot be used in C42.

The system Redis service was inactive for the campaign and restored afterward.
