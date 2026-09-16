#!/usr/bin/env bash
# Batch 68 (review B64-65-R06): the equivalence falsifier for a refactor
# that must not change behaviour — `inf-sim` built from a base revision
# and from the working tree, every scenario × seed run by both, and the
# reports compared **raw, byte for byte**. The simulator prints no
# wall-clock field (its `elapsed` values are virtual `now`), so nothing
# is filtered; `INF_AB_IGNORE=<regex>` may name a real elapsed-time field
# explicitly and is printed in the summary when set. Retained per run:
# both revisions, build flags, binary hashes, every raw report and exit
# code — the evidence the historical `grep -Ev 'sim_seconds|…'` recipe
# threw away (it dropped deterministic counters with the virtual time).
#
# Usage: scripts/sim-ab-diff.sh <base-rev> <out-dir> [scenario-list-file]
#   scenario-list-file: one `<scenario> [--cells N] <seed>` per line;
#   default = the fifteen-scenario × four-seed set of ADR-0125 D6.

set -euo pipefail
base=${1:?base revision}
out=${2:?output directory}
list=${3:-}
root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"
mkdir -p "$out"
out=$(cd "$out" && pwd)
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-2}"

work="$out/base-worktree"
if [ -n "$work" ] && [ -d "$work" ]; then
    git worktree remove --force "$work"
fi
git worktree add -q "$work" "$base"
trap '[ -n "$work" ] && [ -d "$work" ] && git worktree remove --force "$work"' EXIT

flags=(build -p inf-sim --features dst --release)
(cd "$work" && cargo "${flags[@]}") > "$out/build-base.log" 2>&1
cargo "${flags[@]}" > "$out/build-tree.log" 2>&1
base_bin="$work/target/release/inf-sim"
tree_bin="$root/target/release/inf-sim"
{
    echo "base=$(git -C "$work" rev-parse HEAD)"
    echo "tree=$(git rev-parse HEAD)+$(git status --porcelain | wc -l | tr -d ' ')-dirty-paths"
    echo "flags=cargo ${flags[*]}"
    echo "base_sha256=$(sha256sum "$base_bin" | cut -d' ' -f1)"
    echo "tree_sha256=$(sha256sum "$tree_bin" | cut -d' ' -f1)"
    echo "ignore=${INF_AB_IGNORE:-<none: raw byte comparison>}"
} > "$out/summary.txt"

if [ -z "$list" ]; then
    list="$out/scenarios.txt"
    : > "$list"
    for seed in 0xC0FFEE 0xD5EE0016 0x7A11 0x1234ABCD; do
        for sc in "m0-smoke" "m0-adversarial --cells 4" "m0-surface --cells 4" \
            "m0-admission" "m1-cache" "m2-durable" "m2-clean-stop" "m2-recycle" \
            "m2-combined" "m3-document" "m4-tiered" "m4-recovery" "m4-cold" \
            "m45-backfill" "m45-sidecar"; do
            echo "$sc $seed" >> "$list"
        done
    done
fi

mkdir -p "$out/base" "$out/tree"
same=0
diff=0
while read -r line; do
    [ -n "$line" ] || continue
    # shellcheck disable=SC2206
    parts=($line)
    seed=${parts[-1]}
    unset 'parts[-1]'
    name=$(echo "${parts[*]} $seed" | tr ' ' '_')
    for side in base tree; do
        bin=$base_bin; [ "$side" = tree ] && bin=$tree_bin
        status=0
        "$bin" --scenario "${parts[@]}" --seed "$seed" > "$out/$side/$name.log" 2>&1 || status=$?
        echo "$status" > "$out/$side/$name.exit"
    done
    if [ -n "${INF_AB_IGNORE:-}" ]; then
        a=$(grep -Ev "$INF_AB_IGNORE" "$out/base/$name.log" | sha256sum)
        b=$(grep -Ev "$INF_AB_IGNORE" "$out/tree/$name.log" | sha256sum)
    else
        a=$(sha256sum < "$out/base/$name.log")
        b=$(sha256sum < "$out/tree/$name.log")
    fi
    if [ "$a" = "$b" ] && cmp -s "$out/base/$name.exit" "$out/tree/$name.exit"; then
        echo "SAME ${parts[*]} $seed ($(wc -l < "$out/tree/$name.log" | tr -d ' ') lines, exit $(cat "$out/tree/$name.exit"))"
        same=$((same + 1))
    else
        echo "DIFF ${parts[*]} $seed"
        diff -u "$out/base/$name.log" "$out/tree/$name.log" | head -40 || true
        diff=$((diff + 1))
    fi
done < "$list" | tee "$out/ab-sim-diff.log"
same=$(grep -c '^SAME' "$out/ab-sim-diff.log" || true)
diff=$(grep -c '^DIFF' "$out/ab-sim-diff.log" || true)
echo "sim-ab-diff: $same same, $diff different (raw byte comparison; see $out/summary.txt)" | tee -a "$out/summary.txt"
[ "$diff" -eq 0 ]
