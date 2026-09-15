#!/usr/bin/env bash
# Dependency-DAG law enforcement (M0-S01, master plan §20).
# ADR-0106 D16: every permission is active or explicitly reserved.
# Dev-dependencies are exempt (tests may cross layers).
set -euo pipefail
cd "${INF_CHECK_ROOT:-$(dirname "$0")/..}"

python3 - <<'PY'
import json
import re
import subprocess
import sys
import tomllib

def scope_error(message):
    sys.exit(f"dep-dag SCOPE ERROR: {message}")


def names(value):
    return isinstance(value, list) and all(isinstance(v, str) and v for v in value)


try:
    with open("docs/dep-dag.toml", "rb") as f:
        policy = tomllib.load(f)
except (OSError, tomllib.TOMLDecodeError) as err:
    scope_error(f"cannot read docs/dep-dag.toml ({err})")
if set(policy) - {"edges", "unused", "zero-dependency"}:
    scope_error("unknown policy section")
allowed = policy.get("edges")
unused = policy.get("unused", {})
zero = policy.get("zero-dependency", [])
if not isinstance(allowed, dict) or not isinstance(unused, dict) or not names(zero):
    scope_error("edges/unused must be tables; zero-dependency must be a string array")
if any(not names(targets) or len(targets) != len(set(targets)) for targets in allowed.values()):
    scope_error("each edge row must be an array of unique package names")
if any(not isinstance(targets, dict) for targets in unused.values()):
    scope_error("each unused row must map targets to reasons")

try:
    result = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--no-deps"],
        stdout=subprocess.PIPE, text=True,
    )
except OSError as err:
    scope_error(f"cannot run cargo metadata ({err})")
if result.returncode:
    scope_error(f"cargo metadata exited {result.returncode}")
try:
    meta = json.loads(result.stdout)
    packages = meta["packages"]
    members = meta["workspace_members"]
    if not isinstance(packages, list) or not packages or not names(members):
        raise ValueError("no workspace crates or malformed package/member list")
    if len(members) != len(set(members)) or {p["id"] for p in packages} != set(members):
        raise ValueError("workspace members do not match package metadata")
    workspace = {p["name"] for p in packages}
    if len(workspace) != len(packages) or not all(isinstance(p, str) and p for p in workspace):
        raise ValueError("duplicate or invalid package names")
    for pkg in packages:
        if not isinstance(pkg["dependencies"], list):
            raise ValueError("dependencies must be an array")
        for dep in pkg["dependencies"]:
            if (not isinstance(dep["name"], str) or not dep["name"]
                    or dep["kind"] not in (None, "build", "dev")):
                raise ValueError("invalid dependency name or kind")
except (ValueError, KeyError, TypeError) as err:
    scope_error(f"invalid cargo metadata ({err})")

violations = []
for name in sorted(workspace - allowed.keys()):
    violations.append(f"MISSING PACKAGE ROW: {name}")
for name in sorted(allowed.keys() - workspace):
    violations.append(f"UNKNOWN PACKAGE ROW: {name}")
for name in sorted(unused.keys() - workspace):
    violations.append(f"UNKNOWN RESERVATION PACKAGE: {name}")
if len(zero) != len(set(zero)) or set(zero) - workspace:
    violations.append("INVALID ZERO-DEPENDENCY POLICY: duplicate or unknown package")
permissions = {(source, target) for source, targets in allowed.items() for target in targets}
reservations = {
    (source, target): reason
    for source, targets in unused.items() for target, reason in targets.items()
}
for source, target in sorted(permissions):
    if target not in workspace:
        violations.append(f"UNKNOWN EDGE TARGET: {source} -> {target}")
for edge, reason in sorted(reservations.items()):
    source, target = edge
    if edge not in permissions or source not in workspace or target not in workspace:
        violations.append(f"INVALID RESERVATION: {source} -> {target} is not an allowed workspace edge")
    if (not isinstance(reason, str) or not re.search(r"\bADR-[0-9]{4}\b", reason)
            or not re.search(r"\bM[0-9]+(?:\.[0-9]+)?\b", reason)):
        violations.append(f"INVALID RESERVATION: {source} -> {target} needs an ADR and activation milestone")

actual = set()
checked = exempt = 0
for pkg in packages:
    for dep in pkg["dependencies"]:
        source, target = pkg["name"], dep["name"]
        if source in zero:
            kind = dep["kind"] or "normal"
            violations.append(f"ZERO-DEPENDENCY VIOLATION: {source} -> {target} ({kind})")
        if dep["name"] not in workspace:
            continue
        if dep["kind"] == "dev":  # tests may cross layers
            exempt += 1
            print(f"dep-dag dev-edge exempt: {pkg['name']} -> {dep['name']} (tests may cross layers)")
            continue
        checked += 1
        actual.add((source, target))

for source, target in sorted(actual - permissions):
    violations.append(f"FORBIDDEN DEPENDENCY EDGE: {source} -> {target}")
for source, target in sorted(permissions - actual - reservations.keys()):
    violations.append(f"UNANNOTATED UNUSED EDGE: {source} -> {target}")
for source, target in sorted(actual & reservations.keys()):
    violations.append(f"ACTIVE RESERVED EDGE: {source} -> {target}; remove the unused annotation")
for (source, target), reason in sorted(reservations.items()):
    print(f"dep-dag reserved edge: {source} -> {target} ({reason})")

scope = (
    f"{len(workspace)} workspace crates, {checked} checked declarations / {len(actual)} active edges, "
    f"{len(permissions)} permissions, {len(reservations)} reserved, {exempt} dev edges exempt"
)
print(f"dep-dag scope: {scope}; zero-dependency packages: {', '.join(zero) or 'none'}")
if violations:
    print("\n".join(violations))
    print("dep-dag FAILED: permissions/reservations must match Cargo; changing an edge needs an ADR.")
    sys.exit(1)

print(f"dep-dag OK: {scope}")
PY
