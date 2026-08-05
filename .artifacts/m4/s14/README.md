# M4-S14 — Live-set tracking: evidence bundle (2026-07-25)

Dev-tier per the box profile (Linux dev box; MemFs/SimDisk harnesses —
no device numbers claimed here). Story: per-tier-file {live, dead}
counters (ADR-0058).

## AC 1 — exactness storm (proptest + named 10⁶-op run)

`crates/inf-store/tests/tiered_live_set.rs`:

- `live_set_storm_million_ops` — 10⁶ random write/update/delete ops
  (seed 0x14E_5EED) against a shadow model, real `TierFlush` (MemFs)
  filing, interleaved seal/flush/release slices, ring wraps and
  capacity rotations. After **every op**: the space's dead bytes
  decompose exactly into filed dead + pending dead + ring-top holes,
  and `live_bytes + dead = allocated`. Every 4096 ops and at the end:
  per-file `live + dead = file bytes` with live computed from the
  model — exact. Coverage asserted: > 3 tier files, ≥ 1 ring-top hole.
- `live_set_matches_model` — proptest seed sweep at CI scale (2k ops).

Run: `cargo test -p inf-store --release --test tiered_live_set` — PASS
(also green in the debug workspace suite, ~19 s).

## AC 2 — counters survive crash/recovery (DST + store-level)

- `bins/inf-sim` `m4-recovery` grew the live-set leg: every publish
  emits `.ick` 0x04 sections; every recovery restores them and runs the
  per-life oracle (per recovered file: slot count == pinned-walk ground
  truth; `dead ≤ len`; byte-exact ⇒ fully dead), folded into the
  determinism trace.
- Sweep: `recovery-live-set-sweep-20260725/` — **10,000 seeds, 0
  violations**, 3,655,623 refs, 13,507,503 images, 7,523
  cut-before-publish lives, 9,979 flush-lag lives, 40,981 live-set
  entries. Single-seed determinism verified (`--verify-determinism`).
- Store-level: `tiered_recovery.rs::unified_recovery_round_trips_all_classes`
  extended — 0x04 round-trip, replay-time count reconciliation oracle,
  post-recovery cold overwrite/delete routing (pre-life deaths charge
  the recovered file only), this-life files byte-exact.

## Finding (fixed in this story): cross-life displacement collision

The first sweep run read **2/10k violations** ("never-none violated —
index miss"), reproduced byte-identically on the pre-S14 tree (seeds
0x514d731, 0x514d19e) — a latent S12-era bug, not an S14 regression:
`Index::position_of` matched `(ctrl tag, addr)` without the full
sidecar hash, so a `ColdDisplace` marker's crashed-life address could
remove a different key's numerically-colliding new-life slot (~2⁻⁷ per
address collision). Fixed by requiring the mode's exact-pair check
(sidecar hash for tiered; memory mode compiles to constant true) —
ADR-0058 D6. Pinned by
`displacement_never_removes_a_foreign_key_at_a_colliding_address`
(verified to fail against the neutered fix). Post-fix sweep: 0/10k.
