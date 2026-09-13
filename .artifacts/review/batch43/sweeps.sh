#!/bin/bash
set -u
cd /home/kcaicedo/Documents/Projects/databases/infinitydb
A=.artifacts/review/batch43
ulimit -v 8000000
run() { # binary scenario logname
  timeout 1500 "$1" --scenario "$2" --sweep 24 --shard 0/1 --seed 0xB43 2>&1 | grep -E "HOLD EPISODE|FLUSH-class barrier|panicked|sweep shard|violations" | head -40 > "$A/$3"
}
run $A/inf-sim-prefix-l0104 m2-durable   pre-fix-l01-04-dst-durable-sweep24.log
run $A/inf-sim-prefix-l0104 m2-fill-tick pre-fix-l01-04-dst-filltick-sweep24.log
run $A/inf-sim-prefix-l0105 m2-durable   pre-fix-l01-05-dst-durable-sweep24.log
run $A/inf-sim-fixed        m2-durable   post-fix-dst-m2-durable-sweep24.log
run $A/inf-sim-fixed        m2-fill-tick post-fix-dst-m2-fill-tick-sweep24.log
run $A/inf-sim-fixed        m2-reorder-window post-fix-dst-m2-reorder-window-sweep24.log
echo DONE > $A/sweeps.done
