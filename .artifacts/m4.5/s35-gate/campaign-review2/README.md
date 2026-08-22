# M4.5 E4.7 review follow-up 2 — the clean K = 3 / 4 MiB rerun (2026-08-21 23:16 → 23:31 local)

**Why.** The review of `2cb6074` (finding 3) refused to let the 08-21
K = 3 / 4 MiB arm's red 4c/1c gate (1.38) pass by explanation. This is the
clean rerun it asked for, on the engine with the completion-ledger reorder
window (`6fb1f01`, clean tree): `fstrim` by the owner before the session,
governor `performance`, 60 s idle before the row, 40 s before every durable
leg, **the 1-cell leg interleaved per replicate** (the S35 row's new shape —
drive-state drift lands on both arms of the ratio), **5 replicates**.

**Result (`k3s4-rerun-report.md`):**

| gate | threshold | measured | verdict |
|---|---|---|---|
| S35 `always` p50 ÷ barrier p50 @32 | ≤ 1.3 | **1.21** | PASS |
| S35 4-cell ÷ 1-cell p50 @32 | ≤ 1.3 | **1.34** | **FAIL** |

4-cell legs: p50 735–751 µs (median 751), 36.1–39.1k ops/s, barrier p50
607–639 — identical to the 2 MiB pairing's 4-cell legs in every campaign.
1-cell legs: p50 543–575 (median 559; rep3 735 in the drive-state bad mode
— barrier p99 62 ms, 7.5k ops/s), barrier p50 527–559. Reads 1.56–1.61 M
ops/s. Two durable legs flagged by the harness (barrier p99 > 10 ms: rep0
c256 15 ms, rep3 1c 62 ms).

**Disposition.** Under the gate as written K = 3 / 4 MiB is **not
eligible**: two campaigns, 1.38 and 1.34. The ratio is decided by the
1-cell leg (the 4-cell figures equal the 2 MiB pairing's); the barrier
itself scales 1.15× from 1 to 4 cells in the same legs, and the client
histogram's bucket width at these latencies (543 → 559 → 575; 735 → 751)
is one gate-margin wide — recorded as an observation about the gate's
resolution, **not** as a pass. **K = 3 / 2 MiB remains the only pairing
with both S35 gates green** (campaign 2: 1.18–1.21 and 1.25–1.28). The
default stays K = 1 in code; the owner's choice is between K = 3 / 2 MiB
(measured, both gates) and K = 1.
