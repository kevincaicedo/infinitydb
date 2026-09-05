#!/usr/bin/env bash
# Fault-point inventory check (M2-S16/S17; rewritten at ADR-0106 D9 on
# review 2026-08-30, F-L20-04). Every fault point declared in a crate's
# `src/fault.rs` must be (a) FIRED in production library code and (b) ARMED
# by an exerciser — "an unexercised fault point fails the build"
# (m2-durability §S16).
#
# What the old gate did instead: `grep -e fault::CONST -e "point"` over the
# tests trees. Any textual mention satisfied it. Proven on this tree:
# `shadow_twin_read_fail` had ZERO arming call sites; its two matches were
# a `//!` doc line and an assert's message string, and deleting the entire
# `#[test]` body left the gate printing "OK (28 points wired + exercised)".
# Its name extractor was also `[a-z_]+`, so a point whose name carries a
# digit (`ckpt_v2_torn`) was dropped from the inventory in both directions
# — declared, never fired, never exercised, never counted.
#
# Now:
#   * every `pub const … : &str = "…"` in a declaration module must parse
#     ([a-z0-9_]+ / [A-Z0-9_]+), and one that does not is a scope error;
#   * every declared point must be a member of its crate's `ALL` inventory,
#     and every `ALL` member must be a declared const (the runtime uses ALL);
#   * (a) firing is read from PRODUCTION source only (test modules stripped
#     — ADR-0106 D3), so a fire site that exists only in a unit test fails;
#   * (b) arming is a *statement-shaped* reference (scripts/fault-arming.awk):
#     `fault::arm(` or a `(POINT, FaultSpec::…)` plan row, never a comment
#     and never a message string. The exerciser set spans crate `tests/`
#     trees, the workspace test crates (`tests/*`), the test-only modules
#     inside `crates/*/src`, and `bins/inf-sim/src` — the DST scenarios arm
#     two points that no test can arm directly (a real node's registry is
#     thread-local), which the old search set never looked at;
#   * the scope line prints the zone each point is armed from.
#
# Portable bash 3.2 / POSIX awk (macOS is in the CI matrix).
set -euo pipefail
SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
cd "${INF_CHECK_ROOT:-$SCRIPT_DIR/..}"
ARMING="$SCRIPT_DIR/fault-arming.awk"
STRIP="$SCRIPT_DIR/strip-test-modules.awk"
[ -f "$ARMING" ] && [ -f "$STRIP" ] || { echo "FAULT SCOPE ERROR: helper awk missing next to $0"; exit 2; }

work=$(mktemp -d)
[ -n "$work" ] && [ -d "$work" ] || { echo "FAULT SCOPE ERROR: mktemp failed"; exit 2; }
trap '[ -n "$work" ] && [ -d "$work" ] && rm -rf "$work"' EXIT

fail=0

# ---- 1. declaration modules, parsed exhaustively ------------------------
DECLS=()
for decl in crates/*/src/fault.rs; do
    [ -e "$decl" ] && DECLS+=("$decl")
done
if [ "${#DECLS[@]}" -eq 0 ]; then
    echo "FAULT SCOPE ERROR: no declaration modules found (crates/*/src/fault.rs) from $(pwd)"
    exit 1
fi

: >"$work/points"
for decl in "${DECLS[@]}"; do
    # Every `pub const NAME: &str = "value";` must parse. A line the
    # extractor cannot read is a silent inventory hole, not a skip.
    while IFS= read -r line; do
        case "$line" in
            OK\ *) echo "${line#OK } $decl" >>"$work/points" ;;
            BAD\ *)
                echo "FAULT SCOPE ERROR: $decl: unparsable point declaration: ${line#BAD }"
                fail=1 ;;
        esac
    done < <(awk '
        /pub const [A-Za-z0-9_]+[[:space:]]*:[[:space:]]*&.?str[[:space:]]*=/ {
            if (match($0, /pub const [A-Z][A-Z0-9_]*[[:space:]]*:[[:space:]]*&.?str[[:space:]]*=[[:space:]]*"[a-z][a-z0-9_]*"/)) {
                m = substr($0, RSTART, RLENGTH)
                c = m
                sub(/^pub const /, "", c)
                sub(/[[:space:]]*:.*$/, "", c)
                match(m, /"[a-z][a-z0-9_]*"/)
                v = substr(m, RSTART + 1, RLENGTH - 2)
                print "OK " v " " c
            } else {
                line = $0; sub(/^[[:space:]]*/, "", line)
                print "BAD " line
            }
        }' "$decl")
done

POINTS=()
while IFS= read -r row; do POINTS+=("$row"); done <"$work/points"
if [ "${#POINTS[@]}" -eq 0 ]; then
    echo "FAULT SCOPE ERROR: no fault points declared in: ${DECLS[*]}"
    exit 1
fi

# ---- 2. ALL inventory agreement ----------------------------------------
# The runtime and the crash matrix enumerate `ALL`; a const missing from it
# is invisible to both while still looking declared.
for decl in "${DECLS[@]}"; do
    decl_consts=$(awk '$0 ~ /pub const [A-Z][A-Z0-9_]*[[:space:]]*:[[:space:]]*&.?str/ {
        c = $0; sub(/^.*pub const /, "", c); sub(/[[:space:]]*:.*$/, "", c); print c }' "$decl" | sort)
    [ -n "$decl_consts" ] || continue
    all_members=$(awk '
        /pub const ALL[[:space:]]*:[[:space:]]*&\[&.?str\][[:space:]]*=/ { inall = 1 }
        inall == 1 {
            line = $0
            while (match(line, /[A-Z][A-Z0-9_]{2,}/)) {
                t = substr(line, RSTART, RLENGTH)
                line = substr(line, RSTART + RLENGTH)
                if (t != "ALL") print t
            }
            if ($0 ~ /\];/) { inall = 0 }
        }' "$decl" | sort -u)
    missing=$(comm -23 <(printf '%s\n' "$decl_consts") <(printf '%s\n' "$all_members") || true)
    extra=$(comm -13 <(printf '%s\n' "$decl_consts") <(printf '%s\n' "$all_members") || true)
    for m in $missing; do
        echo "FAULT violation: $decl declares $m but ALL does not list it (the runtime enumerates ALL)"
        fail=1
    done
    for e in $extra; do
        echo "FAULT violation: $decl: ALL lists $e, which is not a declared point const"
        fail=1
    done
done

# ---- 3. the two evidence zones -----------------------------------------
# Production library code (test modules stripped) — where a point must fire.
# Library code only (`crates/*/src`): a point fired solely from a binary
# would be a simulator prop, not a product fault point.
prod_files=0
: >"$work/fired"
for dir in crates/*/src; do
    [ -d "$dir" ] || continue
    while IFS= read -r f; do
        case "$f" in */src/fault.rs) continue ;; esac
        grep -q 'fault::' "$f" || continue
        prod_files=$((prod_files + 1))
        awk -f "$STRIP" "$f" | awk -f "$ARMING" | awk '$1=="FIRE"{print $3}' >>"$work/fired"
    done < <(find "$dir" -name '*.rs' | sort)
done

# Exercisers: crate tests trees, workspace test crates, the DST scenarios,
# and the test-only modules inside crate sources.
exer_files=0
: >"$work/armed"
scan_arm() { # <file> <zone>
    awk -f "$ARMING" "$1" | awk -v z="$2" '$1=="ARM"{print $3 " " z}' >>"$work/armed"
}
for dir in crates/*/tests tests/*/src tests/*/tests bins/inf-sim/src; do
    [ -d "$dir" ] || continue
    zone=$(printf '%s' "$dir" | sed 's|^bins/inf-sim/src$|dst|; s|^crates/.*/tests$|crate-tests|; s|^tests/.*|workspace-tests|')
    while IFS= read -r f; do
        grep -q 'fault' "$f" || continue
        exer_files=$((exer_files + 1))
        scan_arm "$f" "$zone"
    done < <(find "$dir" -name '*.rs' | sort)
done
for dir in crates/*/src; do
    [ -d "$dir" ] || continue
    while IFS= read -r f; do
        grep -q 'fault::arm' "$f" || continue
        exer_files=$((exer_files + 1))
        awk -v mode=testonly -f "$STRIP" "$f" >"$work/testonly.rs"
        scan_arm "$work/testonly.rs" "unit-tests"
    done < <(find "$dir" -name '*.rs' | sort)
done
sort -u "$work/armed" -o "$work/armed"
sort -u "$work/fired" -o "$work/fired"

# ---- 4. the verdict, per point -----------------------------------------
zones=""
for row in "${POINTS[@]}"; do
    set -- $row
    point=$1
    decl=$3
    if ! grep -qx "$point" "$work/fired"; then
        echo "UNWIRED fault point: $point ($decl) is declared but never fired in production library code"
        fail=1
    fi
    where=$(awk -v p="$point" '$1==p{printf "%s%s", (n++?",":""), $2}' "$work/armed")
    if [ -z "$where" ]; then
        echo "UNARMED fault point: $point ($decl) has no arming site — a doc comment or an"
        echo "  assert message is not evidence; arm it with fault::arm(…) or a (POINT, FaultSpec::…) plan row"
        fail=1
    else
        zones="$zones  armed $point <- $where
"
    fi
done

printf '%s' "$zones"
scope="${#POINTS[@]} points / ${#DECLS[@]} declaration modules, $prod_files production files scanned for firing, $exer_files exerciser files scanned for arming (window +/-2 lines)"
if [ "$fail" -ne 0 ]; then
    echo "fault-point inventory FAILED ($scope)"
    exit 1
fi
echo "fault-point inventory OK ($scope)"
