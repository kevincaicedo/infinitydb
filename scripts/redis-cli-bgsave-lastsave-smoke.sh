#!/usr/bin/env bash
# M2-S20 external redis-cli smoke for the live checkpoint command surface.
set -euo pipefail

cd "$(dirname "$0")/.."
repo="$(pwd)"

if ! command -v redis-cli >/dev/null 2>&1; then
    echo "redis-cli-bgsave-lastsave-smoke: redis-cli is required" >&2
    exit 127
fi

pick_port() {
    python3 - <<'PY'
import socket

sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
sock.bind(("127.0.0.1", 0))
print(sock.getsockname()[1])
sock.close()
PY
}

port="${1:-}"
if [ -z "$port" ]; then
    port="$(pick_port)"
fi

bin="${INF_INFINITYD_BIN:-target/debug/infinityd}"
case "$bin" in
    /*) ;;
    *) bin="$repo/$bin" ;;
esac

if [ "${INF_SKIP_BUILD:-0}" != "1" ] || [ ! -x "$bin" ]; then
    cargo build -p infinityd
fi

workdir="$(mktemp -d "${TMPDIR:-/tmp}/infinitydb-m2-bgsave-smoke.XXXXXX")"
log="$workdir/infinityd.log"
pid=""

cleanup() {
    status=$?
    if [ -n "$pid" ]; then
        kill "$pid" >/dev/null 2>&1 || true
        wait "$pid" >/dev/null 2>&1 || true
    fi
    if [ "$status" -ne 0 ] && [ -f "$log" ]; then
        echo "---- infinityd log ----" >&2
        sed -n '1,160p' "$log" >&2
    fi
    rm -rf "$workdir"
}
trap cleanup EXIT

(
    cd "$workdir"
    "$bin" --port "$port" --cells 1 --data-dir data >"$log" 2>&1
) &
pid=$!

for _ in $(seq 1 100); do
    if redis-cli -h 127.0.0.1 -p "$port" --raw PING 2>/dev/null | grep -qx PONG; then
        break
    fi
    if ! kill -0 "$pid" >/dev/null 2>&1; then
        echo "redis-cli-bgsave-lastsave-smoke: infinityd exited before PING" >&2
        exit 1
    fi
    sleep 0.1
done

if ! redis-cli -h 127.0.0.1 -p "$port" --raw PING 2>/dev/null | grep -qx PONG; then
    echo "redis-cli-bgsave-lastsave-smoke: infinityd did not answer PING on port $port" >&2
    exit 1
fi

before="$(redis-cli -h 127.0.0.1 -p "$port" --raw LASTSAVE)"
if ! [[ "$before" =~ ^[0-9]+$ ]]; then
    echo "redis-cli-bgsave-lastsave-smoke: LASTSAVE before BGSAVE was not numeric: $before" >&2
    exit 1
fi
if [ "$before" -ne 0 ]; then
    echo "redis-cli-bgsave-lastsave-smoke: fresh data root LASTSAVE = $before, want 0" >&2
    exit 1
fi

bgsave="$(redis-cli -h 127.0.0.1 -p "$port" --raw BGSAVE)"
if [ "$bgsave" != "Background saving started" ]; then
    echo "redis-cli-bgsave-lastsave-smoke: BGSAVE reply = $bgsave" >&2
    exit 1
fi

last="0"
for _ in $(seq 1 200); do
    last="$(redis-cli -h 127.0.0.1 -p "$port" --raw LASTSAVE)"
    if [[ "$last" =~ ^[0-9]+$ ]] && [ "$last" -gt 0 ]; then
        break
    fi
    sleep 0.05
done

if ! [[ "$last" =~ ^[0-9]+$ ]] || [ "$last" -le 0 ]; then
    echo "redis-cli-bgsave-lastsave-smoke: LASTSAVE did not advance after BGSAVE" >&2
    exit 1
fi

wait_reply="$(redis-cli -h 127.0.0.1 -p "$port" --raw INF.CKPT WAIT)"
if [ "$wait_reply" != "OK" ]; then
    echo "redis-cli-bgsave-lastsave-smoke: INF.CKPT WAIT reply = $wait_reply" >&2
    exit 1
fi

after_wait="$(redis-cli -h 127.0.0.1 -p "$port" --raw LASTSAVE)"
if ! [[ "$after_wait" =~ ^[0-9]+$ ]] || [ "$after_wait" -lt "$last" ]; then
    echo "redis-cli-bgsave-lastsave-smoke: LASTSAVE after WAIT = $after_wait, previous = $last" >&2
    exit 1
fi

echo "redis-cli-bgsave-lastsave-smoke: OK port=$port lastsave=$after_wait"
