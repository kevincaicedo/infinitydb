# InfinityDB workspace tasks. Run from infinity/.

default: check

check:
    cargo fmt --all --check
    ./scripts/check-dep-dag.sh
    ./scripts/check-cell-denylist.sh
    ./scripts/check-fault-points.sh
    ./scripts/check-fsync-fail-stop.sh
    ./scripts/check-panic-policy.sh
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

# M2-S19 durability sweep (the §6 dst_sweep gate shape). Usage:
#   just durable-sweep [seeds] [base]
durable-sweep seeds="10000" base="0xD5EE0000":
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build --release --bin inf-sim
    out=$(mktemp -d)
    for i in 0 1 2 3 4 5 6 7; do
        ./target/release/inf-sim --scenario m2-durable --sweep {{seeds}} --seed {{base}} \
            --shard "$i/8" --out "$out" & done
    wait
    cat "$out"/manifest-shard-*.txt

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
