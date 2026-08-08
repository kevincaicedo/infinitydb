# M0–M2.5 regression re-pass — dev tier, 2026-07-16

Method: fresh `gate-run m0|m1|m2 --unsafe-env` on the M3 working tree,
plus the decisive control: the **M2.5-final tree (`a5a15ed`) rebuilt in a
worktree and gate-run on the same box the same day** (its report is
`m0-m25baseline/`). Same-day/same-box tree-vs-tree is the honest
regression comparison; the 2026-07-10 archived numbers came from a
sudo-prepped box state (`--reference` env: governor/isolation) that
`--unsafe-env` desktop runs do not reproduce.

## m0 — tree vs tree (same box, same day)

| row | M2.5 tree | M3 tree | delta |
|---|---|---|---|
| pipelined GET/SET ops/s | 2,898,553 | 2,840,228 | **−2.0% ✓** |
| unpipelined vs Redis (in-run) | 2.91× | 2.76× | −5.2% (marginal; single runs — replicate at reference) |
| sqes/submit | 17.49 | 17.11 | −2.2% ✓ (tripwire ≥ 16 green both) |
| fabric hop RTT p50 | 172.03 µs | 172.03 µs | 0% |
| vs Dragonfly (in-run) | 1.68× | 1.68× | 0% |
| cross-cell penalty | 64.75% | 63.26% | improved |

The apparent −28% vs the archived 2026-07-10 report is **environmental**
(box state), proven by the baseline tree measuring the same 2.9M on
today's box. Verdict: **regression sub-gate PASS at dev tier** on the
pipelined row; the unpipelined-vs-Redis ratio sits at the −5% line from
single runs and is flagged for 3–5 replicates in the reference campaign.

## m1 — M3 tree vs archived (rows are box-insensitive latencies/ratios)

RSS vs Redis 0.61× (identical) · expiry storm 863 µs vs 831 (+3.9%) ·
debt drain 0.48 s (identical) · TTL-heavy 1,247 µs vs 1,343 (improved;
an earlier 2,303 µs reading came from a memory-pressured box — tmpfs
incident — and is disregarded with the cause named) · pub/sub fan-out
0.56 ms vs 0.67 (improved) · KV p99.9 under pub/sub 2,815 µs vs 2,687
(+4.8%, informational). Verdict: **within ≤5% on every row.**

## m2 — M3 tree vs archived

everysec penalty **10.00%** vs 10.07 · always grouped writes 155,284 w/s
vs 153,911 (+0.9%; the ≥300k row stays Gen4-hardware-dispositioned per
M2.5-S18) · memory-only zero log records PASS. Verdict: **within noise.**

## Findings (tooling/ops, recorded in the release-readiness report)

1. `gate-run m2`'s everysec row writes ~13 GB to its data root
   (default `$TMPDIR`): on a 16 GB tmpfs it exhausts space mid-row, the
   server fail-stops per §8.4 (`errno 122 on LogWrite`), and the harness
   reports "server closed connection under load" — this is the standing
   M2.5-S22 watch item, now root-caused with the stderr hook. Run with
   `--pressure-data-root` on a real filesystem (as this run did).
2. An interrupted `gate-run m2` **leaks** `inf-m2-esec-<pid>` (~10 GB);
   three leaks took this session's tmpfs down twice. The harness should
   sweep stale dirs and clean on error paths.
3. The zero-cost A/B rows remain PENDING (tooling): they need
   `--baseline-bin`; the worktree pattern used here (build `a5a15ed`,
   compare same-day) is the working substitute and could be automated.
