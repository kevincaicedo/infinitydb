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
#     tree; a file with no row fails at its first breach. The baseline is
#     validated before it is compared (ADR-0125 A3): a row that is not
#     `positive-integer<TAB>in-scope .rs path`, or a path listed twice, is
#     a scope error — never a silent pass;
#   * a function may opt out only with `#[allow(clippy::too_many_lines,
#     reason = "…")]` on the function itself — the reason is mandatory and
#     every opt-out is listed on the OK line, so the exemptions are visible,
#     never silent. The audit is structural (ADR-0125 A3): attributes are
#     read whole (multi-line included), an inner `#![allow]`, an attribute
#     on a `mod`/`impl`/`trait`/type, a `cfg_attr`, an `expect`, or a group
#     suppression (`clippy::pedantic`, `warnings`) is a violation;
#   * zero breaches and zero baseline rows is the ratchet's final state and
#     passes; a scan that produced no compiler output is still a scope
#     error (ADR-0106 D2).
#
# INF_FN_LENGTH_INPUT=<file> replaces the cargo run with a captured
# `--message-format=short` log (the self-test's planted breaches).
# INF_FN_LENGTH_HOST=<uname -s> names the host (ADR-0125 A7): the baseline
# is recorded on Linux, where the ratchet-down direction is authoritative;
# elsewhere a row whose cfg-gated file never compiled is disclosed, not
# failed. A new breach fails on every host.

set -euo pipefail
cd "${INF_CHECK_ROOT:-$(dirname "$0")/..}"
BASELINE="${INF_FN_LENGTH_BASELINE:-docs/fn-length-baseline.tsv}"
HOST="${INF_FN_LENGTH_HOST:-$(uname -s)}"
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

# `path:line:col: warning: this function has too many lines (N/70)`.
# Keyed by site (`path:line`) and deduplicated before counting: cargo
# emits a crate's diagnostics once per feature set it compiles, and the
# simulator's `dst` lane recompiles every crate upstream of it under
# `collision-oracle` — batch 64's baseline counted those twice (ADR-0125
# A1). Two functions of the same length in one file are two sites. The
# line number is cut by awk: BSD sed reads `[^\t]` as "not backslash, not
# t", which kept `path:line` as the key on macOS (ADR-0125 A7).
{ grep -E 'warning: this function has too many lines' "$work/clippy.log" || true; } \
    | sed -E 's/^([^:]+:[0-9]+):[0-9]+: .*\(([0-9]+)\/[0-9]+\).*$/\1\t\2/' \
    | sort -u | awk -F'\t' 'BEGIN { OFS = "\t" } { sub(/:[0-9]+$/, "", $1); print }' \
    | sort > "$work/breaches"
# No breach at all is the ratchet's goal, not evidence of a scan: the
# compiler must have finished (ADR-0125 A3, ADR-0106 D2).
if [ ! -s "$work/breaches" ] && ! grep -q 'Finished' "$work/clippy.log"; then
    echo "FN-LENGTH SCOPE ERROR: clippy reported no breach and no completed build"
    exit 1
fi
cut -f1 "$work/breaches" | sort | uniq -c | sed -E 's/^ *([0-9]+) /\1\t/' > "$work/counts"

fail=0
if [ ! -f "$BASELINE" ]; then
    echo "FN-LENGTH SCOPE ERROR: $BASELINE is missing"
    exit 1
fi
# Baseline schema (ADR-0125 A3): `positive-integer<TAB>crates|bins|tests/….rs`,
# one row per path. A malformed row used to make bash's integer compare
# skip both branches — and pass.
grep -v '^#' "$BASELINE" | grep -v '^[[:space:]]*$' > "$work/baseline-raw" || true
if ! awk -F'\t' '
    NF != 2 || $1 !~ /^[1-9][0-9]*$/ || $2 !~ /^(crates|bins|tests)\/[^\t ]+\.rs$/ {
        printf "FN-LENGTH SCOPE ERROR: malformed baseline row %d: %s\n", NR, $0; bad = 1
    }
    seen[$2]++ == 1 { printf "FN-LENGTH SCOPE ERROR: baseline lists %s twice\n", $2; bad = 1 }
    END { exit bad }' "$work/baseline-raw"; then
    exit 1
fi
sort -t "$(printf '\t')" -k2,2 "$work/baseline-raw" > "$work/baseline"
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
# baseline vs tree (ADR-0125 A7: the baseline's host is Linux — elsewhere
# a row's file may simply be cfg-gated out of this build)
skipped=0
while IFS=$'\t' read -r base file; do
    count=$(awk -F'\t' -v f="$file" '$2 == f { print $1 }' "$work/counts")
    if [ -z "$count" ]; then
        if [ "$HOST" = Linux ]; then
            echo "FN-LENGTH ratchet: $file has no function over the bar, baseline $base — delete the row in $BASELINE"
            fail=1
        else
            echo "FN-LENGTH note: $file (baseline $base) not compiled on this host ($HOST) — the ratchet for it runs on Linux"
            skipped=$((skipped + 1))
        fi
    fi
done < "$work/baseline"

# Opt-outs (ADR-0125 D2 + A3): structural audit of every attribute that
# could silence the lint. Prints `ok<TAB>site<TAB>reason` for a reasoned
# function-level allow and `violation<TAB>site<TAB>why` otherwise.
for dir in crates bins tests; do
    [ -d "$dir" ] || { echo "FN-LENGTH SCOPE ERROR: $dir/ is missing"; exit 1; }
done
python3 - crates bins tests > "$work/optouts" <<'PY'
import os
import re
import sys

GROUP = re.compile(r"clippy\s*::\s*pedantic|(^|[(,\s])warnings([),\s]|$)")
LINT = re.compile(r"too_many_lines|clippy\s*::\s*pedantic|(^|[(,\s])warnings([),\s]|$)")
REASON = re.compile(r'reason\s*=\s*"')
FN = re.compile(
    r'^\s*(pub(\([^)]*\))?\s+)?(default\s+)?(const\s+)?(async\s+)?(unsafe\s+)?'
    r'(extern\s+("[^"]*"\s+)?)?fn\s+[A-Za-z_]'
)


def attribute_end(lines, start):
    """Index one past the line where the attribute opened at `start` closes."""
    depth = 0
    in_str = False
    for i in range(start, len(lines)):
        text = lines[i]
        j = 0
        while j < len(text):
            ch = text[j]
            if in_str:
                if ch == "\\":
                    j += 1
                elif ch == '"':
                    in_str = False
            elif ch == '"':
                in_str = True
            elif ch == "[":
                depth += 1
            elif ch == "]":
                depth -= 1
                if depth == 0:
                    return i + 1
            j += 1
    return len(lines)


def next_item(lines, at):
    """The first line after `at` that is not blank, a comment or an attribute."""
    i = at
    while i < len(lines):
        s = lines[i].strip()
        if not s or s.startswith("//"):
            i += 1
        elif s.startswith("#[") or s.startswith("#!["):
            i = attribute_end(lines, i)
        else:
            return lines[i]
    return ""


def audit(path):
    with open(path, encoding="utf-8", errors="replace") as f:
        lines = f.read().split("\n")
    i = 0
    while i < len(lines):
        s = lines[i].strip()
        if not (s.startswith("#[") or s.startswith("#![")):
            i += 1
            continue
        end = attribute_end(lines, i)
        text = " ".join(l.strip() for l in lines[i:end])
        site = f"{path}:{i + 1}"
        head = text.split("reason")[0]  # lint names; never the reason text
        if LINT.search(head):
            if s.startswith("#!["):
                print(f"violation\t{site}\tinner attribute (crate/module scope) silences the lint")
            elif "cfg_attr" in head:
                print(f"violation\t{site}\tcfg_attr suppression is conditional and hidden")
            elif re.search(r"\bexpect\s*\(", head):
                print(f"violation\t{site}\texpect is unfulfilled in normal builds; use allow with a reason")
            elif GROUP.search(head):
                print(f"violation\t{site}\tlint-group suppression (pedantic/warnings) hides the lint")
            elif not REASON.search(text):
                print(f"violation\t{site}\topt-out without a reason")
            elif not FN.match(next_item(lines, end)):
                print(f"violation\t{site}\topt-out is not on a function (mod/impl/trait/type scope)")
            else:
                reason = re.search(r'reason\s*=\s*"((?:[^"\\]|\\.)*)"', text)
                why = re.sub(r"\\\s*", "", reason.group(1)) if reason else ""
                print(f"ok\t{site}\t{why}")
        i = end


for root in sys.argv[1:]:
    for dirpath, _, files in os.walk(root):
        for name in sorted(files):
            if name.endswith(".rs"):
                audit(os.path.join(dirpath, name))
PY
while IFS=$'\t' read -r kind site why; do
    if [ "$kind" = violation ]; then
        echo "FN-LENGTH violation: $why — $site"
        fail=1
    fi
done < "$work/optouts"
allows=$(grep -c '^ok' "$work/optouts" || true)

total=$(wc -l < "$work/breaches" | tr -d ' ')
filecount=$(wc -l < "$work/counts" | tr -d ' ')
scope="$total function(s) over 70 lines in $filecount file(s) (the baseline backlog), $allows reasoned opt-out(s)"
if [ "$skipped" -ne 0 ]; then
    scope="$scope; $skipped baseline row(s) not compiled on $HOST"
fi
if [ "$fail" -ne 0 ]; then
    echo "fn-length FAILED: $scope"
    exit 1
fi
echo "fn-length OK: $scope"
{ grep '^ok' "$work/optouts" || true; } | awk -F'\t' '{ print "    opt-out: " $2 " — " $3 }'
