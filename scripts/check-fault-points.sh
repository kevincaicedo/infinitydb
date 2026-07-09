#!/usr/bin/env bash
# Fault-point inventory check (M2-S16, extended M2-S17): every fault point
# declared in a crate's `src/fault.rs` registry inventory must (a) be
# fired somewhere in library code — a declared-but-unwired point is dead
# weight — and (b) be exercised by at least one test, so crash-test
# coverage cannot rot ("an unexercised fault point fails the build" —
# m2-durability §S16). Declaration modules are discovered per crate
# (ADR-0019 D6: names are owned by the crate that owns the mechanism);
# tests may live in crate `tests/` trees or workspace test crates
# (`tests/*`, e.g. the M2-S17 crash matrix).
set -euo pipefail
cd "$(dirname "$0")/.."

mapfile -t DECLS < <(ls crates/*/src/fault.rs 2>/dev/null)
if [ "${#DECLS[@]}" -eq 0 ]; then
    echo "no fault-point declaration modules found (crates/*/src/fault.rs)"
    exit 1
fi

# Point names: every `pub const NAME: &str = "..."` in a declaration
# module (each ALL inventory is built from the same consts).
POINTS=()
for decl in "${DECLS[@]}"; do
    mapfile -t -O "${#POINTS[@]}" POINTS < <(grep -oP 'pub const [A-Z_]+: &str = "\K[a-z_]+' "$decl")
done
if [ "${#POINTS[@]}" -eq 0 ]; then
    echo "no fault points declared in: ${DECLS[*]}"
    exit 1
fi

fail=0
for point in "${POINTS[@]}"; do
    const_name=$(echo "$point" | tr '[:lower:]' '[:upper:]')
    # (a) fired in library code (any crate's src/, not the decl line).
    if ! grep -rn --include='*.rs' "fault::fire(crate::fault::${const_name})" crates/*/src >/dev/null; then
        echo "UNWIRED fault point: ${point} is declared but never fired in library code"
        fail=1
    fi
    # (b) exercised by at least one test: crate tests/ trees or workspace
    # test crates reference the const or the literal name.
    if ! grep -rn --include='*.rs' -e "fault::${const_name}" -e "\"${point}\"" crates/*/tests tests/*/src tests/*/tests 2>/dev/null >/dev/null; then
        echo "UNEXERCISED fault point: ${point} has no test arming it"
        fail=1
    fi
done

if [ "$fail" -ne 0 ]; then
    echo "fault-point inventory check FAILED"
    exit 1
fi
echo "fault-point inventory OK (${#POINTS[@]} points wired + exercised)"
