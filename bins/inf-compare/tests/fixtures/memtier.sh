#!/bin/sh
set -eu
json=''
port=''
while [ "$#" -gt 0 ]; do
    case "$1" in
        --version) echo fixture; exit 0 ;;
        --json-out-file) json=$2; shift ;;
        -p) port=$2; shift ;;
    esac
    shift
done
if [ -f "$COMPARE_FIXTURE/observe-affinity" ]; then
    role=memtier-fill
    if [ -n "$json" ]; then role=memtier-measured; fi
    /usr/bin/python3 "$COMPARE_FIXTURE/affinity.py" "$role"
fi
[ -n "$json" ] || exit 0
echo "$port" >> "$COMPARE_FIXTURE/events"
count_file="$COMPARE_FIXTURE/count-$port"
count=0
if [ -f "$count_file" ]; then read -r count < "$count_file"; fi
count=$((count + 1))
echo "$count" > "$count_file"
if [ -f "$COMPARE_FIXTURE/fail" ]; then
    measured=$(wc -l < "$COMPARE_FIXTURE/events")
    if [ "$measured" -eq 2 ]; then exit 7; fi
fi
if [ -f "$COMPARE_FIXTURE/oversize" ]; then
    dd if=/dev/zero of="$json" bs=1048576 count=17 2>/dev/null
    exit 0
fi
case $((count % 3)) in
    1) metric=10 ;;
    2) metric=30 ;;
    0) metric=20 ;;
esac
cat > "$json" <<EOF
{"ALL STATS":{"Totals":{"Ops/sec":$metric,"Average Latency":1,"Percentile Latencies":{"p50.00":1,"p99.00":2,"p99.90":3}}}}
EOF
