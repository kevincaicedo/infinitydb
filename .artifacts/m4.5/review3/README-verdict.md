# Review-3 campaign (2026-08-22, 10:51 → 13:16 local) — verdicts

Rules: `README.md` (written before each campaign ran). Engine `5e162b7`
(first four A reports) / `4d7678f` (docs-only commit; engine byte-identical)
for the rest. Reference box, governor `performance`, turbo off, clean tree,
no `fstrim` this session (disclosed), 40/60 s idles as per row.

- **Campaign A** (S39a at K = 1 / 4 MiB, 5 ABBA pairs): R1 ✓, R2 ✓ (zero
  stalls in 10 offered legs), R3 parks ✓, R3 p99 pairwise **3/5** ⇒ no
  flip by A (rule defect recorded).
- **Campaign B** (K gate, 3 pairings × 5 rounds): G1 K1 1.853 FAIL · K3/2
  1.198 · K3/4 1.208 PASS; **G2 barrier 4c/1c 1.54 / 1.35 / 1.44 — all
  FAIL** ⇒ K = 1 by the table; G2 recorded as a device-characterization
  row (1-cell barrier 383–559 µs across sessions/queue depth, 4-cell pinned
  591–623).
- **Campaign C at K = 1** (3 S36 pairs + S35 fill on/off): every clause ✓
  ⇒ **fill default = 1000 µs / 16 KiB** (engine commit after this file).
- **Campaign C at K = 3 / 4 MiB** (candidate, not a decision): with fill on
  K = 3 equals K = 1 on `everysec` and keeps its `always` p50/throughput
  win; with fill off it loses `everysec` (−11 %, padding 29 %); fill costs
  `always` c32 p99 +14–43 % at K = 3 (+3–7 % at K = 1) — unattributed.
