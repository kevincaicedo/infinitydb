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
