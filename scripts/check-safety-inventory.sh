#!/usr/bin/env bash
# M2.5-S16: SAFETY.md inventory vs code (§17.3, INFINITY_STYLE §Unsafe Rust).
#
# One direction is load-bearing and enforced hard: every file under a
# crate's src/ that USES unsafe (blocks, fns, impls, extern) must be named
# in that crate's SAFETY.md — new unsafe in an unnamed module fails the
# build until the inventory covers it. The complementary direction (stale
# inventory entries) is a docs nit, not a soundness hole, and stays a
# review concern.
#
# Scope: shipped surface only (crates/*/src, bins/*/src). Test/bench
# unsafe lives outside src/ and is covered by clippy's
# undocumented_unsafe_blocks = deny (workspace-wide). The grep matches
# unsafe *usage* tokens, not the word in prose or `forbid(unsafe_code)`.

set -euo pipefail
cd "$(dirname "$0")/.."

fail=0
pattern='\bunsafe[[:space:]]+(\{|fn\b|impl\b|extern\b)'

for crate_dir in crates/* bins/*; do
    [ -d "$crate_dir/src" ] || continue
    inventory="$crate_dir/SAFETY.md"
    # Files whose unsafe usage survives comment stripping would need a real
    # parser; the token match over-approximates (prose mentioning `unsafe fn`
    # forces an inventory line) — over-approximation errs safe.
    mapfile -t unsafe_files < <(grep -rlE "$pattern" "$crate_dir/src" 2>/dev/null || true)
    if [ "${#unsafe_files[@]}" -eq 0 ]; then
        continue
    fi
    if [ ! -f "$inventory" ]; then
        echo "SAFETY INVENTORY MISSING: $crate_dir has unsafe in src/ but no SAFETY.md:"
        printf '  %s\n' "${unsafe_files[@]}"
        fail=1
        continue
    fi
    for file in "${unsafe_files[@]}"; do
        name=$(basename "$file")
        if ! grep -q "$name" "$inventory"; then
            echo "SAFETY INVENTORY GAP: $file uses unsafe but $inventory never names $name"
            fail=1
        fi
    done
done

if [ "$fail" -ne 0 ]; then
    echo "safety-inventory check FAILED — inventory the unsafe (SAFETY.md + // SAFETY: + Miri/Loom) or make it safe"
    exit 1
fi
echo "safety-inventory OK: every unsafe-bearing src file is named in its crate's SAFETY.md"
