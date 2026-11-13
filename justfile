# InfinityDB workspace tasks. Run from infinitydb/ (the published repo root).

# Every recipe builds in this checkout's own target (ADR-0204 D1): cargo reads
# another checkout's outputs as fresh, and a moved root or a target triple
# leaves a path into target/<profile> naming an older build, so before any
# recipe runs scripts/target-root.sh refuses each setting that moves the root,
# a root a copied or moved checkout carries, and one holding outputs no marker
# names.
export CARGO_TARGET_DIR := `./scripts/target-root.sh --print`

default: check

check:
    cargo fmt --all --check
    # ADR-0200 D5: the tests run under cargo-nextest, at least the version
    # .config/nextest.toml requires; a missing or older one is red here, in
    # about a second, not after the gates.
    cargo nextest show-config version
    # Every relative link in a published Markdown file resolves inside the
    # repository (ADR-0166 D5), beside the document identities.
    ./scripts/check-doc-artifacts.sh
    ./scripts/check-dep-dag.sh
    ./scripts/check-cell-denylist.sh
    ./scripts/check-fault-points.sh
    ./scripts/check-fsync-fail-stop.sh
    ./scripts/check-panic-policy.sh
    # ADR-0190 D4, A2: no production site of inf-server decides a reply's outcome
    # from its bytes, or names a fabric reply tag outside the functions that set and read it.
    ./scripts/check-reply-outcome.sh
    # ADR-0203 D3: a test names infinityd only through the node module, whose
    # starts hold the box's node lock; every ring outside a node is a census row.
    ./scripts/check-node-spawn.sh
    # ADR-0172 D2: a pre-effect refusal's text is inf_wire::Refusal's alone,
    # so no production line of inf-server spells a refusal class suffix.
    ./scripts/check-refusal-text.sh
    # ADR-0107 D2 (review 2026-08-30, Theme 4): every release assert/expect/
    # panic/unreachable in cell code is a classified inventory row.
    ./scripts/check-release-asserts.sh
    ./scripts/check-safety-inventory.sh
    # ADR-0125: the counted disciplines are gates: 3000 production lines per
    # file, 100 columns on every line of the Rust files under crates/, bins/
    # and tests/, and the 70-line function ratchet.
    ./scripts/check-file-length.sh
    ./scripts/check-line-width.sh
    # ADR-0144 D3: one ratchet — fn-length, and the cast / arithmetic
    # backlog of the decoder files docs/lint-scopes.tsv lists — judged
    # against the table's approved copies in history.
    ./scripts/check-lint-ratchet.sh
    # ADR-0144: every `src/lib.rs`, `src/main.rs` and `src/bin/*.rs` root
    # under crates/ and bins/ is under the wildcard deny, an attribute
    # silences a lint of the ADR only as a reasoned allow of one of that
    # lint's classes, the ADR-0143 exemptions are frozen, and the probe's
    # plants draw their lint.
    ./scripts/check-lint-scopes.sh
    # ADR-0165 D2: every saturating_/wrapping_ call in a deny-arith scope is a
    # counted row of docs/arith-spellings.tsv, reviewed against D1's rows.
    ./scripts/check-arith-spellings.sh
    # ADR-0121: every `src/lib.rs`, `src/main.rs` and `src/bin/*.rs` root of
    # the workspace forbids or denies unsafe_code, the deny set is the
    # audited-leaf list, and every allow written exactly
    # `allow(unsafe_code)` is module-scoped.
    ./scripts/check-unsafe-roots.sh
    # ADR-0107 (review 2026-08-30, F-L16-01): no normal dependency edge may
    # request fault-points/collision-oracle — a workspace build would link
    # infinityd against a colliding hasher.
    ./scripts/check-shipping-features.sh
    # ADR-0198: every fuzz target compiles, on the pinned toolchain with
    # warnings denied, under its committed lock and cfg(fuzzing), and a probe
    # of planted reds proves the gate sees each shape it guards.
    ./scripts/check-fuzz-targets.sh
    # ADR-0106 first amendment (review 2026-08-30 F-L18-05): the ambient-
    # clock ban is clippy's type-resolved disallowed-methods; this gate
    # proves the config is in force on planted bypass spellings; the
    # common lint-scopes audit lists and checks the sanctioned sites.
    ./scripts/check-clock-ban.sh
    # ADR-0106 second amendment (review 2026-08-30, F-L20-03): the M0-S06
    # AC's own gate ran nowhere and scanned zero instructions per waker.
    ./scripts/check-waker-atomics.sh
    # ADR-0207 D1-D3, ADR-0200 D4, D7: every static lock and every marked
    # run-alone or long test has its nextest override, CI installs the nextest
    # the config requires, and no test file reads free space or leaves a
    # tiered namespace's stall bound unstated.
    ./scripts/check-nextest-config.sh
    cargo clippy --workspace --all-targets -- -D warnings
    # ADR-0164: the document gates of the combined checkout; a standalone
    # checkout prints the skip. They run after clippy, so a compile or lint
    # red comes first.
    ./scripts/check-parent-doc-gates.sh
    # ADR-0106: the gates above must go red on a planted violation, or
    # their OK is a claim (review 2026-08-30 P1/P1c: two were inert).
    # It runs after clippy, so a compile or lint red stops the run first.
    ./scripts/check-scripts-selftest.sh
    # ADR-0200 D1: each set's tests run under nextest, one process a test,
    # every binary even after a red; its doctests run under cargo test.
    cargo nextest run --no-fail-fast --workspace
    cargo test --doc --workspace
    cargo clippy -p inf-doc -p inf-store --all-targets --features doc-intern-keys -- -D warnings
    cargo nextest run --no-fail-fast -p inf-doc -p inf-store --features doc-intern-keys
    cargo test --doc -p inf-doc -p inf-store --features doc-intern-keys
    # Slim-build lane (L11, ADR-0041 D3): a docless server carries zero
    # document/path code and must keep compiling that way — warnings
    # denied since ADR-0125 A2 (a doc-only const warned here for months).
    cargo clippy -p inf-server -p inf-store --no-default-features -- -D warnings
    cargo nextest run --no-fail-fast -p inf-server -p inf-store --no-default-features
    cargo test --doc -p inf-server -p inf-store --no-default-features

build:
    cargo build --workspace --exclude inf-sim
    cargo build -p inf-sim --features dst

test:
    cargo test --workspace

fmt:
    cargo fmt --all

dag:
    ./scripts/check-dep-dag.sh

# cargo-deny on every lock that holds a third-party crate, the root's and
# each fuzz manifest's: the manifests the fuzz census lists (ADR-0198 D6),
# with the features PR CI's cargo-deny jobs judge (--all-features, the
# action's default). Every leg runs with no stdin, so none can read the list
# the loop reads, and its status is read; one red leg fails the recipe, and so
# do an empty list and a leg count other than the census's rows. The first leg
# fetches the advisory database and the rest read that copy, so every leg
# judges one snapshot.
deny:
    #!/usr/bin/env bash
    set -euo pipefail
    rows=$(./scripts/check-fuzz-targets.sh --matrix deny)
    list='import json, sys; print("\n".join(r["manifest"] for r in json.load(sys.stdin)["include"]))'
    manifests=$(python3 -I -c "$list" <<<"$rows")
    size='import json, sys; print(len(json.load(sys.stdin)["include"]))'
    count=$(python3 -I -c "$size" <<<"$rows")
    [ "$count" -gt 0 ] || { echo "deny SCOPE ERROR: the fuzz census listed no manifest"; exit 1; }
    n=0 red=0 fetch=check
    while IFS= read -r manifest; do
        n=$((n + 1))
        if cargo deny --manifest-path "$manifest" --all-features $fetch </dev/null; then
            echo "deny: $manifest ok"
        else
            status=$?
            echo "deny: $manifest red (exit $status)"
            red=$((red + 1))
        fi
        fetch="check --disable-fetch"
    done <<<"$manifests"
    [ "$n" -eq "$count" ] \
        || { echo "deny FAILED: ran $n of the $count manifests the census listed"; exit 1; }
    [ "$red" -eq 0 ] || { echo "deny FAILED: $red of $n manifests red"; exit 1; }
    echo "deny OK: $n manifests (the root and $((n - 1)) fuzz manifests), each rc 0"

# Loom models: the SPSC ring, checkpoint issuance and the wake bridge (PRs
# touching inf-fabric or inf-foundation's `issue` or `wake` module must run
# this).
loom:
    RUSTFLAGS="--cfg loom" LOOM_MAX_PREEMPTIONS=3 cargo test -p inf-fabric --release loom_
    # ADR-0159 A1.6/A1.7 and ADR-0003 A1.7: the checkpoint issuance orderings
    # and the wake bridge's lane, checked on the product's own `issue` and
    # `wake` types.
    RUSTFLAGS="--cfg loom" LOOM_MAX_PREEMPTIONS=3 cargo test -p inf-foundation --release loom_

# Compat-diff requires Redis 8.0.5 on PATH or INF_COMPAT_ORACLE_ADDR. The
# server it diffs is the one it builds, in this checkout's target root.
compat:
    cargo build -p infinityd
    INF_COMPAT_REQUIRE_BINARY=1 INFINITYD_BIN={{quote(CARGO_TARGET_DIR + "/debug/infinityd")}} cargo test -p compat -- --nocapture

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
# net against the wired plane: cut → recover → durability-class command audit →
# re-pressure flush liveness → DISKFULL clamp → the S19 drop race).
sim-smoke:
    # F-L19-03 (review 2026-08-30): one registry — every scenario the
    # binary accepts, once, determinism-verified; `bins/inf-sim/tests/
    # lanes.rs` fails when a scenario has no row. PR CI runs the same script.
    ./scripts/sim-smoke.sh

# Planted-bug canaries (ADR-0129; ADR-0090 D5): each `--cfg inf_canary_*`
# built into its own target dir must turn its scenario red, and the plain
# build must stay green on the same row. Nightly runs it too.
sim-canaries:
    ./scripts/sim-canaries.sh

# M2-S19 durability sweep (the dst_sweep gate shape). Every *-sweep
# recipe runs through scripts/run-sweep.sh (ADR-0106 D7): eight shards,
# each waited on by pid, each manifest required to carry ` violations=0 `
# — the old inline bodies ended in a bare `wait`, whose status is 0
# whatever the shards did (review 2026-08-30 follow-up). Usage:
#   just durable-sweep [seeds] [base]
durable-sweep seeds="10000" base="0xD5EE0000":
    ./scripts/run-sweep.sh m2-durable {{seeds}} {{base}}

# M4.5-S36 device-budget sweep (ADR-0088 D8): the m2 durable shape under
# a tight budget model over a bandwidth-modeled disk — the accounting
# identity, the rate bound, engagement, progress, the foreground bound,
# and the durability oracle, per seed. Usage:
#   just budget-sweep [seeds] [base]
budget-sweep seeds="1000" base="0xB0D6E700":
    ./scripts/run-sweep.sh m2-device-budget {{seeds}} {{base}}

# Barrier-class transition sweep: two lives per seed under the m2
# durability oracle. FLUSH → FUA on even seeds reopens a packed tail under
# a Direct rotor after a dirty cut, which resumes Buffered at its own
# offset, never padded to alignment (ADR-0086 D4); FUA → FLUSH runs on odd
# seeds. The no-checkpoint half exercises the stale-residue rule: a
# discarded life's frames in sealed slack are residue, never a hole
# (ADR-0031 D5). Usage:
#   just transition-sweep [seeds] [base]
transition-sweep seeds="4000" base="0x7A4E0000":
    ./scripts/run-sweep.sh m2-mode-transition {{seeds}} {{base}}

# Completion-ledger reorder-window sweep: the ledger queues at most a
# reorder window of frames and holds the next one while it is full
# (ADR-0087 D2). Every 40th plain write wedged 150 ms on a K ≥ 2 pipeline
# of everysec-only frames, so the window fills and the next frame holds —
# the window must engage on every seed (oracle), the ledger's bound is
# release-asserted, the m2 durability oracle holds. Usage:
#   just reorder-sweep [seeds] [base]
reorder-sweep seeds="2000" base="0x2E0D0000":
    ./scripts/run-sweep.sh m2-reorder-window {{seeds}} {{base}}

# M4.5-S39b segment-recycling sweep (ADR-0090 D5): the m2 durable shape
# under the FUA class with an `always` namespace on every seed, small
# segments and a checkpoint interval at the segment size (many rotations,
# checkpoints, truncations and recyclings per run); the recycle oracle
# (rotated + truncated ⇒ recycled; zero-fill ≤ unserved preallocs ×
# segment), a refused boot is a finding, the m2 durability oracle holds.
# The planted-bug canary: `RUSTFLAGS="--cfg inf_canary_foreign_segment"`
# on a scratch target dir must turn this sweep red. Usage:
#   just recycle-sweep [seeds] [base]
recycle-sweep seeds="10000" base="0xD5EE0000":
    ./scripts/run-sweep.sh m2-recycle {{seeds}} {{base}}

# M3-S24 document power-cut + replay-equivalence sweep (the M3 exit-gate crash
# and replay-equivalence gate shape; ADR-0045). Usage:
#   just doc-sweep [seeds] [base]
doc-sweep seeds="10000" base="0xD0C24000":
    ./scripts/run-sweep.sh m3-document {{seeds}} {{base}}

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
    {{quote(CARGO_TARGET_DIR + "/release/infinityd")}} --port {{port}} >/tmp/infinityd-client-smoke.log 2>&1 &
    pid=$!
    trap 'kill "$pid" 2>/dev/null || true' EXIT
    for i in $(seq 1 50); do redis-cli -p {{port}} ping 2>/dev/null | grep -q PONG && break || sleep 0.2; done
    docker run --rm --network host -e INF_HOST=127.0.0.1 -e INF_PORT={{port}} infinitydb-client-smoke
