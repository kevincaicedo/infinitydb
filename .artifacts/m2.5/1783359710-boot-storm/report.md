# boot-storm (M2.5-S01)

- date: 1783359710 (unix)
- kernel: 7.0.0-27-generic
- infinityd: target/release/infinityd
- cycles: 500 · cells: 4 · pressure: 2048 MiB/cycle · ready bound: 10s · pin-start: 4
- data-root: /home/kcaicedo/.cache/inf-bootstorm (must not be tmpfs)

| metric | value |
|---|---|
| wedges (gate: 0) | 15 |
| named fail-stop exits (ADR-0026 D3 Phase-H item; informational under pressure) | 0 |
| retries consumed (by design) | 0 |
| time-to-all-ready p50 | 20 ms |
| time-to-all-ready p99 | 37 ms |
| time-to-all-ready max | 115 ms |

## wedges

cycle 38: node stayed -LOADING past 10s (stderr: infinityd-37769.stderr)
cycle 95: node stayed -LOADING past 10s (stderr: infinityd-38893.stderr)
cycle 155: node stayed -LOADING past 10s (stderr: infinityd-36947.stderr)
cycle 168: node stayed -LOADING past 10s (stderr: infinityd-46407.stderr)
cycle 180: node stayed -LOADING past 10s (stderr: infinityd-45843.stderr)
cycle 213: node stayed -LOADING past 10s (stderr: infinityd-36443.stderr)
cycle 241: node stayed -LOADING past 10s (stderr: infinityd-33955.stderr)
cycle 252: node stayed -LOADING past 10s (stderr: infinityd-36619.stderr)
cycle 292: node stayed -LOADING past 10s (stderr: infinityd-35099.stderr)
cycle 302: node stayed -LOADING past 10s (stderr: infinityd-44977.stderr)
cycle 348: node stayed -LOADING past 10s (stderr: infinityd-32879.stderr)
cycle 350: node stayed -LOADING past 10s (stderr: infinityd-36283.stderr)
cycle 355: node stayed -LOADING past 10s (stderr: infinityd-38063.stderr)
cycle 451: node stayed -LOADING past 10s (stderr: infinityd-34155.stderr)
cycle 466: node stayed -LOADING past 10s (stderr: infinityd-40657.stderr)

## post-run classification note (2026-07-06)

All 15 rows above were misclassified by the harness build that ran them:
every flagged cycle's stderr carries a loud `cell N failed: driver setup
(ring create): Cannot allocate memory (os error 12)` line (5× cell 0,
2× cell 1, 5× cell 2, 3× cell 3 — no fixed-cell structure), i.e. the
**named ADR-0026 D3 Phase-H fail-stop**, not a silent wedge: the process
listener binds at setup step 10, before ring create at step 12, so a
fail-stopped node can accept the readiness probe and then exit; the
harness burned its -LOADING timeout on the corpse. Harness fixed in the
follow-up commit (`ServerGuard::try_exited` + fail-stop classification in
the ready-wait loop); the rerun report supersedes this one for the S01
AC. **Silent wedges in this run: 0/500.** Evidence stderrs:
`boot-storm-postfix-stderr/`.
