# M4.5-S29 A/B verdict — tiered fabric apply gated-reply detachment (2026-08-19)

**Change:** `crates/inf-server/src/plane.rs` — `tier_apply_pump` no longer
awaits `ack_gate.waiter(seq)` inline; gated verdicts queue on `tier_gated`
and FABRIC-IN spawns the ADR-0015 D6 deferred-reply future (see
`fix.patch` in this directory; ADR-0082).

**Box:** i7-13700KF (8P+8E), 30 GB, ADATA LEGEND 700 Gen3 NVMe,
governor `performance`, kernel 7.0.0-30. Server: 4 cells, taskset
0,2,4,6 (`--pin-start 0`), native glibc release build, data on the real
filesystem (not tmpfs). Generator: `tri-bench` (exploratory harness —
NOT inf-bench/inf-compare) on cores 8,10,12,14, closed-loop, 1 KiB
values, zipfian, 100% write; namespaces `tierbig` (tiered `always`,
MEM-BUDGET 4gb/cell — demoter idle, isolates the fabric path) and
`syncflat` (non-tiered `always`), both loaded 50k keys.

**Tier: dev.** Not §19-valid; no row backs a public claim. Fresh server
+ fresh data-dir + fresh port per replicate, 3 replicates per binary,
arms interleaved per replicate (tiered/flat × c64/c256 within one boot).

## Medians of 3 replicates (ops/s · p99 ms)

| leg | pre-fix (e5b2c48+docs) | post-fix | delta |
|---|---|---|---|
| load 50k, 64 conns, tiered | 4,457 | 11,674 | **2.6×** |
| tiered `always` c64 | 4,448 · 53.0 | 10,918 · 8.5 | **2.5× · p99 ÷6** |
| tiered `always` c256 | 3,978 · 364.0 | 43,085 · 8.4 | **10.8× · p99 ÷43** |
| slope c256/c64 (tiered) | 0.89× | 3.9× | restored |
| flat `always` c64 | 12,202 · 19.5 | 11,038 · 8.5 | unchanged (noise) |
| flat `always` c256 | 42,989 · 8.9 | 43,210 · 8.7 | unchanged |

Raw rows: `ab-results.txt`. Outliers disclosed: prefix rep3 ran ~20% low
on both arms (drive state); s29fix rep2 had one low tiered-c256 leg
(25,249) and one low flat-c64 leg (8,790) in the same replicate —
box/device noise, not arm-specific.

**Contamination disclosure:** an earlier aborted A/B script left idle
`infinityd` processes alive during the recorded run (the kill matched
the wrong process name). Each recorded replicate owned its port
exclusively (one server per port); the stale servers were idle (no
clients, no data traffic) and shared only the CPU set. Medians are
consistent with the clean single-run sweeps taken before and after; the
binding numbers remain the reference-box gate run's.

## Follow-on shape checks (post-fix binary, single runs)

- Beyond-RAM identity config (2M × 1 KiB into 1 GB node budget,
  `always`): load 12,313 ops/s; write c64→c256 = 5,490 → 30,263 (scales;
  pre-fix finding was flat 3,267 → 3,196 at 5× ratio); 50/50 c64 =
  18,190 (the finding's 5,037 was *below its own* 100%-write row —
  the mixed-workload collapse is gone). Dataset ratios differ (2× vs
  5×) — shape evidence, not a like-for-like delta.
- `everysec` tiered: drive-state dominated on this box (4× run-to-run
  spread on BOTH binaries at ~300 MB/s sustained; hermetic A/B/A/B
  inconclusive both directions). Evidence-pending; the fix does not
  touch everysec control flow.

## Gate

`inf-bench gate-run m4.5` (new; `docs/milestones/m4.5-gates.toml`),
dev-tier run `.artifacts/m4.5/s29/1787159548-gate-run/report.md`:
slope 2.95 PASS · parity@64 0.78 PASS · p99-ratio@256 2.03 PASS ·
parity@256 0.57 **FAIL (dev-tier)** — residual named in ADR-0082 D4
(cold resolve on write path, blocking tier-flush fdatasync on the
reactor, in-place displacement markers), owned by S30/S31 follow-ups.

## Validation

`just check` green · `cargo deny check` green · `just sim-smoke`
(m4-tiered scenario) determinism verified byte-identical. NOT run:
loom (no ring changes), miri (no unsafe touched), fuzz (no decoder
touched), compat oracle (no wire-visible change), reference-box gate.
