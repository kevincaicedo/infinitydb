# M3-S23/S24 development evidence — 2026-07-16

This directory closes the CI/fleet evidence owned by M3-S23 (delta-replay
equivalence oracle) and M3-S24 (document power-cut matrix + fuzz-derived
corpus in DST), per ADR-0045. It is development-tier evidence from a
disclosed dirty tree, not a reference-box campaign and not a public
performance claim.

## S23 verdict

The equivalence oracle is a passive, read-only observer inside the
`m3-document` scenario (ADR-0045 D1): per cell it reconstructs a shadow
keyspace from the shared SimDisk (manifest → named `.ick` → segment tail
walk with recovery's prefix semantics) through the production replay
applier (`Keyspace::apply_record`), then compares live vs replayed state
with §3.4 R3's currency — the layout-independent `StateDigest` (canonical
idoc + lineage + version) plus a per-document walk over lineage, version,
frozen idoc bytes, and `serialize_canonical_into` output (the E8
comparator; interned/arena forms unreachable by construction, ADR-0038
D3). Checks run at two mid-run quiesce instants and post-recovery; the
power cut itself is never quiesced, preserving S18's unacked-tail cases.

Canary: `--replay-canary` makes the shadow skip the newest tail
`DocDelta`. The fleet test caught it on the **first seed**; the captured
run (`canary-run.txt`, seed 0xD0C023CA) shows all three comparator layers
firing with named diagnostics — digest divergence, version divergence
(live 10 vs replayed 9), and the canonical-text diff exposing the skipped
merge. An earlier canary variant (skip the *first* tail delta) survived
11 seeds because the first delta is usually checkpoint-stale — recorded
here as the reason the canary targets the newest delta.

The merge-heavy workload and witness-verified program re-execution
discharge the two named hand-offs parked at S23: S14's merge-heavy DST
scenario and S09's `Matches`-equality/apply-order DST generalization.

## S24 verdict

The document workload generalized to the ADR-0045 D3 model: root
`JSON.SET` (every other one embedding a fuzz-corpus document as the
`blob` subtree), multi-match `NUMINCRBY`, three RFC 7386 `MERGE` classes
(nested set; null delete + root add; member re-add), path
`JSON.SET $.blob` subtree replace, root-member null delete, and
`JSON.GET` audits — all with byte-exact harness-modeled expectations, so
the §8.2 admissible-state oracle keeps binding. The corpus pool is the
minimized `json_parse` corpus (sorted, parse-valid, canonical ≤ 6 KiB,
256 docs — crossing the 512 B inline and 4 KiB morph thresholds).

Cut coverage is disclosed, never assumed (ADR-0045 D4): each run
classifies the surviving log's boundary record class and sweeps aggregate
the distribution.

## The 10,000-seed gate sweep (`doc-sweep-10k.txt`)

`just doc-sweep` — 10,000 seeds, base 0xD0C24000, 8 shards, release:

- **0 durability-oracle violations, 0 equivalence-oracle violations, 0
  refusals** across all shards;
- **30,000 equivalence checks** — every seed reached both mid-run
  instants and the post-recovery check;
- **150,000 documents byte-compared**; **106,369 fuzz-corpus documents**
  entered the workload;
- cut-class distribution: doc-delta 6,340 · doc-full 8,436 · ckpt-begin
  143 (per-shard rows in the manifest lines).

A 256-seed slice at the S18 base (0xD0C01800) is in `validation.txt` for
continuity with the S18 evidence: 0 violations, 768 checks, classes
ckpt-begin:4 / doc-delta:175 / doc-full:215.

## Scope boundary

No reference-box campaign, RedisJSON RSS comparison, or public claim.
S25 owns reference-tier gates. The nightly fleet gains a 4,000-seed
m3-document sweep beside the M2 sweeps (`infinity-dst-nightly.yml`).
