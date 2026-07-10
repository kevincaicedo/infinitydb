# M2.5-S19 closure re-campaign — runbook (prep 2026-07-10)

Status: **prep complete; campaign blocked on one user action** — the box
rebooted (memlock raise), which reset the governor to powersave and
`perf_event_paranoid` to 4. Binding legs are §19-invalid until:

```bash
# outer repo root, needs sudo:
sudo ./setup-infinity-benchmark-env.sh
# then verify: inf-bench env-check → OK; perf_event_paranoid = -1; no_turbo = 1
```

## What the reboot bought (verified this session)

- `ulimit -l` = **8388608 KB (8 GiB)** in agent shells (`memlock-active.txt`).
- **First clean 8-cell boot ever** on this box: `infinityd --cells 8` serves
  PONG, zero ENOMEM (`8cell-boot-smoke.log`, tree `38c278d`). The ADR-0026
  D3 / ADR-0033 D4 blocker class is cleared at spawn level.
- Full workspace test suite **3/3 clean** (`fullsuite-flake-recheck.log`)
  vs the ~1/8 historic `UringDriver::new` ENOMEM flake under full-suite
  load — supporting (not conclusive) evidence the same class cleared;
  S02's nightly gives the long-run read.

## Hygiene (binding-leg discipline — the ADR-0027/0030/0033 lessons)

1. `touch bins/infinityd/build.rs` before the campaign build (version-stamp
   staleness); build release once; **zero box work during legs** (no cargo,
   no agents, no editors indexing).
2. `mkdir -p` the `INF_GATERUN_STDERR_DIR` before every run (inf-bench does
   not create it).
3. `git add .artifacts && git commit` **between legs** (next env-check
   refuses a dirty tree).
4. perf.data extracted → deleted immediately (tmpfs RAM lesson).
5. Heavy durable tests only with `TMPDIR=$HOME/.cache/inf-tmp`.

## Campaign order

### Phase 0 — env + generator ceiling

- `inf-bench env-check` must read OK; record governor/EPP/turbo in the
  artifact header.
- **Loadgen-ceiling row (new, feeds the 8-cell shape):** the 8-cell spec
  shape pins cells to all 8 P-cores, so the loadgen loses cpus 12/14 and
  runs on E-cores 16–23 only. Measure the generator ceiling first:
  4-cell all-local server (known ≥ 8M capable), loadgen
  `taskset -c 16-23`, P=16 — the plateau is the ceiling. Any 8-cell row
  at/near this number is a **generator floor, not an engine number**
  (label per the S22 cross-check discipline; §19 saturation rule).

### Phase 1 — m0 regression + closure (4-cell shape)

```bash
inf-bench gate-run m0 --reference-box --replicates 5
```
Regression read vs `m0/1783391461` (baseline, 2026-07-06) and the
ADR-0033 on-tree rows (`m2.5/s21-parsebatch-20260709/results.md`):
≤ 5% or investigated. Tripwires green in-run or the leg is discarded
and disclosed.

### Phase 2 — m0 8-cell spec shape (the ADR-0029 re-read)

```bash
inf-bench gate-run m0 --reference-box --cells 8 --pin-start 0 --replicates 3
```
- Cells on cpus 0,2,4,6,8,10,12,14 (all 8 P-cores); loadgen on 16–23
  (E-cores). Check `/proc/interrupts` for cpu0 IRQ load; disclose.
- Rows: natural + all-local + anchor. ADR-0029 projections: all-local
  ≈ 12.4M (likely **generator-floored** — the ≥ 6M gate still reads PASS
  if the floor itself clears 6M; label honestly), natural at the staged
  ≤ 40% would be ≈ 7.4M — the measured number *is* the ≤ 40% gate's
  8-cell read (ADR-0034 D3 path).
- **De-async A/B arm at 8 cells** (ADR-0034 D2): one
  `--deasync-dispatch` leg vs off, n=3 — per-iteration machinery
  amortizes ~8× worse here; if flat again, S19 deletes the fast path.
- Watch `sqes/submit` (the one red 8-cell leg saw 6–8 vs ≥ 16): if red,
  the 8-cell loop shape needs diagnosis before any row is cited.

### Phase 3 — m1 closure (the never-bound rows)

```bash
inf-bench gate-run m1 --reference-box --replicates 3            # storm/FLUSHALL/fan-out/slow-sub (C5/C6)
inf-bench gate-run m1 --reference-box --with-zipfian            # LFU parity (C8)
# fill/RSS leg (C7) + attribution row (C11) ride the m1 flow's fill leg
```

### Phase 4 — m2 re-validation

```bash
inf-bench gate-run m2 --reference-box --replicates 3
```
Re-validates C13–C16, C21, C22 artifacts for the release re-validation
rule; C12 wording re-signs against the v2 tree citing
`m2.5/s12-frame-seq-20260709/`.

### Phase 5 — inf-compare reduced sweep

```bash
inf-bench compare --engines redis,dragonfly,infinitydb --generator both \
  --workload set,get,mixed,memory --pipeline 1,16 --reference-box
```
(Exact CLI as wired in `infinity-bench-weekly.yml`.) Comparative ledger
rows cite this + `compare/1783652925`.

### Phase 6 — flamegraph + attribution per row (the recorded gap)

One `perf record -C <cell cpus> -F 1997 -g --call-graph dwarf,16384` window
inside each row class (m0 natural, m0 all-local, m1 mixed, m2 durable,
storm), report extracted per `dev-perf.sh` shape, perf.data deleted.
Attach to the campaign dir; every cited row names its flamegraph.

### Phase 7 — ledger walk + website re-sign

Worksheet (docs/claim-ledger.md):

| row | closes via | expected disposition |
|---|---|---|
| C2 client libs | S02 runner first green (user) | Narrowed→Allowed at S02, or stays bound to the named runner task |
| C5 throughput | Phase 1–3 rows | Narrowed wording extended: M1 rows bound; 8-cell rows added with penalty context |
| C6 latency | Phase 3 re-read | Allowed/Narrowed final wording (pub/sub-background KV p99.9 row disposition included) |
| C7 RSS ≤ 1.0× | Phase 3 fill leg | flip Allowed/Narrowed at measured value |
| C8 zipfian parity | Phase 3 zipfian row | flip Allowed/Narrowed at measured pp |
| C9 image size | release-pipeline dry-run (S02, user) | flips on dry-run or stays bound to the named task |
| C11 attribution | Phase 3 attribution row | Narrowed→Allowed if RSS-divergence gate binds |
| C12 durability DST | wording re-sign (v2 tree) | Allowed, refusal-disclosure updated to 0.00% |
| C17/C18/C19 device rows | S18 decision (user) | stay Evidence-pending **bound to a dated Gen4 plan** (= dispositioned per the cut line) |
| C20 recovery | Phase 4 re-validation | Narrowed, stands |
| C13–C16/C21/C22 | Phase 4 re-validation | Allowed, artifacts refreshed |

Then: refresh `website/site/_ledger-snapshot.md` from the final ledger,
run `python3 scripts/check-ledger-copy.py` + the compat staleness check,
re-sign site/announcement copy with the ledger commit hash. **Exit gate:
zero undispositioned Evidence-pending rows for v0.1/v0.2.**

### S18 decision memo (seed — the user's call, drafted for it)

The three device rows (C17 ckpt-absolute, C18 300k always, C19 everysec
penalty) close without code on Gen4-class NVMe (ADR-0022 D4/D8.5):
- **Option A — swap the designated box's NVMe to Gen4** (~$80–150,
  1 TB class): keeps one reference box; **re-baselines every
  device-bound artifact** (disclosed re-run: m2 gate set + recovery +
  soak; plan the re-run in the same week to keep the ledger coherent).
- **Option B — second box with Gen4**: splits the reference-box
  designation (§19 note must name which box backs which row class);
  no re-baseline of existing rows, but two environments to maintain.
- **Option C — defer**: rows stay dispositioned with a dated plan
  (allowed by the cut line; the debt stays visible in the ledger).
Record the decision as an ADR-0022 amendment (or new ADR) either way.

### S20 (cut line)

After the walk: record ship/skip for `v0.2.0-alpha.2` in the review
ledger with rationale (ship only if Phase H changed publishable claims —
S12's 0.00% refusal row is the strongest candidate).
