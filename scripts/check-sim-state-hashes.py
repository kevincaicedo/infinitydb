#!/usr/bin/env python3
"""Compare trace and state evidence across debug/release for every registered scenario."""
import argparse
import hashlib
import pathlib
import re
import subprocess


def run(binary, scenario, seed):
    result = subprocess.run(
        [binary, "--scenario", scenario, "--seed", seed, "--verify-determinism"],
        capture_output=True, text=True, timeout=180, check=False,
    )
    if result.returncode:
        raise RuntimeError(f"{binary} {scenario}: {result.stdout}\n{result.stderr}")
    state = re.findall(r"state_hash=(0x[0-9a-f]+)", result.stdout)
    trace = re.findall(r"\b(?:hash|trace) (0x[0-9a-f]+)", result.stdout)
    if len(state) != 1 or len(trace) != 1:
        raise RuntimeError(f"missing/duplicate evidence for {scenario}: {result.stdout}")
    return trace[0], state[0]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--debug-bin", default="target/debug/inf-sim")
    parser.add_argument("--release-bin", default="target/release/inf-sim")
    parser.add_argument("--seed", default="0xC0FFEE")
    parser.add_argument("--scenario", action="append", help="restrict to a registered scenario")
    args = parser.parse_args()
    root = pathlib.Path(__file__).resolve().parent.parent
    registry = (root / "bins/inf-sim/src/lib.rs").read_text()
    body = registry.split("pub const SCENARIOS: &[&str] = &[", 1)[1].split("];", 1)[0]
    scenarios = re.findall(r'"([a-z0-9-]+)"', body)
    if not scenarios or len(scenarios) != len(set(scenarios)):
        raise RuntimeError("invalid registry")
    if args.scenario:
        if not set(args.scenario).issubset(scenarios):
            raise RuntimeError("unregistered scenario requested")
        scenarios = args.scenario
    binaries = [root / args.debug_bin, root / args.release_bin]
    hashes = [hashlib.sha256(binary.read_bytes()).hexdigest() for binary in binaries]
    for binary, digest in zip(binaries, hashes):
        print(f"{binary}: sha256={digest}", flush=True)
    for scenario in scenarios:
        debug = run(str(root / args.debug_bin), scenario, args.seed)
        release = run(str(root / args.release_bin), scenario, args.seed)
        if debug != release:
            raise RuntimeError(f"{scenario}: debug {debug}, release {release}")
        print(f"{scenario}: trace_hash={debug[0]} state_hash={debug[1]} identical", flush=True)
    if hashes != [hashlib.sha256(binary.read_bytes()).hexdigest() for binary in binaries]:
        raise RuntimeError("a binary changed during validation")
    print(f"{len(scenarios)} scenarios: debug/release trace and state hashes identical")


if __name__ == "__main__":
    main()
