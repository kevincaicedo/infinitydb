# M2.5-S21 de-async dispatch — A/B results (2026-07-10)

Verdict: **Rejected by A/B** (the pre-registered dev-tier stop rule in
`hypothesis.md` triggered: arm overlap; the ≥ +4% acceptance floor is
unreachable). The lever ships implemented, **default off** (the
remote-first-execute precedent): the off arm is byte-identical to the
shipped path and `deasync_dispatch_matches_pump_semantics` pins the
equivalence, so S19 may re-read the on-arm at the 8-cell spec shape
(where per-iteration machinery amortizes ~8× worse) without new code.

## Environment

Box: designated reference box (i7-13700KF), governor performance, turbo
off (3.4 GHz P-core base, verified per-leg by cycle arithmetic),
`perf_event_paranoid=-1`, env-check PASS pre-campaign. Tree: inner
`38c278d` (lever commit), clean at leg time; artifacts untracked during
legs (dev tier — disclosed). Server `--cells 4 --pin-start 4` (cpus
4/6/8/10); loadgen `inf-bench load` on cpus 12,14,16–23, conns 64 ×
P=16, 1M keys filled, uniform natural mix — the s09-s21 campaign shape.

## Throughput (natural row, ops/s)

| pair | off (`--no-deasync-dispatch`) | on (`--deasync-dispatch`) | delta |
|---|---|---|---|
| 1 (20 s) | 3,205,149 | 3,203,463 | −0.05% |
| 2 (20 s) | 3,173,127 | 3,256,131 | +2.6% |
| 3 (30 s, perf windows in-run) | 3,021,587 | 3,090,018 | +2.3% |

Off arms span 3,173–3,205k (clean legs); on arms 3,203–3,256k —
**overlap at pair 1 (dead tie)**. Mean effect ≈ +1.3–1.6%, far below
the ≥ +4% floor and the hypothesized +5–9%.

## Perf re-attribution (pair 3, 8 s stat + 8 s dwarf windows, 4 cell cpus)

Gross: both arms ~108.85 G cycles / 8 s (always-busy cells) →
**off 4,501 cyc/op-mix, on 4,403 — Δ ≈ 98 cyc/op-mix (~2.2%)**.
IPC ~2.09 both arms.

Machinery bucket (sum over 4 cells, % of leg cycles → cyc/op-mix):

| symbol group | off | on |
|---|---|---|
| `dispatch_one::{{closure}}` | 4.92% | — (gone, < 0.10%/cell) |
| `send_apply::{{closure}}` | 2.39% | — (gone) |
| `pump::{{closure}}` (on-arm: fast path + mirror inlined) | 2.38% | 6.18% |
| `try_send_apply` | — | 2.04% |
| **bucket total** | **9.69% ≈ 436 cyc/op-mix** | **8.22% ≈ 362 cyc/op-mix** |

The fast path is mechanically complete — the async closures vanish from
the on-arm profile (fallback is rare; `dispatch_one` below the 0.10%
report floor) — and it recovers only ~74 cyc/op-mix of bucket +
~98 cyc/op-mix end-to-end.

## Finding (the number the ADR carries)

**The async machinery the lever targeted was already near-zero-cost.**
The executor's L6 design (Ready-on-first-poll never allocates; scratch
reuse; inlined nested state machines) held: constructing + polling the
`dispatch_one` → `send_apply` futures costs ~2% of the natural mix, not
the ~10–14% the ~580 cyc/op-mix bucket suggested. The bucket's mass is
the *dispatch work itself*, shared by both arms: guard chain + routing,
argv/`ApplyArgs`/`Op` staging, window/emit bookkeeping — plus the
adjacent deferral-copy class the profile names
(`__memmove_avx_unaligned_erms` 5.2% + `OwnedCmd::from_argv_into` 1.4%
≈ ≤ 290 cyc/op-mix, partly recv/reply traffic).

Residual per-op levers after this slice, with measured ceilings:
- deferral-copy class (≤ ~290 cyc/op-mix, bound not attribution);
- pump-bypass restructure (parse-loop direct send + parked reply slots —
  would attack the remaining ~362 cyc bucket + part of the copies;
  a structural change, not a slice);
- codec+mesh (~244), kernel (fixed-rate — amortizes with throughput).

None projects the +65% natural the ≤ 40% staged gate needs at today's
all-local. The gate's honest path is the S19 8-cell spec-shape re-read
(blocked on the RLIMIT_MEMLOCK user task) and, failing that, a
comparator-anchored disposition (the ADR-0027 RC-4 method) — never a
silent close.

## Files

- `dev-ab.sh`, `A1-off/B1-on/A2-off/B2-on-{load,fill}.stdout`, `*-info.txt`
- `dev-perf.sh`, `P-{off,on}-perf-{agg,stat}.txt`, `P-*-perf-load.stdout`
  (perf.data deleted after extraction — the RAM lesson)
