#!/usr/bin/env python3
"""Inspect real server/generator threads during a non-citable Linux comparison."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import signal
import shutil
import socket
import subprocess
import time


def cpu_ranges():
    available = sorted(os.sched_getaffinity(0))
    pairs = [cpu for cpu in available if cpu + 1 in available]
    for server in pairs:
        for load in pairs:
            if abs(server - load) >= 2:
                return server, load
    raise RuntimeError("smoke needs two disjoint pairs of available logical CPUs")


def port_block():
    for offset in range(100):
        base = 20_000 + ((os.getpid() + offset) % 1_000) * 8
        sockets = []
        try:
            for port in range(base, base + 6):
                connection = socket.socket()
                sockets.append(connection)
                connection.bind(("127.0.0.1", port))
            return base
        except OSError:
            continue
        finally:
            for connection in sockets:
                connection.close()
    raise RuntimeError("no free native smoke port block")


def descendants(root):
    pending = [root]
    seen = set()
    while pending:
        pid = pending.pop()
        if pid in seen:
            continue
        seen.add(pid)
        yield pid
        for task in Path(f"/proc/{pid}/task").glob("*"):
            try:
                pending.extend(map(int, (task / "children").read_text().split()))
            except (FileNotFoundError, ProcessLookupError):
                continue


def observe(root, samples, executables, version_probes):
    for pid in descendants(root):
        try:
            role = executables.get(os.readlink(f"/proc/{pid}/exe"))
        except (FileNotFoundError, ProcessLookupError):
            continue
        if role is None:
            continue
        try:
            command = Path(f"/proc/{pid}/cmdline").read_bytes().split(b"\0")
        except (FileNotFoundError, ProcessLookupError):
            continue
        if b"--version" in command:
            version_probes.add((role, pid))
            continue
        for task in Path(f"/proc/{pid}/task").glob("*"):
            try:
                mask = tuple(sorted(os.sched_getaffinity(int(task.name))))
                name = (task / "comm").read_text().strip()
            except (FileNotFoundError, ProcessLookupError):
                continue
            samples.add((role, pid, int(task.name), name, mask))


def run(args):
    server, load = cpu_ranges()
    root = args.artifacts_root.resolve()
    root.mkdir(parents=True, exist_ok=False)
    engines = "infinitydb" if args.durability == "everysec" else "redis,dragonfly,infinitydb"
    command = [str(args.compare.resolve()), "run", "--engines", engines,
               "--generator", "both", "--workload", "set,get", "--pipeline", "16",
               "--replicates", "1", "--duration", "1", "--threads", str(args.threads),
               "--clients", "1",
               "--keyspace", "1000", "--rb-requests", "100000", "--port-base", str(port_block()),
               "--pin-start", str(server), "--out", str(root / "run")]
    if not args.legacy_arguments:
        command += ["--load-pin-start", str(load), "--load-cpus", "2"]
    if args.durability == "everysec":
        command += ["--durability", "everysec", "--data-root", str(root / "data")]
    server_cpus = list(range(server, server + args.threads))
    provenance = {"command": command, "server_cpus": server_cpus,
                  "generator_cpus": [load, load + 1], "tier": "non-citable correctness smoke",
                  "compare_sha256": hashlib.sha256(args.compare.read_bytes()).hexdigest()}
    (root / "provenance.json").write_text(json.dumps(provenance, indent=2) + "\n")
    samples = set()
    version_probes = set()
    executables = {}
    programs = ["memtier_benchmark", "redis-benchmark"]
    if args.durability == "none":
        programs += ["redis-server", "dragonfly"]
    for role in programs:
        binary = shutil.which(role)
        if binary is None:
            raise RuntimeError(f"required native binary missing: {role}")
        executables[str(Path(binary).resolve())] = role
    for binary in [Path("target/release/infinityd"), Path("target/debug/infinityd")]:
        if binary.exists():
            executables[str(binary.resolve())] = "infinityd"
    with (root / "stdout.log").open("w") as stdout, (root / "stderr.log").open("w") as stderr:
        process = subprocess.Popen(command, stdout=stdout, stderr=stderr, start_new_session=True)
        try:
            deadline = time.monotonic() + 90
            while process.poll() is None:
                observe(process.pid, samples, executables, version_probes)
                if time.monotonic() >= deadline:
                    raise TimeoutError("native comparison exceeded 90 seconds")
                time.sleep(0.01)
        finally:
            if process.poll() is None:
                os.killpg(process.pid, signal.SIGKILL)
            process.wait()
    observations = []
    failures = []
    for role, pid, tid, name, mask in sorted(samples):
        generator = role in {"memtier_benchmark", "redis-benchmark"}
        expected = {load, load + 1} if generator else set(server_cpus)
        entry = dict(role=role, pid=pid, tid=tid, name=name, cpus=mask)
        observations.append(entry)
        if not mask or not set(mask) <= expected:
            failures.append(entry)
    (root / "observed-affinity.json").write_text(json.dumps(observations, indent=2) + "\n")
    roles = {sample[0] for sample in samples}
    required = {"infinityd", *programs}
    summary = dict(exit=process.returncode, roles=sorted(roles), observations=len(samples),
                   escaped_masks=failures, missing_roles=sorted(required - roles))
    control_seen = any(sample[3] == "inf-control" for sample in samples)
    summary["control_thread_observed"] = control_seen
    summary["version_probes_excluded_from_workload_scope"] = sorted(version_probes)
    (root / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    print(f"native affinity: exit={process.returncode}, roles={sorted(roles)}, "
          f"observations={len(samples)}, escaped={len(failures)}; artifacts={root}")
    return int(process.returncode != 0 or bool(failures) or roles != required
               or (args.durability == "everysec" and not control_seen))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--compare", type=Path, default=Path("target/debug/inf-compare"))
    parser.add_argument("--artifacts-root", type=Path, required=True)
    parser.add_argument("--threads", type=int, choices=[1, 2], default=2)
    parser.add_argument("--durability", choices=["none", "everysec"], default="none",
                        help="everysec inspects InfinityDB's control thread without touching Redis")
    parser.add_argument("--legacy-arguments", action="store_true",
                        help="reproduce pre-fix masks with only the original --pin-start flag")
    raise SystemExit(run(parser.parse_args()))
