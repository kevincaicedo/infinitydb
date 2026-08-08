# M2.5-S21 — early-fabric-flush lever A/B — verdict (2026-07-06)

- design: full `gate-run m0` legs, lever off vs on (`--early-fabric-flush`:
  flush request packs at the MAINTAIN-step head instead of waiting for
  FABRIC-OUT at step 8), ABBA × 2. Leg 3 (off) lost its server mid-leg
  (post-ready death, stderr not captured — see incident note below) and
  was excluded; rerun as leg 3r with stderr capture, clean.
- tier: **dev (non-binding)** — powersave governor, `--unsafe-env
  --allow-dirty` disclosed; within-run deltas only.
- binary: commit 46591fd; comparator dragonfly (system, in-run, ABBA
  interleaved inside each leg).

## legs

| leg | lever | pipelined ops/s | penalty % | vs Dragonfly | hop RTT p50 µs | p999 µs | loop p999 µs |
|---|---|---|---|---|---|---|---|
| 0 | off | 3561673 | 57.23 | 1.51 | 147 | 1183 | 231 |
| 1 | on | 3599002 | 59.51 | 1.46 | 143 | 1535 | 251 |
| 2 | on | 3499144 | 58.30 | 1.49 | 147 | 1599 | 279 |
| 3 | off | *(excluded — server died post-ready mid-leg; no stderr)* | | | | | |
| 3r | off | 3609260 | 58.75 | 1.45 | 139 | 1567 | 223 |

medians (n=2 per arm): penalty off 57.99 vs on 58.91 (+0.9 pp); pipelined
−1.0%; Dragonfly anchor 1.48 both arms; hop RTT unchanged; loop p999
227 → 265 µs (arms overlap on client p999).

## verdict: **Rejected** (lever ships implemented but off)

At saturating pipeline depth, flushing request packs early buys no
penalty reduction — the wait until step 8 was already buying batching
worth more than the latency it costs (consistent with the request packs
being full either way at this load; the lever's theoretical win case is
low-load latency, which is not the penalty row's regime). The A/B kills
the "requests wait too long for FABRIC-OUT" hypothesis as a penalty
component at this workload shape; recorded as such in the hop-cost
decomposition.

## incident note (leg 3)

One infinityd death after ready, between the pipelined replicates and the
penalty row ("connection refused" at row start; the ready-poll named
nothing, so the process died after accepting its readiness probe). Not
reproduced in leg 3r (stderr capture armed) or any other leg. Untracked
as a story until it recurs with a captured stderr — the fail-stop
narration from M2.5-S01 means any recurrence under capture names its
cell and error.

## anchor status

≥ 1.25× Dragonfly holds in every leg (1.45–1.51× dev-tier; prior binding
1.66×). No regression introduced by the campaign: the shipped default is
byte-identical to the pre-campaign fabric path.
