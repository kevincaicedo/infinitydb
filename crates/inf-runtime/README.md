# inf-runtime

Cell runtime for InfinityDB (master plan §5, milestone M0-E2): the
`BackendDriver` contract, io_uring and kqueue backends, the single-threaded
cell executor with typed suspension gates, the timer wheel, scheduler
groups, and the 10-step reactor loop.

## Backend tiers

| Backend | Target | Tier |
|---------|--------|------|
| `UringDriver` | Linux, `--features uring` | **Performance** — the only backend that appears in gate artifacts |
| `KqueueDriver` | macOS | **Correctness/dev only** — never in any performance gate |
| sim driver | `inf-sim` (M0-S20) | Deterministic testing |

The kqueue backend is a readiness→completion adapter: it performs real
syscalls at readiness and makes **no batching or performance claims**
(`Capabilities::performance_tier == false`). Any benchmark number produced
on it is a development sanity check, not evidence (L10). Performance gates
run on the Linux reference box against `UringDriver` only.

`UringDriver` validation status: authored against `io-uring` 0.7,
compile-checked for Linux targets, exercised by the same conformance suite
as kqueue in CI (`kernel-matrix` job, probed + forced-degraded modes).
Runtime validation on real kernels is pending the Linux reference box — see
`reviews/infinity-m0-skeleton.md`.

See `SAFETY.md` for the unsafe-code audit areas.
