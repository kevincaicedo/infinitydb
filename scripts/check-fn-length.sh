#!/usr/bin/env bash
# Review 2026-08-30 lane L18 R1 (batch 64, ADR-0125): INFINITY_STYLE's
# "~70 lines per function" had no mechanical check — clippy's
# `too_many_lines` is pedantic (off) and the eleven `#[allow]`s that named
# it were inert. This gate turns the rule into a ratchet:
#
#   * clippy runs the lint at `too-many-lines-threshold` (clippy.toml, 70
#     code lines — blank lines and comments excluded) on every lib and bin
#     target, production code only (`--lib --bins`: no test cfg);
#   * `docs/fn-length-baseline.tsv` is the disclosed backlog, one row per
#     file: `count<TAB>file`. A file above its row fails ("split it, never
#     raise the baseline"); a file below its row fails too ("lower the
#     baseline") so the number in the docs is always the number in the
#     tree; a file with no row fails at its first breach.
#   * a function may opt out only with `#[allow(clippy::too_many_lines,
#     reason = "…")]` — the reason is mandatory and every opt-out is listed
#     on the OK line, so the exemptions are visible, never silent.
#
# INF_FN_LENGTH_INPUT=<file> replaces the cargo run with a captured
# `--message-format=short` log (the self-test's planted breaches).

set -euo pipefail
cd "${INF_CHECK_ROOT:-$(dirname "$0")/..}"
BASELINE="${INF_FN_LENGTH_BASELINE:-docs/fn-length-baseline.tsv}"
work=$(mktemp -d)
trap '[ -n "$work" ] && [ -d "$work" ] && rm -rf "$work"' EXIT

if [ -n "${INF_FN_LENGTH_INPUT:-}" ]; then
    cp "$INF_FN_LENGTH_INPUT" "$work/clippy.log"
else
    if ! grep -Eq '^too-many-lines-threshold *= *70$' clippy.toml; then
        echo "FN-LENGTH SCOPE ERROR: clippy.toml does not pin too-many-lines-threshold = 70"
        exit 1
    fi
    # The simulator's lib only builds under its `dst` feature (ADR-0107:
    # never on a normal edge), so it lints in its own invocation.
    {
        cargo clippy --workspace --exclude inf-sim --lib --bins --message-format=short -- \
            -W clippy::too_many_lines &&
        cargo clippy -p inf-sim --features dst --lib --bins --message-format=short -- \
            -W clippy::too_many_lines
    } >"$work/clippy.log" 2>&1 || {
        cat "$work/clippy.log"
        echo "FN-LENGTH SCOPE ERROR: cargo clippy failed"
        exit 1
    }
fi

# `path:line:col: warning: this function has too many lines (N/70)`
grep -E 'warning: this function has too many lines' "$work/clippy.log" \
    | sed -E 's/^([^:]+):[0-9]+:[0-9]+: .*\(([0-9]+)\/[0-9]+\).*$/\1\t\2/' \
    | sort > "$work/breaches"
if [ ! -s "$work/breaches" ] && ! grep -q 'Finished\|Checking\|warning\|^$' "$work/clippy.log" && [ -z "${INF_FN_LENGTH_INPUT:-}" ]; then
    echo "FN-LENGTH SCOPE ERROR: clippy produced no output"
    exit 1
fi
cut -f1 "$work/breaches" | sort | uniq -c | sed -E 's/^ *([0-9]+) /\1\t/' > "$work/counts"

fail=0
if [ ! -f "$BASELINE" ]; then
    echo "FN-LENGTH SCOPE ERROR: $BASELINE is missing"
    exit 1
fi
grep -v '^#' "$BASELINE" | grep -v '^[[:space:]]*$' | sort -t "$(printf '\t')" -k2,2 > "$work/baseline" || true
# tree vs baseline
while IFS=$'\t' read -r count file; do
    base=$(awk -F'\t' -v f="$file" '$2 == f { print $1 }' "$work/baseline")
    if [ -z "$base" ]; then
        echo "FN-LENGTH violation: $file has $count function(s) over the bar and no baseline row — split them (functions over 70 lines:)"
        awk -F'\t' -v f="$file" '$1 == f { print "    " f " (" $2 " lines)" }' "$work/breaches"
        fail=1
    elif [ "$count" -gt "$base" ]; then
        echo "FN-LENGTH violation: $file has $count function(s) over the bar, baseline $base — split the new one, never raise the baseline"
        fail=1
    elif [ "$count" -lt "$base" ]; then
        echo "FN-LENGTH ratchet: $file has $count function(s) over the bar, baseline $base — lower the row in $BASELINE to $count"
        fail=1
    fi
done < "$work/counts"
# baseline vs tree
while IFS=$'\t' read -r base file; do
    count=$(awk -F'\t' -v f="$file" '$2 == f { print $1 }' "$work/counts")
    if [ -z "$count" ]; then
        echo "FN-LENGTH ratchet: $file has no function over the bar, baseline $base — delete the row in $BASELINE"
        fail=1
    fi
done < "$work/baseline"

# opt-outs: reason mandatory, every site disclosed
allows=0
while IFS= read -r site; do
    allows=$((allows + 1))
    if ! printf '%s' "$site" | grep -q 'reason *= *"'; then
        echo "FN-LENGTH violation: opt-out without a reason — $site"
        fail=1
    fi
done < <(grep -rn --include='*.rs' -E '#\[allow\([^]]*clippy::too_many_lines' crates bins tests 2>/dev/null || true)

total=$(wc -l < "$work/breaches" | tr -d ' ')
filecount=$(wc -l < "$work/counts" | tr -d ' ')
scope="$total function(s) over 70 lines in $filecount file(s) (the baseline backlog), $allows reasoned opt-out(s)"
if [ "$fail" -ne 0 ]; then
    echo "fn-length FAILED: $scope"
    exit 1
fi
echo "fn-length OK: $scope"
grep -rn --include='*.rs' -E '#\[allow\([^]]*clippy::too_many_lines' crates bins tests 2>/dev/null | sed 's/^/    opt-out: /' || true
