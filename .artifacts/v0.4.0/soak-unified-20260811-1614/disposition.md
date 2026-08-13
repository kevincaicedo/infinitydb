# Disposition — attempt 4 (2026-08-11 → 12)

**FAIL** by the run's own hardened verdict (ADR-0071 D4).
**Discharges: NOTHING.**

- M4 §7 endurance / memory honesty — **NOT discharged** (compaction never
  ran; the story's own title went unproven for a fourth time).
- M2.5 stability soak, M3 §7 doc soak — **untouched**: discharged by
  attempt 3 on its own artifact; a failing run neither adds to nor
  subtracts from that.

**What this run did establish** (recorded as disclosure, not gate
evidence): the F17 harness regression is fixed and proven — the tiered
plane served 99.9 M cold reads and 22,772 flush slices across 30.35 h,
where attempt 3's was byte-static for 31.4 h. Memory behaviour on a live
tiered plane is the best measured to date: RSS steady +0.151 %/24 h,
accounted −0.001 %/24 h, attribution residual +1.2 %, zero crashes, zero
DISKFULL refusals, WA 1.92× max.

**Why it failed, and what it is not:** tier writes stopped after the fill
(WA → 1.086, flush 9.8/h, disk +0.23 GB over 22.35 h) so no dead space
accumulated and compaction had nothing to reclaim. A same-day diagnostic
on the same tree (`diag-compaction-path.txt`) drove the identical `a`
row against a filled tiered namespace and produced 244 MB of dead bytes
and 16,276 compaction slices in ~2 minutes — **the compaction path is not
broken.** The defect is in workload delivery at soak scale, with a named
leading hypothesis (one fixed seed replayed across all 145 legs) and a
named missing instrument (`dead_bytes` + `compact_idle_pressure` columns
in `samples.csv`).

**Run validity:** operator-terminated at 30.35 h of a declared 32 h, so
the ADR-0069 steady window is 22.35 h rather than 24 h. No crash, no
alerts, no tier-leg sentinel. The verdict was replayed offline through
the unmodified in-tree verdict body (`verdict-replay.txt`) because the
script never reached its own verdict stage.

**Owner decision owed before attempt 5:** whether the tiered leg's seed
advances per leg (changes the declared workload — ADR-0064 D7 territory)
and whether the sampler gains the two reclaim columns. Both are small;
neither should be made silently.
