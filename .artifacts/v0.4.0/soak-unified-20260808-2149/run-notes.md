# Run notes — unified soak attempt 3 (32 h)

Bundle: `.artifacts/v0.4.0/soak-unified-20260808-2149/`
Launch tree: `da303579d5e4ca96d5c8d272707e8957402a042e` (= `origin/main` at
launch; `infinityd.log` line 1 stamps `da30357`, so the `build.rs`
provenance fix of `f922237` is confirmed working on this run).
Wall clock: 2026-08-08 21:49 → 2026-08-10 05:54 · 32.08 h · 11,440 samples
at 10 s.

§19 requires every excursion in a cited run to carry a root cause written
into the artifact. This file is that record. The run's disposition is in
`disposition.md`; `verdict.txt` is the stamp as the instrument printed it
and is not rewritten.

---

## 1. Excursion: the tiered leg failed and stopped, 35 minutes in

`alerts.log` (4 lines):

```
1786241042 ALERT ycsb tiered leg failed (1 consecutive)
1786241649 ALERT ycsb tiered leg failed (2 consecutive)
1786242255 ALERT ycsb tiered leg failed (3 consecutive)
3 consecutive tiered leg failures
```

**Root cause — confirmed, code-traced, fixed.**

1. Commit `f922237` withdrew `tiering_ram_hit_p{50,99,999}_us` from `INFO
   tiering` and rendered `tiering_ram_hit_split:unmeasured-iteration-clock`
   instead. That withdrawal is correct: the reactor's per-iteration clock
   cannot time a command that never suspends, so those fields recorded
   0 µs for every memory hit.
2. The same three names remained in the harness's `SPLIT_FIELDS` array
   (`bins/inf-bench/src/ycsb.rs:60`), and `split_section` returned `Err`
   for any tiered row missing any of them (`ycsb.rs:653`).
3. Every tiered YCSB row therefore failed by construction. `loadgen-tier.log`
   carries the refusal three times, verbatim:
   `` tiered row ran but `tiering_ram_hit_p50_us` is missing from INFO
   tiering — the split histogram contract (M4-S26 / SPLIT_FIELDS) is not
   met ``.
4. `scripts/soak-unified.sh:353` breaks the tiered loop after three
   consecutive failures. It did.

**Consequence — quantified from `samples.csv` hourly medians.** The tiered
plane was byte-static from hour 0 to hour 32:

| Column | h0 | h8 | h16 | h24 | h32 |
|---|---|---|---|---|---|
| `disk_used_bytes` | 20.3 GiB | 20.7 | 20.7 | 20.7 | 20.7 |
| `tier_committed_bytes` | 2056 MiB | 2056 | 2056 | 2056 | 2056 |
| `tier_index_bytes` | 544 MiB | 544 | 544 | 544 | 544 |
| `wa_milli_max` | 1569 | 1539 | 1539 | 1539 | 1539 |

`INFO tiering` at end: `tiering_compact_slices:0`,
`tiering_compaction_bytes:0`. Every cold read, demotion and flush in the
end-of-run counters (1,693,476 cold reads; 5,293 demote slices; 5.55 GB
flushed) is attributable to the 20 GiB loader fill in the first ~5 minutes.

Tiered-plane active window: **1,814 s of 115,513 s = 1.6% of the run.**

**Fix (ADR-0071 D1/D4, landed before the re-run):** the contract splits —
the cold half stays a hard refusal, the withdrawn ram-hit names become
named-absent and never refuse a row; the memory-hit half is derived
client-side with a separation check. And the soak verdict gains the three
gates that would have caught this: alerts are a gate, tiered liveness
(`cold_reads_issued` / `flush_slices` / `compact_slices` must advance) is a
gate, and the discharge stamp follows what the run did.

**Not a server defect.** `infinityd` behaved correctly for 32 hours across
all three planes. This was a harness contract mismatch.

---

## 2. Error replies on the durable legs — `-BUSY`, by design

| Leg | Ops | Errors | Rate |
|---|---|---|---|
| `kv_mem` (memory ns) | 216,010,973 (last leg) | 0 | 0% |
| `kv_esec` (durable, everysec) | 58,941,710,484 | 56,764,165 | **0.0963%** |
| `kv_alw` (durable, always) | 588,923,612 | 721 | 0.0001% |
| `doc_esec` | — | 20,135 pipe iterations with ≥1 error | — |

Single error string across every occurrence:
`-BUSY durable log staging is full, retry`.

**Root cause — carried forward from attempt 2, now with server-side
evidence.** This is bounded, typed admission backpressure at the
group-commit staging boundary, not an error condition: the fabric owner
path cannot suspend inside the drain, so it refuses rather than parking
(local keys park — that is the structural `esec ≫ alw ≫ mem` asymmetry).
ADR-0015 D6 is amended accordingly.

The `log_admission_busy` counter added in `f922237` closes the evidence gap
that made attempt 2's version of this un-adjudicable:
`log_admission_busy:16,367,185` at end of run, against
`acks_gated:147,920,319` and `fsyncs_completed:25,475,634`.

Disposition: **by design, not a stability finding.** The client sees a
typed retryable refusal under sustained pipelined pressure into a durable
namespace, which is the M1-S07/M2 backpressure contract working.

---

## 3. Environment: thermal probe red, but no throttling during the run

`env-start.txt` and `env-end.txt` both report:

```
thermal-throttle  FAIL  cpu0/package_throttle_count=31, cpu1/package_throttle_count=31,
                        cpu10/core_throttle_count=23, cpu10/package_throttle_count=31
```

The counters are **identical at start and end**. They are cumulative since
boot, so the equality is the measurement that matters: **zero throttle
events occurred during the 32 h run.** The red probe reflects throttling
earlier in this boot, before the soak.

Disposition for this run: disclosed, non-invalidating — a soak's memory
slope is not a latency claim, and the state is known rather than unknown
(the §19 rule is about *unknown* environment). **Not acceptable for a
binding campaign leg**: the box is to be rebooted before S24 so the
counters reset.

`git-dirty-tree FAIL` at start names one untracked entry
(`.artifacts/v0.4.0/soak-20260808.log`, this run's own nohup log) and at
end names two (plus this bundle). Expected and annotated in `env-end.txt`.

---

## 4. What the run measured well

These stand on their own and are the strongest memory evidence produced on
this project so far — three failed attempts' worth of instrument work
arriving at a clean answer:

- RSS slope, steady-state window (ADR-0069 gate): **+0.208%/24 h**
- RSS slope, whole run (disclosure): **+0.248%/24 h** — passes *without*
  the ADR-0069 window; the window turned out not to be needed
- Accounted slope, steady window (hard sub-gate): **+0.066%/24 h**
- Attribution residual: **30,742 kB on 3,352,756 kB = +0.9%**
- Crashes 0 · DISKFULL refusals 0 · checkpoints 36,814 completed, **0
  aborted** · `sqes_per_submit` 7.3 flat
- KV legs: 383 × 300 s, continuous for the full 32 h
- Document legs: 1,869,828 pipe iterations, `doc_read`/`doc_ingest` zero
  errors throughout

`mem_fragmentation_ratio:5.43` is `rss / used_memory` with ~2.7 GiB of
cell-scope tiering outside `used_memory` — the INFO scope trap, not
fragmentation. The bottom-up model above reconciles to 0.9%.
