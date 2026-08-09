# M4 cold-read gate investigation: why loaded cold p99 is 12.8 ms, and what to do

Date: 2026-08-08 · Evidence tier: DEV-TIER analysis of
`.artifacts/v0.4.0/soak-unified-20260807-0227/` (research, non-binding;
feeds readiness §9 D8 and S24 phase-4 prep). No code was changed by this
investigation; the instrument defects it names are fixed separately
(readiness F13).

## 1. Problem statement

M4 §7 gate: "Cold reads: p99 < 1.5 ms on NVMe, cold-read split histogram
under loaded zipfian rows (not idle drains)" (M4 plan §7). The week-4
idle-drain measurement passed with 7.5× headroom (163–199 µs). The 24 h
unified soak's tiered leg under full three-plane load measured
`tiering_cold_p50_us:1087 / p99:12799 / p999:47103` — p99 **8.5× over**
the gate, and even the **median** is 5.5–6.7× the idle-drain *p99*.

**One correction to the prior session's framing:** `fsync_latency_p999_us:
516095` is **516 ms**, not half a millisecond — and `fsync_latency_p50_us:
5887` means the *median* durable-log fsync took 5.9 ms. This materially
strengthens the device-interference hypothesis.

## 2. What the artifact shows

### 2.1 Time series

`samples.csv` (8,559 rows, 10 s cadence) **has no cold-latency column** —
sub-leg cold-p99 correlation is not extractable from this artifact (named
gap, §4). The 60 per-leg gate-run reports (`legs/*/report.md`) carry
worst-cell `tiering_cold_*`, but cumulative-since-boot:

| leg | cold p50 µs | cold p99 µs | cold p999 µs |
|---|---|---|---|
| 1 (fill + demote catch-up) | 1087 | **23,551** | **208,895** |
| 2–3 | 1119 | 15,615 → 14,847 | 73,727 → 61,439 |
| ~10 onward (steady) | 1119 | 13,311 → 13,055 | 49,151 → 47,103 |
| end (`info-end.txt`) | 1087 | 12,799 | 47,103 |

Reading: **13 ms p99 / 1.1 ms p50 is the stationary state for the whole
run**, not a late-run degradation. The worst tail sits in leg 1 — the
post-fill demotion catch-up, i.e. the heaviest sustained-write window
produced the worst cold tail (p999 209 ms): within-artifact evidence of
write→read interference.

Write activity: a **continuous** component — tier disk grows at median
~2.5 MB/s demote flush, checkpoints ~0.5/s node-wide — plus a **bursty**
component: 189/8,558 windows with |Δdisk| > 100 MB (up to +1.40 GB/10 s
compaction copy-forward) and 181 retirement drops (median −1.05 GB), a
compaction/retirement cycle roughly every 8 minutes. Since the *median*
cold read is 1.1 ms all run, the dominant interference is the continuous
load (flush + WAL + fsyncs + checkpoints); the bursts plausibly own the
47–209 ms p999 — unadjudicated at 25-min leg cadence (§4.2).

### 2.2 Loadgen-side agreement

Per-leg client blocks: combined client p99 24–44 ms (ycsb-a), 17–39 ms
(ycsb-b), max 1.7–1.9 s — consistent with the server's 13 ms cold split
(combined includes RAM hits; cold-touch rate is high; at pipeline 8 one
13 ms cold read holds up to 8 pipelined commands). The two instruments
agree directionally; the gate correctly reads the server split histogram.

### 2.3 Instrument findings (fix before S24)

- `cold_read_p99_us:85899345919` — the known u32-wraparound field
  (readiness F13, fixed separately).
- `coalesce_ratio_milli:1000` **contradicts ADR-0055 D5's definition.**
  Raw counters: `enqueued 102,311,118 − issued 102,310,691 = 427` — i.e.
  essentially **zero coalescing all run** (~0.0004%). Per D5 the ratio
  should read ~0 milli, but the field reports `issued/enqueued` — inverted
  semantics or wiring bug. A third broken field in the pinned
  `SPLIT_FIELDS` block (`inf-server/src/admin.rs:418-431`).
- `pool_dry` / `queue_full` exist in `ColdReadCounters`
  (`cold.rs:160-171`) but are not surfaced in INFO — pool-sizing stalls
  can't be fully excluded from artifacts (QD ≈ 5 vs the 64-buffer
  ceiling makes them very unlikely here).

## 3. Hypothesis ranking

### (a) Device-level read/write interference — RANK 1, strongly supported; primary mechanism

For: (i) fsync p50 5,887 µs / p99 59,391 µs / p999 516,095 µs measured at
io_uring CQE completion — below all InfinityDB scheduling — on the same
device; 10.28 M fsyncs / 86.5 ks ≈ **119/s average (358/s at end)**; at a
5.9 ms median the device is servicing or queueing an NVMe FLUSH for a
majority of wall time. On a DRAM-less HMB drive, FLUSH persists write
buffer + FTL map state; concurrent O_DIRECT reads wait ms-scale — exactly
the observed cold p50 of 1.1 ms. (ii) The entire cold distribution shifts
right ~7–10× vs idle — continuous interference, not spikes. (iii)
Little's-law consistency: 1,183 cold reads/s × ~1.5 ms ≈ 1.8 average
in-flight, matching `cold_read_qd_p99:5` — latency is per-IO device wait,
not queue depth. (iv) Prior documented pathology of this exact device:
S11's flush AC recorded a mid-rep **0.459×** "DRAM-less device's
sustained-write collapse". (v) Leg-1 worst tail during the heaviest write
window. (vi) The device is actually the **476.9 GiB** LEGEND 700 SKU
(lsblk; not 1 TB as previously assumed) — fewer dies, less SLC, ~82 GB
free at launch → minimal dynamic SLC — worst-case sustained mixed
behavior. Also disclosed in `run-notes.md`: 6.2/8 GB swap in use on the
same device (desktop traffic — unquantified contamination).

### (d) fsync/tier-read sharing in our submission path — software half refuted; device half merges into (a)

One cell = one ring (`inf-runtime/src/uring.rs:51`); `LogFsync` and
`TierRead` share it. But submission is asynchronous and the reactor is
healthy (`loop_iter_p999_us:279`, sqes/submit 7.6) — no SQ stall. The
structural consequence that does matter: with one ring and one device
there is **no I/O-class separation lever** between durability barriers
and cold reads — which is what solutions 1–2 exploit.

### (b) QD-cap/coalescing mistuning — refuted as cause

Cap is 64 (`cold.rs:132`; ADR-0055 D2); observed QD p99 = 5 — the cap
never binds, and "QD too high" is equally refuted at QD ≈ 1–5.
Coalescing did nothing (427 non-issued intents in 102 M — no adjacency
exists in point-read zipfian over 10.5 M keys; the S10 A/B's 35.6% win
was on batched ×16 load). Absence of merging adds no latency.

### (c) Upstream scheduler queueing — refuted as dominant

`cold_queue_depth:0` at end; lifetime enqueued − issued = 427; zero
errors; drain runs once per reactor iteration and iterations are ≤ 279 µs
p999 — worst-case one-iteration deferral ~0.3 ms against a 13 ms p99.
Caveat: no queue-depth time series / high-water in INFO — close via §4.4.

### (e) The 1024 MiB scale deviation — not the cause; direction if-anything flattering

The gate-relevant 10× dataset:budget ratio and zipf θ are preserved
(zipf self-check 71.2% vs analytic 70.8%), so the cold-touch fraction is
~scale-invariant; the smaller footprint gives the nearly-full drive
*more* spare area than the 2048 profile would. Re-running at 2048 will
measure the **same or worse**.

## 4. What this artifact cannot adjudicate — exact experiments

1. **Device vs software attribution (decisive).** fio counter-probe, when
   the box is idle, fio installed, on an NVMe-backed path (NOT the tmpfs
   scratchpad), DEV-TIER label, ≤ 45 s, delete file after:
   - Baseline: `fio --name=coldbase --filename=<nvme>/probe.bin --size=2g
     --rw=randread --bs=16k --iodepth=5 --direct=1 --time_based
     --runtime=15 --lat_percentiles=1`
   - Interference leg: same read job + concurrent `--name=wal --rw=write
     --bs=64k --fdatasync=1 --rate=4m` (the WAL/fsync shape); optionally a
     1 MiB-write/fdatasync-per-MiB job (the ADR-0053 D3 flush-slice shape).
   - Verdict rule: read p99 collapsing to multi-ms under the write leg ⇒
     device-bound, buy hardware; read p99 < ~500 µs ⇒ our write shape is
     implicated, prioritize solutions 3/4.
2. **Sub-leg burst correlation.** Add windowed (delta) cold p99, fsync
   p99, cold queue high-water, issued/enqueued deltas to the 10 s sampler
   before the next tiered soak — 25-min leg cadence cannot resolve the
   8-minute compaction cycle.
3. **fsync vs flush/compaction attribution.** A/B soak windows: (i)
   demote/compaction paused N minutes → cold-p99 delta isolates the
   write-stream term; (ii) durable legs at everysec vs always → isolates
   the fsync term. Campaign-config experiments, no engine change.
4. **Surface `pool_dry`/`queue_full` in INFO** to close the last
   upstream-queueing loophole.

## 5. Solutions, ranked by expected impact ÷ cost

1. **Split the durable log (WAL + checkpoints) onto a second device;
   tier files keep the other.** Removes the fsync FLUSH barrage — the
   largest continuous interference term — from the cold-read device.
   Mount-point configuration, no seam change. Validation: S24 phase-4
   A/B, 3–5 replicates, one-device vs two-device legs.
2. **Gen4 DRAM-full device** (§6). Categorically different flush and
   read-under-write behavior; also retires the standing §19
   device-profile deviation on every ledger row.
3. **Maintain-class *write* pacing under foreground cold pressure — a
   knob that does not exist today.** ADR-0055 D3's 3:1 deficit split
   shapes Maintain **reads** only; flush/compaction writes are budgeted
   in bytes/slice but never keyed to cold-read latency. Shape:
   defer/stretch Maintain slices when foreground cold reads are in
   flight and recent cold-completion latency exceeds a threshold (token
   bucket with a floor rate). Needs an ADR (touches ADR-0053 watermark
   backpressure; over-deferral converts to bounded typed
   `tail_alloc_stalls`) + DST rows. Helps burst-driven p999; marginal
   for fsync-driven p50/p99 — size via §4.3 first.
4. **fsync-side shaping.** Group commit already batches well
   (`fsync_group_p50:447`). Config lever: non-tier durable legs at
   everysec (disclosed, zero code). Engine lever (cross-cell fsync
   coalescing) only if §4.3(ii) shows fsyncs dominate.
5. **QD-cap retune / coalescing / read-ahead: no action.** The cap never
   binds; no adjacency to merge; gap-bridging is forbidden as bandwidth
   theft (ADR-0055 D4). Keep one control row in the S22 matrix. Do fix
   the §2.3 instrument defects before S24.
6. **Honest gate re-expression (only alongside, never instead of, a
   fix).** M2 precedent: the gate **binds on the §19 device profile
   (Gen4, DRAM-full)**; on the current Gen3 DRAM-less box the row
   publishes as a disclosed deviation with the loaded number, the
   idle-drain number (software path 7.5× inside the gate), and the
   device-attribution evidence — status `Evidence-pending
   (Gen4-dispositioned)`. Dishonest forms, named: raising the threshold;
   citing idle-drain for the loaded gate; letting the leg reports'
   `FAIL (DEV-TIER, non-binding)` disappear from the ledger.

## 6. Gen4 revisit — recommendation: buy, before S24 phase 4

The M3 deferral was explicitly dated ("revisit at M4") — this is that
window, and the evidence is now specific: every software hypothesis is
counter-evidenced by the run's own counters (QD p99 5 vs cap 64; queue
depth 0; reactor p999 279 µs), while the device shows a 5.9 ms median
fsync, a documented sustained-write collapse in S11's own artifact, and
a 476 GiB nearly-full DRAM-less reality against a §19 profile that names
Gen4. A Gen4 DRAM-full 1–2 TB device (capacity also un-blocks the
2048/20 GiB runbook scale) plus keeping the current Gen3 as the log
device (solution 1) addresses the interference class structurally.

- Evidence justifying the buy: already in hand. The fio probe (§4.1) is
  the cheap positive control — if it reproduces read collapse under a
  write+fsync stream, the decision is closed.
- Evidence justifying keep-Gen3 + re-expression: the probe showing read
  p99 < ~500 µs under the write stream (⇒ software write shape is the
  problem — solutions 3/4 first), or an owner decision that v0.4.0-alpha
  ships with the §5.6 deviation disclosed and hardware moves to the next
  milestone. Given the in-run fsync latencies, the first outcome is
  unlikely.
