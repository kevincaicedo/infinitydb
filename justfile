# InfinityDB workspace tasks. Run from infinity/.

default: check

check:
    cargo fmt --all --check
    ./scripts/check-dep-dag.sh
    ./scripts/check-cell-denylist.sh
    ./scripts/check-fault-points.sh
    ./scripts/check-fsync-fail-stop.sh
    ./scripts/check-panic-policy.sh
    ./scripts/check-safety-inventory.sh
    cargo clippy --workspace --all-targets -- -D warnings
    cargo test --workspace
    cargo clippy -p inf-doc -p inf-store --all-targets --features doc-intern-keys -- -D warnings
    cargo test -p inf-doc -p inf-store --features doc-intern-keys
    # Slim-build lane (L11, ADR-0041 D3): a docless server carries zero
    # document/path code and must keep compiling that way.
    cargo check -p inf-server -p inf-store --no-default-features

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

# Deterministic simulator smoke scenarios, each twice, comparing traces.
# m4-steel is the M4-S04 steel-thread twin (tiered lifecycle + cold reads
# through suspension + the S06 crash/replay content oracle); m4-pressure
# is the M4-S07 throttled-device backpressure scenario (budget bound,
# typed stall timeouts, deadlock freedom); m4-cold is the M4-S08
# cold-read storm (relocation races, chunked staging, cancellation,
# pin-deferred unlinks — the 10⁶-op AC run passes --ops 1000000);
# m4-recovery is the M4-S12 unified-recovery power-cut chain (hybrid
# checkpoints, MANIFEST v2, D4 tail replay, never-none oracle);
# m4-tiered is the M4-S26 command-driven tiered node (RESP over the sim
# net against the wired plane: cut → recover → §8.2 command audit →
# re-pressure flush liveness → DISKFULL clamp → the S19 drop race).
sim-smoke:
    cargo run --release --bin inf-sim -- --scenario m0-smoke --seed 0xC0FFEE --verify-determinism
    cargo run --release --bin inf-sim -- --scenario m4-steel --seed 0xC0FFEE --verify-determinism
    cargo run --release --bin inf-sim -- --scenario m4-pressure --seed 0xC0FFEE --verify-determinism
    cargo run --release --bin inf-sim -- --scenario m4-cold --seed 0xC0FFEE --verify-determinism
    cargo run --release --bin inf-sim -- --scenario m4-recovery --seed 0xC0FFEE --verify-determinism
    cargo run --release --bin inf-sim -- --scenario m4-diskfull --seed 0xC0FFEE --verify-determinism
    cargo run --release --bin inf-sim -- --scenario m4-tiered --seed 0xC0FFEE --verify-determinism
    cargo run --release --bin inf-sim -- --scenario m2-device-budget --seed 0xC0FFEE --verify-determinism

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

# M4.5-S36 device-budget sweep (ADR-0088 D8): the m2 durable shape under
# a tight budget model over a bandwidth-modeled disk — the accounting
# identity, the rate bound, engagement, progress, the foreground bound,
# and the durability oracle, per seed. Usage:
#   just budget-sweep [seeds] [base]
budget-sweep seeds="1000" base="0xB0D6E700":
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build --release --bin inf-sim
    out=$(mktemp -d)
    for i in 0 1 2 3 4 5 6 7; do
        ./target/release/inf-sim --scenario m2-device-budget --sweep {{seeds}} --seed {{base}} \
            --shard "$i/8" --out "$out" & done
    wait
    cat "$out"/manifest-shard-*.txt

# M3-S24 document power-cut + replay-equivalence sweep (the M3 §7 crash
# and replay-equivalence gate shape; ADR-0045). Usage:
#   just doc-sweep [seeds] [base]
doc-sweep seeds="10000" base="0xD0C24000":
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build --release --bin inf-sim
    out=$(mktemp -d)
    for i in 0 1 2 3 4 5 6 7; do
        ./target/release/inf-sim --scenario m3-document --sweep {{seeds}} --seed {{base}} \
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
