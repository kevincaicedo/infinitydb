#!/bin/bash
ulimit -v 16777216
cd /home/kcaicedo/Documents/Projects/databases/infinitydb || exit 1
echo "== fixed tree $(git rev-parse --short HEAD) (dirty: batch 55 working tree)"
cargo build --release -p inf-sim --features dst --bin inf-sim 2>&1 | tail -1
for seed in $(seq 1 4 125); do
  ./target/release/inf-sim --scenario m2-recycle --seed $seed 2>&1 | grep -E "ORACLE VIOLATION|PHANTOM" | cut -c1-260 | head -3
  echo "seed=$seed exit=${PIPESTATUS[0]}"
done
echo "== determinism on the red seed"
./target/release/inf-sim --scenario m2-recycle --seed 0x6d --verify-determinism 2>&1 | grep -E "inf-sim:|determin|PHANTOM|VIOLATION" | cut -c1-300 | tail -4
echo "seed=0x6d-verify exit=${PIPESTATUS[0]}"
echo "SWEEP DONE"
