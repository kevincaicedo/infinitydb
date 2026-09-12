#!/usr/bin/env bash
set -euo pipefail
# Run from infinitydb/.
mkdir -p target/batch32-equivalence/src target/batch32-equivalence/seeds
git show c4ea42c:bins/inf-sim/src/txmodel.rs > target/batch32-equivalence/src/before.rs
cp bins/inf-sim/src/txmodel.rs target/batch32-equivalence/src/after.rs
cp bins/inf-sim/seeds/watch-redis-oracle.txt target/batch32-equivalence/seeds/
cp .artifacts/review/batch32/equivalence-main.rs target/batch32-equivalence/main.rs
rustc --edition 2024 -O target/batch32-equivalence/main.rs -o target/batch32-equivalence/run
./target/batch32-equivalence/run
