#!/usr/bin/env bash
# The DST smoke lane (F-L19-03, review 2026-08-30): every scenario the
# `inf-sim` binary accepts, once, at a fixed seed, determinism-verified.
# One registry for `just sim-smoke`, the PR CI `sim-smoke` job and
# `bins/inf-sim/tests/lanes.rs` (which fails when a scenario in
# `inf_sim::SCENARIOS` has no row here, or a row names no scenario).
# Rows are `<scenario> [flags…]`; a row's flags replace the default
# `--verify-determinism`. Usage: scripts/sim-smoke.sh [seed]
set -euo pipefail
cd "$(dirname "$0")/.."
seed=${1:-0xC0FFEE}

# The simulator this run builds is the one it runs: both in this checkout's
# target root, whatever CARGO_TARGET_DIR holds (ADR-0198 A2).
# shellcheck source=target-root.sh
. scripts/target-root.sh
target_root_setup "$(pwd -P)"
cargo build --release -p inf-sim --features dst --bin inf-sim --target-dir "$TARGET_ROOT"
bin="$TARGET_ROOT/release/inf-sim"

# F-L11-05: an accept-path error is a counter, never connection teardown.
# The accept-broken row parks each listener as broken, which no close
# resumes: the plane's accept retry is the queued clients' one way in, and
# its oracle times each re-arm against the retry's deadline. Its twin runs,
# as its trace is its own.
# Adversarial key/value lengths at 4 cells.
# F-L19-05/06: namespace-bound + SELECTed clients, the served surface and
# the stored content under audit at 4 cells.
# F-L12-02: one hot owner, the binary's mesh sizing, deep pipelines.
# F-L15-05 (ADR-0123): maxclients refusal at accept + the idle reaper.
# ADR-0124 (F-L15-08): a graceful stop before the cut keeps every ack.
# F-L17-14: the M4.5 index crash scenarios.
# F-L04-02 (ADR-0119): one EIO under a cold read is one typed reply.
# F-L19-03: the m2 device/mode/window/recycle oracles and the three
# m2 fill/hold/pending scenarios ran in no automated lane.
# m2-fill-dense is the fill scenario's dense leg: hundreds of hold
# episodes inside one window, and a cell's timers stay its roles' owners.
# The two --park-at-once rows run the wake oracle on the hold scenarios at
# spin 0: a held frame's park asks no longer than its window. The fill leg
# drops the twin (its trace is its base row's, which runs the twin); the
# group leg keeps it, and its twin compares the wake readings too.
# ADR-0174 D1: the replay-above-window seed class forced on the smoke
# seed — a boot whose tiered replay exceeds the window demotes. D2 rule
# 6: m4-recovery's spec-variant class, each case forced — a boot at a
# window below its ring pads. ADR-0202 D5: m4-tiered's listing-repeat class
# forced on the smoke seed — the post-drop reboot's sweep reads its dropped
# namespace past one batch and takes each repeated name as its earlier
# removal.
# m4-tier-budget runs each of its legs (two namespaces at 8mb, eight at
# 1mb, the one-namespace control, two at 1mb at the hostile share, one at
# 64mb with no device model, and two at the hostile share whose first is a
# thief of the tier class's credit) on its own row, and the retry arm (one tier
# write fails and is retried) on the 8mb leg and on the no-model leg,
# where the 64 MiB round is more ops than one turn's visits and the
# retry group is built behind a run of done writes: a leg is named,
# never drawn from the seed.
# m2-ledger-wedge runs its main leg (a seal held while 128 writers keep the
# log rotating: at most one segment id appears inside the hold) and its
# control (one writer: no rotation and no new segment inside the hold) on
# their own rows.
# m4-epoch-marks runs its two legs on their own rows: the everysec leg (no
# wedge, every merge of the reclaim marks exact, no crossing) and the wedge
# leg (a seal held at the ledger's front with write-through frames behind
# it: the marks' row must reach its cap and cross it).
rows=(
  "m0-smoke"
  "m0-smoke --plant accept-error"
  "m0-smoke --plant accept-broken --verify-determinism"
  "m0-adversarial --cells 4 --verify-determinism"
  "m0-surface --cells 4 --verify-determinism"
  "m0-fabric-fairness"
  "m0-admission"
  "m1-cache"
  "m2-durable"
  "m2-clean-stop"
  "m2-device-budget"
  "m2-mode-transition"
  "m2-reorder-window"
  "m2-fill-tick"
  "m2-fill-tick --park-at-once"
  "m2-fill-dense"
  "m2-group-hold"
  "m2-group-hold --park-at-once --verify-determinism"
  "m2-fua-pending"
  "m2-ledger-wedge --wedge-leg main --verify-determinism"
  "m2-ledger-wedge --wedge-leg control --verify-determinism"
  "m2-stop-tick --verify-determinism"
  "m2-ckpt-refused"
  "m2-recycle"
  "m2-combined"
  "m2-ns-create-window"
  "m2-ns-drop-window"
  "m2-ns-ddl-race"
  "m3-document"
  "boot-storm"
  "m4-steel"
  "m4-pressure"
  "m4-cold"
  "m4-recovery"
  "m4-recovery --replay-above-window --verify-determinism"
  "m4-recovery --spec-variant ring-top --verify-determinism"
  "m4-recovery --spec-variant page --verify-determinism"
  "m4-diskfull"
  "m4-tiered"
  "m4-ns-lifetimes --verify-determinism"
  "m4-ns-lifetimes --lifetimes-leg control"
  # A tiered namespace dropped mid-checkpoint-walk, past its first pass.
  "m4-ns-lifetimes --seed 0x2"
  "m4-tiered --plant tier-read-eio"
  "m4-tiered --replay-above-window --verify-determinism"
  "m4-tiered --listing-repeats --verify-determinism"
  "m4-tier-budget --tier-leg two-8mb --verify-determinism"
  "m4-tier-budget --tier-leg eight-1mb --verify-determinism"
  "m4-tier-budget --tier-leg one-1mb --verify-determinism"
  "m4-tier-budget --tier-leg two-1mb-hostile --verify-determinism"
  "m4-tier-budget --tier-leg two-8mb --plant tier-write-eio --verify-determinism"
  "m4-tier-budget --tier-leg one-64mb-no-model --plant tier-write-eio --verify-determinism"
  "m4-epoch-marks --marks-leg everysec --verify-determinism"
  "m4-epoch-marks --marks-leg wedge --verify-determinism"
  "m4-extent-window --window-leg burst --verify-determinism"
  "m4-extent-window --window-leg cap --verify-determinism"
  "m4-extent-window --window-leg cold --verify-determinism"
  "m45-backfill"
  "m45-sidecar"
  # Publishes that cross between cells faster than their owners answer, on
  # eight credits: two cells publishing to each other's channels, a ring of
  # three, eight cells each publishing to any other, the two-credit floor;
  # and the controls, no publish crossing and a window that never fills.
  "m1-pub-cross"
  "m1-pub-cross --cells 3 --verify-determinism"
  "m1-pub-cross --cells 8 --pub-target others --verify-determinism"
  "m1-pub-cross --fabric-credits 2 --verify-determinism"
  "m1-pub-cross --pub-target own --verify-determinism"
  "m1-pub-cross --fabric-credits 256 --verify-determinism"
  "m4-tier-budget --tier-leg two-1mb-thief --verify-determinism"
  # m4-cold's identity leg: the cold-read host's issuer placed 32 pairs before
  # the end of its 2^55, so the space runs out inside the run (ADR-0181 R4).
  "m4-cold --cold-leg identity --cold-identity-start 0x7fffffffffffe0 --verify-determinism"
  # m4-cold's delivery legs: 31-member cohorts past a turn's 64 visits, 65
  # orphaned members, and errored reads, each round's visits counted.
  "m4-cold --cold-leg dense --verify-determinism"
  "m4-cold --cold-leg cancel-all --verify-determinism"
  "m4-cold --cold-leg errors --verify-determinism"
)

ran=0
start=$SECONDS
for row in "${rows[@]}"; do
  # shellcheck disable=SC2206
  parts=($row)
  name=${parts[0]}
  flags=("${parts[@]:1}")
  if [ ${#flags[@]} -eq 0 ]; then flags=(--verify-determinism); fi
  echo "== sim-smoke: $name ${flags[*]} (seed $seed)"
  "$bin" --scenario "$name" --seed "$seed" "${flags[@]}"
  ran=$((ran + 1))
done
echo "sim-smoke: $ran rows green in $((SECONDS - start)) s (seed $seed)"
