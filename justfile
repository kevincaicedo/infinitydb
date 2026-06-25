# InfinityDB workspace tasks. Run from infinity/.

default: check

check:
    cargo fmt --all --check
    ./scripts/check-dep-dag.sh
    ./scripts/check-cell-denylist.sh
    cargo clippy --workspace --all-targets -- -D warnings
    cargo test --workspace

build:
    cargo build --workspace

test:
    cargo test --workspace

fmt:
    cargo fmt --all

dag:
    ./scripts/check-dep-dag.sh

deny:
    cargo deny check

# Loom model of the SPSC ring (PRs touching inf-fabric must run this).
loom:
    RUSTFLAGS="--cfg loom" LOOM_MAX_PREEMPTIONS=3 cargo test -p inf-fabric --release loom_

# Compat-diff vs real redis-server (requires redis-server on PATH).
compat:
    cargo test -p compat -- --nocapture

# Deterministic simulator smoke scenario, twice, comparing traces.
sim-smoke:
    cargo run --release --bin inf-sim -- --scenario m0-smoke --seed 0xC0FFEE --verify-determinism

# M2-S17 partial crash-matrix runner over checked `runner_rows`.
m2-crash-matrix:
    cargo run --release --bin inf-sim -- --scenario m2-crash-matrix --verify-determinism

# M2-S20 external redis-cli smoke over a durable data root.
m2-bgsave-lastsave-smoke *ARGS:
    ./scripts/redis-cli-bgsave-lastsave-smoke.sh {{ARGS}}

# M2-S19 synthetic 10k durability-oracle sweep. This is a gate rehearsal,
# not the final recovered-state durability campaign.
m2-durability-oracle-10k:
    cargo run --release --bin inf-sim -- --scenario m2-durability-oracle --seed 0xD2D2 --sweep-seeds 10000 --writes-per-seed 96 --verify-determinism

# M2-S19 public recovered-state sweep through RESP, sim power cut, recovery,
# and public GET digest verification.
m2-public-durability-sweep:
    cargo run --release --bin inf-sim -- --scenario m2-public-durability-sweep --seed 0xD2550000 --sweep-seeds 64 --writes-per-seed 24 --key-space 8 --verify-determinism

# M2-S19 gate-scale public recovered-state sweep.
m2-public-durability-sweep-10k:
    cargo run --release --bin inf-sim -- --scenario m2-public-durability-sweep --seed 0xD2550000 --sweep-seeds 10000 --writes-per-seed 24 --key-space 8 --verify-determinism

# M2-S19 public everysec loss-window sweep through RESP, sim power cut,
# recovery, and public GET verification on both sides of the timer fsync.
m2-public-everysec-sweep:
    cargo run --release --bin inf-sim -- --scenario m2-public-everysec-sweep --seed 0xE5EC0000 --sweep-seeds 64 --verify-determinism

# M2-S19 gate-scale public everysec loss-window sweep.
m2-public-everysec-sweep-10k:
    cargo run --release --bin inf-sim -- --scenario m2-public-everysec-sweep --seed 0xE5EC0000 --sweep-seeds 10000 --verify-determinism

# M2-S19 public everysec multi-write workload sweep. The current shape proves a
# flushed multi-write prefix plus one loss-window suffix command.
m2-public-everysec-workload-sweep:
    cargo run --release --bin inf-sim -- --scenario m2-public-everysec-workload-sweep --seed 0xE5EC8500 --sweep-seeds 64 --writes-per-seed 24 --key-space 8 --verify-determinism

# M2-S19 gate-scale public everysec multi-write workload sweep.
m2-public-everysec-workload-sweep-10k:
    cargo run --release --bin inf-sim -- --scenario m2-public-everysec-workload-sweep --seed 0xE5EC8500 --sweep-seeds 10000 --writes-per-seed 24 --key-space 8 --verify-determinism

# Competitive benchmark: drive memtier against redis + infinitydb (Phase-1 MVP),
# render a markdown report under .artifacts/compare/. Pass extra flags through,
# e.g. `just benchmark --workload mixed --duration 10 --pipeline 1`.
# See `cargo run -p inf-compare -- help` for the full flag surface.
benchmark *ARGS:
    cargo run --release -p inf-compare -- run {{ARGS}}

# Build the release Docker image (static musl -> scratch). Usage:
#   just docker-build [tag] [version]
docker-build tag="infinitydb:dev" version="v0.1.0-alpha-dev":
    docker build --build-arg INF_RELEASE_VERSION={{version}} --build-arg INF_GIT_SHA=$(git rev-parse --short HEAD) -t {{tag}} .

# Run the Redis client-library smoke suite locally in Docker (NOT used in CI —
# a convenience to test redis-py / node-redis / go-redis / lettuce without
# installing four toolchains). Builds + starts infinityd, runs the clients
# against it over host networking, then stops it. Linux host. Usage:
#   just client-smoke [port]
client-smoke port="6379":
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build --release -p infinityd
    docker build -t infinitydb-client-smoke -f deploy/client-smoke/Dockerfile tests/client-smoke
    ./target/release/infinityd --port {{port}} >/tmp/infinityd-client-smoke.log 2>&1 &
    pid=$!
    trap 'kill "$pid" 2>/dev/null || true' EXIT
    for i in $(seq 1 50); do redis-cli -p {{port}} ping 2>/dev/null | grep -q PONG && break || sleep 0.2; done
    docker run --rm --network host -e INF_HOST=127.0.0.1 -e INF_PORT={{port}} infinitydb-client-smoke
