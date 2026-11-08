#!/usr/bin/env bash
# One checkout, one target root: the checkout's own `target` (ADR-0204 D1).
# Cargo judges a unit fresh by its sources' modification times against its
# outputs. It keys a unit whose closure has no path dependency outside its own
# workspace alike in every checkout, and it names a file included from outside
# the package by the absolute path of the checkout that wrote the unit. So a
# target directory that two checkouts write, or that a copied checkout
# carries, serves one checkout the other's verdicts: code that does not
# compile reads as compiled. And a target triple, or a root moved off
# <checkout>/target, leaves a path into target/<profile> naming an older build
# or none. This script is the one place that decides where a checkout builds.
#
#   key   16 hex digits of SHA-256 of the checkout's physical path (n
#         checkouts collide with probability about n^2 / 2^65).
#   root  <checkout>/target, a directory, by the checkout's physical path.
#         Nothing moves it: whatever cargo reads for its target directory,
#         its build directory or its target triple is refused below, so every
#         spelling of a path into target/<profile> names this root's own build.
#   kept  <root>/<name>-<key> for each name of TARGET_ROOT_KEPT: the target
#         a gate keeps warm beside the recipe's (its RUSTFLAGS or lint
#         arguments differ). The key makes a copied checkout's gate, run
#         alone on the copied target, build its own instead of reading the
#         original's.
#
# Each entry refuses, in this order, with exit 2, nothing written, and on
# stderr what it found and the action that clears it:
#   1. a set CARGO_BUILD_BUILD_DIR or CARGO_BUILD_TARGET (any value), or a
#      cargo config that sets build.target or holds an include, or that
#      cannot be parsed: the config read loads, with python's tomllib,
#      .cargo/config and .cargo/config.toml in the checkout and in each
#      directory above it, and config and config.toml in CARGO_HOME (else
#      ~/.cargo), every file cargo reads for a call from the checkout. It
#      follows no include, and no config unsets a triple;
#   2. a set, non-empty CARGO_TARGET_DIR whose physical path is not the
#      root's (a relative one is read from the checkout): unset it for this
#      checkout (`env -u CARGO_TARGET_DIR just check`); an empty one counts
#      as unset;
#   3. a root that is a link, or that exists and is not a directory: for
#      another volume, clone the checkout there;
#   4. a checkout at or under another checkout's root: a directory named
#      target beside a Cargo.toml, or one holding a .checkout-path, at or
#      above the checkout.
#
# Sourced (portable bash 3.2) by every gate that keeps a target and every
# script that runs a binary it builds:
#   target_root_setup <checkout>  checks 1-4, then sets TARGET_ROOT_KEY,
#                                 TARGET_ROOT_CHECKOUT (physical), TARGET_ROOT
#                                 and an exported CARGO_TARGET_DIR equal to
#                                 the root, each read-only; exit status 2 and
#                                 the reason on stderr on a refusal
#   target_root_kept <name>       prints <root>/<name>-<key>
#   target_root_cargo_fresh <dir> <command> [args…]
#                                 a per-run build, cold: `env
#                                 CARGO_TARGET_DIR=<dir> <command> [args…]`
#                                 when <dir> is absent or an empty directory
#                                 and no argument names --target-dir,
#                                 --config or CARGO_TARGET_DIR (each outranks
#                                 or undoes the directory); else exit 2,
#                                 nothing run. It needs no setup: a probe
#                                 keeps its own --target and prefixes (`env
#                                 -u …`, `env RUSTFLAGS=…`) inside the argv,
#                                 and env, a program, sets the variable
#                                 whatever the calling shell holds read-only.
# Setup runs no cargo, so a script run alone under a cargo config's build-dir
# puts its intermediate outputs there; the export below refuses one.
#
# Run as `scripts/target-root.sh --print` (the justfile exports what it
# prints as CARGO_TARGET_DIR, so every recipe builds in its checkout's root),
# it runs checks 1-4, then asks cargo where this checkout's builds land:
# `cargo metadata --no-deps --offline --format-version 1`, run in the
# checkout with CARGO_TARGET_DIR removed, must report the root as its target
# directory and as its build directory, or it exits 2 naming cargo's
# directory and each config file and variable it read (a config's
# build.target-dir or build-dir, or CARGO_BUILD_TARGET_DIR: remove it, or
# override target-dir in a git-ignored <checkout>/.cargo/config.toml holding
# `[build] target-dir = "target"`). Then it reads the root's marker,
# .checkout-path, the physical path of the checkout it was made for:
#   - this checkout: it prints the root;
#   - another checkout W that exists and whose target is this directory
#     (the same device and inode: a link or a mount): exit 2, naming W;
#   - another (a checkout copied or moved with its target): exit 2;
#   - none, and the root absent or holding only this checkout's kept
#     directories and the marker's rename temporaries: it writes the marker
#     (a temporary, then a rename), then a CACHEDIR.TAG, and prints the root;
#   - none, and any other entry: it walks the root without following links,
#     and a .git (a work tree or a repository) or a .checkout-path (another
#     root) there is named, exit 2; else (outputs that cargo or an editor
#     wrote before the first recipe, a copy of a checkout that never ran
#     one, a root from before this rule) exit 2, naming its first entries.
# A refusal prints at most one command, `cd <checkout> &&
# ./scripts/target-root.sh --clean` (the copy's and the unmarked outputs'),
# and never a path to remove: the tree can change between a print and its
# run. Gates never read the marker: CI runs them alone on a restored target.
#
# Run as `scripts/target-root.sh --clean`, it runs the export's checks, then
# walks the root without following links and refuses, removing nothing, when
# the root or a directory below it holds a .git, or a directory below its top
# holds a .checkout-path, and when its marker names a live checkout whose
# target is this directory. Otherwise it runs `cargo clean --target-dir
# <root>`, then marks the empty root as --print does and prints it: one cold
# build. The checks and the removal run in one process, so nothing changes
# between them, and no command it prints or runs removes another checkout or
# another root.

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
TRIPLE = ("a target triple moves every build to target/<triple>/<profile>, so a path into "
          "target/<profile> would name an older build or none")
mode, checkout, kept = sys.argv[1], sys.argv[2], sys.argv[3].split()


def refuse(*lines):
    for line in lines:
        print(line, file=sys.stderr)
    sys.exit(2)


def quoted(path):
    return shlex.quote(path)


def config_files(phys):
    """Every cargo config file a cargo call from phys reads, nearest first."""
    paths, here = [], phys
    while True:
        paths += [os.path.join(here, ".cargo", "config"),
                  os.path.join(here, ".cargo", "config.toml")]
        parent = os.path.dirname(here)
        if parent == here:
            break
        here = parent
    home = os.environ.get("CARGO_HOME") or os.path.join(os.path.expanduser("~"), ".cargo")
    home = home if os.path.isabs(home) else os.path.join(phys, home)
    paths += [os.path.join(home, "config"), os.path.join(home, "config.toml")]
    seen, files = set(), []
    for path in paths:
        if os.path.isfile(path) and os.path.realpath(path) not in seen:
            seen.add(os.path.realpath(path))
            files.append(path)
    return files


def config_read(phys):
    """Check 1's config half; returns the files read."""
    try:
        import tomllib
    except ImportError:
        refuse("target-root: python3 has no tomllib (3.11 or newer is needed) to read the "
               "cargo config files")
    files = config_files(phys)
    for path in files:
        try:
            with open(path, "rb") as f:
                data = tomllib.loads(f.read().decode("utf-8"))
        except (OSError, UnicodeDecodeError, tomllib.TOMLDecodeError) as err:
            refuse(f"target-root: the cargo config {path} cannot be read or parsed ({err}), so "
                   "whether it moves this checkout's builds is unknown; fix or remove it")
        if "include" in data:
            refuse(f"target-root: the cargo config {path} holds an include, which cargo follows "
                   "and this read does not, so whether it sets build.target is unknown; inline "
                   "the included keys")
        build = data.get("build")
        if isinstance(build, dict) and "target" in build:
            refuse(f"target-root: the cargo config {path} sets build.target "
                   f"({build['target']!r}): {TRIPLE}; remove the key (no config unsets a triple)")
    return files


for name in ("CARGO_BUILD_BUILD_DIR", "CARGO_BUILD_TARGET"):
    if name in os.environ:
        refuse(f"target-root: {name} is set ({os.environ[name]!r}): "
               + ("cargo would write its intermediate outputs there, outside this checkout's "
                  "target, where another checkout can share them" if name.endswith("_DIR")
                  else TRIPLE)
               + "; unset it")
if not os.path.isdir(checkout):
    refuse(f"target-root: the checkout {checkout} is not a directory")
phys = os.path.realpath(checkout)
var = os.environ.get("CARGO_TARGET_DIR", "")
if "\n" in phys or "\n" in var:
    refuse("target-root: a checkout path or CARGO_TARGET_DIR holding a newline is refused")
read = config_read(phys)
root = os.path.join(phys, "target")
if var != "":
    named = os.path.realpath(var if os.path.isabs(var) else os.path.join(phys, var))
    if named != os.path.realpath(root):
        refuse(f"target-root: CARGO_TARGET_DIR is {var!r}, not this checkout's target {root}: "
               "a checkout builds in its own target, and a root two checkouts share, or one "
               "inside another's, serves one checkout the other's outputs or goes when that "
               "root is removed. Unset it for this checkout (env -u CARGO_TARGET_DIR just "
               "check), or set it to this checkout's target; an empty value counts as unset")
if os.path.islink(root):
    refuse(f"target-root: {root} is a link (to {os.readlink(root)}), not a directory: git lists "
           "it as untracked, clippy's walk up from it misses the workspace's clippy.toml, and "
           "two links to one directory share a root. Remove the link; to keep the outputs on "
           "another volume, clone the checkout there")
if os.path.lexists(root) and not os.path.isdir(root):
    refuse(f"target-root: {root} exists and is not a directory; remove it")
here = phys
while True:
    parent = os.path.dirname(here)
    if os.path.basename(here) == "target" and os.path.exists(os.path.join(parent, "Cargo.toml")):
        found = f"{here}, a target directory beside {os.path.join(parent, 'Cargo.toml')}"
    elif os.path.lexists(os.path.join(here, MARKER)):
        found = f"{here}, a target root (it holds {MARKER})"
    else:
        found = None
    if found:
        refuse(f"target-root: the checkout {phys} lies at or under {found}: removing that root "
               "would remove this checkout. Move the checkout out of it (git worktree move, or a "
               "new clone)")
    if parent == here:
        break
    here = parent
key = hashlib.sha256(os.fsencode(phys)).hexdigest()[:16]
if mode == "setup":
    print(key)
    print(phys)
    print(root)
    sys.exit(0)

# --print and --clean: where cargo says this checkout's builds land.
env = {k: v for k, v in os.environ.items() if k != "CARGO_TARGET_DIR"}
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
for what, path in (("target", target), ("build", build)):
    if os.path.realpath(path) != os.path.realpath(root):
        settings = [f"CARGO_BUILD_TARGET_DIR={os.environ['CARGO_BUILD_TARGET_DIR']!r}"
                    if "CARGO_BUILD_TARGET_DIR" in os.environ else None] + read
        refuse(f"target-root: cargo's {what} directory is {path}, not this checkout's target "
               f"{root}: a cargo config's build.target-dir or build-dir, or "
               "CARGO_BUILD_TARGET_DIR, moves it. Read here: "
               + (", ".join(s for s in settings if s) or "no config file and no variable")
               + ". Remove that setting; a target-dir can instead be overridden in a git-ignored "
               f"{os.path.join(phys, '.cargo', 'config.toml')} holding "
               "[build] target-dir = \"target\"")

# The marker, and what a removal of the root would also remove.
CLEAN = f"    cd {quoted(phys)} && ./scripts/target-root.sh --clean"
marker = os.path.join(root, MARKER)


def written_for():
    """The marker's checkout, or None when the root or its marker is absent."""
    if not os.path.isdir(root):
        return None
    try:
        with open(marker, encoding="utf-8", errors="surrogateescape") as f:
            return f.read()
    except FileNotFoundError:
        return None
    except OSError as err:
        refuse(f"target-root: cannot read {marker} ({err})")


def shares_root(writer):
    """The marker names a live checkout whose target is this directory."""
    other = writer.rstrip("\n")
    try:
        mine, theirs = os.stat(root), os.stat(os.path.join(other, "target"))
    except OSError:
        return None
    if os.path.isdir(other) and (mine.st_dev, mine.st_ino) == (theirs.st_dev, theirs.st_ino):
        return other
    return None


def walk_hit():
    """The first .git at or below the root, or .checkout-path below its top."""
    def unreadable(err):
        refuse(f"target-root: cannot read {err.filename} while walking {root} ({err.strerror}), "
               "so what removing the root would remove is unknown")
    if not os.path.isdir(root):
        return None
    for here, dirs, files in os.walk(root, onerror=unreadable):
        for name in dirs + files:
            if name == ".git" or (name == MARKER and here != root):
                return os.path.join(here, name)
    return None


def hit_refusal(hit, after):
    what = ("a work tree or a repository" if os.path.basename(hit) == ".git"
            else "another checkout's target root")
    refuse(f"target-root: {root} holds {hit}, {what}: removing this root would remove it. Move "
           f"it out of the root (a work tree: git worktree move <it> <a directory outside "
           f"{root}>), then {after}")


def sharer_refusal(other):
    refuse(f"target-root: {root} is also the target of the checkout at {other} (its target is a "
           "link or a mount to this directory, the same device and inode): removing it would "
           f"remove that checkout's root. Remove {os.path.join(other, 'target')} (that "
           "checkout's own recipes refuse a linked target), then run the recipe again")


def stamp():
    os.makedirs(root, exist_ok=True)
    temporary = os.path.join(root, f"{TEMPORARY}{os.getpid()}")
    with open(temporary, "w", encoding="utf-8", errors="surrogateescape") as f:
        f.write(phys + "\n")
    os.replace(temporary, marker)


def tag():
    path = os.path.join(root, "CACHEDIR.TAG")
    if not os.path.exists(path):
        with open(path, "w", encoding="utf-8") as f:
            f.write(TAG)


if mode == "clean":
    hit = walk_hit()
    if hit:
        hit_refusal(hit, "run this command again")
    writer = written_for()
    if writer is not None and writer != phys + "\n" and shares_root(writer):
        sharer_refusal(shares_root(writer))
    if os.path.lexists(root):
        run = subprocess.run(["cargo", "clean", "--target-dir", root], cwd=phys,
                             capture_output=True, text=True)
        if run.returncode != 0:
            refuse(f"target-root: cargo clean --target-dir {root} exited {run.returncode}; run "
                   "this command again:",
                   *("    " + line for line in run.stderr.strip("\n").split("\n")[-20:]))
        if os.path.lexists(root):
            refuse(f"target-root: cargo clean --target-dir {root} left the root in place; "
                   "nothing was marked")
    stamp()
    tag()
    print(root)
    sys.exit(0)

writer = written_for()
if writer is not None and writer != phys + "\n":
    if shares_root(writer):
        sharer_refusal(shares_root(writer))
    refuse(f"target-root: {root} holds the outputs of the checkout at "
           f"{writer.rstrip(chr(10))}, not of this one at {phys}: a checkout copied or moved with "
           "its target. Cargo would read them as fresh here, so no recipe runs until this root "
           "is cleaned (one cold build):", CLEAN)
if writer is None:
    if os.path.isdir(root):
        allowed = {f"{name}-{key}" for name in kept}
        others = sorted(e for e in os.listdir(root)
                        if e not in allowed and not e.startswith(TEMPORARY))
        if others:
            hit = walk_hit()
            if hit:
                hit_refusal(hit, "run the recipe again, which prints the command that cleans it")
            shown = ", ".join(others[:5]) + (f" and {len(others) - 5} more" if len(others) > 5
                                              else "")
            refuse(f"target-root: {root} holds {shown} but no marker naming this checkout: "
                   "outputs that cargo or an editor wrote before the first recipe, a copy of a "
                   "checkout that never ran one, or a root from before this rule. Cargo would "
                   "read them as fresh, so no recipe runs until this root is cleaned (one cold "
                   "build):", CLEAN)
    stamp()
tag()
print(root)
PY
}

# target_root_setup <checkout>: checks 1-4, then the root, read-only. A second
# call in one shell keeps the root the first one set: check 2 has just found
# the exported CARGO_TARGET_DIR, that root, naming this checkout's.
target_root_setup() {
    local out key checkout root
    out=$(target_root_py setup "$1" "$TARGET_ROOT_KEPT") || return 2
    {
        IFS= read -r key
        IFS= read -r checkout
        IFS= read -r root
    } <<EOF
$out
EOF
    if ! (TARGET_ROOT=) 2>/dev/null; then
        return 0
    fi
    TARGET_ROOT_KEY=$key
    TARGET_ROOT_CHECKOUT=$checkout
    TARGET_ROOT=$root
    CARGO_TARGET_DIR=$root
    export CARGO_TARGET_DIR
    readonly TARGET_ROOT_KEY TARGET_ROOT_CHECKOUT TARGET_ROOT CARGO_TARGET_DIR
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

target_root_cargo_fresh() {
    local dir=${1:-} arg
    [ "$#" -ge 2 ] && [ -n "$dir" ] ||
        { echo "target-root: usage: target_root_cargo_fresh <dir> <command> [args…]" >&2; return 2; }
    shift
    for arg in "$@"; do
        case "$arg" in
            --target-dir | --target-dir=* | --config | --config=* | *CARGO_TARGET_DIR*)
                echo "target-root: the per-run build of $dir names '$arg', which outranks or undoes" \
                    "its directory; nothing ran" >&2
                return 2 ;;
        esac
    done
    if [ -L "$dir" ] || { [ -e "$dir" ] && [ ! -d "$dir" ]; }; then
        echo "target-root: the per-run directory $dir is not a directory; nothing ran" >&2
        return 2
    fi
    if [ -d "$dir" ] && [ -n "$(ls -A "$dir")" ]; then
        echo "target-root: the per-run directory $dir holds an earlier run's entries, which a" \
            "build would read as fresh; nothing ran" >&2
        return 2
    fi
    env CARGO_TARGET_DIR="$dir" "$@"
}

if [ "${BASH_SOURCE[0]}" = "$0" ]; then
    set -euo pipefail
    case "$#:${1:-}" in
        1:--print) mode=print ;;
        1:--clean) mode=clean ;;
        *) echo "usage: scripts/target-root.sh --print | --clean (or source it)" >&2; exit 2 ;;
    esac
    target_root_py "$mode" "$(cd "$(dirname "$0")/.." && pwd -P)" "$TARGET_ROOT_KEPT"
fi
