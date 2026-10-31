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
# and target that produced it; the bins cargo reports compiled for a
# workspace differing from its [[bin]] rows, whatever cargo's status (the
# count is printed for every workspace, green or red). Every row is judged
# and printed before the gate exits; only an empty census stops it early.
#
# The probe (scripts/fuzz-check-probe, ADR-0198 D2) is compiled first, with
# the same call, on every run: plant_crate must be refused with E0433 naming
# absent_crate in shared/leaf.rs (a production file that names a crate the
# fuzz crate lacks), plant_warning for a denied warning, plant_cfg with
# E0433 naming absent_cfg_crate (an arm only cfg(fuzzing) compiles),
# stale-lock's stale_lock because --locked was passed, and control must
# compile; any other outcome is red, because the gate could not see what it
# guards. Its two workspaces hold exactly those five targets.
#
# --matrix nightly|smoke judges the census only and prints the fuzz jobs'
# matrix, one row per target in census order ({"include": [{"dir",
# "target", "seeds"}, ...]}); a red census prints none and exits 1.
#
# Knobs: INF_CHECK_ROOT (a fixture root); INF_FUZZ_CARGO (the cargo binary,
# the self-test's stub); INF_FUZZ_CHECK_PROBE=off skips the probe on a
# fixture root only, and the OK line says so. Portable bash 3.2 over
# python3 3.11+ (tomllib), as check-dep-dag.sh.
set -euo pipefail
SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
cd "${INF_CHECK_ROOT:-$SCRIPT_DIR/..}"

mode=gate
case "$#:${1:-}:${2:-}" in
    0::) ;;
    2:--matrix:nightly | 2:--matrix:smoke) mode=matrix ;;
    *) echo "usage: check-fuzz-targets.sh [--matrix nightly|smoke]" >&2; exit 2 ;;
esac
probe=${INF_FUZZ_CHECK_PROBE:-on}
case "$probe" in
    on) ;;
    off)
        if [ -z "${INF_CHECK_ROOT:-}" ]; then
            echo "FUZZ-TARGETS SCOPE ERROR: INF_FUZZ_CHECK_PROBE=off is honoured on a fixture root (INF_CHECK_ROOT) only"
            exit 1
        fi ;;
    *) echo "FUZZ-TARGETS SCOPE ERROR: INF_FUZZ_CHECK_PROBE is '$probe', not on or off"; exit 1 ;;
esac

INF_SCRIPT_DIR="$SCRIPT_DIR" INF_FUZZ_MODE="$mode" INF_FUZZ_PROBE="$probe" \
    INF_FUZZ_CARGO_BIN="${INF_FUZZ_CARGO:-cargo}" python3 -I -B - <<'PY'
import json
import os
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
PROBE_TARGETS = {PROBE_MAIN: ["plant_crate", "plant_warning", "plant_cfg", "control"],
                 PROBE_LOCK: ["stale_lock"]}
ERROR_LEVELS = ("error", "error: internal compiler error")

census = fuzz_targets.census(".")
if census.scope_error:
    print(f"{TOKEN} SCOPE ERROR: {census.scope_error}")
    sys.exit(1)
reds = list(census.errors)
if MODE == "matrix":
    for red in reds:
        print(f"{TOKEN}: {red}", file=sys.stderr)
    if reds:
        sys.exit(1)
    print(json.dumps(fuzz_targets.matrix(census), separators=(",", ":")))
    sys.exit(0)

ENV = {k: v for k, v in os.environ.items() if k != "CARGO_ENCODED_RUSTFLAGS"}
ENV["RUSTFLAGS"] = RUSTFLAGS


class Report:
    """What cargo said about one workspace: its exit status, the bins it
    reports compiled, its compiler errors and its own `error:` lines."""

    def __init__(self, manifest):
        self.manifest, self.status, self.bins, self.errors, self.cargo_errors = manifest, 0, [], [], []
        self.unread = 0  # stdout lines that are not cargo's JSON
        own = os.path.realpath(manifest)
        try:
            run = subprocess.run([CARGO, CALL[0], "--manifest-path", manifest, *CALL[1:]], env=ENV,
                                 stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        except OSError as err:
            self.status, self.cargo_errors = -1, [f"cannot run {CARGO} ({err})"]
            return
        self.status = run.returncode
        self.cargo_errors = [l for l in run.stderr.split("\n") if l.startswith("error")]
        for line in run.stdout.split("\n"):
            if not line.strip():
                continue
            try:
                msg = json.loads(line)
                reason, target = msg["reason"], msg.get("target", {})
            except (ValueError, KeyError, TypeError, AttributeError):
                self.unread += 1
                self.cargo_errors.append(f"a report line that is not cargo's JSON: {line[:120]}")
                continue
            if reason == "compiler-artifact":
                if "bin" in target.get("kind", []) and os.path.realpath(msg.get("manifest_path", "")) == own:
                    self.bins.append(target.get("name"))
            elif reason == "compiler-message" and msg.get("message", {}).get("level") in ERROR_LEVELS:
                self.errors.append(msg)

    def errors_of(self, name):
        return [m["message"] for m in self.errors if m.get("target", {}).get("name") == name]

    def print_errors(self):
        for m in self.errors:
            target, rendered = m.get("target", {}), m["message"].get("rendered") or m["message"].get("message", "")
            kind = ",".join(target.get("kind", [])) or "?"
            body = "\n".join("    " + l for l in rendered.rstrip("\n").split("\n"))
            print(f"{self.manifest} {kind} {target.get('name')}:\n{body}")
        for line in self.cargo_errors:
            print(f"{self.manifest} cargo: {line}")


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
    before = len(reds)
    main = Report(PROBE_MAIN)
    if refused(main, "plant_crate", lambda m: e0433(m, "absent_crate", "shared/leaf.rs"), "P1"):
        verdicts["P1"] = "plant_crate E0433 absent_crate"
    if refused(main, "plant_warning", denied_warning, "P2"):
        verdicts["P2"] = "plant_warning denied"
    if "control" in main.bins and not main.errors_of("control"):
        verdicts["P3"] = "control compiled"
    else:
        reds.append("P3 probe target control not compiled: the probe proves nothing")
    lock = Report(PROBE_LOCK)
    if (lock.status != 0 and "stale_lock" not in lock.bins
            and any("because --locked was passed" in l for l in lock.cargo_errors)):
        verdicts["P4"] = "stale_lock refused by --locked"
    else:
        reds.append("P4 probe target stale_lock compiled, or refused without cargo's "
                    "\"because --locked was passed\": the gate does not pass --locked")
        lock.print_errors()
    if refused(main, "plant_cfg", lambda m: e0433(m, "absent_cfg_crate"), "P5"):
        verdicts["P5"] = "plant_cfg E0433 absent_cfg_crate"
    if len(reds) > before:
        main.print_errors()
    held = [verdicts[k] for k in ("P1", "P2", "P5", "P3", "P4") if k in verdicts]
    probe = ", ".join(held) if len(held) == 5 else f"{len(held)} of 5 verdicts held"

compiled = total = 0
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
        report.print_errors()
    if missing or unexpected:
        reds.append(f"K2 {manifest}: the bins cargo compiled are not its [[bin]] rows")

rows_n = sum(len(r) for r in census.rows.values())
scope = (f"{len(census.workspaces)} fuzz workspaces, {compiled} of {total} targets compiled "
         f"({rows_n} [[bin]] rows, {census.target_files} files), warnings denied, locks locked, "
         f"cfg(fuzzing) set; probe: {probe}; {len(census.probes)} probe manifests"
         f"{'' if any(r.startswith('C4 ') for r in reds) else ', each run by its gate'}; "
         f"{len(census.seed_dirs)} seed directories (git-listed); {census.path_sites} #[path] "
         f"sites, {len(census.path_files)} files read")
if reds:
    for red in reds:
        print(f"{TOKEN}: {red}")
    print(f"fuzz-targets FAILED: {len(reds)} red row(s) ({scope})")
    sys.exit(1)
print(f"fuzz-targets OK: {scope}, none naming crate:: or super:: or aliasing either, declaring a "
      "module by file or calling include!; no target calls include!, and no including target defines macro_rules! "
      "or carries #[macro_use]")
PY
