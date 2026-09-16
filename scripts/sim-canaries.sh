#!/usr/bin/env bash
# Planted-bug canaries (ADR-0090 D5; F-L19-03 addendum, F-L19-04): each
# `--cfg inf_canary_*` disables one load-bearing rule in the product, and
# the scenario whose oracle owns that rule must go red on the planted
# build — and green on the plain build. A canary the fleet never plants
# proves nothing; `bins/inf-sim/tests/lanes.rs` fails when a cfg in the
# workspace's `check-cfg` list has no row here.
# Rows: `<cfg> <scenario> <expected violation substring> [flags…]` (a
# sweep where one seed may not reach the rule).
# Usage: scripts/sim-canaries.sh [seed]
set -euo pipefail
cd "$(dirname "$0")/.."
seed=${1:-0xC0FFEE}

rows=(
  # ADR-0090 D5: the segment-blind scanner takes a foreign segment's
  # residue for this life's frames — an honest power-cut image of a
  # recycling log then refuses boot (RECYCLED RESIDUE REFUSED).
  "inf_canary_foreign_segment m2-recycle RESIDUE --sweep 64 --out target/canary-out"
  # F-L19-04: GET answers the stored value with one byte appended. The
  # shared-store replay cannot see it (both sides run the same code); the
  # shadow model (bins/inf-sim/src/harness/shadow.rs) must.
  "inf_canary_reply_lie m0-smoke divergence"
)

cargo build --release -p inf-sim --features dst --bin inf-sim
plain=target/release/inf-sim
fail=0
for row in "${rows[@]}"; do
  # shellcheck disable=SC2206
  parts=($row)
  cfg=${parts[0]}; name=${parts[1]}; expect=${parts[2]}; flags=("${parts[@]:3}")
  target="target/canary-$cfg"
  echo "== canary $cfg: building inf-sim with --cfg $cfg into $target"
  RUSTFLAGS="--cfg $cfg" cargo build --release -p inf-sim --features dst --bin inf-sim \
    --target-dir "$target"
  planted="$target/release/inf-sim"
  log=$(mktemp)
  echo "== canary $cfg: $name (seed $seed) on the planted build must go red"
  if "$planted" --scenario "$name" --seed "$seed" "${flags[@]}" > "$log" 2>&1; then
    echo "   NOT CAUGHT: the planted build ran green (the oracle has no teeth)"
    fail=1
  elif ! grep -q -- "$expect" "$log"; then
    echo "   red for another reason (expected a violation mentioning '$expect'):"
    tail -5 "$log"
    fail=1
  else
    echo "   caught: $(grep -m1 -- "$expect" "$log" | cut -c1-160)"
  fi
  rm -f "$log"
  echo "== canary $cfg: $name on the plain build must stay green"
  "$plain" --scenario "$name" --seed "$seed" "${flags[@]}" > /dev/null
done
if [ "$fail" -ne 0 ]; then echo "sim-canaries: FAILED"; exit 1; fi
echo "sim-canaries: ${#rows[@]} canaries caught, plain builds green (seed $seed)"
