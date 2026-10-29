#!/bin/sh
set -eu
test=SET
while [ "$#" -gt 0 ]; do
    case "$1" in
        --version) echo fixture; exit 0 ;;
        -t) test=$2; shift ;;
    esac
    shift
done
if [ -f "$COMPARE_FIXTURE/observe-affinity" ]; then
    /usr/bin/python3 "$COMPARE_FIXTURE/affinity.py" redis-benchmark
fi
echo '"test","rps","avg","min","p50","p95","p99","max"'
printf '"%s","40","1","0","2","3","4","5"\n' "$test"
