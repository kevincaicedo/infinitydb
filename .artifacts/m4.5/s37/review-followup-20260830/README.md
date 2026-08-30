# M4.5-S37 — review follow-up 3 (2026-08-30): the proof set on the epoch-3 keyed-hash tree

Engine `b30966a` (ADR-0094 first amendment D6–D9: MANIFEST epoch 3 names
the placing secret, the data-directory `LOCK`, `link(2)` no-replace
`0600` publication; the `DBSIZE` counted fold fail-closed; the lifecycle
test on acknowledged readiness and `--port 0`). Dev tier
(`linux-devbox-profile`); determinism runs, not timing rows — no
claim-ledger number moves. `chain.log` is the sequence verbatim
(`sim-smoke` → the three sweeps, each sweep's 8 shards under
`ulimit -v 3 GB`); `dirty_paths=1` at chain start is this directory
being written — the engine tree was clean at `b30966a`.

| row | result |
|---|---|
| `sim-smoke` (12 scenarios, `--verify-determinism`) | exit 0, every scenario byte-identical (`sim-smoke.log`) |
| `m4-tiered` (`0x5EED0000`, 8 shards) | 1 000 seeds (750 arm) · violations 0 · refused 0 · 43,604 tickets / 43,588 same-key / 0 collision, 0 stale, 0 fallbacks · forced collisions 6,000 tickets / 3,000 verdicts · phase 6d: 3,000 tickets held open, 2,243 DBSIZE drains reading 4,177 twins, 3,000 SCAN twins, 1,500 read-free DELs, 3,000 Ticketed, 3,000 collision verdicts, 750 injected read errors relayed, 2,319 settled without a read after resume |
| `m4-recovery` (`0x5EED0000`, 8 shards) | 1 000 seeds · violations 0 · 79,294 tickets opened, 17,551 open at a cut, 21,380 re-formed, 61,709 same-key / 176 collision verdicts, 92 settled at boot through the cursor, 4,000 drain checks |
| `m2-durable` (`0xD5EE0000`, 8 shards) | 10 000 seeds · violations 0 · refused 0 |

Every shard manifest is **byte-identical** to its 2026-08-28 counterpart
(`review-followup-20260828/`): the simulators inject a seed-derived
hasher and never read `key-hash.toml`, so the amendment changes the
persisted MANIFEST bytes (epoch 3) and nothing the scenarios observe —
which is the expected shape of a lifecycle fix.

Run outside this directory, same engine: `just check` (210 test
binaries / 1 942 tests, clippy `-D warnings`, the doc-intern-keys and
slim lanes), `cargo deny check`, the `manifest_decode` fuzz target
(120 s, 29 966 265 runs, cov 413 / ft 648, no crash), and
`infinityd/tests/key_hash.rs` (6 rows; 3 consecutive runs; 3 concurrent
copies of the binary — 18 servers — green).

Not here: the reference-box campaign (`gate-run m4.5 --only-s37
--s37-shadow` + the S35 read leg, ADR-0093 D9) that decides the
default, and `gate-run m0/m1/m2` for the hash cost at the wire — both
owed, now on the epoch-3 binary.
