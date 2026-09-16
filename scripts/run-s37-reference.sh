#!/usr/bin/env bash
# Q, D9 and DBSIZE on the reference host; rules in docs/validation-s37.md.
set -euo pipefail
cd "$(dirname "$0")/.."
if [[ "$(hostname)" != HomeLab ]]; then
    echo "Reference host must be HomeLab (ADR-0022)." >&2
    exit 1
fi
if [[ -n "$(git status --porcelain)" ]]; then
    echo "Commit the reviewed source and protocol before the reference campaign." >&2
    exit 1
fi
if pgrep -x infinityd >/dev/null; then
    echo "An infinityd process is already running; use an idle reference host." >&2
    exit 1
fi
campaign_root="${S37_CAMPAIGN_ROOT:-$HOME/bench-data/s37/campaign-R-$(date -u +%Y%m%dT%H%M%SZ)}"
mkdir "$campaign_root"
exec > >(tee "$campaign_root/campaign.log") 2>&1
git rev-parse HEAD
git status --porcelain
uname -a
lscpu
CARGO_NET_OFFLINE=true cargo build --release -p infinityd -p inf-bench -p inf
target/release/infinityd --version
sha256sum target/release/infinityd target/release/inf-bench
target/release/inf-bench env-check
perf stat -e task-clock -- true
mkdir "$campaign_root/data" "$campaign_root/stderr"
target/release/inf probe-device "$campaign_root/data"
cp "$campaign_root/data/io-properties.toml" "$campaign_root/io-properties.toml"
export INF_GATERUN_STDERR_DIR="$campaign_root/stderr"
python3 scripts/s37-host-sample.py "$campaign_root/host.jsonl" &
sampler_pid=$!
trap 'kill "$sampler_pid" 2>/dev/null || true; wait "$sampler_pid" 2>/dev/null || true' EXIT
common=(gate-run m4.5 --only-s37 --reference-box --cells 4 --pin-start 0
    --replicates 3 --duration 20 --s37-keys 1000000 --s37-del-keys 12288
    --leg-idle-s 40 --device-probe off)
for row in Q D9 DBSIZE; do
    case "$row" in
        Q) options=(--s37-ticketed-del --s37-del-cycles 4) ;;
        D9) options=(--s37-shadow --s37-controls --read-leg-fill) ;;
        DBSIZE) options=(--s37-dbsize --s37-del-cycles 1) ;;
    esac
    target/release/inf-bench env-check
    mkdir "$campaign_root/data/$row"
    cp "$campaign_root/io-properties.toml" "$campaign_root/data/$row/io-properties.toml"
    perf record -F 99 -g --call-graph dwarf -o "$campaign_root/$row.perf" -- \
        taskset -c 8,10,12,14 target/release/inf-bench "${common[@]}" "${options[@]}" \
        --data-root "$campaign_root/data/$row" --artifacts-root "$campaign_root/$row"
    target/release/inf-bench env-check
    kill -0 "$sampler_pid"
    perf report --stdio -i "$campaign_root/$row.perf" > "$campaign_root/$row-profile.txt"
    printf '%s complete\n' "$row"
done
printf 'Measurements complete. Review validity and D9 thresholds before accepting or changing a default.\n'
printf 'Record commands, revisions, environment and results in the ledger; keep output local.\n'
