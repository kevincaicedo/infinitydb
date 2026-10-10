"""Judge exact lint/code/path witnesses in the compiler probe (ADR-0144).

usage: judge.py <probe lib.rs> <clippy JSON> <clippy.toml> <rustc JSON> <rustc exit>
                <target arch> <foreign entries, space-separated>

Every plant must report its own lint and, for disallowed APIs, its resolved
path. Unrelated errors, unmarked diagnostics and missing config plants fail.
A plant marked `xN` must draw exactly N distinct spans (columns) on its line:
the container census counts spans, so two containers on one line are two.
A config entry that resolves to nothing is red, except one the caller names
as foreign (scripts/probe-target.sh: under another CI leg's `core::arch`
module), which is disclosed (ADR-0106 D18).
"""
import json
from pathlib import Path
import re
import sys
import tomllib

src, diag = Path(sys.argv[1]), Path(sys.argv[2])
config, unstable_diag = Path(sys.argv[3]), Path(sys.argv[4])
unstable_exit = int(sys.argv[5])
# ADR-0106 D18: the probe's target architecture, and the unresolved entries
# probe-target.sh defers to their own leg; the rule has that one owner.
ARCH, FOREIGN = sys.argv[6], set(sys.argv[7].split())
foreign = set()
# This is an exact census, not a fallback for missing Clippy witnesses.
UNSTABLE = {"std::fs::set_times": ("set_times.rs", "fs_set_times")}
plants, controls, errors, spans_wanted = {}, set(), [], {}
probe_root = src.parent.parent
for file in sorted(src.parent.glob("*.rs")):
    relative = file.relative_to(probe_root).as_posix()
    for n, line in enumerate(file.read_text().splitlines(), 1):
        at = (relative, n)
        marker = re.search(r"// PLANT (\S+)(?: (\S+?))?(?: x([2-9]))?\s*$", line)
        if marker:
            plants[at] = marker.groups()[:2]
            if marker.group(3):
                spans_wanted[at] = int(marker.group(3))
        elif re.search(r"// CONTROL\s*$", line):
            controls.add(at)


def filesystem(path):
    return path.startswith(("std::fs::", "std::path::Path::", "std::os::unix::fs::"))


def container(path):
    return path.startswith("std::collections::")


def slot(path):
    return path.startswith("inf_foundation::bounded::")


cfg = tomllib.loads(config.read_text())
# Clippy's help for an unresolved entry is `allow-invalid = true`, which
# silences it on every architecture: an inert entry would stay green
# (ADR-0106 D7.5).
for key in ("disallowed-methods", "disallowed-types", "await-holding-invalid-types"):
    for row in cfg.get(key, []):
        if isinstance(row, dict) and "allow-invalid" in row:
            errors.append(f"{key} entry {row.get('path')} carries allow-invalid, "
                          "which hides an entry that resolves to nothing")
# (config key, lint, family, its name, the exact count): every entry needs a
# plant naming its path and every plant an entry (ADR-0144 D5, ADR-0163 D2).
# A slot type is a reservation of `inf_foundation::bounded`, held across an
# `await` by its plant (ADR-0144 A2); each type lands with its own plant.
CENSUS = (("disallowed-methods", "disallowed_methods", filesystem, "filesystem", 37),
          ("disallowed-types", "disallowed_types", filesystem, "filesystem", 5),
          ("disallowed-types", "disallowed_types", container, "container", 3),
          ("await-holding-invalid-types", "await_holding_invalid_type", slot, "slot type", 1))
for key, lint, family, label, count in CENSUS:
    paths = [row["path"] for row in cfg.get(key, []) if family(row["path"])]
    if len(paths) != count or len(set(paths)) != len(paths):
        errors.append(f"config needs {count} distinct {label} {key}, found {len(paths)}")
    witnesses = {path for code, path in plants.values()
                 if code == f"clippy::{lint}" and path and family(path)}
    unstable = set(UNSTABLE) if lint == "disallowed_methods" else set()
    for path in sorted(unstable - set(paths)):
        errors.append(f"unstable path {path} has no config entry")
    for path in sorted(set(paths) - witnesses - unstable):
        errors.append(f"config path {path} has no exact-path plant")
    for path in sorted(witnesses - set(paths)):
        errors.append(f"plant path {path} has no config entry")
    if witnesses & unstable:
        errors.append("an unstable call must not be counted as a Clippy plant")

seen, columns, messages = {}, {}, 0
for raw in diag.read_text().splitlines():
    try:
        msg = json.loads(raw)
    except ValueError:
        continue
    if msg.get("reason") != "compiler-message":
        continue
    messages += 1
    d = msg["message"]
    code = (d.get("code") or {}).get("code")
    text = d["message"]
    if "does not refer to a reachable" in text:
        entry = re.match(r"`([^`]+)`", text)
        if entry and entry.group(1) in FOREIGN:
            foreign.add(entry.group(1))
        else:
            errors.append(f"unresolved config entry: {text}")
    if code is None:
        if d["level"] == "error" and not text.startswith(("aborting due to", "could not compile")):
            errors.append(f"unrelated compiler error: {text}")
        continue
    path = (re.search(r"use of a disallowed (?:method|type) `([^`]+)`", text)
            or re.search(r"holding a disallowed type across an await point `([^`]+)`", text))
    for sp in d["spans"]:
        if not sp.get("is_primary"):
            continue
        file = Path(sp["file_name"])
        if file.is_absolute() and file.is_relative_to(probe_root):
            file = file.relative_to(probe_root)
        at = (file.as_posix(), sp["line_start"])
        seen.setdefault(at, set()).add((code, path.group(1) if path else None))
        columns.setdefault((at, code, path.group(1) if path else None), set()).add(sp["column_start"])

for at, (lint_spec, path) in sorted(plants.items()):
    lints = set(lint_spec.split(","))
    drew = seen.get(at, set())
    for lint in sorted(lints):
        if not any(code == lint and (path is None or actual == path) for code, actual in drew):
            errors.append(f"{at}: did NOT draw {lint} {path or ''}; drew {sorted(drew)}")
        elif at in spans_wanted and len(columns.get((at, lint, path), ())) != spans_wanted[at]:
            got = sorted(columns.get((at, lint, path), ()))
            errors.append(f"{at}: wants {spans_wanted[at]} distinct spans of {lint}, drew columns {got}")
    if any(code not in lints or (path is not None and actual != path) for code, actual in drew):
        errors.append(f"{at}: plant drew an unrelated diagnostic: {sorted(drew)}")
for at in sorted(controls):
    if seen.get(at):
        errors.append(f"{at}: control drew {sorted(seen[at])}")
for at, codes in sorted(seen.items()):
    if at not in plants and at not in controls:
        errors.append(f"{at}: unmarked diagnostic {sorted(codes)}")
# nine decoder/enum, 41 stable APIs, two alias/UFCS bypasses, 11 containers,
# one slot across an await, three discarded publishes
if len(plants) < 67:
    errors.append(f"{len(plants)} plants; expected at least 67")
if messages == 0:
    errors.append("clippy produced no diagnostics; the probe did not run")

# The separate rustc call must reject exactly the configured API at its call
# span for its own unstable feature. Other compiler failures are not evidence.
unstable_root = probe_root / "unstable"
expected_files = {name for name, _ in UNSTABLE.values()}
if {p.name for p in unstable_root.glob("*.rs")} != expected_files:
    errors.append("unstable source census must contain only set_times.rs")
unstable_plants, unstable_seen = {}, set()
for path, (name, feature) in UNSTABLE.items():
    file = unstable_root / name
    if not file.is_file():
        errors.append(f"missing unstable probe: {name}")
        continue
    lines = file.read_text().splitlines()
    markers = [(n, line) for n, line in enumerate(lines, 1) if "// UNSTABLE" in line]
    if len(markers) != 1:
        errors.append(f"{name}: expected exactly one unstable-call marker")
        continue
    n, line = markers[0]
    if not line.endswith(f"// UNSTABLE E0658 {feature} {path}") or not re.search(
        rf"\b{re.escape(path)}\s*\(", line.split("//", 1)[0]
    ):
        errors.append(f"{name}:{n}: marker and source must name the configured unstable call")
        continue
    unstable_plants[(file.resolve(), n)] = (path, feature, line)

for raw in unstable_diag.read_text().splitlines():
    try:
        d = json.loads(raw)
    except ValueError:
        errors.append(f"non-JSON unstable compiler output: {raw}")
        continue
    code = (d.get("code") or {}).get("code")
    message, level = d.get("message", ""), d.get("level")
    # rustc's two terminal summaries contain no source diagnostic.
    if code is None and not d.get("spans") and (
        (level == "error" and message == "aborting due to 1 previous error")
        or (level == "failure-note" and message ==
            "For more information about this error, try `rustc --explain E0658`.")
    ):
        continue
    spans = [sp for sp in d.get("spans", []) if sp.get("is_primary")]
    if code != "E0658" or level != "error" or len(spans) != 1:
        errors.append(f"unrelated unstable-probe diagnostic: {code} {message}")
        continue
    sp = spans[0]
    at = (Path(sp["file_name"]).resolve(), sp["line_start"])
    expected = unstable_plants.get(at)
    if expected is None or at in unstable_seen:
        errors.append("unstable diagnostic is outside its call or was reported twice")
        continue
    path, feature, line = expected
    spelled = line[sp["column_start"] - 1:sp["column_end"] - 1]
    if (sp["line_end"] != at[1] or spelled != path
            or message != f"use of unstable library feature `{feature}`"):
        errors.append(f"unstable diagnostic does not identify {path} and {feature}")
        continue
    unstable_seen.add(at)
if unstable_exit != 1 or len(unstable_seen) != len(UNSTABLE):
    errors.append(f"unstable probe: exit {unstable_exit}, {len(unstable_seen)}/1 exact refusals")
disclosed = (f"; {len(foreign)} config entries not resolvable on {ARCH}, enforced on their own "
             f"architecture's leg: {', '.join(sorted(foreign))}" if foreign else "")
if errors:
    for error in errors:
        print(f"LINT-SCOPES violation: {error}")
    if disclosed:
        print(f"lint-scopes probe: {disclosed[2:]}")
    sys.exit(1)
print(f"lint-scopes probe OK: {len(plants)} exact lint/path plants, "
      f"{len(controls)} clean controls; 36 stable filesystem methods, five filesystem types, "
      f"three containers and one slot type across an await covered{disclosed}")
print("lint-scopes unstable probe OK: 1/1 pinned-stable refusal (E0658 fs_set_times); "
      "all 37 filesystem method bans retained")
