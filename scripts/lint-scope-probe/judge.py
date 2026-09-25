"""Judge exact lint/code/path witnesses in the compiler probe (ADR-0144).

usage: judge.py <probe lib.rs> <clippy JSON> <clippy.toml> <rustc JSON> <rustc exit>

Every plant must report its own lint and, for disallowed APIs, its resolved
path. Unrelated errors, unmarked diagnostics and missing config plants fail.
"""
import json
from pathlib import Path
import re
import sys
import tomllib

src, diag = Path(sys.argv[1]), Path(sys.argv[2])
config, unstable_diag = Path(sys.argv[3]), Path(sys.argv[4])
unstable_exit = int(sys.argv[5])
# This is an exact census, not a fallback for missing Clippy witnesses.
UNSTABLE = {"std::fs::set_times": ("set_times.rs", "fs_set_times")}
plants, controls, errors = {}, set(), []
probe_root = src.parent.parent
for file in sorted(src.parent.glob("*.rs")):
    relative = file.relative_to(probe_root).as_posix()
    for n, line in enumerate(file.read_text().splitlines(), 1):
        at = (relative, n)
        marker = re.search(r"// PLANT (\S+)(?: (\S+))?\s*$", line)
        if marker:
            plants[at] = marker.groups()
        elif re.search(r"// CONTROL\s*$", line):
            controls.add(at)


def filesystem(path):
    return path.startswith(("std::fs::", "std::path::Path::", "std::os::unix::fs::"))


cfg = tomllib.loads(config.read_text())
for key, lint, count in (("disallowed-methods", "disallowed_methods", 37),
                         ("disallowed-types", "disallowed_types", 5)):
    paths = [row["path"] for row in cfg[key] if filesystem(row["path"])]
    if len(paths) != count or len(set(paths)) != len(paths):
        errors.append(f"config needs {count} distinct filesystem {key}, found {len(paths)}")
    witnesses = {path for code, path in plants.values() if code == f"clippy::{lint}"}
    unstable = set(UNSTABLE) if lint == "disallowed_methods" else set()
    for path in sorted(unstable - set(paths)):
        errors.append(f"unstable path {path} has no config entry")
    for path in sorted(set(paths) - witnesses - unstable):
        errors.append(f"config path {path} has no exact-path plant")
    for path in sorted(witnesses - set(paths)):
        errors.append(f"plant path {path} has no config entry")
    if witnesses & unstable:
        errors.append("an unstable call must not be counted as a Clippy plant")

seen, messages = {}, 0
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
        errors.append(f"unresolved config entry: {text}")
    if code is None:
        if d["level"] == "error" and not text.startswith(("aborting due to", "could not compile")):
            errors.append(f"unrelated compiler error: {text}")
        continue
    path = re.search(r"use of a disallowed (?:method|type) `([^`]+)`", text)
    for sp in d["spans"]:
        if not sp.get("is_primary"):
            continue
        file = Path(sp["file_name"])
        if file.is_absolute() and file.is_relative_to(probe_root):
            file = file.relative_to(probe_root)
        at = (file.as_posix(), sp["line_start"])
        seen.setdefault(at, set()).add((code, path.group(1) if path else None))

for at, (lint_spec, path) in sorted(plants.items()):
    lints = set(lint_spec.split(","))
    drew = seen.get(at, set())
    for lint in sorted(lints):
        if not any(code == lint and (path is None or actual == path) for code, actual in drew):
            errors.append(f"{at}: did NOT draw {lint} {path or ''}; drew {sorted(drew)}")
    if any(code not in lints or (path is not None and actual != path) for code, actual in drew):
        errors.append(f"{at}: plant drew an unrelated diagnostic: {sorted(drew)}")
for at in sorted(controls):
    if seen.get(at):
        errors.append(f"{at}: control drew {sorted(seen[at])}")
for at, codes in sorted(seen.items()):
    if at not in plants and at not in controls:
        errors.append(f"{at}: unmarked diagnostic {sorted(codes)}")
if len(plants) < 52:  # nine decoder/enum plants, 41 stable APIs, two alias/UFCS bypasses
    errors.append(f"{len(plants)} plants; expected at least 52")
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
if errors:
    for error in errors:
        print(f"LINT-SCOPES violation: {error}")
    sys.exit(1)
print(f"lint-scopes probe OK: {len(plants)} exact lint/path plants, "
      f"{len(controls)} clean controls; 36 stable filesystem methods and five types covered")
print("lint-scopes unstable probe OK: 1/1 pinned-stable refusal (E0658 fs_set_times); "
      "all 37 filesystem method bans retained")
