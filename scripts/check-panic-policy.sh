#!/usr/bin/env bash
# Panic-policy grep (M2.5-S13, INFINITY_STYLE §Panics): on the durable and
# cell-resident paths, operating conditions return typed errors — a naked
# `.unwrap()` (or `todo!`/`unimplemented!`) is a review reject. `expect()`
# with an invariant justification is an assertion and is allowed; `panic!`/
# `unreachable!` are for violated internal invariants (audited in
# reviews/ + the interfaces-m2 invariant inventory). Backstops review.
set -euo pipefail
cd "$(dirname "$0")/.."

# Durable-path + cell-resident non-test sources (the §8 durability surface
# and the L1/L6 cell path). Extend as the durable surface grows.
FILES=(
    crates/inf-log/src/commit.rs
    crates/inf-log/src/staging.rs
    crates/inf-log/src/segment.rs
    crates/inf-runtime/src/gate.rs
    crates/inf-server/src/durable.rs
    crates/inf-server/src/ckpt.rs
    crates/inf-server/src/recover.rs
    crates/inf-server/src/control.rs
    crates/inf-server/src/plane.rs
)

# Banned on these paths: naked unwrap (unwrap_or* is fine), and the
# unfinished-code macros. `expect(`, `assert`, `panic!`, `unreachable!`
# are intentionally NOT banned (assertions / audited fail-stops).
PATTERN='(\.unwrap\(\)|todo!|unimplemented!)'

fail=0
for f in "${FILES[@]}"; do
    [ -f "$f" ] || continue
    # Strip the test module: everything from the first `#[cfg(test)]`.
    # (Correct for these files — tests are a trailing `mod tests`.)
    body=$(awk '/#\[cfg\(test\)\]/{exit} {print}' "$f")
    if hits=$(printf '%s\n' "$body" | grep -nE "$PATTERN" | grep -vE 'unwrap_or|denylist-allow'); then
        echo "PANIC-POLICY violation in $f (naked unwrap / todo / unimplemented):"
        echo "$hits"
        fail=1
    fi
done

if [ "$fail" -ne 0 ]; then
    echo "Operating conditions return typed errors (§Panics); use expect(\"<invariant>\")"
    echo "for a justified assertion, or a Result for an operating error."
    exit 1
fi
echo "panic-policy grep OK (${#FILES[@]} durable/cell sources, 0 naked unwrap)"
