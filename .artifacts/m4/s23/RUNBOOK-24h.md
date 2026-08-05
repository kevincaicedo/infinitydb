# M4-S23 · 24 h endurance run — operator runbook (prepared 2026-07-31)

The script is `scripts/soak-m4.sh`; this runbook is the protocol that makes
the 24 h of wall time produce a **valid** §7 memory-honesty artifact instead
of an expensive invalid one (§19: an undocumented excursion invalidates the
run). The 30 min mechanics sample under `.artifacts/m4/s23/soak-*` validated
samplers, alerting, the leg loop, and the verdict pipeline — that was the
plan's "dry-run before committing wall time" step, already done.

## Gate being proven (m4-gates.toml rows)

| row | threshold | verdict source |
|---|---|---|
| `endurance_rss_slope` | RSS slope < 0.5%/24 h | `verdict.txt` (first/last-5% medians) |
| `endurance_crashes` | 0 crashes | `verdict.txt` (server liveness + leg failures) |
| disk bounded | `disk_used ≤ DISK-BUDGET` whole run | `samples.csv` max + `verdict.txt` |
| WA flat | worst-ns WA < 3× and flat after warm-up | `samples.csv` q2 vs q4 means |

## HARD PRECONDITION — do not spend the wall time yet

**The tiered data plane is behind the ADR-0062 D8 `USE` refusal until the
command-wiring story (M4-S26 in the plan) lands.** The script probes this
live: today it runs MECHANICS-ONLY (durable namespace, tiered namespace
standing) and stamps the verdict as not satisfying the §7 gate. The real
24 h run is worth starting only when:

1. M4-S26 is merged (`INF.NS USE soak` succeeds — the script flips to
   TIERED mode by itself, no script change needed);
2. S15's endurance-slice test is still green on the wired tree (the plan
   forbids spending 24 h on a known leak);
3. a short tiered soak (`./scripts/soak-m4.sh 1`) runs clean end-to-end
   (fill → demotion → cold reads → compaction visible in `samples.csv`:
   `disk_used_bytes` > 0 and moving, `wa_milli_max` > 1000).

## Box preparation (reference-box tier — §19)

```bash
# outer repo root, needs sudo (a reboot resets governor + perf_event_paranoid):
sudo ./setup-infinity-benchmark-env.sh
cd infinitydb
./target/release/inf-bench env-check          # must read OK (clean tree, performance governor)
```

- Clean tree: commit or stash everything; the env-check refuses dirty.
- `/tmp` hygiene: the box's `/tmp` is a 16 GiB tmpfs — the script refuses
  tmpfs data dirs, but also clear stale `inf*` dirs so unrelated jobs
  can't wedge the shell (`find /tmp -maxdepth 1 -user $USER -name 'inf*'`).
- **Nothing else runs for 24 h**: no cargo, no editors indexing, no agents,
  no browser builds (heavy-jobs discipline; one background job max on this
  box). Thermal counters accumulate — env-check discloses them at start
  and end; a governor/EPP change mid-run invalidates the run.
- Disk headroom: dataset×4 for the tier budget + WAL segments + the
  sampler CSV (~35 MB/24 h). Default config needs ~3 GiB at
  `SOAK_MEM_BUDGET_MB=64`; the gate-scale run below needs ~80 GiB free.

## Launch (gate scale)

```bash
cd infinitydb
SOAK_MEM_BUDGET_MB=2048 SOAK_MULTIPLE=10 \
  nohup ./scripts/soak-m4.sh 24 "$HOME/.cache/inf-m4-soak/data" \
  > .artifacts/m4/s23/soak-console.log 2>&1 &
```

- `SOAK_MEM_BUDGET_MB=2048` ⇒ 20 GiB dataset = 10× budget (sized to the
  box: 30 GiB RAM, budget well under it, dataset well over it — the
  beyond-RAM shape the gate names). Keep the NVMe device profile
  disclosure (ADATA Gen3 DRAM-less vs the §19 Gen4 profile — ADR-0022 D4)
  in the ledger row.
- Loadgen stays on cpus 12–23 (`SOAK_LOADGEN_CPUS`), server cells pin from
  cpu 4 — the C5 lesson: generator placement is part of the result.
- The A+B blend runs as alternating `inf-bench ycsb --workloads a,b` legs
  (zipfian θ=0.99), ~30 min per leg, `--skip-fill` after the first fill.

## During the run (babysitting, ~3 check-ins)

```bash
OUT=$(ls -dt .artifacts/m4/s23/soak-* | head -1)
tail -n 5 $OUT/samples.csv     # RSS + disk + WA moving, sqes/submit ≥ 8
cat $OUT/alerts.log            # empty is the goal; every line needs a root cause
tail -n 3 $OUT/loadgen.log     # legs cycling, no consecutive failures
```

- An alert line = the run is over unless you can root-cause it **into the
  artifact** (write the analysis into `$OUT/excursion.md`); "noted" is not
  a disposition — rerun after the fix (§19).
- Do not touch the server: DISKFULL refusals, if any, are the S21
  machinery being honest — they alert, and recovery is automatic; watch,
  don't intervene.

## After the run

1. `cat $OUT/verdict.txt` — PASS needs no MECHANICS-ONLY stamp.
2. `diff $OUT/env-start.txt $OUT/env-end.txt` — governor unchanged;
   thermal deltas disclosed in the ledger row.
3. Bundle: `samples.csv`, `verdict.txt`, `alerts.log` (+ excursion
   analyses), `info-{start,end}.txt`, `env-{start,end}.txt`,
   `infinityd.log`, `legs/` reports, `mode.txt`. Commit the bundle
   (`git add -f .artifacts/m4/s23/<run>`).
4. Carry the numbers into the S24 campaign gate table:

```bash
inf-bench gate-run m4 ... \
  --endurance-rss-slope-pct <slope> --endurance-crashes 0 \
  --campaign-note "s23 soak .artifacts/m4/s23/<run>/verdict.txt"
```

5. Ledger: the memory-honesty row cites the bundle; if the run was
   page-cache mode (`TIER-IO-MODE` buffered), the cgroup file-cache series
   must be in the attribution or the pass is false (S09 disclosure rule —
   default `Direct` avoids this).

## Failure triage

- **Server death** → `infinityd.log` tail is the finding; the run is a
  FAIL artifact, not a discard — file it with the crash class.
- **RSS slope breach** → check `samples.csv` for step vs drift: a step
  correlates with a leg boundary or checkpoint (look at `ckpts` column);
  drift is the leak class S23 exists to catch — bisect with 1 h runs.
- **Disk breach / refusal storm** → compaction starving: check
  `compact_idle_pressure` in `info-end.txt` (ADR-0059 D1 alarm) and the
  dead-ratio config before blaming the reserve.
- **WA rising after warm-up** → read against MAINTAIN slice + dead-ratio
  (S16 F1: WA ≈ wal/user + 1 + (1−t)/t); a rising curve at fixed config
  is compaction debt accumulating — a real gate failure.
