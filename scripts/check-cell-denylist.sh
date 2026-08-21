#!/usr/bin/env bash
# Cell-code deny list (M0-S06): no async runtimes, no std sync primitives,
# no thread spawn/sleep inside cell-resident crates. Backstops clippy.toml.
# M4.5-S36 (ADR-0088): no ambient clocks either — L7's injected-time rule
# was enforced by convention until the device budget became the first
# clock consumer added since it was written (`inf-foundation::time` is
# the one sanctioned `Instant::now`, outside the cell crates).
set -euo pipefail
cd "$(dirname "$0")/.."

CELL_CRATES=(
    crates/inf-fabric/src
    crates/inf-runtime/src
    crates/inf-wire/src
    crates/inf-store/src
    crates/inf-alloc/src
    crates/inf-doc/src
    crates/inf-server/src/cell
)

PATTERNS=(
    'tokio::'
    'async_std::'
    'std::sync::Mutex'
    'std::sync::RwLock'
    'std::sync::Condvar'
    'thread::sleep'
    'Instant::now'
    'SystemTime::now'
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
