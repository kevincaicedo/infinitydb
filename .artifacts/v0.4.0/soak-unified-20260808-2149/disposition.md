# Disposition — unified soak attempt 3

**PARTIAL.** Recorded 2026-08-10. Root causes in `run-notes.md`; the
instrument decisions in ADR-0071.

## What this run discharges

- **M2.5-S03 stability soak** — 32 h, zero crashes, KV legs continuous for
  the full run, checkpoints advancing with zero aborts.
- **M3 §7 document soak** — document plane live throughout (1.87 M pipe
  iterations), `doc_resident_bytes` flat, zero errors on the read and
  ingest legs.

Memory rows carried by this run, citable as such:

| Row | Measured | Bar |
|---|---|---|
| RSS slope, steady window | +0.208%/24 h | < 0.5 |
| RSS slope, whole run (disclosure) | +0.248%/24 h | < 0.5 |
| Accounted slope, steady window | +0.066%/24 h | < 0.5 |
| Attribution residual | +0.9% | disclosure |

Scale: 2048 MiB tier budget / 20 GiB dataset / 80 GiB disk cap — the full
runbook scale (readiness D-B), no deviation.

## What this run does NOT discharge

**M4 §7 memory honesty.** The tiered leg refused at +35 min and the tiered
plane was byte-static for 31.4 of the run's 32.1 hours (`run-notes.md` §1).
A memory profile measured over a frozen tier does not speak for the tiered
profile, and compaction — the mechanism S23's own title names — ran zero
times.

Also undischarged, and unmeasured here for the same reason: cold-read p99
under load, write amplification under compaction, tripwire bounds with
tiering active.

## Why `verdict.txt` says PASS

The verdict logic of the day never read `alerts.log`, never re-checked the
launch-time `FULL` stamp against whether the tiered leg survived, and had
no sampled column that could show a static tier. All three holes are closed
by ADR-0071 D4. Replayed through the hardened verdict, this run's own
`samples.csv` returns:

```
FAIL — discharges: NOTHING (a failing run is not evidence for any gate)
  - 4 alert line(s) in alerts.log …
  - tiered leg broke mid-run (tier-leg-broken.txt) …
  - tier-liveness columns absent from samples.csv …
```

`verdict.txt` is left exactly as the instrument printed it. Correcting a
stamped verdict in place is the post-hoc move ADR-0069 exists to forbid; the
correction lives beside it, here.

## Next

Attempt 4 — same 32 h / 2048 MiB form, on the fixed harness, with the
liveness gates armed. Pre-flight per the readiness doc §8 H5, plus: confirm
within the first 15 minutes that `loadgen-tier.log` shows a **completed**
row and that `disk_used_bytes` in `samples.csv` is moving.
