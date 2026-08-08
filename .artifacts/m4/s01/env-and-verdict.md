# M4-S01 resolver bench env snapshot
2026-07-18T01:58:02-04:00
git: a1ebcb988e65334b5b08c34920175d2710fcb2c1 (dirty files: 21 — S01/S02/S03 work in progress, disclosed)
kernel: 7.0.0-28-generic
cpu:  13th Gen Intel(R) Core(TM) i7-13700KF
governor(cpu4): powersave
epp(cpu4): performance
pinned: taskset -c 4
tier: dev box (linux-devbox profile) — informational, non-citable (L10)

## medians of 3 replicates
cache-hot delta: +0.03/+0.04/+0.10 ns -> median +0.04 ns (budget <= 2 ns: PASS dev-tier)
miss-bound delta: +0.34/+0.52/+0.57 ns -> median +0.52 ns (budget <= 2 ns: PASS dev-tier)
