#!/bin/bash
cd /home/kcaicedo/Documents/Projects/databases/infinitydb || exit 1
A=.artifacts/review/batch55
(ulimit -v 16777216; just sim-smoke > $A/sim-smoke.log 2>&1; echo "sim-smoke exit=$?" >> $A/sim-smoke.log)
(ulimit -v 8388608; cd crates/inf-log && cargo +nightly fuzz run frame_decode -- -max_total_time=300 > ../../$A/fuzz-frame-decode.log 2>&1; echo "fuzz frame_decode exit=$?" >> ../../$A/fuzz-frame-decode.log)
(ulimit -v 8388608; cd crates/inf-log && cargo +nightly fuzz run segment_read -- -max_total_time=300 > ../../$A/fuzz-segment-read.log 2>&1; echo "fuzz segment_read exit=$?" >> ../../$A/fuzz-segment-read.log)
(ulimit -v 16777216; just compat > $A/compat.log 2>&1; echo "compat exit=$?" >> $A/compat.log)
echo "TAIL DONE" >> $A/compat.log
