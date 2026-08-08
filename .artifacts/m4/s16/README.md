# M4-S16 evidence bundle — write-amplification accounting + reporting

**Tier: dev.** Single box, device deviation disclosed (ADR-0022 D4 class).
No claim-ledger row is asserted by this story; the reference-box re-read
is S24's, and the gate-grade WA figure under campaign load is S22/S23's.

```
date       2026-07-29
box        HomeLab dev box — i7-13700KF (8P+8E), 30 GiB RAM,
           ADATA LEGEND 700 (Gen3, DRAM-less NVMe — §19 device deviation)
kernel     7.0.0-28-generic
cpu state  governor `performance`, EPP `performance`, turbo off (no_turbo=1)
           pinned `taskset -c 4` for the device legs
filesystem ext4 on /dev/nvme0n1p3 (diskstats device 259:3),
           INF_ACCT_DIR=$HOME/.cache/inf-bench-acct
git        inner repo a5f5fac + the S16 change set (tree dirty with exactly
           that change set — disclosed, §19)
env-check  FAILED on `git-dirty-tree` (expected: the S16 tree) and on
           `thermal-throttle` (counters accumulated earlier in the session);
           governor/EPP probes PASS. gate-runs below carry `--unsafe-env`
           and are therefore explicitly not citation-grade.
```

## What each file is

| File | What it shows |
|---|---|
| `reconcile-replicate-{1,2,3}.txt` | AC 1. `cargo bench -p inf-store --bench write_accounting` — the S13 block-layer instrument with the S16 churn leg added. Two legs per run: insert-only (the S13 baseline) and skewed overwrite churn with copy-forward + retirement live. Each leg prints the counters, the reported ratio, `/proc/diskstats` device bytes, and the divergence of **both** candidate numerators. |
| `canary-test-20260729.txt` | AC 2. `cargo test -p inf-store --test tiered_write_amp --release -- --nocapture` — the three S16 store-tier tests, including the deliberate-regression canary (50% vs 10% dead-ratio trigger) and the ADR-0060 D2 counter-test. |
| `gate-control/*/report.md` | The control report: the device-measured default-tuning figure (1.730×) evaluated by the real gate machinery → `PASS`, exit code 0. |
| `gate-canary/*/report.md` | The canary report: the mis-tuned figure (8.318×) → `Write amplification, worst tiered namespace … FAIL`, `1 binding gate(s) FAILED`, exit code **1**. This is the tripwire firing. |
| `recovery-sweep-3k-20260729.txt` | Regression cover for the address-space fix (finding F2 below): `m4-recovery` 3 000 seeds, base `0x516C0DE`, **0 violations** over 211,263 relocations — the flush path this story changed, driven through crash/recovery interleavings. The 10 000-seed sweep of record stays S15's. |

Both gate reports also carry the new **write amplification by row** table
(three `n/a (no tiered namespace on the node — memory-mode row)` rows) —
the M4-S16 rule that a report row without a WA disposition is an invalid
row, and that `n/a` must be structurally verified rather than assumed.

## Numbers (3 replicates each, deterministic workloads)

### AC 1 — the figure reconciles with the block layer

| Leg | reported WA | relocation volume | counters vs device | rejected numerator vs device |
|---|---|---|---|---|
| insert-only (S13 shape) | **1.999×** | 0 | **−1.40 / −1.40 / −1.42%** | same (nothing relocated) |
| overwrite churn (S16 shape) | **1.730×** | 146,470,896 B (16% of the numerator) | **−2.24 / −2.27 / −2.28%** | **+13.17 / +13.15 / +13.13%** |

Read the churn row twice. First: with compaction active, the reported
numerator (`wal + flush`) still reconciles to within 2.3% — six times
inside the ±10% AC. Second: the numerator the plan's story text
specified (`+ compaction_bytes`) misses by **+13%**, outside the window
in every replicate. That is the measurement that settled ADR-0060 D2 —
relocated bytes reach the device through the flush leg, so adding them
counts each one twice.

Why the churn leg's WA (1.730×) is *below* the insert leg's (1.999×),
which surprises at first: with a skewed workload many overwrites kill a
record while it is still in the mutable region, and an in-place update
writes no tier byte at all. Skew buys real headroom against the 3× gate —
see the finding below for why that matters to S22.

### AC 2 — the canary trips the gate

| Leg (same build, same workload) | relocation volume | reported WA | gate verdict |
|---|---|---|---|
| tuned — ADR-0059 D1 default, 50% dead | 1,994,165 B | 3.039× | — |
| **canary — mis-tuned to 10% dead** | 19,839,381 B | **8.318×** | **FAIL** (`gate-canary/`) |
| control — device-measured default tuning | (see AC 1) | 1.730× | PASS (`gate-control/`) |

`user_bytes` and `wal_bytes` are asserted identical across the two legs:
the trigger is the only variable.

## Finding carried out of this bundle (owners S19/S22/S23)

Under sustained churn where every dead byte lands in a tier file,
`WA ≈ wal/user + 1 + (1 − t)/t` for a dead-ratio trigger `t` — compaction
must reclaim one dead byte per user byte written, relocating `(1 − t)/t`
live bytes to do it, each of which is flushed again. That model puts the
shipped `t = 0.5` at ≈ 3.0×, i.e. *at* the §7 gate; the MemFs pure-churn
leg measures 3.039×, within 1.3% of it. The device leg's 1.730× shows how
much skew relaxes it. So the gate's headroom is a property of the
workload, not of the trigger: S19 owns exposing `t` (and the MAINTAIN
slice — the S13 finding), S22 owns tuning both against the gate, S23's
24 h run is where the steady state is met at scale. The canary test
deliberately does not assert that the default *passes* on the pure-churn
shape; tuning that workload until it did would be exactly the silent
narrowing L10 forbids.

## Reproduce

```bash
# AC 1 (needs a real filesystem; refuses tmpfs)
INF_ACCT_DIR=<dir-on-nvme> INF_ACCT_MIB=512 taskset -c 4 \
  cargo bench -p inf-store --bench write_accounting

# AC 2, the measurement
cargo test -p inf-store --test tiered_write_amp --release -- --nocapture

# AC 2, the report (the canary figure comes from the run above)
inf-bench gate-run m4 --unsafe-env --replicates 1 --duration 2 --cells 2 \
  --artifacts-root .artifacts/m4/s16/gate-canary \
  --write-amp-milli 8318 --campaign-note "<harness + artifact path>"
```

`--write-amp-milli` exists because `infinityd` cannot yet create a tiered
namespace (S19 owns `INF.NS`), so the only WA a live node can report today
is the memory-mode `n/a`. The flag requires `--campaign-note`: a number
without provenance is not evidence. When S19 lands, the same gate row is
fed natively from the per-namespace `INFO` scrape — that path is already
implemented and is what produces the `n/a` rows in these reports.
