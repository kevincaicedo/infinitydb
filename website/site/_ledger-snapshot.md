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
| C2 | "Any Redis client library works unchanged" | **Narrowed** → "redis-py, node-redis, go-redis and lettuce pass a smoke suite in CI" | CI `client-smoke` job (infinity-ci.yml) | **Dispositioned-bound (S19 walk, 2026-07-10): bound to the named S02 blocker** — the self-hosted runner registration (user task); flips Allowed on the job's first green run. Not a parking lot: the blocker, owner, and flip condition are named. |
| C3 | "Deterministic-simulation tested: same seed, byte-identical traces; per-key linearizability, TTL, pub/sub delivery, and accounting oracles run nightly" | **Allowed** | `cargo test -p inf-sim` + `inf-sim --verify-determinism` runs 2026-06-12 (m0-smoke hash `0xc038aae0837a1995`); nightly fleet: infinity-dst-nightly.yml | Correctness claim. "≥ 1M simulated seconds nightly" only after the nightly job's first green run. **M2.5-S14 deepened the fleet** (device-stall model → everysec-deferral oracle; combined durable+memory+pub/sub+expiry scenario → L2 memory-volatility/log-scan oracles; boot-storm promoted in), each with a demonstrated planted-bug catch (`m2.5/s14-dst-20260707/`); the *deepened* fleet's nightly-live evidence is still pending S02's runner — do not claim the new scenarios "run nightly" until the first green run. |
| C4 | "10M TTLs across 48 simulated hours fire exactly (zero early, zero missed)" | **Allowed** | `INF_DST_FULL=1 cargo test --release -p inf-store --test expiry` (2026-06-12, dev box, 6 s) | Correctness claim, virtual time. |
| C5 | Any throughput number (ops/s, vs Redis, vs Dragonfly) | **Narrowed** → binding M0 + M1 rows exist and are citable with box + no-turbo tier disclosed | S19 closure campaign 2026-07-10 (`--reference-box`, env-check OK): `.artifacts/m0/1783698386-gate-run/` (4 cells, n=5): unpipelined **3.21× Redis** (512 conns); pipelined natural 2.81–2.89M / all-local **8.03M**; anchor **1.72×** Dragonfly in-run · `.artifacts/m0/1783698588-gate-run/` (8 cells, n=3): natural 3.92–4.02M / all-local **11.15M**; anchor 1.41×; unpipelined 4.21× Redis · `.artifacts/m1/1783699242-gate-run/` (M1 rows bound: baseline pipelined 2.79–2.83M, eviction-pressure 2.21M, TTL-mix 2.47M) | Citable per row with "i7-13700KF, N cells, turbo off" disclosed. **8-cell caveats travel with the 8-cell rows:** the `sqes/submit` tripwire read red (7.8–9.5) on that run — quote its rows as shape-caveated; the pinned spec-shape supplementary read (`m2.5/s19-closure-20260710/phase2-8cell/`) gives natural 5.08–5.57M with loadgen on E-cores and its all-local generator-floored at ~7.0–7.1M (ceiling measured 7.07M, phase0). The ≥ 6M pipelined-gate verdict binds on the natural row — red at both shapes, dispositioned by ADR-0029 → ADR-0035; never publish a natural number without the penalty context. Supersedes the 2026-07-06 rows (`1783391461` +2). **S24 phase-2/3 re-validation (2026-08-11, binding):** wedge re-proven in-run — inf-compare `1786420407` unpipelined 754–782k vs redis 265–287k (≈ 2.7–2.9×) and ≈ 1.3–1.4× dragonfly; m0 `1786419995` pipelined natural 2.844M (July reproduced), anchor 1.68× (both binaries, same night). Environment shift disclosed: the m0 memtier-512 unpipelined ratio reads 2.7–2.8× tonight *for both the m4 and the a1ebcb9 baseline binaries* (redis unchanged) — the July 3.21× is box/generator-state-specific; S25 re-signs wording against the fresh artifacts. |
| C6 | Any latency number (p99/p99.9 under storm, FLUSHALL-under-load, fan-out < 5 ms) | **Narrowed** → the M0 memtier p99.9 row and the full M1 latency row set bind green: expiry storm drains 0.70 s with reads-through-the-storm p99.9 911 µs; FLUSHALL-under-load read p99 607 µs (the flush itself 7.7 ms at 2M keys); pub/sub fan-out PUBLISH p99 0.75 ms to 512 receivers | `.artifacts/m1/1783699242-gate-run/` (S19 closure campaign 2026-07-10, `--reference-box`) + `.artifacts/m0/1783698386-gate-run/` (memtier p99.9 row) + the S10 storm replicates (`m1/1783439386` +2: 0.44–0.49 s / 799–863 µs) | Historic 11.0–11.4 s "drain" numbers are **retracted as measurements** — the harness poll floor (S10; `m2.5/s10-expiry-drain-20260707/results.md`); never quote them as drain times. Two rows stay red-informational, disclosed wherever latency is quoted: KV p99.9 under pub/sub background 2.88 ms vs the 2 ms bar (pre-existing) and the TTL-heavy feature-pressure mix p99.9 2.11 ms vs 2 ms. |
| C7 | "RSS ≤ 1.0× Redis on 10M × (16 B, 64 B)" | **Allowed** → allowed wording: "0.61× Redis RSS at 10M keys × (16 B, 64 B), both engines filled in the same run on the same box" | `.artifacts/m0/1783698386-gate-run/` + `.artifacts/m1/1783699242-gate-run/` (S19 closure campaign 2026-07-10, `--reference-box`, env-check OK; both runs read 0.611×) | Closed by S19. The 8-cell shape reads 0.657× (`m0/1783698588`) — still comfortably under 1.0×; quote the 4-cell 0.61× as the primary number with cells disclosed. **Re-validated 2026-08-11 (S24 phase 2, binding): 0.61× again on the m4 tree** (`m0/1786419995` + the same-night baseline run read identically; compare `1786420407` bytes/key 88.4 vs July 88.3). |
| C8 | "Eviction hit-rate parity with Redis LFU (±2 pp, zipfian)" | **Allowed** → allowed wording: "LFU hit-rate parity with Redis under a zipfian trace: 96.19% vs 96.19% (+0.00 pp), 1M keyspace, 5M ops, same trace both engines" | `.artifacts/m1/1783699242-gate-run/` (S19 closure campaign 2026-07-10, binding) | Closed by S19 — exact parity, well inside ±2 pp. **Re-validated 2026-08-11 (S24 phase 2, binding): 0.00 pp again on the m4 tree** (`m1/1786420253`). |
| C9 | "Docker image < 30 MB, serves redis-cli out of the box" | **Evidence-pending — bound to the named S02 blocker** (S19 walk, 2026-07-10) | — | infinity-release.yml measures/gates this on the release-pipeline dry-run, which needs the S02 runner (user task); flips on that run. Dev-box build: 0.8 MB image, serves redis-cli **with the bundled `deploy/seccomp/infinitydb-seccomp.json` profile** — io_uring requires it under Docker's default seccomp, so "out of the box" carries that documented requirement (see docs/deployment.md). |
| C10 | "A slow pub/sub subscriber is disconnected at the configured output cap, never bloats the server" | **Allowed** | `node_e2e::slow_subscriber_hits_the_output_cap_and_dies` + `gate-run m1` slow-subscriber row (killed=true, dev run 2026-06-12) | Correctness/behavioral claim. |
| C11 | "Memory accounting is exact: per-domain attribution reconciles, divergence from RSS > 10% fails CI" | **Allowed** → allowed wording: "per-domain memory attribution reconciles against RSS in every binding gate campaign — 2.7% divergence on the 10M-key memory fill, 1.0% on the durable fill (log domains included); > 10% fails the gate" | `.artifacts/m0/1783698386-gate-run/` (2.7%) + `.artifacts/m2/1783699515-gate-run/` (1.04%, gate row PASS) — S19 closure campaign 2026-07-10 | Closed by S19: the divergence gate ran binding on the reference box in both campaigns. |

## Rows — v0.2.0-alpha.1 (M2)

Box for every performance row: the **user-designated reference box**
(ADR-0022 D1) — HomeLab i7-13700KF, pinned P-cores, `performance` governor,
**ADATA LEGEND 700 consumer Gen3 DRAM-less NVMe** (§19 profiles Gen4; the
deviation is named in each row). Campaign artifacts:
`infinitydb/.artifacts/m2/` (2026-07-05, clean tree, env-check green).

| # | Claim (allowed wording) | Status | Artifact | Notes |
|---|---|---|---|---|
| C12 | "Durability is deterministically tested: 10,000 simulated power-cut/disk-fault seeds, zero durability-oracle violations — every `always`-acked write survives, `everysec` loses at most one second (simulated time); every seed replays byte-identically — and on the frame-v2 tree, zero recovery refusals across the same 10,000 seeds" | **Allowed** (re-signed against the v2 tree, S19 2026-07-10) | `m2.5/s12-frame-seq-20260709/` (frame v2: 0 refusals / 0 violations at 10k identical seeds, paired against the re-measured 2.31% v1 baseline) + `dst-sweep-10k-s22-20260705/` (the release-code regeneration) | Correctness claim. The old 1.33% legal-refusal disclosure is **superseded by frame v2** (ADR-0031) — the honest historical note stays in the artifact, not in public copy. |
| C13 | "The crash matrix — 8 named fault points × fsync policies × workloads, kill-and-recover with digest verification — is green at 256 seeds per combination; fsync failure is fail-stop" | **Allowed** (re-validated post-format-change: S12 re-ran the matrix green on the frame-v2 tree, 2026-07-09) | `m2.5/s12-frame-seq-20260709/` (v2 tree) + crash-matrix run 2026-07-05 + `tests/crash-matrix/m2.toml` | Correctness claim. |
| C14 | "A 10 GB node (8 cells) cold-boots to serving in under 10 seconds on the reference box (9.8 s, cold page cache, 3 replicates < 1% spread)" | **Allowed** | `parallel-boot-cold-20260705/` + the S08 re-run (7.3 s, `m2.5/s08-readahead-20260706/`) | Say "under 10 s measured / 15 s gate"; device is Gen3 DRAM-less (named). Release re-validation (cold-boot harness re-run) rides the S04 pre-tag campaign — flagged at the S19 walk. |
| C15 | "Memory namespaces pay zero for durability: A/B vs the pre-durability build shows ≤ 1% on every gate mix (worst 0.63%, p99.9 same-bucket-or-better), and memory-only runs append zero log records (report-enforced)" | **Allowed** | binding report `.artifacts/m2/1783232863-gate-run/` (A/B halves) + `.artifacts/m2/1783699515-gate-run/` (zero-log-records half re-validated S19 2026-07-10, gate row PASS) | Signed deltas + LogHistogram ~3% quantization in-report; one retained repeat showed ±1–2-bucket latency drift (disclosed there). **S04 re-validation item:** the A/B halves need the pre-M2 baseline binary rebuilt (`--baseline-bin`) in the pre-tag campaign — flagged at the S19 walk. |
| C16 | "Checkpoints are fork-free and memory-flat: continuous checkpoint cycling under a saturating durable write mix costs ≤ 15 MiB of peak RSS vs a no-checkpoint control (a fork/COW rewrite would cost the dataset)" | **Allowed** (re-validated S19 2026-07-10: pressure RSS 239–241 MiB vs 235–238 MiB control, 68–88 ckpt cycles/rep, gate row PASS) | `.artifacts/m2/1783699515-gate-run/` + S22 binding report (`ckpt_rss_overhead` 14.4 MiB, 176 cycles in-row) | The anti-BGREWRITEAOF *memory* claim. One pressure rep logged 1,595 request errors of 17.5M ops (0.009%) — disclosed; the control's own p99.9 excursions (36–426 ms) are the C17 device class. |
| C17 | "Checkpoint-under-load foreground p99.9 < 2 ms" | **Narrowed** → "checkpointing adds no measurable foreground p99.9 over the no-checkpoint control on our reference device, and holds p99.9 ≈ 1.1 ms under 324 checkpoint cycles at the loop tier (tmpfs isolation row)" | S12 tmpfs row (`1783049677-gate-run`) + S22 device rows | Absolute 2 ms bar on the Gen3 device: the *control itself* stalls 36–45 ms (device class; the S19 re-read reproduced it — control p99.9 excursions to 426 ms with checkpoints **off**). Evidence-pending (Gen4) for the absolute wording — ADR-0022 D4.3; **dated plan: S18 decision memo delivered 2026-07-10 (`m2.5/s19-closure-20260710/runbook.md`), user decision due at the S04 tag.** |
| C18 | "`always` group commit: ≥ 300k grouped writes/s with honest fsync histograms" | **Evidence-pending (Gen4 device)** | binding report `1783232093`/`1783232863` (128–156k w/s post-fix, ratio 100, fsync p50 2.5–3.1 ms attached) + `redis-aof-context-20260705` (Redis AOF-always 373k same box/shape) + driver-tier 778k @ group 1024 (same device) + tmpfs full-stack 2.34M + M2.5-S07 A/B `m2.5/s07-ab-20260706/verdict.md` (formation instrumented: group p50 = arrivals-during-flush at ~100% collection efficiency; two-in-flight pipeline Rejected, −3.7% w/s, fsync p99 +37%) | Do not publish an `always` throughput number yet. The citable story is the mechanism: one durability fsync in flight (ADR-0022 D3, re-validated by the S07 A/B — pipelining *splits* groups on this device) collecting every arrival during the flush (group p50 ≈ per-cell rate × fsync p50, measured 101 vs 97 predicted). D8.6's dead-time premise is Rejected (ADR-0026 D4); the 300k absolute is owned by S18/Gen4 (flush latency and arrival rate move together). **Dated plan: S18 decision memo delivered 2026-07-10 (`m2.5/s19-closure-20260710/runbook.md`), user decision due at the S04 tag.** |
| C19 | "everysec penalty < 10%" | **Narrowed** (S19, 2026-07-10) → allowed wording: "`everysec` durability costs ≈ 10–11% of throughput vs a memory-mode namespace on our reference box (consumer Gen3 DRAM-less NVMe; interleaved ABBA, 3 replicates)" — the "< 10%" absolute stays **Evidence-pending (Gen4 device)** | `.artifacts/m2/1783699515-gate-run/` (10.1–10.9%, memory-ns 2.07M vs everysec 1.84–1.86M) — supersedes the M2-era 30–45% device-bound reading | The Phase-P/H levers moved this row from 30–45% to ~10.5%: the log stream no longer saturates the device at the higher throughput's duty cycle. The < 10% gate misses by ~0.1–0.9 pp on Gen3; re-read rides the S04 pre-tag campaign and S18/Gen4 (ADR-0022 D4.2 disposition narrows accordingly). **NOT re-validated by the S24 phase-2 leg (2026-08-11): the row read 33.6% on the device** (everysec 1.38–1.65M, p999 3–14 ms; `m2/1786421205`) while the same binary the same night on tmpfs read 1.96–1.99M — the delta is drive state (DRAM-less SLC at 40% fill; readiness F20), not the code. **This wording reverts to Evidence-pending for the v0.4.0 announcement** unless a post-fstrim re-read restores it; do not quote ≈ 10–11% until then. |
| C20 | "Recovery replays ≥ 1 GB/s per cell" | **Narrowed** → "≥ 1 GB/s per cell on the steady-state (checkpoint + tail) boot shape, cold" | `m2.5/s08-readahead-20260706/` (ick-tail cold 1.07–1.09 GiB/s = 1.15–1.17 GB/s, 5 replicates × ABBA legs, digests identical; tail-only full-log worst case 0.91–0.92 GiB/s = 0.98–0.99 GB/s) | Serial read∘apply is eliminated (ADR-0028: prefetch thread + `.ick` footer probe; WILLNEED Rejected by A/B) — cold = warm on both shapes. The worst-case row sits on this box's apply-CPU ceiling at no-turbo clocks; that ceiling, not I/O, is what a full-log-replay claim would need to name. Multi-cell boots keep prefetch off (regime split measured: 7.3 s vs 10.0 s; Gen4 re-eval at S18). Release re-validation (replay harness re-run) rides the S04 pre-tag campaign — flagged at the S19 walk. |
| C21 | "Memory attribution stays exact with the durable plane live: sum(domains) — including log staging and checkpoint buffers — within 1.1% of RSS on a 1 GB durable fill" | **Allowed** (re-validated S19 2026-07-10: 1.04%) | `.artifacts/m2/1783699515-gate-run/` attribution row (supersedes the S22-M2 artifact for re-validation purposes) | |
| C22 | "M0/M1 behavior carried: all M1 binding rows equal-or-better on the M2 tree (worst +1.5%)" | **Allowed** (re-validated S19 2026-07-10: the full m0+m1 gate sets re-ran binding on the v2+levers tree — every previously-green row green, throughput rows within ≤ 5% of the ADR-0033 baselines or better) | `.artifacts/m0/1783698386` + `.artifacts/m1/1783699242` (S19 closure campaign) | Carried disclosed failures: ~~M1 expiry-debt drain~~ (closed by S10 — instrument floor; see C6), M0-era absolute gates per ADR-0006/0007 → the pipelined-gate row is dispositioned by ADR-0029/ADR-0035. Red-informational rows named in C6. |

## Rows — v0.3.0-alpha.1 (M2.5 + M3, seeded 2026-07-17 by the S26 package; the S25 reference campaign re-validates every row before the tag)

| # | Claim (allowed wording) | Status | Artifact | Notes |
|---|---|---|---|---|
| C23 | "22 `JSON.*` commands declared — 7 `full`, 15 `partial` with documented deviations — byte-diff-verified against digest-pinned RedisJSON (Redis Stack 7.4.0-v8): 84 cases × RESP2+RESP3 = 168 executions, 148 exact, 20 justified deviations, 0 failures" | **Allowed** | `infinitydb/.artifacts/m3/s20-s21-20260716/redisjson-oracle.txt` + `s22-20260716/oracle-probes.txt` + generated `docs/compat-matrix.md` (JSON section, CI staleness gate) | Correctness claim — dev-tier oracle is valid evidence (environment named). Never round the surface up to "full RedisJSON compatibility". |
| C24 | "Documents are stored as a compact binary tape (`idoc` v1): 1.108× msgpack and 0.764× JSON text aggregate density on the reference corpus shapes" | **Allowed** | `infinitydb/.artifacts/m3/s02-idoc-20260710/` (S20 generator re-assertion) | Deterministic byte counts — hardware-independent; per-shape worst cases live in the artifact (small-200B 0.905× text, wide-array 1.119× msgpack) and travel with any per-shape quote. |
| C25 | "Document reads never run a JSON text parser — profile-proven: zero parser/path-compiler symbols under a live `JSON.GET` load" | **Allowed** (dev-tier proof; reference campaign re-proves via `scripts/check-doc-read-profile.sh`) | `infinitydb/.artifacts/m3/s25-dev-20260716/` (1,659 profile rows, mechanical grep = 0) | Correctness-of-architecture claim; the CI check makes regressions fail loudly. |
| C26 | "Every path mutation is one delta log record (0.066× full-document log volume on the 64-op mutation mix); 10,000 simulated document power-cut seeds: zero durability-oracle and zero replay-equivalence violations, replayed state byte-exact including lineage and version" | **Allowed** | `infinitydb/.artifacts/m3/s16-s17-20260712/` (log volume) + `s23-s24-20260716/doc-sweep-10k.txt` (30k equivalence checks, 150k documents compared) | Correctness claim, simulated time; the harness embeds fuzz-derived pathological documents (disclosed per seed). |
| C27 | "Document memory is attributed always-on (tape/tree/slack/path-cache domains, per namespace); attribution reconciles with RSS at 0.056% divergence on a 65k-document load (10% fails CI)" | **Allowed** | `infinitydb/.artifacts/m3/s18-s19-20260713/` | Correctness claim (Linux CI). Distinct from the *memory-gate* rows below. |
| C28 | Document read latency vs GET (gate ≤ 1.5×), document write throughput vs SET (gate ≥ 0.70×) | **Evidence-pending (reference box)** | dev tier 2026-07-17 (`jset-server-20260717/wire-final/`): reads 1.217×/1.240×, write 0.6985–0.7252 — non-citable | The write row is ON the gate line at dev tier with the cost fully attributed; ADR-0050 D3 owns the disposition either way. Do not publish any of these numbers before the campaign. |
| C29 | Document RSS ≤ 1.5× serialized and ≤ 0.7× RedisJSON (frozen corpus v2, write-once scope disclosed) | **Evidence-pending (reference box)** | dev tier 2026-07-16 (`s25-remediation-20260716/`): 1.025–1.043× / 0.375–0.381× mixed — non-citable | The corpus freeze (ADR-0046 D3) and the write-once scope note travel with any future quote; per-shape rows are diagnostics only. |
| C30 | 24 h document soak (RSS slope < 0.5%) · fuzz ≥ 24 h/target | **Evidence-pending** | harness ready (`scripts/soak-m3.sh`; nightly fuzz on the S02 runner) | Bound to the named human/machine items (release-readiness inventory 1–3, 6). |

## Standing obligations

- ~~**M0-R2** (reference-box campaign)~~ — **retired 2026-07-05** by the
  ADR-0022 D1 reference-box designation; the M0/M1 gate sets ran binding on
  the designated box inside the S22 campaign (C22). Device-bound M2 rows
  remain Evidence-pending on a Gen4 device (C17, C19, C20).
- **M2.5 owns closure (ADR-0023) — S19 walk executed 2026-07-10:** the
  former `Evidence-pending` set is dispositioned: **C7, C8, C11 → Allowed**
  (binding, S19 campaign); **C19 → Narrowed** at the measured ≈ 10–11%
  (the < 10% absolute stays Gen4-bound); **C5, C6 → Narrowed** with the
  full M0+M1+8-cell row set; **C12 re-signed** on the frame-v2 tree;
  **C17, C18 → bound to the dated S18/Gen4 plan** (decision memo
  delivered, user decision due at the S04 tag); **C2, C9 → bound to the
  named S02 runner blocker** (flip conditions stated in-row). Zero rows
  remain undispositioned; the release re-validation items for S04 are
  flagged in C14, C15, C20.
- **Cross-cell remote penalty (ADR-0025, owned by M2.5-S21 → S19):**
  binding trajectory: 60.4% baseline (2026-07-06) → 54.6–55.7% (ADR-0030
  levers) → ~64% after parse-batch grew all-local faster (ADR-0033's
  predicted arithmetic artifact) → **S19 8-cell re-reads 2026-07-10**:
  64.7% harness shape (tripwire-caveated), spec shape boundable-only
  (all-local generator-floored at the measured 7.07M E-core ceiling).
  The lever list is dispositioned to exhaustion (2 Accepted, 3 Rejected,
  1 inconclusive — ADR-0030/0033/0034); the **anchor holds with margin,
  1.72× (4 cells) / 1.41× (8 cells) Dragonfly in-run** — an architecture
  claim may cite it with the tier disclosed. Disposition: **ADR-0035
  (Proposed — user ratification)** re-expresses the staged gate as
  anchored gate + tracked ratio; the ≤ 25% real target stays carried
  (ADR-0027) with the pump-bypass restructure as its named structural
  lever. No ≤-40%/≤-25% claim either way.
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
