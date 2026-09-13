#!/bin/bash
set -u
cd /home/kcaicedo/Documents/Projects/databases/infinitydb
A=.artifacts/review/batch44
python3 $A/toggle-prefix.py fifo-on || exit 1
cargo build --release -p inf-sim --features dst --bin inf-sim > $A/build-prefix.log 2>&1 || { python3 $A/toggle-prefix.py fifo-off; echo FAIL-PREFIX > $A/build.done; exit 1; }
cp target/release/inf-sim $A/inf-sim-prefix-fifo
python3 $A/toggle-prefix.py fifo-off || exit 1
cargo build --release -p inf-sim --features dst --bin inf-sim > $A/build-fixed.log 2>&1 || { echo FAIL-FIXED > $A/build.done; exit 1; }
cp target/release/inf-sim $A/inf-sim-fixed
echo OK > $A/build.done
