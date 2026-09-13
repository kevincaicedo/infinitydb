#!/usr/bin/env bash
# Batch 44 (review 2026-08-30, F-L17-09; ADR-0121): the §17.3 posture is
# mechanical. Every crate root (`src/lib.rs`, `src/main.rs`, `src/bin/*.rs`)
# under crates/, bins/ and tests/ carries `#![forbid(unsafe_code)]` or
# `#![deny(unsafe_code)]`; the set of `deny` roots equals the audited-leaf
# list below (drift either way is red — a leaf that lost its unsafe drops
# off the list by ADR, a new one joins by ADR); and every
# `allow(unsafe_code)` in shipped source is module-scoped: an outer
# `#[allow(unsafe_code)]` on a `mod` item of a `deny` crate's root, or an
# inner `#![allow(unsafe_code)]` at the top of a module file the root
# declares — never on a function, impl or block, never in a `forbid`
# crate. The compiler then refuses new unsafe outside a named module, and
# `check-safety-inventory.sh` refuses a named module SAFETY.md omits.
#
# Scope is asserted (ADR-0106): missing directories, zero roots, or a
# crate directory without a root are failures, never skips.

set -euo pipefail
cd "${INF_CHECK_ROOT:-$(dirname "$0")/..}"

# The §17.3 exception list as amended by ADR-0121 — crate name → the
# roots that carry `deny`. Overridable for the self-test's fixtures.
LEAVES="${INF_UNSAFE_LEAVES-inf-simd inf-alloc inf-fabric inf-runtime inf-doc inf-server inf-probe inf-sim}"

INF_UNSAFE_LEAVES="$LEAVES" python3 - <<'PY'
import os, re, sys
from pathlib import Path

leaves = set(os.environ["INF_UNSAFE_LEAVES"].split())
attr = re.compile(r"^\s*#!\[(forbid|deny)\(unsafe_code\)\]\s*$", re.M)
inner_allow = re.compile(r"^\s*#!\[allow\(unsafe_code\)\]", re.M)
any_allow = re.compile(r"allow\(unsafe_code\)")
outer_allow_line = re.compile(r"^\s*#\[allow\(unsafe_code\)\]\s*$")
cfg_line = re.compile(r"^\s*#\[cfg\(.*\)\]\s*$")
mod_line = re.compile(r"^\s*(pub(\(\w+\))?\s+)?mod\s+(\w+)\s*[;{]")

errors, roots, denies, allows = [], 0, {}, 0
for top in [Path("crates"), Path("bins"), Path("tests")]:
    if not top.is_dir():
        errors.append(f"UNSAFE ROOTS SCOPE: missing {top}")
        continue
    for crate in sorted(top.iterdir()):
        src = crate / "src"
        if not crate.is_dir() or not (crate / "Cargo.toml").is_file():
            continue  # not a workspace member (a fixture's excluded dir, a scratch dir)
        if not src.is_dir():
            errors.append(f"UNSAFE ROOTS SCOPE: {crate} has a manifest and no src/")
            continue
        crate_roots = [p for p in [src / "lib.rs", src / "main.rs"] if p.is_file()]
        crate_roots += sorted((src / "bin").glob("*.rs")) if (src / "bin").is_dir() else []
        if not crate_roots:
            errors.append(f"UNSAFE ROOTS SCOPE: {crate} has a manifest and no crate root")
            continue
        levels = set()
        for root in crate_roots:
            roots += 1
            text = root.read_text()
            found = attr.findall(text)
            if not found:
                errors.append(f"UNSAFE ROOT UNGOVERNED: {root} carries neither forbid nor deny(unsafe_code)")
                continue
            levels.add(found[0])
            if found[0] == "forbid" and any_allow.search(text):
                errors.append(f"UNSAFE ALLOW UNDER FORBID: {root}")
        if "deny" in levels:
            denies[crate.name] = [r.as_posix() for r in crate_roots]
        # Module-scoped allows only, and only in deny crates.
        declared = set()
        for root in crate_roots:
            lines = root.read_text().split("\n")
            for i, line in enumerate(lines):
                if not outer_allow_line.match(line):
                    if any_allow.search(line) and not line.lstrip().startswith("//"):
                        errors.append(f"UNSAFE ALLOW NOT ON A MOD ITEM: {root}:{i + 1}")
                    continue
                j = i + 1
                while j < len(lines) and (cfg_line.match(lines[j]) or outer_allow_line.match(lines[j])):
                    j += 1
                m = mod_line.match(lines[j]) if j < len(lines) else None
                if not m:
                    errors.append(f"UNSAFE ALLOW NOT ON A MOD ITEM: {root}:{i + 1}")
                    continue
                allows += 1
                declared.add(m.group(3))
                if crate.name not in leaves:
                    errors.append(f"UNSAFE ALLOW OUTSIDE THE LEAF LIST: {root}:{i + 1} (crate {crate.name})")
        for file in sorted(src.rglob("*.rs")):
            if file in crate_roots:
                continue
            lines = file.read_text().split("\n")
            for i, line in enumerate(lines):
                if not any_allow.search(line) or line.lstrip().startswith("//"):
                    continue
                if inner_allow.match(line) and i < 40:
                    allows += 1  # the log_bytes.rs shape: a whole-file allow at the top
                elif outer_allow_line.match(line):
                    j = i + 1
                    while j < len(lines) and (cfg_line.match(lines[j]) or outer_allow_line.match(lines[j])):
                        j += 1
                    if j < len(lines) and mod_line.match(lines[j]):
                        allows += 1  # an inline `mod imp { .. }` per target arch
                    else:
                        errors.append(f"UNSAFE ALLOW NOT ON A MOD ITEM: {file}:{i + 1}")
                        continue
                else:
                    errors.append(f"UNSAFE ALLOW NOT ON A MOD ITEM: {file}:{i + 1}")
                    continue
                if crate.name not in leaves:
                    errors.append(f"UNSAFE ALLOW OUTSIDE THE LEAF LIST: {file}:{i + 1} (crate {crate.name})")

for name in sorted(leaves - set(denies)):
    errors.append(f"UNSAFE LEAF LIST STALE: {name} is listed but carries no deny root (a forbid crate leaves the list by ADR)")
for name in sorted(set(denies) - leaves):
    errors.append(f"UNSAFE DENY OUTSIDE THE LEAF LIST: {name} ({', '.join(denies[name])}) — join the list by ADR or drop the unsafe")
if roots == 0:
    errors.append("UNSAFE ROOTS SCOPE: no crate roots found")
scope = f"{roots} crate roots, {len(denies)} deny crates ({', '.join(sorted(denies)) or 'none'}), {allows} module-scoped allows"
if errors:
    print("\n".join(errors))
    print(f"unsafe-roots FAILED: {scope}")
    sys.exit(1)
print(f"unsafe-roots OK: {scope}; every root governed, every allow a named module of a listed leaf")
PY
