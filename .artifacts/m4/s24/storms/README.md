# M4-S24 phase 5 — foreground-protection storm rows (device-loaded)

Closes the last `PENDING (tooling)` row in the campaign gate table:
`foreground_p999_storms` — "Foreground protection: p99.9 during demotion +
compaction storms", threshold **< 2 ms** (`m4-gates.toml`, tier
`linux-reference-box`).

date: 2026-08-16 · reference box (i7-13700KF, 24 cpu) · governor
`performance` · `no_turbo=1` · EPP `performance` · kernel 7.0.0-29-generic
· `taskset -c 4` (single P-core, the S07/S11 storm shape) · substrate
`INF_STORM_DIR=$HOME/.cache/inf-tmp` on `/dev/nvme0n1p3` ext4 = ADATA
LEGEND 700, **Gen3 DRAM-less, disclosed (ADR-0022 D4)** · `TierIoMode::Direct`
(ADR-0054 default) · tree `bf1dc07`, clean.

Harness: `cargo bench -p inf-store --bench {demotion,compaction}_storm`.
`INF_STORM_DIR` set ⇒ the device-loaded leg runs and the MemFs leg does
not (`demotion_storm.rs:210`). Replicate 1 of each was run by the operator;
replicates 2–3 were run in the same box state immediately afterwards.

**What the histogram covers.** `foreground+slice` — every foreground op is
timed individually *and* each `maintain()` slice's wall time is pushed into
the same vector (`demotion_storm.rs:167-172`). A maintenance slice that
stalls the loop bills exactly like a slow op. This is the conservative
reading: the gate value includes the maintenance cost rather than
excluding it.

## Demotion storm (M4-S07/S11) — real-device Direct, 3 replicates

| rep | p50 | p99 | **p99.9** | max | stalls |
|---|---|---|---|---|---|
| 1 | 84 ns | 2227 ns | **1.704 ms** | 11.216 ms | 0 |
| 2 | 138 ns | 2378 ns | **1.715 ms** | 10.687 ms | 0 |
| 3 | 135 ns | 2270 ns | **1.702 ms** | 9.275 ms | 0 |

Deterministic counters identical in all three reps: 400 000 ops, 30 cold
candidates, 3243 demote slices, 13 283 553 B sealed, 3233 flush slices,
13 279 272 B flushed, 3 files sealed, committed 8 396 800 B against a
33 554 432 B budget + 4096 B slice. `tail_alloc_stalls == 0` in every rep
(asserted by the bench: "a paced storm never stalls").

## Compaction storm (M4-S15) — real-device Direct, 3 replicates

| rep | p50 | p99 | **p99.9** | max | stalls |
|---|---|---|---|---|---|
| 1 | 121 ns | 2215 ns | **1.699 ms** | 11.603 ms | 0 |
| 2 | 137 ns | 2232 ns | **1.721 ms** | 13.269 ms | 0 |
| 3 | 138 ns | 2257 ns | **1.704 ms** | 11.467 ms | 0 |

Deterministic counters identical in all three reps: 400 000 ops, 13 933
cold candidates, 13 compact slices, 1415 records / 250 507 B relocated,
3 files retired and unlinked, cold floor 12 574 730, 3208 flush slices,
3247 demote slices.

## Verdict

**PASS.** Worst p99.9 across all six replicates = **1.721 ms < 2 ms**
(compaction rep 2). Spread across the six: 1.699–1.721 ms, i.e. 1.3% —
this row is far more stable than the degenerate p99.9 rows, because it is
a single-core in-process histogram rather than a client-observed one.

Gate carrier for the campaign assembly:
`--foreground-p999-ms 1.721 --campaign-note "…/s24/storms"` (worst binds,
matching how the other worst-of rows bind).

## Disclosed drift vs the July device leg — headroom fell by half

`.artifacts/m4/s11/demotion-storm-device-20260718.md` measured the same
demotion bench, same substrate, same pin, on 2026-07-18 at **1.287–1.367 ms**
(3 reps, tree `147c33a`+S11). Today the same bench reads **1.702–1.715 ms**:
**+27.9% median-to-median** (1.3329 -> 1.7044 ms; +25.5% worst-to-worst), and the gate's headroom drops from 31.7% to 14.0%.

This is recorded, not explained away. It is **not** attributed to any
specific change, because this campaign did not run the A/B that would
attribute it. Candidate causes, none of them tested here:

- tree drift `147c33a` → `bf1dc07` (S15 compaction, S16–S18, S26 wiring);
- drive state — the same box showed a 10% → 34% swing on the m2 everysec
  row from drive state alone, cleared by `fstrim` (F20);
- the OS update that changed `/tmp` to a tmpfs default and introduced the
  `usrquota` fail-stop.

`p50` also rose (87 ns → 84–138 ns) and `max` grew (7.5–8.2 ms → 9.3–13.3 ms).
**Owed:** if a future milestone wants this row's headroom back, the first
step is a crossover A/B of `147c33a` vs `bf1dc07` on this bench — one
afternoon — not a guess. Filed as an observation on the M4.5 debt epic
rather than a gate failure, because the row passes as measured.

## Files

- `demotion-rep{1,2,3}.txt`, `compaction-rep{1,2,3}.txt` — raw bench stdout
- `box-state.txt` — governor/EPP/turbo/thermal/filesystem at run time
