# S39d campaign H — fixed-work recovery attribution on a recycled log (ADR-0090 A10)

Rules written before the run (2026-08-22). Reference box, same shape as
campaigns F/G where the shape carries over.

## What is compared

- **baseline (A):** `--no-segment-recycle` — every segment pre-zeroed,
  the slack the audit walks is zeros (word-wise skip).
- **arm (B):** the server default — one-slot recycling + the D9 `quarter`
  pool wait — the slack the audit walks is recycled-life residue
  (decoded + CRC-validated foreign-segment frames).

Both arms receive exactly the same work: `--s39d-warm-records 3000000`
(fill mode: every key once, partitioned across 32 conns, pipeline 16,
1 KiB values, `always`, FUA class, 256 MiB segments at the 256 MiB
checkpoint floor), then `INF.CKPT WAIT` (the boundary), truncation
settled 2 s, then `--s39d-tail-records 200000`, SIGKILL after the tail's
last ack, 40 s idle on the untouched image, exactly one boot timed to
`loading:0`. Three interleaved replicates (ABBA). 40 s idle before every
leg. Bench loadgen pinned to cores 8,10,12,14; cells pinned from core 0.

Row: `gate-run m4.5 --only-s39d --reference-box --cells 4 --pin-start 0
--barrier-class fua --replicates 3 --leg-idle-s 40 --device-stat nvme0n1`.

Engine: the first commit carrying `RecoverStats::phases` (per-phase
bytes + loop-clock time) and the ADR-0088 D2 carry (without it the
boundary `INF.CKPT WAIT` never returns on an idle budgeted node — the
smoke's finding).

## Hypothesis and decision rules (predeclared)

- **Validity:** `s39d:records_recovered_match = 1` in every pair (both
  arms recover exactly 3 200 000 records); every leg `warmed` (every cell
  truncated ≥ 1 and rotated ≥ 2 before the boundary); `frame_bytes_x`
  within 1.00 ± 0.03 (the encoded-bytes parity group formation allows).
  A pair failing any of these is excluded and said so.
- **The diagnostic ratio** `s39d:recovery_total_x` (slowest cell's
  engine total, arm ÷ baseline, per-replicate median): ≤ 1.05 →
  "recovery parity holds on fixed work" and campaign F's 1.293 is
  confirmed workload-confounded; > 1.05 → the per-phase ratios name the
  term and the plan's S39d branch table decides the next story. **In
  neither case does this row turn recycling off** (ADR-0090 A7.3).
- **The binding gate** `s39d:recovery_first_boot_s_arm ≤ 15 s` (S18's
  absolute STOP gate re-read on the recycled log at this row's dataset,
  ~3.3 GB of frames — a third of the S18 10 GB shape, disclosed).
- **The recycling term** `s39d:phase_audit_x` (informational, ≤ 1.5):
  the audit phase is where residue differs from zeros; its byte ratio
  `audit_bytes_x` should read ≈ 1.0 (same slack extent) and
  `audit_foreign_frames_arm` > 0 proves the arm's image carried residue
  (otherwise the row did not measure what it claims).
- Dominating phase per arm is reported, not gated.

## Disclosures expected

- No `fstrim` (no sudo); drive state per the campaign log's
  `/sys/block/nvme0n1/stat` header/footer.
- `proc_read_bytes` is `/proc/<pid>/io read_bytes` at `loading:0` — the
  process's storage reads including the page cache misses only.

## H2 — the clean rerun (same rules, same binary `3bb32df`)

Campaign H's rep0 arm boot (23:28:12) overlapped `cargo clippy`/`cargo
test` of the S40 harness on the same box (all cores; the cells are
pinned from core 0 — core contention, the env-check is blind to it).
H2 (engine `e123e83` = `3bb32df` + the S40/S37 harness rows and a `bench-diagnostics` arm compiled out of this binary — no cell-resident code in a shipping build changed) reruns the identical row on a quiet box: nothing else is compiled or
run between its header and footer. H stays on the record as run, with
that disclosure; H2 is the citable row.
