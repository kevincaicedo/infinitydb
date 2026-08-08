# M2-S22 Redis AOF context rows (comparator on the same box + workload)

Date 2026-07-05 · kernel 7.0.0-27-generic · governor `performance` · git `72354a8` (clean)
Box: HomeLab i7-13700KF, ADATA LEGEND 700 (consumer NVMe Gen3, DRAM-less) — the
user-designated M2 reference box. Oracle: redis-server 8.0.5 (jemalloc 5.3.0,
`/usr/bin`, the pinned compat oracle), AOF dir on the same NVMe.

Context rows (§22 S22 task: "interleaved A/B vs Redis on the same box for
context"): same load generator (`inf-bench load`), same connection/pipeline/
value shapes as the InfinityDB gate rows they contextualize. Redis legs ran
solo (not interleaved with an InfinityDB server on the same cores — the
InfinityDB numbers come from the binding gate-run on the same box minutes
apart, both engines unshared at run time).

## appendfsync always — vs the InfinityDB `always` grouped-write row
`redis-server --port 7411 --dir <nvme> --save '' --appendonly yes --appendfsync always`
`inf-bench load --conns 64 --pipeline 16 --duration 10 --mix 1:0 --keys 100000 --value-size 64`

| rep | ops/s | p99.9 |
|---|---|---|
| 0 | 373,105 | 8,447 µs |
| 1 | 372,144 | 8,703 µs |
| 2 | 374,591 | 8,703 µs |

Median **373k w/s**, spread 0.7%.

## appendfsync everysec — vs the InfinityDB everysec penalty row
Same server shape with `--appendfsync everysec`;
`inf-bench load --conns 64 --pipeline 16 --duration 10 --mix 1:1 --keys 200000 --value-size 512`

| rep | ops/s | p99.9 |
|---|---|---|
| 0 | 702,367 | 7,551 µs |
| 1 | 704,943 | 7,935 µs |
| 2 | 698,518 | 7,807 µs |

Median **702k ops/s**, spread 0.9%.

Raw outputs: `redis-{always,everysec}-rep{0,1,2}.txt`. InfinityDB comparison
rows live in the binding S22 gate-run report (same date, this artifact tree).
