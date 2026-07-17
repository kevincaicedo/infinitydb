# M3-S04 — repeated-key interning A/B (dev-tier, 2026-07-10)

- Box: HomeLab i7-13700KF, kernel 7.0.0-27-generic, governor=performance,
  `no_turbo=1` (binding env; absolutes are turbo-off — LOWER than the
  S02 ledger's dev-tier 218.4 ns row, which ran a different clock state.
  Same-session arms are directly comparable; cross-session absolutes are
  not).
- Pinning: `taskset -c 4` (P-core), criterion, 3 replicates per arm,
  ABAB leg order. Tree state: c50f2b6 + the S03/S04 working tree (the
  change under test); baseline arm from a clean worktree at c50f2b6.
- Commands: `taskset -c 4 cargo bench -p inf-doc [--features
  doc-intern-keys] --bench {traverse,intern}`; raw logs under `raw/`.
- Tier: **dev** — no public claim exists from this artifact (L10).

## Stored bytes per corpus shape (the dev-tier RSS proxy)

| shape | plain | interned | delta |
|---|---|---|---|
| small-200B | 191 | (stays plain) | 0.0% |
| gate-1KiB | 707 | 664 | −6.1% |
| medium-2KiB | 1182 | 1143 | −3.3% |
| large-64KiB | 35401 | 24212 | −31.6% |
| deep-32 | 1396 | 1148 | −17.8% |
| wide-array | 300454 | 270467 | **−9.98%** |

The wide-array shape — the one the ≥ 10% decision-rule bound names —
lands at 9.98%, marginally **below** the bar even on the stored-bytes
proxy. (The large-64KiB shape wins big, but it is not the rule's shape.)

## Depth-4 leaf fetch, 1 KiB gate shape (the read-regression rows)

| arm | ns (median of 3, criterion mid-estimate) | vs default build |
|---|---|---|
| pre-change baseline (c50f2b6 worktree) | 219.0 / 222.1 / 219.3 | — |
| **default build, ZST dict (ships)** | 217.6 / 218.1 / 218.4 | **0% (at baseline)** |
| feature build, plain document | 238.3 (236.4–245.3 across runs) | **+9.4%** |
| feature build, interned document | 246.4 / 246.2 / 246.4 | **+13.0%** |

Intermediate finding that changed the design: the first implementation
threaded the dict as a fat pointer through every cursor unconditionally —
that alone cost the **default** build ~3.6% on this row (227.1 ns vs
219.3 baseline). Rejected per L4/M0-S14 and rebuilt as a zero-sized type
without the feature; the default build returned to baseline (raw logs
keep both generations: `traverse-{off,on}-{1..3}.txt` = fat-pointer
arms, `traverse-{off,on}-zst.txt` = shipped layout).

## Wide-array element probe (arr.index(5000) → obj.get("qty"))

| arm | µs | delta |
|---|---|---|
| feature build, plain | 18.09 | — |
| feature build, interned | 17.12 | **−5.4%** (id compare beats memcmp) |

## Ingest transform cost

`intern()` on the 1 KiB gate shape: **3.60–3.67 µs** — noted against the
S05 parse budget arithmetic (§4.1: ~1 µs/KiB total ingest at 1 GB/s);
the transform costs ~3.6× the entire parse budget per KiB where it
applies. An ingest-path cost of this size is itself disqualifying for
default-on at the `JSON.SET ≥ 70% SET` gate.

## Decision-rule application (ADR-0038 D6)

Default-on requires ≥ 10% RSS win on wide-array **and** ≤ 2% read
regression on all shapes. Measured: the read bound fails decisively
(+9.4% plain / +13% interned on the gate shape, feature build vs default
build), the wide-array bytes win is marginally under the bar (9.98%),
and the ingest transform adds ~3.6 µs/KiB. **Disposition: Rejected as a
default.** The mechanism ships behind the off-by-default
`doc-intern-keys` feature (absent from default builds — which measure at
baseline by construction); no S25 RSS re-run is required because the
read half of the rule fails independently of RSS tier. Revisit only with
the M4 corpus data (the plan's debt-forward entry), and only with a
design that moves needle resolution off the per-get path (e.g. resolving
path-program keys to intern ids at S10 compile time).
