# M2.5-S21 — remote-hop cost decomposition (2026-07-06)

- box: HomeLab i7-13700KF, 4 cells pinned (`--pin-start 4`), tier **dev
  (non-binding)** — governor powersave; `perf_event_paranoid=4` (reset by
  the box reboot), so PMU/cycle attribution is **blocked** this campaign;
  the decomposition below is arithmetic + instrument-bounded, with the
  cycle-level split named as the missing artifact.
- source rows: `.artifacts/m0/1783359118-gate-run` (lever-off leg 0 of
  `s21-campaign-20260706`), uniform-random pipelined P=16 load.

## the arithmetic bound (the "~3.0×" made exact for this run)

- all-local (`--route-local-only`): 8,732,538 ops/s
- natural routing (uniform random, 4 cells → 75% remote): 3,735,172 ops/s
- penalty: 57.2%
- per-op cost model `T_loc/T_nat = 0.25 + 0.75·r` (r = remote/local cost
  multiple, assuming loadgen saturation both legs — the report's
  saturation tripwires were green):

      r = (8732538/3735172 − 0.25) / 0.75 = **2.78×**

- absolute budget: per-cell local cost ≈ 4/8.73M ≈ **458 ns/op**, so one
  remote op carries ≈ **816 ns of added cost** split across the requester
  and executor cells (CPU) *or* lost to reduced overlap (waiting).

## component inventory (what the 816 ns can hide in)

| component | bound / measurement | instrument |
|---|---|---|
| payload copies (origin side) | **< 2% of the added cost at 64 B values** (3 copies: request pack, reply pack, response-buffer publication — audited, counts match L1's once-across-the-fabric rule) | `reply-path-copy-audit.md` (this dir) |
| cross-connection coalescing headroom | **already structural**: per-destination `BatchOp` packs merge all connections' ops in an iteration at the slot level | code audit (same artifact) |
| request dead time before FABRIC-OUT (step 8) | **not the dominant term at this load**: the `--early-fabric-flush` lever (flush request packs at MAINTAIN head) moved penalty by −0.4 pp…+2.3 pp across legs (see `verdict-early-flush.md`) — i.e. within noise or a small loss; the batching the wait buys outweighs the latency it costs under saturating pipeline depth | lever A/B, this campaign |
| fabric hop RTT (queueing + wakeup) | loop-granularity p50 ≈ 143–147 µs under load (upper bound only — `shared.now` advances once per loop step, so this measures *loop iterations between enqueue and reply*, not CPU) | gate-run fabric RTT row |
| doorbell cadence / executor wakeup · FABRIC-IN dequeue + EXECUTE + reply publication · command suspend/resume | **unattributed** — requires cycle-level sampling; blocked by `perf_event_paranoid=4` this session | perf (blocked) |

## reading

The two cheap structural suspects are eliminated with numbers: copies are
<2% of the gap and coalescing already exists at the pack level. The
early-flush A/B kills the "requests wait too long for step 8" hypothesis
at saturating load. What remains of the 816 ns/op is the
**per-op fabric execution overhead** (enqueue/dequeue, executor wakeup
cadence, suspend/resume bookkeeping) and **overlap loss** (a parked
remote op holds pipeline depth hostage for a hop RTT that spans loop
iterations) — separating those two requires the blocked cycle
attribution, or the remote-first EXECUTE reorder as a *behavioral*
discriminator (it attacks overlap loss specifically: if issuing remote
ops at the head of EXECUTE materially moves the penalty, overlap loss is
the dominant term; if not, it is per-op CPU).

## named missing artifacts (for the binding campaign)

1. `setup-infinity-benchmark-env.sh` (sudo) → governor performance +
   `perf_event_paranoid` relaxed → rerun this campaign `--reference-box`
   (no `--unsafe-env`) for the binding penalty number (AC: ≤ 40%).
2. perf-based cycle split of the 816 ns across the component inventory.
3. remote-first EXECUTE ordering A/B (the overlap-loss discriminator and
   the remaining untested lever from the story's list).
