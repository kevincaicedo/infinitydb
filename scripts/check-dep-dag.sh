#!/usr/bin/env bash
# Dependency-DAG law enforcement (M0-S01, master plan §20).
# Fails with a named-edge error on any internal dependency not allowed by
# docs/dep-dag.toml. Dev-dependencies are exempt (tests may cross layers).
set -euo pipefail
cd "${INF_CHECK_ROOT:-$(dirname "$0")/..}"

python3 - <<'PY'
import json
import subprocess
import sys
import tomllib

with open("docs/dep-dag.toml", "rb") as f:
    allowed = tomllib.load(f)["edges"]

result = subprocess.run(
    ["cargo", "metadata", "--format-version", "1", "--no-deps"],
    stdout=subprocess.PIPE, text=True,
)
if result.returncode:
    sys.exit(f"dep-dag SCOPE ERROR: cargo metadata exited {result.returncode}")
try:
    meta = json.loads(result.stdout)
except json.JSONDecodeError as err:
    sys.exit(f"dep-dag SCOPE ERROR: cargo metadata is not JSON ({err})")
workspace = {p["name"] for p in meta["packages"]}
if not workspace:
    sys.exit("dep-dag SCOPE ERROR: no workspace crates")

violations = []
checked = exempt = 0
for pkg in meta["packages"]:
    for dep in pkg["dependencies"]:
        if dep["name"] not in workspace:
            continue
        if dep["kind"] == "dev":  # tests may cross layers
            exempt += 1
            print(f"dep-dag dev-edge exempt: {pkg['name']} -> {dep['name']} (tests may cross layers)")
            continue
        checked += 1
        if dep["name"] not in allowed.get(pkg["name"], []):
            violations.append(f"  {pkg['name']} -> {dep['name']}")

print(f"dep-dag scope: {len(workspace)} workspace crates, {checked} checked edges, {exempt} dev edges exempt")
if violations:
    print("FORBIDDEN DEPENDENCY EDGE(S) — not in docs/dep-dag.toml:")
    print("\n".join(violations))
    print("Arrows point down only (master plan §20). Adding an edge needs an ADR.")
    sys.exit(1)

print(f"dep-dag OK: {len(workspace)} workspace crates, all edges allowed")
PY
