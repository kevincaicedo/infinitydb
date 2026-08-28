# M4.5-S37 — review follow-up 2 (2026-08-28): the amended proof set on the keyed-hash tree

Engine: the commit this directory lands with (ADR-0093 second amendment
A3′/A4′/A7′/A8/A9, ADR-0094 keyed key hashing, ADR-0088 third amendment).
Dev tier (`linux-devbox-profile`); determinism runs, not timing rows —
no claim-ledger number moves. Every sweep's shard logs, manifests and
per-seed results are here verbatim (`chain.log` shape: `just check` →
`cargo deny` → `sim-smoke` → the sweeps, sequential).

| row | seeds | violations | coverage (Σ shards) |
|---|---|---|---|
| `m4-tiered` (`0x5EED0000`, 8 shards) | 1 000 (750 arm) | **0** | 43 604 tickets / 43 588 same-key, 0 stale, 0 fallbacks; phase 6c 6 000 tickets / 3 000 collision verdicts; **phase 6d (open-ticket rows, ADR-0093 A8) on 750 seeds: 3 000 tickets held open, 2 243 DBSIZE drains reading 4 177 twins, 3 000 SCAN twins, 3 000 retargets, 1 500 read-free forced deletes, 3 000 Ticketed refusals, 3 000 collision verdicts, 750 injected twin-read errors relayed by DBSIZE, 2 319 settled without a read after resume** |
| `m4-recovery` (`0x5EED0000`, 8 shards) | 1 000 | **0** | 79 294 tickets opened, 17 551 open at a cut, 21 380 re-formed, 61 709 same-key / 176 collision verdicts (each checked against the decoded keys), 154 212 crafted colliding-pair ops, **92 slots settled at boot through the rebuild cursor**, 4 000 DBSIZE-drain exactness checks |
| `m2-durable` (`0xD5EE0000`, 8 shards) | 10 000 | **0** | the M2 contract untouched by the arm and the keyed hash |

Phase 6c's node-level drain/Ticketed/SCAN-twin counters still read 0 by
construction (the reconciler resolves between commands); phase 6d is
the row that holds tickets open — its counters are the coverage.

Not here: the reference-box campaign (`gate-run m4.5 --only-s37
--s37-shadow` + the S35 read leg, ADR-0093 D9) that decides the
default — still owed, now on the keyed-hash binary.
