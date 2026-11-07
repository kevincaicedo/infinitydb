#!/usr/bin/env bash
# One checkout, one target root (ADR-0198 A2). Cargo judges a unit fresh by
# its sources' modification times against its outputs. It keys a unit whose
# closure has no path dependency outside its own workspace alike in every
# checkout, and it names a file included from outside the package by the
# absolute path of the checkout that wrote the unit. So a target directory
# that two checkouts write, or that a copied checkout carries, serves one
# checkout the other's verdicts: code that does not compile reads as
# compiled. This script is the one place that maps a checkout to the
# directory its builds may use.
#
#   key   16 hex digits of SHA-256 of the checkout's physical path (n
#         checkouts collide with probability about n^2 / 2^65).
#   root  <checkout>/target when CARGO_TARGET_DIR is unset, empty or names
#         that directory; the variable itself when its last component is
#         checkout-<key> with this checkout's key; else
#         $CARGO_TARGET_DIR/checkout-<key>, so checkouts that share one
#         variable each build in a root of their own. A relative variable is
#         read from the checkout's root.
#   kept  <root>/<name>-<key> for each name of TARGET_ROOT_KEPT: the target
#         a gate keeps warm beside the recipe's (its RUSTFLAGS or lint
#         arguments differ). The key makes a copied checkout's gate, run
#         alone on the copied target, build its own instead of reading the
#         original's.
#
# Cargo writes its intermediate outputs to the build directory, and they stay
# under the root only while none is configured: a set CARGO_BUILD_BUILD_DIR is
# refused here (exit 2), so a gate run under one is red.
#
# Sourced (portable bash 3.2) by every gate that keeps a target and every
# script that runs a binary it builds; no other script reads
# CARGO_TARGET_DIR:
#   target_root_setup <checkout>  sets TARGET_ROOT_KEY, TARGET_ROOT_CHECKOUT
#                                 (physical) and TARGET_ROOT; exit status 2
#                                 and the reason on stderr on a refusal
#   target_root_kept <name>       prints <root>/<name>-<key>
#
# Run as `scripts/target-root.sh --print` (the justfile exports what it
# prints as CARGO_TARGET_DIR, so every recipe builds in its checkout's root),
# it first asks cargo where this checkout's builds land: `cargo metadata
# --no-deps --offline --format-version 1` with CARGO_TARGET_DIR set to the
# root must report the root as its target directory and that directory as
# its build directory, or it exits 2 naming both paths and the cause, a
# cargo config file's build-dir; it prints no clean command then. Then it
# reads the root's marker, .checkout-path, the physical path of the checkout
# it was made for:
#   - this checkout: it prints the root;
#   - another (a checkout copied or moved with its target): exit 2;
#   - none, and the root absent or holding only this checkout's kept
#     directories and the marker's rename temporaries: it writes the marker
#     (a temporary, then a rename), then a CACHEDIR.TAG, and prints the root;
#   - none, and any other entry (outputs that cargo or an editor wrote
#     before the first recipe, a copy of a checkout that never ran one, a
#     root from before this rule): exit 2.
# Each refusal prints the one command that removes this root alone and marks
# it again, its paths shell-quoted: one cold build, never another root (a
# bare `cargo clean` under a set CARGO_TARGET_DIR removes every root). Under
# a set CARGO_TARGET_DIR it then lists on stderr each sibling checkout-*
# root whose marker names a checkout that is gone, or that has no marker,
# with the commands that size and remove it; it reads one marker a root,
# walks none and removes none. Gates never read the marker: CI runs them
# alone on a restored target.
#
# Limits: two hosts that share one variable with checkouts at one path share
# a key; a config's build-dir or build.target-dir that a gate run alone, a
# command outside the recipes or a cargo call from another directory reads
# goes unseen here.

TARGET_ROOT_KEPT="lint-ratchet fuzz-check"

# target_root_py <mode> <args…>: the rules above, in one place (python3 -I).
target_root_py() {
    python3 -I - "$@" <<'PY'
import hashlib
import json
import os
import shlex
import subprocess
import sys

MARKER = ".checkout-path"
TEMPORARY = MARKER + ".tmp."
TAG = ("Signature: 8a477f597d28d172789f06886806bc55\n"
       "# This file is a cache directory tag: the directory holds cargo's build outputs.\n"
       "# For information about cache directory tags see https://bford.info/cachedir/\n")
mode, checkout, kept = sys.argv[1], sys.argv[2], sys.argv[3].split()


def refuse(*lines):
    for line in lines:
        print(line, file=sys.stderr)
    sys.exit(2)


def quoted(path):
    return shlex.quote(path)


if "CARGO_BUILD_BUILD_DIR" in os.environ:
    refuse(f"target-root: CARGO_BUILD_BUILD_DIR is set "
           f"({os.environ['CARGO_BUILD_BUILD_DIR']!r}): cargo would write its intermediate "
           "outputs there, outside this checkout's target root, where another checkout can "
           "share them; unset it")
if not os.path.isdir(checkout):
    refuse(f"target-root: the checkout {checkout} is not a directory")
phys = os.path.realpath(checkout)
var = os.environ.get("CARGO_TARGET_DIR", "")
if "\n" in phys or "\n" in var:
    refuse("target-root: a checkout path or CARGO_TARGET_DIR holding a newline is refused")
key = hashlib.sha256(os.fsencode(phys)).hexdigest()[:16]
own = os.path.join(phys, "target")
leaf = "checkout-" + key
if var == "":
    root = own
else:
    named = os.path.realpath(var if os.path.isabs(var) else os.path.join(phys, var))
    if named == os.path.realpath(own):
        root = own
    elif os.path.basename(named) == leaf:
        root = named
    else:
        root = os.path.join(named, leaf)
if mode == "setup":
    print(key)
    print(phys)
    print(root)
    sys.exit(0)

# --print: where cargo says this checkout's builds land.
env = dict(os.environ, CARGO_TARGET_DIR=root)
try:
    run = subprocess.run(["cargo", "metadata", "--no-deps", "--offline", "--format-version", "1"],
                         cwd=phys, env=env, capture_output=True, text=True)
except OSError as err:
    refuse(f"target-root: cannot run cargo metadata in {phys} ({err})")
if run.returncode != 0:
    refuse(f"target-root: cargo metadata exited {run.returncode} in {phys}:",
           *("    " + line for line in run.stderr.strip("\n").split("\n")[-20:]))
try:
    meta = json.loads(run.stdout)
except ValueError:
    meta = None
target = meta.get("target_directory") if isinstance(meta, dict) else None
build = meta.get("build_directory") if isinstance(meta, dict) else None
if not isinstance(target, str) or not isinstance(build, str):
    refuse(f"target-root: cargo metadata in {phys} names no target or build directory as a "
           "path, so where this checkout's builds land is unknown")
if os.path.realpath(target) != os.path.realpath(root):
    refuse(f"target-root: cargo's target directory is {target}, not this checkout's root "
           f"{root}")
if os.path.realpath(build) != os.path.realpath(target):
    refuse(f"target-root: cargo's build directory is {build}, not its target directory "
           f"{target}: a cargo config file's build-dir (read from {phys}, each directory "
           "above it and CARGO_HOME) moves cargo's intermediate outputs out of this "
           "checkout's root, where another checkout can share them; remove that setting")

# The marker.
clean = (f"    cd {quoted(phys)} && cargo clean --target-dir {quoted(root)} && "
         "./scripts/target-root.sh --print")
marker = os.path.join(root, MARKER)
if os.path.lexists(root) and not os.path.isdir(root):
    refuse(f"target-root: the target root {root} is not a directory")
stamp = True
if os.path.isdir(root):
    try:
        with open(marker, encoding="utf-8", errors="surrogateescape") as f:
            writer = f.read()
    except FileNotFoundError:
        writer = None
    except OSError as err:
        refuse(f"target-root: cannot read {marker} ({err})")
    if writer is not None:
        if writer != phys + "\n":
            refuse(f"target-root: {root} holds the outputs of the checkout at "
                   f"{writer.rstrip(chr(10))}, not of this one at {phys}: a checkout copied or "
                   "moved with its target. Cargo would read them as fresh here, so no recipe "
                   "runs until this root is removed and marked again (one cold build):", clean)
        stamp = False
    else:
        allowed = {f"{name}-{key}" for name in kept}
        others = sorted(e for e in os.listdir(root)
                        if e not in allowed and not e.startswith(TEMPORARY))
        if others:
            shown = ", ".join(others[:5]) + (f" and {len(others) - 5} more" if len(others) > 5
                                              else "")
            refuse(f"target-root: {root} holds {shown} but no marker naming this checkout: "
                   "outputs that cargo or an editor wrote before the first recipe, a copy of a "
                   "checkout that never ran one, or a root from before this rule. Cargo would "
                   "read them as fresh, so no recipe runs until this root is removed and "
                   "marked (one cold build):", clean)
if stamp:
    os.makedirs(root, exist_ok=True)
    temporary = os.path.join(root, f"{TEMPORARY}{os.getpid()}")
    with open(temporary, "w", encoding="utf-8", errors="surrogateescape") as f:
        f.write(phys + "\n")
    os.replace(temporary, marker)
tag = os.path.join(root, "CACHEDIR.TAG")
if not os.path.exists(tag):
    with open(tag, "w", encoding="utf-8") as f:
        f.write(TAG)

# The dead roots beside this one, under a shared variable.
if os.path.basename(root) == leaf and root != own:
    parent = os.path.dirname(root)
    for name in sorted(os.listdir(parent)):
        sibling = os.path.join(parent, name)
        if (not name.startswith("checkout-") or name == leaf or os.path.islink(sibling)
                or not os.path.isdir(sibling)):
            continue
        try:
            with open(os.path.join(sibling, MARKER), encoding="utf-8",
                      errors="surrogateescape") as f:
                writer = f.read().rstrip("\n")
        except FileNotFoundError:
            writer = None
        except OSError:
            continue
        if writer is None:
            why = "has no marker (a root a gate run alone wrote, or one from before this rule)"
        elif not os.path.isdir(writer):
            why = f"was made for {writer}, which is gone"
        else:
            continue
        print(f"target-root: the root {sibling} {why}; its size, and its removal:",
              f"    du -sh {quoted(sibling)}", f"    rm -rf {quoted(sibling)}", sep="\n",
              file=sys.stderr)
print(root)
PY
}

target_root_setup() {
    local out
    out=$(target_root_py setup "$1" "$TARGET_ROOT_KEPT") || return 2
    {
        IFS= read -r TARGET_ROOT_KEY
        IFS= read -r TARGET_ROOT_CHECKOUT
        IFS= read -r TARGET_ROOT
    } <<EOF
$out
EOF
}

target_root_kept() {
    local name
    [ -n "${TARGET_ROOT:-}" ] || { echo "target-root: target_root_setup has not run" >&2; return 2; }
    for name in $TARGET_ROOT_KEPT; do
        if [ "$1" = "$name" ]; then
            printf '%s\n' "$TARGET_ROOT/$1-$TARGET_ROOT_KEY"
            return 0
        fi
    done
    echo "target-root: '$1' is not a kept directory of scripts/target-root.sh ($TARGET_ROOT_KEPT)" >&2
    return 2
}

if [ "${BASH_SOURCE[0]}" = "$0" ]; then
    set -euo pipefail
    case "$#:${1:-}" in
        1:--print) ;;
        *) echo "usage: scripts/target-root.sh --print (or source it)" >&2; exit 2 ;;
    esac
    target_root_py print "$(cd "$(dirname "$0")/.." && pwd -P)" "$TARGET_ROOT_KEPT"
fi
