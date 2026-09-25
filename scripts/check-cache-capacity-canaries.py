#!/usr/bin/env python3
"""ADR-0146: exercise cache refusal/accounting mutants in an isolated source snapshot.

Run from infinitydb with no concurrent heavy work. All output and build products
stay below --artifacts. Each mutant must compile and run its exact test; every
restoration must rebuild its owning crate and pass that test. No checkout edits.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import resource
import subprocess
import tarfile
import time


def judge(output, code, owner, red):
    events = []
    for line in output.splitlines():
        if line.startswith("{"):
            try:
                events.append(json.loads(line))
            except json.JSONDecodeError:
                pass
    builds = [event for event in events if event.get("reason") == "build-finished"]
    artifacts = [event for event in events if event.get("reason") == "compiler-artifact"
                 and event["target"]["name"] == owner]
    if not builds or not builds[-1]["success"] or not artifacts or artifacts[-1]["fresh"]:
        raise ValueError("the planted/restored owner was not freshly compiled")
    if "running 1 test" not in output:
        raise ValueError("the exact carrier did not run once")
    if red:
        if code == 0 or red not in output:
            raise ValueError("the planted defect did not fail its named witness")
    elif code != 0 or "1 passed; 0 failed" not in output:
        raise ValueError("the restored control did not pass")


def self_test():
    build = {"reason": "build-finished", "success": True}
    artifact = {"reason": "compiler-artifact", "target": {"name": "owner"}, "fresh": False}
    prefix = json.dumps(build) + "\n" + json.dumps(artifact) + "\nrunning 1 test\n"
    judge(prefix + "1 passed; 0 failed", 0, "owner", None)
    judge(prefix + "named defect", 101, "owner", "named defect")
    plants = [
        (prefix.replace("running 1 test", "running 0 tests") + "named defect", 101, "named defect"),
        (prefix.replace('"fresh": false', '"fresh": true') + "named defect", 101, "named defect"),
        (prefix.replace('"success": true', '"success": false') + "named defect", 101, "named defect"),
        (prefix + "different failure", 101, "named defect"),
        (prefix + "named defect", 0, "named defect"),
        (prefix + "0 passed; 1 failed", 101, None),
    ]
    for output, code, red in plants:
        try:
            judge(output, code, "owner", red)
        except ValueError:
            continue
        raise AssertionError("invalid canary/control receipt was accepted")
    print("cache-canary judge: two controls and six planted invalid receipts passed")


def snapshot(repo, destination, artifacts):
    destination.mkdir()
    archive = artifacts / "snapshot.tar"
    with archive.open("wb") as stream:
        subprocess.run(["git", "archive", "HEAD"], cwd=repo, stdout=stream, check=True)
    with tarfile.open(archive) as stream:
        stream.extractall(destination, filter="data")
    archive.unlink()
    subprocess.run(["git", "init", "-q"], cwd=destination, check=True)
    patch = subprocess.check_output(["git", "diff", "--binary", "HEAD"], cwd=repo)
    (artifacts / "source.diff").write_bytes(patch)
    subprocess.run(["git", "apply", "--binary", "-"], input=patch, cwd=destination, check=True)
    untracked = subprocess.check_output(
        ["git", "ls-files", "--others", "--exclude-standard", "-z"], cwd=repo,
    ).decode().split("\0")
    for name in filter(None, untracked):
        source = repo / name
        target = destination / name
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(source.read_bytes())
        target.chmod(source.stat().st_mode & 0o777)
    files = {}
    for path in sorted(destination.rglob("*")):
        relative = path.relative_to(destination)
        if path.is_file() and relative.parts[0] != ".git":
            files[str(relative)] = {
                "sha256": hashlib.sha256(path.read_bytes()).hexdigest(),
                "mode": path.stat().st_mode & 0o777,
            }
    manifest = {
        "base_revision": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=repo).decode().strip(),
        "dirty_status": subprocess.check_output(["git", "status", "--porcelain"], cwd=repo).decode(),
        "toolchain": subprocess.check_output(["rustc", "-Vv"], cwd=repo).decode(),
        "files": files,
    }
    (artifacts / "source-manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")


def run(artifacts, tree, label, package, target, case, owner, red):
    command = ["cargo", "test", "--locked", "-p", package, *target,
               "--message-format=json", case, "--", "--exact", "--nocapture"]
    env = os.environ.copy()
    env.update(CARGO_BUILD_JOBS="2", CARGO_INCREMENTAL="0",
               CARGO_TARGET_DIR=str(artifacts / "target"))
    log = artifacts / (label + ".log")
    start = time.time()
    with log.open("w") as stream:
        result = subprocess.run(command, cwd=tree, env=env, stdout=stream,
                                stderr=subprocess.STDOUT, check=False, timeout=1800)
    output = log.read_text()
    judge(output, result.returncode, owner, red)
    receipt = {"command": command, "exit": result.returncode, "owner": owner,
               "expected_failure": red, "log": str(log), "start_unix": start,
               "elapsed_seconds": time.time() - start}
    (artifacts / (label + ".json")).write_text(json.dumps(receipt, indent=2) + "\n")
    print(label + ": expected result observed", flush=True)


def mutations():
    for crate, prefix in (("inf-doc", "PROGRAM"), ("inf-query", "STATEMENT")):
        stem = "path" if crate == "inf-doc" else "partiql"
        owner = crate.replace("-", "_")
        yield (crate + "-width", f"crates/{crate}/src/limits.rs",
               f"if requested > usize::from({prefix}_CACHE_ENTRIES_MAX)",
               "if requested > usize::from(u16::MAX)", crate, ["--test", "cache_capacity"],
               "capacity_boundary_refuses_before_any_allocation", owner, "out-of-budget count")
        yield (crate + "-infallible", f"crates/{crate}/src/{stem}/cache.rs",
               "buckets.try_reserve_exact(capacity.buckets())?;",
               "buckets.reserve_exact(capacity.buckets());", crate, ["--test", "cache_capacity"],
               "either_metadata_reservation_can_refuse_without_replacing_the_cache",
               owner, "memory allocation of")
    yield ("query-encoded-only", "crates/inf-query/src/partiql/cache.rs",
           "text.len().saturating_add(compiled.heap_bytes())",
           "text.len().saturating_add(compiled.program.as_bytes().len())",
           "inf-query", ["--test", "cache_capacity"],
           "resident_byte_gauge_counts_decoded_buffers_and_shared_program_once",
           "inf_query", "statement:")
    yield ("rc-header-omitted", "crates/inf-foundation/src/footprint.rs",
           "Layout::new::<[usize; 2]>()", "Layout::new::<[usize; 0]>()",
           "inf-doc", ["--test", "cache_capacity"],
           "resident_byte_gauge_matches_independent_allocation_census",
           "inf_foundation", "assertion `left == right` failed")
    yield ("boot-refusal-masked", "crates/inf-server/src/exec.rs",
           "let path_cache = inf_doc::ProgramCache::try_new(config.path_cache_capacity())?;",
           "let path_cache = inf_doc::ProgramCache::try_new(config.path_cache_capacity()).unwrap_or_else(|_| "
           "inf_doc::ProgramCache::try_new(inf_doc::limits::ProgramCacheCapacity::try_from(0).unwrap()).unwrap());",
           "infinityd", ["--bin", "infinityd"],
           "cache_boot_tests::cache_refusal_propagates_through_production_boot_before_listen",
           "inf_server", "boot must propagate cache allocation refusal")
    yield ("peer-cache-barrier-bypassed", "crates/inf-server/src/plane/cell_loop.rs",
           "match self.shared.node.cache_boot.status() {",
           "match { let _ = self.shared.node.cache_boot.status(); CacheBootStatus::Ready } {",
           "inf-server", ["--lib"],
           "plane::cache_boot_tests::delayed_cache_never_arms_a_peer_listener",
           "inf_server", "peer cache is still pending")
    yield ("retry-cache-barrier-bypassed", "crates/inf-server/src/plane/cell_loop.rs",
           "if key == ACCEPT_RETRY_TIMER_KEY && self.started {",
           "if key == ACCEPT_RETRY_TIMER_KEY {",
           "inf-server", ["--lib"],
           "plane::cache_boot_tests::premature_retry_timer_cannot_bypass_cache_admission",
           "inf_server", "retry timer cannot admit a pending group")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--artifacts", type=Path)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        self_test()
        return
    if args.artifacts is None:
        parser.error("--artifacts must name a new ignored output directory")
    repo = Path.cwd()
    artifacts = args.artifacts.resolve()
    if not artifacts.is_relative_to(repo / ".artifacts"):
        parser.error("output must be below this checkout's .artifacts directory")
    artifacts.mkdir(parents=True, exist_ok=False)
    soft, hard = resource.getrlimit(resource.RLIMIT_AS)
    cap = min([12 * 1024**3, *[n for n in (soft, hard) if n != resource.RLIM_INFINITY]])
    resource.setrlimit(resource.RLIMIT_AS, (cap, hard))
    resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
    tree = artifacts / "source"
    snapshot(repo, tree, artifacts)
    for label, filename, old, new, package, target, case, owner, red in mutations():
        path = tree / filename
        original = path.read_text()
        if original.count(old) != 1:
            raise ValueError(f"mutant no longer matches exactly once: {label}")
        try:
            path.write_text(original.replace(old, new, 1))
            run(artifacts, tree, label + "-red", package, target, case, owner, red)
        finally:
            # New mtimes are intentional: restoring a saved mtime can reuse the mutant binary.
            path.write_text(original)
        run(artifacts, tree, label + "-green", package, target, case, owner, None)


if __name__ == "__main__":
    main()
