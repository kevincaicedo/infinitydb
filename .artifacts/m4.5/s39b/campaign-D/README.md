# S39b campaign D — segment recycling on the reference box (rules written before the run)

Engine `16b4dd5` (clean tree, `env-check` OK: governor performance, EPP, thermal
clean; no `fstrim` possible from this shell — disclosed; box idle, last heavy
run the DST sweeps ~25 min before). Generator `taskset -c 8,10,12,14`, cells
pinned 0,2,4,6. Data root `~/bench-data/s39b/data` (ext4 on nvme0n1p3, the
S35/S36/review-3 root). Probe file: the S35 campaign's `io-properties.toml`.

Row: `inf-bench gate-run m4.5 --only-s39b --reference-box --cells 4 --pin-start 0
--barrier-class fua --duration 150 --replicates 3 --leg-idle-s 40 --device-stat
nvme0n1` — K and fill at the **shipped defaults** (`--frames-in-flight auto` ⇒ 3
× 4 MiB under FUA; fill 1 000 µs / 16 KiB), segments 256 MiB (the product
default; ADR-0090 A5 says a smaller segment at this dataset measures the row),
checkpoint floor 256 MiB, arm `--segment-recycle-slots 1` vs baseline
`--no-segment-recycle`, ABBA across replicates, 32-conn closed-loop `always`
writes for 150 s, counters snapshotted at the first-generation trigger (every
cell truncated ≥ 1 and rotated ≥ 2) and at the end (warmed = deltas), a 10 s
read leg, then SIGKILL + respawn timed to `loading:0`.

## Predeclared rules

Binding (gates file, `s39b_*`): warmed zero-fill share ≤ 0.10 (arm, per-
replicate median); recycle deficit ≤ 2 per cell; |Δpadding| ≤ 3 pts (the S39c
control); `always` c32 p50 arm/base ≤ 1.05; p99 ≤ 1.10; read ≥ 0.98; recovery
time ≤ 1.05; rotations per cell ≥ 8 (validity — a row below it is invalid, not
failed). Informational: host bytes per log byte, device sectors per log byte.

Drive-state: a leg whose barrier p99 > 10 ms is flagged and disclosed; a pair
with a flagged leg is excluded when ≥ 2 clean pairs remain, else the campaign
re-runs after a 10-minute idle (no re-reading of a red gate by explanation).

Dispositions: every binding gate green ⇒ recycling `Accepted`, default on
stands (ADR-0090 A5). Falsifier (a) (warmed share > 0.3) ⇒ the 1-slot bound is
wrong, not the mechanism: a 2-slot arm runs the same night as the next
hypothesis, recorded by amendment, default unchanged until it passes. (b)
(p50 or p99 red) ⇒ the rename moves to the control thread before the story
closes — not this session. (c) (recovery > 1.05) ⇒ the reader's skip is fixed,
not the rule. Any red that is not a named falsifier ⇒ `Rejected` for this
shape, default off, recorded.
