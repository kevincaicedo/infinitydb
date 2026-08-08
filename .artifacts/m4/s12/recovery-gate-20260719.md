# M4-S12 recovery-gate re-proof — hybrid checkpoint apply (dev tier)

Date: 2026-07-19 · Box: linux dev (i7-13700KF, governor `performance`,
pinned `taskset -c 4`) · Build: `cargo bench -p inf-store --bench
recovery_gate` (release) · Tier: **dev** per the box profile — the
binding reference-box wall-clock row joins the S22/S24 campaign.

## What is measured

The two apply paths a tiered boot is made of (ADR-0057 D6), isolated on
the CPU side over MemFs. The device half of the boot path (segment
read + M2.5-S08 read-ahead overlap) is M2 machinery this story did not
touch; its cold-composition row was re-proven at ADR-0028 and re-reads
on the reference box with the campaign.

- **image row** — v2 `.ick` of 2 M string post-images (440 B values,
  0.92 GB file) through `read_ick_hybrid` → `TieredTable::apply_image`
  (decode + CRC + re-append into the address space + index insert).
- **ref row** — 20 M addr-refs (14 B entries, 280 MB of section bytes —
  standing in for the cold index of a multi-hundred-GB tier) through
  the same loader → `apply_ref` (idempotency probe + insert; zero
  record bytes, zero disk).

## Results (3 pinned process replicates × 3 in-process reps)

| row | rep 1 | rep 2 | rep 3 | gate / hypothesis |
|---|---|---|---|---|
| image apply | 1.458–1.467 GB/s | 1.432–1.479 GB/s | 1.465–1.472 GB/s | ≥ 1 GB/s/cell — **pass, ~1.46 GB/s median** |
| ref apply | 21.0–21.1 M/s | 21.1 M/s | 21.1–21.2 M/s | ≥ 20 M entries/s (ledger L4 hypothesis) — **pass, ~21.1 M/s** |

## Gate reading

- The M2 replay gate (≥ 1 GB/s/cell) holds **with tiering on** for the
  image class: hybrid image apply through the tiered re-append path
  runs at ~1.46 GB/s — above the M2 CellStore-path dev rehearsals
  (ADR-0018 D6: 1.11 GiB/s), because the tiered insert path is
  hash-precomputed (the sidecar) and presized.
- The ref class recovers the cold *index* at ~21 M entries/s: a 50 M
  cold-record cell (a ~15 GB per-cell cold tier at 300 B/record)
  re-indexes in ~2.4 s with zero cold reads — the property that keeps
  "10 GB < 15 s" bounded by RAM-resident bytes, not by the tier.
- 10 GB-node projection at these rates: 8 cells × (1 GB images + 6 M
  refs)/cell ≈ 0.7 s/cell CPU-side, parallel boot — the wall-clock
  bound is the device read, unchanged from M2.5. Projection, not a
  claim (L10): the binding wall-clock row is S22/S24's.

## Validity

Single-process, pinned, performance governor, no competitor load, MemFs
(no device, no page-cache variance) — a CPU-path isolation by design,
disclosed. Dev tier: no public claim exists until the reference-box
campaign row lands (claim-ledger rule).
