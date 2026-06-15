# syntax=docker/dockerfile:1
#
# LOCAL-ONLY client-library smoke image (NOT used in CI). Bundles redis-py,
# node-redis, go-redis, and lettuce so you can verify Redis client
# compatibility against a running infinityd without installing four toolchains
# on the host. CI runs these via setup-* actions instead.
#
# Build context is `tests/client-smoke/`. Drive it with `just client-smoke`,
# or manually:
#   docker build -t infinitydb-client-smoke -f deploy/client-smoke/Dockerfile tests/client-smoke
#   docker run --rm --network host -e INF_HOST=127.0.0.1 -e INF_PORT=6379 infinitydb-client-smoke

FROM debian:bookworm-slim

# Go 1.23 — bookworm ships 1.19, too old for the smoke's go.mod; lift the
# official toolchain in rather than installing from apt.
COPY --from=golang:1.23-bookworm /usr/local/go /usr/local/go
ENV PATH="/usr/local/go/bin:/root/go/bin:${PATH}" \
    GOTOOLCHAIN=local

RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates git \
      python3 python3-redis \
      nodejs npm \
      default-jdk-headless maven \
      redis-tools \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /smoke
COPY . /smoke

# Pre-fetch every toolchain's dependencies at build time so `just client-smoke`
# runs are fast and need no network. The Java step also primes the exec plugin
# by running once against a dead port (it fails to connect — that is fine, the
# plugin and deps are now cached).
RUN npm install --no-save redis@^4 \
 && (cd go && go mod tidy) \
 && (cd java && mvn -q -B compile \
       && (timeout 120 mvn -q -B exec:java -Dexec.mainClass=Smoke -Dexec.args='127.0.0.1 1' || true))

COPY <<'EOF' /entrypoint.sh
#!/usr/bin/env bash
set -uo pipefail
HOST="${INF_HOST:-127.0.0.1}"
PORT="${INF_PORT:-6379}"
echo "== client-smoke against ${HOST}:${PORT} =="
for _ in $(seq 1 50); do
  [ "$(redis-cli -h "$HOST" -p "$PORT" ping 2>/dev/null)" = "PONG" ] && break
  sleep 0.2
done

declare -A RESULT
run() {
  local name="$1"; shift
  echo; echo "--- ${name} ---"
  if "$@"; then RESULT[$name]="ok"; else RESULT[$name]="FAIL"; fi
}

run redis-py   python3 /smoke/smoke.py "$HOST" "$PORT"
run node-redis bash -c "cd /smoke && node smoke.mjs '$HOST' '$PORT'"
run go-redis   bash -c "cd /smoke/go && go run . '${HOST}:${PORT}'"
run lettuce    bash -c "cd /smoke/java && mvn -q -B -o exec:java -Dexec.mainClass=Smoke -Dexec.args='$HOST $PORT'"

echo; echo "== summary =="
fail=0
for k in redis-py node-redis go-redis lettuce; do
  printf '  %-12s %s\n' "$k" "${RESULT[$k]:-skip}"
  [ "${RESULT[$k]:-}" = "ok" ] || fail=1
done
exit "$fail"
EOF
RUN chmod +x /entrypoint.sh

ENTRYPOINT ["/entrypoint.sh"]
