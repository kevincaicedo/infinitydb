# Deploying InfinityDB

> [!WARNING]
> `v0.1.0-alpha` is **in-memory only, single-node, with no authentication or
> TLS.** Do not expose it to an untrusted network and do not use it as a
> source of truth. Bind it to localhost or a trusted private network.

- [Requirements](#requirements)
- [Running with Docker](#running-with-docker)
- [The io_uring / seccomp requirement](#the-io_uring--seccomp-requirement)
- [Running a prebuilt binary](#running-a-prebuilt-binary)
- [Server options](#server-options)
- [Configuration](#configuration)
- [Connecting](#connecting)
- [Limitations](#limitations)

## Requirements

- **Linux** with `io_uring` support — **kernel 5.15+** (6.1 or newer
  recommended). InfinityDB probes the kernel at boot and uses the best
  available `io_uring` features (multishot accept/recv, provided buffers).
- For Docker: a runtime that lets you set a seccomp profile (Docker, Podman,
  containerd) — see [below](#the-io_uring--seccomp-requirement).
- macOS builds for development and correctness testing (via `kqueue`) but is
  not a performance target and is not recommended for deployment.

## Running with Docker

The release image is a `scratch`-based image containing only the static
`infinityd` binary (no shell, no libc, minimal CVE surface):

```bash
docker run --rm -p 6379:6379 \
  --security-opt seccomp=deploy/seccomp/infinitydb-seccomp.json \
  ghcr.io/kevincaicedo/infinitydb:v0.1.0-alpha
```

The `deploy/seccomp/infinitydb-seccomp.json` profile ships in this repository.
If you are running the image elsewhere, download that file alongside it, or
use one of the alternatives in the next section.

## The io_uring / seccomp requirement

InfinityDB's networking is built on Linux `io_uring`. **Docker's default
seccomp profile blocks the `io_uring` syscalls** (`io_uring_setup`,
`io_uring_enter`, `io_uring_register`). Under the default profile, InfinityDB cannot create its reactor and exits immediately with:

```
infinityd: cell failed: Operation not permitted (os error 1)
```

You have three options, from most to least hardened:

**1. The bundled hardened profile (recommended).**
`deploy/seccomp/infinitydb-seccomp.json` allows `io_uring` while still denying
the high-risk container-escape syscalls (`mount`, `pivot_root`, kernel-module
loading, `kexec_load`, `bpf`, `ptrace`, `perf_event_open`, keyring and
clock-setting calls, and more):

```bash
docker run --security-opt seccomp=deploy/seccomp/infinitydb-seccomp.json ...
```

**2. Start from your runtime's full default profile and add io_uring.** For the
strictest allow-list posture, take Docker's upstream `default.json` and append
the three `io_uring` syscalls to an `SCMP_ACT_ALLOW` entry. This keeps the full
default deny-list and only adds what InfinityDB needs.

**3. Unconfined seccomp (development only).** Quick but removes *all* seccomp
filtering — only for local development on a trusted machine:

```bash
docker run --security-opt seccomp=unconfined ...
```

> [!NOTE]
> Running `infinityd` **directly on a host** (not in a container) needs none of
> this — host processes are not subject to Docker's seccomp profile. The
> requirement is specific to containerized runs.

Hosts/orchestrators differ: some Kubernetes and CI environments already permit
`io_uring`, others apply the Docker default. If the container exits with the
error above, seccomp is the cause.

## Running a prebuilt binary

Static musl binaries for `linux/x86_64` and `linux/aarch64` are attached to
each [release](https://github.com/kevincaicedo/infinitydb/releases):

```bash
tar xzf infinitydb-v0.1.0-alpha.1-linux-x86_64.tar.gz
./infinityd --version          # prints version, git SHA, and target
./infinityd --port 6379
```

Each tarball also includes the generated `compat-matrix.md`. Verify downloads
against the published `SHA256SUMS`.

## Server options

```
infinityd [--port 6379] [--cells N] [--buffers 4096] [--buf-size 4096]
          [--pin-start CORE] [--route-local-only] [--version]
```

| Flag | Meaning |
|---|---|
| `--port` | TCP port to listen on (default 6379). |
| `--cells` | Number of cells (cores). Defaults to a value derived from the machine. |
| `--buffers` / `--buf-size` | `io_uring` provided-buffer pool size and per-buffer bytes. |
| `--pin-start` | First core index to pin cells to (cell *i* pins to `pin-start + i`). |
| `--route-local-only` | Treat every key as local to the accepting cell (benchmark/diagnostic mode). |
| `--version` | Print version + git SHA + target and exit. |

## Configuration

Configuration uses the Redis `CONFIG` command surface. The most relevant keys
for the cache core:

```bash
redis-cli config set maxmemory 256mb
redis-cli config set maxmemory-policy allkeys-lfu
redis-cli config set client-output-buffer-limit "pubsub 33554432 8388608 60"
```

`maxmemory` is divided across cells. Supported eviction policies: `noeviction`,
`allkeys-lru`, `volatile-lru`, `allkeys-lfu`, `volatile-lfu`, `allkeys-random`,
`volatile-random`, `volatile-ttl`.

## Connecting

Any Redis client works. Examples:

```bash
redis-cli -p 6379                          # interactive
redis-cli -p 6379 INFO server              # server section
```

```python
import redis
r = redis.Redis(host="127.0.0.1", port=6379, decode_responses=True)
r.set("k", "v"); print(r.get("k"))
```

See the [compatibility matrix](compat-matrix.md) for exactly which commands are
supported and any documented deviations.

## Limitations

`v0.1.0-alpha` is an early release. Known limitations:

- **No persistence.** All data is in memory and lost on restart (durability is
  milestone M2).
- **No authentication or TLS** (milestone M5). Bind to localhost / trusted
  networks only.
- **Single node.** No replication or clustering yet.
- **No collections** (lists/sets/hashes/sorted sets — milestone M3), **no
  transactions or Lua** (M4), **no streams or JSON** (M5/M6).
- **`MULTI`/`EXEC`** return "not yet supported" errors.

See the [roadmap](roadmap.md) for what lands when.
