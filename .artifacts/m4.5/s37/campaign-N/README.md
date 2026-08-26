# S37 campaign N — step 2's first discriminator: the cold-read cap widened (COLD-READ-QD 64 / 128 / 256)

Written 2026-08-25 **before** the first leg. Reference box; tier per the
run's own `env-check` header. Plan S37 (2026-08-23): "the foreground
cold reads ride the ADR-0055 shaper's bounded in-flight window; widen
it and re-measure A before designing shadow slots (if A approaches B,
the read's cost is queueing, not the read)".

## Row

`gate-run m4.5 --only-s37 --s37-cold-read-qd 64,128,256 --reference-box
--cells 4 --pin-start 0 --replicates 3 --duration 20 --s37-keys 1000000
--leg-idle-s 10 --data-root ~/bench-data/s37/data-N` on the **shipping**
binary (no `bench-diagnostics`), the reference device model at the data
root (campaign J's configuration). Per leg: fresh server, tiered
`always` namespace (MEM-BUDGET 128 MB/cell, DISK-BUDGET 10 GB, TIER-IO-MODE
direct, **COLD-READ-QD = the arm**), 1 M × 1 KiB fill (≈ 250 MB/cell —
beyond the budget, ≈ 90 % of SETs meet a cold candidate), then 100 % SET
closed-loop pipeline 1 at 64 and 256 conns, 20 s each. Three arms, the
order rotated per replicate (each arm in each position once). Memory:
the cold-read pool is `qd × 16 KiB` per cell — 1 / 2 / 4 MiB.

Arm 64 = the ADR-0055 D2 default = campaign J's arm A.

## What is read

Per arm and conns (per-replicate medians): ops/s, p50, p99 as ratios
to arm 64; the shaper's `cold_read_qd_p99` (device QD at issue — a
whole-session histogram, so the c256 reading carries the c64 leg's
samples, disclosed), `cold_read_p99_us`, `cold_queue_full`,
`cold_pool_dry`, `cold_reads_issued` per SET.

## Predeclared rule

Let `A64`, `A128`, `A256` be the c256 ops/s medians here, and `B_J =
100 235` ops/s campaign J's c256 ceiling median (a different night's
number — an upper bound from an unsound build, used as the gap's far
end, never as a target; J's own A read 21 975).

`closed = (A256 − A64) ÷ (B_J − A64)` — the share of the gap to the
ceiling the widest cap recovers.

- **Validity first:** `s37:qd_base_cold_qd_p99_c256` (arm 64's device
  QD p99 at issue) ≥ 48 — the cap bound on the baseline. If it reads
  well below 64, the cap never bound, the discriminator cannot
  discriminate, and the reading is "the read's cost is not the cap" —
  the shadow-slot branch, with this validity line quoted.
- **`closed ≥ 0.5` ⇒ queueing in the cap:** S37 step 2 is re-scoped to
  the shaper (the cap, the drain, the pool — cheap, safe, no format);
  shadow-slot reconciliation is **not** built. The smallest cap that
  wins (≥ 80 % of the widest cap's gain at ≤ 1.1 × its p99) is proposed
  as the default by ADR amendment to ADR-0055 D2, with its memory named.
- **`closed < 0.15` ⇒ the read is the cost:** the cap is not the lever;
  shadow-slot reconciliation proceeds ADR-first as the plan says.
- **Between:** both terms are named with their shares; the shaper share
  is taken first (it is cheap), shadow slots stay `Proposed` behind it.
- **Tail clause:** an arm whose c256 p99 ratio exceeds 1.1 × charges a
  tail for its throughput — recorded; it cannot be proposed as a default.
- **64-conn pair:** reported beside the 256-conn decision (at 16 conns
  per cell the cap of 64 cannot bind; a gain there would be a different
  mechanism and is a finding, not a decision input).

## Predictions on the record

At 256 conns, 64 per cell, pipeline 1: at most 64 cold reads
outstanding per cell — exactly the cap, so `cold_qd_p99` ≈ 64 on arm 64
if the reads queue there. A DRAM-less consumer NVMe at 256 × 4 KiB
random reads outstanding under 100+ MB/s of FUA writes is the other
candidate for the 8.9 ms p50 (J); if the device is the queue, widening
the cap moves little and `cold_read_p99_us` grows with the cap. No
prediction of the outcome is made — that is what the row is for.
