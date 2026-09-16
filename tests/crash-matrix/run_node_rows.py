#!/usr/bin/env python3
"""Execute node crash rows and reconcile receipts from their assertion bodies."""

import argparse
from collections import Counter, defaultdict
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import sys
import tomllib
import uuid

ROOT = Path(__file__).resolve().parents[2]
MATRICES = ("m2.toml", "m4.toml", "m45.toml")
PASSED = re.compile(r"^test result: ok\. 1 passed; 0 failed; 0 ignored;", re.MULTILINE)


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def run(command, *, env=None, timeout=180):
    # A timed-out test may own node children. Reap the whole invocation.
    with subprocess.Popen(command, cwd=ROOT, env=env, text=True,
                          stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                          start_new_session=True) as child:
        try:
            stdout, stderr = child.communicate(timeout=timeout)
        except subprocess.TimeoutExpired:
            os.killpg(child.pid, signal.SIGKILL)
            child.communicate()
            raise RuntimeError(f"timed out after {timeout}s: {command}") from None
        if child.returncode:
            raise RuntimeError(f"exit {child.returncode}: {command}\n{stdout}\n{stderr}")
        return stdout, stderr


def load_rows(directory):
    rows = []
    identities = set()
    for name in MATRICES:
        with (directory / name).open("rb") as source:
            definition = tomllib.load(source)
        require(definition["row"], f"{name}: empty matrix")
        require(definition["seeds"] > 0, f"{name}: no seeds")
        for row in definition["row"]:
            if row.get("tier", "memfs") != "node":
                require(name == "m2.toml" and row.get("tier", "memfs") == "memfs",
                        f"unexecuted tier in {name}: {row}")
                continue
            carrier = row["test"].split("::", 2)
            require(not row.get("policies") and not row.get("workloads"),
                    f"node axes must be exercised inside the carrier: {row}")
            require(len(carrier) == 3 and all(carrier), f"exact carrier required: {row}")
            require(all(re.fullmatch(r"[A-Za-z0-9_:-]+", value) for value in carrier), row)
            require(row["point"] and row["expect"], f"empty point/verdict: {row}")
            require(row.get("platform", "all") in ("all", "linux"), row)
            identity = (row["test"], row["point"], row["expect"])
            require(identity not in identities, f"duplicate node row: {identity}")
            identities.add(identity)
            rows.append(row)
    require(rows, "no node rows")
    return rows


def build_carriers(rows, cargo, profile, target_dir):
    packages = sorted({row["test"].split("::", 1)[0] for row in rows})
    metadata, _ = run([cargo, "metadata", "--offline", "--no-deps", "--format-version=1"])
    names = {package["id"]: package["name"] for package in json.loads(metadata)["packages"]}
    require(set(packages) <= set(names.values()), f"unknown carrier packages: {packages}")
    command = [cargo, "test", "--offline", "--no-run", "--tests", "--message-format=json",
               "--profile", profile, "--target-dir", str(target_dir)]
    for package in packages:
        command.extend(["-p", package])
    print(f"node crash matrix: building {', '.join(packages)} ({profile})", flush=True)
    output, _ = run(command, timeout=1200)
    executables = {}
    for line in output.splitlines():
        event = json.loads(line)
        if event.get("reason") != "compiler-artifact" or not event.get("executable"):
            continue
        if event["target"]["kind"] != ["test"]:
            continue
        key = (names[event["package_id"]], event["target"]["name"])
        require(key not in executables, f"ambiguous carrier: {key}")
        executables[key] = event["executable"]
    for row in rows:
        package, target, _ = row["test"].split("::", 2)
        require((package, target) in executables, f"missing carrier target: {row['test']}")
    return executables


def verify_carrier(executable, function, rows):
    token = uuid.uuid4().hex
    environment = os.environ.copy()
    environment["INF_CRASH_MATRIX_RECEIPT"] = token
    output, errors = run([str(executable), "--exact", function, "--nocapture",
                          "--test-threads=1"], env=environment)
    require(PASSED.search(output), f"carrier did not pass exactly one test: {function}\n{output}")
    prefix = f"CRASH_MATRIX_VERIFIED\t{token}\t"
    receipts = Counter()
    for line in errors.splitlines():
        if line.startswith(prefix):
            fields = line[len(prefix):].split("\t")
            require(len(fields) == 2, f"malformed receipt: {line}")
            receipts[tuple(fields)] += 1
    expected = Counter((row["point"], row["expect"]) for row in rows)
    require(receipts == expected,
            f"unproved node rows in {function}: missing={expected - receipts}, "
            f"unexpected={receipts - expected}\n{output}\n{errors}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cargo", default="cargo")
    parser.add_argument("--profile", default="test")
    parser.add_argument("--target-dir", type=Path, default=ROOT / "target")
    args = parser.parse_args()
    rows = load_rows(Path(__file__).resolve().parent)
    groups = defaultdict(list)
    unsupported = 0
    for row in rows:
        if row.get("platform") == "linux" and sys.platform != "linux":
            print(f"UNSUPPORTED on {sys.platform}: Linux row {row['test']} {row['expect']}",
                  flush=True)
            unsupported += 1
            continue
        groups[row["test"]].append(row)
    supported = [row for group in groups.values() for row in group]
    require(supported, "no supported node rows")
    executables = build_carriers(supported, args.cargo, args.profile, args.target_dir)
    for carrier, group in groups.items():
        package, target, function = carrier.split("::", 2)
        verify_carrier(executables[package, target], function, group)
        print(f"VERIFIED {carrier}: {len(group)} row(s)", flush=True)
    print(f"node crash matrix: {len(supported)}/{len(rows)} rows verified, "
          f"{len(groups)} exact tests; {unsupported} unsupported on this host", flush=True)


if __name__ == "__main__":
    try:
        main()
    except (AssertionError, KeyError, ValueError, OSError, RuntimeError) as error:
        sys.exit(f"node crash matrix FAILED: {error}")
