#!/usr/bin/env bash
# Every fuzz target compiles in `just check` (ADR-0198 D2). Each fuzz
# manifest is a workspace of its own, outside the root's, so the root's
# clippy and tests compile none of its targets: without this gate a
# production edit that breaks a target, or a dependency a fuzz lock lacks,
# stays silent until a fuzz job builds that target.
#
# The census (scripts/fuzz_targets.py, ADR-0198 D1) defines the targets and
# judges the tree: every manifest is the root, a member, a fuzz manifest or a
# listed probe; every target sits at fuzz_targets/<name>.rs and is named
# once; every seed directory names a target; a target reaches production
# code through its crate (D4). The gate then runs, for each fuzz workspace in
# census order,
#   cargo check --manifest-path <m> --bins --locked --keep-going
#               --message-format json --color never --target-dir target/fuzz-check
# with RUSTFLAGS="-D warnings --cfg fuzzing" in place of the caller's and
# CARGO_ENCODED_RUSTFLAGS unset (cargo ranks it above RUSTFLAGS), on the
# toolchain rust-toolchain.toml pins. cargo-fuzz sets cfg(fuzzing) by
# default, so every arm a fuzz build compiles is compiled here; the root
# workspace compiles the other arm. No build-script knob is set, so
# libfuzzer-sys's build script compiles libFuzzer's C++ runtime: the gate
# needs a C++17 compiler where it runs, as the fuzz jobs do, and a host
# without one is red there. `cargo check` stops before monomorphization and
# linking, so a post-monomorphization error (a const assertion in a generic
# item a target instantiates) or a link error is the fuzz jobs' to see,
# which build each target before running it.
#
# Red: a census row; cargo exiting non-zero on a workspace (a compile error,
# a warning, a stale lock), each compiler error printed under the workspace
# and target that produced it, and cargo's own errors under the workspace
# with their `Caused by:` and `help:` lines (a lock cargo refuses names the
# refresh that adds what the lock lacks); the bins cargo reports compiled
# for a workspace differing from its [[bin]] rows, whatever cargo's status
# (the count is printed for every workspace, green or red); a compiled
# target whose dep-info, rustc's list of the files it read for the crate
# (beside the crate's metadata, deps/<name>-<hash>.d), names a file that is
# neither the target's own nor one the census read for it by #[path], or
# that the gate cannot find or read. A target reaches production code
# through its crate (ADR-0198 D4), so a file compiled into it by a shape the
# census does not read, such as a dependency's macro that calls include!
# from the target's call site, is red here. Every row is judged and printed
# before the gate exits; only an empty census stops it early.
#
# The probe (scripts/fuzz-check-probe, ADR-0198 D2) is compiled first, with
# the same call, on every run: plant_crate must be refused with E0433 naming
# absent_crate in shared/leaf.rs (a production file that names a crate the
# fuzz crate lacks), plant_warning for a denied warning, plant_cfg with
# E0433 naming absent_cfg_crate (an arm only cfg(fuzzing) compiles),
# stale-lock's stale_lock because --locked was passed, and control must
# compile, its dep-info naming shared/ok.rs, the file it includes, as one
# the gate was not told of (the dep-info read sees an included file); any
# other outcome is red, because the gate could not see what it guards. Its
# two workspaces hold exactly those five targets. stale-lock's
# committed lock must still name its package at another version than the
# manifest's: a cargo run without --locked rewrites it, and that red names
# the git command that restores it.
#
# --matrix nightly|smoke judges the census only and prints the fuzz jobs'
# matrix, one row per target in census order ({"include": [{"dir",
# "target", "seeds"}, ...]}); --matrix deny prints PR CI's cargo-deny
# matrix, the root manifest and then every fuzz manifest ({"include":
# [{"manifest"}, ...]}), each a workspace with a lock of its own (ADR-0198
# D6). A red census prints no matrix and exits 1, its rows and a scope error
# on stderr, so stdout holds the whole matrix or nothing, and a matrix is
# never empty: an empty census is a scope error.
#
# Knobs: INF_CHECK_ROOT (a fixture root); INF_FUZZ_CARGO (the cargo binary,
# the self-test's stub); INF_FUZZ_CHECK_PROBE=off skips the probe on a
# fixture root only, and the OK line says so (the matrix mode, which
# compiles nothing, does not read it). Portable bash 3.2 over
# python3 3.11+ (tomllib), as check-dep-dag.sh.
set -euo pipefail
SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
cd "${INF_CHECK_ROOT:-$SCRIPT_DIR/..}"

mode=gate
case "$#:${1:-}:${2:-}" in
    0::) ;;
    2:--matrix:nightly | 2:--matrix:smoke | 2:--matrix:deny) mode=matrix ;;
    *) echo "usage: check-fuzz-targets.sh [--matrix nightly|smoke|deny]" >&2; exit 2 ;;
esac
matrix=${2:-}
probe=${INF_FUZZ_CHECK_PROBE:-on}
[ "$mode" = gate ] || probe=on
case "$probe" in
    on) ;;
    off)
        if [ -z "${INF_CHECK_ROOT:-}" ]; then
            echo "FUZZ-TARGETS SCOPE ERROR: INF_FUZZ_CHECK_PROBE=off is honoured on a fixture root (INF_CHECK_ROOT) only"
            exit 1
        fi ;;
    *) echo "FUZZ-TARGETS SCOPE ERROR: INF_FUZZ_CHECK_PROBE is '$probe', not on or off"; exit 1 ;;
esac

INF_SCRIPT_DIR="$SCRIPT_DIR" INF_FUZZ_MODE="$mode" INF_FUZZ_MATRIX="$matrix" INF_FUZZ_PROBE="$probe" \
    INF_FUZZ_CARGO_BIN="${INF_FUZZ_CARGO:-cargo}" python3 -I -B - <<'PY'
import json
import os
import re
import subprocess
import sys

sys.path.insert(0, os.environ["INF_SCRIPT_DIR"])
import fuzz_targets  # noqa: E402

TOKEN = "FUZZ-TARGETS"
MODE, PROBE, CARGO = (os.environ[k] for k in ("INF_FUZZ_MODE", "INF_FUZZ_PROBE", "INF_FUZZ_CARGO_BIN"))
# The call is the verdict: every flag is one the probe or the self-test reads.
CALL = ["check", "--bins", "--locked", "--keep-going", "--message-format", "json",
        "--color", "never", "--target-dir", "target/fuzz-check"]
RUSTFLAGS = "-D warnings --cfg fuzzing"
PROBE_MAIN = "scripts/fuzz-check-probe/Cargo.toml"
PROBE_LOCK = "scripts/fuzz-check-probe/stale-lock/Cargo.toml"
PROBE_LOCK_FILE = "scripts/fuzz-check-probe/stale-lock/Cargo.lock"
PROBE_CONTROL = "scripts/fuzz-check-probe/fuzz_targets/control.rs"
PROBE_CONTROL_INCLUDE = "scripts/fuzz-check-probe/shared/ok.rs"
PROBE_TARGETS = {PROBE_MAIN: ["plant_crate", "plant_warning", "plant_cfg", "control"],
                 PROBE_LOCK: ["stale_lock"]}
ERROR_LEVELS = ("error", "error: internal compiler error")
# cargo's own error is a block: its `error` line, the `Caused by:` chain, the
# `help:` and `note:` lines, up to the next `error` or `warning` line or a
# status line (a verb right-aligned in cargo's 12 columns). A block over
# CARGO_BLOCK_LINES_MAX lines prints its first and last half and the count
# between: a build script's reason comes last, after its environment dump.
# cargo prints one block a failing unit (a crate or a build script), so a
# workspace's crate graph bounds how many it prints.
CARGO_BLOCK_LINES_MAX = 24
STATUS_LINE = re.compile(r"( *)([A-Z][a-z]+) ")


def cargo_blocks(stderr):
    """cargo's error blocks in its stderr, each a list of lines."""
    blocks, block = [], None
    for line in stderr.split("\n"):
        status = STATUS_LINE.match(line)
        if line.startswith("error"):
            block = [line]
            blocks.append(block)
        elif line.startswith("warning") or (status and len(status[1] + status[2]) == 12):
            block = None
        elif block is not None:
            block.append(line)
    for block in blocks:
        while not block[-1].strip():
            block.pop()
    return blocks

census = fuzz_targets.census(".")
if census.scope_error:
    # A matrix job captures stdout as its matrix, so its reasons go to stderr.
    print(f"{TOKEN} SCOPE ERROR: {census.scope_error}",
          file=sys.stderr if MODE == "matrix" else sys.stdout)
    sys.exit(1)
reds = list(census.errors)
if MODE == "matrix":
    for red in reds:
        print(f"{TOKEN}: {red}", file=sys.stderr)
    if reds:
        sys.exit(1)
    rows = (fuzz_targets.deny_matrix if os.environ["INF_FUZZ_MATRIX"] == "deny"
            else fuzz_targets.matrix)(census)
    print(json.dumps(rows, separators=(",", ":")))
    sys.exit(0)

ENV = {k: v for k, v in os.environ.items() if k != "CARGO_ENCODED_RUSTFLAGS"}
ENV["RUSTFLAGS"] = RUSTFLAGS


class Report:
    """What cargo said about one workspace: its exit status, the bins it
    reports compiled, its compiler errors and its own error blocks."""

    def __init__(self, manifest):
        self.manifest, self.status, self.bins, self.errors, self.cargo_errors = manifest, 0, [], [], []
        self.unread = 0  # stdout lines that are not cargo's JSON
        self.depinfo = {}  # bin -> the dep-info beside its metadata, or None
        own = os.path.realpath(manifest)
        try:
            run = subprocess.run([CARGO, CALL[0], "--manifest-path", manifest, *CALL[1:]], env=ENV,
                                 stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        except OSError as err:
            self.status, self.cargo_errors = -1, [[f"cannot run {CARGO} ({err})"]]
            return
        self.status = run.returncode
        self.cargo_errors = cargo_blocks(run.stderr)
        for line in run.stdout.split("\n"):
            if not line.strip():
                continue
            try:
                msg = json.loads(line)
                reason, target = msg["reason"], msg.get("target", {})
            except (ValueError, KeyError, TypeError, AttributeError):
                self.unread += 1
                self.cargo_errors.append([f"a report line that is not cargo's JSON: {line[:120]}"])
                continue
            if reason == "compiler-artifact":
                if "bin" in target.get("kind", []) and os.path.realpath(msg.get("manifest_path", "")) == own:
                    self.bins.append(target.get("name"))
                    self.depinfo[target.get("name")] = depinfo_path(msg.get("filenames"))
            elif reason == "compiler-message" and msg.get("message", {}).get("level") in ERROR_LEVELS:
                self.errors.append(msg)

    def refused_lock(self):
        """cargo refused the committed lock: it would have to rewrite it."""
        return any("because --locked was passed" in block[0] for block in self.cargo_errors)

    def errors_of(self, name):
        return [m["message"] for m in self.errors if m.get("target", {}).get("name") == name]

    def print_errors(self, refresh_hint=False):
        for m in self.errors:
            target, rendered = m.get("target", {}), m["message"].get("rendered") or m["message"].get("message", "")
            kind = ",".join(target.get("kind", [])) or "?"
            body = "\n".join("    " + l for l in rendered.rstrip("\n").split("\n"))
            print(f"{self.manifest} {kind} {target.get('name')}:\n{body}")
        half = CARGO_BLOCK_LINES_MAX // 2
        for block in self.cargo_errors:
            if len(block) > CARGO_BLOCK_LINES_MAX:
                omitted = f"... {len(block) - 2 * half} lines omitted"
                block = block[:half] + [omitted] + block[-half:]
            print(f"{self.manifest} cargo: {block[0]}")
            for line in block[1:]:
                print(f"    {line}" if line.strip() else "")
        if refresh_hint and self.refused_lock():
            # `cargo update --workspace` adds what a lock lacks and updates a
            # pinned package only when the manifests no longer admit it; a
            # bare `cargo update` would move every pin of the lock.
            lock = os.path.join(os.path.dirname(self.manifest), "Cargo.lock")
            print(f"{self.manifest} fix: cargo update --workspace --offline --manifest-path "
                  f"{self.manifest} adds what the lock lacks and updates a pinned package only "
                  "when the manifests no longer admit it (drop --offline to fetch a crate the "
                  f"registry cache lacks); then commit {lock}")


DEPINFO_TOKEN = re.compile(r"(?:\\.|[^\s\\])+")  # a path, `\ ` escaping a space


def depinfo_path(filenames):
    """rustc writes a crate's dep-info beside its metadata: `deps/lib<n>-<h>.rmeta`
    has `deps/<n>-<h>.d`. None when the artifact names no metadata."""
    for name in filenames if isinstance(filenames, list) else []:
        base = os.path.basename(name) if isinstance(name, str) else ""
        if base.startswith("lib") and base.endswith(".rmeta"):
            return os.path.join(os.path.dirname(name), base[3:-len(".rmeta")] + ".d")
    return None


def unread_files(manifest, depinfo, own, reads):
    """(why the dep-info cannot be read or None, the files it names beyond `own`
    and `reads`), each relative to the tree's root: a relative path in it is the
    manifest directory's, where cargo runs rustc for the fuzz workspace."""
    if depinfo is None:
        return "no dep-info (its artifact names no .rmeta)", set()
    try:
        with open(depinfo, encoding="utf-8") as f:
            text = f.read()
    except (OSError, UnicodeDecodeError) as err:
        return f"no dep-info at {depinfo} ({type(err).__name__})", set()
    home, top, files = os.path.dirname(manifest), os.path.realpath("."), set()
    for line in text.split("\n"):
        _, sep, deps = line.partition(": ")
        if line.startswith("#") or not sep:
            continue  # an env-dep comment, or a file's empty rule
        for token in DEPINFO_TOKEN.findall(deps):
            name = re.sub(r"\\(.)", r"\1", token)
            name = os.path.realpath(name if os.path.isabs(name) else os.path.join(home, name))
            files.add(os.path.relpath(name, top))
    if own not in files:
        return f"its dep-info at {depinfo} does not name {own}, its own file", set()
    return None, files - {own} - set(reads)


def e0433(message, crate, file_suffix=None):
    code = (message.get("code") or {}).get("code")
    if code != "E0433" or f"`{crate}`" not in message.get("message", ""):
        return False
    if file_suffix is None:
        return True
    primary = [s for s in message.get("spans", []) if s.get("is_primary")]
    return any(os.path.normpath(s.get("file_name", "")).endswith(file_suffix) for s in primary)


def denied_warning(message):
    return (message.get("code") or {}).get("code") == "unused_imports"


def refused(report, name, test, label):
    """`name` was not compiled and each of its errors is the planted one."""
    errors = report.errors_of(name)
    if name in report.bins:
        reds.append(f"{label} probe target {name} compiled: the gate cannot see its plant")
        return False
    if not errors or not all(test(m) for m in errors):
        codes = ", ".join(str((m.get("code") or {}).get("code")) for m in errors) or "none"
        reds.append(f"{label} probe target {name} refused without its planted error "
                    f"({len(errors)} errors, codes: {codes})")
        return False
    return True


def lock_rewritten():
    """Why the stale-lock probe's committed lock plants nothing, or None. It
    must name its package at a version the manifest does not; cargo rewrites
    it to the manifest's version on any run without --locked (an editor's
    `cargo metadata` among them), and then --locked has nothing to refuse."""
    try:
        package = fuzz_targets.read_manifest(PROBE_LOCK)["package"]
        name, version = package["name"], package["version"]
        locked = [p.get("version") for p in fuzz_targets.read_manifest(PROBE_LOCK_FILE)["package"]
                  if p.get("name") == name]
    except (fuzz_targets.CensusError, KeyError, TypeError, AttributeError) as err:
        return f"cannot be read against its manifest ({type(err).__name__}: {err})"
    if not locked:
        return f"names no package {name}"
    if version in locked:
        return f"names {name} at {version}, the version its manifest says"
    return None


probe = "skipped (fixture mode, INF_FUZZ_CHECK_PROBE=off)"
if PROBE == "on":
    verdicts = {}
    for manifest, want in PROBE_TARGETS.items():
        try:
            rows = [n for n, _ in fuzz_targets.bin_rows(manifest)]
        except fuzz_targets.CensusError as err:
            rows = [f"unreadable: {err}"]
        if rows != want:
            reds.append(f"P {manifest} holds targets {rows}, exactly {want} expected: an edited-down "
                        "probe proves less")
    main = Report(PROBE_MAIN)
    if refused(main, "plant_crate", lambda m: e0433(m, "absent_crate", "shared/leaf.rs"), "P1"):
        verdicts["P1"] = "plant_crate E0433 absent_crate"
    if refused(main, "plant_warning", denied_warning, "P2"):
        verdicts["P2"] = "plant_warning denied"
    if "control" in main.bins and not main.errors_of("control"):
        # The census never reads the probe, so shared/ok.rs, which control
        # includes by #[path], is a file the gate was not told of: the dep-info
        # read must report it, exactly, on the real toolchain.
        why, extra = unread_files(PROBE_MAIN, main.depinfo.get("control"),
                                  PROBE_CONTROL, ())
        if why is not None:
            reds.append(f"P3 probe target control: {why}: the gate cannot read the files rustc "
                        "compiles")
        elif extra != {PROBE_CONTROL_INCLUDE}:
            reds.append(f"P3 probe target control's dep-info names {sorted(extra) or 'no file'} "
                        f"beyond its own, not exactly {PROBE_CONTROL_INCLUDE}, the file it "
                        "includes: the gate cannot read the files rustc compiles")
        else:
            verdicts["P3"] = "control compiled naming shared/ok.rs"
    else:
        reds.append("P3 probe target control not compiled: the probe proves nothing")
    rewritten = lock_rewritten()
    if rewritten:
        reds.append(f"P4 {PROBE_LOCK_FILE} {rewritten}, so it plants no stale lock: a cargo "
                    "run without --locked rewrote it; restore it with "
                    f"`git checkout -- {PROBE_LOCK_FILE}`")
    else:
        lock = Report(PROBE_LOCK)
        if lock.status != 0 and "stale_lock" not in lock.bins and lock.refused_lock():
            verdicts["P4"] = "stale_lock refused by --locked"
        else:
            reds.append("P4 probe target stale_lock compiled, or refused without cargo's "
                        "\"because --locked was passed\": the gate does not pass --locked")
            lock.print_errors()
    if refused(main, "plant_cfg", lambda m: e0433(m, "absent_cfg_crate"), "P5"):
        verdicts["P5"] = "plant_cfg E0433 absent_cfg_crate"
    # The main probe's errors are its planted ones: printed only when one of
    # its own verdicts failed.
    if any(k not in verdicts for k in ("P1", "P2", "P3", "P5")):
        main.print_errors()
    held = [verdicts[k] for k in ("P1", "P2", "P5", "P3", "P4") if k in verdicts]
    probe = ", ".join(held) if len(held) == 5 else f"{len(held)} of 5 verdicts held"

compiled = total = depinfo_read = 0
targets = {(t.workspace, t.name): t for t in census.targets}
for manifest in census.workspaces:
    report = Report(manifest)
    rows = [n for n, _ in census.rows[manifest] if n is not None]
    have = set(report.bins)
    missing = [n for n in rows if n not in have]
    unexpected = sorted(n for n in have if n not in rows)
    compiled, total = compiled + len(rows) - len(missing), total + len(rows)
    line = f"fuzz-targets: {manifest}: compiled {len(rows) - len(missing)} of {len(rows)}"
    line += f" (missing: {', '.join(missing)})" if missing else ""
    line += f" (unexpected: {', '.join(unexpected)})" if unexpected else ""
    print(line)
    if report.status != 0 or report.unread:
        reds.append(f"K1 {manifest}: cargo exited {report.status}"
                    + (f", {report.unread} report lines unread" if report.unread else ""))
        report.print_errors(refresh_hint=True)
    if missing or unexpected:
        reds.append(f"K2 {manifest}: the bins cargo compiled are not its [[bin]] rows")
    for name in (n for n in rows if n in have and (manifest, n) in targets):
        target = targets[(manifest, name)]  # a row C5 refused is no target: not read here
        why, extra = unread_files(manifest, report.depinfo.get(name), target.path, target.reads)
        if why is not None:
            reds.append(f"K3 {manifest} {name}: {why}: the gate cannot read the files rustc "
                        "compiled into it")
            continue
        depinfo_read += 1
        for file in sorted(extra):
            reds.append(f"K3 {manifest} {name}: rustc compiled {file} into it, a file the gate "
                        "did not read: a target reaches production code through its crate, and "
                        "a file reached by a shape the census does not read (a dependency's "
                        "macro calling include!, a module the scan missed) is red (ADR-0198 D4)")

rows_n = sum(len(r) for r in census.rows.values())
scope = (f"{len(census.workspaces)} fuzz workspaces, {compiled} of {total} targets compiled "
         f"({rows_n} [[bin]] rows, {census.target_files} files), warnings denied, locks locked, "
         f"cfg(fuzzing) set; probe: {probe}; {len(census.probes)} probe manifests"
         f"{'' if any(r.startswith('C4 ') for r in reds) else ', each run by its gate'}; "
         f"{len(census.seed_dirs)} seed directories (git-listed); {census.path_sites} #[path] "
         f"sites, {len(census.path_files)} files read")
depinfo = f"dep-info of {depinfo_read} of {compiled} compiled targets read"
if reds:
    for red in reds:
        print(f"{TOKEN}: {red}")
    print(f"fuzz-targets FAILED: {len(reds)} red row(s) ({scope}; {depinfo})")
    sys.exit(1)
print(f"fuzz-targets OK: {scope}, none naming crate:: or super:: or aliasing either, declaring a "
      "module by file or calling include!; no target calls include!, and no including target defines macro_rules! "
      f"or carries #[macro_use]; {depinfo}, each naming only its target's file and the #[path] "
      "files read")
PY
