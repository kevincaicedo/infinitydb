#!/usr/bin/env bash
# Cell-code deny list (M0-S06): no async runtimes, no std sync primitives,
# no thread spawn/sleep inside cell-resident crates. Backstops clippy.toml.
set -euo pipefail
cd "$(dirname "$0")/.."

CELL_CRATES=(
    crates/inf-fabric/src
    crates/inf-runtime/src
    crates/inf-wire/src
    crates/inf-store/src
    crates/inf-alloc/src
    crates/inf-server/src/cell
)

PATTERNS=(
    'tokio::'
    'async_std::'
    'std::sync::Mutex'
    'std::sync::RwLock'
    'std::sync::Condvar'
    'thread::sleep'
)

fail=0
for dir in "${CELL_CRATES[@]}"; do
    [ -d "$dir" ] || continue
    for pat in "${PATTERNS[@]}"; do
        if hits=$(grep -rn --include='*.rs' -e "$pat" "$dir" | grep -v 'denylist-allow'); then
            echo "DENY-LISTED in cell code ($pat):"
            echo "$hits"
            fail=1
        fi
    done
done

if [ "$fail" -ne 0 ]; then
    echo "Cell code must not block, lock, or pull in an async runtime (L1/L6)."
    exit 1
fi
echo "cell deny-list OK"
