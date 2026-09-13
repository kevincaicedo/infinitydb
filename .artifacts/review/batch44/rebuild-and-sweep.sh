#!/bin/bash
set -u
cd /home/kcaicedo/Documents/Projects/databases/infinitydb
A=.artifacts/review/batch44
$A/build-sims.sh || { echo BUILD-FAIL > $A/round2.done; exit 1; }
ulimit -v 8000000
run() { timeout 1800 "$1" --scenario "$2" --sweep 24 --shard 0/1 --seed 0xB44 2>&1 | grep -E "WRITE-THROUGH TICKETS|panicked|sweep shard|violations|write_through_entries_max" | head -60 > "$A/$3"; }
run $A/inf-sim-prefix-fifo m2-fua-pending pre-fix-fifo-dst-fua-pending-sweep24.log
run $A/inf-sim-fixed       m2-fua-pending post-fix-dst-m2-fua-pending-sweep24.log
run $A/inf-sim-fixed       m2-durable     post-fix-dst-m2-durable-sweep24.log
echo DONE > $A/round2.done
