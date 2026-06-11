#!/usr/bin/env bash
# M0-S06 AC: no atomic instructions in the executor waker path (ADR-0003).
#
# Emits release asm for inf-runtime and scans the waker vtable functions
# (clone/wake/wake_by_ref/drop) for atomic mnemonics. Covers aarch64
# (ldxr/ldaxr/stlxr/ldadd/cas/swp/dmb) and x86-64 (`lock` prefix,
# xchg/cmpxchg, mfence). Exits non-zero if any waker symbol contains one.
set -euo pipefail
cd "$(dirname "$0")/.."

ASM_DIR=$(mktemp -d)
trap 'rm -rf "$ASM_DIR"' EXIT

RUSTFLAGS="--emit asm=$ASM_DIR/inf_runtime.s" \
    cargo rustc -p inf-runtime --release --lib >/dev/null 2>&1

ASM_FILE="$ASM_DIR/inf_runtime.s"
[ -s "$ASM_FILE" ] || { echo "no asm emitted at $ASM_FILE"; exit 2; }

# Extract each waker function body (mangled symbols contain "waker_").
awk '
    /^[_A-Za-z0-9$.]*waker_[a-z_]*[^:]*:/ { inside = 1; name = $0 }
    inside && /^[ \t]*(ldxr|ldaxr|ldxp|ldaxp|stxr|stlxr|ldadd|ldclr|ldeor|ldset|swp[ab]?|cas[ab]?|dmb|dsb|lock |xchg|cmpxchg|mfence)/ {
        print "ATOMIC in " name ": " $0; found = 1
    }
    inside && /^\.?L?[_A-Za-z0-9$.]+:/ && !/waker_/ && /^[_A-Za-z0-9$.]+:/ { inside = 0 }
    END { exit found ? 1 : 0 }
' "$ASM_FILE" && FOUND=0 || FOUND=1

WAKERS=$(grep -c 'waker_[a-z_]*' "$ASM_FILE" || true)
if [ "$WAKERS" -eq 0 ]; then
    echo "waker symbols not found in asm — check symbol mangling"; exit 2
fi

if [ "$FOUND" -ne 0 ]; then
    echo "FAIL: atomic instructions found in the waker path (L1/ADR-0003)."
    exit 1
fi
echo "waker-atomics OK: $WAKERS waker symbol references, zero atomic instructions"
