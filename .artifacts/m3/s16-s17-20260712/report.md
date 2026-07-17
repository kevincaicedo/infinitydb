# M3-S16/S17 engineering acceptance — mutation and document durability

Date: 2026-07-12

Environment: [`env.txt`](env.txt)

Tier: **engineering/dev**, dirty tree disclosed, no public or comparative
claim. S25 still owns reference-box performance.

## Decision and verdict

- **M3-S16: Revised, then Done.** The accepted specialization is the
  allocation-free simple `Child`/`Index` same-width number/boolean patch
  for tape and arena forms. The canonical two-phase engine remains the
  only general mutation interpreter. Generic arena surgery was rejected
  pre-evidence; no second semantic engine, rollback protocol, lock,
  atomic, queue, unsafe block, or dependency edge was added.
- **M3-S17: Accepted and Done.** Tags 6/7 carry versioned `DocDelta` and
  canonical `DocFull` records through the existing staging, frame, group
  commit, fsync-watermark, checkpoint, and recovery paths. The cadence is
  full-after-64 or when accumulated/current operand delta cost reaches
  the current canonical document size. The candidate-audit amendment adds
  per-incarnation lineage plus exact match-count/post-length replay
  witnesses; measurements below were refreshed after that correction.

## S16 performance evidence

Harness: three independent pinned invocations of:

```text
taskset -c 4 cargo bench -p inf-server --bench json_cmd -- json_cmd_1kib
```

Criterion interval midpoints, nanoseconds per command:

| Rep | plain `INCR` | `JSON.NUMINCRBY` tape | JSON / INCR | forced tree | tree / tape |
|---:|---:|---:|---:|---:|---:|
| 1 | 86.515 | 101.75 | 1.176097 | 101.90 | 1.001474 |
| 2 | 86.508 | 101.57 | 1.174111 | 101.94 | 1.003643 |
| 3 | 86.512 | 102.15 | 1.180761 | 101.87 | 0.997259 |

The worst replicate is **1.180761x**, passing the **<= 1.3x** story
budget. Forced tree and tape are effectively flat; neither result pays
the one-time morph and higher tree-memory cost or justifies moving the
density-led 4 KiB default on this engineering row. The threshold remains
configurable and S25 re-measures the full mix.

Allocation counter: 10,000 accepted tape patches plus 10,000 accepted
arena patches performed **zero heap allocations** after construction
(`scalar_patch_alloc`).

## S16 correctness evidence

- `PROPTEST_CASES=1000000`, release, canonical mutation differential:
  **1,000,000 cases green in 18.95 s**.
- `PROPTEST_CASES=100000`, release, store error atomicity across inline,
  arena-tape, and arena-tree forms: **100,000 cases green in 1.52 s**;
  canonical bytes, version, and memory accounting stayed unchanged on
  every rejection.
- The first million-case attempt found a defect in the reference model:
  it re-evaluated an ancestor retaining-op after a descendant edit instead
  of preserving snapshot semantics. The oracle was corrected, the seed
  was retained in `json_apply.proptest-regressions`, and the full campaign
  was rerun green. This was an instrument defect, not a production-engine
  mismatch.

## S17 log-volume and replay evidence

Harness:

```text
taskset -c 4 cargo bench -p inf-store --bench doc_durability
```

The volume arm evolves a real 1,024-byte idoc for 64 mutations using a
20-operation mix: 40% numeric, 20% toggle, 15% string append, 15% array
append, and 10% merge. It invokes the store's actual cadence decision
after every mutation.

- Cadenced records: **4,477 bytes**.
- Full image after every mutation: **68,196 bytes**.
- Ratio: **0.065649x (6.5649%)**, passing the **<= 0.25x** budget.

The replay arm applies 64 sequential deltas to each of 1,000,000
documents: **64,000,000 mutations** and 65.536 GB of equivalent logical
idoc input per replicate.

| Rep | Seconds | Equivalent GB/s |
|---:|---:|---:|
| 1 | 12.653703 | 5.179195 |
| 2 | 12.647585 | 5.181701 |
| 3 | 12.560106 | 5.217791 |

The minimum is **5.179195 GB/s-equivalent**, passing the M2
**>= 1 GB/s/cell-equivalent** story floor. This is the CPU-side semantic
applier row, not a disk cold-recovery or S25 reference-box claim.

## Decoder, replay, and durability evidence

- `fuzz_delta_apply`, corrected 300-second run: **6,177,695 executions**,
  coverage 1,979, feature count 4,217, corpus 481 / 18 KiB, no crash.
- Post-audit `fuzz_delta_apply` run over the lineage/witness framing:
  **1,247,173 executions in 61 seconds**, no crash.
- The first 60-second fuzz run found root-path `Del` entering a production
  invariant panic. Root deletion belongs to the key-lifecycle `Delete`
  record, so the delta decoder/apply boundary now returns typed
  `ApplyError::RootDelete`. A corrected 60-second run completed 1,287,050
  executions; the full 300-second run above then closed the decoder gate.
- Record/frame/effect round trips cover both new tags and every opcode.
  Cadence count/byte boundaries, lineage/recreation, modular-u24
  wrap/stale/ahead behavior, recorded-bound replay under lower boot
  limits, missing records, root-delete rejection, checkpoint
  counts/digest, and exact pre-commit durable full-image admission are
  table-tested.
- The exact admission tests prove an expanded full image that exceeds the
  single-record ceiling or remaining staging budget is refused **before**
  the logical document commit.
- Real Linux fuzzy checkpoint/restart integration is green with document
  full/delta records. The synthetic overlap test counts stale, missing,
  and prior-incarnation delta skips and finishes at the live state.
- `just durable-sweep`: **10,000 seeds, all 8 shards, 0 durability-oracle
  violations, 0 refusals**.

## Final validation

- `just check` — green: formatting; dependency DAG; cell denylist;
  fault/fsync/panic/safety inventories; workspace clippy with `-D
  warnings`; workspace tests; optional `doc-intern-keys`; docless slim
  build.
- `cargo deny check` — green; only the pre-existing unmatched-license
  allowance warnings.
- Affected crate tests (`inf-doc`, `inf-log`, `inf-store`, `inf-server`),
  real node checkpoint/restart integration, and no-default-feature builds
  are green.

## Evidence boundary and next work

S16/S17 are closed only at their stated engineering story budgets. No
M3 gate, comparative claim, or release claim is closed here. M3-S18 owns
the expanded document crash matrix and power-cut closure; M3-S21 owns the
RedisJSON oracle; M3-S23/S24 own the 10,000-seed document fleets; M3-S25
owns reference-box performance and the full morph/RSS campaign.
