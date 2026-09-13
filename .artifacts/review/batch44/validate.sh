#!/bin/bash
set -u
cd /home/kcaicedo/Documents/Projects/databases/infinitydb
A=.artifacts/review/batch44
ulimit -v 12000000
just check > $A/just-check.log 2>&1; echo "just check exit $?" > $A/validate.status
cargo deny check > $A/cargo-deny.log 2>&1; echo "cargo deny exit $?" >> $A/validate.status
just sim-smoke > $A/sim-smoke.log 2>&1; echo "sim-smoke exit $?" >> $A/validate.status
echo DONE > $A/validate.done
