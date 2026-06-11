#!/usr/bin/env bash
# Dependency-DAG law enforcement (M0-S01, master plan §20).
# Fails with a named-edge error on any internal dependency not allowed by
# docs/dep-dag.toml. Dev-dependencies are exempt (tests may cross layers).
set -euo pipefail
cd "$(dirname "$0")/.."

cargo metadata --format-version 1 --no-deps >/tmp/inf-metadata.json

python3 - <<'PY'
import json, sys, tomllib

with open("docs/dep-dag.toml", "rb") as f:
    allowed = tomllib.load(f)["edges"]

meta = json.load(open("/tmp/inf-metadata.json"))
workspace = {p["name"] for p in meta["packages"]}

violations = []
for pkg in meta["packages"]:
    for dep in pkg["dependencies"]:
        if dep["name"] not in workspace:
            continue
        if dep["kind"] == "dev":  # tests may cross layers
            continue
        if dep["name"] not in allowed.get(pkg["name"], []):
            violations.append(f"  {pkg['name']} -> {dep['name']}")

if violations:
    print("FORBIDDEN DEPENDENCY EDGE(S) — not in docs/dep-dag.toml:")
    print("\n".join(violations))
    print("Arrows point down only (master plan §20). Adding an edge needs an ADR.")
    sys.exit(1)

print(f"dep-dag OK: {len(workspace)} workspace crates, all edges allowed")
PY
