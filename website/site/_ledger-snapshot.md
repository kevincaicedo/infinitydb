# InfinityDB Claim Ledger

Master plan §19 made operational (M1-S16). **Every number or comparative
statement in public copy must have a row here with status `Allowed`, citing
its artifact.** A claim absent from this ledger does not exist. Statuses:

- **Allowed** — artifact exists, run validity rules held, claim may ship.
- **Narrowed** — a weaker form is allowed; the allowed wording is in the row.
- **Rejected** — evidence contradicts it or the run was invalid.
- **Evidence-pending** — no qualifying artifact yet. Treat as Rejected for
  public copy.

Run-validity rules (constitutional, §19): 3–5 replicates · clean tree ·
governor/EPP recorded · thermals monitored · load-gen saturation disposition
· tripwires green in the same run · competitors on the same box and workload
· flamegraph + memory attribution attached. **Dev-box runs can never back a
performance claim** — only the Linux reference box (M0-R2) can. Correctness
claims (byte-compat, determinism, test counts) may cite dev-tier runs, with
the environment named.

Re-validation: every release re-runs the artifact behind each `Allowed` row;
a row whose artifact has not been re-validated for the release being
announced reverts to Evidence-pending for that announcement.

## Review checklist (release manager, per announcement)

1. Extract every number/comparative from the draft announcement.
2. Each must match an `Allowed` row's wording — narrowings are not optional.
3. Each cited artifact must be from the release's own re-validation run.
4. Tripwires in the citing run must be green; otherwise the row flips.
5. Sign the announcement with the ledger commit hash it was checked against.

---

## Rows — v0.1.0-alpha.1 (M1)

| # | Claim (allowed wording) | Status | Artifact | Notes |
|---|---|---|---|---|
| C1 | "Serves the Redis protocol: 65 commands declared — 47 `full`, 15 `partial` with documented deviations — byte-diff-verified against Redis 8.0.5 (378 byte-compared cases, 32 documented deviations, 0 failures)" | **Allowed** | [`docs/compat-matrix.md`](compat-matrix.md) (generated; CI staleness gate) + `cargo test -p compat` run 2026-06-12 vs local redis-server 8.0.5 | Correctness claim — dev-tier oracle is valid evidence. CI re-runs against the pinned `redis:8.0.5` container. Do **not** round up to "~90 commands". |
| C2 | "Any Redis client library works unchanged" | **Narrowed** → "redis-py, node-redis, go-redis and lettuce pass a smoke suite in CI" | CI `client-smoke` job (infinity-ci.yml) | Evidence-pending until the job's first green run; then Allowed with the four libraries named. |
| C3 | "Deterministic-simulation tested: same seed, byte-identical traces; per-key linearizability, TTL, pub/sub delivery, and accounting oracles run nightly" | **Allowed** | `cargo test -p inf-sim` + `inf-sim --verify-determinism` runs 2026-06-12 (m0-smoke hash `0xc038aae0837a1995`); nightly fleet: infinity-dst-nightly.yml | Correctness claim. "≥ 1M simulated seconds nightly" only after the nightly job's first green run. |
| C4 | "10M TTLs across 48 simulated hours fire exactly (zero early, zero missed)" | **Allowed** | `INF_DST_FULL=1 cargo test --release -p inf-store --test expiry` (2026-06-12, dev box, 6 s) | Correctness claim, virtual time. |
| C5 | Any throughput number (ops/s, vs Redis, vs Dragonfly) | **Narrowed** → binding M0 rows exist and are citable with box + no-turbo tier disclosed | `.artifacts/m0/1783391461-gate-run/` (env-check OK, reference-box, 5 replicates): unpipelined 2.76× Redis (512 conns); pipelined natural 2.51M / all-local 6.21M ops/s (4 cells); anchor 1.44× Dragonfly in-run; repeats `1783392817`/`1783393859` | Citable per row with "i7-13700KF, 4 cells, turbo off" disclosed. The pipelined-vs-≥6M verdict itself stays red, dispositioned by ADR-0029 (carried — one debt with the S21 penalty); never publish the natural number without the penalty context. M1 rows still need their own binding runs. |
| C6 | Any latency number (p99/p99.9 under storm, FLUSHALL-under-load, fan-out < 5 ms) | **Narrowed** → the M0 memtier p99.9 row binds; M1 storm rows still pending | `.artifacts/m0/1783391461-gate-run/` (p99.9 975 µs < 3 ms, memtier 8 threads, binding) | M1-S17 storm/FLUSHALL/fan-out rows bind only with `--reference-box` runs of `gate-run m1` (not yet re-run on the restored env). |
| C7 | "RSS ≤ 1.0× Redis on 10M × (16 B, 64 B)" | **Evidence-pending** | — | Gate-run m1 fill leg on the reference box. M0 dev-tier showed 0.608× — non-citable, do not quote. |
| C8 | "Eviction hit-rate parity with Redis LFU (±2 pp, zipfian)" | **Evidence-pending** | — | Trace-replay tooling now exists (`inf-bench zipfian` / `gate-run m1 --with-zipfian`); a dev-box run shows InfinityDB at parity-or-better, but the binding number is a reference-box run (M0-R2). |
| C9 | "Docker image < 30 MB, serves redis-cli out of the box" | **Evidence-pending** | — | infinity-release.yml measures/gates this on the first tag run. Dev-box build: 0.8 MB image, serves redis-cli **with the bundled `deploy/seccomp/infinitydb-seccomp.json` profile** — io_uring requires it under Docker's default seccomp, so "out of the box" carries that documented requirement (see docs/deployment.md). |
| C10 | "A slow pub/sub subscriber is disconnected at the configured output cap, never bloats the server" | **Allowed** | `node_e2e::slow_subscriber_hits_the_output_cap_and_dies` + `gate-run m1` slow-subscriber row (killed=true, dev run 2026-06-12) | Correctness/behavioral claim. |
| C11 | "Memory accounting is exact: per-domain attribution reconciles, divergence from RSS > 10% fails CI" | **Narrowed** → "per-domain memory attribution is built in and reconciliation-tested" | attribution tests + sim accounting oracle | The RSS-divergence CI gate number belongs to reference-box runs. |

## Rows — v0.2.0-alpha.1 (M2)

Box for every performance row: the **user-designated reference box**
(ADR-0022 D1) — HomeLab i7-13700KF, pinned P-cores, `performance` governor,
**ADATA LEGEND 700 consumer Gen3 DRAM-less NVMe** (§19 profiles Gen4; the
deviation is named in each row). Campaign artifacts:
`infinitydb/.artifacts/m2/` (2026-07-05, clean tree, env-check green).

| # | Claim (allowed wording) | Status | Artifact | Notes |
|---|---|---|---|---|
| C12 | "Durability is deterministically tested: 10,000 simulated power-cut/disk-fault seeds, zero durability-oracle violations — every `always`-acked write survives, `everysec` loses at most one second (simulated time); every seed replays byte-identically" | **Allowed** | `dst-sweep-10k-s22-20260705/` (regenerated against the release code) | Correctness claim. Disclose alongside: 1.33% of boots legally refuse with a named corruption error (survival-audited; ADR-0021). |
| C13 | "The crash matrix — 8 named fault points × fsync policies × workloads, kill-and-recover with digest verification — is green at 256 seeds per combination; fsync failure is fail-stop" | **Allowed** | crash-matrix run 2026-07-05 + `tests/crash-matrix/m2.toml` | Correctness claim. |
| C14 | "A 10 GB node (8 cells) cold-boots to serving in under 10 seconds on the reference box (9.8 s, cold page cache, 3 replicates < 1% spread)" | **Allowed** | `parallel-boot-cold-20260705/` | Say "under 10 s measured / 15 s gate"; device is Gen3 DRAM-less (named). |
| C15 | "Memory namespaces pay zero for durability: A/B vs the pre-durability build shows ≤ 1% on every gate mix (worst 0.63%, p99.9 same-bucket-or-better), and memory-only runs append zero log records (report-enforced)" | **Allowed** | binding report `.artifacts/m2/1783232863-gate-run/` | Signed deltas + LogHistogram ~3% quantization in-report; one retained repeat showed ±1–2-bucket latency drift (disclosed there). |
| C16 | "Checkpoints are fork-free and memory-flat: continuous checkpoint cycling under a saturating durable write mix costs ≤ 15 MiB of peak RSS vs a no-checkpoint control (a fork/COW rewrite would cost the dataset)" | **Allowed** | S22 binding report (`ckpt_rss_overhead` 14.4 MiB, 176 cycles in-row) + S12 artifacts | The anti-BGREWRITEAOF *memory* claim. |
| C17 | "Checkpoint-under-load foreground p99.9 < 2 ms" | **Narrowed** → "checkpointing adds no measurable foreground p99.9 over the no-checkpoint control on our reference device, and holds p99.9 ≈ 1.1 ms under 324 checkpoint cycles at the loop tier (tmpfs isolation row)" | S12 tmpfs row (`1783049677-gate-run`) + S22 device rows | Absolute 2 ms bar on the Gen3 device: the *control itself* stalls 36–45 ms (device class). Evidence-pending (Gen4) for the absolute wording — ADR-0022 D4.3. |
| C18 | "`always` group commit: ≥ 300k grouped writes/s with honest fsync histograms" | **Evidence-pending (Gen4 device)** | binding report `1783232093`/`1783232863` (128–156k w/s post-fix, ratio 100, fsync p50 2.5–3.1 ms attached) + `redis-aof-context-20260705` (Redis AOF-always 373k same box/shape) + driver-tier 778k @ group 1024 (same device) + tmpfs full-stack 2.34M + M2.5-S07 A/B `m2.5/s07-ab-20260706/verdict.md` (formation instrumented: group p50 = arrivals-during-flush at ~100% collection efficiency; two-in-flight pipeline Rejected, −3.7% w/s, fsync p99 +37%) | Do not publish an `always` throughput number yet. The citable story is the mechanism: one durability fsync in flight (ADR-0022 D3, re-validated by the S07 A/B — pipelining *splits* groups on this device) collecting every arrival during the flush (group p50 ≈ per-cell rate × fsync p50, measured 101 vs 97 predicted). D8.6's dead-time premise is Rejected (ADR-0026 D4); the 300k absolute is owned by S18/Gen4 (flush latency and arrival rate move together). |
| C19 | "everysec penalty < 10%" | **Evidence-pending (Gen4 device)** | S22 binding report (device row, penalty 30–45% device-write-bound, spread ~57%) + tmpfs rehearsal ~9.6% | The saturating 512 B 1:1 log stream exceeds what the Gen3 DRAM-less device absorbs flat; ADR-0022 D4.2. Do not publish a penalty number. |
| C20 | "Recovery replays ≥ 1 GB/s per cell" | **Narrowed** → "≥ 1 GB/s per cell on the steady-state (checkpoint + tail) boot shape, cold" | `m2.5/s08-readahead-20260706/` (ick-tail cold 1.07–1.09 GiB/s = 1.15–1.17 GB/s, 5 replicates × ABBA legs, digests identical; tail-only full-log worst case 0.91–0.92 GiB/s = 0.98–0.99 GB/s) | Serial read∘apply is eliminated (ADR-0028: prefetch thread + `.ick` footer probe; WILLNEED Rejected by A/B) — cold = warm on both shapes. The worst-case row sits on this box's apply-CPU ceiling at no-turbo clocks; that ceiling, not I/O, is what a full-log-replay claim would need to name. Multi-cell boots keep prefetch off (regime split measured: 7.3 s vs 10.0 s; Gen4 re-eval at S18). |
| C21 | "Memory attribution stays exact with the durable plane live: sum(domains) — including log staging and checkpoint buffers — within 1.1% of RSS on a 1 GB durable fill" | **Allowed** | S22 binding report attribution row | |
| C22 | "M0/M1 behavior carried: all M1 binding rows equal-or-better on the M2 tree (worst +1.5%)" | **Allowed** | `.artifacts/m0/1783228176` + `.artifacts/m1/1783228421` vs archived M1-era runs | Carried disclosed failures: M1 expiry-debt drain 11.34 s vs 10 s gate (pre-existing), M0-era absolute gates per ADR-0006/0007. |

## Standing obligations

- ~~**M0-R2** (reference-box campaign)~~ — **retired 2026-07-05** by the
  ADR-0022 D1 reference-box designation; the M0/M1 gate sets ran binding on
  the designated box inside the S22 campaign (C22). Device-bound M2 rows
  remain Evidence-pending on a Gen4 device (C17, C19, C20).
- **M2.5 owns closure (ADR-0023):** every `Evidence-pending` row above (C2,
  C5–C9, C11, C17–C20) and the release-blocker list below must leave M2.5
  **dispositioned** — `Allowed`/`Narrowed`/`Rejected`, or bound to the named
  Gen4 hardware plan (M2.5-S18). Zero undispositioned rows is an M2.5 STOP
  gate (M2.5-S19).
- **Cross-cell remote penalty (ADR-0025, owned by M2.5-S21 → Phase H):**
  binding trajectory: 60.4% baseline (2026-07-06, 5 replicates) →
  **55.7% with the Phase-H lever slice** (2026-07-07, ADR-0030 — int-hash
  gates, allocation-free deferral, fabric-apply staged prefetch; ABA legs
  `m0/1783400704`/`1783400906`, n=3 each, natural 2.461M → **2.945M**
  +19.6%). The **anchor rose 1.44× → 1.74× Dragonfly** (binding, in-run) —
  an architecture claim may cite it with the tier disclosed. The ≤ 40%
  staged gate is **still open** (residuals named in ADR-0030: plane
  dispatch machinery, codec, fixed-rate kernel); the ≤ 25% real target
  stays carried per ADR-0027. No ≤-40%/≤-25% claim; S19 re-reads the row
  binding at `--cells 8` (ADR-0029).
- **Comparative instrument (ADR-0025):** from M2.5, comparative rows cite
  `inf-compare` artifacts (independent generators, competitors in-run,
  tier-bannered report). `inf-bench` rows prove gates; where a workload
  exceeds the external generators, the row names its harness — no silent
  substitution.
- **v0.2.0-alpha.1 release blockers (human tasks — now M2.5-E1):** 24 h soak
  (`infinitydb/scripts/soak-m2.sh`), first live-runner CI passes
  (fuzz-nightly hour, DST-nightly sweep, crash-matrix job, release
  pipeline on tag push), and the boot-wedge stop-item disposition
  (ADR-0022 D7).
- `v0.1.0-alpha.1` was never tagged (blocked on M0-R2) and is superseded by
  `v0.2.0-alpha.1`; its announcement draft stays unpublished. The M1-era
  correctness rows (C1–C4, C10) remain citable; C5–C9/C11 resolve under the
  designated-box rules going forward.
